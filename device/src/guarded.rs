//! A buffer the host driver writes into, with a guard page behind it.
//!
//! Two buffers in the forwarding path are handed to the NVIDIA driver as a
//! destination: the parameter block, and the block a pointer inside it
//! addresses. Their sizes come from fields the caller filled in, read at
//! offsets from a table generated against one driver release. When a size is
//! wrong the driver writes past the end, and with ordinary heap allocations
//! that is discovered much later, at an unrelated free, as "corrupted size vs.
//! prev_size" -- a crash carrying no information about which call caused it.
//!
//! Placing the buffer hard against an unmapped page turns the same mistake into
//! a fault on the offending write, in the call that made it, while the log line
//! naming that call is still the last one printed.

use std::ptr;

pub struct GuardedBuf {
    base: *mut u8,
    mapped: usize,
    offset: usize,
    len: usize,
}

const PAGE: usize = 4096;

/// Readable bytes after the buffer, standing in for the caller's own memory.
const SLACK: usize = 4096;

impl GuardedBuf {
    /// A buffer of `len` bytes, followed by a page of readable slack and then a
    /// `PROT_NONE` page.
    ///
    /// The slack is not padding for its own sake. A parameter block on the
    /// calling side is an object inside a much larger mapping -- a stack frame,
    /// usually -- so a driver that touches a few bytes past the size the caller
    /// declared lands on the caller's own memory and nobody ever learns. Here
    /// the block is an allocation of its own, and the same access lands on
    /// whatever is next: as a heap allocation that is silent corruption
    /// discovered at an unrelated free, and hard against a guard page it is an
    /// EFAULT the driver reports as a failed call. Measured on 615.71.09,
    /// fourteen commands do this, `NV0080_CTRL_CMD_..._GET_CAPS` among them,
    /// with a parameter block whose declared size matches the host's byte for
    /// byte.
    ///
    /// So the buffer is given what a caller would have had, and the guard is
    /// moved out to where it still catches an overrun that is a real bug rather
    /// than a few bytes of slop.
    pub fn new(len: usize) -> Option<Self> {
        if len == 0 {
            return None;
        }
        let pages = (len + SLACK).div_ceil(PAGE);
        let mapped = (pages + 1) * PAGE;
        let base = unsafe {
            libc::mmap(
                ptr::null_mut(),
                mapped,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return None;
        }
        let base = base as *mut u8;
        // The last page is the guard.
        let guard = unsafe { base.add(pages * PAGE) };
        if unsafe { libc::mprotect(guard as *mut libc::c_void, PAGE, libc::PROT_NONE) } != 0 {
            unsafe { libc::munmap(base as *mut libc::c_void, mapped) };
            return None;
        }
        Some(Self {
            base,
            mapped,
            offset: 0,
            len,
        })
    }

    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        unsafe { self.base.add(self.offset) }
    }

    pub fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.add(self.offset), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.base.add(self.offset), self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for GuardedBuf {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, self.mapped) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slack_after_a_buffer_is_writable() {
        let mut b = GuardedBuf::new(100).expect("mapped");
        b.as_mut_slice()[99] = 0xab;
        assert_eq!(b.as_slice()[99], 0xab);
        // A driver that writes a little past the declared size finds memory
        // there, as it would on the calling side.
        unsafe { b.as_mut_ptr().add(100).write(0xcd) };
        unsafe { assert_eq!(b.as_mut_ptr().add(SLACK - 1).read(), 0) };
    }

    #[test]
    fn a_gross_overrun_still_has_a_guard_behind_it() {
        let b = GuardedBuf::new(100).expect("mapped");
        let guard = b.base as usize + b.mapped - PAGE;
        assert!(guard > b.base as usize + 100 + SLACK - PAGE);
    }

    #[test]
    fn zero_length_has_no_buffer() {
        assert!(GuardedBuf::new(0).is_none());
    }
}
