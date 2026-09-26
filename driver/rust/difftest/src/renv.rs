//! The Rust side: the core's traits implemented over the same world the C
//! harness serves, doing what `driver/nvgpu_rs.rs` and `nvgpu_rs_glue.c` do
//! in the kernel. The module's own tables (deep controls, V1V2 rewrites,
//! RM_ALLOC class sizes, UVM sizes) come from the same C the kernel uses.

use std::cell::RefCell;
use std::rc::Rc;

use nvgpu_guest_core::guest::atomic;
use nvgpu_guest_core::guest::deep::{self, UserMem};
use nvgpu_guest_core::guest::i2::{self, State, Store, Xfer};
use nvgpu_guest_core::guest::osdesc::{self, PinError};
use nvgpu_guest_core::guest::rm;
use nvgpu_guest_core::guest::schema::{SchemaSet, Table};
use nvgpu_guest_core::guest::wire::Errno;

use crate::backend;
use crate::cabi::{self, CDeep, CDev, CTable, CTables};
use crate::hooks::{self, CallBufs};
use crate::world::{Ev, World};

/// `NVGPU_BCAP_*` bits the paths read.
pub const BCAP_DEEP_SEGS: u32 = 1 << 5;
pub const BCAP_PROC_ID: u32 = 1 << 8;
pub const BCAP_OS_DESC: u32 = 1 << 7;
pub const BCAP_PROC_EUID: u32 = 1 << 9;

/// The device, as the Rust side sees it.
#[derive(Clone, Debug)]
pub struct Dev {
    pub dev: *mut CDev,
    pub v2: bool,
    pub caps: u32,
    pub version: String,
    pub fdt: Vec<(u32, u32)>,
    pub handle: u32,
}

/// The protocol-v1 paths' environment.
pub struct RmEnv<'a> {
    pub w: &'a mut World,
    pub d: &'a Dev,
}

impl UserMem for RmEnv<'_> {
    fn copy_from_user(&mut self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        if self.w.copy_from_user(dst, src) {
            Ok(())
        } else {
            Err(-14)
        }
    }

    fn copy_to_user(&mut self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        if self.w.copy_to_user(dst, src) {
            Ok(())
        } else {
            Err(-14)
        }
    }
}

fn deep(c: &CDeep) -> deep::Control {
    let mut out = deep::Control { cmd: c.cmd, nptrs: c.nptrs, ..Default::default() };
    for (o, p) in out.ptrs.iter_mut().zip(c.ptrs.iter()) {
        *o = deep::Ptr {
            ptr: p.ptr as u16,
            flags: p.flags as u8,
            ncounts: p.ncounts as u8,
            counts: [
                deep::Count { offset: p.count_off[0] as u16, width: p.count_width[0] as u8 },
                deep::Count { offset: p.count_off[1] as u16, width: p.count_width[1] as u8 },
            ],
            scale: p.scale,
            elem: p.elem,
        };
    }
    out
}

/// The log line each warning is in C, by its format string.
pub fn rm_warn_fmt(w: rm::Warn) -> &'static str {
    match w {
        rm::Warn::ControlFd { .. } => {
            "virtio-gpu-nv: RM control 0x%x names fd %d, which is not one of our devices\n"
        }
        rm::Warn::ControlOsEvent { .. } => {
            "virtio-gpu-nv: RM control 0x%x names OS event 0x%llx, which is not one of our devices\n"
        }
        rm::Warn::AllocEventFd { .. } => {
            "virtio-gpu-nv: RM_ALLOC of event class 0x%x names fd %d, which is not one of our devices\n"
        }
        rm::Warn::EventBufferOsEvent { .. } => {
            "virtio-gpu-nv: NV_EVENT_BUFFER names OS event 0x%llx, which is not one of our devices\n"
        }
        rm::Warn::SurfaceFd { .. } => "virtio-gpu-nv: REGISTER_SURFACE names fd %d, which is not one of ours\n",
    }
}

impl rm::Env for RmEnv<'_> {
    type Buf = Vec<u8>;

    fn alloc(&mut self, len: usize) -> Option<Vec<u8>> {
        Some(vec![0; len])
    }

    fn send_recv(&mut self, req: &[u8], resp: &mut [u8]) -> Result<u32, Errno> {
        resp.fill(0);
        backend::serve(self.w, req, resp)
    }

    fn handle_for_fd(&mut self, fd: i32) -> Result<u32, Errno> {
        if fd < 0 {
            return Err(-9);
        }
        self.w.handle_for_fd(fd)
    }

    fn proc_id(&mut self) -> [u8; 16] {
        self.w.proc_id
    }

    fn caps(&self) -> rm::Caps {
        let c = self.d.caps;
        let proc_ids = self.d.v2 && c & BCAP_PROC_ID != 0;
        rm::Caps {
            deep_segs: self.d.v2 && c & BCAP_DEEP_SEGS != 0,
            proc_ids,
            proc_euid: proc_ids && c & BCAP_PROC_EUID != 0,
        }
    }

    fn handle(&self) -> u32 {
        self.d.handle
    }

    fn driver_version(&self) -> &[u8] {
        let v = self.d.version.as_bytes();
        &v[..v.len().min(31)]
    }

    fn clock_to_guest(&mut self, clk: rm::Clock, host_ns: i64) -> Option<i64> {
        self.w.clock_to_guest(clk == rm::Clock::MonotonicRaw, host_ns)
    }

    fn deep_control(&self, cmd: u32) -> Option<deep::Control> {
        let mut c = CDeep::default();
        unsafe { cabi::harness_rm_deep(cmd, false, &mut c) }.then(|| deep(&c))
    }

    fn deep_idle_channels(&self) -> deep::Control {
        let mut c = CDeep::default();
        unsafe { cabi::harness_rm_deep(0, true, &mut c) };
        deep(&c)
    }

    fn v1v2(&self, cmd: u32) -> Option<rm::V1V2> {
        let (mut off, mut info) = (0u32, false);
        unsafe { cabi::harness_v1v2(cmd, &mut off, &mut info) }
            .then_some(rm::V1V2 { v1_userptr_offset: off, info_style: info })
    }

    fn class_param_size(&self, hclass: u32) -> u32 {
        unsafe { cabi::harness_rmalloc_size(hclass) }
    }

    fn fd_translation(&self, key: u32) -> Option<u32> {
        self.d.fdt.iter().take(16).find(|(nr, _)| *nr == key).map(|&(_, p)| p)
    }

    fn uvm_size(&self, cmd: u32) -> Option<u32> {
        u32::try_from(unsafe { cabi::harness_uvm_size(self.d.dev, cmd) }).ok()
    }

    fn warn(&mut self, w: rm::Warn) {
        self.w.events.push(Ev::Warn(rm_warn_fmt(w).to_string()));
    }
}

/// Pinned pages: their physical addresses.
pub struct RPin {
    pas: Vec<u64>,
    write: bool,
}

impl osdesc::Env for RmEnv<'_> {
    type Pin = RPin;

    fn os_desc(&self) -> bool {
        self.d.v2 && self.d.caps & BCAP_OS_DESC != 0
    }

    fn reap(&mut self) {
        self.w.events.push(Ev::Reap);
    }

    fn pin(&mut self, start: u64, npages: u64, write: bool) -> Result<RPin, PinError> {
        self.w.pin(start, npages, write).map(|pas| RPin { pas, write }).ok_or(PinError::NotPinned)
    }

    fn page_phys(&self, pin: &RPin, i: u64) -> u64 {
        pin.pas.get(i as usize).copied().unwrap_or(0)
    }

    fn keep(&mut self, id: u64, pin: RPin) {
        self.w.events.push(Ev::Keep { id, n: pin.pas.len() as u64, write: pin.write });
    }

    fn unpin(&mut self, pin: RPin) {
        self.w.events.push(Ev::Unpin { n: pin.pas.len() as u64, write: pin.write });
    }

    fn send_pinned(&mut self, req: &[u8], resp: &mut [u8], pin: RPin) -> (Result<u32, Errno>, Option<RPin>) {
        let r = rm::Env::send_recv(self, req, resp);
        match r {
            Err(e) if i2::abandons(e) => {
                self.w.events.push(Ev::HandOver { n: pin.pas.len() as u64, write: pin.write });
                (r, None)
            }
            _ => (r, Some(pin)),
        }
    }
}

/// The schema set the C device selected.
pub fn tables(d: &Dev) -> SchemaSet<'static> {
    let mut t = std::mem::MaybeUninit::<CTables>::zeroed();
    unsafe { cabi::harness_tables(d.dev, t.as_mut_ptr()) };
    let t = unsafe { t.assume_init() };
    fn table(t: &CTable) -> Table<'static> {
        unsafe {
            Table {
                ioctls: std::slice::from_raw_parts(t.ioctls, t.nioctls as usize),
                fields: std::slice::from_raw_parts(t.fields, t.nfields as usize),
                planes: if t.planes.is_null() { &[] } else { std::slice::from_raw_parts(t.planes, t.nplanes as usize) },
            }
        }
    }
    SchemaSet { drm: table(&t.drm), modeset: (t.has_modeset != 0).then(|| table(&t.modeset)) }
}

/// An IOCTL2's buffers, over the shared world.
pub struct RStore {
    w: Rc<RefCell<World>>,
    bufs: Vec<Option<Vec<u8>>>,
}

impl RStore {
    pub fn new(w: Rc<RefCell<World>>) -> RStore {
        RStore { w, bufs: vec![None; 256] }
    }
}

impl Store for RStore {
    fn alloc(&mut self, i: usize, len: usize) -> Result<(), Errno> {
        let s = self.bufs.get_mut(i).ok_or(-22)?;
        if s.is_some() {
            return Err(-22);
        }
        *s = Some(vec![0; len]);
        Ok(())
    }

    fn fetch(&mut self, i: usize, uptr: u64) -> Result<(), Errno> {
        let w = self.w.clone();
        let b = self.bufs.get_mut(i).and_then(|b| b.as_mut()).ok_or(-22)?;
        if w.borrow_mut().copy_from_user(b, uptr) {
            Ok(())
        } else {
            Err(-14)
        }
    }

    fn buf(&self, i: usize) -> &[u8] {
        self.bufs.get(i).and_then(|b| b.as_deref()).unwrap_or(&[])
    }

    fn buf_mut(&mut self, i: usize) -> &mut [u8] {
        self.bufs.get_mut(i).and_then(|b| b.as_deref_mut()).unwrap_or(&mut [])
    }

    fn copy_out(&mut self, i: usize, uptr: u64, start: usize, end: usize) -> Result<(), Errno> {
        let src = self.buf(i).get(start..end).ok_or(-14)?.to_vec();
        if self.w.borrow_mut().copy_to_user(uptr.wrapping_add(start as u64), &src) {
            Ok(())
        } else {
            Err(-14)
        }
    }
}

/// The IOCTL2 environment: the hooks and the transport.
pub struct I2Env {
    pub w: Rc<RefCell<World>>,
    pub render: u32,
}

struct RCall<'a>(&'a mut State<RStore>);

impl CallBufs for RCall<'_> {
    fn buf(&mut self, b: u32) -> Option<&mut [u8]> {
        self.0.buf_mut(b).filter(|k| !k.is_empty())
    }

    fn add_dyn(&mut self, kind: u32, buf: u32, off: u32, len: u32) -> i32 {
        self.0.add_dyn(kind, buf, off, len)
    }

    fn add_fd(&mut self, buf: u32, off: u32, handle: u32, flags: u32) -> i32 {
        self.0.add_fd(buf, off, handle, flags)
    }

    fn ret(&self) -> i32 {
        self.0.ret
    }

    fn set_ret(&mut self, r: i32) {
        self.0.ret = r;
    }

    fn atomic(&mut self, w: &mut World, fences: bool) -> (i32, bool, u32) {
        let mut out = atomic::Out::default();
        let r = atomic::parse(self.0, &mut AEnv { w, commit: false }, fences, &mut out);
        (r, out.commit, out.values_buf)
    }
}

/// The atomic parse's hooks, over the world, and what the parse has said
/// about the commit (`begin`), as the kernel's hook reads it.
struct AEnv<'a> {
    w: &'a mut World,
    commit: bool,
}

impl atomic::Env<RStore> for AEnv<'_> {
    fn begin(&mut self, commit: bool) {
        self.commit = commit;
    }
    fn obj_class(&mut self, obj: u32) -> (i32, u32) {
        hooks::a_obj(self.w, obj)
    }
    fn prop_class(&mut self, id: u32) -> i32 {
        hooks::a_prop(self.w, id)
    }
    fn in_fence(&mut self, st: &mut State<RStore>, buf: u32, off: u32, fd: i64) -> i32 {
        hooks::a_in_fence(self.w, &mut RCall(st), buf, off, fd, self.commit)
    }
    fn out_fence(&mut self, st: &mut State<RStore>, buf: u32, off: u32, uptr: u64) -> i32 {
        hooks::a_out_fence(self.w, &mut RCall(st), buf, off, uptr)
    }
    fn learn(&mut self, obj: u32, crtc: u32) {
        hooks::a_learn(self.w, obj, crtc)
    }
    fn reserve(&mut self, crtc: u32, user_data: u64) -> i32 {
        hooks::a_reserve(self.w, crtc, user_data)
    }
}

impl I2Env {
    fn has(&self, bit: u32) -> bool {
        hooks::has(&self.w.borrow(), bit)
    }

    /// Record what the backend's reply should be shaped by, once gathered.
    fn record_shape(&self, st: &State<RStore>) {
        use nvgpu_guest_core::guest::schema::{SF_FD_OUT, SF_GEM_OUT};
        let mut fd_out: Vec<(u32, u32)> = st.positions(SF_FD_OUT).collect();
        fd_out.extend(st.dyn_positions());
        let shape = crate::world::I2Shape {
            bufs: st.buffers().collect(),
            fd_out,
            gem_out: st.positions(SF_GEM_OUT).collect(),
        };
        self.w.borrow_mut().shape = Some(shape);
    }
}

impl i2::Env<RStore> for I2Env {
    type TBuf = Vec<u8>;

    fn fd_in(&mut self, _st: &mut State<RStore>, buf: u32, off: u32, value: i64, kinds: u32) -> (i32, u32, u32) {
        if !self.has(0) {
            return (-22, 0, 0);
        }
        hooks::fd_in(&mut self.w.borrow_mut(), buf, off, value, kinds)
    }

    fn gem_in(&mut self, _st: &mut State<RStore>, buf: u32, off: u32, guest: u32) -> (i32, u32, u32) {
        if !self.has(1) {
            return (-22, 0, 0);
        }
        hooks::gem_in(&mut self.w.borrow_mut(), buf, off, guest)
    }

    fn fd_out(&mut self, _st: &mut State<RStore>, buf: u32, off: u32, handle: u32, kind: u32) -> (i32, i64) {
        if !self.has(2) {
            return (-22, -1);
        }
        hooks::fd_out(&mut self.w.borrow_mut(), buf, off, handle, kind)
    }

    fn gem_out(&mut self, _st: &mut State<RStore>, buf: u32, off: u32, gem: u32, size: u64) -> (i32, u32) {
        if !self.has(3) {
            return (-22, 0);
        }
        hooks::gem_out(&mut self.w.borrow_mut(), buf, off, gem, size)
    }

    fn special(&mut self, st: &mut State<RStore>, id: u32, phase: i32) -> i32 {
        if !self.has(4) {
            return 0;
        }
        let mut w = std::mem::take(&mut *self.w.borrow_mut());
        let r = hooks::special(&mut w, &mut RCall(st), id, phase);
        *self.w.borrow_mut() = w;
        r
    }

    fn phase(&mut self, st: &mut State<RStore>, phase: i32) -> i32 {
        if phase == 0 {
            // Once the call is gathered, translated and specialled: what
            // the backend will answer to.
            self.record_shape_later(st);
        }
        if !self.has(5) {
            return 0;
        }
        let mut w = std::mem::take(&mut *self.w.borrow_mut());
        let r = hooks::phase(&mut w, &mut RCall(st), phase);
        *self.w.borrow_mut() = w;
        r
    }

    fn close_handle(&mut self, handle: u32) {
        self.w.borrow_mut().events.push(Ev::Close(handle));
    }

    fn gem_close(&mut self, gem: u32) {
        self.w.borrow_mut().events.push(Ev::GemClose(self.render, gem));
    }

    fn tbuf_alloc(&mut self, len: usize) -> Option<Vec<u8>> {
        (len != 0).then(|| vec![0; len])
    }

    fn tbuf_write(&mut self, tb: &mut Vec<u8>, off: usize, src: &[u8]) -> Result<(), Errno> {
        let end = off.checked_add(src.len()).ok_or(-22)?;
        tb.get_mut(off..end).ok_or(-22)?.copy_from_slice(src);
        Ok(())
    }

    fn tbuf_read(&mut self, tb: &Vec<u8>, off: usize, dst: &mut [u8]) -> Result<(), Errno> {
        let end = off.checked_add(dst.len()).ok_or(-22)?;
        dst.copy_from_slice(tb.get(off..end).ok_or(-22)?);
        Ok(())
    }

    fn hand_over_held(&mut self, _st: &mut State<RStore>, _req: &mut Vec<u8>) {}

    fn xfer(&mut self, req: Vec<u8>, mut resp: Vec<u8>, flags: u32) -> Xfer<Vec<u8>> {
        self.w.borrow_mut().events.push(Ev::XferFlags(flags));
        resp.fill(0);
        match backend::serve(&mut self.w.borrow_mut(), &req, &mut resp) {
            Ok(used) => Xfer::Done { req, resp, used },
            Err(e) if i2::abandons(e) => Xfer::Abandoned { err: e },
            Err(e) => Xfer::Failed { req, resp, err: e },
        }
    }

    fn warn(&mut self, w: i2::Warn<'_>) {
        let fmt = match w {
            i2::Warn::CmdSize { .. } => "virtio-gpu-nv: IOCTL2 %s: caller's ioctl 0x%08x is not 0x%08x (size %u)\n",
            i2::Warn::Malformed { .. } => {
                "virtio-gpu-nv: IOCTL2 %s: malformed reply (%u bytes, %u fds, %u GEM handles)\n"
            }
            i2::Warn::Unnamed { .. } => {
                "virtio-gpu-nv: IOCTL2 %s: the reply names a descriptor or GEM handle where the schema has none\n"
            }
        };
        self.w.borrow_mut().events.push(Ev::Warn(fmt.to_string()));
    }
}

impl I2Env {
    fn record_shape_later(&self, st: &State<RStore>) {
        self.record_shape(st);
    }
}
