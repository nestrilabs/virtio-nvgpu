//! Guest RAM, as the backend can reach it, and how a guest's pages become one
//! host address.
//!
//! RM registers memory by a CPU address: it takes an address and a length and
//! pins what is there. The guest's address means nothing in this process, so
//! the guest sends the *physical* pages behind it instead -- pinned on its
//! side so they cannot move -- and the backend builds an address of its own
//! that aliases exactly those pages and nothing else.
//!
//! That is possible because a vhost-user frontend hands the backend each guest
//! RAM region as a file descriptor, not merely as a mapping: `vhost-user-backend`
//! builds every region with `MmapRegion::from_file`, so the fd and the offset
//! within it are still there to be read. The backend reserves a span of its own
//! address space and maps each run into it from that fd at the matching offset.
//! The result is contiguous for RM and is the guest's own memory page for page.
//!
//! Everything that could make it *not* exactly the guest's memory is checked
//! here, because this is the only place that knows both halves:
//!
//! - every run lies wholly inside one region of guest RAM, so a run cannot
//!   reach past the end of a region into whatever the backend has mapped next;
//! - the runs add up to exactly the length being registered, so RM cannot pin
//!   a byte the guest did not send;
//! - runs and lengths are page-aligned, because a partial page cannot be
//!   mapped and rounding one up would hand RM a neighbour.
//!
//! A failure leaves nothing mapped: the reservation is dropped as a unit.

use std::os::fd::{AsRawFd, OwnedFd};

/// The most runs one registration may carry.
///
/// A fully fragmented buffer costs one run per page, so this covers 4 MiB at
/// worst and any larger buffer that coalesces at all. `cuCtxCreate` registers
/// 2 MiB, which fits even with every page scattered. A guest that needs more
/// is refused and logged rather than quietly truncated: the number to raise
/// this to should come from a real workload, not from a guess.
pub const MAX_RUNS: usize = 1024;

/// The most one registration may cover. Not a security bound -- the per-run
/// checks are -- but a bound on what one message can be asked to describe.
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;

const PAGE: u64 = 4096;

/// Where one guest-physical address lives in the backend.
///
/// The descriptor is owned rather than borrowed, and the implementation is
/// expected to duplicate it. A vhost-user frontend may replace the memory
/// table at any time, and a borrowed descriptor could be closed between being
/// handed over and being mapped from -- after which the number names whatever
/// this process opens next. One `dup` per run is nothing beside the `mmap`
/// that follows it.
#[derive(Debug)]
pub struct Backing {
    /// The guest RAM region's file, this caller's own handle on it.
    pub fd: OwnedFd,
    /// Byte offset of this address within that file.
    pub offset: u64,
    /// How many bytes from here stay inside the same region. A run longer than
    /// this spans two regions and is refused rather than stitched across the
    /// gap.
    pub len: u64,
}

/// Guest RAM, as something the backend can map from.
///
/// Implemented by the transport, which is the only part that has the regions.
/// A backend with no implementation registers nothing, which is the same
/// answer it gives before it knows the host's release.
pub trait GuestRam: Send {
    /// Where `gpa` lives, or `None` if it is not guest RAM at all.
    fn backing(&self, gpa: u64) -> Option<Backing>;
}

/// One run of guest-physical memory: an address and a length, both in pages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Run {
    pub gpa: u64,
    pub len: u64,
}

/// A contiguous host mapping that aliases a guest's pages.
///
/// Unmapped as one span when dropped, which releases the backend's view of
/// those pages. The guest's own pin is the guest's to release.
#[derive(Debug)]
pub struct Stitched {
    addr: u64,
    len: usize,
}

impl Stitched {
    /// The address to give RM.
    pub fn addr(&self) -> u64 {
        self.addr
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Drop for Stitched {
    fn drop(&mut self) {
        // SAFETY: this span was reserved by `stitch` and nothing else has been
        // mapped into it; every run was placed inside it with MAP_FIXED.
        let rc = unsafe { libc::munmap(self.addr as *mut libc::c_void, self.len) };
        if rc != 0 {
            log::error!(
                "unmapping a registered span at {:#x}+{:#x} failed: {}",
                self.addr,
                self.len,
                std::io::Error::last_os_error()
            );
        }
    }
}

// SAFETY: `Stitched` owns a mapping and hands out only its address and length.
// Moving it between threads moves no borrow of this process's memory.
unsafe impl Send for Stitched {}

/// Build one host address that aliases exactly these guest pages.
///
/// `want` is the length being registered, and the runs must add up to it
/// exactly. Returns the reason on refusal, which is logged and sent back to
/// the guest as an RM status.
pub fn stitch(ram: &dyn GuestRam, runs: &[Run], want: u64) -> Result<Stitched, String> {
    if runs.is_empty() {
        return Err("no pages were sent for it".into());
    }
    if runs.len() > MAX_RUNS {
        return Err(format!(
            "it is described by {} runs and this backend carries {MAX_RUNS}",
            runs.len()
        ));
    }
    if want == 0 || want > MAX_BYTES {
        return Err(format!("it is {want} bytes, and the limit is {MAX_BYTES}"));
    }
    if !want.is_multiple_of(PAGE) {
        return Err(format!("it is {want} bytes, which is not whole pages"));
    }

    // Checked before anything is mapped, so a refusal leaves nothing behind.
    let mut total: u64 = 0;
    for (i, r) in runs.iter().enumerate() {
        if r.len == 0 {
            return Err(format!("run {i} is empty"));
        }
        if !r.gpa.is_multiple_of(PAGE) || !r.len.is_multiple_of(PAGE) {
            return Err(format!(
                "run {i} is {:#x}+{:#x}, which is not whole pages",
                r.gpa, r.len
            ));
        }
        if r.gpa.checked_add(r.len).is_none() {
            return Err(format!(
                "run {i} at {:#x} is {:#x} long and wraps",
                r.gpa, r.len
            ));
        }
        total = total
            .checked_add(r.len)
            .ok_or_else(|| "the runs add up to more than an address space".to_string())?;
    }
    if total != want {
        // Short would have RM pin past what the guest sent; long would have it
        // pin pages the registration never covered.
        return Err(format!(
            "the pages sent cover {total} bytes and it is {want}"
        ));
    }

    // One reservation, so the runs land next to each other whatever the kernel
    // would otherwise have chosen, and so a failure half way through unmaps as
    // one span.
    let len = want as usize;
    // SAFETY: an anonymous reservation at an address of the kernel's choosing.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!(
            "reserving {len} bytes for it failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let span = Stitched {
        addr: base as u64,
        len,
    };

    let mut at: u64 = 0;
    for (i, r) in runs.iter().enumerate() {
        let Some(b) = ram.backing(r.gpa) else {
            return Err(format!(
                "run {i} at {:#x} is not this guest's memory",
                r.gpa
            ));
        };
        // The run has to fit in the region it starts in. Mapping across the
        // end of one would take whatever the next region, or nothing at all,
        // happens to be -- and the guest is the one that knows its own
        // physical layout, so it is the one that should have split the run.
        if r.len > b.len {
            return Err(format!(
                "run {i} at {:#x} is {:#x} long and only {:#x} of it is in one region",
                r.gpa, r.len, b.len
            ));
        }
        // SAFETY: inside the reservation above -- `total == want == len` and
        // `at` has advanced by exactly the runs already placed.
        let got = unsafe {
            libc::mmap(
                (span.addr + at) as *mut libc::c_void,
                r.len as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_FIXED,
                b.fd.as_raw_fd(),
                b.offset as libc::off_t,
            )
        };
        if got == libc::MAP_FAILED {
            return Err(format!(
                "mapping run {i} at {:#x} failed: {}",
                r.gpa,
                std::io::Error::last_os_error()
            ));
        }
        at += r.len;
    }

    Ok(span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// Guest RAM made of memfds, one per region, so the stitcher can be run
    /// against something whose contents are known.
    struct FakeRam {
        regions: Vec<(u64, u64, OwnedFd)>, // base, len, fd
    }

    impl FakeRam {
        fn new(spans: &[(u64, u64)]) -> Self {
            let regions = spans
                .iter()
                .map(|&(base, len)| {
                    let raw =
                        unsafe { libc::memfd_create(c"guest-ram".as_ptr(), libc::MFD_CLOEXEC) };
                    assert!(raw >= 0);
                    assert_eq!(unsafe { libc::ftruncate(raw, len as libc::off_t) }, 0);
                    (base, len, unsafe { OwnedFd::from_raw_fd(raw) })
                })
                .collect();
            Self { regions }
        }

        /// Fill every page with a byte derived from its guest address, so a
        /// stitched span can be checked page by page.
        fn fill(&self) {
            for &(base, len, ref fd) in &self.regions {
                for page in 0..len / PAGE {
                    let b = [mark(base + page * PAGE); PAGE as usize];
                    let n = unsafe {
                        libc::pwrite(
                            fd.as_raw_fd(),
                            b.as_ptr() as *const libc::c_void,
                            PAGE as usize,
                            (page * PAGE) as libc::off_t,
                        )
                    };
                    assert_eq!(n, PAGE as isize);
                }
            }
        }
    }

    fn mark(gpa: u64) -> u8 {
        (gpa / PAGE) as u8 ^ 0xa5
    }

    impl GuestRam for FakeRam {
        fn backing(&self, gpa: u64) -> Option<Backing> {
            self.regions
                .iter()
                .find(|&&(base, len, _)| gpa >= base && gpa < base + len)
                .map(|&(base, len, ref fd)| Backing {
                    fd: fd.try_clone().expect("duplicating a memfd"),
                    offset: gpa - base,
                    len: base + len - gpa,
                })
        }
    }

    fn ram() -> FakeRam {
        // Two regions with a hole between them, as a real guest has.
        FakeRam::new(&[(0, 16 * PAGE), (1024 * PAGE, 16 * PAGE)])
    }

    fn page_at(s: &Stitched, i: u64) -> u8 {
        // SAFETY: inside the stitched span, which is mapped read-write.
        unsafe { *((s.addr() + i * PAGE) as *const u8) }
    }

    /// The property the whole thing exists for: the host address RM is given
    /// reads back the guest's own pages, in the order the guest asked for,
    /// however they are scattered.
    #[test]
    fn a_stitched_span_is_the_guest_s_pages_in_order() {
        let ram = ram();
        ram.fill();
        // Out of order, from both regions, with a hole skipped.
        let runs = [
            Run {
                gpa: 8 * PAGE,
                len: 2 * PAGE,
            },
            Run {
                gpa: 1024 * PAGE,
                len: PAGE,
            },
            Run { gpa: 0, len: PAGE },
        ];
        let s = stitch(&ram, &runs, 4 * PAGE).expect("these are all guest pages");

        assert_eq!(page_at(&s, 0), mark(8 * PAGE));
        assert_eq!(page_at(&s, 1), mark(9 * PAGE));
        assert_eq!(page_at(&s, 2), mark(1024 * PAGE));
        assert_eq!(page_at(&s, 3), mark(0));
    }

    /// And it is an alias, not a copy: a write through the span is a write to
    /// the guest's page, which is what makes the GPU and the guest agree.
    #[test]
    fn writing_through_the_span_writes_the_guest_s_page() {
        let ram = ram();
        ram.fill();
        let s = stitch(
            &ram,
            &[Run {
                gpa: 4 * PAGE,
                len: PAGE,
            }],
            PAGE,
        )
        .unwrap();
        // SAFETY: inside the span.
        unsafe { *(s.addr() as *mut u8) = 0x5c };

        let again = stitch(
            &ram,
            &[Run {
                gpa: 4 * PAGE,
                len: PAGE,
            }],
            PAGE,
        )
        .unwrap();
        assert_eq!(page_at(&again, 0), 0x5c, "the two spans must see one page");
    }

    /// An address that is not guest RAM is the attack this is for. Anything
    /// the backend has mapped -- its own heap, the SHM window, a device
    /// mapping -- is reachable if a run is taken on trust.
    #[test]
    fn an_address_outside_guest_ram_is_refused() {
        let ram = ram();
        // Past the first region, past the second, and in the hole between
        // them. The wrapping case is refused earlier still, for a more
        // specific reason: see `a_run_that_wraps_is_refused`.
        for gpa in [100 * PAGE, 1040 * PAGE, (1 << 40) * PAGE] {
            let e = stitch(&ram, &[Run { gpa, len: PAGE }], PAGE).unwrap_err();
            assert!(e.contains("not this guest's memory"), "{gpa:#x}: {e}");
        }
    }

    /// A run that starts in a region and runs off the end of it would take
    /// whatever follows. The guest knows its own layout and should have split
    /// the run.
    #[test]
    fn a_run_that_leaves_its_region_is_refused() {
        let ram = ram();
        let e = stitch(
            &ram,
            &[Run {
                gpa: 15 * PAGE,
                len: 2 * PAGE,
            }],
            2 * PAGE,
        )
        .unwrap_err();
        assert!(e.contains("in one region"), "{e}");
    }

    /// The runs have to account for the whole registration and no more. Short
    /// leaves RM reading past what was sent; long covers pages the
    /// registration never named.
    #[test]
    fn the_pages_must_add_up_to_what_is_being_registered() {
        let ram = ram();
        for (runs, want) in [
            (vec![Run { gpa: 0, len: PAGE }], 2 * PAGE),
            (
                vec![Run {
                    gpa: 0,
                    len: 2 * PAGE,
                }],
                PAGE,
            ),
        ] {
            let e = stitch(&ram, &runs, want).unwrap_err();
            assert!(e.contains("cover"), "{e}");
        }
    }

    /// A partial page cannot be mapped, and rounding one up would hand RM the
    /// neighbouring page -- which may be anyone's.
    #[test]
    fn a_run_that_is_not_whole_pages_is_refused() {
        let ram = ram();
        let e = stitch(&ram, &[Run { gpa: 64, len: PAGE }], PAGE).unwrap_err();
        assert!(e.contains("whole pages"), "{e}");
        let e = stitch(&ram, &[Run { gpa: 0, len: 64 }], 64).unwrap_err();
        assert!(e.contains("whole pages"), "{e}");
    }

    #[test]
    fn an_unreasonable_registration_is_refused_before_anything_is_mapped() {
        let ram = ram();
        assert!(stitch(&ram, &[], 0).unwrap_err().contains("no pages"));
        assert!(
            stitch(&ram, &[Run { gpa: 0, len: PAGE }], MAX_BYTES + PAGE)
                .unwrap_err()
                .contains("limit")
        );
        let many: Vec<_> = (0..MAX_RUNS + 1)
            .map(|i| Run {
                gpa: i as u64 * PAGE,
                len: PAGE,
            })
            .collect();
        assert!(
            stitch(&ram, &many, (MAX_RUNS as u64 + 1) * PAGE)
                .unwrap_err()
                .contains("runs")
        );
    }

    /// A run that wraps the address space would pass a naive end-of-run check.
    #[test]
    fn a_run_that_wraps_is_refused() {
        let ram = ram();
        let e = stitch(
            &ram,
            &[Run {
                gpa: u64::MAX - PAGE + 1,
                len: 2 * PAGE,
            }],
            2 * PAGE,
        )
        .unwrap_err();
        assert!(e.contains("wraps"), "{e}");
    }
}
