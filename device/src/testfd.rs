//! Test-only: whether this process still holds the other end of a pipe.
//!
//! A test that checks the backend closed its end of a pipe asks the kernel:
//! EPIPE on a write, EOF on a read. The kernel counts every process's
//! descriptors, and sandbox.rs's tests fork: for a moment each child holds a
//! copy of every descriptor the test process has, other tests' pipe ends
//! included. So a "still open" from the kernel is checked again against this
//! process's own descriptor table, which no child shares.

#![forbid(unsafe_code)]

use std::os::fd::{AsRawFd, BorrowedFd};

/// Whether `end` is the only descriptor of this process on its file: for a
/// pipe, the other end is closed here.
pub fn only_end_here(end: BorrowedFd) -> bool {
    let link = |p: &std::path::Path| std::fs::read_link(p).ok();
    let me = link(format!("/proc/self/fd/{}", end.as_raw_fd()).as_ref());
    assert!(me.is_some(), "descriptor {} is not open", end.as_raw_fd());
    let n = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| link(&e.path()) == me)
        .count();
    n == 1
}
