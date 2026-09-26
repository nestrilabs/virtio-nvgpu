// SPDX-License-Identifier: Apache-2.0
//! The whole dispatcher, fed a guest's messages.
//!
//! Input: a configuration byte, a byte of host mood, then messages, each a
//! little-endian u16 length and that many bytes -- a whole message, header
//! included, exactly as the guest's driver puts it on the control queue. The
//! response capacity of each is the next u16 (0: the transport's maximum).
//!
//! Before the messages the backend has a v1 session (or, with `CFG_HELLO`, a
//! v2 one) and a set of files already open, one of each kind the guest can
//! hold, at handles 1 upwards in [`KINDS`] order; a message may name any
//! handle. The host is `host.rs`; the window and the UVM aperture are
//! [`Window`], which holds the backend to its placements; guest RAM is
//! [`guest_ram`], every page holding its own address.
//!
//! After each message: the reply fits the capacity, carries no address the
//! host was handed, and every IOCTL2 is executed and finished as the
//! transport would. After the last, the session is torn down, and every
//! descriptor the run made must be closed.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

use protocol::messages::*;

use super::{Bytes, host};
use crate::hostfd::{CardNode, HandleKind};
use crate::nvidia::{DriDevice, NvidiaBackend};
use crate::osdesc::{GuestRam, RamRegion};
use crate::session::Outcome;
use crate::shm::ZoneConfig;

pub const CFG_HELLO: u8 = 1 << 0;
pub const CFG_COMPUTE: u8 = 1 << 1;
pub const CFG_RAM: u8 = 1 << 2;
pub const CFG_PROC_ID: u8 = 1 << 3;
pub const CFG_KMS_CARD: u8 = 1 << 4;
pub const CFG_FENCES: u8 = 1 << 5;
/// Two bits: no version known, 535.129.03, 595.99.02, 610.57.04.
pub const CFG_VERSION_SHIFT: u8 = 6;

const VERSIONS: [Option<&str>; 4] = [
    None,
    Some("535.129.03"),
    Some("595.99.02"),
    Some("610.57.04"),
];

/// The files open before the first message, at handles 1.. in this order.
pub const KINDS: [HandleKind; 12] = [
    HandleKind::Dev(DeviceKind::Ctl),
    HandleKind::Dev(DeviceKind::Gpu(0)),
    HandleKind::Dev(DeviceKind::Modeset),
    HandleKind::DriRender(0),
    HandleKind::DrmLease(0),
    HandleKind::Dev(DeviceKind::Uvm),
    HandleKind::Syncobj,
    HandleKind::SyncFile,
    HandleKind::Dmabuf,
    HandleKind::Eventfd,
    HandleKind::Memfd,
    HandleKind::DrmCard(0),
];

// ───────────────────────────── the window ─────────────────────────────

#[derive(Default)]
struct Placed {
    /// Window offset -> length.
    window: BTreeMap<u64, u64>,
    /// Aperture offset -> (length, host address).
    aperture: BTreeMap<u64, (u64, u64)>,
    /// Placements made, for `host::report`.
    placed: u64,
    placed_uvm: u64,
}

/// The VMM's side of the window and the UVM aperture: it places what it is
/// asked to and checks each request is one a VMM could honour without
/// exposing anything else -- inside the window, page-aligned, over nothing
/// already placed, of a descriptor the backend holds for the guest (never
/// one of its own, `privfd`); a withdrawal of exactly something placed; a
/// UVM pool inside the aperture and at a host address in UVM's band.
pub struct Window {
    size: u64,
    aperture: u64,
    placed: Arc<Mutex<Placed>>,
}

fn pages(len: u64) -> u64 {
    len.div_ceil(4096) * 4096
}

impl Window {
    /// Whether a placement (rounded up to whole pages, as the VMM's mmap
    /// rounds it) covers any of `[off, off + len)`.
    fn check_window(p: &Placed, off: u64, len: u64) -> bool {
        p.window
            .range(..off + len)
            .next_back()
            .is_some_and(|(&o, &l)| o + pages(l) > off)
    }
}

impl crate::shm::WindowPlacer for Window {
    fn place(
        &self,
        off: u64,
        len: u64,
        fd: i32,
        _fd_off: u64,
        _w: bool,
    ) -> crate::error::Result<()> {
        // The VMM maps whole pages: an unaligned length covers the rest of
        // its last page, which must be this placement's alone.
        assert!(
            len > 0 && off % 4096 == 0,
            "placement {off:#x}+{len:#x} not page-aligned"
        );
        assert!(
            off.checked_add(pages(len)).is_some_and(|e| e <= self.size),
            "placement {off:#x}+{len:#x} outside the {:#x}-byte window",
            self.size
        );
        assert!(
            !crate::privfd::is_private(fd),
            "the backend placed one of its own descriptors ({fd}) in the guest's window"
        );
        let mut p = self.placed.lock().unwrap();
        assert!(
            !Self::check_window(&p, off, pages(len)),
            "placement {off:#x}+{len:#x} over another"
        );
        p.window.insert(off, len);
        p.placed += 1;
        Ok(())
    }

    fn withdraw(&self, off: u64, len: u64) -> crate::error::Result<()> {
        let mut p = self.placed.lock().unwrap();
        // A withdrawal covers whole placements, never part of one.
        let inside: Vec<(u64, u64)> = p
            .window
            .range(off..off + pages(len))
            .map(|(&o, &l)| (o, l))
            .collect();
        for &(o, l) in &inside {
            assert!(
                o + pages(l) <= off + pages(len),
                "withdrawal {off:#x}+{len:#x} cuts placement {o:#x}+{l:#x}"
            );
            p.window.remove(&o);
        }
        assert!(
            !Self::check_window(&p, off, pages(len)),
            "withdrawal {off:#x}+{len:#x} cuts a placement that starts before it"
        );
        Ok(())
    }

    fn place_uvm(&self, ap_off: u64, len: u64, fd: i32, addr: u64) -> crate::error::Result<()> {
        assert!(len > 0 && ap_off % 4096 == 0 && len % 4096 == 0 && addr % 4096 == 0);
        assert!(
            ap_off.checked_add(len).is_some_and(|e| e <= self.aperture),
            "UVM placement {ap_off:#x}+{len:#x} outside the {:#x}-byte aperture",
            self.aperture
        );
        assert!(
            addr >= UVM_HVA_MIN && addr.checked_add(len).is_some_and(|e| e <= UVM_HVA_MAX),
            "UVM pool at host address {addr:#x}+{len:#x}, outside UVM's band"
        );
        assert!(
            !crate::privfd::is_private(fd),
            "a private descriptor placed as a UVM pool"
        );
        let mut p = self.placed.lock().unwrap();
        for (&o, &(l, a)) in &p.aperture {
            assert!(
                o + l <= ap_off || ap_off + len <= o,
                "UVM placement over another in the aperture"
            );
            assert!(
                a + l <= addr || addr + len <= a,
                "two UVM pools at one host address"
            );
        }
        p.aperture.insert(ap_off, (len, addr));
        p.placed_uvm += 1;
        Ok(())
    }

    fn withdraw_uvm(&self, ap_off: u64, len: u64) -> crate::error::Result<()> {
        let mut p = self.placed.lock().unwrap();
        match p.aperture.remove(&ap_off) {
            Some((l, _)) => assert_eq!(l, len, "UVM withdrawal of another length"),
            None => panic!("UVM withdrawal of {ap_off:#x}, which holds nothing"),
        }
        Ok(())
    }
}

// ───────────────────────────── guest RAM ─────────────────────────────

const LOW_PAGES: u64 = 64;
const HIGH_PAGES: u64 = 64;
const HIGH: u64 = 1 << 32;

/// Guest RAM as a VMM splits it: low RAM at 0 and high RAM at 4 GiB, one
/// memfd behind both (adjacent in the file, not in guest-physical space),
/// and a small anonymous region at 8 GiB. Every 8 bytes hold
/// `host::ram_word` of their own guest-physical address.
pub fn guest_ram() -> GuestRam {
    use crate::sys::mem::Mapping;
    let file = std::fs::File::from(
        crate::sys::fd::memfd(c"fuzz-guest-ram", libc::MFD_CLOEXEC).unwrap(),
    );
    let len = ((LOW_PAGES + HIGH_PAGES) * 4096) as usize;
    file.set_len(len as u64).unwrap();
    let base = Mapping::shared(&file, len, 0, true).unwrap();
    let anon_len = 16 * 4096;
    let anon = Mapping::anon(anon_len).unwrap();
    let fill = |m: &Mapping, at: usize, gpa: u64, len: usize| {
        let words: Vec<u8> = (0..len)
            .step_by(8)
            .flat_map(|i| host::ram_word(gpa + i as u64).to_le_bytes())
            .collect();
        m.write(at, &words);
    };
    let lo = (LOW_PAGES * 4096) as usize;
    fill(&base, 0, 0, lo);
    fill(&base, lo, HIGH, len - lo);
    fill(&anon, 0, 2 * HIGH, anon_len);
    crate::privfd::register(file.as_raw_fd());
    let file = Arc::new(file);
    let (bs, anons) = (base.span(), anon.span());
    GuestRam::new(
        vec![
            RamRegion {
                gpa: 0,
                len: lo as u64,
                host: bs.sub(0, lo as u64).unwrap(),
                file: Some((file.clone(), 0)),
            },
            RamRegion {
                gpa: HIGH,
                len: (len - lo) as u64,
                host: bs.sub(lo as u64, (len - lo) as u64).unwrap(),
                file: Some((file.clone(), lo as u64)),
            },
            RamRegion {
                gpa: 2 * HIGH,
                len: anon_len as u64,
                host: anons,
                file: None,
            },
        ],
        Arc::new((base, anon, FileGuard(file))),
    )
}

struct FileGuard(Arc<std::fs::File>);
impl Drop for FileGuard {
    fn drop(&mut self) {
        crate::privfd::unregister(self.0.as_raw_fd());
    }
}

// ───────────────────────────── the files ─────────────────────────────

fn file_for(kind: HandleKind) -> OwnedFd {
    use crate::sys::fd;
    let made = match kind {
        HandleKind::Eventfd => fd::eventfd(libc::EFD_CLOEXEC),
        HandleKind::Dev(DeviceKind::Ctl)
        | HandleKind::Dev(DeviceKind::Modeset)
        | HandleKind::Syncobj
        | HandleKind::SyncFile => fd::open(c"/dev/null", libc::O_RDWR | libc::O_CLOEXEC),
        // Mappable: a GPU file, a render node, a dma-buf, a memfd.
        _ => fd::memfd(c"fuzz-file", libc::MFD_CLOEXEC).inspect(|f| {
            let _ = fd::ftruncate(f, 1 << 20);
        }),
    };
    made.unwrap_or_else(|e| panic!("cannot make a file for {kind:?}: {e}"))
}

fn hello(be: &mut NvidiaBackend, caps: u32, aperture_mib: u32) {
    let req = HelloReq {
        proto: PROTO_V2,
        flags: HELLO_F_FRESH,
        guest_caps: caps,
        uvm_aperture_mib: aperture_mib,
    };
    let mut msg = Vec::new();
    for v in [MsgType::Hello as u32, 0, 0, 1] {
        msg.extend_from_slice(&v.to_le_bytes());
    }
    msg.extend_from_slice(crate::sys::pod::bytes(&req));
    let mut resp = [0u8; 256];
    be.dispatch(&msg, &mut resp);
}

/// One run, configured by the input's first byte.
pub fn run(data: &[u8]) {
    run_with(data, 0);
}

/// One run with a v2 session, compute, guest RAM and the newest release's
/// tables, whatever the first byte says: IOCTL2, HOST_OP, WATCH, OS
/// descriptors and the UVM aperture from the first message.
pub fn run_v2(data: &[u8]) {
    run_with(
        data,
        CFG_HELLO | CFG_COMPUTE | CFG_RAM | 3 << CFG_VERSION_SHIFT,
    );
}

/// A backend on the fake host, configured by `cfg` (the `CFG_*` bits), with
/// the files of [`KINDS`] open; and what its window holds.
pub struct Vm {
    pub be: NvidiaBackend,
    placed: Arc<Mutex<Placed>>,
}

impl Vm {
    pub fn new(cfg: u8, mood: &[u8]) -> Self {
        let version = VERSIONS[(cfg >> CFG_VERSION_SHIFT) as usize & 3];
        let parsed = version.and_then(abi::version::DriverVersion::parse);
        host::reset(mood, parsed);
        let mut be = NvidiaBackend::new(ZoneConfig {
            uc_size: 4096 * 4,
            wc_size: 4096 * 16,
            wb_size: 4096 * 8,
        });
        be.set_host_nodes_for_test(
            vec![DriDevice {
                name: "renderD128".into(),
                major: 226,
                minor: 128,
                slot_index: 0,
                dev_info: [0; crate::nvidia::NV_DEV_INFO_WORDS],
                dev_info_size: 0,
            }],
            vec![CardNode {
                name: "card1".into(),
                major: 226,
                minor: 1,
                render_index: 0,
            }],
        );
        be.set_host_ioctl_for_test(host::fake_ioctl);
        be.xfer_sys = Arc::new(host::FakeSys { version: parsed });
        let placed = Arc::new(Mutex::new(Placed::default()));
        be.set_window(Box::new(Window {
            size: be.shm_total_size(),
            aperture: 64 << 20,
            placed: placed.clone(),
        }));
        if let Some(v) = version {
            be.set_host_driver_version(v);
        }
        {
            let c = be.config_mut();
            c.allow_compute = cfg & CFG_COMPUTE != 0;
            c.kms_card = cfg & CFG_KMS_CARD != 0;
            c.fences = cfg & CFG_FENCES != 0;
        }
        if cfg & CFG_RAM != 0 {
            be.set_guest_ram(Some(guest_ram()));
        }
        if cfg & CFG_HELLO != 0 {
            let caps = if cfg & CFG_PROC_ID != 0 {
                GCAP_PROC_ID | GCAP_PROC_EUID | GCAP_UVM_APERTURE
            } else {
                GCAP_UVM_APERTURE
            };
            hello(&mut be, caps, 64);
        }
        for k in KINDS {
            be.adopt_for_test(file_for(k), k);
        }
        Self { be, placed }
    }

    /// Serve one message into a response buffer of `cap` bytes, all the way
    /// through, as the transport would; the reply, checked.
    pub fn serve(&mut self, msg: &[u8], cap: usize) -> Vec<u8> {
        host::begin(msg);
        let mut reply = match self.be.serve(msg, cap) {
            Outcome::Reply(r) => r,
            Outcome::Ioctl2(mut p) => {
                p.execute();
                self.be.finish_ioctl2(p)
            }
        };
        reply.stamp();
        assert!(
            reply.bytes.len() <= cap,
            "a reply of {} bytes for a capacity of {cap}",
            reply.bytes.len()
        );
        if let Some(a) = host::leaked(&reply.bytes) {
            panic!("the reply carries {a:#x}, an address the host was handed");
        }
        // The pump would take these; its descriptors close here.
        drop(self.be.take_pump_cmds());
        reply.bytes
    }

    /// The session ends: nothing may be left in the window or the aperture.
    pub fn end(mut self) {
        self.be.teardown();
        drop(self.be.take_pump_cmds());
        drop(self.be);
        let p = self.placed.lock().unwrap();
        host::report(p.placed, p.placed_uvm);
        assert!(
            p.window.is_empty(),
            "placements left after teardown: {:?}",
            p.window
        );
        assert!(
            p.aperture.is_empty(),
            "UVM placements left after teardown: {:?}",
            p.aperture
        );
    }
}

/// Run `f` and check it left no descriptor open.
pub fn no_fd_leak(f: impl FnOnce()) {
    let fds_before = super::open_fds();
    f();
    crate::closer::wait_idle(std::time::Duration::from_secs(5));
    let fds_after = super::open_fds();
    assert!(
        fds_after <= fds_before,
        "{} descriptor(s) left open after the session",
        fds_after - fds_before
    );
}

fn run_with(data: &[u8], force: u8) {
    super::sandboxed();
    let mut b = Bytes::new(data);
    let cfg = b.u8() | force;
    let mood = b.take(16).to_vec();
    no_fd_leak(|| {
        let mut vm = Vm::new(cfg, &mood);
        let mut n = 0;
        while !b.is_empty() && n < 64 {
            n += 1;
            let len = b.u16() as usize;
            let msg = b.take(len);
            let cap = match b.u16() {
                0 => 1 << 20,
                c => c as usize,
            };
            vm.serve(msg, cap);
        }
        vm.end();
    });
}
