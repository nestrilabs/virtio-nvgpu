// SPDX-License-Identifier: GPL-2.0-only
//! The C side: the module's parsers and the harness, as Rust sees them, and
//! the `dt_*` callbacks through which the harness reaches the world of the
//! run in progress.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr};

use crate::backend;
use crate::hooks::{self, CallBufs};
use crate::world::{Ev, World};

/// `struct nvgpu_device`, opaque.
#[repr(C)]
pub struct CDev {
    _p: [u8; 0],
}

/// `struct nvgpu_fd`, opaque.
#[repr(C)]
pub struct CFd {
    _p: [u8; 0],
}

/// `struct harness_tables` (the layout of `struct nvgpu_rs_tables`).
#[repr(C)]
pub struct CTable {
    pub ioctls: *const nvgpu_guest_core::guest::schema::SIoctl,
    pub nioctls: u64,
    pub fields: *const nvgpu_guest_core::guest::schema::SField,
    pub nfields: u64,
    pub planes: *const u8,
    pub nplanes: u64,
}

#[repr(C)]
pub struct CTables {
    pub drm: CTable,
    pub modeset: CTable,
    pub has_modeset: u32,
    pub reserved: u32,
}

/// `struct nvgpu_rs_deep_control`'s layout.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CDeepPtr {
    pub ptr: u32,
    pub flags: u32,
    pub ncounts: u32,
    pub count_off: [u32; 2],
    pub count_width: [u32; 2],
    pub scale: u32,
    pub elem: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct CDeep {
    pub cmd: u32,
    pub nptrs: u32,
    pub ptrs: [CDeepPtr; 4],
}

extern "C" {
    pub fn harness_dev(
        version: *const c_char,
        v2: bool,
        caps: u32,
        max_req: u32,
        max_resp: u32,
        fdt_nr: *const u32,
        fdt_payload: *const u32,
        nfdt: u32,
    ) -> *mut CDev;
    pub fn harness_dev_free(dev: *mut CDev);
    pub fn harness_fd(dev: *mut CDev, handle: u32) -> *mut CFd;
    pub fn harness_fd_free(nfd: *mut CFd);
    pub fn harness_tables(dev: *const CDev, t: *mut CTables);
    pub fn harness_uvm_size(dev: *const CDev, cmd: u32) -> c_int;
    pub fn harness_rm_deep(cmd: u32, idle: bool, out: *mut CDeep) -> bool;
    pub fn harness_v1v2(cmd: u32, off: *mut u32, info: *mut bool) -> bool;
    pub fn harness_rmalloc_size(hclass: u32) -> u32;
    pub fn harness_call_ret(call: *mut c_void) -> i32;
    pub fn harness_set_kmalloc_fill(fill: c_int);
    pub fn harness_dev_bad_schema(dev: *mut CDev);
    pub fn harness_set_call_ret(call: *mut c_void, ret: i32);
    #[allow(clippy::too_many_arguments)]
    pub fn harness_i2(
        dev: *mut CDev,
        sclass: u32,
        cmd: u32,
        uarg: u64,
        handle: u32,
        render: u32,
        xflags: u32,
        kernel: bool,
        karg: bool,
        mask: u32,
        ret_out: *mut i32,
    ) -> c_long;

    pub fn nvgpu_ioctl_fd(nfd: *mut CFd, cmd: c_uint, arg: u64) -> c_long;
    pub fn nvgpu_uvm_ioctl_fd(nfd: *mut CFd, cmd: c_uint, arg: u64) -> c_long;
    pub fn nvgpu_ioctl_modeset(nfd: *mut CFd, cmd: c_uint, uarg: u64) -> c_long;
    pub fn nvgpu_i2_has_schema(
        dev: *mut CDev,
        sclass: u32,
        cmd: c_uint,
        prefix: *const c_void,
        len: usize,
    ) -> bool;
    pub fn nvgpu_i2_native_cmd(dev: *mut CDev, sclass: u32, cmd: c_uint) -> c_uint;
    pub fn nvgpu_fd_kind_allowed(device_type: u32, kinds: u32) -> bool;
    fn nvgpu_i2_buf(call: *mut c_void, buf: u32, len: *mut u32) -> *mut u8;
    fn nvgpu_i2_add_dyn(call: *mut c_void, kind: u32, buf: u32, off: u32, len: u32) -> c_int;
    fn nvgpu_i2_add_fd(call: *mut c_void, buf: u32, off: u32, handle: u32, flags: u32) -> c_int;
    fn harness_atomic(
        call: *mut c_void,
        fences: bool,
        ctx: *mut c_void,
        commit: *mut bool,
        values_buf: *mut u32,
    ) -> c_int;
    fn harness_atomic_commit() -> bool;
}

thread_local! {
    /// The world of the C run in progress on this thread.
    static CUR: RefCell<Option<World>> = const { RefCell::new(None) };
}

/// Run `f` (C) in world `w`; the world after it.
pub fn in_world<R>(w: World, f: impl FnOnce() -> R) -> (R, World) {
    CUR.with(|c| *c.borrow_mut() = Some(w));
    let r = f();
    let w = CUR.with(|c| c.borrow_mut().take()).expect("world");
    (r, w)
}

fn with<R>(f: impl FnOnce(&mut World) -> R) -> R {
    CUR.with(|c| f(c.borrow_mut().as_mut().expect("a C call outside a world")))
}

/// # Safety
/// C passes `n` writable bytes at `to`.
#[no_mangle]
pub unsafe extern "C" fn dt_copy_from_user(to: *mut u8, from: u64, n: usize) -> c_int {
    let dst = unsafe { std::slice::from_raw_parts_mut(to, n) };
    if with(|w| w.copy_from_user(dst, from)) {
        0
    } else {
        -14
    }
}

/// # Safety
/// C passes `n` readable bytes at `from`.
#[no_mangle]
pub unsafe extern "C" fn dt_copy_to_user(to: u64, from: *const u8, n: usize) -> c_int {
    let src = unsafe { std::slice::from_raw_parts(from, n) };
    if with(|w| w.copy_to_user(to, src)) {
        0
    } else {
        -14
    }
}

/// # Safety
/// C passes `n` writable bytes at `to`.
#[no_mangle]
pub unsafe extern "C" fn dt_kread(to: *mut u8, from: u64, n: usize) {
    let dst = unsafe { std::slice::from_raw_parts_mut(to, n) };
    with(|w| w.kread(dst, from));
}

/// # Safety
/// C passes `n` readable bytes at `from`.
#[no_mangle]
pub unsafe extern "C" fn dt_kwrite(to: u64, from: *const u8, n: usize) {
    let src = unsafe { std::slice::from_raw_parts(from, n) };
    with(|w| w.kwrite(to, src));
}

/// # Safety
/// As the transport's contract.
#[no_mangle]
pub unsafe extern "C" fn dt_send_recv(
    req: *const u8,
    req_len: usize,
    resp: *mut u8,
    resp_len: usize,
    used: *mut u32,
) -> c_int {
    let (req, resp) = unsafe {
        (
            std::slice::from_raw_parts(req, req_len),
            std::slice::from_raw_parts_mut(resp, resp_len),
        )
    };
    match with(|w| backend::serve(w, req, resp)) {
        Ok(u) => {
            unsafe { *used = u };
            0
        }
        Err(e) => e,
    }
}

/// # Safety
/// As [`dt_send_recv`].
#[no_mangle]
pub unsafe extern "C" fn dt_xfer(
    req: *const u8,
    req_len: usize,
    resp: *mut u8,
    resp_len: usize,
    flags: u32,
    used: *mut u32,
) -> c_int {
    with(|w| w.events.push(Ev::XferFlags(flags)));
    unsafe { dt_send_recv(req, req_len, resp, resp_len, used) }
}

/// # Safety
/// `handle` is writable.
#[no_mangle]
pub unsafe extern "C" fn dt_handle_for_fd(fd: c_int, handle: *mut u32) -> c_int {
    match with(|w| w.handle_for_fd(fd)) {
        Ok(h) => {
            unsafe { *handle = h };
            0
        }
        Err(e) => e,
    }
}

/// # Safety
/// 16 writable bytes at `dst`.
#[no_mangle]
pub unsafe extern "C" fn dt_proc_id(dst: *mut u8) {
    let id = with(|w| w.proc_id);
    unsafe { std::ptr::copy_nonoverlapping(id.as_ptr(), dst, 16) };
}

/// # Safety
/// `guest` is writable.
#[no_mangle]
pub unsafe extern "C" fn dt_clock(raw: bool, host: i64, guest: *mut i64) -> bool {
    match with(|w| w.clock_to_guest(raw, host)) {
        Some(g) => {
            unsafe { *guest = g };
            true
        }
        None => false,
    }
}

#[no_mangle]
pub extern "C" fn dt_reap() {
    with(|w| w.events.push(Ev::Reap));
}

/// # Safety
/// `npages` writable u64s at `phys`.
#[no_mangle]
pub unsafe extern "C" fn dt_pin(start: u64, npages: u64, write: bool, phys: *mut u64) -> c_int {
    match with(|w| w.pin(start, npages, write)) {
        Some(v) => {
            unsafe { std::ptr::copy_nonoverlapping(v.as_ptr(), phys, v.len()) };
            0
        }
        None => -14,
    }
}

#[no_mangle]
pub extern "C" fn dt_keep(id: u64, n: u64, write: bool) {
    with(|w| w.events.push(Ev::Keep { id, n, write }));
}

#[no_mangle]
pub extern "C" fn dt_unpin(n: u64, write: bool) {
    with(|w| w.events.push(Ev::Unpin { n, write }));
}

#[no_mangle]
pub extern "C" fn dt_hand_over(n: u64, write: bool) {
    with(|w| w.events.push(Ev::HandOver { n, write }));
}

/// # Safety
/// `fmt` is a C string.
#[no_mangle]
pub unsafe extern "C" fn dt_warn(fmt: *const c_char) {
    let s = unsafe { CStr::from_ptr(fmt) }
        .to_string_lossy()
        .into_owned();
    with(|w| w.events.push(Ev::Warn(s)));
}

#[no_mangle]
pub extern "C" fn dt_compat() -> bool {
    with(|w| w.compat)
}

#[no_mangle]
pub extern "C" fn dt_close(h: u32) {
    with(|w| w.events.push(Ev::Close(h)));
}

#[no_mangle]
pub extern "C" fn dt_gem_close(file: u32, gem: u32) {
    with(|w| w.events.push(Ev::GemClose(file, gem)));
}

#[no_mangle]
pub extern "C" fn dt_canary() {
    with(|w| w.canary = true);
}

/// A C call's buffers, for the hooks.
struct CCall(*mut c_void);

impl CallBufs for CCall {
    fn buf(&mut self, b: u32) -> Option<&mut [u8]> {
        let mut len = 0u32;
        let p = unsafe { nvgpu_i2_buf(self.0, b, &mut len) };
        if p.is_null() {
            return None;
        }
        Some(unsafe { std::slice::from_raw_parts_mut(p, len as usize) })
    }

    fn add_dyn(&mut self, kind: u32, buf: u32, off: u32, len: u32) -> i32 {
        unsafe { nvgpu_i2_add_dyn(self.0, kind, buf, off, len) }
    }

    fn add_fd(&mut self, buf: u32, off: u32, handle: u32, flags: u32) -> i32 {
        unsafe { nvgpu_i2_add_fd(self.0, buf, off, handle, flags) }
    }

    fn ret(&self) -> i32 {
        unsafe { harness_call_ret(self.0) }
    }

    fn set_ret(&mut self, r: i32) {
        unsafe { harness_set_call_ret(self.0, r) }
    }

    fn atomic(&mut self, w: &mut World, fences: bool) -> (i32, bool, u32) {
        let mut ctx = ACtx { w, call: self.0 };
        let (mut commit, mut values_buf) = (false, 0u32);
        let r = unsafe {
            harness_atomic(
                self.0,
                fences,
                (&raw mut ctx).cast(),
                &mut commit,
                &mut values_buf,
            )
        };
        (r, commit, values_buf)
    }
}

/// What the C atomic parse's hooks get as their context.
struct ACtx<'a> {
    w: &'a mut World,
    call: *mut c_void,
}

fn actx<'a>(ctx: *mut c_void) -> &'a mut ACtx<'a> {
    unsafe { &mut *ctx.cast::<ACtx<'a>>() }
}

/// # Safety
/// `crtc` is writable.
#[no_mangle]
pub unsafe extern "C" fn dt_a_obj(ctx: *mut c_void, obj: u32, crtc: *mut u32) -> c_int {
    let (r, c) = hooks::a_obj(actx(ctx).w, obj);
    unsafe { *crtc = c };
    r
}

#[no_mangle]
pub extern "C" fn dt_a_prop(ctx: *mut c_void, id: u32) -> c_int {
    hooks::a_prop(actx(ctx).w, id)
}

#[no_mangle]
pub extern "C" fn dt_a_in_fence(
    ctx: *mut c_void,
    _st: *mut c_void,
    buf: u32,
    off: u32,
    fd: i64,
) -> c_int {
    let a = actx(ctx);
    // What the parse has said about the commit by now.
    let commit = unsafe { harness_atomic_commit() };
    hooks::a_in_fence(a.w, &mut CCall(a.call), buf, off, fd, commit)
}

#[no_mangle]
pub extern "C" fn dt_a_out_fence(
    ctx: *mut c_void,
    _st: *mut c_void,
    buf: u32,
    off: u32,
    uptr: u64,
) -> c_int {
    let a = actx(ctx);
    hooks::a_out_fence(a.w, &mut CCall(a.call), buf, off, uptr)
}

#[no_mangle]
pub extern "C" fn dt_a_learn(ctx: *mut c_void, obj: u32, crtc: u32) {
    hooks::a_learn(actx(ctx).w, obj, crtc)
}

#[no_mangle]
pub extern "C" fn dt_a_reserve(ctx: *mut c_void, crtc: u32, user_data: u64) -> c_int {
    hooks::a_reserve(actx(ctx).w, crtc, user_data)
}

/// # Safety
/// The hook's out-parameters are writable.
#[no_mangle]
pub unsafe extern "C" fn dt_fd_in(
    _call: *mut c_void,
    buf: u32,
    off: u32,
    v: i64,
    kinds: u32,
    handle: *mut u32,
    flags: *mut u32,
) -> c_int {
    let (r, h, f) = with(|w| hooks::fd_in(w, buf, off, v, kinds));
    unsafe {
        *handle = h;
        *flags = f;
    }
    r
}

/// # Safety
/// As [`dt_fd_in`].
#[no_mangle]
pub unsafe extern "C" fn dt_gem_in(
    _call: *mut c_void,
    buf: u32,
    off: u32,
    guest: u32,
    owner: *mut u32,
    gem: *mut u32,
) -> c_int {
    let (r, o, g) = with(|w| hooks::gem_in(w, buf, off, guest));
    unsafe {
        *owner = o;
        *gem = g;
    }
    r
}

/// # Safety
/// As [`dt_fd_in`].
#[no_mangle]
pub unsafe extern "C" fn dt_fd_out(
    _call: *mut c_void,
    buf: u32,
    off: u32,
    handle: u32,
    kind: u32,
    v: *mut i64,
) -> c_int {
    let (r, x) = with(|w| hooks::fd_out(w, buf, off, handle, kind));
    unsafe { *v = x };
    r
}

/// # Safety
/// As [`dt_fd_in`].
#[no_mangle]
pub unsafe extern "C" fn dt_gem_out(
    _call: *mut c_void,
    buf: u32,
    off: u32,
    gem: u32,
    size: u64,
    guest: *mut u32,
) -> c_int {
    let (r, g) = with(|w| hooks::gem_out(w, buf, off, gem, size));
    unsafe { *guest = g };
    r
}

/// The world is taken out while a hook reaches into the C call's buffers,
/// which calls back into C but never into the world.
fn with_call<R>(call: *mut c_void, f: impl FnOnce(&mut World, &mut dyn CallBufs) -> R) -> R {
    let mut w = CUR.with(|c| c.borrow_mut().take()).expect("world");
    let r = f(&mut w, &mut CCall(call));
    CUR.with(|c| *c.borrow_mut() = Some(w));
    r
}

#[no_mangle]
pub extern "C" fn dt_special(call: *mut c_void, id: u32, phase: c_int) -> c_int {
    with_call(call, |w, c| hooks::special(w, c, id, phase))
}

#[no_mangle]
pub extern "C" fn dt_phase(call: *mut c_void, phase: c_int) -> c_int {
    with_call(call, |w, c| hooks::phase(w, c, phase))
}
