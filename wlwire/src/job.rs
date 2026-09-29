// SPDX-License-Identifier: Apache-2.0
//! Lazy reads: bytes a message promised the channel, read from a descriptor
//! only as the channel takes them.
//!
//! Two things are sent this way: the copy a `wl_surface.commit` needs of an
//! shm buffer ([`SyncJob`](crate::shm::SyncJob)) and the rest of a blob after
//! its first chunk ([`BlobJob`](crate::blob::BlobJob)). Each is queued for the
//! channel at what it has left to read ([`Job::remaining`]), which is what
//! the engine counts toward the channel's backlog and so toward the input
//! limit, and read a record at a time ([`Job::next_unit`]).
//!
//! They differ in what a short read means: a commit's copy stops (the client
//! truncated its own pool, and the compositor keeps what it had there), and a
//! blob pads with zeros (the receiver was promised its size). The engine does
//! not need to know which: it counts what a step took off `remaining`, not
//! what the step says it read, so however a job ends, what was counted for it
//! is given back. Counted by the bytes read instead, a truncated pool's
//! commit would stay charged for good, and the client's input blocked
//! behind a backlog that was never going to drain.

#![forbid(unsafe_code)]

use crate::frame::Unit;

pub trait Job {
    /// Bytes this job may still put on the channel. Never grows.
    fn remaining(&self) -> u64;

    /// The next record and the bytes of the source it carries; `None` once
    /// there is nothing more to send, which a job may decide early (a short
    /// read). `None` ends the job: whatever `remaining` still says goes with
    /// it.
    fn next_unit(&mut self) -> Option<(Unit, usize)>;
}
