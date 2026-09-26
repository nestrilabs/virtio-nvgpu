// SPDX-License-Identifier: Apache-2.0
//! The frame format of the Wayland channel: what a WL_SEND request and a
//! WL_RECV response carry after their `MsgHeader`, and what `/dev/nvgpu-wl`
//! SEND/RECV take from and give to the guest daemon.
//!
//! The layout is the same in both directions and on both carriers (the
//! virtqueue message and the ioctl buffer), so the kernel handles a frame
//! without understanding it beyond its descriptor table. Little-endian.
//!
//! ```text
//! frame   = FrameHdr | Desc[ndesc] | records (rec_len bytes)
//! FrameHdr  { u32 magic "NVWL"; u16 version = 1; u16 ndesc; u32 rec_len; u32 flags }   16 B
//! Desc      { u16 kind; u16 flags; s32 fd; u32 a; u32 b; u64 c }                        24 B
//! record    = RecHdr | payload (len bytes) | zero padding to 8
//! RecHdr    { u16 type; u16 flags; u32 len; u32 id; u32 arg }                           16 B
//! ```
//!
//! **Descriptors.** A message's descriptors never travel as descriptors: each
//! one becomes a `Desc` saying what it was and how the far side rebuilds it,
//! and the descriptor table is the frame's FIFO of them. `WAYLAND` records
//! consume it in order, each message taking as many as its signature has
//! `fd` arguments; a record's `arg` is how many it consumes in total, which the
//! receiver checks against its own count. Every desc in a frame belongs to one
//! of its `WAYLAND` records, and records whose payload a desc refers to (a
//! `BLOB`'s bytes) come before the `WAYLAND` record that consumes it -- possibly
//! in an earlier frame.
//!
//! | kind | g→h (normal mode) | h→g |
//! |---|---|---|
//! | `DMABUF` | daemon: `fd` = the client's dma-buf; kernel: `a`,`b` = the proxy's (owner handle, host GEM), `fd` = -1 | backend: `a` = a Dmabuf backend handle, `c` = size; kernel imports it and sets `fd` |
//! | `SHM_POOL` | `c` = pool size; the far side creates the memfd | same (export mode) |
//! | `BLOB` | `a` = blob id, `c` = length; bytes in earlier `BLOB` records | same |
//! | `STREAM` | `a` = stream id, `b` = the sink's first credit (0: `WINDOW`; only to a peer that said `HELLO_STREAM_WINDOW`); the far side makes a pipe | same |
//! | `DRM_FILE` | refused | backend: `a` = backend handle, `b` = its `HK_*`; kernel adopts it and sets `fd` |
//! | `SYNCOBJ` | daemon: `fd` = the client's syncobj; kernel: `a` = the backend `Syncobj` handle behind it (a host-handle file), `fd` = -1 | not carried (export mode hides the global) |
//!
//! `DESC_F_INVALID` marks a descriptor that could not be carried (a dma-buf
//! the guest kernel does not own, a failed export or adoption): the receiver
//! substitutes an empty sealed memfd, so the message still consumes one
//! descriptor and the compositor or client fails that one request its own way
//! (`zwp_linux_buffer_params_v1.failed`, say) instead of desynchronising.
//!
//! **Records.**
//!
//! | type | id | arg | payload |
//! |---|---|---|---|
//! | `HELLO` | 0 | 0 | `Hello` (first record each way) |
//! | `WAYLAND` | 0 | descs consumed | whole Wayland messages, never a partial one |
//! | `STREAM_DATA` | stream | 0 | bytes, at most the credit the sink granted |
//! | `STREAM_EOF` | stream | 0 or errno | none; from the source: end of data; from the sink: stop |
//! | `STREAM_CREDIT` | stream | bytes | none; the sink wrote that many more |
//! | `SHM_SYNC` | `wl_buffer` id | byte offset in the buffer | bytes to store there |
//! | `BLOB` | blob id | byte offset | chunk of the blob |
//! | `ERROR` | object id | `wl_display` error code | message text |
//! | `HANGUP` | 0 | errno or 0 | none; the far connection is gone |
//!
//! Stream and blob ids are allocated by the side that creates them: the guest
//! uses `1..=0x7fff_ffff`, the host sets bit 31.

#![forbid(unsafe_code)]

use std::os::fd::OwnedFd;

pub const FRAME_MAGIC: u32 = u32::from_le_bytes(*b"NVWL");
pub const FRAME_VERSION: u16 = 1;
pub const FRAME_HDR_LEN: usize = 16;
pub const DESC_LEN: usize = 24;
pub const REC_HDR_LEN: usize = 16;
/// Descriptor table limit per frame (also the kernel's).
pub const MAX_DESC: usize = 256;
/// Largest payload one record carries. Keeps any record well inside the
/// smallest frame a guest may ask for, so a frame always makes progress.
pub const MAX_REC_PAYLOAD: usize = 64 * 1024;
/// The smallest `max_bytes` a WL_RECV may ask for.
pub const MIN_FRAME: usize = FRAME_HDR_LEN + 32 * DESC_LEN + REC_HDR_LEN + MAX_REC_PAYLOAD;

/// FrameHdr.flags (h→g): more is queued than fit in this frame.
pub const FRAME_F_MORE: u32 = 1 << 0;

pub const DESC_DMABUF: u16 = 1;
pub const DESC_SHM_POOL: u16 = 2;
pub const DESC_BLOB: u16 = 3;
pub const DESC_STREAM: u16 = 4;
pub const DESC_DRM_FILE: u16 = 5;
pub const DESC_SYNCOBJ: u16 = 6;

pub const DESC_F_INVALID: u16 = 1 << 0;

pub const REC_HELLO: u16 = 1;
pub const REC_WAYLAND: u16 = 2;
pub const REC_STREAM_DATA: u16 = 3;
pub const REC_STREAM_EOF: u16 = 4;
pub const REC_STREAM_CREDIT: u16 = 5;
pub const REC_SHM_SYNC: u16 = 6;
pub const REC_BLOB: u16 = 7;
pub const REC_ERROR: u16 = 8;
pub const REC_HANGUP: u16 = 9;

/// Version of the record protocol carried in HELLO.
pub const WL_PROTO_VERSION: u32 = 1;
/// HELLO caps from the guest: it can adopt DRM files (a lease, the lease
/// device's fd) into guest DRM files.
pub const HELLO_G_DRM_FILE: u32 = 1 << 0;
/// HELLO caps from the guest: it can import host dma-bufs (export mode).
pub const HELLO_G_DMABUF_IMPORT: u32 = 1 << 1;
/// HELLO caps from the guest: its kernel names a client's syncobj by the
/// backend handle of its host syncobj (the backend serves fences), so
/// `wp_linux_drm_syncobj_manager_v1` can be offered.
pub const HELLO_G_SYNCOBJ: u32 = 1 << 2;
/// HELLO caps, either way: a stream's source takes the sink's first credit
/// from its descriptor (`b`), so a sink may grant less than `WINDOW`
/// (`stream.rs`).
pub const HELLO_STREAM_WINDOW: u32 = 1 << 8;

/// OPEN(DEV_WAYLAND) flags, chosen by the guest kernel from the daemon's
/// CONNECT: a connection to the host compositor; the export listener's
/// readiness handle; the next connection a host client made to the export
/// socket.
pub const WL_OPEN_CONNECT: u32 = 0;
pub const WL_OPEN_LISTEN: u32 = 1;
pub const WL_OPEN_ACCEPT: u32 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    pub kind: u16,
    pub flags: u16,
    pub fd: i32,
    pub a: u32,
    pub b: u32,
    pub c: u64,
}

impl Desc {
    pub fn new(kind: u16) -> Self {
        Self {
            kind,
            flags: 0,
            fd: -1,
            a: 0,
            b: 0,
            c: 0,
        }
    }
    pub fn invalid(kind: u16) -> Self {
        Self {
            flags: DESC_F_INVALID,
            ..Self::new(kind)
        }
    }
    pub fn is_invalid(&self) -> bool {
        self.flags & DESC_F_INVALID != 0
    }
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.fd.to_le_bytes());
        out.extend_from_slice(&self.a.to_le_bytes());
        out.extend_from_slice(&self.b.to_le_bytes());
        out.extend_from_slice(&self.c.to_le_bytes());
    }
    pub fn read(b: &[u8]) -> Self {
        Self {
            kind: u16::from_le_bytes(b[0..2].try_into().unwrap()),
            flags: u16::from_le_bytes(b[2..4].try_into().unwrap()),
            fd: i32::from_le_bytes(b[4..8].try_into().unwrap()),
            a: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            b: u32::from_le_bytes(b[12..16].try_into().unwrap()),
            c: u64::from_le_bytes(b[16..24].try_into().unwrap()),
        }
    }
}

/// A descriptor on its way to the channel. `fd` is what the transport still
/// has to turn into the desc's fields: the guest kernel (a client dma-buf),
/// or the backend's handle table (a DRM file or dma-buf received from the
/// host compositor, adopted when the guest takes the frame).
#[derive(Debug)]
pub struct DescOut {
    pub desc: Desc,
    pub fd: Option<OwnedFd>,
}

impl DescOut {
    pub fn plain(desc: Desc) -> Self {
        Self { desc, fd: None }
    }
}

/// One record with the descriptors it consumes: the unit frames are packed
/// from, so a `WAYLAND` record never leaves its descriptors behind.
#[derive(Debug)]
pub struct Unit {
    pub rec: Vec<u8>,
    pub descs: Vec<DescOut>,
}

impl Unit {
    pub fn bytes(&self) -> usize {
        self.rec.len() + self.descs.len() * DESC_LEN
    }
}

fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// Encode one record.
pub fn record(ty: u16, id: u32, arg: u32, payload: &[u8]) -> Vec<u8> {
    let mut r = Vec::with_capacity(REC_HDR_LEN + pad8(payload.len()));
    r.extend_from_slice(&ty.to_le_bytes());
    r.extend_from_slice(&0u16.to_le_bytes());
    r.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    r.extend_from_slice(&id.to_le_bytes());
    r.extend_from_slice(&arg.to_le_bytes());
    r.extend_from_slice(payload);
    r.resize(REC_HDR_LEN + pad8(payload.len()), 0);
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub caps: u32,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(16);
        p.extend_from_slice(&self.version.to_le_bytes());
        p.extend_from_slice(&self.caps.to_le_bytes());
        p.extend_from_slice(&[0u8; 8]);
        p
    }
    pub fn decode(p: &[u8]) -> Option<Self> {
        if p.len() < 8 {
            return None;
        }
        Some(Self {
            version: u32::from_le_bytes(p[0..4].try_into().unwrap()),
            caps: u32::from_le_bytes(p[4..8].try_into().unwrap()),
        })
    }
}

/// Pack as many whole units from the front of `q` as fit in `max_bytes` and
/// `max_desc`. The descriptors' pending fds are returned beside the frame, in
/// descriptor order, for the transport to resolve.
pub fn pack(
    q: &mut std::collections::VecDeque<Unit>,
    max_bytes: usize,
    max_desc: usize,
    more_flag: bool,
) -> (Vec<u8>, Vec<Option<OwnedFd>>) {
    let max_desc = max_desc.min(MAX_DESC);
    let mut descs: Vec<Desc> = Vec::new();
    let mut fds = Vec::new();
    let mut recs = Vec::new();
    let mut used = FRAME_HDR_LEN;
    while let Some(u) = q.front() {
        if used + u.bytes() > max_bytes || descs.len() + u.descs.len() > max_desc {
            break;
        }
        let u = q.pop_front().unwrap();
        used += u.bytes();
        recs.extend_from_slice(&u.rec);
        for d in u.descs {
            descs.push(d.desc);
            fds.push(d.fd);
        }
    }
    let flags = if more_flag && !q.is_empty() {
        FRAME_F_MORE
    } else {
        0
    };
    let mut f = Vec::with_capacity(used);
    f.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    f.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    f.extend_from_slice(&(descs.len() as u16).to_le_bytes());
    f.extend_from_slice(&(recs.len() as u32).to_le_bytes());
    f.extend_from_slice(&flags.to_le_bytes());
    for d in &descs {
        d.write(&mut f);
    }
    f.extend_from_slice(&recs);
    (f, fds)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    Short,
    Magic,
    Version,
    TooManyDescs,
    Length,
    Record,
}

/// A decoded frame, borrowing the input.
#[derive(Debug)]
pub struct Frame<'a> {
    pub flags: u32,
    pub descs: Vec<Desc>,
    pub records: &'a [u8],
}

pub fn decode(f: &[u8]) -> Result<Frame<'_>, FrameError> {
    if f.len() < FRAME_HDR_LEN {
        return Err(FrameError::Short);
    }
    if u32::from_le_bytes(f[0..4].try_into().unwrap()) != FRAME_MAGIC {
        return Err(FrameError::Magic);
    }
    if u16::from_le_bytes(f[4..6].try_into().unwrap()) != FRAME_VERSION {
        return Err(FrameError::Version);
    }
    let ndesc = u16::from_le_bytes(f[6..8].try_into().unwrap()) as usize;
    let rec_len = u32::from_le_bytes(f[8..12].try_into().unwrap()) as usize;
    let flags = u32::from_le_bytes(f[12..16].try_into().unwrap());
    if ndesc > MAX_DESC {
        return Err(FrameError::TooManyDescs);
    }
    let dend = FRAME_HDR_LEN + ndesc * DESC_LEN;
    if dend.checked_add(rec_len) != Some(f.len()) {
        return Err(FrameError::Length);
    }
    let descs = f[FRAME_HDR_LEN..dend]
        .as_chunks::<DESC_LEN>()
        .0
        .iter()
        .map(|d| Desc::read(d))
        .collect();
    let records = &f[dend..];
    // Validate the record chain up front so consumers can iterate without
    // re-checking.
    let mut it = Records { buf: records };
    for r in &mut it {
        r?;
    }
    Ok(Frame {
        flags,
        descs,
        records,
    })
}

#[derive(Clone, Copy, Debug)]
pub struct Rec<'a> {
    pub ty: u16,
    pub id: u32,
    pub arg: u32,
    pub payload: &'a [u8],
}

pub struct Records<'a> {
    pub buf: &'a [u8],
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<Rec<'a>, FrameError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        if self.buf.len() < REC_HDR_LEN {
            self.buf = &[];
            return Some(Err(FrameError::Record));
        }
        let b = self.buf;
        let ty = u16::from_le_bytes(b[0..2].try_into().unwrap());
        let len = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        let id = u32::from_le_bytes(b[8..12].try_into().unwrap());
        let arg = u32::from_le_bytes(b[12..16].try_into().unwrap());
        let total = REC_HDR_LEN + pad8(len);
        if len > MAX_REC_PAYLOAD || total > b.len() {
            self.buf = &[];
            return Some(Err(FrameError::Record));
        }
        self.buf = &b[total..];
        Some(Ok(Rec {
            ty,
            id,
            arg,
            payload: &b[REC_HDR_LEN..REC_HDR_LEN + len],
        }))
    }
}

impl<'a> Frame<'a> {
    pub fn records(&self) -> impl Iterator<Item = Rec<'a>> {
        // decode() validated the chain.
        Records { buf: self.records }.map(|r| r.unwrap())
    }
}
