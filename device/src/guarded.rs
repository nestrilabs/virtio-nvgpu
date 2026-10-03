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

// SAFETY: a GuardedBuf owns its mapping outright; nothing else holds the
// pointer, so moving it to another thread moves sole ownership with it.
unsafe impl Send for GuardedBuf {}

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

    /// Bytes this mapping can hold with the full slack still behind them.
    fn capacity(&self) -> usize {
        self.mapped - PAGE - SLACK
    }

    /// Reuse the mapping for `len` bytes, placed so that the slack and the
    /// guard sit right behind them, as they would in a fresh buffer of that
    /// size. The bytes from the end of the buffer to the guard are zeroed, so
    /// nothing from an earlier call is there for the driver to read.
    fn reset(&mut self, len: usize) -> bool {
        if len == 0 || len > self.capacity() {
            return false;
        }
        // 16-byte aligned, as a fresh mapping's start is (and more than any
        // parameter block needs).
        self.offset = (self.capacity() - len) & !15;
        self.len = len;
        let tail = self.mapped - PAGE - self.offset - len;
        unsafe { ptr::write_bytes(self.base.add(self.offset + len), 0, tail) };
        true
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

/// A few guarded buffers kept for reuse.
///
/// A fresh buffer is three system calls -- mmap, mprotect, munmap -- and a
/// page fault for each page the driver touches, on every forwarded call that
/// carries a parameter block. Most of those blocks are well under a page, so
/// a handful of small mappings serves nearly all of them.
#[derive(Default)]
pub struct GuardPool {
    free: Vec<GuardedBuf>,
}

impl GuardPool {
    /// Buffers kept, and the largest kept. Larger blocks are rare (class
    /// lists, caps tables) and get a mapping of their own each time.
    const KEEP: usize = 8;
    const KEEP_MAX: usize = 64 * 1024;

    /// A buffer for `len` bytes: a kept one if any is big enough, else fresh.
    pub fn take(&mut self, len: usize) -> Option<GuardedBuf> {
        let best = self
            .free
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity() >= len)
            .min_by_key(|(_, b)| b.capacity())
            .map(|(i, _)| i);
        if let Some(i) = best {
            let mut b = self.free.swap_remove(i);
            if b.reset(len) {
                return Some(b);
            }
        }
        GuardedBuf::new(len)
    }

    /// Hand a buffer back.
    pub fn give(&mut self, b: GuardedBuf) {
        if self.free.len() < Self::KEEP && b.capacity() <= Self::KEEP_MAX {
            self.free.push(b);
        }
    }
}

/// A pooled buffer that goes back to its pool when dropped, so every early
/// return from a handler gives it back too.
pub struct Lease<'a> {
    buf: Option<GuardedBuf>,
    pool: &'a std::cell::RefCell<GuardPool>,
}

impl GuardPool {
    pub fn lease(pool: &std::cell::RefCell<GuardPool>, len: usize) -> Option<Lease<'_>> {
        let buf = pool.borrow_mut().take(len)?;
        Some(Lease {
            buf: Some(buf),
            pool,
        })
    }
}

impl std::ops::Deref for Lease<'_> {
    type Target = GuardedBuf;
    fn deref(&self) -> &GuardedBuf {
        self.buf.as_ref().expect("held until drop")
    }
}

impl std::ops::DerefMut for Lease<'_> {
    fn deref_mut(&mut self) -> &mut GuardedBuf {
        self.buf.as_mut().expect("held until drop")
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(b) = self.buf.take() {
            self.pool.borrow_mut().give(b);
        }
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

    /// A reused buffer must look like a fresh one of its new size: the same
    /// slack behind it, the guard right after, and no bytes left over from
    /// the call before.
    #[test]
    fn a_reused_buffer_has_its_slack_and_guard_where_a_fresh_one_would() {
        let mut pool = GuardPool::default();
        let mut b = pool.take(3000).expect("mapped");
        b.as_mut_slice().fill(0xee);
        let base = b.base as usize;
        pool.give(b);

        let mut b = pool.take(100).expect("reused");
        assert_eq!(b.base as usize, base, "the kept mapping is reused");
        let guard = b.base as usize + b.mapped - PAGE;
        let end = b.as_mut_ptr() as usize + 100;
        assert!(guard - end >= SLACK, "full slack behind the buffer");
        assert!(guard - end < SLACK + 16, "guard right after the slack");
        assert_eq!(b.as_mut_ptr() as usize % 16, 0);
        let tail = unsafe { std::slice::from_raw_parts(end as *const u8, guard - end) };
        assert!(
            tail.iter().all(|&x| x == 0),
            "nothing left from the last call"
        );
    }

    #[test]
    fn a_block_bigger_than_any_kept_buffer_gets_a_fresh_one() {
        let mut pool = GuardPool::default();
        let b = pool.take(100).expect("mapped");
        pool.give(b);
        let big = pool.take(3 * PAGE).expect("mapped");
        assert!(big.capacity() >= 3 * PAGE);
        assert_eq!(pool.free.len(), 1, "the small one is still kept");
    }

    #[test]
    fn zero_length_has_no_buffer() {
        assert!(GuardedBuf::new(0).is_none());
    }
}
