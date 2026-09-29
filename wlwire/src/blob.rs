// SPDX-License-Identifier: Apache-2.0
//! Small read-only files copied by value: keymaps, dma-buf format tables, ICC
//! profiles.
//!
//! The sending side reads the bytes the message says (the size argument, from
//! the offset argument if there is one) and sends them as `BLOB` chunks ahead
//! of the message: the first chunk at once, which is all of a keymap or a
//! format table and says the file can be read at all, and the rest as the
//! channel takes them ([`BlobJob`]), so a 16 MiB ICC profile costs the sender
//! one chunk at a time, not 16 MiB per message. The receiving side assembles
//! them into a memfd, seals it
//! (no growing, shrinking or writing -- a client may map a keymap and trust
//! its size) and hands that over in place of the original. Nothing inside is
//! reinterpreted: a format table's indices stay valid because the table is
//! copied verbatim.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;

use crate::frame::{MAX_REC_PAYLOAD, REC_BLOB, Unit, record};
use crate::job::Job;
use crate::shm::ShmBudget;
use crate::sys;

/// Largest blob either side will carry. Keymaps are tens of KiB and format
/// tables a few; an ICC profile can reach a few MiB.
pub const MAX_BLOB: u64 = 16 * 1024 * 1024;
/// Most blob bytes a receiver holds unfinished at once.
const MAX_PENDING: u64 = 4 * MAX_BLOB;
/// Most blobs a receiver holds unfinished at once, whatever their size: each
/// is a memfd here, and bytes alone would let a peer open one per empty chunk
/// until this process runs out of descriptors. A sender's chunks come just
/// ahead of the message that names the blob (`send`), so an honest peer has
/// one or two in flight.
///
/// The memfds' pages are charged to the owner's shm budgets (the VM's and
/// the guest process's, on the backend) as chunks arrive, and a blob is
/// dropped, charge and all, if the `WAYLAND` record after its chunks does
/// not take it: that is where its sender puts the message that names it
/// ([`Blobs::expire`]).
pub const MAX_INCOMING: usize = 16;

struct Incoming {
    fd: OwnedFd,
    have: u64,
}

pub struct Blobs {
    host_side: bool,
    next: u32,
    incoming: HashMap<u32, Incoming>,
    pending_bytes: u64,
    budgets: Vec<Arc<ShmBudget>>,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BlobError {
    BadId(u32),
    OutOfOrder(u32),
    TooBig(u32),
    Incomplete(u32),
    /// A chunk with no bytes: never sent (`send` makes none for an empty
    /// blob), and the one way to open a memfd here without paying in bytes.
    Empty(u32),
    /// More unfinished blobs than `MAX_INCOMING`.
    TooMany(u32),
    Io,
}

/// What unfinished blobs held goes back to the budgets however they go.
impl Drop for Blobs {
    fn drop(&mut self) {
        self.clear();
    }
}

impl Blobs {
    pub fn new(host_side: bool) -> Self {
        Self {
            host_side,
            next: 1,
            incoming: HashMap::new(),
            pending_bytes: 0,
            budgets: Vec::new(),
            sent: 0,
            received: 0,
        }
    }

    /// Send `len` bytes at `off` of `fd` as BLOB records: the first chunk is
    /// read now, into `out`, and the rest is left to the returned job, if
    /// any. Returns the blob id, or `None` if the file cannot be read or is
    /// too large (the caller sends an invalid descriptor instead).
    pub fn send(
        &mut self,
        fd: OwnedFd,
        off: u64,
        len: u64,
        out: &mut Vec<Unit>,
    ) -> Option<(u32, Option<BlobJob>)> {
        if len > MAX_BLOB {
            return None;
        }
        let id = (self.next & 0x7fff_ffff) | if self.host_side { 0x8000_0000 } else { 0 };
        let mut job = BlobJob {
            fd,
            id,
            src: off,
            len,
            pos: 0,
        };
        // The first read says whether the file can be read at all: a pipe,
        // a directory or a write-only descriptor is refused here, before
        // the message is committed to naming the blob.
        let n = (len as usize).min(MAX_REC_PAYLOAD);
        let mut first = vec![0u8; n];
        sys::pread_full(job.fd.as_raw_fd(), &mut first, off).ok()?;
        self.next = self.next.wrapping_add(1).max(1);
        self.sent += 1;
        if n > 0 {
            out.push(Unit {
                rec: record(REC_BLOB, id, 0, &first),
                descs: Vec::new(),
            });
        }
        job.pos = n as u64;
        Some((id, (job.remaining() > 0).then_some(job)))
    }

    /// A BLOB chunk from the channel.
    pub fn chunk(&mut self, id: u32, off: u32, bytes: &[u8]) -> Result<(), BlobError> {
        if (id & 0x8000_0000 != 0) == self.host_side || id & 0x7fff_ffff == 0 {
            return Err(BlobError::BadId(id));
        }
        if bytes.is_empty() {
            return Err(BlobError::Empty(id));
        }
        if !self.incoming.contains_key(&id) {
            if off != 0 {
                return Err(BlobError::OutOfOrder(id));
            }
            if self.incoming.len() >= MAX_INCOMING {
                return Err(BlobError::TooMany(id));
            }
            let fd = sys::memfd(c"nvgpu-wl-blob", 0).map_err(|_| BlobError::Io)?;
            self.incoming.insert(id, Incoming { fd, have: 0 });
        }
        let inc = self.incoming.get_mut(&id).unwrap();
        if off as u64 != inc.have {
            return Err(BlobError::OutOfOrder(id));
        }
        let n = bytes.len() as u64;
        if inc.have + n > MAX_BLOB || self.pending_bytes + n > MAX_PENDING {
            return Err(BlobError::TooBig(id));
        }
        if !charge(&self.budgets, n) {
            return Err(BlobError::TooBig(id));
        }
        if sys::pwrite_full(inc.fd.as_raw_fd(), bytes, inc.have).is_err() {
            uncharge(&self.budgets, n);
            return Err(BlobError::Io);
        }
        inc.have += n;
        self.pending_bytes += n;
        Ok(())
    }

    /// Draw on `b` too for what unfinished blobs hold.
    pub fn add_budget(&mut self, b: Arc<ShmBudget>) {
        self.budgets.push(b);
    }

    /// Bytes unfinished blobs hold.
    pub fn held(&self) -> u64 {
        self.pending_bytes
    }

    /// Drop every unfinished blob (the connection is over).
    pub fn clear(&mut self) {
        self.incoming.clear();
        uncharge(&self.budgets, self.pending_bytes);
        self.pending_bytes = 0;
    }

    /// A `WAYLAND` record is done: any blob it did not take is one no
    /// message will (its sender puts the message right after the chunks),
    /// and goes.
    pub fn expire(&mut self) {
        if !self.incoming.is_empty() {
            self.clear();
        }
    }

    /// The finished blob `id`, which must be exactly `len` bytes, sealed and
    /// rewound, for the local peer.
    pub fn take(&mut self, id: u32, len: u64) -> Result<OwnedFd, BlobError> {
        let inc = match self.incoming.remove(&id) {
            Some(i) => i,
            // A zero-length blob sends no chunks.
            None if len == 0 => Incoming {
                fd: sys::memfd(c"nvgpu-wl-blob", 0).map_err(|_| BlobError::Io)?,
                have: 0,
            },
            None => return Err(BlobError::BadId(id)),
        };
        self.pending_bytes -= inc.have;
        uncharge(&self.budgets, inc.have);
        if inc.have != len {
            return Err(BlobError::Incomplete(id));
        }
        sys::seal_readonly(inc.fd.as_raw_fd()).map_err(|_| BlobError::Io)?;
        self.received += 1;
        Ok(inc.fd)
    }
}

/// The rest of a blob being sent: bytes `[pos, len)` of the file, from
/// `src` in it, read a chunk at a time as the channel takes them. The
/// receiver was promised `len` bytes: a file shorter than that, or one that
/// stops reading, gives zeros for the rest, as its first chunk did.
pub struct BlobJob {
    fd: OwnedFd,
    id: u32,
    src: u64,
    len: u64,
    pos: u64,
}

impl Job for BlobJob {
    /// Bytes still to send.
    fn remaining(&self) -> u64 {
        self.len - self.pos
    }

    /// The next BLOB record, and how many bytes it carries; `None` once done.
    fn next_unit(&mut self) -> Option<(Unit, usize)> {
        if self.pos >= self.len {
            return None;
        }
        let n = ((self.len - self.pos) as usize).min(MAX_REC_PAYLOAD);
        let mut chunk = vec![0u8; n];
        let _ = sys::pread_full(self.fd.as_raw_fd(), &mut chunk, self.src + self.pos);
        let u = Unit {
            rec: record(REC_BLOB, self.id, self.pos as u32, &chunk),
            descs: Vec::new(),
        };
        self.pos += n as u64;
        Some((u, n))
    }
}

/// `n` bytes from every budget, or from none.
fn charge(budgets: &[Arc<ShmBudget>], n: u64) -> bool {
    for (i, b) in budgets.iter().enumerate() {
        if !b.take(n, 0) {
            uncharge(&budgets[..i], n);
            return false;
        }
    }
    true
}

fn uncharge(budgets: &[Arc<ShmBudget>], n: u64) {
    if n > 0 {
        for b in budgets {
            b.give(n, 0);
        }
    }
}

impl From<io::Error> for BlobError {
    fn from(_: io::Error) -> Self {
        BlobError::Io
    }
}
