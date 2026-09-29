// SPDX-License-Identifier: GPL-2.0-only

//! virtio-gpu-nv's untrusted-input parsers, in Rust (`NVGPU_RUST=1`).
//!
//! The parsing itself is `driver/rust/core` -- the same sources, included
//! here as the [`guest`] module -- which has no `unsafe` and no kernel
//! dependency. This file is the only Rust in the module that has either: it
//! implements the core's traits on top of the C services in
//! `nvgpu_rs_glue.c` (copies from and to the caller, allocation, the
//! transport, the hooks, pinning; `nvgpu_rs.h` is the ABI), and exports the
//! functions `nvgpu.h` declares for the parsers. Every `unsafe` block is an
//! FFI call, a C table or buffer made a slice, or the state a hook is handed
//! made a reference again; each says why it is sound.
//!
//! C hands Rust nothing of the caller's but addresses: every byte behind
//! them is copied once, by `nvgpu_rs_copy_from()`, into a buffer this side
//! owns, and parsed there.

#[path = "rust/core/src/guest/mod.rs"]
#[forbid(unsafe_code)]
pub mod guest;

use core::ffi::{c_char, c_int, c_long, c_uint, c_void};
use core::ptr;

use guest::atomic;
use guest::deep::{self, UserMem};
use guest::dispatch;
use guest::i2::{self, State, Store, Xfer};
use guest::osdesc::{self, PinError};
use guest::rm;
use guest::schema::{SField, SIoctl, SchemaSet, Table};
use guest::wire::{Errno, EFAULT, EINVAL, ENOMEM, I2_MAX_BUFS, PROC_ID_LEN};

// `unsigned long` is the kernel's 64-bit word: the ioctl argument comes as u64.
const _: () = assert!(core::mem::size_of::<core::ffi::c_ulong>() == 8);

/// `struct nvgpu_rs_table`.
#[repr(C)]
struct RsTable {
    ioctls: *const SIoctl,
    nioctls: u64,
    fields: *const SField,
    nfields: u64,
    planes: *const u8,
    nplanes: u64,
}

/// `struct nvgpu_rs_tables`.
#[repr(C)]
struct RsTables {
    drm: RsTable,
    modeset: RsTable,
    has_modeset: u32,
    reserved: u32,
}

/// `struct nvgpu_rs_i2_args`.
#[repr(C)]
struct RsI2Args {
    uarg: u64,
    max_req: u64,
    max_resp: u64,
    sclass: u32,
    cmd: u32,
    handle: u32,
    render: u32,
    xflags: u32,
    compat: u8,
    kernel: u8,
    karg: u8,
    reserved: u8,
}

/// `struct nvgpu_rs_deep_ptr`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RsDeepPtr {
    ptr: u32,
    flags: u32,
    ncounts: u32,
    count_off: [u32; 2],
    count_width: [u32; 2],
    scale: u32,
    elem: u32,
}

/// `struct nvgpu_rs_deep_control`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RsDeepControl {
    cmd: u32,
    nptrs: u32,
    ptrs: [RsDeepPtr; 4],
}

// The layouts nvgpu_rs.h asserts on the C side.
const _: () = assert!(core::mem::size_of::<RsTable>() == 48);
const _: () = assert!(core::mem::size_of::<RsTables>() == 104);
const _: () = assert!(core::mem::size_of::<RsI2Args>() == 48);
const _: () = assert!(core::mem::size_of::<RsDeepPtr>() == 36);
const _: () = assert!(core::mem::size_of::<RsDeepControl>() == 152);

const CAP_DEEP_SEGS: u32 = 1 << 0;
const CAP_PROC_IDS: u32 = 1 << 1;
const CAP_PROC_EUID: u32 = 1 << 2;
const CAP_OS_DESC: u32 = 1 << 3;

const WARN_CONTROL_FD: u32 = 1;
const WARN_CONTROL_OS_EVENT: u32 = 2;
const WARN_ALLOC_EVENT_FD: u32 = 3;
const WARN_EVENT_BUFFER: u32 = 4;
const WARN_SURFACE_FD: u32 = 5;
const WARN_I2_CMD: u32 = 16;
const WARN_I2_MALFORMED: u32 = 17;
const WARN_I2_UNNAMED: u32 = 18;

const PIN_OK: c_int = 0;
const PIN_NOMEM: c_int = 1;

extern "C" {
    fn nvgpu_rs_copy_from(kernel: bool, dst: *mut c_void, src: u64, len: usize) -> c_int;
    fn nvgpu_rs_copy_to(kernel: bool, dst: u64, src: *const c_void, len: usize) -> c_int;
    fn nvgpu_rs_kvzalloc(len: usize) -> *mut c_void;
    fn nvgpu_rs_kvfree(p: *mut c_void);

    fn nvgpu_rs_caps(nfd: *mut c_void) -> u32;
    fn nvgpu_rs_fd_handle(nfd: *mut c_void) -> u32;
    fn nvgpu_rs_send_recv(
        nfd: *mut c_void,
        req: *const c_void,
        req_len: usize,
        resp: *mut c_void,
        resp_len: usize,
        used: *mut u32,
    ) -> c_int;
    fn nvgpu_rs_handle_for_fd(nfd: *mut c_void, guest_fd: c_int, handle: *mut u32) -> c_int;
    fn nvgpu_rs_proc_id(nfd: *mut c_void, dst: *mut c_void);
    fn nvgpu_rs_driver_version(nfd: *mut c_void, len: *mut usize) -> *const c_char;
    fn nvgpu_rs_clock_to_guest(nfd: *mut c_void, raw: u32, host_ns: i64, guest_ns: *mut i64) -> bool;
    fn nvgpu_rs_rm_deep(cmd: u32, idle_channels: bool, out: *mut RsDeepControl) -> bool;
    fn nvgpu_rs_v1v2(cmd: u32, userptr_offset: *mut u32, info_style: *mut bool) -> bool;
    fn nvgpu_rs_rmalloc_size(hclass: u32) -> u32;
    fn nvgpu_rs_fd_translation(nfd: *mut c_void, key: u32, payload: *mut u32) -> bool;
    fn nvgpu_rs_uvm_size(nfd: *mut c_void, cmd: u32) -> c_int;
    fn nvgpu_rs_warn(nfd: *mut c_void, code: u32, a: u64, b: u64);
    fn nvgpu_rs_osdesc_reap(nfd: *mut c_void);
    fn nvgpu_rs_osdesc_pin(start: u64, npages: u64, write: bool, pages: *mut *mut c_void) -> c_int;
    fn nvgpu_rs_page_phys(pages: *mut c_void, i: u64) -> u64;
    fn nvgpu_rs_osdesc_keep(nfd: *mut c_void, id: u64, pages: *mut c_void, npages: u64, write: bool);
    fn nvgpu_rs_osdesc_unpin(pages: *mut c_void, npages: u64, write: bool);
    fn nvgpu_rs_osdesc_send(
        nfd: *mut c_void,
        req: *const c_void,
        req_len: usize,
        resp: *mut c_void,
        resp_len: usize,
        used: *mut u32,
        pages: *mut c_void,
        npages: u64,
        write: bool,
    ) -> c_int;

    fn nvgpu_rs_i2_fd_in(
        call: *mut c_void,
        st: *mut c_void,
        ret: *mut i32,
        buf: u32,
        off: u32,
        value: i64,
        kinds: u32,
        handle: *mut u32,
        flags: *mut u32,
    ) -> c_int;
    fn nvgpu_rs_i2_gem_in(
        call: *mut c_void,
        st: *mut c_void,
        ret: *mut i32,
        buf: u32,
        off: u32,
        guest: u32,
        owner: *mut u32,
        gem: *mut u32,
    ) -> c_int;
    fn nvgpu_rs_i2_fd_out(
        call: *mut c_void,
        st: *mut c_void,
        ret: *mut i32,
        buf: u32,
        off: u32,
        handle: u32,
        kind: u32,
        value: *mut i64,
    ) -> c_int;
    fn nvgpu_rs_i2_gem_out(
        call: *mut c_void,
        st: *mut c_void,
        ret: *mut i32,
        buf: u32,
        off: u32,
        gem: u32,
        size: u64,
        guest: *mut u32,
    ) -> c_int;
    fn nvgpu_rs_i2_special(call: *mut c_void, st: *mut c_void, ret: *mut i32, id: u32, phase: c_int) -> c_int;
    fn nvgpu_rs_i2_phase(call: *mut c_void, st: *mut c_void, ret: *mut i32, phase: c_int) -> c_int;
    fn nvgpu_rs_i2_close(call: *mut c_void, handle: u32);
    fn nvgpu_rs_i2_gem_close(call: *mut c_void, gem: u32);
    fn nvgpu_rs_i2_warn(call: *mut c_void, code: u32, name: *const u8, a: u32, b: u32, c: u32);
    fn nvgpu_rs_tbuf_alloc(len: usize) -> *mut c_void;
    fn nvgpu_rs_tbuf_free(tb: *mut c_void);
    fn nvgpu_rs_tbuf_write(tb: *mut c_void, off: usize, src: *const c_void, len: usize) -> c_int;
    fn nvgpu_rs_tbuf_read(tb: *const c_void, off: usize, dst: *mut c_void, len: usize) -> c_int;
    fn nvgpu_rs_i2_hand_over(req: *mut c_void, held: *mut *mut c_void);
    fn nvgpu_rs_held_release(held: *mut c_void);
    fn nvgpu_rs_i2_xfer(call: *mut c_void, req: *mut c_void, resp: *mut c_void, flags: u32, used: *mut u32) -> c_int;
}

// ───────────────────────── kernel buffers ─────────────────────────

/// A zeroed `kvzalloc()` buffer this side owns; freed on drop.
struct KBuf {
    p: *mut u8,
    len: usize,
}

impl KBuf {
    fn new(len: usize) -> Option<KBuf> {
        if len == 0 {
            return Some(KBuf { p: ptr::null_mut(), len: 0 });
        }
        // SAFETY: a plain allocation; NULL on failure, checked below.
        let p = unsafe { nvgpu_rs_kvzalloc(len) }.cast::<u8>();
        (!p.is_null()).then_some(KBuf { p, len })
    }
}

impl AsRef<[u8]> for KBuf {
    fn as_ref(&self) -> &[u8] {
        if self.p.is_null() {
            return &[];
        }
        // SAFETY: `p` is a live kvzalloc() allocation of `len` initialised
        // (zeroed) bytes, owned by this KBuf and borrowed through `self`.
        unsafe { core::slice::from_raw_parts(self.p, self.len) }
    }
}

impl AsMut<[u8]> for KBuf {
    fn as_mut(&mut self) -> &mut [u8] {
        if self.p.is_null() {
            return &mut [];
        }
        // SAFETY: as above, and `&mut self` makes this the only reference.
        unsafe { core::slice::from_raw_parts_mut(self.p, self.len) }
    }
}

impl Drop for KBuf {
    fn drop(&mut self) {
        if !self.p.is_null() {
            // SAFETY: allocated by nvgpu_rs_kvzalloc() and not freed before.
            unsafe { nvgpu_rs_kvfree(self.p.cast()) };
        }
    }
}

fn copy_from(kernel: bool, dst: &mut [u8], src: u64) -> Result<(), Errno> {
    // SAFETY: `dst` is a live, writable buffer of `dst.len()` bytes; the C
    // copies at most that many into it (copy_from_user, or memcpy for a
    // call the driver built from its own kernel memory).
    let r = unsafe { nvgpu_rs_copy_from(kernel, dst.as_mut_ptr().cast(), src, dst.len()) };
    if r == 0 {
        Ok(())
    } else {
        Err(-EFAULT)
    }
}

fn copy_to(kernel: bool, dst: u64, src: &[u8]) -> Result<(), Errno> {
    // SAFETY: `src` is `src.len()` readable bytes; the C copies them out.
    let r = unsafe { nvgpu_rs_copy_to(kernel, dst, src.as_ptr().cast(), src.len()) };
    if r == 0 {
        Ok(())
    } else {
        Err(-EFAULT)
    }
}

// ───────────────────────── RM escapes ─────────────────────────

/// The calling file, as the protocol-v1 paths see it.
struct RmEnv {
    nfd: *mut c_void,
    caps: u32,
}

impl RmEnv {
    /// # Safety
    ///
    /// `nfd` is the caller's `struct nvgpu_fd *`, live for the whole call.
    unsafe fn new(nfd: *mut c_void) -> RmEnv {
        // SAFETY: the caller's contract.
        let caps = unsafe { nvgpu_rs_caps(nfd) };
        RmEnv { nfd, caps }
    }
}

impl UserMem for RmEnv {
    fn copy_from_user(&mut self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        copy_from(false, dst, src)
    }

    fn copy_to_user(&mut self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        copy_to(false, dst, src)
    }
}

fn deep_control(c: &RsDeepControl) -> deep::Control {
    let mut out = deep::Control { cmd: c.cmd, nptrs: c.nptrs, ..deep::Control::default() };
    for (o, p) in out.ptrs.iter_mut().zip(c.ptrs.iter()) {
        let mut counts = [deep::Count::default(); deep::COUNTS_MAX];
        for (i, k) in counts.iter_mut().enumerate() {
            *k = deep::Count {
                offset: p.count_off.get(i).copied().unwrap_or(0) as u16,
                width: p.count_width.get(i).copied().unwrap_or(0) as u8,
            };
        }
        *o = deep::Ptr {
            ptr: p.ptr as u16,
            flags: p.flags as u8,
            ncounts: p.ncounts as u8,
            counts,
            scale: p.scale,
            elem: p.elem,
        };
    }
    out
}

impl rm::Env for RmEnv {
    type Buf = KBuf;

    fn alloc(&mut self, len: usize) -> Option<KBuf> {
        KBuf::new(len)
    }

    fn send_recv(&mut self, req: &[u8], resp: &mut [u8]) -> Result<u32, Errno> {
        let mut used = 0u32;
        // SAFETY: `req` and `resp` are live buffers of the lengths passed,
        // `nfd` the caller's live file; the transport copies through its
        // own buffers and writes at most `resp.len()` bytes back.
        let r = unsafe {
            nvgpu_rs_send_recv(
                self.nfd,
                req.as_ptr().cast(),
                req.len(),
                resp.as_mut_ptr().cast(),
                resp.len(),
                &mut used,
            )
        };
        if r < 0 {
            Err(r)
        } else {
            Ok(used)
        }
    }

    fn handle_for_fd(&mut self, fd: i32) -> Result<u32, Errno> {
        let mut h = 0u32;
        // SAFETY: fget()s a number of the calling process's and writes one
        // u32 through a pointer to a local; `nfd` is the live file.
        let r = unsafe { nvgpu_rs_handle_for_fd(self.nfd, fd, &mut h) };
        if r != 0 {
            Err(r)
        } else {
            Ok(h)
        }
    }

    fn proc_id(&mut self) -> [u8; PROC_ID_LEN] {
        let mut id = [0u8; PROC_ID_LEN];
        // SAFETY: writes sizeof(struct nvgpu_proc_id) == PROC_ID_LEN bytes.
        unsafe { nvgpu_rs_proc_id(self.nfd, id.as_mut_ptr().cast()) };
        id
    }

    fn caps(&self) -> rm::Caps {
        rm::Caps {
            deep_segs: self.caps & CAP_DEEP_SEGS != 0,
            proc_ids: self.caps & CAP_PROC_IDS != 0,
            proc_euid: self.caps & CAP_PROC_EUID != 0,
        }
    }

    fn handle(&self) -> u32 {
        // SAFETY: reads the live file's handle.
        unsafe { nvgpu_rs_fd_handle(self.nfd) }
    }

    fn driver_version(&self) -> &[u8] {
        let mut len = 0usize;
        // SAFETY: the device's driver_version[32], which lives as long as
        // the device the file holds a reference on; `len` is its strnlen.
        unsafe {
            let p = nvgpu_rs_driver_version(self.nfd, &mut len);
            if p.is_null() {
                return &[];
            }
            core::slice::from_raw_parts(p.cast::<u8>(), len)
        }
    }

    fn clock_to_guest(&mut self, clk: rm::Clock, host_ns: i64) -> Option<i64> {
        let mut g = 0i64;
        let raw = u32::from(clk == rm::Clock::MonotonicRaw);
        // SAFETY: reads the device's clock state, writes one i64 local.
        unsafe { nvgpu_rs_clock_to_guest(self.nfd, raw, host_ns, &mut g) }.then_some(g)
    }

    fn deep_control(&self, cmd: u32) -> Option<deep::Control> {
        let mut c = RsDeepControl::default();
        // SAFETY: fills one struct nvgpu_rs_deep_control local.
        unsafe { nvgpu_rs_rm_deep(cmd, false, &mut c) }.then(|| deep_control(&c))
    }

    fn deep_idle_channels(&self) -> deep::Control {
        let mut c = RsDeepControl::default();
        // SAFETY: as above.
        unsafe { nvgpu_rs_rm_deep(0, true, &mut c) };
        deep_control(&c)
    }

    fn v1v2(&self, cmd: u32) -> Option<rm::V1V2> {
        let (mut off, mut info) = (0u32, false);
        // SAFETY: reads the static generated table, writes two locals.
        unsafe { nvgpu_rs_v1v2(cmd, &mut off, &mut info) }
            .then_some(rm::V1V2 { v1_userptr_offset: off, info_style: info })
    }

    fn class_param_size(&self, hclass: u32) -> u32 {
        // SAFETY: a pure function of its argument.
        unsafe { nvgpu_rs_rmalloc_size(hclass) }
    }

    fn fd_translation(&self, key: u32) -> Option<u32> {
        let mut p = 0u32;
        // SAFETY: reads the device's config table, writes one local.
        unsafe { nvgpu_rs_fd_translation(self.nfd, key, &mut p) }.then_some(p)
    }

    fn uvm_size(&self, cmd: u32) -> Option<u32> {
        // SAFETY: reads the device's UVM table.
        u32::try_from(unsafe { nvgpu_rs_uvm_size(self.nfd, cmd) }).ok()
    }

    fn warn(&mut self, w: rm::Warn) {
        let (code, a, b) = match w {
            rm::Warn::ControlFd { cmd, fd } => (WARN_CONTROL_FD, u64::from(cmd), fd as u64),
            rm::Warn::ControlOsEvent { cmd, val } => (WARN_CONTROL_OS_EVENT, u64::from(cmd), val),
            rm::Warn::AllocEventFd { class, fd } => (WARN_ALLOC_EVENT_FD, u64::from(class), fd as u64),
            rm::Warn::EventBufferOsEvent { val } => (WARN_EVENT_BUFFER, 0, val),
            rm::Warn::SurfaceFd { fd } => (WARN_SURFACE_FD, 0, fd as u64),
        };
        // SAFETY: logs; reads the live file's device.
        unsafe { nvgpu_rs_warn(self.nfd, code, a, b) };
    }
}

/// One registration's pinned pages; unpinned on drop unless kept.
struct KPin {
    pages: *mut c_void,
    npages: u64,
    write: bool,
}

impl Drop for KPin {
    fn drop(&mut self) {
        if !self.pages.is_null() {
            // SAFETY: `pages` is the array nvgpu_rs_osdesc_pin() filled with
            // `npages` pinned pages, not yet kept or unpinned.
            unsafe { nvgpu_rs_osdesc_unpin(self.pages, self.npages, self.write) };
        }
    }
}

impl osdesc::Env for RmEnv {
    type Pin = KPin;

    fn os_desc(&self) -> bool {
        self.caps & CAP_OS_DESC != 0
    }

    fn reap(&mut self) {
        // SAFETY: process context, the live file's device.
        unsafe { nvgpu_rs_osdesc_reap(self.nfd) };
    }

    fn pin(&mut self, start: u64, npages: u64, write: bool) -> Result<KPin, PinError> {
        let mut pages = ptr::null_mut();
        // SAFETY: pins the calling process's own range, as RM would; the
        // array comes back through a pointer to a local.
        match unsafe { nvgpu_rs_osdesc_pin(start, npages, write, &mut pages) } {
            PIN_OK => Ok(KPin { pages, npages, write }),
            PIN_NOMEM => Err(PinError::NoMemory),
            _ => Err(PinError::NotPinned),
        }
    }

    fn page_phys(&self, pin: &KPin, i: u64) -> u64 {
        if i >= pin.npages {
            return 0;
        }
        // SAFETY: `i` is within the `npages` pinned pages of the array.
        unsafe { nvgpu_rs_page_phys(pin.pages, i) }
    }

    fn keep(&mut self, id: u64, mut pin: KPin) {
        let pages = core::mem::replace(&mut pin.pages, ptr::null_mut());
        // SAFETY: hands the pinned array to nvgpu_osdesc.c, which owns it
        // from here; `pin` no longer names it, so its drop does nothing.
        unsafe { nvgpu_rs_osdesc_keep(self.nfd, id, pages, pin.npages, pin.write) };
    }

    fn unpin(&mut self, pin: KPin) {
        drop(pin);
    }

    fn send_pinned(&mut self, req: &[u8], resp: &mut [u8], mut pin: KPin) -> (Result<u32, Errno>, Option<KPin>) {
        let mut used = 0u32;
        // SAFETY: as for send_recv; `pages` is the array
        // nvgpu_rs_osdesc_pin() filled with `npages` pinned pages, which
        // the call takes over only when it says so (-EINTR, -ETIMEDOUT).
        let r = unsafe {
            nvgpu_rs_osdesc_send(
                self.nfd,
                req.as_ptr().cast(),
                req.len(),
                resp.as_mut_ptr().cast(),
                resp.len(),
                &mut used,
                pin.pages,
                pin.npages,
                pin.write,
            )
        };
        if i2::abandons(r) {
            // The transport's now: `pin` no longer names them, so its drop
            // does nothing.
            pin.pages = ptr::null_mut();
            return (Err(r), None);
        }
        (if r < 0 { Err(r) } else { Ok(used) }, Some(pin))
    }
}

/// `nvgpu_ioctl_fd()`: an ioctl on one of our `/dev/nvidia*` files, or a
/// DRM file's driver range.
///
/// # Safety
///
/// `nfd` is a live `struct nvgpu_fd *` for the whole call; process context.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_ioctl_fd(nfd: *mut c_void, cmd: c_uint, arg: u64) -> c_long {
    // SAFETY: the caller's contract.
    let mut env = unsafe { RmEnv::new(nfd) };
    c_long::from(dispatch::ioctl_fd(&mut env, cmd, arg))
}

/// `nvgpu_uvm_ioctl_fd()`: a UVM command.
///
/// # Safety
///
/// As [`nvgpu_ioctl_fd`].
#[no_mangle]
pub unsafe extern "C" fn nvgpu_uvm_ioctl_fd(nfd: *mut c_void, cmd: c_uint, arg: u64) -> c_long {
    // SAFETY: the caller's contract.
    let mut env = unsafe { RmEnv::new(nfd) };
    c_long::from(dispatch::uvm_ioctl(&mut env, cmd, arg))
}

/// `nvgpu_ioctl_modeset()`: an NVKMS command to a v1 backend.
///
/// # Safety
///
/// As [`nvgpu_ioctl_fd`].
#[no_mangle]
pub unsafe extern "C" fn nvgpu_ioctl_modeset(nfd: *mut c_void, cmd: c_uint, uarg: *mut c_void) -> c_long {
    // SAFETY: the caller's contract.
    let mut env = unsafe { RmEnv::new(nfd) };
    c_long::from(rm::modeset_v1(&mut env, cmd, uarg as u64))
}

// ───────────────────────── IOCTL2 ─────────────────────────

/// The buffers of one IOCTL2 call. All-zero bytes are a valid empty store
/// (a `bool`, a raw pointer, and pairs of them), which `KState` relies on.
#[repr(C)]
pub struct KStore {
    kernel: bool,
    /// Buffer 0, the argument, is kernel memory: the DRM node's entry
    /// copied it in (`nvgpu_i2_call.karg`); what it points at is the
    /// caller's.
    karg: bool,
    /// What `nvgpu_i2_hold()` was given (`struct nvgpu_rs_held *`), until it
    /// goes with the request.
    held: *mut c_void,
    bufs: [(*mut u8, usize); I2_MAX_BUFS],
}

impl KStore {
    fn slot(&self, i: usize) -> Option<(*mut u8, usize)> {
        self.bufs.get(i).copied().filter(|(p, _)| !p.is_null())
    }

    /// Whether buffer `i`'s address is a kernel one (the C's `kb->kern`).
    fn kern(&self, i: usize) -> bool {
        self.kernel || (self.karg && i == 0)
    }
}

impl Store for KStore {
    fn alloc(&mut self, i: usize, len: usize) -> Result<(), Errno> {
        let slot = self.bufs.get_mut(i).ok_or(-EINVAL)?;
        if !slot.0.is_null() {
            return Err(-EINVAL);
        }
        // SAFETY: a plain allocation; NULL on failure, checked below.
        let p = unsafe { nvgpu_rs_kvzalloc(len) }.cast::<u8>();
        if p.is_null() {
            return Err(-ENOMEM);
        }
        *slot = (p, len);
        Ok(())
    }

    fn fetch(&mut self, i: usize, uptr: u64) -> Result<(), Errno> {
        let kernel = self.kern(i);
        copy_from(kernel, self.buf_mut(i), uptr)
    }

    fn buf(&self, i: usize) -> &[u8] {
        match self.slot(i) {
            // SAFETY: a live kvzalloc() allocation of `n` bytes this store
            // owns until KState::free(), borrowed through `self`.
            Some((p, n)) => unsafe { core::slice::from_raw_parts(p, n) },
            None => &[],
        }
    }

    fn buf_mut(&mut self, i: usize) -> &mut [u8] {
        match self.slot(i) {
            // SAFETY: as above, and `&mut self` makes this the only
            // reference Rust holds; a hook may hold the raw pointer
            // nvgpu_i2_buf() gave it only while no Rust reference is live.
            Some((p, n)) => unsafe { core::slice::from_raw_parts_mut(p, n) },
            None => &mut [],
        }
    }

    fn copy_out(&mut self, i: usize, uptr: u64, start: usize, end: usize) -> Result<(), Errno> {
        let kernel = self.kern(i);
        let src = self.buf(i).get(start..end).ok_or(-EFAULT)?;
        copy_to(kernel, uptr.wrapping_add(start as u64), src)
    }
}

/// One call's state.
type KState = State<KStore>;

/// A transport buffer; freed on drop unless the transport took it.
pub struct KTBuf(*mut c_void);

impl Drop for KTBuf {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: allocated by nvgpu_rs_tbuf_alloc() and still ours.
            unsafe { nvgpu_rs_tbuf_free(self.0) };
        }
    }
}

/// The hooks and the transport of one call.
struct I2Env {
    call: *mut c_void,
}

fn st_ptr(st: &mut KState) -> *mut c_void {
    ptr::from_mut(st).cast()
}

impl i2::Env<KStore> for I2Env {
    type TBuf = KTBuf;

    // Each hook is handed the state as a pointer derived from `st`, which is
    // not used again until the hook returns: the C reaches the kernel copies
    // only through that pointer (nvgpu_i2_buf() and friends), while no Rust
    // reference to them is live.

    fn fd_in(&mut self, st: &mut KState, buf: u32, off: u32, value: i64, kinds: u32) -> (i32, u32, u32) {
        let (mut h, mut f, mut ret) = (0u32, 0u32, st.ret);
        // SAFETY: `call` is the live call; see above for `st`; the rest are
        // locals the hook writes.
        let r = unsafe { nvgpu_rs_i2_fd_in(self.call, st_ptr(st), &mut ret, buf, off, value, kinds, &mut h, &mut f) };
        st.ret = ret;
        (r, h, f)
    }

    fn gem_in(&mut self, st: &mut KState, buf: u32, off: u32, guest: u32) -> (i32, u32, u32) {
        let (mut o, mut g, mut ret) = (0u32, 0u32, st.ret);
        // SAFETY: as for fd_in.
        let r = unsafe { nvgpu_rs_i2_gem_in(self.call, st_ptr(st), &mut ret, buf, off, guest, &mut o, &mut g) };
        st.ret = ret;
        (r, o, g)
    }

    fn fd_out(&mut self, st: &mut KState, buf: u32, off: u32, handle: u32, kind: u32) -> (i32, i64) {
        let (mut v, mut ret) = (-1i64, st.ret);
        // SAFETY: as for fd_in.
        let r = unsafe { nvgpu_rs_i2_fd_out(self.call, st_ptr(st), &mut ret, buf, off, handle, kind, &mut v) };
        st.ret = ret;
        (r, v)
    }

    fn gem_out(&mut self, st: &mut KState, buf: u32, off: u32, gem: u32, size: u64) -> (i32, u32) {
        let (mut g, mut ret) = (0u32, st.ret);
        // SAFETY: as for fd_in.
        let r = unsafe { nvgpu_rs_i2_gem_out(self.call, st_ptr(st), &mut ret, buf, off, gem, size, &mut g) };
        st.ret = ret;
        (r, g)
    }

    fn special(&mut self, st: &mut KState, id: u32, phase: i32) -> i32 {
        let mut ret = st.ret;
        // SAFETY: as for fd_in.
        let r = unsafe { nvgpu_rs_i2_special(self.call, st_ptr(st), &mut ret, id, phase) };
        st.ret = ret;
        r
    }

    fn phase(&mut self, st: &mut KState, phase: i32) -> i32 {
        let mut ret = st.ret;
        // SAFETY: as for fd_in.
        let r = unsafe { nvgpu_rs_i2_phase(self.call, st_ptr(st), &mut ret, phase) };
        st.ret = ret;
        r
    }

    fn close_handle(&mut self, handle: u32) {
        // SAFETY: queues a CLOSE on the live call's device.
        unsafe { nvgpu_rs_i2_close(self.call, handle) };
    }

    fn gem_close(&mut self, gem: u32) {
        // SAFETY: queues a GEM_CLOSE on the live call's device.
        unsafe { nvgpu_rs_i2_gem_close(self.call, gem) };
    }

    fn tbuf_alloc(&mut self, len: usize) -> Option<KTBuf> {
        // SAFETY: a plain allocation; NULL on failure, checked below.
        let p = unsafe { nvgpu_rs_tbuf_alloc(len) };
        (!p.is_null()).then_some(KTBuf(p))
    }

    fn tbuf_write(&mut self, tb: &mut KTBuf, off: usize, src: &[u8]) -> Result<(), Errno> {
        // SAFETY: `tb` is a live transport buffer; the C checks the range
        // against its length and reads `src.len()` bytes of `src`.
        let r = unsafe { nvgpu_rs_tbuf_write(tb.0, off, src.as_ptr().cast(), src.len()) };
        if r == 0 {
            Ok(())
        } else {
            Err(r)
        }
    }

    fn tbuf_read(&mut self, tb: &KTBuf, off: usize, dst: &mut [u8]) -> Result<(), Errno> {
        // SAFETY: as above, writing at most `dst.len()` bytes of `dst`.
        let r = unsafe { nvgpu_rs_tbuf_read(tb.0, off, dst.as_mut_ptr().cast(), dst.len()) };
        if r == 0 {
            Ok(())
        } else {
            Err(r)
        }
    }

    fn hand_over_held(&mut self, st: &mut KState, req: &mut KTBuf) {
        // SAFETY: `req` is a live request buffer with no release set yet;
        // the held list moves to it and `held` is cleared.
        unsafe { nvgpu_rs_i2_hand_over(req.0, &mut st.store.held) };
    }

    fn xfer(&mut self, req: KTBuf, resp: KTBuf, flags: u32) -> Xfer<KTBuf> {
        let mut used = 0u32;
        // SAFETY: both buffers are live and ours; on -ETIMEDOUT and -EINTR
        // the transport owns them, and they are forgotten here.
        let r = unsafe { nvgpu_rs_i2_xfer(self.call, req.0, resp.0, flags, &mut used) };
        if r == 0 {
            Xfer::Done { req, resp, used }
        } else if i2::abandons(r) {
            core::mem::forget(req);
            core::mem::forget(resp);
            Xfer::Abandoned { err: r }
        } else {
            Xfer::Failed { req, resp, err: r }
        }
    }

    fn warn(&mut self, w: i2::Warn<'_>) {
        let (code, e, a, b, c) = match w {
            i2::Warn::CmdSize { entry, cmd, size } => (WARN_I2_CMD, entry, cmd, entry.cmd, size),
            i2::Warn::Malformed { entry, used, nfd, ngem } => (WARN_I2_MALFORMED, entry, used, nfd, ngem),
            i2::Warn::Unnamed { entry } => (WARN_I2_UNNAMED, entry, 0, 0, 0),
        };
        // SAFETY: logs the entry's name, a static C string of the tables.
        unsafe { nvgpu_rs_i2_warn(self.call, code, e.name, a, b, c) };
    }
}

/// # Safety
///
/// `t` is one of `struct nvgpu_rs_tables`' tables, filled from the static
/// generated arrays, which live as long as the module.
unsafe fn table(t: &RsTable) -> Table<'static> {
    // SAFETY: each pointer is a static const C array of that many entries
    // (or NULL with none), whose layouts schema.rs and nvgpu_rs.h assert.
    unsafe {
        Table {
            ioctls: if t.ioctls.is_null() { &[] } else { core::slice::from_raw_parts(t.ioctls, t.nioctls as usize) },
            fields: if t.fields.is_null() { &[] } else { core::slice::from_raw_parts(t.fields, t.nfields as usize) },
            planes: if t.planes.is_null() { &[] } else { core::slice::from_raw_parts(t.planes, t.nplanes as usize) },
        }
    }
}

/// # Safety
///
/// `t` points at a filled `struct nvgpu_rs_tables`.
unsafe fn schema_set(t: *const RsTables) -> SchemaSet<'static> {
    // SAFETY: the caller's contract; the tables it names are static.
    unsafe {
        let t = &*t;
        SchemaSet { drm: table(&t.drm), modeset: (t.has_modeset != 0).then(|| table(&t.modeset)) }
    }
}

/// Free a state: every buffer, whatever is still held, the state itself.
///
/// # Safety
///
/// `p` came from [`nvgpu_rs_i2_ioctl`]'s allocation and nothing refers to
/// it any more.
unsafe fn free_state(p: *mut KState) {
    // SAFETY: the caller's contract.
    let st = unsafe { &mut *p };
    if !st.store.held.is_null() {
        // SAFETY: the held list nvgpu_i2_hold() built, not handed over.
        unsafe { nvgpu_rs_held_release(st.store.held) };
        st.store.held = ptr::null_mut();
    }
    for (b, _) in st.store.bufs.iter_mut() {
        if !b.is_null() {
            // SAFETY: allocated by nvgpu_rs_kvzalloc(), freed once.
            unsafe { nvgpu_rs_kvfree(b.cast()) };
            *b = ptr::null_mut();
        }
    }
    // SAFETY: the state's own allocation.
    unsafe { nvgpu_rs_kvfree(p.cast()) };
}

/// `nvgpu_i2_ioctl()` after its v2 check: run one IOCTL2.
///
/// # Safety
///
/// `call` is a live `struct nvgpu_i2_call *`, `a` and `t` point at filled
/// `struct nvgpu_rs_i2_args` / `nvgpu_rs_tables`, `ret_out` at an `s32`;
/// process context.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_ioctl(
    call: *mut c_void,
    a: *const c_void,
    t: *const c_void,
    ret_out: *mut i32,
) -> c_long {
    // SAFETY: the caller's contract.
    let (a, set) = unsafe { (&*a.cast::<RsI2Args>(), schema_set(t.cast())) };
    let args = i2::Args {
        sclass: a.sclass,
        cmd: a.cmd,
        uarg: a.uarg,
        handle: a.handle,
        render: a.render,
        xflags: a.xflags,
        compat: a.compat != 0,
        max_req: a.max_req,
        max_resp: a.max_resp,
    };
    // Some 70 KiB: on the heap, zeroed, which is a valid empty state (see
    // i2::State and KStore).
    // SAFETY: a plain allocation; NULL on failure, checked below.
    let p = unsafe { nvgpu_rs_kvzalloc(core::mem::size_of::<KState>()) }.cast::<KState>();
    if p.is_null() {
        return c_long::from(-ENOMEM);
    }
    // SAFETY: kvzalloc() memory is aligned for any kernel object (at least
    // 8 bytes, which is KState's alignment), zeroed, and ours alone.
    let st = unsafe { &mut *p };
    st.store.kernel = a.kernel != 0;
    st.store.karg = a.karg != 0;
    let mut env = I2Env { call };
    let r = i2::run(&mut env, st, &set, &args);
    // SAFETY: `ret_out` is the caller's s32.
    unsafe { *ret_out = st.ret };
    // SAFETY: the call is over and nothing refers to the state any more;
    // the C clears call->st.
    unsafe { free_state(p) };
    c_long::from(r)
}

/// `nvgpu_i2_has_schema()`.
///
/// # Safety
///
/// `t` points at a filled `struct nvgpu_rs_tables`, `prefix` at
/// `prefix_len` readable bytes (or is NULL).
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_has_schema(
    t: *const c_void,
    sclass: u32,
    cmd: u32,
    prefix: *const c_void,
    prefix_len: usize,
) -> bool {
    // SAFETY: the caller's contract.
    let set = unsafe { schema_set(t.cast()) };
    let prefix = if prefix.is_null() {
        &[][..]
    } else {
        // SAFETY: the caller's contract: kernel memory it copied.
        unsafe { core::slice::from_raw_parts(prefix.cast::<u8>(), prefix_len) }
    };
    i2::has_schema(&set, sclass, cmd, prefix)
}

/// `nvgpu_i2_native_cmd()`.
///
/// # Safety
///
/// `t` points at a filled `struct nvgpu_rs_tables`.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_native_cmd(t: *const c_void, sclass: u32, cmd: u32) -> u32 {
    // SAFETY: the caller's contract.
    let set = unsafe { schema_set(t.cast()) };
    i2::native_cmd(&set, sclass, cmd)
}

/// The state a hook was handed, or `None`.
///
/// # Safety
///
/// `st` is NULL or the pointer the running hook was handed as `call->st`.
unsafe fn hook_state<'a>(st: *mut c_void) -> Option<&'a mut KState> {
    // SAFETY: the caller's contract: while the hook runs, the interpreter
    // holds no reference to the state (see I2Env), so this one is unique.
    unsafe { st.cast::<KState>().as_mut() }
}

/// `nvgpu_i2_add_dyn()`.
///
/// # Safety
///
/// `st` is NULL or `call->st` of a running hook.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_add_dyn(st: *mut c_void, kind: u32, buf: u32, off: u32, len: u32) -> c_int {
    // SAFETY: the caller's contract.
    match unsafe { hook_state(st) } {
        Some(st) => st.add_dyn(kind, buf, off, len),
        None => -EINVAL,
    }
}

/// `nvgpu_i2_add_fd()`.
///
/// # Safety
///
/// As [`nvgpu_rs_i2_add_dyn`].
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_add_fd(st: *mut c_void, buf: u32, off: u32, handle: u32, flags: u32) -> c_int {
    // SAFETY: the caller's contract.
    match unsafe { hook_state(st) } {
        Some(st) => st.add_fd(buf, off, handle, flags),
        None => -EINVAL,
    }
}

/// `nvgpu_i2_buf()`: the kernel copy of buffer `buf` (NULL if none, or
/// empty), valid while the hook runs.
///
/// # Safety
///
/// As [`nvgpu_rs_i2_add_dyn`]; `len` points at a `u32`.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_buf(st: *mut c_void, buf: u32, len: *mut u32) -> *mut c_void {
    // SAFETY: the caller's contract.
    let b = unsafe { hook_state(st) }.and_then(|st| st.buf_mut(buf)).filter(|b| !b.is_empty());
    let (p, n) = match b {
        Some(b) => (b.as_mut_ptr().cast::<c_void>(), b.len() as u32),
        None => (ptr::null_mut(), 0),
    };
    // SAFETY: the caller's u32.
    unsafe { *len = n };
    p
}

/// Where the state keeps what `nvgpu_i2_hold()` was given.
///
/// # Safety
///
/// As [`nvgpu_rs_i2_add_dyn`].
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_i2_held(st: *mut c_void) -> *mut *mut c_void {
    // SAFETY: the caller's contract.
    match unsafe { hook_state(st) } {
        Some(st) => &mut st.store.held,
        None => ptr::null_mut(),
    }
}

// ───────────────────────── ATOMIC ─────────────────────────

/// `struct nvgpu_atomic_ops`: nvgpu_kms.c's hooks for the parse.
#[repr(C)]
pub struct AtomicOps {
    obj_class: unsafe extern "C" fn(ctx: *mut c_void, obj: u32, crtc: *mut u32) -> c_int,
    prop_class: unsafe extern "C" fn(ctx: *mut c_void, id: u32) -> c_int,
    in_fence: unsafe extern "C" fn(ctx: *mut c_void, st: *mut c_void, buf: u32, off: u32, fd: i64) -> c_int,
    out_fence: unsafe extern "C" fn(ctx: *mut c_void, st: *mut c_void, buf: u32, off: u32, uptr: u64) -> c_int,
    learn: unsafe extern "C" fn(ctx: *mut c_void, obj: u32, crtc: u32),
    reserve: unsafe extern "C" fn(ctx: *mut c_void, crtc: u32, user_data: u64) -> c_int,
}

/// `struct nvgpu_atomic_out`.
#[repr(C)]
pub struct AtomicOut {
    commit: bool,
    values_buf: u32,
}

// The layouts nvgpu_rs_glue.c asserts on the C side.
const _: () = assert!(core::mem::size_of::<AtomicOut>() == 8);
const _: () = assert!(core::mem::size_of::<AtomicOps>() == 6 * core::mem::size_of::<usize>());

/// The hooks of one parse, and the C's `struct nvgpu_atomic_out`, which a
/// hook may read (through its context) while the parse runs: only ever
/// written through the raw pointer, never held as a reference across a hook.
struct AtomicEnv<'a> {
    ops: &'a AtomicOps,
    ctx: *mut c_void,
    out: *mut AtomicOut,
}

impl atomic::Env<KStore> for AtomicEnv<'_> {
    fn begin(&mut self, commit: bool) {
        // SAFETY: `out` is the caller's live struct nvgpu_atomic_out (the
        // nvgpu_rs_atomic_parse contract); no reference to it is held.
        unsafe { ptr::addr_of_mut!((*self.out).commit).write(commit) };
    }

    fn obj_class(&mut self, obj: u32) -> (i32, u32) {
        let mut crtc = 0u32;
        // SAFETY: nvgpu_kms.c's hook, on the context it passed with it,
        // writing one u32 local.
        let r = unsafe { (self.ops.obj_class)(self.ctx, obj, &mut crtc) };
        (r, crtc)
    }

    fn prop_class(&mut self, id: u32) -> i32 {
        // SAFETY: as above.
        unsafe { (self.ops.prop_class)(self.ctx, id) }
    }

    fn in_fence(&mut self, st: &mut KState, buf: u32, off: u32, fd: i64) -> i32 {
        // SAFETY: as above; the hook reaches the kernel copies only through
        // the state pointer it is handed, derived from `st`, which is not
        // used again until it returns (see I2Env).
        unsafe { (self.ops.in_fence)(self.ctx, st_ptr(st), buf, off, fd) }
    }

    fn out_fence(&mut self, st: &mut KState, buf: u32, off: u32, uptr: u64) -> i32 {
        // SAFETY: as for in_fence.
        unsafe { (self.ops.out_fence)(self.ctx, st_ptr(st), buf, off, uptr) }
    }

    fn learn(&mut self, obj: u32, crtc: u32) {
        // SAFETY: as for obj_class.
        unsafe { (self.ops.learn)(self.ctx, obj, crtc) }
    }

    fn reserve(&mut self, crtc: u32, user_data: u64) -> i32 {
        // SAFETY: as for obj_class.
        unsafe { (self.ops.reserve)(self.ctx, crtc, user_data) }
    }
}

/// `nvgpu_atomic_parse()`, on the state the ATOMIC special was handed.
///
/// # Safety
///
/// `st` is NULL or `call->st` of a running special hook; `ops` points at a
/// filled `struct nvgpu_atomic_ops` whose hooks accept `ctx`; `out` at a
/// `struct nvgpu_atomic_out`.
#[no_mangle]
pub unsafe extern "C" fn nvgpu_rs_atomic_parse(
    st: *mut c_void,
    fences: bool,
    ops: *const c_void,
    ctx: *mut c_void,
    out: *mut c_void,
) -> c_int {
    let out = out.cast::<AtomicOut>();
    // SAFETY: the caller's contract.
    let (Some(st), Some(ops)) = (unsafe { hook_state(st) }, unsafe { ops.cast::<AtomicOps>().as_ref() }) else {
        return -EINVAL;
    };
    if out.is_null() {
        return -EINVAL;
    }
    // SAFETY: `out` is a live struct nvgpu_atomic_out; read and written by
    // value, as the hooks may read it while the parse runs (AtomicEnv).
    let mut o = unsafe { atomic::Out { commit: ptr::addr_of!((*out).commit).read(), values_buf: ptr::addr_of!((*out).values_buf).read() } };
    let mut env = AtomicEnv { ops, ctx, out };
    let r = atomic::parse(st, &mut env, fences, &mut o);
    // SAFETY: as above.
    unsafe {
        ptr::addr_of_mut!((*out).commit).write(o.commit);
        ptr::addr_of_mut!((*out).values_buf).write(o.values_buf);
    }
    r
}
