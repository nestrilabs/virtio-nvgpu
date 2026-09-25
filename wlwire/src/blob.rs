//! Small read-only files copied by value: keymaps, dma-buf format tables, ICC
//! profiles.
//!
//! The sending side reads the bytes the message says (the size argument, from
//! the offset argument if there is one) and sends them as `BLOB` chunks ahead
//! of the message; the receiving side assembles them into a memfd, seals it
//! (no growing, shrinking or writing -- a client may map a keymap and trust
//! its size) and hands that over in place of the original. Nothing inside is
//! reinterpreted: a format table's indices stay valid because the table is
//! copied verbatim.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

use crate::frame::{MAX_REC_PAYLOAD, REC_BLOB, Unit, record};
use crate::sys;

/// Largest blob either side will carry. Keymaps are tens of KiB and format
/// tables a few; an ICC profile can reach a few MiB.
pub const MAX_BLOB: u64 = 16 * 1024 * 1024;
/// Most blob bytes a receiver holds unfinished at once.
const MAX_PENDING: u64 = 4 * MAX_BLOB;

struct Incoming {
    fd: OwnedFd,
    have: u64,
}

pub struct Blobs {
    host_side: bool,
    next: u32,
    incoming: HashMap<u32, Incoming>,
    pending_bytes: u64,
    pub sent: u64,
    pub received: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum BlobError {
    BadId(u32),
    OutOfOrder(u32),
    TooBig(u32),
    Incomplete(u32),
    Io,
}

impl Blobs {
    pub fn new(host_side: bool) -> Self {
        Self {
            host_side,
            next: 1,
            incoming: HashMap::new(),
            pending_bytes: 0,
            sent: 0,
            received: 0,
        }
    }

    /// Read `len` bytes at `off` of `fd` and queue them as BLOB records.
    /// Returns the blob id, or `None` if the file cannot be read or is too
    /// large (the caller sends an invalid descriptor instead).
    pub fn send(&mut self, fd: &OwnedFd, off: u64, len: u64, out: &mut Vec<Unit>) -> Option<u32> {
        if len > MAX_BLOB {
            return None;
        }
        let mut data = vec![0u8; len as usize];
        // A file shorter than the size claimed keeps its zero tail, as a
        // private mapping past EOF would not -- but the receiver sees the
        // length it was promised.
        sys::pread_full(fd.as_raw_fd(), &mut data, off).ok()?;
        let id = (self.next & 0x7fff_ffff) | if self.host_side { 0x8000_0000 } else { 0 };
        self.next = self.next.wrapping_add(1).max(1);
        for (i, c) in data.chunks(MAX_REC_PAYLOAD).enumerate() {
            out.push(Unit {
                rec: record(REC_BLOB, id, (i * MAX_REC_PAYLOAD) as u32, c),
                descs: Vec::new(),
            });
        }
        self.sent += 1;
        Some(id)
    }

    /// A BLOB chunk from the channel.
    pub fn chunk(&mut self, id: u32, off: u32, bytes: &[u8]) -> Result<(), BlobError> {
        if (id & 0x8000_0000 != 0) == self.host_side || id & 0x7fff_ffff == 0 {
            return Err(BlobError::BadId(id));
        }
        if !self.incoming.contains_key(&id) {
            if off != 0 {
                return Err(BlobError::OutOfOrder(id));
            }
            let fd = sys::memfd(c"nvgpu-wl-blob", 0).map_err(|_| BlobError::Io)?;
            self.incoming.insert(id, Incoming { fd, have: 0 });
        }
        let inc = self.incoming.get_mut(&id).unwrap();
        if off as u64 != inc.have {
            return Err(BlobError::OutOfOrder(id));
        }
        if inc.have + bytes.len() as u64 > MAX_BLOB
            || self.pending_bytes + bytes.len() as u64 > MAX_PENDING
        {
            return Err(BlobError::TooBig(id));
        }
        sys::pwrite_full(inc.fd.as_raw_fd(), bytes, inc.have).map_err(|_| BlobError::Io)?;
        inc.have += bytes.len() as u64;
        self.pending_bytes += bytes.len() as u64;
        Ok(())
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
        if inc.have != len {
            return Err(BlobError::Incomplete(id));
        }
        sys::seal_readonly(inc.fd.as_raw_fd()).map_err(|_| BlobError::Io)?;
        self.received += 1;
        Ok(inc.fd)
    }
}

impl From<io::Error> for BlobError {
    fn from(_: io::Error) -> Self {
        BlobError::Io
    }
}
