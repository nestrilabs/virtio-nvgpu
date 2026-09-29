// SPDX-License-Identifier: Apache-2.0
//! Bytes and descriptors from the local Wayland peer that no message has
//! taken yet.
//!
//! What a read brings is kept here until [`Engine::from_local`] takes it: the
//! bytes up to the last whole message (the rest waits for the next read, or,
//! while the channel has [`CHANNEL_HIGH_WATER`] queued, for the channel to
//! take some), and the descriptors received beside them, which messages take
//! in order as their signatures say (`wire.rs`). The owner decides where this
//! lives -- the guest daemon beside each client, the backend's reader thread
//! outside the connection's lock, where the lease probe reads the bytes -- and
//! this holds the one rule for what may pile up in it.
//!
//! [`Engine::from_local`]: crate::engine::Engine::from_local
//! [`CHANNEL_HIGH_WATER`]: crate::engine::CHANNEL_HIGH_WATER

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::os::fd::OwnedFd;

use crate::engine::{Blame, Fatal};
use crate::wire::MAX_FDS_QUEUED;

#[derive(Default)]
pub struct LocalIn {
    pub(crate) data: Vec<u8>,
    pub(crate) fds: VecDeque<OwnedFd>,
}

impl LocalIn {
    /// Input already read, for tests and in-process peers.
    pub fn new(data: Vec<u8>, fds: impl Into<VecDeque<OwnedFd>>) -> Self {
        Self {
            data,
            fds: fds.into(),
        }
    }

    /// Add what one read brought. Past [`MAX_FDS_QUEUED`] descriptors that
    /// no message has taken, the peer is sending descriptors beside messages
    /// that carry none, which would otherwise be held for ever: the error
    /// that ends the connection is returned, as libwayland ends a peer that
    /// overflows its descriptor ring. What was read is kept either way.
    pub fn push(&mut self, data: &[u8], fds: Vec<OwnedFd>) -> Result<(), Fatal> {
        self.data.extend_from_slice(data);
        self.fds.extend(fds);
        if self.fds.len() > MAX_FDS_QUEUED {
            return Err(Fatal::no_memory(
                Blame::Local,
                format!("more than {MAX_FDS_QUEUED} file descriptors sent that no message takes"),
            ));
        }
        Ok(())
    }

    /// The bytes not taken yet: at most a partial message, unless the
    /// engine stopped taking input.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Descriptors no message has taken yet.
    pub fn fds(&self) -> usize {
        self.fds.len()
    }

    /// Let go of everything (the connection is over).
    pub fn clear(&mut self) {
        self.data.clear();
        self.fds.clear();
    }
}
