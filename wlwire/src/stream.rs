// SPDX-License-Identifier: Apache-2.0
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
//!
//! **What a sink may hold.** Every byte a source sends waits in its sink
//! until the local reader takes it, so the sink's grants are what the far
//! side can make this side hold. They shrink as streams are added: a sink
//! grants [`window`] of the open sinks, `WINDOW` for the first sixteen and
//! [`SINK_TOTAL`] shared among more, down to [`MIN_WINDOW`] -- one transfer
//! runs at full speed, and 256 at once hold 4 MiB, not 64. The first grant
//! rides in the stream's descriptor (`b`) when both ends said
//! [`HELLO_STREAM_WINDOW`](crate::frame::HELLO_STREAM_WINDOW); an older peer
//! starts at `WINDOW`, and later grants shrink either way. What sinks hold is
//! also charged to the budgets the owner gives ([`ByteBudget`]: the backend's
//! per-VM queue, by guest process), and a sink whose data the budget refuses
//! ends its stream (`ENOBUFS`), not the connection.

#![forbid(unsafe_code)]

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use crate::budget::Budgets;
use crate::frame::{REC_STREAM_CREDIT, REC_STREAM_DATA, REC_STREAM_EOF, Unit, record};
use crate::sys;

/// Bytes a source may send before the sink acknowledges any, at most.
pub const WINDOW: usize = 256 * 1024;
/// What a connection's sinks are granted together once there are more than
/// `SINK_TOTAL / WINDOW` of them.
pub const SINK_TOTAL: usize = 4 << 20;
/// The least a sink grants, however many there are.
pub const MIN_WINDOW: usize = 16 * 1024;
const CHUNK: usize = 64 * 1024;

/// A sink's credit with `n` sinks open.
pub fn window(n: usize) -> usize {
    (SINK_TOTAL / n.max(1)).clamp(MIN_WINDOW, WINDOW)
}

/// Bytes shared with other connections that sinks' pending data is charged
/// to, all or nothing.
pub trait ByteBudget: Send + Sync {
    fn take(&self, n: usize) -> bool;
    fn give(&self, n: usize);
}
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
        /// Credit the source holds: what it may still send.
        granted: usize,
    },
}

pub struct Streams {
    map: HashMap<u32, Kind>,
    /// Ids this side allocates are in its half of the space: the guest's have
    /// bit 31 clear, the host's set.
    host_side: bool,
    next: u32,
    /// The peer takes a sink's first grant from its descriptor.
    peer_windows: bool,
    /// Sinks open.
    sinks: usize,
    budgets: Budgets<dyn ByteBudget>,
    /// Ended streams' descriptors, until [`Streams::take_closed`].
    closed: Vec<OwnedFd>,
    pub bytes_out: u64,
    pub bytes_in: u64,
    pub opened: u64,
}

/// What an event loop should wait for on one stream. Neither `read` nor
/// `write` means nothing: an event loop stops watching the descriptor
/// altogether then, since epoll reports an error or a hangup whatever it was
/// asked for, and the stream would wake it for nothing until the far side
/// moves.
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

/// What sinks held goes back to the budgets however the streams go.
impl Drop for Streams {
    fn drop(&mut self) {
        self.clear();
    }
}

impl Streams {
    pub fn new(host_side: bool) -> Self {
        Self {
            map: HashMap::new(),
            host_side,
            next: 1,
            peer_windows: false,
            sinks: 0,
            budgets: Budgets::default(),
            closed: Vec::new(),
            bytes_out: 0,
            bytes_in: 0,
            opened: 0,
        }
    }

    /// The peer said `HELLO_STREAM_WINDOW`: sinks made from now on grant
    /// [`window`] from the start, in their descriptor.
    pub fn set_peer_windows(&mut self, on: bool) {
        self.peer_windows = on;
    }

    /// Charge what sinks hold to `b` too.
    pub fn add_budget(&mut self, b: Arc<dyn ByteBudget>) {
        self.budgets.push(b);
    }

    /// Bytes sinks hold for their readers.
    pub fn held(&self) -> usize {
        self.map
            .values()
            .map(|k| match k {
                Kind::Sink { pending, .. } => pending.len(),
                Kind::Source { .. } => 0,
            })
            .sum()
    }

    /// Close every stream (the connection is over), giving back what sinks
    /// held.
    pub fn clear(&mut self) {
        let ids: Vec<u32> = self.map.keys().copied().collect();
        for id in ids {
            self.remove(id);
        }
    }

    fn remove(&mut self, id: u32) {
        match self.map.remove(&id) {
            Some(Kind::Sink { pending, wr, .. }) => {
                self.sinks -= 1;
                self.budgets.give(pending.len());
                self.closed.push(wr);
            }
            Some(Kind::Source { rd, .. }) => self.closed.push(rd),
            None => {}
        }
    }

    /// The descriptors of streams that have ended since the last call, for
    /// the owner to stop watching and then drop. Held until then because an
    /// event loop can only take a descriptor out of epoll while it is still
    /// open, and a sink's shares its open file with the process that sent
    /// it, so a registration left behind outlives the close (and would be
    /// reported for good). An owner that polls afresh each time need only
    /// drop them.
    pub fn take_closed(&mut self) -> Vec<OwnedFd> {
        std::mem::take(&mut self.closed)
    }

    /// Descriptors this holds: open streams' and ended ones not yet taken.
    pub fn fds(&self) -> usize {
        self.map.len() + self.closed.len()
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

    /// A write end arrived from the local peer: become its sink. Returns
    /// the stream id and, for a peer that takes it, the first grant for the
    /// descriptor (0 for one that starts at `WINDOW`).
    pub fn add_sink(&mut self, wr: OwnedFd) -> io::Result<(u32, u32)> {
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
        self.sinks += 1;
        let (granted, said) = if self.peer_windows {
            let w = window(self.sinks);
            (w, w as u32)
        } else {
            (WINDOW, 0)
        };
        self.map.insert(
            id,
            Kind::Sink {
                wr,
                pending: VecDeque::new(),
                eof: false,
                granted,
            },
        );
        self.opened += 1;
        Ok((id, said))
    }

    /// The far side is sink `id`, which grants `first` bytes to begin with
    /// (0: `WINDOW`, an older sink): make the pipe, keep the read end, and
    /// return the write end for the local peer.
    pub fn add_source(&mut self, id: u32, first: u32) -> Result<OwnedFd, StreamError> {
        if self.ours(id) || id & 0x7fff_ffff == 0 || self.map.contains_key(&id) {
            return Err(StreamError::BadId(id));
        }
        if self.map.len() >= MAX_STREAMS {
            return Err(StreamError::TooMany);
        }
        let credit = match first as usize {
            0 => WINDOW,
            n => n.min(WINDOW),
        };
        let (rd, wr) = sys::pipe().map_err(|_| StreamError::BadId(id))?;
        self.map.insert(id, Kind::Source { rd, credit });
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
        let Some(Kind::Sink { granted, .. }) = self.map.get(&id) else {
            // Closed here already (its reader went away); the source will
            // stop when our EOF reaches it.
            return Ok(());
        };
        if bytes.len() > *granted {
            return Err(StreamError::Overrun(id));
        }
        if !self.budgets.take(bytes.len()) {
            // The VM's share for this is spent: this transfer ends, and the
            // connection with the rest of its streams goes on.
            out.push(Unit {
                rec: record(REC_STREAM_EOF, id, libc::ENOBUFS as u32, &[]),
                descs: Vec::new(),
            });
            self.remove(id);
            return Ok(());
        }
        let Some(Kind::Sink {
            pending, granted, ..
        }) = self.map.get_mut(&id)
        else {
            unreachable!()
        };
        *granted -= bytes.len();
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
                self.remove(id);
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
        let mut written = 0;
        let target = window(self.sinks);
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
                granted,
            }) if writable => {
                while !pending.is_empty() {
                    let (a, _) = pending.as_slices();
                    match sys::write(wr.as_raw_fd(), a) {
                        Ok(n) => {
                            pending.drain(..n);
                            written += n;
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
                    // Credit back up to what a sink may hold now, which is
                    // less the more streams are open: what the source holds
                    // and what waits here, together.
                    let room = target.saturating_sub(*granted + pending.len());
                    if room > 0 && (room >= target / 4 || pending.is_empty()) && !*eof {
                        out.push(Unit {
                            rec: record(REC_STREAM_CREDIT, id, room as u32, &[]),
                            descs: Vec::new(),
                        });
                        *granted += room;
                    }
                    if *eof && pending.is_empty() {
                        remove = true;
                    }
                }
            }
            _ => {}
        }
        self.budgets.give(written);
        if remove {
            self.remove(id);
        }
    }
}
