//! IOCTL2 end to end, as close as it gets without a VM: the guest's
//! interpreter against the whole backend.
//!
//! `Guest` below is `driver/nvgpu_i2.c` transliterated function for function
//! (`nvgpu_i2_gather`/`_walk`/`_new_buf`/`_translate`/`_build` on the way out,
//! `nvgpu_i2_parse`/`_restore`/`_outputs`/`_copy_back` on the way back),
//! walking the Rust copy of the generated tables -- which the regeneration
//! test in `abi::schema` holds equal to the C copy. It builds the request byte
//! for byte as the driver does, including the header the transport sends and
//! the response capacity it posts, and reads the reply the way the driver
//! reads it, down to what lands in the caller's memory.
//!
//! In between is the real backend: `NvidiaBackend::serve` (the class from the
//! target's kind, the -EMSGSIZE check against the posted capacity,
//! `xfer::prepare`), `PendingIoctl2::execute` against a fake kernel, and
//! `finish_ioctl2` with the backend's own `Finisher` -- descriptor adoption,
//! the private-descriptor registry, consumed handles. So a disagreement
//! between the halves about layout, padding, record positions or ownership
//! shows up here as a refused request or a wrong byte in user memory, rather
//! than in a guest.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};

use protocol::messages::*;

use crate::hostfd::{self, HandleKind};
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;
use crate::pump::PumpCmd;
use crate::schema::{self, CopyBack, Dir, Field, Ioctl, Kind, Len, Span, Table};
use crate::session::Outcome;
use crate::xfer::Sys;

// ───────────────────────── the guest, transliterated ─────────────────────────

const HDR: usize = 16;
/// NVGPU_SDIR_IN / NVGPU_SDIR_OUT.
const IN: u8 = 1;
const OUT: u8 = 2;
const MAX_DEPTH: usize = 8;

fn dir_bits(d: Dir) -> u8 {
    match d {
        Dir::In => IN,
        Dir::Out => OUT,
        Dir::InOut => IN | OUT,
    }
}

fn align8(n: u64) -> u64 {
    (n + 7) & !7
}

fn rd(b: &[u8], off: usize, width: usize) -> u64 {
    let mut v = [0u8; 8];
    v[..width].copy_from_slice(&b[off..off + width]);
    u64::from_le_bytes(v)
}

fn wr(b: &mut [u8], off: usize, width: usize, v: u64) {
    b[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

/// A guest process's memory: the regions an ioctl's pointers reach.
#[derive(Default)]
struct UserMem {
    regions: BTreeMap<u64, Vec<u8>>,
}

impl UserMem {
    fn put(&mut self, addr: u64, bytes: &[u8]) {
        self.regions.insert(addr, bytes.to_vec());
    }

    fn region(&mut self, addr: u64, len: usize) -> Result<&mut [u8], i32> {
        let (&base, r) = self
            .regions
            .range_mut(..=addr)
            .next_back()
            .ok_or(libc::EFAULT)?;
        let at = (addr - base) as usize;
        r.get_mut(at..at + len).ok_or(libc::EFAULT)
    }

    /// copy_from_user
    fn read(&mut self, addr: u64, len: usize) -> Result<Vec<u8>, i32> {
        Ok(self.region(addr, len)?.to_vec())
    }

    /// copy_to_user
    fn write(&mut self, addr: u64, bytes: &[u8]) -> Result<(), i32> {
        self.region(addr, bytes.len())?.copy_from_slice(bytes);
        Ok(())
    }

    fn get(&self, addr: u64) -> &[u8] {
        &self.regions[&addr]
    }
}

/// struct nvgpu_i2_kbuf
struct KBuf {
    k: Vec<u8>,
    dir: u8,
    f: Option<&'static Field>,
    parent: usize,
    pbase: usize,
    sent: u64,
    uptr: u64,
}

/// struct nvgpu_i2_slot
struct GSlot {
    f: &'static Field,
    buf: usize,
    off: usize,
    orig: u64,
}

/// What the guest-side hooks (struct nvgpu_i2_ops) did, and what they
/// answer: a guest fd table and a guest GEM table standing in for the KMS
/// workstream's materialisers.
#[derive(Default)]
struct Hooks {
    /// guest fd -> (backend handle, flags)
    fds: HashMap<i64, (u32, u32)>,
    /// guest GEM handle -> (owner, host gem)
    gems: HashMap<u32, (u32, u32)>,
    /// fd_out: (backend handle, kind) materialised as guest fd 100, 101, ...
    fd_outs: Vec<(u32, u32)>,
    /// gem_out: (host gem, size) materialised as guest handle 200, 201, ...
    gem_outs: Vec<(u32, u64)>,
    /// Handles the interpreter closed itself (nvgpu_close_handle_async).
    closed: Vec<u32>,
}

/// struct nvgpu_i2_call plus struct nvgpu_i2_state.
struct Guest {
    table: &'static Table,
    e: &'static Ioctl,
    handle: u32,
    render: u32,
    cmd: u32,
    max_req: u64,
    max_resp: u64,
    bufs: Vec<KBuf>,
    slots: Vec<GSlot>,
    fd: Vec<[u32; 4]>,
    gem: Vec<[u32; 4]>,
    dyn_: Vec<[u32; 4]>,
    in_bytes: u64,
    out_bytes: u64,
    fdo: Vec<[u32; 4]>,
    gemo: Vec<(u32, u32, u32, u64, u32)>,
    ret: i32,
}

impl Guest {
    /// nvgpu_i2_gather, for a call of `class` (call->sclass): a DRM-table
    /// one, or an NVKMS one against `modeset` (dev->schema->modeset), keyed
    /// by the command in the outer struct's first bytes.
    fn gather(
        mem: &mut UserMem,
        class: schema::Class,
        handle: u32,
        render: u32,
        cmd: u32,
        uarg: u64,
    ) -> Result<Self, i32> {
        Self::gather_in(mem, class, None, handle, render, cmd, uarg)
    }

    fn gather_in(
        mem: &mut UserMem,
        class: schema::Class,
        modeset: Option<&'static Table>,
        handle: u32,
        render: u32,
        cmd: u32,
        uarg: u64,
    ) -> Result<Self, i32> {
        let (table, e) = if class == schema::Class::Modeset {
            if cmd != schema::NVKMS_IOCTL_IOWR {
                return Err(libc::ENOTTY);
            }
            let t = modeset.ok_or(libc::ENOTTY)?;
            let prefix = mem.read(uarg, 4)?;
            let e = t
                .lookup_nvkms(rd(&prefix, 0, 4) as u32)
                .ok_or(libc::ENOTTY)?;
            (t, e)
        } else {
            let table = schema::DRM_TABLE;
            let e = table.lookup(class, cmd).ok_or(libc::ENOTTY)?;
            if e.cmd != cmd {
                return Err(libc::EINVAL);
            }
            (table, e)
        };
        let mut g = Guest {
            table,
            e,
            handle,
            render,
            cmd,
            max_req: 256 << 10,
            max_resp: 256 << 10,
            bufs: Vec::new(),
            slots: Vec::new(),
            fd: Vec::new(),
            gem: Vec::new(),
            dyn_: Vec::new(),
            in_bytes: 0,
            out_bytes: 0,
            fdo: Vec::new(),
            gemo: Vec::new(),
            ret: 0,
        };
        let mut dir = 0;
        if cmd & (1 << 30) != 0 {
            dir |= IN; // _IOC_WRITE
        }
        if cmd & (1 << 31) != 0 {
            dir |= OUT; // _IOC_READ
        }
        // NVKMS never writes the outer struct.
        if class == schema::Class::Modeset {
            dir = IN;
        }
        // _IOC_SIZE
        g.new_buf(mem, u64::from((cmd >> 16) & 0x3fff), dir, uarg)?;
        g.walk(mem, 0, 0, e.fields, 0)?;
        Ok(g)
    }

    fn rd(&self, b: usize, off: usize, width: usize) -> Result<u64, i32> {
        let kb = &self.bufs[b].k;
        if width > kb.len() || off > kb.len() - width {
            return Err(libc::EINVAL);
        }
        Ok(rd(kb, off, width))
    }

    fn wr(&mut self, b: usize, off: usize, width: usize, v: u64) {
        let kb = &mut self.bufs[b].k;
        if width <= kb.len() && off <= kb.len() - width {
            wr(kb, off, width, v);
        }
    }

    /// nvgpu_i2_new_buf
    fn new_buf(&mut self, mem: &mut UserMem, len: u64, dir: u8, uptr: u64) -> Result<usize, i32> {
        if self.bufs.len() >= I2_MAX_BUFS as usize {
            return Err(libc::E2BIG);
        }
        if dir & IN != 0 {
            self.in_bytes += align8(len);
        }
        if dir & OUT != 0 {
            self.out_bytes += align8(len);
        }
        if self.in_bytes > self.max_req || self.out_bytes > self.max_resp {
            return Err(libc::E2BIG);
        }
        let k = if dir & IN != 0 && len > 0 {
            mem.read(uptr, len as usize)?
        } else {
            vec![0; len as usize]
        };
        self.bufs.push(KBuf {
            k,
            dir,
            f: None,
            parent: 0,
            pbase: 0,
            sent: 0,
            uptr,
        });
        Ok(self.bufs.len() - 1)
    }

    /// nvgpu_i2_len
    fn len(
        &self,
        f: &Field,
        b: usize,
        base: usize,
        first: u16,
        created: &[i64],
    ) -> Result<u64, i32> {
        let Kind::Ptr { len, max, .. } = f.kind else {
            unreachable!()
        };
        Ok(match len {
            Len::Const(n) => u64::from(n),
            Len::Count { off, width, elem } => self
                .rd(b, base + off as usize, width as usize)?
                .checked_mul(u64::from(elem))
                .ok_or(libc::E2BIG)?,
            Len::Sum { field, elem } => {
                let src = created.get((field - first) as usize).copied().unwrap_or(-1);
                let sum: u64 = if src >= 0 {
                    self.bufs[src as usize]
                        .k
                        .chunks_exact(4)
                        .map(|c| u64::from(u32::from_le_bytes(c.try_into().unwrap())))
                        .sum()
                } else {
                    0
                };
                sum.checked_mul(u64::from(elem)).ok_or(libc::E2BIG)?
            }
            Len::NvkmsParams => {
                let n = self.rd(b, base + 4, 4)?;
                if n != u64::from(max) {
                    return Err(libc::EINVAL);
                }
                n
            }
        })
    }

    /// nvgpu_i2_walk
    fn walk(
        &mut self,
        mem: &mut UserMem,
        b: usize,
        base: usize,
        span: Span,
        depth: usize,
    ) -> Result<(), i32> {
        if depth > MAX_DEPTH {
            return Err(libc::EINVAL);
        }
        let fields = self.table.fields(span);
        let mut created = vec![-1i64; fields.len()];
        for (i, f) in fields.iter().enumerate() {
            if let Some(c) = f.cond {
                let v = self.rd(b, base + c.off as usize, 4)? as u32;
                if !c.holds(v) {
                    continue;
                }
            }
            let at = base + f.off as usize;
            match f.kind {
                Kind::Ptr {
                    dir,
                    copyback,
                    max,
                    stride,
                    children,
                    ..
                } => {
                    let v = self.rd(b, at, 8)?;
                    let len = self.len(f, b, base, span.first, &created)?;
                    self.slots.push(GSlot {
                        f,
                        buf: b,
                        off: at,
                        orig: v,
                    });
                    if v == 0 || len == 0 {
                        continue;
                    }
                    if len > u64::from(max) {
                        return Err(libc::E2BIG);
                    }
                    if children.len > 0 && (stride == 0 || len % u64::from(stride) != 0) {
                        return Err(libc::EINVAL);
                    }
                    let nb = self.new_buf(mem, len, dir_bits(dir), v)?;
                    created[i] = nb as i64;
                    self.bufs[nb].f = Some(f);
                    self.bufs[nb].parent = b;
                    self.bufs[nb].pbase = base;
                    let counted = match copyback {
                        CopyBack::Partial { off, width, .. }
                        | CopyBack::AllOrNothing { off, width, .. }
                        | CopyBack::Exact { off, width } => Some((off, width)),
                        _ => None,
                    };
                    if let Some((off, width)) = counted {
                        self.bufs[nb].sent = self.rd(b, base + off as usize, width as usize)?;
                    }
                    if children.len > 0 {
                        for e in 0..(len / u64::from(stride)) as usize {
                            self.walk(mem, nb, e * stride as usize, children, depth + 1)?;
                        }
                    }
                }
                Kind::Array {
                    count,
                    stride,
                    limit,
                    children,
                } => {
                    let v = match limit {
                        schema::Limit::All => 0,
                        schema::Limit::Count { off, width } => {
                            self.rd(b, base + off as usize, width as usize)?
                        }
                        schema::Limit::Planes { off } => self.rd(b, base + off as usize, 4)?,
                    };
                    let n = limit.elements(count, v, self.table.planes);
                    for e in 0..n as usize {
                        self.walk(mem, b, at + e * stride as usize, children, depth + 1)?;
                    }
                }
                _ => {
                    let v = self.rd(b, at, f.width() as usize)?;
                    self.slots.push(GSlot {
                        f,
                        buf: b,
                        off: at,
                        orig: v,
                    });
                }
            }
        }
        Ok(())
    }

    /// nvgpu_i2_translate, with the hooks' answers from `h`.
    fn translate(&mut self, h: &Hooks) -> Result<(), i32> {
        for i in 0..self.slots.len() {
            let (f, b, off, orig) = {
                let s = &self.slots[i];
                (s.f, s.buf, s.off, s.orig)
            };
            match f.kind {
                Kind::FdIn { width, none, .. } => {
                    let v = if width == 4 {
                        orig as u32 as i32 as i64
                    } else {
                        orig as i64
                    };
                    if v == i64::from(none) {
                        continue;
                    }
                    let &(handle, flags) = h.fds.get(&v).ok_or(libc::EBADF)?;
                    self.wr(b, off, width as usize, none as i64 as u64);
                    self.fd.push([b as u32, off as u32, handle, flags]);
                }
                Kind::GemIn { .. } => {
                    if orig == 0 {
                        continue;
                    }
                    let &(owner, gem) = h.gems.get(&(orig as u32)).ok_or(libc::ENOENT)?;
                    if gem == 0 {
                        return Err(libc::EINVAL);
                    }
                    self.wr(b, off, 4, u64::from(gem));
                    self.gem.push([b as u32, off as u32, owner, gem]);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn count(&self, pick: impl Fn(&Kind) -> bool) -> usize {
        self.slots.iter().filter(|s| pick(&s.f.kind)).count()
    }

    fn max_fdo(&self) -> usize {
        self.count(|k| matches!(k, Kind::FdOut { .. })) + self.dyn_.len()
    }

    fn max_gemo(&self) -> usize {
        self.count(|k| matches!(k, Kind::GemOut))
    }

    /// The capacity nvgpu_i2_ioctl posts for the reply (resp_len).
    fn resp_len(&self) -> usize {
        HDR + 32 + self.out_bytes as usize + 16 * self.max_fdo() + 24 * self.max_gemo()
    }

    /// Whether nvgpu_i2_ioctl sends this with NVGPU_XF_EXECUTOR.
    fn executor(&self) -> bool {
        self.e.class != schema::Class::Render || self.e.exec == schema::Exec::Executor
    }

    /// nvgpu_i2_build: struct nvgpu_i2_head, buf_len[], the records, the IN
    /// bytes padded to 8.
    fn build(&self) -> Vec<u8> {
        let mut t = Vec::new();
        let words = |t: &mut Vec<u8>, ws: &[u32]| {
            ws.iter()
                .for_each(|w| t.extend_from_slice(&w.to_le_bytes()))
        };
        // nvgpu_msg_hdr (req_id is the transport's to fill in).
        words(&mut t, &[MsgType::Ioctl2 as u32, self.handle, 0, 0x1234]);
        words(
            &mut t,
            &[
                self.cmd,
                0,
                self.bufs.len() as u32,
                self.fd.len() as u32,
                self.gem.len() as u32,
                self.dyn_.len() as u32,
                self.in_bytes as u32,
                self.render,
            ],
        );
        for kb in &self.bufs {
            words(&mut t, &[kb.k.len() as u32]);
        }
        for r in self.fd.iter().chain(&self.gem).chain(&self.dyn_) {
            words(&mut t, r);
        }
        for kb in self.bufs.iter().filter(|kb| kb.dir & IN != 0) {
            t.extend_from_slice(&kb.k);
            t.resize(
                t.len() + (align8(kb.k.len() as u64) as usize - kb.k.len()),
                0,
            );
        }
        t
    }

    fn at_slot(&self, pick: impl Fn(&Kind) -> bool, b: u32, off: u32) -> bool {
        self.slots
            .iter()
            .any(|s| pick(&s.f.kind) && s.buf == b as usize && s.off == off as usize)
    }

    /// nvgpu_i2_parse. On a non-zero status, `h.closed` gets the consumed
    /// handles (nvgpu_i2_drop_consumed).
    fn parse(&mut self, r: &[u8], h: &mut Hooks) -> Result<(), i32> {
        let used = r.len();
        if used < HDR {
            return Err(libc::EPROTO);
        }
        let status = rd(r, 8, 4) as u32 as i32;
        if status != 0 {
            for f in &self.fd {
                if f[3] & I2_FD_CONSUME != 0 {
                    h.closed.push(f[2]);
                }
            }
            return Err(-status);
        }
        if used < HDR + 32 {
            return Err(libc::EPROTO);
        }
        let w = |i: usize| rd(r, HDR + 4 * i, 4) as u32;
        let (ret, nbuf, nfd, ngem, dlen) = (w(0) as i32, w(1), w(2) as usize, w(3) as usize, w(4));
        let need = HDR + 32 + self.out_bytes as usize + nfd * 16 + ngem * 24;
        if nbuf as usize != self.bufs.len()
            || u64::from(dlen) != self.out_bytes
            || nfd > self.max_fdo()
            || ngem > self.max_gemo()
            || used < need
            || ret < -4095
        {
            return Err(libc::EPROTO);
        }
        let mut off = HDR + 32;
        for kb in self.bufs.iter_mut().filter(|kb| kb.dir & OUT != 0) {
            let n = kb.k.len();
            kb.k.copy_from_slice(&r[off..off + n]);
            off += align8(n as u64) as usize;
        }
        for _ in 0..nfd {
            self.fdo
                .push(std::array::from_fn(|j| rd(r, off + 4 * j, 4) as u32));
            off += 16;
        }
        for _ in 0..ngem {
            let q = |j: usize| rd(r, off + 4 * j, 4) as u32;
            self.gemo.push((q(0), q(1), q(2), rd(r, off + 16, 8), 0));
            off += 24;
        }
        for (i, o) in self.fdo.iter().enumerate() {
            let at_dyn = self.dyn_.iter().any(|d| d[1] == o[0] && d[2] == o[1]);
            let ok = (self.at_slot(|k| matches!(k, Kind::FdOut { .. }), o[0], o[1]) || at_dyn)
                && self.fdo[..i].iter().all(|p| p[0] != o[0] || p[1] != o[1]);
            if !ok {
                return Err(libc::EPROTO);
            }
        }
        for (i, o) in self.gemo.iter().enumerate() {
            let ok = o.2 != 0
                && self.at_slot(|k| matches!(k, Kind::GemOut), o.0, o.1)
                && self.gemo[..i].iter().all(|p| p.0 != o.0 || p.1 != o.1);
            if !ok {
                return Err(libc::EPROTO);
            }
        }
        self.ret = ret;
        Ok(())
    }

    /// nvgpu_i2_restore
    fn restore(&mut self) {
        for i in 0..self.slots.len() {
            let (f, b, off, orig) = {
                let s = &self.slots[i];
                (s.f, s.buf, s.off, s.orig)
            };
            match f.kind {
                Kind::Ptr { .. } => self.wr(b, off, 8, orig),
                Kind::FdIn { width, .. } => self.wr(b, off, width as usize, orig),
                Kind::GemIn { .. } => self.wr(b, off, 4, orig),
                Kind::FdOut { width } => self.wr(b, off, width as usize, u64::MAX),
                Kind::GemOut => self.wr(b, off, 4, 0),
                Kind::Array { .. } => {}
            }
        }
    }

    /// nvgpu_i2_outputs
    fn outputs(&mut self, h: &mut Hooks) -> Result<(), i32> {
        for i in 0..self.gemo.len() {
            let (b, off, gem, size, _) = self.gemo[i];
            let guest = match self.gemo[..i].iter().find(|p| p.2 == gem) {
                Some(p) => p.4,
                None => {
                    h.gem_outs.push((gem, size));
                    199 + h.gem_outs.len() as u32
                }
            };
            self.gemo[i].4 = guest;
            self.wr(b as usize, off as usize, 4, u64::from(guest));
        }
        for i in 0..self.fdo.len() {
            let [b, off, handle, kind] = self.fdo[i];
            if handle == 0 {
                return Err(libc::EMFILE);
            }
            h.fd_outs.push((handle, kind));
            let v = 99 + h.fd_outs.len() as u64;
            let width = self
                .slots
                .iter()
                .find(|s| {
                    matches!(s.f.kind, Kind::FdOut { .. })
                        && s.buf == b as usize
                        && s.off == off as usize
                })
                .map(|s| s.f.width() as usize);
            if let Some(width) = width {
                self.wr(b as usize, off as usize, width, v);
            }
        }
        Ok(())
    }

    /// nvgpu_i2_copy_back, through CopyBack::extent (which
    /// nvgpu_i2_copy_extent mirrors line for line).
    fn copy_back(&mut self, mem: &mut UserMem) -> Result<(), i32> {
        for kb in self
            .bufs
            .iter()
            .filter(|kb| kb.dir & OUT != 0 && !kb.k.is_empty())
        {
            let (mut start, mut end) = (0, kb.k.len() as u64);
            if let Some(f) = kb.f {
                let Kind::Ptr { dir, copyback, .. } = f.kind else {
                    unreachable!()
                };
                let left = match copyback {
                    CopyBack::Partial { off, width, .. }
                    | CopyBack::AllOrNothing { off, width, .. }
                    | CopyBack::Exact { off, width }
                    | CopyBack::Written { off, width } => {
                        let p = &self.bufs[kb.parent].k;
                        rd(p, kb.pbase + off as usize, width as usize)
                    }
                    _ => 0,
                };
                (start, end) = copyback.extent(dir, kb.k.len() as u64, self.ret, kb.sent, left);
            }
            if end > start {
                mem.write(kb.uptr + start, &kb.k[start as usize..end as usize])?;
            }
        }
        Ok(())
    }
}

// ───────────────────────────── the host ─────────────────────────────

/// A kernel with three DRM files, told apart by the memfd behind each host
/// descriptor: "e2e-kms" (a lease), "e2e-render", "e2e-modeset".
#[derive(Default)]
struct Kernel {
    /// file -> GEM handle -> object
    gems: HashMap<String, BTreeMap<u32, u64>>,
    /// object -> what GEM_IDENTIFY_OBJECT says it is, NVKMS (0) if absent.
    types: HashMap<u64, u32>,
    dmabufs: HashMap<RawFd, u64>,
    next_dmabuf: RawFd,
    /// What each main ioctl saw, for the test to look at.
    calls: Vec<(String, u32)>,
    /// What CREATE_LEASE hands back as the new descriptor, if not a fresh one.
    lease_fd: Option<RawFd>,
    /// NVKMS commands that reached the host, in order.
    nvkms: Vec<u32>,
    fbs: Vec<(u32, u32)>,
    /// The timeout_nsec a SYNCOBJ_WAIT reached the host with.
    wait_timeout: Option<i64>,
    /// What "e2e-kms"'s lease looks like to GET_LEASE now.
    lease: LeaseState,
    /// GET_LEASE calls, and GETRESOURCES calls asking after the lease, kept
    /// out of `calls`.
    lease_probes: u32,
    /// completionNotifier.awaken of each layer of the last one-element FLIP
    /// the host saw.
    flip_awaken: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LeaseState {
    #[default]
    Held,
    /// The lessor revoked it: the lessee's object idr is empty.
    Revoked,
    /// The lessor dropped master: DRM_MASTER calls on the lessee fail, and
    /// the lease still holds its objects.
    LessorNotMaster,
    /// The lessor closed (drm_master_release): no master, and the lease
    /// emptied (drm_auth.c:351-357).
    LessorGone,
}

struct Fake(Mutex<Kernel>);

fn file_of(fd: RawFd) -> String {
    let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap_or_default();
    let link = link.to_string_lossy();
    link.trim_start_matches("/memfd:")
        .split(' ')
        .next()
        .unwrap_or("")
        .to_string()
}

unsafe fn peek(p: *const u8, off: usize, width: usize) -> u64 {
    let mut v = [0u8; 8];
    unsafe { std::ptr::copy_nonoverlapping(p.add(off), v.as_mut_ptr(), width) };
    u64::from_le_bytes(v)
}

unsafe fn poke(p: *mut u8, off: usize, width: usize, v: u64) {
    unsafe { std::ptr::copy_nonoverlapping(v.to_le_bytes().as_ptr(), p.add(off), width) };
}

const PRIME_HANDLE_TO_FD: u32 = 0xc00c_642d;
const PRIME_FD_TO_HANDLE: u32 = 0xc00c_642e;
const GEM_CLOSE: u32 = 0x4008_6409;
const IDENTIFY: u32 = 0xc008_644e;
const GETRESOURCES: u32 = 0xc040_64a0;
const ADDFB2: u32 = 0xc068_64b8;
const CREATE_LEASE: u32 = 0xc018_64c6;
const GRANT: u32 = 0xc00c_6452;
const SYNCOBJ_WAIT: u32 = 0xc028_64c3;
const SYNCOBJ_EVENTFD: u32 = 0xc018_64cf;
const NVKMS: u32 = schema::NVKMS_IOCTL_IOWR;
const LUT: usize = 6144;
const GET_LEASE: u32 = crate::kms::DRM_IOCTL_MODE_GET_LEASE;
const REVOKE_LEASE: u32 = 0xc004_64c9;
const SET_MASTER: u32 = 0x0000_641e;
const DROP_MASTER: u32 = 0x0000_641f;
const GETPROPERTY: u32 = 0xc040_64aa;
const NV12: u32 = u32::from_le_bytes(*b"NV12");

impl Sys for Fake {
    fn ioctl(&self, fd: RawFd, cmd: u32, arg: *mut u8) -> i32 {
        let mut k = self.0.lock().unwrap();
        let file = file_of(fd);
        // SAFETY: `arg` is the buffer xfer built for `cmd`; every pointer in
        // it aims at another of its buffers (or is NULL).
        unsafe {
            match cmd {
                PRIME_HANDLE_TO_FD => {
                    let h = peek(arg, 0, 4) as u32;
                    let Some(&obj) = k.gems.get(&file).and_then(|g| g.get(&h)) else {
                        return -libc::ENOENT;
                    };
                    k.next_dmabuf += 1;
                    let d = 900_000 + k.next_dmabuf;
                    k.dmabufs.insert(d, obj);
                    poke(arg, 8, 4, d as u64);
                    0
                }
                PRIME_FD_TO_HANDLE => {
                    let Some(&obj) = k.dmabufs.get(&(peek(arg, 8, 4) as RawFd)) else {
                        return -libc::EBADF;
                    };
                    let g = k.gems.entry(file).or_default();
                    let h = match g.iter().find(|(_, o)| **o == obj) {
                        Some((&h, _)) => h,
                        None => (1..).find(|h| !g.contains_key(h)).unwrap(),
                    };
                    g.insert(h, obj);
                    poke(arg, 0, 4, h as u64);
                    0
                }
                GEM_CLOSE => {
                    let h = peek(arg, 0, 4) as u32;
                    match k.gems.get_mut(&file).and_then(|g| g.remove(&h)) {
                        Some(_) => 0,
                        None => -libc::EINVAL,
                    }
                }
                // Per object, as nv_drm_gem_identify_object_ioctl answers it
                // (nvidia-drm-gem.c:310-345): the type of whatever the handle
                // names in this file, UNKNOWN for a handle it does not hold.
                IDENTIFY => {
                    let h = peek(arg, 0, 4) as u32;
                    let t = match k.gems.get(&file).and_then(|g| g.get(&h)) {
                        Some(obj) => k
                            .types
                            .get(obj)
                            .copied()
                            .unwrap_or(hostfd::NV_GEM_OBJECT_NVKMS),
                        None => hostfd::NV_GEM_OBJECT_UNKNOWN,
                    };
                    poke(arg, 4, 4, u64::from(t));
                    0
                }
                // drm_mode_get_lease_ioctl with count_objects 0: the count
                // only (drm_lease.c:636-684). DRM_MASTER: -EACCES once the
                // lessor is no longer master (drm_ioctl.c:746).
                GET_LEASE if file == "e2e-kms" => {
                    k.lease_probes += 1;
                    assert_eq!(peek(arg, 0, 4), 0, "a count, never the ids");
                    match k.lease {
                        LeaseState::Held => poke(arg, 0, 4, 3),
                        LeaseState::Revoked => poke(arg, 0, 4, 0),
                        LeaseState::LessorNotMaster | LeaseState::LessorGone => {
                            return -libc::EACCES;
                        }
                    }
                    0
                }
                // drm_mode_getresources on a lessee whose lessor is not
                // master (it needs none): only what the lease still holds,
                // through drm_lease_held (drm_mode_config.c:131-172).
                GETRESOURCES if file == "e2e-kms" && k.lease != LeaseState::Held => {
                    k.lease_probes += 1;
                    assert!((0..48).all(|i| *arg.add(i) == 0), "counts only");
                    let (crtcs, connectors) = match k.lease {
                        LeaseState::LessorNotMaster => (2, 3),
                        _ => (0, 0),
                    };
                    poke(arg, 36, 4, crtcs);
                    poke(arg, 40, 4, connectors);
                    poke(arg, 44, 4, 1);
                    0
                }
                _ => {
                    k.calls.push((file.clone(), cmd));
                    main_ioctl(&mut k, &file, cmd, arg)
                }
            }
        }
    }

    fn close(&self, fd: RawFd) {
        self.0.lock().unwrap().dmabufs.remove(&fd);
    }

    fn size_of(&self, _: RawFd) -> i64 {
        4096
    }
}

/// The ioctls the tests make, as the host kernel answers them.
unsafe fn main_ioctl(k: &mut Kernel, file: &str, cmd: u32, arg: *mut u8) -> i32 {
    unsafe {
        match (file, cmd) {
            // drm_mode_getresources: each list gets min(count, actual) ids,
            // and every count becomes the actual one.
            ("e2e-kms", GETRESOURCES) => {
                let lists: [&[u32]; 4] = [&[], &[41, 42], &[51, 52, 53], &[61]];
                for (i, ids) in lists.iter().enumerate() {
                    let p = peek(arg, 8 * i, 8) as *mut u8;
                    let room = peek(arg, 32 + 4 * i, 4) as usize;
                    for (j, id) in ids.iter().enumerate().take(room) {
                        assert!(!p.is_null(), "a non-zero count always has a buffer");
                        poke(p, 4 * j, 4, u64::from(*id));
                    }
                    poke(arg, 32 + 4 * i, 4, ids.len() as u64);
                }
                poke(arg, 48, 4, 320);
                0
            }
            // Framebuffers are made from GEM handles of the lease file itself:
            // the proxies' objects, re-homed there for this job.
            ("e2e-kms", ADDFB2) => {
                for plane in 0..2 {
                    let h = peek(arg, 20 + 4 * plane, 4) as u32;
                    if !k.gems.get("e2e-kms").is_some_and(|g| g.contains_key(&h)) {
                        return -libc::ENOENT;
                    }
                }
                let id = 77 + k.fbs.len() as u32;
                k.fbs.push((id, peek(arg, 12, 4) as u32));
                poke(arg, 0, 4, u64::from(id));
                0
            }
            ("e2e-kms", CREATE_LEASE) => {
                let ids = peek(arg, 0, 8) as *const u8;
                let n = peek(arg, 8, 4) as usize;
                assert_eq!(n, 2);
                assert_eq!(peek(ids, 0, 4), 41);
                assert_eq!(peek(ids, 4, 4), 51);
                let fd = match k.lease_fd {
                    Some(fd) => fd,
                    None => hostfd::new_eventfd().unwrap().into_raw_fd(),
                };
                poke(arg, 16, 4, 9);
                poke(arg, 20, 4, fd as u32 as u64);
                0
            }
            // drm_syncobj_wait_ioctl with a zero timeout: nothing signalled,
            // so -ETIME at once (drm_syncobj.c:1156-1159).
            ("e2e-render", SYNCOBJ_WAIT) => {
                k.wait_timeout = Some(peek(arg, 8, 8) as i64);
                -libc::ETIME
            }
            // drm_setmaster_ioctl / drm_dropmaster_ioctl on a card: no
            // argument at all, only the answer.
            ("e2e-card", SET_MASTER) | ("e2e-card", DROP_MASTER) => 0,
            // drm_mode_revoke_lease_ioctl: the lessee's objects go
            // (drm_lease.c:700-730); the lessee file stays open.
            ("e2e-card", REVOKE_LEASE) => {
                assert_eq!(peek(arg, 0, 4), 9, "the lessee id the guest named");
                k.lease = LeaseState::Revoked;
                0
            }
            // drm_mode_getproperty with every count 0: name and flags only
            // (drm_property.c:458), which is how the guest classifies an id.
            (_, GETPROPERTY) => {
                assert_eq!(peek(arg, 0, 8), 0, "no values pointer");
                assert_eq!(peek(arg, 8, 8), 0, "no enum pointer");
                let name = match peek(arg, 16, 4) {
                    7 => &b"IN_FENCE_FD"[..],
                    8 => b"CRTC_ID",
                    _ => return -libc::ENOENT,
                };
                std::ptr::copy_nonoverlapping(name.as_ptr(), arg.add(24), name.len());
                0
            }
            ("e2e-kms", GRANT) => {
                let fd = peek(arg, 0, 4) as i32;
                assert!(
                    file_of(fd).starts_with("e2e-modeset"),
                    "the modeset file, by our number"
                );
                0
            }
            ("e2e-modeset", NVKMS) => nvkms_ioctl(k, arg),
            _ => -libc::ENOTTY,
        }
    }
}

/// NVKMS 610.57.04 as the tests use it (offsets: gen/nvkms/610.57.04.json).
/// Every failure is -EPERM, with the reply half written all the same
/// (nvidia-modeset-linux.c:1539, nvkms.c:5220-5243).
unsafe fn nvkms_ioctl(k: &mut Kernel, arg: *mut u8) -> i32 {
    unsafe {
        let (cmd, size) = (peek(arg, 0, 4) as u32, peek(arg, 4, 4) as u32);
        let p = peek(arg, 8, 8) as *mut u8;
        k.nvkms.push(cmd);
        match (cmd, size) {
            // ALLOC_DEVICE: deviceHandle 1, disp 0 is 0x100.
            (0, 1440) => {
                poke(p, 628, 4, 1);
                poke(p, 644, 4, 0x100);
                0
            }
            // VALIDATE_MODE: five bytes of pInfoString, and a failure.
            (8, 656) => {
                let s = peek(p, 296, 8) as *mut u8;
                assert!(!s.is_null(), "a buffer of infoStringSize bytes");
                std::ptr::copy_nonoverlapping(b"hello".as_ptr(), s, 5);
                poke(p, 456, 4, 5);
                -libc::EPERM
            }
            (11, 20) => 0, // MOVE_CURSOR
            // FLIP of one head: what each layer's awaken reached the host as.
            (15, 3104) if peek(p, 16, 4) == 1 => {
                let heads = peek(p, 8, 8) as *const u8;
                k.flip_awaken = (0..8).map(|l| *heads.add(216 + l * 592 + 52)).collect();
                0
            }
            // FLIP: two heads, head 0 with an input LUT, head 1 with an
            // output one; refused, with a flipResult.
            (15, 3104) => {
                let heads = peek(p, 8, 8) as *const u8;
                assert_eq!(peek(p, 16, 4), 2);
                assert_eq!(peek(heads, 4, 4), 5, "head 0's head");
                assert_eq!(peek(heads, 4952 + 4, 4), 6, "head 1's head");
                let in0 = peek(heads, 88, 8) as *const u8;
                let out1 = peek(heads, 4952 + 104, 8) as *const u8;
                assert_eq!((peek(heads, 104, 8), peek(heads, 4952 + 88, 8)), (0, 0));
                assert_eq!((*in0, *in0.add(LUT - 1)), (0x11, 0x11));
                assert_eq!((*out1, *out1.add(LUT - 1)), (0x22, 0x22));
                poke(p, 28, 4, 0x77);
                -libc::EPERM
            }
            // REGISTER_SURFACE: format 19 (Y8___U8V8_N420) has two planes,
            // so two descriptors of ours and the third as the caller left it.
            (17, 152) => {
                // useFd == FALSE names RM handles, which a user client may
                // not (nvkms.c:2727-2730).
                if peek(p, 4, 1) == 0 {
                    return -libc::EPERM;
                }
                for plane in [16, 48] {
                    let fd = peek(p, plane, 4) as i32;
                    assert_eq!(file_of(fd), "e2e-ctl", "plane at {plane}");
                }
                assert_eq!(peek(p, 80, 4), 0, "the unused third plane");
                poke(p, 144, 4, 0x55);
                0
            }
            // ACQUIRE_PERMISSIONS of a grant file: MODESET on head 1 for
            // dpy bit 3.
            (41, 28) => {
                let fd = peek(p, 0, 4) as i32;
                assert_eq!(file_of(fd), "e2e-modeset-grant");
                poke(p, 4, 4, 1);
                poke(p, 8, 4, 2);
                poke(p, 12 + 4, 4, 1 << 3);
                0
            }
            _ => -libc::EPERM,
        }
    }
}

// ───────────────────────────── the tests ─────────────────────────────

fn memfd(name: &std::ffi::CStr) -> OwnedFd {
    // SAFETY: a NUL-terminated name; the result is owned below.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: a descriptor memfd_create just returned.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

struct World {
    be: NvidiaBackend,
    fake: Arc<Fake>,
    kms: u32,
    render: u32,
    modeset: u32,
    mem: UserMem,
    hooks: Hooks,
}

fn world() -> World {
    let mut be = NvidiaBackend::for_test();
    let fake = Arc::new(Fake(Mutex::new(Kernel::default())));
    be.xfer_sys = fake.clone();
    let kms = be.adopt_for_test(memfd(c"e2e-kms"), HandleKind::DrmLease(0));
    let render = be.adopt_for_test(memfd(c"e2e-render"), HandleKind::DriRender(0));
    let modeset = be.adopt_for_test(memfd(c"e2e-modeset"), HandleKind::Dev(DeviceKind::Modeset));
    // What a successful HELLO leaves behind. (The guest's FRESH one would
    // also close the handles just made, which a guest would have opened
    // after it.)
    be.session.v2 = true;
    World {
        be,
        fake,
        kms,
        render,
        modeset,
        mem: UserMem::default(),
        hooks: Hooks::default(),
    }
}

impl World {
    /// One call all the way through: gather, send (with the capacity the
    /// guest posts, less `short`), serve, execute, finish, parse, copy back.
    /// Returns what nvgpu_i2_ioctl would.
    fn call(&mut self, target: u32, cmd: u32, uarg: u64, short: usize) -> Result<i32, i32> {
        self.call_in(schema::Class::Kms, target, cmd, uarg, short)
    }

    fn call_in(
        &mut self,
        class: schema::Class,
        target: u32,
        cmd: u32,
        uarg: u64,
        short: usize,
    ) -> Result<i32, i32> {
        let g = Guest::gather(&mut self.mem, class, target, self.render, cmd, uarg)?;
        self.send(g, short)
    }

    /// An NVKMS ioctl on `target` (nvgpu_nvkms.c: render 0, the table the
    /// host version selects), its NvKmsIoctlParams at `uarg`.
    fn nvkms(&mut self, target: u32, uarg: u64) -> Result<i32, i32> {
        let table = self.be.driver.and_then(schema::modeset_table);
        let g = Guest::gather_in(
            &mut self.mem,
            schema::Class::Modeset,
            table,
            target,
            0,
            NVKMS,
            uarg,
        )?;
        self.send(g, 0)
    }

    fn send(&mut self, mut g: Guest, short: usize) -> Result<i32, i32> {
        g.translate(&self.hooks)?;
        let req = g.build();
        let cap = g.resp_len() - short;
        let reply = match self.be.serve(&req, cap) {
            Outcome::Reply(r) => r,
            Outcome::Ioctl2(mut p) => {
                assert_eq!(
                    p.executor_key().is_some(),
                    g.executor(),
                    "both halves agree on who waits"
                );
                p.execute();
                self.be.finish_ioctl2(p)
            }
        };
        assert!(reply.bytes.len() <= cap, "the reply fits what was posted");
        assert_eq!(rd(&reply.bytes, 12, 4), 0x1234, "req_id echoed");
        g.parse(&reply.bytes, &mut self.hooks)?;
        g.restore();
        g.outputs(&mut self.hooks)?;
        g.copy_back(&mut self.mem)?;
        Ok(g.ret)
    }

    fn calls(&self) -> Vec<(String, u32)> {
        std::mem::take(&mut self.fake.0.lock().unwrap().calls)
    }
}

/// drm_mode_card_res at `addr`, with its four lists at the given addresses.
fn card_res(counts: [u32; 4], ptrs: [u64; 4]) -> Vec<u8> {
    let mut a = vec![0u8; 64];
    for i in 0..4 {
        wr(&mut a, 8 * i, 8, ptrs[i]);
        wr(&mut a, 32 + 4 * i, 4, u64::from(counts[i]));
    }
    a
}

#[test]
fn getresources_arrays_go_out_empty_and_come_back_as_the_kernel_fills_them() {
    let mut w = world();
    // No fb list at all (NULL), room for both CRTCs, room for one of three
    // connectors (the kernel writes one and says three), an encoder list.
    w.mem
        .put(0x1000, &card_res([0, 2, 1, 4], [0, 0x2000, 0x3000, 0x4000]));
    w.mem.put(0x2000, &[0xee; 8]);
    w.mem.put(0x3000, &[0xee; 8]);
    w.mem.put(0x4000, &[0xee; 16]);
    let kms = w.kms;
    assert_eq!(w.call(kms, GETRESOURCES, 0x1000, 0), Ok(0));
    assert_eq!(w.calls(), vec![("e2e-kms".to_string(), GETRESOURCES)]);

    let a = w.mem.get(0x1000);
    assert_eq!(rd(a, 8, 8), 0x2000, "the caller's own pointers come back");
    assert_eq!(rd(a, 16, 8), 0x3000);
    assert_eq!(
        [rd(a, 32, 4), rd(a, 36, 4), rd(a, 40, 4), rd(a, 44, 4)],
        [0, 2, 3, 1],
        "every count is the actual one"
    );
    assert_eq!(rd(a, 48, 4), 320);
    assert_eq!(w.mem.get(0x2000), [41, 0, 0, 0, 42, 0, 0, 0]);
    // min(sent, left) elements: one connector, and the second slot untouched.
    assert_eq!(&w.mem.get(0x3000)[..4], &51u32.to_le_bytes());
    assert_eq!(&w.mem.get(0x3000)[4..], &[0xee; 4]);
    assert_eq!(&w.mem.get(0x4000)[..4], &61u32.to_le_bytes());
    assert_eq!(
        &w.mem.get(0x4000)[4..],
        &[0xee; 12],
        "only what the kernel wrote"
    );
}

#[test]
fn addfb2_takes_the_proxies_objects_into_the_lease_for_one_job_only() {
    let mut w = world();
    // Two guest GEM handles (proxies) of objects in the render file.
    {
        let mut k = w.fake.0.lock().unwrap();
        let g = k.gems.entry("e2e-render".into()).or_default();
        g.insert(7, 0xa);
        g.insert(8, 0xb);
    }
    w.hooks.gems.insert(3, (w.render, 7));
    w.hooks.gems.insert(4, (w.render, 8));
    let mut a = vec![0u8; 104];
    wr(&mut a, 4, 4, 1920);
    wr(&mut a, 8, 4, 1080);
    wr(&mut a, 12, 4, u64::from(NV12));
    wr(&mut a, 20, 4, 3);
    wr(&mut a, 24, 4, 4);
    w.mem.put(0x1000, &a);
    let kms = w.kms;
    assert_eq!(w.call(kms, ADDFB2, 0x1000, 0), Ok(0));

    let a = w.mem.get(0x1000).to_vec();
    assert_eq!(rd(&a, 0, 4), 77, "the host's fb id reaches the caller");
    assert_eq!(
        (rd(&a, 20, 4), rd(&a, 24, 4)),
        (3, 4),
        "and its own GEM handles, not the host's"
    );
    let k = w.fake.0.lock().unwrap();
    assert_eq!(k.fbs, vec![(77, NV12)]);
    assert!(
        k.gems["e2e-kms"].is_empty(),
        "the temporaries are closed in the lease file"
    );
    assert_eq!(
        k.gems["e2e-render"].len(),
        2,
        "and the proxies' objects stay where they were"
    );
    drop(k);
    assert!(
        w.be.kms_states[&kms].owns_fb(77),
        "the lease's own framebuffer, for GETFB to answer with handles"
    );
    // The state is the handle's: a CLOSE ends it, so a later handle that
    // gets the same number owns no framebuffer of this one's.
    w.be.close_handle(kms).unwrap();
    assert!(!w.be.kms_states.contains_key(&kms));
    assert!(
        !w.be.vm_fbs.made_here(77),
        "nor may anything of this VM name it as a scanout source any more"
    );
}

/// A scanout source is a framebuffer this VM made (S-6): the host looks
/// framebuffer ids up device-wide and a lease does not cover them, so an id
/// the host compositor or another VM made would otherwise be shown -- and
/// checksummed -- on the guest's CRTC. The refusal comes before the host
/// sees the call; the VM's own framebuffer, from any of its files, passes.
#[test]
fn a_page_flip_to_a_framebuffer_this_vm_never_made_never_reaches_the_host() {
    const PAGE_FLIP: u32 = 0xc018_64b0;
    let mut w = world();
    let kms = w.kms;
    let other =
        w.be.adopt_for_test(memfd(c"e2e-kms-2"), HandleKind::DrmLease(0));
    // Any KMS call makes the file's state; the other file made fb 77.
    let mut a = vec![0u8; 24];
    wr(&mut a, 4, 4, 5);
    w.mem.put(0x1000, &a);
    assert_eq!(w.call(other, PAGE_FLIP, 0x1000, 0), Ok(-libc::EPERM));
    w.be.kms_states[&other].add_fb(77);
    w.calls();

    wr(&mut a, 4, 4, 5);
    w.mem.put(0x1000, &a);
    assert_eq!(w.call(kms, PAGE_FLIP, 0x1000, 0), Ok(-libc::EPERM));
    assert!(w.calls().is_empty(), "the host never saw it");

    wr(&mut a, 4, 4, 77);
    w.mem.put(0x1000, &a);
    // The fake kernel has no PAGE_FLIP; reaching it is the point.
    assert_eq!(w.call(kms, PAGE_FLIP, 0x1000, 0), Ok(-libc::ENOTTY));
    assert_eq!(w.calls(), vec![("e2e-kms".to_string(), PAGE_FLIP)]);

    w.be.close_handle(other).unwrap();
    assert_eq!(w.call(kms, PAGE_FLIP, 0x1000, 0), Ok(-libc::EPERM));
}

/// A framebuffer is NVKMS memory or nothing: a proxy standing for a foreign
/// dma-buf (a host iGPU's buffer imported in export mode) is refused before
/// the host's ADDFB2 could dereference its NULL pMemory, and the temporaries
/// made for the job are closed all the same.
#[test]
fn addfb2_of_a_dmabuf_object_is_refused_by_what_the_host_says_it_is() {
    let mut w = world();
    {
        let mut k = w.fake.0.lock().unwrap();
        let g = k.gems.entry("e2e-render".into()).or_default();
        g.insert(7, 0xa);
        g.insert(8, 0xb);
        k.types.insert(0xb, hostfd::NV_GEM_OBJECT_DMABUF);
    }
    w.hooks.gems.insert(3, (w.render, 7));
    w.hooks.gems.insert(4, (w.render, 8));
    let mut a = vec![0u8; 104];
    wr(&mut a, 4, 4, 1920);
    wr(&mut a, 8, 4, 1080);
    wr(&mut a, 12, 4, u64::from(NV12));
    wr(&mut a, 20, 4, 3);
    wr(&mut a, 24, 4, 4);
    w.mem.put(0x1000, &a);
    let kms = w.kms;
    assert_eq!(w.call(kms, ADDFB2, 0x1000, 0), Ok(-libc::EINVAL));
    let k = w.fake.0.lock().unwrap();
    assert!(k.fbs.is_empty(), "the host never saw the ADDFB2");
    assert!(
        k.gems.get("e2e-kms").is_none_or(|g| g.is_empty()),
        "the temporaries are closed in the lease file"
    );
}

/// drm_mode_create_lease at 0x1000 leasing objects 41 and 51.
fn create_lease(w: &mut World) {
    let mut a = vec![0u8; 24];
    wr(&mut a, 0, 8, 0x2000);
    wr(&mut a, 8, 4, 2);
    wr(&mut a, 12, 4, libc::O_CLOEXEC as u64);
    wr(&mut a, 20, 4, 0x5a5a); // what the caller left there, never sent
    w.mem.put(0x1000, &a);
    let mut ids = vec![0u8; 8];
    wr(&mut ids, 0, 4, 41);
    wr(&mut ids, 4, 4, 51);
    w.mem.put(0x2000, &ids);
}

#[test]
fn create_lease_hands_back_a_new_backend_handle_that_the_guest_materialises() {
    let mut w = world();
    create_lease(&mut w);
    let before = w.be.handle_count();
    let kms = w.kms;
    assert_eq!(w.call(kms, CREATE_LEASE, 0x1000, 0), Ok(0));

    assert_eq!(w.be.handle_count(), before + 1);
    let [(handle, kind)] = w.hooks.fd_outs[..] else {
        panic!("one descriptor out: {:?}", w.hooks.fd_outs)
    };
    assert_eq!(w.be.handles.kind(handle), Some(HandleKind::Eventfd));
    assert_eq!(kind, HK_EVENTFD, "the kind the kernel says, not the schema");
    let a = w.mem.get(0x1000);
    assert_eq!(rd(a, 16, 4), 9, "lessee id");
    assert_eq!(
        rd(a, 20, 4),
        100,
        "the guest fd the hook made, in the caller's field"
    );
}

#[test]
fn a_descriptor_the_backend_already_holds_is_never_adopted() {
    // A table descriptor (the render file's) and a private one (the kind
    // the pump or the transport holds) at a descriptor-out field: neither
    // may end up under a second owner, whose CLOSE would close it.
    let mut w = world();
    let table_fd = w.be.handles.get_raw(w.render).unwrap();
    let private = PrivateFd::new(hostfd::new_eventfd().unwrap());
    for fd in [table_fd, private.as_raw_fd()] {
        create_lease(&mut w);
        w.fake.0.lock().unwrap().lease_fd = Some(fd);
        let before = w.be.handle_count();
        let kms = w.kms;
        assert_eq!(w.call(kms, CREATE_LEASE, 0x1000, 0), Ok(0));
        assert_eq!(w.be.handle_count(), before, "nothing adopted");
        assert!(w.hooks.fd_outs.is_empty());
        assert_eq!(
            rd(w.mem.get(0x1000), 20, 4) as i32,
            -1,
            "the caller hears of no fd"
        );
        // SAFETY: F_GETFD only asks whether the number is still open.
        assert!(
            unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0,
            "and it was not closed"
        );
    }
}

#[test]
fn a_reply_that_cannot_fit_is_refused_before_the_host_runs_anything() {
    let mut w = world();
    w.mem
        .put(0x1000, &card_res([0, 2, 0, 0], [0, 0x2000, 0, 0]));
    w.mem.put(0x2000, &[0; 8]);
    let kms = w.kms;
    assert_eq!(w.call(kms, GETRESOURCES, 0x1000, 1), Err(libc::EMSGSIZE));
    assert!(w.calls().is_empty(), "the ioctl never ran");
    assert_eq!(
        w.call(kms, GETRESOURCES, 0x1000, 0),
        Ok(0),
        "exactly the posted size fits"
    );
}

#[test]
fn a_consumed_descriptor_is_closed_by_the_backend_once_the_call_ran_and_by_the_guest_otherwise() {
    let mut w = world();
    // GRANT_PERMISSIONS(MODESET) with the modeset fd as a temporary the call
    // consumes.
    let mut a = vec![0u8; 12];
    wr(&mut a, 0, 4, 5); // the caller's fd 5
    wr(&mut a, 8, 4, 2); // NV_DRM_PERMISSIONS_TYPE_MODESET
    w.mem.put(0x1000, &a);
    w.hooks.fds.insert(5, (w.modeset, I2_FD_CONSUME));
    let (kms, modeset) = (w.kms, w.modeset);
    assert_eq!(w.call(kms, GRANT, 0x1000, 0), Ok(0));
    assert_eq!(w.be.handles.kind(modeset), None, "consumed by the call");
    assert!(w.hooks.closed.is_empty(), "so the guest closes nothing");
    assert_eq!(
        rd(w.mem.get(0x1000), 0, 4),
        5,
        "and the caller's own fd comes back"
    );

    // The same call refused before it runs (a SUB_OWNER grant): the guest
    // closes the handle it handed over, since the backend never did.
    let modeset =
        w.be.adopt_for_test(memfd(c"e2e-modeset"), HandleKind::Dev(DeviceKind::Modeset));
    w.hooks.fds.insert(5, (modeset, I2_FD_CONSUME));
    wr(&mut a, 8, 4, 1);
    w.mem.put(0x1000, &a);
    assert_eq!(w.call(kms, GRANT, 0x1000, 0), Err(libc::EPERM));
    assert!(w.be.handles.kind(modeset).is_some());
    assert_eq!(w.hooks.closed, vec![modeset]);
}

#[test]
fn a_syncobj_wait_reaches_the_host_as_a_poll_and_an_eventfd_never_does() {
    // SYNCOBJ_WAIT with a timeout far in the future, on one handle: the
    // backend's FENCES policy (policy.rs, fence.rs) hands the host a zero
    // timeout, and the host's -ETIME comes back as the call's answer -- the
    // guest then sleeps on a registration of its own (nvgpu_fence.c).
    let mut w = world();
    let render = w.render;
    let mut a = vec![0u8; 40];
    wr(&mut a, 0, 8, 0x2000);
    wr(&mut a, 8, 8, i64::MAX as u64);
    wr(&mut a, 16, 4, 1);
    w.mem.put(0x1000, &a);
    w.mem.put(0x2000, &7u32.to_le_bytes());
    assert_eq!(
        w.call_in(schema::Class::Render, render, SYNCOBJ_WAIT, 0x1000, 0),
        Ok(-libc::ETIME)
    );
    assert_eq!(w.fake.0.lock().unwrap().wait_timeout, Some(0));
    assert_eq!(w.calls(), vec![("e2e-render".to_string(), SYNCOBJ_WAIT)]);

    // SYNCOBJ_EVENTFD would leave a kernel entry nobody can take back; it is
    // refused before the host sees it (registrations are HOST_OP state).
    let mut e = vec![0u8; 24];
    wr(&mut e, 0, 4, 7);
    wr(&mut e, 16, 4, u32::MAX as u64);
    w.mem.put(0x3000, &e);
    assert_eq!(
        w.call_in(schema::Class::Render, render, SYNCOBJ_EVENTFD, 0x3000, 0),
        Err(libc::EPERM)
    );
    assert!(w.calls().is_empty());
}

/// Hyprland destroys a dead client's timeline and imports the next one,
/// which the host gives the same syncobj number (lowest free,
/// drm_syncobj.c:606). A watch on the new syncobj must not join the old
/// one's unfired registration, whose point may never come (S-13): the
/// DESTROY orphans it before it runs, and so does closing the render file.
#[test]
fn a_destroyed_syncobjs_wait_is_never_joined_by_the_next_syncobj_of_that_number() {
    use crate::fence::{RegKey, SyncobjHost, Watched};
    struct Host;
    impl SyncobjHost for Host {
        fn syncobj_file(&self, _: RawFd, _: u32) -> std::io::Result<OwnedFd> {
            Ok(memfd(c"e2e-syncobj"))
        }
        fn register(&self, _: RawFd, _: u32, _: u64, _: u32, _: RawFd) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn watch(w: &mut World, key: RegKey, cookie: u64) -> Result<Watched, i32> {
        let mut regs = std::mem::take(&mut w.be.syncobj_regs);
        let r = regs.watch(&Host, &mut w.be, -1, key, cookie);
        w.be.syncobj_regs = regs;
        r
    }
    const DESTROY: u32 = hostfd::DRM_IOCTL_SYNCOBJ_DESTROY;
    let mut w = world();
    let render = w.render;
    let key = RegKey {
        render,
        syncobj: 3,
        point: 120,
        flags: 0,
    };
    let c = |n: u64| (1 << 32) | n;
    assert_eq!(watch(&mut w, key, c(1)), Ok(Watched::New));
    assert_eq!(watch(&mut w, key, c(2)), Ok(Watched::Joined(c(1))));
    let mut a = vec![0u8; 8];
    wr(&mut a, 0, 4, 3);
    w.mem.put(0x1000, &a);
    // Whatever the host answers (this one has no syncobjs at all).
    assert_eq!(
        w.call_in(schema::Class::Render, render, DESTROY, 0x1000, 0),
        Ok(-libc::ENOTTY)
    );
    assert_eq!(watch(&mut w, key, c(3)), Ok(Watched::New));
    assert_eq!(w.be.syncobj_regs.len(), 2, "the orphan keeps its slot");

    // A later render file may be given this handle number: the closed
    // file's registrations are no key's any more.
    w.be.close_handle(render).unwrap();
    assert_eq!(watch(&mut w, key, c(4)), Ok(Watched::New));
    assert_eq!(w.be.syncobj_regs.len(), 3);
}

// ───────────────────────────── NVKMS ─────────────────────────────

/// NvKmsIoctlParams at 0x1000 for `cmd`, its params block of `size` bytes
/// at 0x2000.
fn nvkms_call(w: &mut World, cmd: u32, params: &[u8]) {
    let mut outer = vec![0u8; 16];
    wr(&mut outer, 0, 4, u64::from(cmd));
    wr(&mut outer, 4, 4, params.len() as u64);
    wr(&mut outer, 8, 8, 0x2000);
    w.mem.put(0x1000, &outer);
    w.mem.put(0x2000, params);
}

fn nvkms_world() -> World {
    let mut w = world();
    w.be.set_host_driver_version("610.57.04");
    w
}

#[test]
fn a_flip_carries_its_heads_and_their_luts_and_a_refusal_still_brings_the_reply_back() {
    let mut w = nvkms_world();
    // The guest owns the display: the heads are the host's to refuse.
    w.be.config.kms_card = true;
    let mut params = vec![0u8; 3104];
    wr(&mut params, 0, 4, 1); // deviceHandle
    wr(&mut params, 8, 8, 0x3000); // pFlipHead
    wr(&mut params, 16, 4, 2); // numFlipHeads
    wr(&mut params, 28, 4, 0xdead); // reply.flipResult, as the caller left it
    nvkms_call(&mut w, 15, &params);
    let mut heads = vec![0u8; 2 * 4952];
    wr(&mut heads, 4, 4, 5);
    wr(&mut heads, 4952 + 4, 4, 6);
    wr(&mut heads, 88, 8, 0x5000); // head 0: flip.lut.input.pRamps
    wr(&mut heads, 4952 + 104, 8, 0x6000); // head 1: flip.lut.output.pRamps
    w.mem.put(0x3000, &heads);
    w.mem.put(0x5000, &[0x11; LUT]);
    w.mem.put(0x6000, &[0x22; LUT]);
    let modeset = w.modeset;
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(-libc::EPERM));
    assert_eq!(w.fake.0.lock().unwrap().nvkms, vec![15]);
    let p = w.mem.get(0x2000);
    assert_eq!(rd(p, 28, 4), 0x77, "the reply half, even on -EPERM");
    assert_eq!(rd(p, 8, 8), 0x3000, "the request half is the caller's");
    assert_eq!(rd(w.mem.get(0x1000), 8, 8), 0x2000);
}

#[test]
fn a_flip_asking_for_a_tegra_syncpoint_never_reaches_the_host() {
    let mut w = nvkms_world();
    let mut params = vec![0u8; 3104];
    wr(&mut params, 8, 8, 0x3000);
    wr(&mut params, 16, 4, 1);
    nvkms_call(&mut w, 15, &params);
    let mut heads = vec![0u8; 4952];
    // flip.layer[2].syncObjects.val.useSyncpt: layer array @216, stride 592.
    heads[216 + 2 * 592 + 60] = 1;
    w.mem.put(0x3000, &heads);
    let modeset = w.modeset;
    assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM));
    assert!(w.fake.0.lock().unwrap().nvkms.is_empty());
}

/// A FLIP element that dirties no layer passes NVKMS's own check on any
/// head, so the backend holds every element to the grants; and a guest
/// flip never asks the host for FLIP_OCCURRED, which only nvidia-drm's own
/// open would get. What the backend cleared stays set in the guest's copy.
#[test]
fn a_guest_flip_reaches_the_host_only_on_a_granted_head_and_never_asks_for_flip_occurred() {
    let mut w = nvkms_world();
    grant_head_1(&mut w);
    let modeset = w.modeset;
    let flip = |w: &mut World, head: u64| {
        let mut params = vec![0u8; 3104];
        wr(&mut params, 0, 4, 1); // deviceHandle
        wr(&mut params, 8, 8, 0x3000); // pFlipHead
        wr(&mut params, 16, 4, 1); // numFlipHeads
        nvkms_call(w, 15, &params);
        let mut heads = vec![0u8; 4952];
        wr(&mut heads, 4, 4, head);
        // flip.cursor.imageSpecified would do; a cursor-only element
        // dirties no layer. Layers 0 and 3 ask for FLIP_OCCURRED:
        // flip.layer @216, stride 592, completionNotifier.val.awaken @52.
        heads[216 + 52] = 1;
        heads[216 + 3 * 592 + 52] = 1;
        w.mem.put(0x3000, &heads);
        w.nvkms(modeset, 0x1000)
    };
    let seen = w.fake.0.lock().unwrap().nvkms.len();
    assert_eq!(
        flip(&mut w, 0),
        Err(libc::EPERM),
        "the host compositor's head"
    );
    assert_eq!(w.fake.0.lock().unwrap().nvkms.len(), seen);
    assert_eq!(flip(&mut w, 1), Ok(0));
    assert_eq!(w.fake.0.lock().unwrap().flip_awaken, vec![0; 8]);
    let heads = w.mem.get(0x3000);
    assert_eq!((heads[216 + 52], heads[216 + 3 * 592 + 52]), (1, 1));
}

#[test]
fn register_surface_translates_exactly_the_planes_its_format_has() {
    let mut w = nvkms_world();
    let ctl =
        w.be.adopt_for_test(memfd(c"e2e-ctl"), HandleKind::Dev(DeviceKind::Ctl));
    let mut params = vec![0u8; 152];
    wr(&mut params, 4, 4, 0xff02); // useFd = 2, and padding the kernel ignores
    wr(&mut params, 16, 4, 7); // planes[0].u.fd: the caller's fd 7
    wr(&mut params, 48, 4, 8); // planes[1]
    wr(&mut params, 124, 4, 19); // format: two planes
    nvkms_call(&mut w, 17, &params);
    w.hooks.fds.insert(7, (ctl, 0));
    w.hooks.fds.insert(8, (ctl, 0));
    let modeset = w.modeset;
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(0));
    let p = w.mem.get(0x2000);
    assert_eq!(rd(p, 144, 4), 0x55, "surfaceHandle");
    assert_eq!((rd(p, 16, 4), rd(p, 48, 4)), (7, 8), "the caller's own fds");
    // useFd = 0: no descriptors at all, whatever the planes hold (the
    // guest has none of these fds now), and NVKMS refuses the call itself.
    wr(&mut params, 4, 4, 0xff00);
    nvkms_call(&mut w, 17, &params);
    w.hooks.fds.clear();
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(-libc::EPERM));
}

#[test]
fn validate_mode_copies_back_only_the_bytes_nvkms_wrote() {
    let mut w = nvkms_world();
    let mut params = vec![0u8; 656];
    wr(&mut params, 288, 4, 64); // infoStringSize
    wr(&mut params, 296, 8, 0x4000); // pInfoString
    nvkms_call(&mut w, 8, &params);
    w.mem.put(0x4000, &[0xee; 64]);
    let modeset = w.modeset;
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(-libc::EPERM));
    let s = w.mem.get(0x4000);
    assert_eq!(&s[..5], b"hello");
    assert!(
        s[5..].iter().all(|&b| b == 0xee),
        "nothing past infoStringLenWritten"
    );
    assert_eq!(rd(w.mem.get(0x2000), 456, 4), 5);
}

/// MOVE_CURSOR on `head` of deviceHandle 1.
fn cursor(head: u64) -> Vec<u8> {
    let mut c = vec![0u8; 20];
    wr(&mut c, 0, 4, 1);
    wr(&mut c, 4, 4, 0x100);
    wr(&mut c, 8, 4, head);
    c
}

/// ALLOC_DEVICE, then nvidia-drm GRANT_PERMISSIONS of dpy 1<<3 through the
/// lease on a fresh modeset file, and ACQUIRE_PERMISSIONS of it: head 1 is
/// granted from then on. Checks each step on the way.
fn grant_head_1(w: &mut World) {
    let grant = w.be.adopt_for_test(
        memfd(c"e2e-modeset-grant"),
        HandleKind::Dev(DeviceKind::Modeset),
    );
    let (kms, modeset) = (w.kms, w.modeset);
    nvkms_call(w, 0, &[0u8; 1440]);
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(0), "ALLOC_DEVICE");

    // Before any grant, MOVE_CURSOR on head 1 is refused by the backend.
    nvkms_call(w, 11, &cursor(1));
    assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM));

    // The modeset file, by now typed, is no grant file.
    let mut g = vec![0u8; 12];
    wr(&mut g, 0, 4, 5);
    wr(&mut g, 4, 4, 1 << 3);
    wr(&mut g, 8, 4, 2);
    w.mem.put(0x5000, &g);
    w.hooks.fds.insert(5, (modeset, 0));
    assert_eq!(w.call(kms, GRANT, 0x5000, 0), Err(libc::EPERM));
    // A fresh one is.
    w.hooks.fds.insert(5, (grant, 0));
    assert_eq!(w.call(kms, GRANT, 0x5000, 0), Ok(0));

    let mut acq = vec![0u8; 28];
    wr(&mut acq, 0, 4, 5);
    nvkms_call(w, 41, &acq);
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(0), "ACQUIRE_PERMISSIONS");
    nvkms_call(w, 11, &cursor(1));
    assert_eq!(w.nvkms(modeset, 0x1000), Ok(0), "head 1 is granted");
    nvkms_call(w, 11, &cursor(0));
    assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM), "head 0 is not");
}

#[test]
fn a_grant_through_the_lease_opens_exactly_its_head_until_the_lease_file_closes() {
    let mut w = nvkms_world();
    let (kms, modeset) = (w.kms, w.modeset);
    grant_head_1(&mut w);
    assert!(
        w.fake.0.lock().unwrap().lease_probes > 0,
        "the lease was asked"
    );

    // The lease file closes: nvidia-drm revokes what it granted.
    w.be.close_handle(kms).unwrap();
    nvkms_call(&mut w, 11, &cursor(1));
    assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM));
    assert_eq!(w.fake.0.lock().unwrap().nvkms, vec![0, 41, 11]);
}

/// The host takes a lease back without a word to the lessee's file -- the
/// lessor revokes it, or closes -- and the file stays open, and after the
/// lessor's close nvidia-drm still holds the grant on it. The backend asks
/// the lease before the next NVKMS call, ends what was granted through it,
/// and closes the host file then and there (so nvidia-drm's postclose
/// runs now, not after the next compositor has taken the connector),
/// leaving a handle that answers ENODEV until the guest closes it.
#[test]
fn a_lease_that_ended_for_good_ends_its_grants_and_closes_its_host_file() {
    for gone in [LeaseState::Revoked, LeaseState::LessorGone] {
        let mut w = nvkms_world();
        let (kms, modeset) = (w.kms, w.modeset);
        grant_head_1(&mut w);
        w.fake.0.lock().unwrap().lease = gone;
        nvkms_call(&mut w, 11, &cursor(1));
        assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM), "{gone:?}");
        assert!(w.be.handles.is_buried(kms), "{gone:?}");
        assert_eq!(w.be.handles.kind(kms), Some(HandleKind::Other));
        assert!(
            w.be.take_pump_cmds()
                .iter()
                .any(|c| matches!(c, PumpCmd::Unwatch { handle } if *handle == kms)),
            "the pump lets its duplicate go"
        );
        assert!(w.be.nvkms.granting_handles().is_empty());
        w.mem.put(0x1000, &[0u8; 64]);
        assert_eq!(w.call(kms, GETRESOURCES, 0x1000, 0), Err(libc::ENODEV));
        // Nothing is left to ask about.
        let probes = w.fake.0.lock().unwrap().lease_probes;
        nvkms_call(&mut w, 11, &cursor(1));
        assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM));
        assert_eq!(w.fake.0.lock().unwrap().lease_probes, probes);
        // And the guest closes the number as it would any other.
        w.be.close_handle(kms).unwrap();
        assert!(!w.be.handles.is_buried(kms));
    }
}

/// A master drop (a VT switch) leaves the lease in place: the grants are
/// ended here, the file stays open, and the backend keeps asking, so that
/// a lessor that then closes is caught too.
#[test]
fn a_lessor_dropping_master_only_gates_and_keeps_the_lease_asked_about() {
    let mut w = nvkms_world();
    let (kms, modeset) = (w.kms, w.modeset);
    grant_head_1(&mut w);
    w.fake.0.lock().unwrap().lease = LeaseState::LessorNotMaster;
    nvkms_call(&mut w, 11, &cursor(1));
    assert_eq!(w.nvkms(modeset, 0x1000), Err(libc::EPERM));
    assert_eq!(w.be.handles.kind(kms), Some(HandleKind::DrmLease(0)));
    assert!(!w.be.handles.is_buried(kms));
    assert_eq!(w.be.nvkms.granting_handles(), vec![kms]);
    let probes = w.fake.0.lock().unwrap().lease_probes;
    assert!(w.be.recheck_granting_leases().contains(&kms));
    assert!(
        w.fake.0.lock().unwrap().lease_probes > probes,
        "still asked"
    );
    assert!(!w.be.handles.is_buried(kms));
    w.fake.0.lock().unwrap().lease = LeaseState::LessorGone;
    assert_eq!(w.be.recheck_granting_leases(), vec![kms]);
    assert!(w.be.handles.is_buried(kms));
    assert!(w.be.nvkms.granting_handles().is_empty());
}

/// Compositor-VM mode: the guest is the lessor and the connectors are its
/// own, so a revoked lessee keeps a native open file with an empty lease.
#[test]
fn in_kms_card_mode_a_revoked_lease_file_stays_open() {
    let mut w = nvkms_world();
    let kms = w.kms;
    grant_head_1(&mut w);
    w.be.config.kms_card = true;
    w.fake.0.lock().unwrap().lease = LeaseState::Revoked;
    assert_eq!(w.be.check_leases(None), vec![kms]);
    assert_eq!(w.be.handles.kind(kms), Some(HandleKind::DrmLease(0)));
    assert!(!w.be.handles.is_buried(kms));
}

/// Compositor-VM mode: the guest is the lessor, and revokes a lease through
/// its card. The grants made through the lessee end at once, before any
/// NVKMS call asks.
#[test]
fn a_lease_the_guest_revokes_through_its_card_ends_the_grants_made_through_it() {
    let mut w = nvkms_world();
    let card =
        w.be.adopt_for_test(memfd(c"e2e-card"), HandleKind::DrmCard(0));
    grant_head_1(&mut w);
    assert_eq!(w.be.nvkms.granting_handles(), vec![w.kms]);
    let mut r = vec![0u8; 4];
    wr(&mut r, 0, 4, 9);
    w.mem.put(0x5000, &r);
    assert_eq!(w.call(card, REVOKE_LEASE, 0x5000, 0), Ok(0));
    assert!(w.be.nvkms.granting_handles().is_empty());
}

/// A LEASE uevent names a card; only that card's leases are asked.
#[test]
fn a_lease_uevent_asks_the_leases_of_its_card_only() {
    let mut w = nvkms_world();
    grant_head_1(&mut w);
    w.fake.0.lock().unwrap().lease = LeaseState::Revoked;
    assert!(w.be.check_leases(Some(1)).is_empty());
    assert_eq!(w.be.nvkms.granting_handles(), vec![w.kms]);
    assert_eq!(w.be.check_leases(Some(0)), vec![w.kms]);
    assert!(w.be.nvkms.granting_handles().is_empty());
}

#[test]
fn nvkms_through_v1_takes_only_flat_commands_and_never_a_descriptor() {
    let mut w = nvkms_world();
    let modeset = w.modeset;
    // A v1 IOCTL: nvgpu_ioctl_req {cmd, data_len, nested_offset, nested_len,
    // deep_ptr_offset, deep_len} then the outer struct and the params.
    let v1 = |cmd: u32, size: usize| {
        let mut m = Vec::new();
        for v in [MsgType::Ioctl as u32, modeset, 0, 0x99] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        for v in [NVKMS, 16, 16, size as u32, 0, 0] {
            m.extend_from_slice(&v.to_le_bytes());
        }
        let mut outer = vec![0u8; 16];
        wr(&mut outer, 0, 4, u64::from(cmd));
        wr(&mut outer, 4, 4, size as u64);
        m.extend_from_slice(&outer);
        m.resize(m.len() + size, 0);
        m
    };
    let status = |w: &mut World, m: &[u8]| {
        let mut resp = vec![0u8; 4096];
        assert!(w.be.dispatch(m, &mut resp) >= 16);
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    };
    // REGISTER_SURFACE (fds) and FLIP (pointers) need IOCTL2.
    assert_eq!(status(&mut w, &v1(17, 152)), -libc::EPERM);
    assert_eq!(status(&mut w, &v1(15, 3104)), -libc::EPERM);
    // So does anything whose size is not the command's.
    assert_eq!(status(&mut w, &v1(3, 40)), -libc::EINVAL);
    assert!(!w.be.nvkms.is_typed(modeset), "nothing reached the host");
    // A flat query goes (and the memfd answers ENOTTY, as a host would not).
    assert_ne!(status(&mut w, &v1(3, 44)), -libc::EPERM);
    assert!(w.be.nvkms.is_typed(modeset));
}

#[test]
fn a_vm_holds_only_so_many_modeset_files() {
    let mut w = world();
    let mut have = 1; // world()'s own
    while have < crate::nvkms::MAX_MODESET_OPENS {
        w.be.adopt_for_test(memfd(c"e2e-modeset"), HandleKind::Dev(DeviceKind::Modeset));
        have += 1;
    }
    let mut m = Vec::new();
    for v in [MsgType::Open as u32, 0, 0, 0x98, DEV_MODESET, 0] {
        m.extend_from_slice(&v.to_le_bytes());
    }
    let mut resp = vec![0u8; 64];
    w.be.dispatch(&m, &mut resp);
    assert_eq!(
        i32::from_le_bytes(resp[8..12].try_into().unwrap()),
        -libc::EMFILE
    );
}

/// The guest's master hooks send SET/DROP_MASTER as a bare IOCTL2 (the
/// driver's nvgpu_kms_raw(): one zero-length buffer, nothing else), and the
/// backend lets them reach a card it opened for the guest and never a lease,
/// whose master is the host compositor's to arbitrate.
#[test]
fn master_calls_reach_a_host_card_and_never_a_lease() {
    let mut w = world();
    let card =
        w.be.adopt_for_test(memfd(c"e2e-card"), HandleKind::DrmCard(0));
    assert_eq!(w.call(card, SET_MASTER, 0, 0), Ok(0));
    assert_eq!(w.call(card, DROP_MASTER, 0, 0), Ok(0));
    assert_eq!(
        w.calls(),
        vec![
            ("e2e-card".to_string(), SET_MASTER),
            ("e2e-card".to_string(), DROP_MASTER)
        ]
    );
    let kms = w.kms;
    assert_eq!(w.call(kms, SET_MASTER, 0, 0), Err(libc::EPERM));
    assert_eq!(w.call(kms, DROP_MASTER, 0, 0), Err(libc::EPERM));
    assert!(w.calls().is_empty(), "the lease never saw either");
}

/// How the guest learns what a property id is before an atomic commit
/// (nvgpu_kms.c, nvgpu_kms_prop_class()): GETPROPERTY with both pointers NULL
/// and both counts 0 is one buffer out and one back, and an id the host does
/// not know is the host's -ENOENT.
#[test]
fn a_property_is_named_by_a_getproperty_with_nothing_to_fill() {
    let mut w = world();
    let mut a = vec![0u8; 64];
    wr(&mut a, 16, 4, 7);
    w.mem.put(0x1000, &a);
    let kms = w.kms;
    assert_eq!(w.call(kms, GETPROPERTY, 0x1000, 0), Ok(0));
    assert_eq!(&w.mem.get(0x1000)[24..35], b"IN_FENCE_FD");
    wr(&mut a, 16, 4, 99);
    w.mem.put(0x1000, &a);
    assert_eq!(w.call(kms, GETPROPERTY, 0x1000, 0), Ok(-libc::ENOENT));
}
