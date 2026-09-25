//! Pipes, as credit-controlled byte streams.
//!
//! Every pipe in the allowed protocols is a write end handed to a peer that
//! will write into it (`wl_data_offer.receive`, `wl_data_source.send`, and
//! their primary-selection twins). The side that *received* that write end
//! from its local peer is the stream's **sink**: it keeps the descriptor and
//! writes into it whatever arrives. The other side is the **source**: it makes
//! a pipe, gives its local peer the write end in place of the original, and
//! forwards what it reads from the read end.
//!
//! Flow control is by credit: the source may have at most `WINDOW` bytes
//! unacknowledged, and the sink returns credit as it writes. So a slow reader
//! on one side stalls the writer on the other through its own pipe, never
//! through an unbounded buffer here. End of data is explicit (`STREAM_EOF`
//! from the source) rather than guessed from a short read, and a sink whose
//! reader went away tells the source to stop the same way.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::frame::{REC_STREAM_CREDIT, REC_STREAM_DATA, REC_STREAM_EOF, Unit, record};
use crate::sys;

/// Bytes a source may send before the sink acknowledges any.
pub const WINDOW: usize = 256 * 1024;
const CHUNK: usize = 64 * 1024;
/// Open streams per connection. A clipboard transfer is one; a peer asking
/// for thousands is only after this side's descriptors.
pub const MAX_STREAMS: usize = 256;

enum Kind {
    Source {
        rd: OwnedFd,
        credit: usize,
    },
    Sink {
        wr: OwnedFd,
        pending: VecDeque<u8>,
        eof: bool,
        uncredited: usize,
    },
}

pub struct Streams {
    map: HashMap<u32, Kind>,
    /// Ids this side allocates are in its half of the space: the guest's have
    /// bit 31 clear, the host's set.
    host_side: bool,
    next: u32,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub opened: u64,
}

/// What an event loop should wait for on one stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Interest {
    pub id: u32,
    pub fd: RawFd,
    pub read: bool,
    pub write: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum StreamError {
    /// An id from the wrong half, or already in use.
    BadId(u32),
    /// Data beyond the credit this side granted.
    Overrun(u32),
    /// More than `MAX_STREAMS` open.
    TooMany,
}

impl Streams {
    pub fn new(host_side: bool) -> Self {
        Self {
            map: HashMap::new(),
            host_side,
            next: 1,
            bytes_out: 0,
            bytes_in: 0,
            opened: 0,
        }
    }

    fn ours(&self, id: u32) -> bool {
        (id & 0x8000_0000 != 0) == self.host_side
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// A write end arrived from the local peer: become its sink.
    pub fn add_sink(&mut self, wr: OwnedFd) -> io::Result<u32> {
        if self.map.len() >= MAX_STREAMS {
            return Err(io::Error::other("too many streams"));
        }
        sys::set_nonblock(wr.as_raw_fd())?;
        let id = loop {
            let id = (self.next & 0x7fff_ffff) | if self.host_side { 0x8000_0000 } else { 0 };
            self.next = self.next.wrapping_add(1).max(1);
            if id & 0x7fff_ffff != 0 && !self.map.contains_key(&id) {
                break id;
            }
        };
        self.map.insert(
            id,
            Kind::Sink {
                wr,
                pending: VecDeque::new(),
                eof: false,
                uncredited: 0,
            },
        );
        self.opened += 1;
        Ok(id)
    }

    /// The far side is sink `id`: make the pipe, keep the read end, and return
    /// the write end for the local peer.
    pub fn add_source(&mut self, id: u32) -> Result<OwnedFd, StreamError> {
        if self.ours(id) || id & 0x7fff_ffff == 0 || self.map.contains_key(&id) {
            return Err(StreamError::BadId(id));
        }
        if self.map.len() >= MAX_STREAMS {
            return Err(StreamError::TooMany);
        }
        let (rd, wr) = sys::pipe().map_err(|_| StreamError::BadId(id))?;
        self.map.insert(id, Kind::Source { rd, credit: WINDOW });
        self.opened += 1;
        Ok(wr)
    }

    pub fn interest(&self) -> Vec<Interest> {
        self.map
            .iter()
            .map(|(&id, k)| match k {
                Kind::Source { rd, credit } => Interest {
                    id,
                    fd: rd.as_raw_fd(),
                    read: *credit > 0,
                    write: false,
                },
                Kind::Sink { wr, pending, .. } => Interest {
                    id,
                    fd: wr.as_raw_fd(),
                    read: false,
                    write: !pending.is_empty(),
                },
            })
            .collect()
    }

    /// STREAM_DATA from the channel.
    pub fn data(&mut self, id: u32, bytes: &[u8], out: &mut Vec<Unit>) -> Result<(), StreamError> {
        let Some(Kind::Sink { pending, .. }) = self.map.get_mut(&id) else {
            // Closed here already (its reader went away); the source will
            // stop when our EOF reaches it.
            return Ok(());
        };
        if pending.len() + bytes.len() > WINDOW {
            return Err(StreamError::Overrun(id));
        }
        pending.extend(bytes);
        self.bytes_in += bytes.len() as u64;
        self.io(id, false, true, out);
        Ok(())
    }

    /// STREAM_EOF from the channel: end of data for a sink, stop for a source.
    pub fn eof(&mut self, id: u32, out: &mut Vec<Unit>) {
        match self.map.get_mut(&id) {
            Some(Kind::Sink { eof, .. }) => {
                *eof = true;
                self.io(id, false, true, out);
            }
            Some(Kind::Source { .. }) => {
                self.map.remove(&id);
            }
            None => {}
        }
    }

    pub fn credit(&mut self, id: u32, n: u32) {
        if let Some(Kind::Source { credit, .. }) = self.map.get_mut(&id) {
            *credit = (*credit + n as usize).min(WINDOW);
        }
    }

    /// Do whatever I/O the stream is ready for; queue records in `out`.
    pub fn io(&mut self, id: u32, readable: bool, writable: bool, out: &mut Vec<Unit>) {
        let mut remove = false;
        match self.map.get_mut(&id) {
            Some(Kind::Source { rd, credit }) if readable && *credit > 0 => {
                let mut buf = vec![0u8; (*credit).min(CHUNK)];
                match sys::read(rd.as_raw_fd(), &mut buf) {
                    Ok(0) => {
                        out.push(Unit {
                            rec: record(REC_STREAM_EOF, id, 0, &[]),
                            descs: Vec::new(),
                        });
                        remove = true;
                    }
                    Ok(n) => {
                        *credit -= n;
                        self.bytes_out += n as u64;
                        out.push(Unit {
                            rec: record(REC_STREAM_DATA, id, 0, &buf[..n]),
                            descs: Vec::new(),
                        });
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => {
                        let errno = e.raw_os_error().unwrap_or(libc::EIO) as u32;
                        out.push(Unit {
                            rec: record(REC_STREAM_EOF, id, errno, &[]),
                            descs: Vec::new(),
                        });
                        remove = true;
                    }
                }
            }
            Some(Kind::Sink {
                wr,
                pending,
                eof,
                uncredited,
            }) if writable => {
                while !pending.is_empty() {
                    let (a, _) = pending.as_slices();
                    match sys::write(wr.as_raw_fd(), a) {
                        Ok(n) => {
                            pending.drain(..n);
                            *uncredited += n;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => {
                            // The reader went away: tell the source to stop.
                            let errno = e.raw_os_error().unwrap_or(libc::EPIPE) as u32;
                            out.push(Unit {
                                rec: record(REC_STREAM_EOF, id, errno, &[]),
                                descs: Vec::new(),
                            });
                            remove = true;
                            break;
                        }
                    }
                }
                if !remove {
                    if *uncredited > 0 && (*uncredited >= WINDOW / 4 || pending.is_empty()) && !*eof
                    {
                        out.push(Unit {
                            rec: record(REC_STREAM_CREDIT, id, *uncredited as u32, &[]),
                            descs: Vec::new(),
                        });
                        *uncredited = 0;
                    }
                    if *eof && pending.is_empty() {
                        remove = true;
                    }
                }
            }
            _ => {}
        }
        if remove {
            self.map.remove(&id);
        }
    }
}
