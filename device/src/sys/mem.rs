//! Mappings: every `mmap`, `munmap` and `mprotect` the backend makes, each
//! owned by a type that unmaps it exactly once, and every `MAP_FIXED` checked
//! to land inside a range that type owns.
//!
//! - [`Mapping`]: a mapping of the backend's own (a file shared, or
//!   anonymous memory), read and written by copies, never by reference --
//!   what it maps may be shared with the guest or the host driver.
//! - [`Reservation`]: an inaccessible range, into which pieces of a file are
//!   placed with `MAP_FIXED`: the OS-descriptor ranges (osdesc.rs).
//! - [`Window`]: the shared window's local backing, into which device
//!   memory is placed and from which it is withdrawn (shm.rs).
//! - [`HostSpan`]: a range of memory mapped in this process that the host
//!   kernel may be handed an address in -- a `Mapping`, a `Reservation`, or
//!   guest RAM as the transport mapped it -- with what keeps it mapped.

use std::any::Any;
use std::io;
use std::os::fd::{AsFd, AsRawFd, RawFd};
#[cfg(miri)]
use std::os::fd::OwnedFd;
use std::sync::Arc;

const PAGE: usize = 4096;

fn fixed_inside(len: usize, at: usize, n: usize) -> io::Result<()> {
    match at.checked_add(n) {
        Some(end) if end <= len && at.is_multiple_of(PAGE) && n > 0 => Ok(()),
        _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
    }
}

fn prot(writable: bool) -> i32 {
    libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 }
}

/// One region of guest RAM as the vhost-user memory table gives it:
/// guest-physical start, the backend's mapping of it, and its backing file
/// and offset in that file.
#[cfg(feature = "vhost-user")]
pub type VmRegion = (u64, HostSpan, Option<(Arc<std::fs::File>, u64)>);

/// A range of memory mapped in this process that the host may be handed an
/// address in, kept mapped for as long as any clone lives.
#[derive(Clone)]
pub struct HostSpan {
    base: u64,
    len: usize,
    #[allow(dead_code)]
    keep: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for HostSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HostSpan({:#x}+{:#x})", self.base, self.len)
    }
}

impl HostSpan {
    /// Where it starts, as a number (for comparisons and logs).
    pub fn addr(&self) -> u64 {
        self.base
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The address `at` bytes in, if that is inside.
    pub fn addr_at(&self, at: u64) -> Option<u64> {
        (at < self.len as u64).then(|| self.base + at)
    }

    /// The `len` bytes from `off`, if they are inside, kept by the same.
    pub fn sub(&self, off: u64, len: u64) -> Option<HostSpan> {
        let end = off.checked_add(len)?;
        (end <= self.len as u64).then(|| HostSpan {
            base: self.base + off,
            len: len as usize,
            keep: self.keep.clone(),
        })
    }

    /// This span and `next` as one, when `next` starts where this ends and
    /// both are kept by the same thing.
    pub fn join(&self, next: &HostSpan) -> Option<HostSpan> {
        (Arc::ptr_eq(&self.keep, &next.keep) && self.base + self.len as u64 == next.base).then(
            || HostSpan {
                base: self.base,
                len: self.len + next.len,
                keep: self.keep.clone(),
            },
        )
    }

    /// The regions of guest RAM the vhost-user memory table gave, as
    /// `(guest-physical start, span, backing file and offset)`: vm-memory
    /// keeps each region mapped for as long as `mem` lives, and every span
    /// holds `mem`.
    #[cfg(feature = "vhost-user")]
    pub fn of_vm_memory(mem: Arc<vm_memory::GuestMemoryMmap>) -> Vec<VmRegion> {
        use vm_memory::{Address, GuestMemoryBackend, GuestMemoryRegion};
        let keep: Arc<dyn Any + Send + Sync> = mem.clone();
        mem.iter()
            .map(|r| {
                (
                    r.start_addr().raw_value(),
                    HostSpan {
                        base: r.as_ptr() as u64,
                        len: r.len() as usize,
                        keep: keep.clone(),
                    },
                    r.file_offset().map(|f| (f.arc().clone(), f.start())),
                )
            })
            .collect()
    }

    /// `len` bytes from `at`, copied: for tests and fuzzing, which check
    /// what the host would see there.
    #[cfg(any(test, fuzzing))]
    pub fn read(&self, at: u64, len: usize) -> Vec<u8> {
        let end = at.checked_add(len as u64).expect("no wrap");
        assert!(end <= self.len as u64, "outside the span");
        let mut out = vec![0u8; len];
        // SAFETY: inside the span, which `keep` holds mapped readable; a
        // copy through raw pointers takes no reference to memory another
        // party may write.
        unsafe {
            std::ptr::copy_nonoverlapping((self.base + at) as *const u8, out.as_mut_ptr(), len)
        };
        out
    }
}

/// A mapping of the backend's own, unmapped when it goes.
pub struct Mapping {
    base: *mut u8,
    len: usize,
    writable: bool,
}

// SAFETY: the mapping's pages are not tied to a thread, and nothing but this
// value (by copies through raw pointers) touches them from Rust.
unsafe impl Send for Mapping {}
// SAFETY: as above; `read` and `write` copy, and take no reference.
unsafe impl Sync for Mapping {}

impl Mapping {
    fn map(len: usize, prot: i32, flags: i32, fd: RawFd, off: u64) -> io::Result<Arc<Self>> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let off =
            libc::off_t::try_from(off).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: a new mapping at an address the kernel chooses: it replaces
        // nothing, and nothing refers to it yet.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, prot, flags, fd, off) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            base: p.cast(),
            len,
            writable: prot & libc::PROT_WRITE != 0,
        }))
    }

    /// `len` bytes of `fd` from `off`, shared.
    pub fn shared(fd: impl AsFd, len: usize, off: u64, writable: bool) -> io::Result<Arc<Self>> {
        Self::map(
            len,
            prot(writable),
            libc::MAP_SHARED,
            fd.as_fd().as_raw_fd(),
            off,
        )
    }

    /// `len` bytes of zeroed private memory.
    pub fn anon(len: usize) -> io::Result<Arc<Self>> {
        Self::map(
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    }

    pub fn addr(&self) -> u64 {
        self.base as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Copy `bytes` in at `at`. Panics outside the mapping.
    pub fn write(&self, at: usize, bytes: &[u8]) {
        assert!(self.writable, "a read-only mapping");
        assert!(at.checked_add(bytes.len()).is_some_and(|e| e <= self.len));
        // SAFETY: inside the mapping (checked), which is writable (checked);
        // a raw copy takes no reference to memory another party may write
        // concurrently.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.base.add(at), bytes.len()) };
    }

    /// Copy `len` bytes out from `at`. Panics outside the mapping.
    pub fn read(&self, at: usize, len: usize) -> Vec<u8> {
        assert!(at.checked_add(len).is_some_and(|e| e <= self.len));
        let mut out = vec![0u8; len];
        // SAFETY: inside the mapping (checked), readable; a raw copy.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(at), out.as_mut_ptr(), len) };
        out
    }

    /// The whole mapping as a span the host may be handed.
    pub fn span(self: &Arc<Self>) -> HostSpan {
        HostSpan {
            base: self.base as u64,
            len: self.len,
            keep: self.clone(),
        }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `map`, unmapped once; every reference
        // to it (spans hold the Arc) is gone.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

/// An inaccessible range of address space, which pieces of files are placed
/// into, unmapped when it goes.
pub struct Reservation {
    base: *mut u8,
    len: usize,
}

// SAFETY: as for `Mapping`: the range belongs to no thread, and Rust never
// holds a reference into it.
unsafe impl Send for Reservation {}
// SAFETY: `map_file` replaces pages only inside the range, which no Rust
// reference covers.
unsafe impl Sync for Reservation {}

impl Reservation {
    /// `len` bytes, PROT_NONE, reserving no memory.
    pub fn new(len: usize) -> io::Result<Self> {
        if len == 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: a new anonymous mapping at an address the kernel chooses.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_NONE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            base: p.cast(),
            len,
        })
    }

    pub fn addr(&self) -> u64 {
        self.base as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Place `len` bytes of `fd` from `off` at `at` into the range, shared.
    /// Refused unless `[at, at + len)` is page-aligned and inside it.
    pub fn map_file(
        &self,
        at: usize,
        len: usize,
        fd: impl AsFd,
        off: u64,
        writable: bool,
    ) -> io::Result<()> {
        fixed_inside(self.len, at, len)?;
        let off =
            libc::off_t::try_from(off).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: MAP_FIXED over `[at, at + len)` of this reservation
        // (checked above), which this value owns and no Rust reference
        // covers: nothing else of the process's is replaced.
        let p = unsafe {
            libc::mmap(
                self.base.add(at).cast(),
                len,
                prot(writable),
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd.as_fd().as_raw_fd(),
                off,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The whole range as a span the host may be handed.
    pub fn span(self: &Arc<Self>) -> HostSpan {
        HostSpan {
            base: self.base as u64,
            len: self.len,
            keep: self.clone(),
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // SAFETY: the range made in `new`, unmapped once, after every span
        // holding it is gone.
        if unsafe { libc::munmap(self.base.cast(), self.len) } != 0 {
            log::warn!(
                "unmapping {:#x}+{:#x}: {}",
                self.base as u64,
                self.len,
                io::Error::last_os_error()
            );
        }
    }
}

/// The shared window's local backing: a memfd, mapped whole, over which
/// device memory is placed and from which it is withdrawn.
pub struct Window {
    memfd: crate::privfd::PrivateFd,
    base: *mut u8,
    len: usize,
}

// SAFETY: the mapping belongs to no thread; Rust holds no reference into it
// (the guest's view of it is the VMM's mapping, and tests read by copies).
unsafe impl Send for Window {}
// SAFETY: `place` and `withdraw` replace pages only inside the window.
unsafe impl Sync for Window {}

impl Window {
    /// A window of `len` bytes backed by a new memfd.
    pub fn new(len: usize) -> io::Result<Self> {
        if len == 0 || !len.is_multiple_of(PAGE) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        #[cfg(not(miri))]
        let memfd = super::fd::memfd(c"virtio-gpu-nv-shm", libc::MFD_CLOEXEC)?;
        // Miri has no memfd_create and no file-backed mappings: an unlinked
        // temporary file stands in for the memfd, and the mapping is
        // anonymous. Everything the allocator does is the same.
        #[cfg(miri)]
        let memfd = miri_memfd()?;
        super::fd::ftruncate(&memfd, len as u64)?;
        #[cfg(not(miri))]
        let (flags, backing) = (libc::MAP_SHARED, memfd.as_raw_fd());
        #[cfg(miri)]
        let (flags, backing) = (libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1);
        // SAFETY: a new mapping at an address the kernel chooses.
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                backing,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            memfd: crate::privfd::PrivateFd::new(memfd),
            base: p.cast(),
            len,
        })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The memfd behind the window, by number.
    pub fn memfd_raw(&self) -> RawFd {
        self.memfd.as_raw_fd()
    }

    /// Where the window starts, as a number.
    pub fn addr(&self) -> u64 {
        self.base as u64
    }

    fn fixed(&self, at: u64, len: u64, prot: i32, fd: RawFd, off: u64) -> io::Result<()> {
        let (Ok(at), Ok(n)) = (usize::try_from(at), usize::try_from(len)) else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        };
        fixed_inside(self.len, at, n)?;
        let off =
            libc::off_t::try_from(off).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: MAP_FIXED over `[at, at + n)` of the window (checked),
        // which this value owns and no Rust reference covers.
        let p = unsafe {
            libc::mmap(
                self.base.add(at).cast(),
                n,
                prot,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                off,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Place `len` bytes of `fd` from `off` at `at` in the window.
    pub fn place(&self, at: u64, len: u64, fd: RawFd, off: u64, writable: bool) -> io::Result<()> {
        self.fixed(at, len, prot(writable), fd, off)
    }

    /// Put the window's own memfd back under `[at, at + len)`: atomically,
    /// with no moment of no mapping there.
    pub fn restore(&self, at: u64, len: u64) -> io::Result<()> {
        self.fixed(
            at,
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            self.memfd.as_raw_fd(),
            at,
        )
    }

    /// The 32-bit word at `at`, read once (a device register may be there).
    pub fn read_u32(&self, at: u64) -> u32 {
        let at = usize::try_from(at).expect("an offset");
        assert!(at % 4 == 0 && at + 4 <= self.len, "inside the window");
        // SAFETY: inside the window (checked), aligned, mapped readable.
        unsafe { std::ptr::read_volatile(self.base.add(at).cast::<u32>()) }
    }

    /// `len` bytes from `at`, copied.
    pub fn read(&self, at: u64, len: usize) -> Vec<u8> {
        let at = usize::try_from(at).expect("an offset");
        assert!(at.checked_add(len).is_some_and(|e| e <= self.len), "inside");
        let mut out = vec![0u8; len];
        // SAFETY: inside the window (checked), mapped readable; a raw copy.
        unsafe { std::ptr::copy_nonoverlapping(self.base.add(at), out.as_mut_ptr(), len) };
        out
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: the mapping made in `new`, unmapped once.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

/// Under Miri: an unlinked temporary file.
#[cfg(miri)]
fn miri_memfd() -> io::Result<OwnedFd> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "nvgpu-shm-miri-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    let _ = std::fs::remove_file(&path);
    Ok(OwnedFd::from(f))
}

/// Whether the host lets `len` bytes of `fd` at `off` be mapped writable
/// (shm.rs, `host_mapping_writable`): map it read-only and ask for write;
/// `mprotect` answers EACCES exactly when VM_MAYWRITE is gone. `None` when
/// the mapping itself fails.
pub fn probe_writable(fd: RawFd, len: usize, off: u64) -> Option<bool> {
    let off = libc::off_t::try_from(off).ok()?;
    // SAFETY: a new mapping at an address the kernel chooses; nothing reads
    // or writes through it, and it is unmapped below.
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            off,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: `p` is the mapping made above, `len` bytes long.
    let rc = unsafe { libc::mprotect(p, len, libc::PROT_READ | libc::PROT_WRITE) };
    let err = io::Error::last_os_error().raw_os_error();
    // SAFETY: as above; nothing else refers to it.
    unsafe { libc::munmap(p, len) };
    Some(!(rc != 0 && err == Some(libc::EACCES)))
}

/// Whether any page of `[addr, addr + len)` is mapped in this process.
pub fn any_mapped(addr: u64, len: u64) -> bool {
    let page = PAGE as u64;
    let base = addr & !(page - 1);
    let n = (addr + len - base).div_ceil(page) as usize;
    let mut v = [0u8; 1];
    (0..n).any(|i| {
        // SAFETY: mincore only asks the kernel about the page and writes
        // one byte into `v`; an unmapped page is ENOMEM, never a fault.
        unsafe {
            libc::mincore(
                (base + i as u64 * page) as *mut libc::c_void,
                PAGE,
                v.as_mut_ptr(),
            ) == 0
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no file-backed mappings")]
    fn a_reservation_takes_pieces_of_a_file_and_nothing_outside_it() {
        let f = super::super::fd::memfd(c"t", libc::MFD_CLOEXEC).unwrap();
        super::super::fd::ftruncate(&f, 2 * PAGE as u64).unwrap();
        let m = Mapping::shared(&f, 2 * PAGE, 0, true).unwrap();
        m.write(PAGE, b"second");
        let r = Arc::new(Reservation::new(2 * PAGE).unwrap());
        r.map_file(0, PAGE, &f, PAGE as u64, false).unwrap();
        assert_eq!(r.span().read(0, 6), b"second");
        assert!(r.map_file(PAGE, 2 * PAGE, &f, 0, false).is_err(), "past its end");
        assert!(r.map_file(1, PAGE, &f, 0, false).is_err(), "unaligned");
        let base = r.addr();
        drop(r);
        let _ = base;
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri has no memfd_create")]
    fn a_window_places_and_restores_inside_itself_only() {
        let w = Window::new(4 * PAGE).unwrap();
        let f = super::super::fd::memfd(c"dev", libc::MFD_CLOEXEC).unwrap();
        super::super::fd::ftruncate(&f, PAGE as u64).unwrap();
        Mapping::shared(&f, PAGE, 0, true)
            .unwrap()
            .write(0, &0x42u32.to_le_bytes());
        w.place(PAGE as u64, PAGE as u64, f.as_raw_fd(), 0, true)
            .unwrap();
        assert_eq!(w.read_u32(PAGE as u64), 0x42);
        w.restore(PAGE as u64, PAGE as u64).unwrap();
        assert_eq!(w.read_u32(PAGE as u64), 0);
        assert!(w.place(3 * PAGE as u64, 2 * PAGE as u64, f.as_raw_fd(), 0, true).is_err());
    }
}
