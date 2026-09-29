// SPDX-License-Identifier: Apache-2.0
//! The fake host kernel: RM, UVM, NVKMS and nvidia-drm, as far as what they
//! read and write through a parameter block goes.
//!
//! What it checks, on every call the backend makes:
//!
//! - every pointer the real driver would follow is NULL or a block of the
//!   call's own arena (sys/block.rs): never a value the guest sent (the words
//!   of the whole input are the taint set), never a small number, never any
//!   other memory of the process, and as long as the driver copies -- a copy
//!   that runs past a block's slack is the guard page's EFAULT;
//! - the argument itself holds `_IOC_SIZE` bytes (UVM: its table's size);
//! - memory registered by its guest pages (OS descriptors) is exactly the
//!   pages the guest named, in order, from the backend's mapping of guest
//!   RAM: each guest page holds its own address (`ram_word`), so a page out
//!   of place, a page of another region or of anything else, or a length
//!   past the pages named, fails the check;
//! - for IOCTL2 (`FakeSys`), the same for every pointer of the schema the
//!   call has, walked as the kernel walks it, and every descriptor the
//!   kernel "creates" is a real one this module made.
//!
//! Every address the backend handed the host is remembered for the message,
//! and `backend.rs` checks the reply carries none of them back.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashSet;
use std::os::fd::RawFd;

use abi::ioctl::*;
use abi::version::DriverVersion;

use crate::hostfd;
use crate::schema::{self, Kind, Len, Table};

/// What guest RAM holds at guest-physical `gpa` (8-aligned): its own
/// address, scrambled so no other structure looks like it.
pub fn ram_word(gpa: u64) -> u64 {
    gpa ^ 0x5a5a_0000_0000_a5a5
}

#[derive(Default)]
pub struct State {
    /// Every value the guest could have meant as an address: each 8-byte
    /// window of the input, from 64 KiB up.
    pub taint: HashSet<u64>,
    /// Addresses of the backend's the host was handed this message.
    pub seen: HashSet<u64>,
    /// The message being served, whole.
    pub msg: Vec<u8>,
    /// The host's answers: a stream of bytes the input supplied.
    pub mood: Vec<u8>,
    pub mood_at: usize,
    pub next_handle: u32,
    pub version: Option<DriverVersion>,
    /// Calls the host saw, for the harness's statistics.
    pub calls: u64,
    /// Pinned registrations checked.
    pub pinned: u64,
    /// The host call being emulated, for messages.
    pub call: String,
    /// IOCTL2 host calls.
    pub ioctl2: u64,
}

thread_local! {
    pub static ST: RefCell<State> = RefCell::new(State::default());
}

impl State {
    /// A byte of the host's mood: 0 most of the time, so calls succeed and
    /// the backend goes on to the next step.
    fn mood(&mut self) -> u8 {
        if self.mood.is_empty() {
            return 0;
        }
        let v = self.mood[self.mood_at % self.mood.len()];
        self.mood_at += 1;
        v
    }

    /// RM's status for this call: NV_OK, or now and then an error.
    fn rm_status(&mut self) -> u32 {
        match self.mood() {
            0xf0 => 0x1f,        // NV_ERR_INVALID_ARGUMENT
            0xf1 => 0x1b,        // NV_ERR_INSUFFICIENT_PERMISSIONS
            0xf2 => 0x51,        // NV_ERR_NO_MEMORY
            0xf3 => 0xffff_ffff, // nonsense
            _ => 0,
        }
    }

    fn handle(&mut self) -> u32 {
        self.next_handle = self.next_handle.wrapping_add(1).max(1);
        0xcaf0_0000 | (self.next_handle & 0xffff)
    }
}

/// Start a message: `msg` is its bytes, whole.
pub fn begin(msg: &[u8]) {
    ST.with(|s| {
        let mut s = s.borrow_mut();
        s.taint.clear();
        // Every 8 bytes of the message, and every 5 to 7 bytes zero-extended:
        // a block cut short leaves the host reading the guest's low bytes
        // over zeros of ours (nvidia.rs, dispatch_nested).
        for i in 0..msg.len() {
            for n in 5..=8 {
                let Some(w) = msg.get(i..i + n) else { break };
                let mut b = [0u8; 8];
                b[..n].copy_from_slice(w);
                let v = u64::from_le_bytes(b);
                if v >= 0x1_0000 {
                    s.taint.insert(v);
                }
            }
        }
        s.seen.clear();
        s.msg = msg.to_vec();
    });
}

/// With `NVGPU_FUZZ_STATS` set, one line on stderr per run saying how far it
/// got (`scripts/fuzz.sh stats`): host calls, IOCTL2 calls, OS-descriptor
/// registrations checked, window and UVM placements.
pub fn report(window: u64, uvm: u64) {
    if std::env::var_os("NVGPU_FUZZ_STATS").is_some() {
        ST.with(|s| {
            let s = s.borrow();
            eprintln!(
                "NVGPU_FUZZ_STATS calls={} ioctl2={} pinned={} window={window} uvm={uvm}",
                s.calls, s.ioctl2, s.pinned
            );
        });
    }
}

pub fn reset(mood: &[u8], version: Option<DriverVersion>) {
    ST.with(|s| {
        let mut s = s.borrow_mut();
        *s = State::default();
        s.mood = mood.to_vec();
        s.version = version;
    });
}

fn rd32(a: &[u8], o: usize) -> u32 {
    a.get(o..o + 4)
        .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
}
fn rd64(a: &[u8], o: usize) -> u64 {
    a.get(o..o + 8)
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}
fn put32(a: &mut [u8], o: usize, v: u32) {
    if let Some(b) = a.get_mut(o..o + 4) {
        b.copy_from_slice(&v.to_le_bytes());
    }
}
fn put64(a: &mut [u8], o: usize, v: u64) {
    if let Some(b) = a.get_mut(o..o + 8) {
        b.copy_from_slice(&v.to_le_bytes());
    }
}

/// The most any one copy is emulated for; RM's own limits are lower.
const MAX_COPY: u64 = 64 << 20;

/// The host follows `p` for `len` bytes, as copy_from_user and then
/// copy_to_user would: `false` where the kernel's copy would fault (EFAULT,
/// which the caller answers with), a panic where the copy would read or
/// write memory that is not a block the backend built for this very call --
/// a value the guest sent, a small number, anything else of the process's.
///
/// Probed against the call's own regions (sys/block.rs, `Arg::reach`): a
/// start inside none of them is the finding; a copy that starts in one and
/// runs past its end (and slack) is the guard page doing its job.
pub fn follow(arg: &crate::sys::block::Arg<'_>, p: u64, len: u64, what: &str) -> bool {
    if p == 0 || len == 0 {
        return true;
    }
    ST.with(|s| {
        let mut s = s.borrow_mut();
        assert!(
            !s.taint.contains(&p),
            "a guest value reached the host as a pointer: {what} = {p:#x} ({})",
            s.call
        );
        assert!(
            p >= 0x1_0000,
            "a small number reached the host as a pointer: {what} = {p:#x} ({})",
            s.call
        );
        s.seen.insert(p);
    });
    // The start is the backend's choice, always: a block of the call's or
    // nothing. Only how far the host copies may run into a guard page.
    let Some(fits) = arg.reach(p, len.min(MAX_COPY)) else {
        panic!(
            "the host was handed an address that is no block of the call: {what} = {p:#x} ({})",
            ST.with(|s| s.borrow().call.clone())
        );
    };
    fits
}

/// RM pins `size` bytes from `addr` for an OS descriptor: they must be the
/// pages the message's page list names, in order.
fn pinned(arg: &crate::sys::block::Arg<'_>, addr: u64, size: u64, what: &str) {
    assert!(
        follow(arg, addr, 1, what),
        "{what}: RM handed {addr:#x}, which is not mapped"
    );
    let pages = ST.with(|s| page_list(&s.borrow().msg));
    let Some(pages) = pages else {
        panic!("{what}: RM pins {size:#x} bytes at {addr:#x}, and the message has no page list");
    };
    let in_page = addr % 4096;
    let span = in_page.checked_add(size).expect("a pinned size that wraps");
    assert!(
        span <= pages.len() as u64 * 4096,
        "{what}: RM pins {size:#x} bytes from {in_page:#x} into the first page, past the {} \
         pages the guest named",
        pages.len()
    );
    let first = addr - in_page;
    for (i, gpa) in pages.iter().enumerate() {
        if (i as u64) * 4096 >= span {
            break;
        }
        for off in [0u64, 4088] {
            let at = first + i as u64 * 4096 + off;
            assert!(follow(arg, at, 8, what), "{what}: page {i} is not mapped");
            let got = arg.peek(at, 8);
            assert_eq!(
                got,
                ram_word(gpa + off),
                "{what}: page {i} of the registration is not guest page {gpa:#x}"
            );
        }
    }
    ST.with(|s| s.borrow_mut().pinned += 1);
}

/// The guest pages a v1 IOCTL message names, by an independent reading of
/// the wire format (protocol::messages: IoctlReq, DEEP_PAGE_LIST, OsDescHdr,
/// OsDescRun). `None` if it has none.
pub fn page_list(msg: &[u8]) -> Option<Vec<u64>> {
    use protocol::messages::DEEP_PAGE_LIST;
    let p = msg.get(16..)?;
    let data_len = rd32(p, 4) as usize;
    let nested_len = rd32(p, 12) as usize;
    if rd32(p, 16) != DEEP_PAGE_LIST {
        return None;
    }
    let deep_len = rd32(p, 20) as usize;
    let deep = p.get(24 + data_len + nested_len..)?.get(..deep_len)?;
    let nruns = rd32(deep, 0) as usize;
    let mut out = Vec::new();
    for i in 0..nruns {
        let r = deep.get(8 + 16 * i..8 + 16 * (i + 1))?;
        let gpa = rd64(r, 0);
        for k in 0..rd32(r, 8) as u64 {
            out.push(gpa + k * 4096);
            if out.len() > 1 << 20 {
                return None;
            }
        }
    }
    Some(out)
}

// ───────────────────────────── v1: HostIoctl ─────────────────────────────

const UVM_INITIALIZE: u32 = 0x3000_0001;
const UVM_PAGEABLE_MEM_ACCESS: u32 = 39;
const UVM_MAP_EXTERNAL_ALLOCATION: u32 = 33;

fn uvm_size(request: u32) -> usize {
    let v = ST.with(|s| s.borrow().version);
    v.and_then(schema::uvm_table)
        .and_then(|t| t.lookup(request))
        .map_or(16, |c| (c.size as usize).max(16))
}

/// The v1 host call (`nvidia::HostIoctl`): the argument must hold
/// `_IOC_SIZE(request)` bytes (UVM: its block), which is checked here, and
/// every pointer it follows must be a block of the call's.
pub fn fake_ioctl(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    let request = request as u32;
    ST.with(|s| s.borrow_mut().calls += 1);
    let ty = hostfd::ioc_type(request);
    let len = if ty == 0 {
        uvm_size(request)
    } else {
        hostfd::ioc_size(request)
    };
    if len == 0 {
        return 0;
    }
    let top = arg.addr();
    if !follow(arg, top, len as u64, "the argument") {
        return -libc::EFAULT;
    }
    // The argument, copied out, answered, and copied back.
    let mut a = arg.read(top, len).expect("followed");
    let r = fake_answer(arg, request, ty, len, &mut a);
    arg.write(top, &a);
    r
}

fn fake_answer(
    arg: &mut crate::sys::block::Arg<'_>,
    request: u32,
    ty: u8,
    len: usize,
    a: &mut [u8],
) -> i32 {
    let status = ST.with(|s| s.borrow_mut().rm_status());
    match (ty, hostfd::ioc_nr(request)) {
        (0, _) => {
            // UVM: status 8 bytes from the end (4 for MAP_EXTERNAL).
            if request == UVM_PAGEABLE_MEM_ACCESS {
                a[0] = ST.with(|s| s.borrow_mut().mood()) & 1;
            }
            let _ = UVM_INITIALIZE;
            let st = if request == UVM_MAP_EXTERNAL_ALLOCATION {
                len - 4
            } else {
                len - 8
            };
            put32(a, st, status);
        }
        (b'F', NV_ESC_RM_CONTROL) if len >= 32 => {
            let cmd = rd32(a, 8);
            let params = rd64(a, 16);
            let size = rd32(a, 24);
            if !follow(arg, params, u64::from(size), "RM_CONTROL params") {
                return -libc::EFAULT;
            }
            if params != 0 && size != 0 {
                let n = arg.read(params, size as usize).expect("followed");
                control_pointers(arg, cmd, &n);
            }
            put32(a, 28, status);
        }
        (b'F', NV_ESC_RM_ALLOC) if len >= 48 => {
            let class = rd32(a, 12);
            let params = rd64(a, 16);
            let psize = rd32(a, 32);
            let _ = follow(arg, rd64(a, 24), 4, "RM_ALLOC pRightsRequested");
            if class == crate::nvos::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR {
                if !follow(arg, params, 40, "OS descriptor params") {
                    return -libc::EFAULT;
                }
                let n = arg.read(params, 40).expect("followed");
                pinned(
                    arg,
                    rd64(&n, 16),
                    rd64(&n, 24).wrapping_add(1),
                    "RM_ALLOC OS descriptor",
                );
            } else {
                // RM copies the class's own size; the block is at least what
                // the caller said it is.
                if !follow(
                    arg,
                    params,
                    u64::from(psize.max(1)).min(4096),
                    "RM_ALLOC params",
                ) {
                    return -libc::EFAULT;
                }
            }
            if rd32(a, 8) == 0 {
                let h = ST.with(|s| s.borrow_mut().handle());
                put32(a, 8, h);
            }
            put32(a, 40, status);
        }
        (b'F', NV_ESC_RM_ALLOC_MEMORY) if len >= 56 => {
            if rd32(a, 12) == crate::nvos::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR {
                pinned(
                    arg,
                    rd64(a, 24),
                    rd64(a, 32).wrapping_add(1),
                    "ALLOC_MEMORY OS descriptor",
                );
            } else {
                put64(a, 24, 0);
            }
            if rd32(a, 8) == 0 {
                let h = ST.with(|s| s.borrow_mut().handle());
                put32(a, 8, h);
            }
            put32(a, 40, status);
        }
        (b'F', NV_ESC_RM_VID_HEAP_CONTROL) if len >= 184 => {
            if rd32(a, 8) == crate::nvos::NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR {
                pinned(
                    arg,
                    rd64(a, 64),
                    rd64(a, 72).wrapping_add(1),
                    "VID_HEAP OS descriptor",
                );
            }
            if rd32(a, 40) == 0 {
                let h = ST.with(|s| s.borrow_mut().handle());
                put32(a, 40, h);
            }
            put32(a, 20, status);
        }
        (b'F', NV_ESC_RM_FREE) if len >= 16 => put32(a, 12, status),
        (b'F', NV_ESC_RM_IDLE_CHANNELS) if len >= 56 => {
            let count = u64::from(rd32(a, 12));
            if (rd32(a, 40) >> 4) & 0xf == 0 && count != 0 {
                for off in [16, 24, 32] {
                    if !follow(arg, rd64(a, off), count * 4, "IDLE_CHANNELS list") {
                        return -libc::EFAULT;
                    }
                }
            } else {
                for off in [16, 24, 32] {
                    assert_eq!(
                        rd64(a, off),
                        0,
                        "IDLE_CHANNELS pointer at {off} for one channel"
                    );
                }
            }
            put32(a, 44, status);
        }
        (b'F', NV_ESC_RM_MAP_MEMORY) if len >= 56 => {
            // pLinearAddress: RM's cookie for the mmap that follows.
            put64(
                a,
                32,
                0x7e57_0000_0000 | u64::from(ST.with(|s| s.borrow_mut().handle())) << 12,
            );
            put32(a, 40, status);
        }
        (b'm', 0) if len >= 16 => {
            // NVKMS: NvKmsIoctlParams { cmd, size, address }.
            if !follow(arg, rd64(a, 8), u64::from(rd32(a, 4)), "NVKMS params") {
                return -libc::EFAULT;
            }
        }
        (b'd', 0x41) if len >= 24 => {
            if !follow(
                arg,
                rd64(a, 8),
                rd64(a, 16).min(4096),
                "GEM import nvkms_params_ptr",
            ) {
                return -libc::EFAULT;
            }
        }
        _ => {}
    }
    0
}

/// The pointers RM follows inside a control's parameters.
fn control_pointers(arg: &crate::sys::block::Arg<'_>, cmd: u32, params: &[u8]) {
    let deep = abi::rmctrl::deep_control(cmd);
    for &off in abi::rmctrl::pointers(cmd).map_or(&[][..], |c| c.ptrs) {
        let p = rd64(params, off);
        if p == 0 {
            continue;
        }
        let len = deep
            .and_then(|d| d.ptrs.iter().find(|r| r.ptr == off))
            .and_then(|r| r.size(params))
            .map_or(1, u64::from);
        let _ = follow(arg, p, len, "a pointer inside RM_CONTROL params");
    }
}

// ───────────────────────────── IOCTL2: xfer::Sys ─────────────────────────────

/// The IOCTL2 host: the DRM, nvidia-drm and NVKMS calls the schema tables
/// describe, their pointers followed as the kernel follows them.
pub struct FakeSys {
    pub version: Option<DriverVersion>,
}

fn tables(v: Option<DriverVersion>) -> Vec<&'static Table> {
    let mut t = vec![schema::DRM_TABLE];
    if let Some(m) = v.and_then(schema::modeset_table) {
        t.push(m);
    }
    t
}

/// Walk `fields` of the struct at `base` (`len` bytes, already followed) as
/// the kernel copies it: every pointer followed for its length, each element
/// it points at walked in turn; every descriptor the kernel creates made.
fn walk(
    arg: &mut crate::sys::block::Arg<'_>,
    t: &Table,
    base: u64,
    len: usize,
    fields: schema::Span,
    depth: u32,
    outer_size: u32,
) {
    if depth > 6 || base == 0 || len == 0 {
        return;
    }
    let Some(s) = arg.read(base, len) else {
        return;
    };
    let s = &s[..];
    for f in t.fields(fields) {
        let off = f.off as usize;
        if let Some(c) = f.cond {
            if !c.holds(rd32(s, c.off as usize)) {
                continue;
            }
        }
        match f.kind {
            Kind::Ptr {
                len: l,
                stride,
                children,
                ..
            } => {
                let p = rd64(s, off);
                let n: u64 = match l {
                    Len::Const(n) => u64::from(n),
                    Len::Count { off, width, elem } => {
                        let v = match width {
                            1 => u64::from(s.get(off as usize).copied().unwrap_or(0)),
                            2 => u64::from(rd32(s, off as usize) & 0xffff),
                            4 => u64::from(rd32(s, off as usize)),
                            _ => rd64(s, off as usize),
                        };
                        v.saturating_mul(u64::from(elem))
                    }
                    Len::Sum { .. } => 1,
                    Len::NvkmsParams => u64::from(outer_size),
                };
                if p == 0 || n == 0 {
                    continue;
                }
                if !follow(arg, p, n, f.name) {
                    continue;
                }
                if stride > 0 && children.len > 0 {
                    let n = n.min(MAX_COPY);
                    for i in 0..n / u64::from(stride) {
                        walk(
                            arg,
                            t,
                            p + i * u64::from(stride),
                            stride as usize,
                            children,
                            depth + 1,
                            0,
                        );
                    }
                }
            }
            Kind::Array {
                count,
                stride,
                children,
                ..
            } => {
                for i in 0..count {
                    let at = off + (i * stride) as usize;
                    if at + stride as usize <= len {
                        walk(
                            arg,
                            t,
                            base + at as u64,
                            stride as usize,
                            children,
                            depth + 1,
                            0,
                        );
                    }
                }
            }
            Kind::FdOut { width } => {
                // A real descriptor, the backend's from here, as a
                // kernel-made one would be.
                if let Ok(fd) = crate::sys::fd::eventfd(libc::EFD_CLOEXEC) {
                    let fd = std::os::fd::IntoRawFd::into_raw_fd(fd);
                    let w = if width == 4 { 4 } else { 8 };
                    arg.write(base + off as u64, &(fd as i64 as u64).to_le_bytes()[..w]);
                }
            }
            _ => {}
        }
    }
}

impl crate::sys::block::Kernel for FakeSys {
    fn ioctl(&self, _fd: RawFd, cmd: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let cmd = cmd as u32;
        ST.with(|s| {
            let mut s = s.borrow_mut();
            s.calls += 1;
            s.ioctl2 += 1;
        });
        let size = hostfd::ioc_size(cmd);
        if size == 0 {
            return 0;
        }
        let top = arg.addr();
        if !follow(arg, top, size as u64, "the IOCTL2 argument") {
            return -libc::EFAULT;
        }
        if ST.with(|s| s.borrow_mut().mood()) == 0xee {
            return -libc::EINVAL;
        }
        let entry = tables(self.version).into_iter().find_map(|t| {
            [
                schema::Class::Render,
                schema::Class::Kms,
                schema::Class::Modeset,
            ]
            .into_iter()
            .find_map(|c| t.lookup(c, cmd).filter(|e| e.cmd == cmd))
            .map(|e| (t, e))
        });
        if let Some((t, e)) = entry {
            ST.with(|s| s.borrow_mut().call = e.name.to_string());
            if e.special == schema::Special::NvkmsParams {
                // NvKmsIoctlParams { cmd, size, address }: the command's own
                // entry describes this outer struct, its one root field the
                // pointer to the params block (Len::NvkmsParams: `size`).
                let a = arg.read(top, size).expect("followed");
                let (ncmd, nsize) = (rd32(&a, 0), rd32(&a, 4));
                if let Some(inner) = t.lookup_nvkms(ncmd) {
                    ST.with(|s| s.borrow_mut().call = inner.name.to_string());
                    walk(arg, t, top, size, inner.fields, 0, nsize);
                } else {
                    let _ = follow(
                        arg,
                        rd64(&a, 8),
                        u64::from(nsize),
                        "NvKmsIoctlParams.address",
                    );
                }
            } else {
                walk(arg, t, top, size, e.fields, 0, 0);
            }
        }
        0
    }
}

impl crate::xfer::Sys for FakeSys {
    fn close(&self, fd: std::os::fd::OwnedFd) {
        // A descriptor xfer received from this fake kernel: a real one.
        drop(fd);
    }

    fn size_of(&self, fd: RawFd) -> i64 {
        crate::sys::fd::size(fd).map_or(-(libc::EBADF as i64), |n| n as i64)
    }
}

/// Whether a reply carries an address the host was handed.
pub fn leaked(reply: &[u8]) -> Option<u64> {
    ST.with(|s| {
        let s = s.borrow();
        reply
            .windows(8)
            .map(|w| u64::from_le_bytes(w.try_into().unwrap()))
            .find(|v| s.seen.contains(v))
    })
}
