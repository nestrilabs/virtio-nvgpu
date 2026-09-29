// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const X: u64 = 0x2_06e0_0000;
const L: u64 = 2 << 20;
/// UVM_ALLOC_SEMAPHORE_POOL's block on 595.99.02 (256 GPUs).
const POOL: usize = 9248;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;
const BODY: usize = size_of::<MsgHeader>() + size_of::<IoctlResp>();

std::thread_local! {
    static ALLOC_STATUS: Cell<u32> = const { Cell::new(0) };
    static FREE_STATUS: Cell<u32> = const { Cell::new(0) };
    static HOST: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
}

/// UVM as far as a pool's life goes: INITIALIZE and the pageable check
/// succeed, ALLOC and FREE answer what the test set.
fn fake_uvm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    HOST.with(|h| h.borrow_mut().push(request));
    let a = arg.bytes();
    let mut put = |off: usize, v: u32| {
        // Every block below is longer than `off + 4`.
        a[off..off + 4].copy_from_slice(&v.to_le_bytes());
    };
    match request {
        0x3000_0001 => put(8, 0),
        39 => {
            put(0, 0);
            put(4, 0);
        }
        68 => put(POOL - 8, ALLOC_STATUS.with(Cell::get)),
        34 => put(8, FREE_STATUS.with(Cell::get)),
        _ => {}
    }
    0
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    Place(u64),
    PlaceUvm {
        off: u64,
        len: u64,
        addr: u64,
    },
    /// `file_open`: the descriptor handed to the matching `PlaceUvm` is
    /// still the same open file when the withdraw comes.
    WithdrawUvm {
        off: u64,
        len: u64,
        file_open: bool,
    },
}

fn ino(fd: RawFd) -> Option<u64> {
    crate::sys::fd::fstat(fd).ok().map(|st| st.st_ino)
}

#[derive(Clone, Default)]
struct Vmm {
    calls: Arc<Mutex<Vec<Call>>>,
    fds: Arc<Mutex<HashMap<u64, (RawFd, u64)>>>,
    fail: Arc<AtomicBool>,
}

impl crate::shm::WindowPlacer for Vmm {
    fn place(&self, off: u64, _len: u64, _fd: RawFd, _fo: u64, _w: bool) -> Result<()> {
        self.calls.lock().unwrap().push(Call::Place(off));
        Ok(())
    }
    fn withdraw(&self, _off: u64, _len: u64) -> Result<()> {
        Ok(())
    }
    fn place_uvm(&self, off: u64, len: u64, fd: RawFd, addr: u64) -> Result<()> {
        if self.fail.load(Ordering::Relaxed) {
            return Err(std::io::Error::from_raw_os_error(libc::EEXIST).into());
        }
        self.fds.lock().unwrap().insert(off, (fd, ino(fd).unwrap()));
        self.calls
            .lock()
            .unwrap()
            .push(Call::PlaceUvm { off, len, addr });
        Ok(())
    }
    fn withdraw_uvm(&self, off: u64, len: u64) -> Result<()> {
        let file_open = self
            .fds
            .lock()
            .unwrap()
            .remove(&off)
            .is_some_and(|(fd, i)| ino(fd) == Some(i));
        self.calls.lock().unwrap().push(Call::WithdrawUvm {
            off,
            len,
            file_open,
        });
        Ok(())
    }
}

fn memfd() -> OwnedFd {
    crate::sys::fd::memfd(c"uvm", libc::MFD_CLOEXEC).unwrap()
}

fn msg<T: crate::sys::pod::Pod>(t: MsgType, handle: u32, body: &T) -> Vec<u8> {
    let mut v = vec![0u8; size_of::<MsgHeader>() + size_of::<T>()];
    let n = write_struct(
        &mut v,
        &MsgHeader {
            msg_type: t as u32,
            handle,
            status: 0,
            req_id: 0,
        },
    );
    write_struct(&mut v[n..], body);
    v
}

fn status(resp: &[u8]) -> i32 {
    read_struct::<MsgHeader>(resp, 0).status
}

struct Env {
    be: NvidiaBackend,
    vmm: Vmm,
}

/// A v2 session on 595.99.02 whose guest said it has a 1 GiB aperture,
/// or none.
fn env(aperture: bool) -> Env {
    ALLOC_STATUS.with(|s| s.set(0));
    FREE_STATUS.with(|s| s.set(0));
    HOST.with(|h| h.borrow_mut().clear());
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(fake_uvm);
    be.set_host_driver_version("595.99.02");
    be.config_mut().allow_compute = true;
    let vmm = Vmm::default();
    be.set_window(Box::new(vmm.clone()));
    let mut e = Env { be, vmm };
    let caps = e.hello(aperture);
    assert_eq!(caps & BCAP_UVM_MAP != 0, aperture);
    e
}

impl Env {
    fn hello(&mut self, aperture: bool) -> u32 {
        let req = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: if aperture { GCAP_UVM_APERTURE } else { 0 },
            uvm_aperture_mib: if aperture { 1024 } else { 0 },
        };
        let mut resp = vec![0u8; 256];
        self.be.dispatch(&msg(MsgType::Hello, 0, &req), &mut resp);
        assert_eq!(status(&resp), 0);
        read_struct::<HelloResp>(&resp, size_of::<MsgHeader>()).backend_caps
    }

    fn ioctl(&mut self, h: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
        let mut req = msg(
            MsgType::Ioctl,
            h,
            &IoctlReq {
                cmd,
                data_len: params.len() as u32,
                ..Default::default()
            },
        );
        req.extend_from_slice(params);
        let mut resp = vec![0u8; 256 + params.len()];
        let n = self.be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    /// A UVM file, initialised.
    fn uvm(&mut self) -> u32 {
        let h = self
            .be
            .adopt_for_test(memfd(), HandleKind::Dev(DeviceKind::Uvm));
        assert_eq!(status(&self.ioctl(h, 0x3000_0001, &[0u8; 16])), 0);
        h
    }

    fn alloc(&mut self, h: u32, base: u64, len: u64) {
        let mut p = vec![0u8; POOL];
        p[0..8].copy_from_slice(&base.to_le_bytes());
        p[8..16].copy_from_slice(&len.to_le_bytes());
        assert_eq!(status(&self.ioctl(h, 68, &p)), 0);
    }

    /// UVM_FREE; the rmStatus the host answered.
    fn free(&mut self, h: u32, base: u64) -> u32 {
        let mut p = [0u8; 16];
        p[0..8].copy_from_slice(&base.to_le_bytes());
        let r = self.ioctl(h, 34, &p);
        assert_eq!(status(&r), 0);
        u32::from_le_bytes(r[BODY + 8..BODY + 12].try_into().unwrap())
    }

    fn mmap(
        &mut self,
        h: u32,
        offset: u64,
        size: u64,
        prot: u32,
    ) -> std::result::Result<MmapResp, i32> {
        let req = MmapReq {
            size,
            offset,
            prot,
            padding: 0,
        };
        let mut resp = vec![0u8; 64];
        self.be.dispatch(&msg(MsgType::Mmap, h, &req), &mut resp);
        match status(&resp) {
            0 => Ok(read_struct::<MmapResp>(&resp, size_of::<MsgHeader>())),
            e => Err(-e),
        }
    }

    fn munmap(&mut self, h: u32, id: u32) {
        let req = MunmapReq {
            mapping_id: id,
            padding: 0,
        };
        let mut resp = vec![0u8; 64];
        self.be.dispatch(&msg(MsgType::Munmap, h, &req), &mut resp);
        assert_eq!(status(&resp), 0);
    }

    fn calls(&self) -> Vec<Call> {
        std::mem::take(&mut *self.vmm.calls.lock().unwrap())
    }
}

/// A pool the budgets refuse is refused as UVM refuses one, in its
/// rmStatus with the ioctl succeeding, and UVM is never asked (review
/// 2026-09-29 parity #29).
#[test]
fn a_refused_pool_says_so_in_its_rm_status() {
    let mut e = env(true);
    let h = e.uvm();
    HOST.with(|h| h.borrow_mut().clear());
    let mut p = vec![0u8; POOL];
    p[0..8].copy_from_slice(&X.to_le_bytes());
    for (len, want) in [
        (0u64, NV_ERR_INVALID_ARGUMENT),
        (u64::MAX, NV_ERR_INVALID_ARGUMENT),
    ] {
        p[8..16].copy_from_slice(&len.to_le_bytes());
        let r = e.ioctl(h, 68, &p);
        assert_eq!(status(&r), 0, "{len:#x}");
        let st = BODY + POOL - 8;
        assert_eq!(u32::from_le_bytes(r[st..st + 4].try_into().unwrap()), want);
    }
    assert!(HOST.with(|h| h.borrow().is_empty()), "UVM was never asked");
}

#[test]
fn a_pool_is_placed_at_its_own_address_and_mapped_write_back_from_the_aperture() {
    let mut e = env(true);
    let h = e.uvm();
    e.alloc(h, X, L);
    assert!(e.calls().is_empty());
    let r = e.mmap(h, X, L, 3).unwrap();
    assert_eq!((r.guest_phys_addr, r.size), (0, L));
    assert_eq!(r.caching, MMAP_CACHE_WB);
    assert_eq!(r.flags, MMAP_F_UVM_APERTURE);
    assert_ne!(r.mapping_id, 0);
    assert_eq!(
        e.calls(),
        vec![Call::PlaceUvm {
            off: 0,
            len: L,
            addr: X
        }]
    );
    e.munmap(h, r.mapping_id);
    assert_eq!(
        e.calls(),
        vec![Call::WithdrawUvm {
            off: 0,
            len: L,
            file_open: true
        }]
    );
}

#[test]
fn only_a_pool_of_the_same_file_asked_for_exactly_is_mapped() {
    let mut e = env(true);
    let h = e.uvm();
    let other = e.uvm();
    e.alloc(h, X, L);
    for (file, off, len, prot) in [
        (h, X + 4096, L - 4096, 3),
        (h, X, 2 * L, 3),
        (h, X, L - 4096, 3),
        (h, X, L, 1),
        (h, X + 0x1000_0000, L, 3),
        (other, X, L, 3),
    ] {
        assert_eq!(
            e.mmap(file, off, len, prot).map(|_| ()),
            Err(libc::EINVAL),
            "{file} {off:#x}+{len:#x} prot {prot}"
        );
    }
    // A pool UVM did not make is not one.
    ALLOC_STATUS.with(|s| s.set(NV_ERR_INVALID_ARGUMENT));
    e.alloc(other, X, L);
    assert_eq!(e.mmap(other, X, L, 3).map(|_| ()), Err(libc::EINVAL));
    // The tools device maps nothing.
    let tools =
        e.be.adopt_for_test(memfd(), HandleKind::Dev(DeviceKind::UvmTools));
    assert_eq!(e.mmap(tools, X, L, 3).map(|_| ()), Err(libc::EPERM));
    assert!(e.calls().is_empty(), "the VMM was never asked");
}

/// Without the aperture, a UVM mmap is refused as it always was -- and
/// never goes to the window, where the VMM's mmap of a UVM file failed
/// and took the request channel down with it (F2).
#[test]
fn without_the_aperture_a_uvm_mmap_never_reaches_the_vmm() {
    let mut e = env(false);
    let h = e.uvm();
    e.alloc(h, X, L);
    assert_eq!(e.mmap(h, X, L, 3).map(|_| ()), Err(libc::EINVAL));
    assert_eq!(e.mmap(h, 0, 4096, 3).map(|_| ()), Err(libc::EINVAL));
    assert!(e.calls().is_empty());
    // Nor on a v1 session.
    e.be.session_reset("test");
    let h = e.uvm();
    e.alloc(h, X, L);
    assert_eq!(e.mmap(h, X, L, 3).map(|_| ()), Err(libc::EINVAL));
    assert!(e.calls().is_empty());
}

#[test]
fn a_placement_the_vmm_refuses_is_enomem_and_gives_its_space_back() {
    let mut e = env(true);
    let h = e.uvm();
    e.alloc(h, X, L);
    e.vmm.fail.store(true, Ordering::Relaxed);
    assert_eq!(e.mmap(h, X, L, 3).map(|_| ()), Err(libc::ENOMEM));
    e.vmm.fail.store(false, Ordering::Relaxed);
    assert_eq!(e.mmap(h, X, L, 3).unwrap().guest_phys_addr, 0);
}

/// FREE goes to the host as it is: while the VMM maps the pool, UVM
/// refuses it, as it refuses a native process that still maps it, and
/// the placement stays. Once the last MUNMAP took it out, FREE succeeds
/// and the record goes.
#[test]
fn free_is_forwarded_while_placed_and_the_hosts_refusal_keeps_the_placement() {
    let mut e = env(true);
    let h = e.uvm();
    e.alloc(h, X, L);
    let r = e.mmap(h, X, L, 3).unwrap();
    e.calls();
    FREE_STATUS.with(|s| s.set(NV_ERR_INVALID_ARGUMENT));
    HOST.with(|h| h.borrow_mut().clear());
    assert_eq!(e.free(h, X), NV_ERR_INVALID_ARGUMENT);
    assert_eq!(HOST.with(|h| h.borrow().clone()), vec![34], "forwarded");
    assert!(e.calls().is_empty(), "still placed");
    e.munmap(h, r.mapping_id);
    assert!(matches!(e.calls()[..], [Call::WithdrawUvm { .. }]));
    FREE_STATUS.with(|s| s.set(0));
    assert_eq!(e.free(h, X), 0);
    assert_eq!(
        e.mmap(h, X, L, 3).map(|_| ()),
        Err(libc::EINVAL),
        "forgotten"
    );
    // A host that did free a placed pool: the slot goes as soon as the
    // backend hears of it.
    e.alloc(h, X, L);
    e.mmap(h, X, L, 3).unwrap();
    e.calls();
    assert_eq!(e.free(h, X), 0);
    assert!(matches!(e.calls()[..], [Call::WithdrawUvm { .. }]));
}

#[test]
fn closing_a_uvm_file_withdraws_its_pools_while_the_file_is_still_open() {
    let mut e = env(true);
    let h = e.uvm();
    e.alloc(h, X, L);
    e.alloc(h, X + 0x1000_0000, L);
    e.mmap(h, X, L, 3).unwrap();
    e.mmap(h, X + 0x1000_0000, L, 3).unwrap();
    e.calls();
    let mut resp = vec![0u8; 64];
    e.be.dispatch(&msg(MsgType::Close, h, &[0u8; 0]), &mut resp);
    assert_eq!(status(&resp), 0);
    let calls = e.calls();
    assert_eq!(calls.len(), 2);
    for c in calls {
        assert!(
            matches!(
                c,
                Call::WithdrawUvm {
                    file_open: true,
                    ..
                }
            ),
            "{c:?}"
        );
    }
}

#[test]
fn a_fresh_hello_withdraws_every_pool_and_asks_for_the_aperture_again() {
    let mut e = env(true);
    let (a, b) = (e.uvm(), e.uvm());
    e.alloc(a, X, L);
    e.alloc(b, X + 0x1000_0000, L);
    e.mmap(a, X, L, 3).unwrap();
    e.mmap(b, X + 0x1000_0000, L, 3).unwrap();
    e.calls();
    assert_ne!(e.hello(true) & BCAP_UVM_MAP, 0);
    let calls = e.calls();
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|c| matches!(
        c,
        Call::WithdrawUvm {
            file_open: true,
            ..
        }
    )));
    assert_eq!(e.be.uvm_maps.placements(), 0);
}

#[test]
fn two_files_cannot_map_pools_at_one_host_address() {
    let mut e = env(true);
    let (a, b) = (e.uvm(), e.uvm());
    e.alloc(a, X, L);
    e.alloc(b, X, L);
    e.mmap(a, X, L, 3).unwrap();
    e.calls();
    assert_eq!(
        e.mmap(b, X, L, 3).map(|_| ()),
        Err(libc::ENOMEM),
        "refused as any placement is, with no errno of its own"
    );
    assert!(e.calls().is_empty(), "refused before the VMM is asked");
}

#[test]
fn a_second_mmap_shares_the_placement_until_the_last_munmap() {
    let mut e = env(true);
    let (h, other) = (e.uvm(), e.uvm());
    e.alloc(h, X, L);
    let a = e.mmap(h, X, L, 3).unwrap();
    let b = e.mmap(h, X, L, 3).unwrap();
    assert_eq!(
        (a.guest_phys_addr, a.mapping_id),
        (b.guest_phys_addr, b.mapping_id)
    );
    assert_eq!(e.calls().len(), 1);
    e.munmap(other, a.mapping_id);
    e.munmap(h, a.mapping_id);
    assert!(
        e.calls().is_empty(),
        "another file's MUNMAP, then one of two"
    );
    e.munmap(h, a.mapping_id);
    assert!(matches!(e.calls()[..], [Call::WithdrawUvm { .. }]));
}

#[test]
fn mapping_ids_never_repeat_a_live_uvm_placements() {
    let mut e = env(true);
    let h = e.uvm();
    e.alloc(h, X, L);
    let id = e.mmap(h, X, L, 3).unwrap().mapping_id;
    e.be.next_mapping_id = id;
    let dri = e.be.adopt_for_test(memfd(), HandleKind::DriRender(0));
    let r = e.mmap(dri, 0, 4096, 3).unwrap();
    assert_ne!(r.mapping_id, id);
    assert!(matches!(e.calls()[..], [_, Call::Place(_)]));
}
