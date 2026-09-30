// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use abi::ioctl::*;
use std::cell::{Cell, RefCell};

const CLIENT: u32 = 0xc1d0_0001;
const DEVICE: u32 = 0x5c00_0001;
/// Handles the fake RM answers for: system memory (a DIRECT mapping),
/// registers (MAPPING left as sent) and video memory (REFLECTED, WC).
const SYSMEM: u32 = 0x100;
const REGS: u32 = 0x200;
const VIDMEM: u32 = 0x300;
/// Video memory RM maps uncached (REFLECTED, UNCACHED): not the
/// write-combined type the backend expects of video memory before RM
/// answers.
const UCVID: u32 = 0x400;
const LEN: u64 = 4096;
/// NV_ERR_OBJECT_NOT_FOUND: RM's answer for an address it has no
/// mapping of the object at.
const NOT_FOUND: u32 = 0x57;

std::thread_local! {
    /// NV01_MEMORY_SYSTEM attr words the host was handed.
    static HOST_ATTR: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static NEXT_VA: Cell<u64> = const { Cell::new(0x7f00_0000_0000) };
    /// The mappings RM holds, as (hClient, hMemory, pLinearAddress).
    static HOST_MAPS: RefCell<Vec<(u32, u32, u64)>> = const { RefCell::new(Vec::new()) };
    /// Whether the fake VMM refuses placements.
    static PLACE_FAILS: Cell<bool> = const { Cell::new(false) };
}

fn host_maps() -> Vec<(u32, u32, u64)> {
    HOST_MAPS.with(|m| m.borrow().clone())
}

fn rd(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

fn put(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// RM, for RM_ALLOC, RM_MAP_MEMORY and RM_UNMAP_MEMORY. MAP_MEMORY comes
/// back as escape.c and mapping_cpu.c leave it: caching type DEFAULT, and
/// MAPPING DIRECT for system memory, REFLECTED with WRITECOMBINED for video
/// memory, untouched for registers.
fn fake_rm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    let len = hostfd::ioc_size(request as u32);
    let (b, others) = arg.split();
    let b = &mut b[..len];
    match (request & 0xff) as u32 {
        NV_ESC_RM_ALLOC => {
            let params = u64::from_le_bytes(b[16..24].try_into().unwrap());
            if rd(b, 12) == 0x3e && params != 0 {
                // The backend pointed pAllocParms at the class
                // parameters it built, NV_MEMORY_ALLOCATION_PARAMS.
                let attr = others.peek(params + 24, 4) as u32;
                HOST_ATTR.with(|a| a.borrow_mut().push(attr));
            }
            put(b, 40, 0);
        }
        NV_ESC_RM_MAP_MEMORY => {
            let mut flags = rd(b, 44) & !(3 << 15) & !(7 << 23);
            flags |= 6 << 23;
            match rd(b, 8) {
                SYSMEM => flags |= 1 << 15,
                VIDMEM => flags = (flags & !(7 << 23)) | (2 << 15) | (2 << 23),
                UCVID => flags = (flags & !(7 << 23)) | (2 << 15) | (1 << 23),
                _ => {}
            }
            put(b, 44, flags);
            let va = NEXT_VA.with(|v| {
                let va = v.get();
                v.set(va + 0x10000);
                va
            });
            b[32..40].copy_from_slice(&va.to_le_bytes());
            HOST_MAPS.with(|m| m.borrow_mut().push((rd(b, 0), rd(b, 8), va)));
            put(b, 40, 0);
        }
        // Both find the mapping by the object and the address, as RM
        // does (refFindCpuMappingWithFilter), and say so when there is
        // none.
        NV_ESC_RM_UNMAP_MEMORY => {
            let key = (
                rd(b, 0),
                rd(b, 8),
                u64::from_le_bytes(b[16..24].try_into().unwrap()),
            );
            let gone = HOST_MAPS.with(|m| {
                let mut m = m.borrow_mut();
                let at = m.iter().position(|&e| e == key);
                at.map(|i| m.remove(i)).is_some()
            });
            put(b, 24, if gone { 0 } else { NOT_FOUND });
        }
        NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => {
            let old = u64::from_le_bytes(b[16..24].try_into().unwrap());
            let new = u64::from_le_bytes(b[24..32].try_into().unwrap());
            let (client, mem) = (rd(b, 0), rd(b, 8));
            let moved = HOST_MAPS.with(|m| {
                let mut m = m.borrow_mut();
                let e = m.iter_mut().find(|e| **e == (client, mem, old));
                e.map(|e| e.2 = new).is_some()
            });
            put(b, 32, if moved { 0 } else { NOT_FOUND });
        }
        _ => {}
    }
    0
}

/// One placement: what, where, and whether it was asked to be writable.
type Placement = (&'static str, u64, bool);

/// Records every placement.
#[derive(Clone, Default)]
struct RecWindow(Arc<std::sync::Mutex<Vec<Placement>>>);

impl crate::shm::WindowPlacer for RecWindow {
    fn place(&self, off: u64, _len: u64, _fd: RawFd, _fo: u64, w: bool) -> Result<()> {
        if PLACE_FAILS.with(Cell::get) {
            return Err(std::io::Error::from_raw_os_error(libc::ENOMEM).into());
        }
        self.0.lock().unwrap().push(("place", off, w));
        Ok(())
    }
    fn withdraw(&self, off: u64, _len: u64) -> Result<()> {
        self.0.lock().unwrap().push(("withdraw", off, false));
        Ok(())
    }
}

impl RecWindow {
    fn withdrawn(&self) -> Vec<u64> {
        let log = self.0.lock().unwrap();
        log.iter()
            .filter(|e| e.0 == "withdraw")
            .map(|e| e.1)
            .collect()
    }
    fn last_place(&self) -> (u64, bool) {
        let log = self.0.lock().unwrap();
        let e = log
            .iter()
            .rev()
            .find(|e| e.0 == "place")
            .expect("a placement");
        (e.1, e.2)
    }
}

/// A memfd standing for a device file: mappable, so the writability
/// probe sees a real answer.
fn memfd() -> OwnedFd {
    let fd = crate::sys::fd::memfd(c"devfile", libc::MFD_CLOEXEC).unwrap();
    crate::sys::fd::ftruncate(&fd, 1 << 16).unwrap();
    fd
}

/// The same file, opened read-only: mapping it writable is refused the
/// way nvidia.ko refuses a context without NV_PROTECT_WRITEABLE.
fn read_only(f: &OwnedFd) -> OwnedFd {
    crate::sys::fd::open_path(
        &format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(f)),
        libc::O_RDONLY | libc::O_CLOEXEC,
    )
    .unwrap()
}

struct Env {
    be: NvidiaBackend,
    window: RecWindow,
    ctl: u32,
}

fn env() -> Env {
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(fake_rm);
    be.session.v2 = true;
    let window = RecWindow::default();
    be.set_window(Box::new(window.clone()));
    let ctl = be.adopt_for_test(memfd(), HandleKind::Dev(DeviceKind::Ctl));
    Env { be, window, ctl }
}

fn msg(msg_type: MsgType, handle: u32) -> Vec<u8> {
    let mut v = vec![0u8; size_of::<MsgHeader>()];
    write_struct(
        &mut v,
        &MsgHeader {
            msg_type: msg_type as u32,
            handle,
            status: 0,
            req_id: 0,
        },
    );
    v
}

fn push<T: crate::sys::pod::Pod>(v: &mut Vec<u8>, val: &T) {
    let at = v.len();
    v.resize(at + size_of::<T>(), 0);
    write_struct(&mut v[at..], val);
}

impl Env {
    fn call(&mut self, handle: u32, escape: u32, outer: &[u8], nested: &[u8]) -> Vec<u8> {
        let (status, back) = self.call_raw(handle, escape, outer, nested);
        assert_eq!(status, 0, "escape {escape:#x}");
        back
    }

    /// The transport status and the parameters as the guest reads them.
    fn call_raw(
        &mut self,
        handle: u32,
        escape: u32,
        outer: &[u8],
        nested: &[u8],
    ) -> (i32, Vec<u8>) {
        let mut req = msg(MsgType::Ioctl, handle);
        push(
            &mut req,
            &IoctlReq {
                cmd: _IOWR(escape, outer.len() as u32) as u32,
                data_len: outer.len() as u32,
                nested_offset: outer.len() as u32,
                nested_len: nested.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        let mut resp = vec![0u8; 4096];
        let n = self.be.dispatch(&req, &mut resp);
        let status = read_struct::<MsgHeader>(&resp, 0).status;
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        (status, resp[body.min(n)..n].to_vec())
    }

    /// RM_ALLOC of `class` as `handle`, with `attr` if it is memory.
    /// Returns the class parameters as the guest reads them back.
    fn alloc(&mut self, class: u32, handle: u32, attr: Option<u32>) -> Vec<u8> {
        let mut outer = vec![0u8; 48];
        put(&mut outer, 0, CLIENT);
        put(&mut outer, 4, DEVICE);
        put(&mut outer, 8, handle);
        put(&mut outer, 12, class);
        let nested = attr.map(|a| {
            let mut p = vec![0u8; 128];
            put(&mut p, 24, a);
            p
        });
        let ctl = self.ctl;
        let back = self.call(
            ctl,
            NV_ESC_RM_ALLOC,
            &outer,
            nested.as_deref().unwrap_or(&[]),
        );
        back[48..].to_vec()
    }

    /// RM_MAP_MEMORY of `mem` armed on `file`; returns (its handle, the
    /// window offset the guest reads back as pLinearAddress).
    fn map_on(&mut self, mem: u32, file: OwnedFd) -> (u32, u64) {
        let fd = self
            .be
            .adopt_for_test(file, HandleKind::Dev(DeviceKind::Ctl));
        let mut p = vec![0u8; 56];
        put(&mut p, 0, CLIENT);
        put(&mut p, 4, DEVICE);
        put(&mut p, 8, mem);
        p[24..32].copy_from_slice(&LEN.to_le_bytes());
        put(&mut p, 44, 0x0308_0002);
        put(&mut p, 48, fd);
        let ctl = self.ctl;
        let back = self.call(ctl, NV_ESC_RM_MAP_MEMORY, &p, &[]);
        assert_eq!(rd(&back, 40), 0, "RM status");
        (fd, u64::from_le_bytes(back[32..40].try_into().unwrap()))
    }

    fn map(&mut self, mem: u32) -> (u32, u64) {
        self.map_on(mem, memfd())
    }

    fn unmap(&mut self, mem: u32, linear: u64) {
        let mut p = vec![0u8; 32];
        put(&mut p, 0, CLIENT);
        put(&mut p, 4, DEVICE);
        put(&mut p, 8, mem);
        p[16..24].copy_from_slice(&linear.to_le_bytes());
        let ctl = self.ctl;
        let back = self.call(ctl, NV_ESC_RM_UNMAP_MEMORY, &p, &[]);
        assert_eq!(rd(&back, 24), 0);
    }

    fn mmap(&mut self, fd: u32) -> MmapResp {
        let mut req = msg(MsgType::Mmap, fd);
        push(
            &mut req,
            &MmapReq {
                size: LEN,
                offset: 0,
                prot: 3,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        self.be.dispatch(&req, &mut resp);
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, 0, "mmap");
        read_struct::<MmapResp>(&resp, size_of::<MsgHeader>())
    }

    fn munmap(&mut self, id: u32) {
        let mut req = msg(MsgType::Munmap, 0);
        push(
            &mut req,
            &MunmapReq {
                mapping_id: id,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        self.be.dispatch(&req, &mut resp);
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, 0, "munmap");
    }
}

#[test]
fn uncached_system_memory_is_allocated_coherent_and_mapped_write_back() {
    let mut e = env();
    HOST_ATTR.with(|a| a.borrow_mut().clear());
    // UNCACHED, LOCATION_PCI: the RM default for system memory.
    let asked = 1 << 25;
    let back = e.alloc(0x3e, SYSMEM, Some(asked));
    let host = HOST_ATTR.with(|a| a.borrow().clone());
    assert_eq!(
        host,
        vec![asked | (5 << 29)],
        "the host allocates WRITE_BACK"
    );
    assert_eq!(
        rd(&back, 24),
        asked,
        "the guest reads back what it asked for"
    );

    let (fd, linear) = e.map(SYSMEM);
    let m = e.mmap(fd);
    assert_eq!(m.caching, MMAP_CACHE_WB);
    assert_eq!(m.flags, 0);
    assert_eq!(m.guest_phys_addr, linear);
    assert!(
        linear >= 4096 * 6,
        "placed in the write-back zone, at {linear:#x}"
    );
}

#[test]
fn the_doorbell_is_mapped_uncached_and_video_memory_write_combined() {
    let mut e = env();
    e.alloc(0xc461, REGS, None);
    let (fd, linear) = e.map(REGS);
    assert_eq!(e.mmap(fd).caching, MMAP_CACHE_UC);
    assert!(
        linear < 4096 * 2,
        "placed in the uncached zone, at {linear:#x}"
    );

    let (fd, linear) = e.map(VIDMEM);
    assert_eq!(e.mmap(fd).caching, MMAP_CACHE_WC);
    assert!(
        (4096 * 2..4096 * 6).contains(&linear),
        "write-combining zone, at {linear:#x}"
    );
}

#[test]
fn with_guest_coherency_kept_system_memory_maps_as_it_was_allocated() {
    let mut e = env();
    e.be.set_guest_coherency(false);
    HOST_ATTR.with(|a| a.borrow_mut().clear());
    let asked = (2 << 29) | (1 << 25); // WRITE_COMBINE, PCI
    e.alloc(0x3e, SYSMEM, Some(asked));
    assert_eq!(HOST_ATTR.with(|a| a.borrow().clone()), vec![asked]);
    let (fd, _) = e.map(SYSMEM);
    assert_eq!(e.mmap(fd).caching, MMAP_CACHE_WC);
}

#[test]
fn a_write_back_mapping_that_does_not_fit_its_zone_is_placed_write_combining() {
    let mut e = env();
    e.alloc(0x3e, SYSMEM, Some(1 << 25));
    // for_test's write-back zone holds two pages.
    let (a, _) = e.map(SYSMEM);
    let (b, _) = e.map(SYSMEM);
    let (c, linear) = e.map(SYSMEM);
    assert_eq!(e.mmap(a).caching, MMAP_CACHE_WB);
    assert_eq!(e.mmap(b).caching, MMAP_CACHE_WB);
    assert_eq!(e.mmap(c).caching, MMAP_CACHE_WC);
    assert!((4096 * 2..4096 * 6).contains(&linear));
}

#[test]
fn a_v1_guest_is_told_nothing_it_cannot_read() {
    let mut e = env();
    e.be.session.v2 = false;
    e.alloc(0xc461, REGS, None);
    let src = memfd();
    let (fd, _) = e.map_on(REGS, read_only(&src));
    let m = e.mmap(fd);
    assert_eq!((m.caching, m.flags, m.reserved), (MMAP_CACHE_DEFAULT, 0, 0));
}

#[test]
fn a_read_only_host_mapping_is_placed_read_only_and_the_guest_is_told() {
    let mut e = env();
    let src = memfd();
    let (fd, linear) = e.map_on(VIDMEM, read_only(&src));
    assert_eq!(e.window.last_place(), (linear, false));
    assert_eq!(e.mmap(fd).flags, MMAP_F_READ_ONLY);

    let (fd, linear) = e.map(VIDMEM);
    assert_eq!(e.window.last_place(), (linear, true));
    assert_eq!(e.mmap(fd).flags, 0);
}

/// M-3: MAP, MMAP, UNMAP, MAP. The first extent is still in a guest
/// process's page tables after the unmap, so the second mapping must not
/// be given it; the last MUNMAP of the first gives it back.
#[test]
fn an_extent_under_a_live_guest_mapping_is_not_reused_after_rm_unmap() {
    let mut e = env();
    let empty = e.be.shm_free_bytes();
    let (fd, first) = e.map(VIDMEM);
    let m = e.mmap(fd);
    assert_ne!(m.mapping_id, 0, "RM mappings get an id the guest counts");
    let again = e.mmap(fd);
    assert_eq!(again.mapping_id, m.mapping_id, "one id per mapping");

    e.unmap(VIDMEM, first);
    assert!(
        e.window.withdrawn().is_empty(),
        "withdrawn under a live vma"
    );
    let (_, second) = e.map(VIDMEM);
    assert_ne!(
        second, first,
        "the extent went to another mapping while mapped"
    );

    e.munmap(m.mapping_id);
    assert!(e.window.withdrawn().is_empty(), "one vma still maps it");
    e.munmap(again.mapping_id);
    assert_eq!(
        e.window.withdrawn(),
        vec![first],
        "released by the last MUNMAP"
    );

    e.unmap(VIDMEM, second);
    assert_eq!(e.be.shm_free_bytes(), empty, "every extent given back once");
    e.munmap(m.mapping_id); // a late duplicate is harmless
    assert_eq!(e.be.shm_free_bytes(), empty);
}

/// An RM mapping mmapped u32::MAX times is refused one more MMAP, not
/// counted past it (placement.rs `handle_mmap`).
#[test]
fn an_rm_mapping_mapped_u32_max_times_is_refused_another() {
    let mut e = env();
    let (fd, _) = e.map(VIDMEM);
    e.mmap(fd);
    e.be.active_maps.find_by_fd_handle_mut(fd).unwrap().refs = u32::MAX;
    let mut req = msg(MsgType::Mmap, fd);
    push(
        &mut req,
        &MmapReq {
            size: LEN,
            offset: 0,
            prot: 3,
            padding: 0,
        },
    );
    let mut resp = vec![0u8; 64];
    e.be.dispatch(&req, &mut resp);
    assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, -libc::ENOMEM);
    assert_eq!(e.be.active_maps.find_by_fd_handle(fd).unwrap().refs, u32::MAX);
}

#[test]
fn an_rm_mapping_nobody_still_maps_is_released_at_rm_unmap() {
    let mut e = env();
    let empty = e.be.shm_free_bytes();
    let (fd, linear) = e.map(VIDMEM);
    let m = e.mmap(fd);
    e.munmap(m.mapping_id);
    assert!(e.window.withdrawn().is_empty(), "RM still has it mapped");
    e.unmap(VIDMEM, linear);
    assert_eq!(e.window.withdrawn(), vec![linear]);
    assert_eq!(e.be.shm_free_bytes(), empty);
}

#[test]
fn closing_the_file_of_a_mapped_rm_mapping_waits_for_its_munmap() {
    let mut e = env();
    let empty = e.be.shm_free_bytes();
    let (fd, linear) = e.map(VIDMEM);
    let m = e.mmap(fd);
    e.be.close_handle(fd).unwrap();
    assert!(e.window.withdrawn().is_empty());
    let (_, other) = e.map(VIDMEM);
    assert_ne!(other, linear);
    e.munmap(m.mapping_id);
    assert_eq!(e.window.withdrawn(), vec![linear]);
    e.unmap(VIDMEM, other);
    assert_eq!(e.be.shm_free_bytes(), empty);
}

#[test]
fn a_session_reset_releases_rm_extents_still_under_guest_mappings() {
    let mut e = env();
    let empty = e.be.shm_free_bytes();
    let (fd, linear) = e.map(VIDMEM);
    e.mmap(fd);
    e.unmap(VIDMEM, linear);
    e.be.session_reset("test");
    assert_eq!(e.window.withdrawn(), vec![linear]);
    assert_eq!(e.be.shm_free_bytes(), empty);
}

// ------------------------------------------------------------------
// UPDATE_DEVICE_MAPPING_INFO, and a map that cannot finish
// ------------------------------------------------------------------

fn p(tgid: u32) -> crate::quota::Owner {
    crate::quota::Owner::Proc { tgid, start_ns: 1 }
}

/// A guest process: its own control file, opened by it.
fn process(e: &mut Env, owner: crate::quota::Owner) -> u32 {
    e.be.adopt_for_test_as(memfd(), HandleKind::Dev(DeviceKind::Ctl), owner)
}

impl Env {
    /// RM_MAP_MEMORY of `mem` on control file `ctl`, armed on a file
    /// `owner` opened: the transport status, RM's status and the window
    /// offset the guest reads back.
    fn map_as(&mut self, ctl: u32, owner: crate::quota::Owner, mem: u32) -> (i32, u32, u64) {
        let fd = self
            .be
            .adopt_for_test_as(memfd(), HandleKind::Dev(DeviceKind::Gpu(0)), owner);
        let mut p = vec![0u8; 56];
        put(&mut p, 0, CLIENT);
        put(&mut p, 4, DEVICE);
        put(&mut p, 8, mem);
        p[24..32].copy_from_slice(&LEN.to_le_bytes());
        put(&mut p, 44, 0x0308_0002);
        put(&mut p, 48, fd);
        let (status, back) = self.call_raw(ctl, NV_ESC_RM_MAP_MEMORY, &p, &[]);
        if status != 0 {
            return (status, 0, 0);
        }
        (
            0,
            rd(&back, 40),
            u64::from_le_bytes(back[32..40].try_into().unwrap()),
        )
    }

    /// UPDATE_DEVICE_MAPPING_INFO on `ctl`: RM's status, and the pOld
    /// and pNew the caller reads back.
    fn update_on(&mut self, ctl: u32, mem: u32, old: u64, new: u64) -> (u32, u64, u64) {
        let mut p = vec![0u8; 40];
        put(&mut p, 0, CLIENT);
        put(&mut p, 4, DEVICE);
        put(&mut p, 8, mem);
        p[16..24].copy_from_slice(&old.to_le_bytes());
        p[24..32].copy_from_slice(&new.to_le_bytes());
        let back = self.call(ctl, NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO, &p, &[]);
        let q = |at: usize| u64::from_le_bytes(back[at..at + 8].try_into().unwrap());
        (rd(&back, 32), q(16), q(24))
    }

    /// UNMAP_MEMORY on `ctl` by `linear`: RM's status.
    fn unmap_on(&mut self, ctl: u32, mem: u32, linear: u64) -> u32 {
        let mut p = vec![0u8; 32];
        put(&mut p, 0, CLIENT);
        put(&mut p, 4, DEVICE);
        put(&mut p, 8, mem);
        p[16..24].copy_from_slice(&linear.to_le_bytes());
        let back = self.call(ctl, NV_ESC_RM_UNMAP_MEMORY, &p, &[]);
        rd(&back, 24)
    }
}

/// The Prism stall's leak: the library maps, tells RM the address it
/// mapped at, and unmaps by that address. The unmap used to look for a
/// window offset, find none, and leave the extent charged until the
/// memory was freed; now the zone ends exactly where it started, and so
/// does the host.
/// UNMAP_MEMORY gives the caller its own pLinearAddress back, found or
/// not: RM does not write it and nvidia.ko copies the block back.
#[test]
fn an_unmap_gives_the_caller_its_own_address_back() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    let (_, _, off) = e.map_as(ctl, p(1), VIDMEM);
    let unmap = |e: &mut Env, linear: u64| {
        let mut q = vec![0u8; 32];
        put(&mut q, 0, CLIENT);
        put(&mut q, 4, DEVICE);
        put(&mut q, 8, VIDMEM);
        q[16..24].copy_from_slice(&linear.to_le_bytes());
        let back = e.call(ctl, NV_ESC_RM_UNMAP_MEMORY, &q, &[]);
        (
            rd(&back, 24),
            u64::from_le_bytes(back[16..24].try_into().unwrap()),
        )
    };
    assert_eq!(unmap(&mut e, off), (0, off), "found");
    assert!(host_maps().is_empty());
    let (_, again) = unmap(&mut e, off);
    assert_eq!(again, off, "not found");
}

#[test]
fn an_unmap_by_the_address_an_update_gave_returns_the_zone_exactly() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    let empty = e.be.shm_free_bytes();
    let held = e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1));

    let (_, rm, off) = e.map_as(ctl, p(1), VIDMEM);
    assert_eq!(rm, 0);
    let va = 0x7d5c_b840_0000;
    let (status, old, new) = e.update_on(ctl, VIDMEM, off, va);
    assert_eq!(status, 0, "RM found the mapping by the host's address");
    assert_eq!((old, new), (off, va), "the caller reads back its own");
    assert_eq!(host_maps().len(), 1, "the host's mapping did not move");
    assert_ne!(host_maps()[0].2, va, "no guest address reaches the host");

    assert_eq!(e.unmap_on(ctl, VIDMEM, va), 0, "unmapped by its address");
    assert_eq!(e.be.shm_free_bytes(), empty, "the zone is where it was");
    assert_eq!(
        e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1)),
        held,
        "and so is the process's share"
    );
    assert_eq!(e.window.withdrawn(), vec![off]);
    assert!(host_maps().is_empty(), "and the host holds nothing");
    assert!(e.be.active_maps.is_empty());
}

/// Two mappings of one object are two entries, told apart by address:
/// each UPDATE moves the one it names (a second UPDATE by the address
/// the first gave), and each unmap releases its own.
#[test]
fn two_mappings_of_one_object_are_told_apart_by_address() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    let empty = e.be.shm_free_bytes();
    let (_, _, a) = e.map_as(ctl, p(1), VIDMEM);
    let (_, _, b) = e.map_as(ctl, p(1), VIDMEM);
    let (host_a, host_b) = (host_maps()[0].2, host_maps()[1].2);
    let (va, vb) = (0x7f00_aaaa_0000, 0x7f00_bbbb_0000);

    // The second first: by object alone, the first would have moved.
    assert_eq!(e.update_on(ctl, VIDMEM, b, vb).0, 0);
    assert_eq!(e.update_on(ctl, VIDMEM, a, va).0, 0);
    // The library moves one again (mremap): by the address it gave.
    let va2 = 0x7f00_cccc_0000;
    assert_eq!(e.update_on(ctl, VIDMEM, va, va2).0, 0);
    // An address that is neither, with two to choose from: no guess.
    assert_eq!(
        e.update_on(ctl, VIDMEM, 0x1234_5000, 0x7f00_dddd_0000).0,
        NOT_FOUND
    );

    assert_eq!(e.unmap_on(ctl, VIDMEM, vb), 0);
    assert_eq!(e.window.withdrawn(), vec![b], "b's own extent");
    assert_eq!(
        host_maps(),
        vec![(CLIENT, VIDMEM, host_a)],
        "b's own host mapping"
    );
    assert_ne!(host_a, host_b);
    assert_eq!(e.unmap_on(ctl, VIDMEM, va), NOT_FOUND, "moved on from va");
    assert_eq!(e.unmap_on(ctl, VIDMEM, va2), 0);
    assert_eq!(e.window.withdrawn(), vec![b, a]);
    assert_eq!(e.be.shm_free_bytes(), empty);
    assert!(host_maps().is_empty());
}

/// One process's virtual addresses are its own: another process that
/// quotes one -- the same object, the same address -- finds nothing,
/// neither to move nor to unmap, as RM finds only the calling process's
/// mappings (serverutilMappingFilterCurrentUserProc). The owner's own
/// calls are unaffected.
#[test]
fn another_processes_address_names_nothing() {
    let mut e = env();
    let mine = process(&mut e, p(1));
    let theirs = process(&mut e, p(2));
    let (_, _, off) = e.map_as(mine, p(1), VIDMEM);
    let va = 0x7f00_1000_0000;
    assert_eq!(e.update_on(mine, VIDMEM, off, va).0, 0);
    let before = e.be.shm_free_bytes();

    assert_eq!(
        e.update_on(theirs, VIDMEM, va, 0x7f00_2000_0000).0,
        NOT_FOUND
    );
    assert_eq!(
        e.update_on(theirs, VIDMEM, off, 0x7f00_2000_0000).0,
        NOT_FOUND
    );
    assert_eq!(e.unmap_on(theirs, VIDMEM, va), NOT_FOUND);
    assert_eq!(e.unmap_on(theirs, VIDMEM, off), NOT_FOUND);
    assert_eq!(
        e.be.shm_free_bytes(),
        before,
        "nothing of mine was released"
    );
    assert_eq!(host_maps().len(), 1);

    assert_eq!(e.unmap_on(mine, VIDMEM, va), 0);
    assert!(host_maps().is_empty());
}

/// A process at its share of the zone is refused before RM is asked:
/// no host mapping is made, and nothing more is charged. It used to be
/// refused after, and each refusal left a host mapping behind.
#[test]
fn a_refused_extent_leaves_no_host_mapping_and_no_charge() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    // for_test's write-combining zone is four pages: half is two.
    for _ in 0..2 {
        assert_eq!(e.map_as(ctl, p(1), VIDMEM).1, 0);
    }
    let (free, held) = (
        e.be.shm_free_bytes(),
        e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1)),
    );
    assert_eq!(held, 2 * LEN);

    // RM's own out-of-memory answer, in the status.
    assert_eq!(e.map_as(ctl, p(1), VIDMEM), (0, NV_ERR_NO_MEMORY, 0));
    assert_eq!(host_maps().len(), 2, "RM was never asked");
    assert_eq!(e.be.shm_free_bytes(), free);
    assert_eq!(
        e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1)),
        held
    );
    // Another process still maps.
    let other = process(&mut e, p(2));
    assert_eq!(e.map_as(other, p(2), VIDMEM).1, 0);
}

/// An NVOS33 length near u64::MAX, which any app may send, is refused
/// in RM's status before a reservation is tried: rounding it up to a
/// page would overflow, and the release profile abort the backend for
/// every process of the VM. The same for an MMAP's size.
#[test]
fn a_map_length_near_u64_max_is_refused_not_aborted() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    let empty = e.be.shm_free_bytes();
    for len in [
        u64::MAX,
        0xFFFF_FFFF_FFFF_F001,
        0xFFFF_FFFF_FFFF_F000,
        1 << 40,
    ] {
        let fd =
            e.be.adopt_for_test_as(memfd(), HandleKind::Dev(DeviceKind::Gpu(0)), p(1));
        let mut q = vec![0u8; 56];
        put(&mut q, 0, CLIENT);
        put(&mut q, 4, DEVICE);
        put(&mut q, 8, VIDMEM);
        q[24..32].copy_from_slice(&len.to_le_bytes());
        put(&mut q, 44, 0x0308_0002);
        put(&mut q, 48, fd);
        let (status, back) = e.call_raw(ctl, NV_ESC_RM_MAP_MEMORY, &q, &[]);
        assert_eq!(status, 0, "{len:#x}");
        assert_eq!(rd(&back, 40), NV_ERR_NO_MEMORY, "{len:#x}");
        assert_eq!(back[24..32], len.to_le_bytes(), "the caller's own length");
        assert_eq!(rd(&back, 48), fd, "the caller's own descriptor");
    }
    assert!(host_maps().is_empty(), "RM was never asked");
    assert_eq!(e.be.shm_free_bytes(), empty);

    // An MMAP of an unrecorded file, sized by the guest kernel.
    for size in [u64::MAX, 0xFFFF_FFFF_FFFF_F001] {
        let mut req = msg(MsgType::Mmap, e.ctl);
        push(
            &mut req,
            &MmapReq {
                size,
                offset: 0,
                prot: 3,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        e.be.dispatch(&req, &mut resp);
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, -libc::ENOMEM);
    }
    assert_eq!(e.be.shm_free_bytes(), empty);
}

/// RM maps with another type than expected: the reservation moves to
/// that type's zone, and where that zone is full, the host's mapping is
/// undone and nothing of the call is left.
#[test]
fn a_mapping_rm_types_otherwise_moves_zone_or_is_undone() {
    let mut e = env();
    let ctl = process(&mut e, crate::quota::Owner::Unknown);
    let unknown = crate::quota::Owner::Unknown;
    let (_, wc0, _) = e.be.shm_free_bytes();
    // for_test's uncached zone is two pages.
    for _ in 0..2 {
        let (_, rm, off) = e.map_as(ctl, unknown, UCVID);
        assert_eq!(rm, 0);
        assert!(
            off < 2 * LEN,
            "placed uncached, as RM mapped it, at {off:#x}"
        );
    }
    assert_eq!(e.be.shm_free_bytes().1, wc0, "no write-combining kept");
    let placed = e.window.0.lock().unwrap().len();

    assert_eq!(
        e.map_as(ctl, unknown, UCVID),
        (0, NV_ERR_NO_MEMORY, 0),
        "no uncached extent left: RM's own answer"
    );
    assert_eq!(host_maps().len(), 2, "the third host mapping was undone");
    assert_eq!(e.be.shm_free_bytes().1, wc0, "the reservation went back");
    assert_eq!(e.window.0.lock().unwrap().len(), placed, "nothing placed");
}

/// A placement the VMM refuses undoes the host's mapping and the
/// extent both.
#[test]
fn a_refused_placement_undoes_the_host_mapping_and_the_extent() {
    let mut e = env();
    let ctl = process(&mut e, p(1));
    let empty = e.be.shm_free_bytes();
    PLACE_FAILS.with(|f| f.set(true));
    let refused = e.map_as(ctl, p(1), VIDMEM);
    PLACE_FAILS.with(|f| f.set(false));
    assert_eq!(refused, (0, NV_ERR_NO_MEMORY, 0), "RM's own answer");
    assert!(host_maps().is_empty(), "the host's mapping was undone");
    assert_eq!(e.be.shm_free_bytes(), empty);
    assert_eq!(
        e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1)),
        0
    );
    assert!(e.be.active_maps.is_empty());
}
