// SPDX-License-Identifier: Apache-2.0
//! Bytes and descriptors on their way to the local Wayland peer.
//!
//! libwayland receives descriptors into a FIFO that messages draw from as they
//! are demarshalled, and reads at most 28 per `recvmsg` (`connection.c`,
//! `MAX_FDS_OUT`/`CLEN`): a `sendmsg` carrying more has the rest truncated. So
//! output is kept as segments cut at message boundaries, each with at most 28
//! descriptors, and a segment's descriptors go with its first byte -- which is
//! before the messages that consume them, as the receiver requires.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::sys;
use crate::wire::MAX_FDS_PER_SENDMSG;

struct Seg {
    data: Vec<u8>,
    fds: Vec<OwnedFd>,
    sent: usize,
}

#[derive(Default)]
pub struct LocalOut {
    segs: VecDeque<Seg>,
    bytes: usize,
}

impl LocalOut {
    /// Append one whole message and the descriptors it consumes.
    pub fn push(&mut self, msg: &[u8], fds: Vec<OwnedFd>) {
        let need_new = match self.segs.back() {
            None => true,
            Some(s) => {
                s.sent > 0
                    || s.fds.len() + fds.len() > MAX_FDS_PER_SENDMSG
                    || s.data.len() > 64 * 1024
            }
        };
        if need_new {
            self.segs.push_back(Seg {
                data: Vec::new(),
                fds: Vec::new(),
                sent: 0,
            });
        }
        let s = self.segs.back_mut().unwrap();
        s.data.extend_from_slice(msg);
        s.fds.extend(fds);
        self.bytes += msg.len();
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Bytes not yet written.
    pub fn len(&self) -> usize {
        self.bytes
    }

    /// Descriptors not yet sent.
    pub fn fds(&self) -> usize {
        self.segs.iter().map(|s| s.fds.len()).sum()
    }

    /// Write as much as the socket takes. `Ok(true)` when everything went.
    pub fn flush(&mut self, sock: RawFd) -> io::Result<bool> {
        while let Some(s) = self.segs.front_mut() {
            let raw: Vec<RawFd> = if s.sent == 0 {
                s.fds.iter().map(|f| f.as_raw_fd()).collect()
            } else {
                Vec::new()
            };
            match sys::send_with_fds(sock, &s.data[s.sent..], &raw) {
                Ok(n) => {
                    if s.sent == 0 {
                        // Sent with the first byte; ours can close now.
                        s.fds.clear();
                    }
                    s.sent += n;
                    self.bytes -= n;
                    if s.sent == s.data.len() {
                        self.segs.pop_front();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    /// Everything queued, for tests and in-process peers: (bytes, fds) per
    /// segment.
    pub fn drain(&mut self) -> Vec<(Vec<u8>, Vec<OwnedFd>)> {
        self.bytes = 0;
        self.segs
            .drain(..)
            .map(|s| (s.data[s.sent..].to_vec(), s.fds))
            .collect()
    }
}
