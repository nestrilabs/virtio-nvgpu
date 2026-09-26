// SPDX-License-Identifier: Apache-2.0
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
//!
//! Only `sys` hands the buffer's address to anything: the rest of the crate
//! reads and writes it as a slice, and gives it to the host as a block of a
//! [`super::block::Arena`].

pub struct GuardedBuf {
    base: *mut u8,
    mapped: usize,
    offset: usize,
    len: usize,
}

// SAFETY: a GuardedBuf is the sole owner of its mapping. `base` comes from an
// anonymous private mmap made in `new` and is never handed out except as a
// borrow of the GuardedBuf itself (`as_slice` through `&self`, `as_mut_slice`
// and `as_mut_ptr` through `&mut self`), there is no Clone, and `Drop` unmaps
// it exactly once. So moving the value to another thread moves the only way
// to reach those pages, and no other thread keeps an alias to them: the raw
// pointer is only why the compiler cannot see that. Pages of an anonymous
// mapping are not tied to the thread that created them.
//
// This is what lets an IOCTL2 (`xfer::Prepared`, which holds its host buffers
// in an Arena of GuardedBufs) be prepared on the queue thread and executed on
// a per-file executor thread. A pointer to the buffer does reach the host
// kernel during `execute`, but only for the length of the ioctl call, on the
// thread that owns the Prepared at that moment.
unsafe impl Send for GuardedBuf {}

const PAGE: usize = 4096;

/// Readable bytes after the buffer, standing in for the caller's own memory.
pub(super) const SLACK: usize = 4096;

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
        #[cfg(miri)]
        return Self::heap(len);
        #[cfg(not(miri))]
        Self::mapped(len)
    }

    /// Miri cannot make a PROT_NONE page, and needs none: it faults an access
    /// past any allocation itself. The same layout, from the heap.
    #[cfg(miri)]
    fn heap(len: usize) -> Option<Self> {
        let mapped = ((len + SLACK).div_ceil(PAGE) + 1) * PAGE;
        let layout = std::alloc::Layout::from_size_align(mapped - PAGE, PAGE).ok()?;
        // SAFETY: a non-zero size.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        if base.is_null() {
            return None;
        }
        Some(Self {
            base,
            mapped,
            offset: 0,
            len,
        })
    }

    #[cfg(not(miri))]
    fn mapped(len: usize) -> Option<Self> {
        let pages = (len + SLACK).div_ceil(PAGE);
        let mapped = (pages + 1) * PAGE;
        // SAFETY: a fresh anonymous mapping at an address the kernel chooses;
        // nothing else can refer to it yet.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
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
        // SAFETY: `pages * PAGE` is inside the `mapped` bytes just mapped.
        let guard = unsafe { base.add(pages * PAGE) };
        // SAFETY: the last page of the mapping made above.
        if unsafe { libc::mprotect(guard as *mut libc::c_void, PAGE, libc::PROT_NONE) } != 0 {
            // SAFETY: the mapping made above, which nothing else knows of.
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

    /// The buffer's address, for the host: only `sys` hands it out.
    pub(super) fn as_mut_ptr(&mut self) -> *mut u8 {
        // SAFETY: `offset` is 0, inside the mapping.
        unsafe { self.base.add(self.offset) }
    }

    /// Bytes readable and writable from the start: the buffer and its slack.
    pub(super) fn reach(&self) -> usize {
        self.mapped - PAGE - self.offset
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `len` bytes from `base + offset` are mapped read-write for
        // as long as `self` lives, and only borrows of `self` reach them.
        unsafe { std::slice::from_raw_parts(self.base.add(self.offset), self.len) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as `as_slice`, and `&mut self` makes the borrow unique.
        unsafe { std::slice::from_raw_parts_mut(self.base.add(self.offset), self.len) }
    }

    /// The buffer and the slack after it, for tests of what a driver that
    /// writes a little past the declared size finds there.
    #[cfg(test)]
    pub(crate) fn with_slack(&mut self) -> &mut [u8] {
        let n = self.reach();
        // SAFETY: `reach()` bytes from the start are mapped read-write (the
        // slack pages come before the guard), and `&mut self` makes the
        // borrow unique.
        unsafe { std::slice::from_raw_parts_mut(self.base.add(self.offset), n) }
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
        #[cfg(miri)]
        {
            let layout = std::alloc::Layout::from_size_align(self.mapped - PAGE, PAGE).unwrap();
            // SAFETY: allocated in `new` with this layout, freed once.
            unsafe { std::alloc::dealloc(self.base, layout) };
        }
        // SAFETY: the mapping made in `new`, unmapped exactly once; no borrow
        // of `self` outlives it.
        #[cfg(not(miri))]
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.mapped)
        };
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
        let s = b.with_slack();
        s[100] = 0xcd;
        assert_eq!(s[SLACK - 1], 0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "under Miri the guard is Miri's own bounds check")]
    fn a_gross_overrun_still_has_a_guard_behind_it() {
        let b = GuardedBuf::new(100).expect("mapped");
        let guard = b.base as usize + b.mapped - PAGE;
        assert!(guard > b.base as usize + 100 + SLACK - PAGE);
    }

    #[test]
    fn a_buffer_can_be_filled_on_one_thread_and_read_on_another() {
        let mut b = GuardedBuf::new(64).expect("mapped");
        b.as_mut_slice()[..4].copy_from_slice(b"nvgp");
        let back = std::thread::spawn(move || b.as_slice()[..4].to_vec())
            .join()
            .unwrap();
        assert_eq!(back, b"nvgp");
    }

    #[test]
    fn zero_length_has_no_buffer() {
        assert!(GuardedBuf::new(0).is_none());
    }
}
