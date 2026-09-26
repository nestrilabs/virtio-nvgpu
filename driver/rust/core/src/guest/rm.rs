// SPDX-License-Identifier: GPL-2.0-only
//! The protocol-v1 IOCTL message: RM escapes (flat ones, RM_CONTROL with its
//! nested block, its intercepts and deep pointers, RM_ALLOC, IDLE_CHANNELS),
//! ioctls carrying a descriptor at a fixed offset, and a v1 backend's NVKMS
//! commands. Ports of `nvgpu_ioctl_simple()`, `nvgpu_ioctl_rm_control()`,
//! `nvgpu_ioctl_rm_alloc()`, `nvgpu_ioctl_idle_channels()`,
//! `nvgpu_ioctl_translate_fd()` and `nvgpu_ioctl_modeset()`, whose C stays
//! the reference (`driver/nvgpu_rmio.c`) until the Rust build has passed the
//! hardware regression.
//!
//! Each reads every byte of the caller's it uses once: the argument, the
//! nested block and each deep segment are copied in by [`deep::UserMem`]
//! into buffers this code owns, every decision is taken on those copies, and
//! the copy that was decided on is the one that is sent. What goes back to
//! the caller is the reply, with the caller's own descriptors put back where
//! this code replaced them.

use super::deep::{self, UserMem};
use super::wire::{
    has, ioctl_req_header, le32, le64, put32, put64, sum, Errno, FillFrom, IoctlResp,
    DEEP_SEGMENTED, EBADF, EFAULT, EINVAL, EIO, ENOMEM, ENOTTY, EPERM, IDLE_CHANNELS_MAX,
    IOCTL_REQ_LEN, IOCTL_RESP_LEN, PROC_ID_LEN,
};

/// NV_IOCTL_MAGIC, the type byte of every RM escape.
pub const RM_IOCTL_TYPE: u32 = b'F' as u32;
/// `NV_ESC_RM_DUP_OBJECT`.
pub const ESC_RM_DUP_OBJECT: u32 = 0x34;

/// Largest nested block (`paramsSize`, `dataSize`) carried for one call.
pub const NESTED_MAX: u32 = 1024 * 1024;
/// `NVGPU_DEEP_MAX`: the largest single deep pointer's buffer.
pub const DEEP_MAX: u32 = 64 * 1024;

/// `sizeof(struct NVOS54_PARAMETERS)`.
pub const NVOS54_SIZE: usize = 32;
/// `sizeof(struct NVOS64_PARAMETERS)`.
pub const NVOS64_SIZE: usize = 48;
/// NVOS30 (IDLE_CHANNELS), `NVGPU_RM_IDLE_CHANNELS_SIZE`.
pub const IDLE_CHANNELS_SIZE: usize = 56;
/// `NVGPU_RM_IDLE_CHANNELS_FLAGS`.
const IDLE_CHANNELS_FLAGS: usize = 40;
/// `NVGPU_RM_IDLE_CHANNELS_LIST_LO` / `_HI` / `LIST`.
const IDLE_CHANNELS_LIST_LO: u32 = 4;
const IDLE_CHANNELS_LIST_HI: u32 = 7;
const IDLE_CHANNELS_LIST: u32 = 0;
/// The NVKMS outer struct: `{u32 cmd, u32 dataSize, u64 pData}`.
pub const NVKMS_OUTER_SIZE: usize = 16;

/// Where RM's status sits in NVOS54 (RM_CONTROL).
const NVOS54_STATUS: usize = 28;

/// RM controls that name an open file by descriptor inside their parameters
/// (ctrl0000unix.h).
const RM_EXPORT_OBJECT_TO_FD: u32 = 0x0000_3d05;
const RM_IMPORT_OBJECT_FROM_FD: u32 = 0x0000_3d06;
const RM_GET_EXPORT_OBJECT_INFO: u32 = 0x0000_3d08;
const RM_CREATE_EXPORT_OBJECT_FD: u32 = 0x0000_3d0a;
const RM_EXPORT_OBJECTS_TO_FD: u32 = 0x0000_3d0b;
const RM_IMPORT_OBJECTS_FROM_FD: u32 = 0x0000_3d0c;

/// Semaphore surface waiters, which name an OS event by descriptor.
const RM_SEMSURF_REGISTER_WAITER: u32 = 0x00da_0003;
const RM_SEMSURF_UNREGISTER_WAITER: u32 = 0x00da_0005;

/// Event classes whose allocation parameters name a file of the caller's,
/// in NV0005_ALLOC_PARAMETERS.data at 16.
const CLASS_EVENT: u32 = 0x05;
const CLASS_EVENT_OS_EVENT: u32 = 0x79;
const NV0005_DATA_OFFSET: usize = 16;
/// NV_EVENT_BUFFER's notificationHandle.
const CLASS_EVENT_BUFFER: u32 = 0x90cd;
const EVENT_BUFFER_NOTIFICATION_OFFSET: usize = 40;

/// NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION, answered here.
const RM_GET_BUILD_VERSION: u32 = 0x0000_0101;
/// NV2080_CTRL_CMD_TIMER_GET_GPU_CPU_TIME_CORRELATION_INFO.
pub const RM_TIME_CORRELATION: u32 = 0x2080_0406;
const TCI_CLK_ID: usize = 0;
const TCI_SAMPLE_COUNT: usize = 1;
const TCI_SAMPLES: usize = 8;
const TCI_SAMPLE_SIZE: usize = 16;
const TCI_MAX_SAMPLES: u8 = 16;
const TCI_SRC_OSTIME: u8 = 1;
const TCI_SRC_TSC: u8 = 2;
const TCI_SRC_PLATFORM_API: u8 = 3;
const TCI_PROC_CPU: u8 = 0;
const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;

/// NvKmsIoctlCommand REGISTER_SURFACE, and where its descriptor is.
const NVKMS_REGISTER_SURFACE: u32 = 16;
const NVKMS_SURFACE_FD_OFFSET: usize = 16;

/// The title GET_BUILD_VERSION reports, NUL included.
const BUILD_TITLE: &[u8] = b"NVIDIA UNIX Open Kernel Module\0";

/// What the device offers, as the paths here need it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// `nvgpu_deep_segs_ok()`: v2 with `NVGPU_BCAP_DEEP_SEGS`.
    pub deep_segs: bool,
    /// `nvgpu_proc_ids()`: RM_ALLOC and RM_DUP_OBJECT carry the process.
    pub proc_ids: bool,
    /// `nvgpu_proc_euid()`: RM_CONTROL carries it too.
    pub proc_euid: bool,
}

/// A host clock whose readings RM reports and the guest rebases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clock {
    /// `CLOCK_REALTIME`.
    Realtime,
    /// `CLOCK_MONOTONIC_RAW`.
    MonotonicRaw,
}

/// A generated V1-to-V2 rewrite entry (`struct nvgpu_v1v2_entry`), of which
/// only these two fields are read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct V1V2 {
    /// Offset of the NvP64 in the nested block.
    pub v1_userptr_offset: u32,
    /// The leading count is entries of 8 bytes, not bytes.
    pub info_style: bool,
}

/// Something worth a rate-limited line in the log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Warn {
    /// An RM control named a descriptor that is not one of ours.
    ControlFd {
        /// The control.
        cmd: u32,
        /// The descriptor.
        fd: i32,
    },
    /// An RM control named an OS event that is not one of ours.
    ControlOsEvent {
        /// The control.
        cmd: u32,
        /// The value.
        val: u64,
    },
    /// An event allocation named a descriptor that is not one of ours.
    AllocEventFd {
        /// The class.
        class: u32,
        /// The descriptor.
        fd: i32,
    },
    /// NV_EVENT_BUFFER named an OS event that is not one of ours.
    EventBufferOsEvent {
        /// The value.
        val: u64,
    },
    /// REGISTER_SURFACE named a descriptor that is not one of ours.
    SurfaceFd {
        /// The descriptor.
        fd: i32,
    },
}

/// The module around these paths: the caller's memory, the transport, the
/// descriptor table, and the generated RM tables.
pub trait Env: UserMem {
    /// A kernel buffer; dropping one frees it.
    type Buf: AsRef<[u8]> + AsMut<[u8]>;

    /// `len` zeroed bytes, or `None` (the C paths' -ENOMEM).
    fn alloc(&mut self, len: usize) -> Option<Self::Buf>;
    /// `nvgpu_send_recv_used()`: send `req`, answer into `resp` (which
    /// reads as zero past what the device wrote); the bytes it wrote, or a
    /// negative errno.
    fn send_recv(&mut self, req: &[u8], resp: &mut [u8]) -> Result<u32, Errno>;
    /// `nvgpu_handle_for_fd()`: the backend's handle for one of our files.
    fn handle_for_fd(&mut self, fd: i32) -> Result<u32, Errno>;
    /// `nvgpu_proc_id_fill()`: the calling process, as the wire has it.
    fn proc_id(&mut self) -> [u8; PROC_ID_LEN];
    /// What the device offers.
    fn caps(&self) -> Caps;
    /// The calling file's backend handle.
    fn handle(&self) -> u32;
    /// The host driver's version string, without its NUL.
    fn driver_version(&self) -> &[u8];
    /// `nvgpu_host_clock_to_guest()`.
    fn clock_to_guest(&mut self, clk: Clock, host_ns: i64) -> Option<i64>;
    /// `nvgpu_rm_deep_find()`, when the device takes deep segments.
    fn deep_control(&self, cmd: u32) -> Option<deep::Control>;
    /// `nvgpu_rm_deep_idle_channels`.
    fn deep_idle_channels(&self) -> deep::Control;
    /// `nvgpu_find_v1v2_rewrite()`, then the hand-kept deep-only table.
    fn v1v2(&self, cmd: u32) -> Option<V1V2>;
    /// `nvgpu_rmalloc_class_param_size()`.
    fn class_param_size(&self, hclass: u32) -> u32;
    /// The config's descriptor-translation entry whose `nr` is `key`: its
    /// `payload_offset`.
    fn fd_translation(&self, key: u32) -> Option<u32>;
    /// The UVM command's parameter block size on the host's release
    /// (`nvgpu_uvm_size()`); `None` for one the backend does not let through.
    fn uvm_size(&self, cmd: u32) -> Option<u32>;
    /// `dev_warn_ratelimited()`.
    fn warn(&mut self, w: Warn);
}

/// `nvgpu_rm_deep_only_table`: commands with a second-level pointer and no
/// V2 twin, which the generated table cannot see. Only NV0041's
/// GET_SURFACE_INFO: `{u32 surfaceInfoListSize, pad, NvP64 surfaceInfoList}`,
/// entries of eight bytes.
pub fn deep_only(cmd: u32) -> Option<V1V2> {
    (cmd == 0x0041_0110).then_some(V1V2 {
        v1_userptr_offset: 8,
        info_style: true,
    })
}

fn ioc_type(cmd: u32) -> u32 {
    (cmd >> 8) & 0xff
}

fn ioc_nr(cmd: u32) -> u32 {
    cmd & 0xff
}

/// Allocate and zero a buffer of `len`, or -ENOMEM.
fn alloc<E: Env + ?Sized>(env: &mut E, len: usize) -> Result<E::Buf, Errno> {
    env.alloc(len).ok_or(-ENOMEM)
}

/// `buf[at..at + len]`, or -EFAULT for a range the buffer does not have
/// (never: every buffer here is sized for what goes in it).
fn part(buf: &mut [u8], at: usize, len: usize) -> Result<&mut [u8], Errno> {
    let end = at.checked_add(len).ok_or(-EFAULT)?;
    buf.get_mut(at..end).ok_or(-EFAULT)
}

fn part_ref(buf: &[u8], at: usize, len: usize) -> Result<&[u8], Errno> {
    let end = at.checked_add(len).ok_or(-EFAULT)?;
    buf.get(at..end).ok_or(-EFAULT)
}

/// Send `req` and read the reply's header; -EIO for a reply shorter than a
/// header, as every path here has it.
fn round_trip<E: Env + ?Sized>(
    env: &mut E,
    req: &[u8],
    resp: &mut [u8],
) -> Result<(IoctlResp, usize), Errno> {
    let used = env.send_recv(req, resp)?;
    let h = IoctlResp::parse(resp, used).ok_or(-EIO)?;
    Ok((h, used as usize))
}

/// `nvgpu_ioctl_simple()`: a flat struct, no embedded pointers.
pub fn simple<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, sz: u32) -> i32 {
    let mut data = match alloc(env, sz as usize) {
        Ok(b) => b,
        Err(e) => return e,
    };
    if sz > 0 {
        if let Err(e) = env.copy_from_user(data.as_mut(), uarg) {
            return e;
        }
    }
    simple_bytes(env, cmd, uarg, data.as_ref())
}

/// [`simple`] for an argument already copied in (`data`, `_IOC_SIZE`
/// bytes), which is what is sent.
pub fn simple_bytes<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, data: &[u8]) -> i32 {
    match simple_inner(env, cmd, uarg, data) {
        Ok(r) | Err(r) => r,
    }
}

fn simple_inner<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    data: &[u8],
) -> Result<i32, Errno> {
    let sz = data.len();
    // RM_DUP_OBJECT carries the calling process after the struct.
    let proc =
        ioc_type(cmd) == RM_IOCTL_TYPE && ioc_nr(cmd) == ESC_RM_DUP_OBJECT && env.caps().proc_ids;
    let req_total = sum(&[IOCTL_REQ_LEN, sz, if proc { PROC_ID_LEN } else { 0 }]);
    let resp_max = sum(&[IOCTL_RESP_LEN, sz]);
    let mut req = alloc(env, req_total)?;
    let mut resp = alloc(env, resp_max)?;
    {
        let r = req.as_mut();
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            sz as u32,
            0,
            0,
            0,
            0,
        ));
        part(r, IOCTL_REQ_LEN, sz)?.fill_from(data);
        if proc {
            let id = env.proc_id();
            part(r, sum(&[IOCTL_REQ_LEN, sz]), PROC_ID_LEN)?.fill_from(&id);
        }
    }
    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;
    // Only what the device wrote: a failed call comes back as a bare header.
    let data_len = h.data_len as usize;
    if sz > 0 && data_len != 0 && data_len <= sz && has(used, IOCTL_RESP_LEN, data_len) {
        let back = part_ref(resp.as_ref(), IOCTL_RESP_LEN, data_len)?;
        if env.copy_to_user_failed(uarg, back) {
            ret = -EFAULT;
        }
    }
    Ok(ret)
}

/// A copy-out with the C paths' shape: `true` when it faulted.
trait CopyOut: UserMem {
    fn copy_to_user_failed(&mut self, dst: u64, src: &[u8]) -> bool {
        self.copy_to_user(dst, src).is_err()
    }
}

impl<T: UserMem + ?Sized> CopyOut for T {}

/// `nvgpu_ioctl_translate_fd()`: an ioctl the device's config names as
/// carrying a descriptor at `payload_offset` of its argument. The caller's
/// descriptor becomes the backend's handle for that file on the way out
/// (-1 passes as it is, any other negative is refused), and comes back on
/// every reply that carries the struct.
pub fn translate_fd<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    sz: u32,
    payload_offset: u32,
) -> i32 {
    match translate_fd_inner(env, cmd, uarg, sz, payload_offset, None) {
        Ok(r) | Err(r) => r,
    }
}

/// [`translate_fd`] for an argument already copied in (`data`, `_IOC_SIZE`
/// bytes), which is what is sent.
pub fn translate_fd_bytes<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    data: &[u8],
    payload_offset: u32,
) -> i32 {
    match translate_fd_inner(
        env,
        cmd,
        uarg,
        data.len() as u32,
        payload_offset,
        Some(data),
    ) {
        Ok(r) | Err(r) => r,
    }
}

fn translate_fd_inner<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    sz: u32,
    payload_offset: u32,
    data: Option<&[u8]>,
) -> Result<i32, Errno> {
    if u64::from(sz) < u64::from(payload_offset).saturating_add(4) {
        return Err(-EINVAL);
    }
    let sz = sz as usize;
    let po = payload_offset as usize;
    let mut req = alloc(env, sum(&[IOCTL_REQ_LEN, sz]))?;
    let mut resp = alloc(env, sum(&[IOCTL_RESP_LEN, sz]))?;
    let guest_fd;
    {
        let r = req.as_mut();
        match data {
            Some(d) => part(r, IOCTL_REQ_LEN, sz)?.fill_from(d),
            None => env.copy_from_user(part(r, IOCTL_REQ_LEN, sz)?, uarg)?,
        }
        guest_fd = le32(r, sum(&[IOCTL_REQ_LEN, po])).ok_or(-EINVAL)? as i32;
        // -1 is "no descriptor" and is forwarded unchanged; any other
        // negative number is no descriptor either, and sent as it is the
        // backend would read it as a handle of its own.
        if guest_fd < -1 {
            return Err(-EBADF);
        }
        if guest_fd >= 0 {
            let handle = env.handle_for_fd(guest_fd)?;
            put32(r, sum(&[IOCTL_REQ_LEN, po]), handle);
        }
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            sz as u32,
            0,
            0,
            0,
            0,
        ));
    }
    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;
    let data_len = h.data_len as usize;
    if ret == 0 && data_len > 0 && data_len <= sz && has(used, IOCTL_RESP_LEN, data_len) {
        let r = resp.as_mut();
        if data_len >= po.saturating_add(4) {
            put32(r, sum(&[IOCTL_RESP_LEN, po]), guest_fd as u32);
        }
        if env.copy_to_user_failed(uarg, part_ref(r, IOCTL_RESP_LEN, data_len)?) {
            ret = -EFAULT;
        }
    }
    Ok(ret)
}

/// Where an OS_UNIX control keeps its descriptor (`nvgpu_rm_unix_fd_offset`).
fn unix_fd_offset(ctl: u32) -> Option<usize> {
    match ctl {
        RM_EXPORT_OBJECT_TO_FD => Some(16),
        RM_CREATE_EXPORT_OBJECT_FD => Some(72),
        RM_IMPORT_OBJECT_FROM_FD
        | RM_GET_EXPORT_OBJECT_INFO
        | RM_EXPORT_OBJECTS_TO_FD
        | RM_IMPORT_OBJECTS_FROM_FD => Some(0),
        _ => None,
    }
}

/// Where an RM control keeps an OS event (`nvgpu_rm_os_event_offset`).
fn os_event_offset(ctl: u32) -> Option<usize> {
    match ctl {
        RM_SEMSURF_REGISTER_WAITER => Some(24),
        RM_SEMSURF_UNREGISTER_WAITER => Some(16),
        _ => None,
    }
}

/// `nvgpu_rm_os_event_in()`: an OS event named by descriptor in the u64 at
/// `slot`, swapped for the backend's handle for that file. Zero is "no
/// notification" and passes as it is. Returns the caller's value, which the
/// reply gets back, and -EBADF (with it) for a number that is not ours.
fn os_event_in<E: Env + ?Sized>(env: &mut E, slot: &mut [u8]) -> Result<u64, (Errno, u64)> {
    let v = le64(slot, 0).ok_or((-EBADF, 0))?;
    if v == 0 {
        return Ok(v);
    }
    if v > i32::MAX as u64 {
        return Err((-EBADF, v));
    }
    let handle = env.handle_for_fd(v as i32).map_err(|_| (-EBADF, v))?;
    put64(slot, 0, u64::from(handle));
    Ok(v)
}

/// `nvgpu_set_nvos54_status()`: RM's status in the caller's NVOS54.
fn set_status<E: Env + ?Sized>(env: &mut E, uarg: u64, status: u32) -> i32 {
    match env.copy_to_user(
        uarg.wrapping_add(NVOS54_STATUS as u64),
        &status.to_le_bytes(),
    ) {
        Ok(()) => 0,
        Err(_) => -EFAULT,
    }
}

/// `nvgpu_intercept_get_build_version()`: SYSTEM_GET_BUILD_VERSION, whose
/// three string pointers the backend cannot follow, answered here from the
/// host's version string.
fn get_build_version<E: Env + ?Sized>(
    env: &mut E,
    uarg: u64,
    user_nested: u64,
    nested_size: u32,
) -> i32 {
    const V1_TOTAL: usize = 40;
    if user_nested == 0 || (nested_size as usize) < V1_TOTAL {
        return -EINVAL;
    }
    let mut v1 = [0u8; V1_TOTAL];
    if env.copy_from_user(&mut v1, user_nested).is_err() {
        return -EFAULT;
    }
    let size_of_strings = le32(&v1, 0).unwrap_or(0);
    let ptrs = [
        le64(&v1, 8).unwrap_or(0),
        le64(&v1, 16).unwrap_or(0),
        le64(&v1, 24).unwrap_or(0),
    ];

    let mut ver = [0u8; 33];
    let dv = env.driver_version();
    let dv_len = dv.len().min(32);
    let ver_len = dv_len.saturating_add(1);
    if let Some(d) = ver.get_mut(..dv_len) {
        d.fill_from(dv.get(..dv_len).unwrap_or(&[]));
    }
    let ver = ver.get(..ver_len).unwrap_or(&[]);
    let max_len = ver_len.max(BUILD_TITLE.len()) as u32;

    // All pointers NULL: the size of the strings, only.
    if ptrs == [0, 0, 0] {
        let mut v = [0u8; V1_TOTAL];
        put32(&mut v, 0, max_len);
        if env.copy_to_user(user_nested, &v).is_err() {
            return -EFAULT;
        }
        return set_status(env, uarg, 0);
    }
    if size_of_strings < max_len {
        return -EINVAL;
    }
    for (p, s) in ptrs.iter().zip([ver, ver, BUILD_TITLE]) {
        if *p != 0 && env.copy_to_user(*p, s).is_err() {
            return -EFAULT;
        }
    }
    put32(&mut v1, 0, max_len);
    put64(&mut v1, 32, 0);
    if env.copy_to_user(user_nested, &v1).is_err() {
        return -EFAULT;
    }
    set_status(env, uarg, 0)
}

/// `nvgpu_rebase_time_correlation()`: GPU/CPU time correlation samples
/// moved from the host's clock into the guest's clock of the same id.
/// OSTIME is microseconds of realtime, PLATFORM_API nanoseconds of raw
/// monotonic and fills only `samples[0]`. Left alone for a GSP-side clock,
/// and against a backend too old to report its other clocks.
fn rebase_time_correlation<E: Env + ?Sized>(env: &mut E, p: &mut [u8]) {
    if p.len() < TCI_SAMPLES {
        return;
    }
    let id = p.get(TCI_CLK_ID).copied().unwrap_or(0);
    let mut n = p
        .get(TCI_SAMPLE_COUNT)
        .copied()
        .unwrap_or(0)
        .min(TCI_MAX_SAMPLES);
    if (id >> 4) & 0xf != TCI_PROC_CPU {
        return;
    }
    let (clk, scale) = match id & 0xf {
        TCI_SRC_OSTIME => (Clock::Realtime, 1000u64),
        TCI_SRC_PLATFORM_API => {
            n = n.min(1);
            (Clock::MonotonicRaw, 1u64)
        }
        _ => return,
    };
    for i in 0..usize::from(n) {
        let at = sum(&[TCI_SAMPLES, i.saturating_mul(TCI_SAMPLE_SIZE)]);
        let Some(host) = le64(p, at) else { return };
        let Some(guest) = env.clock_to_guest(clk, host.wrapping_mul(scale) as i64) else {
            return;
        };
        let guest = guest.max(0) as u64;
        put64(p, at, guest.checked_div(scale).unwrap_or(0));
    }
}

/// `nvgpu_ioctl_rm_control()`: NV_ESC_RM_CONTROL, with its nested block
/// and what a pointer in it reaches.
pub fn rm_control<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, sz: u32) -> i32 {
    match rm_control_inner(env, cmd, uarg, sz) {
        Ok(r) | Err(r) => r,
    }
}

fn rm_control_inner<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    sz: u32,
) -> Result<i32, Errno> {
    if (sz as usize) < NVOS54_SIZE {
        return Err(-EINVAL);
    }
    let mut params = [0u8; NVOS54_SIZE];
    env.copy_from_user(&mut params, uarg)?;
    let ctl_cmd = le32(&params, 8).unwrap_or(0);
    let user_nested = le64(&params, 16).unwrap_or(0);
    let nested_size = le32(&params, 24).unwrap_or(0);
    if nested_size > NESTED_MAX {
        return Err(-EINVAL);
    }
    let nz = nested_size as usize;
    let has_nested = user_nested != 0 && nested_size > 0;

    // Answered here, reading only its own 40 bytes.
    if ctl_cmd == RM_GET_BUILD_VERSION {
        return Ok(get_build_version(env, uarg, user_nested, nested_size));
    }

    // A size with no parameters: RM refuses it as it copies the parameters
    // in (param_copy.c rmapiParamsAcquire), NV_ERR_INVALID_ARGUMENT in the
    // struct, the ioctl itself succeeding; so that is the answer here.
    if user_nested == 0 && nested_size != 0 {
        return Ok(set_status(env, uarg, NV_ERR_INVALID_ARGUMENT));
    }

    // The nested block, read once: what every decision below is taken on,
    // and what is sent.
    let mut nested = alloc(env, if has_nested { nz } else { 0 })?;
    if has_nested {
        env.copy_from_user(nested.as_mut(), user_nested)?;
    }

    // A host TSC reading means nothing in the guest: refused as RM refuses a
    // clock it cannot sample.
    if ctl_cmd == RM_TIME_CORRELATION && has_nested {
        let clk = nested.as_ref().first().copied().unwrap_or(0);
        if (clk >> 4) & 0xf == TCI_PROC_CPU && clk & 0xf == TCI_SRC_TSC {
            return Ok(set_status(env, uarg, NV_ERR_NOT_SUPPORTED));
        }
    }

    // A second-level pointer, carried rather than rewritten: where it sits
    // and how much it addresses.
    let mut rw = if has_nested {
        env.v1v2(ctl_cmd).or_else(|| deep_only(ctl_cmd))
    } else {
        None
    };
    let mut deep_user_ptr: u64 = 0;
    let mut deep_ptr_offset: u32 = 0;
    let mut deep_len: u32 = 0;

    // Or several, as deep segments.
    let ctl_deep = if has_nested && env.caps().deep_segs {
        env.deep_control(ctl_cmd)
    } else {
        None
    };
    let mut plan = deep::Plan::default();
    if let Some(ctl) = ctl_deep {
        rw = None;
        plan = deep::plan(&ctl, nested.as_ref());
        if plan.n != 0 {
            deep_ptr_offset = DEEP_SEGMENTED;
            deep_len = plan.bytes;
        }
    }
    if let Some(rw) = rw {
        if u64::from(nested_size) >= u64::from(rw.v1_userptr_offset).saturating_add(8) {
            let n = nested.as_ref();
            deep_user_ptr = le64(n, rw.v1_userptr_offset as usize).unwrap_or(0);
            let count = le32(n, 0).unwrap_or(0);
            // The leading field says how much the buffer holds: entries of
            // eight bytes for the list-style commands, plain bytes for the
            // caps tables. NvU32 arithmetic, as the C has it.
            // Past u32, too large: no deep block, below.
            deep_len = if rw.info_style {
                count.saturating_mul(8)
            } else {
                count
            };
            deep_ptr_offset = rw.v1_userptr_offset;
            if deep_user_ptr == 0 || deep_len == 0 || deep_len > DEEP_MAX {
                deep_user_ptr = 0;
                deep_ptr_offset = 0;
                deep_len = 0;
            }
        }
    }

    // The calling process after the blocks.
    let proc = env.caps().proc_euid;
    let dl = deep_len as usize;
    let req_total = sum(&[
        IOCTL_REQ_LEN,
        NVOS54_SIZE,
        nz,
        dl,
        if proc { PROC_ID_LEN } else { 0 },
    ]);
    let resp_max = sum(&[IOCTL_RESP_LEN, NVOS54_SIZE, nz, dl]);
    let mut req = alloc(env, req_total)?;
    let mut resp = alloc(env, resp_max)?;

    let at_nested = IOCTL_REQ_LEN + NVOS54_SIZE;
    let mut nested_fd: i32 = -1;
    let mut nested_fd_offset = 0usize;
    let mut os_event: Option<(usize, u64)> = None;
    {
        let r = req.as_mut();
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            NVOS54_SIZE as u32,
            NVOS54_SIZE as u32,
            nested_size,
            deep_ptr_offset,
            deep_len,
        ));
        part(r, IOCTL_REQ_LEN, NVOS54_SIZE)?.fill_from(&params);
    }
    if has_nested {
        let r = req.as_mut();
        part(r, at_nested, nz)?.fill_from(nested.as_ref());

        // Exporting objects to a descriptor, importing them back and asking
        // about an export name another of our open files: the backend knows
        // it by the handle it issued. A number that is not one of our files
        // is refused here, never sent as it is; -1, which RM refuses itself,
        // is the one value that passes unchanged.
        if let Some(off) = unix_fd_offset(ctl_cmd) {
            if nz >= off.saturating_add(4) {
                let fd = le32(r, sum(&[at_nested, off])).unwrap_or(0) as i32;
                nested_fd = fd;
                if fd != -1 {
                    match env.handle_for_fd(fd) {
                        Ok(h) => {
                            put32(r, sum(&[at_nested, off]), h);
                            nested_fd_offset = off;
                        }
                        Err(_) => {
                            env.warn(Warn::ControlFd { cmd: ctl_cmd, fd });
                            return Err(-EBADF);
                        }
                    }
                }
            }
        }

        // A block too short to hold the field is the backend's to refuse;
        // only one that holds it is translated.
        if let Some(off) = os_event_offset(ctl_cmd) {
            if nz >= off.saturating_add(8) {
                match os_event_in(env, part(r, sum(&[at_nested, off]), 8)?) {
                    Ok(v) => os_event = Some((off, v)),
                    Err((e, val)) => {
                        env.warn(Warn::ControlOsEvent { cmd: ctl_cmd, val });
                        return Err(e);
                    }
                }
            }
        }
    }

    let at_deep = sum(&[at_nested, nz]);
    if plan.n != 0 {
        deep::fill(&plan, env, part(req.as_mut(), at_deep, dl)?)?;
    } else if deep_len > 0 {
        env.copy_from_user(part(req.as_mut(), at_deep, dl)?, deep_user_ptr)?;
    }
    if proc {
        let id = env.proc_id();
        part(req.as_mut(), sum(&[at_deep, dl]), PROC_ID_LEN)?.fill_from(&id);
    }

    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;

    // RM's own verdict is in params.status, so a successful reply always
    // carries the struct back. A failed one is a bare header: nothing to
    // copy, and the caller's struct is left as it was.
    if !has(used, IOCTL_RESP_LEN, NVOS54_SIZE) {
        return Ok(ret);
    }
    let r = resp.as_mut();
    env.copy_to_user(uarg, part_ref(r, IOCTL_RESP_LEN, NVOS54_SIZE)?)
        .map_err(|_| -EFAULT)?;

    let rn = IOCTL_RESP_LEN + NVOS54_SIZE;
    if user_nested != 0 && h.nested_len > 0 {
        let copy_back = nz.min(h.nested_len as usize);
        if !has(used, rn, copy_back) {
            return Ok(ret);
        }
        if nested_fd >= 0 && copy_back >= nested_fd_offset.saturating_add(4) {
            put32(r, sum(&[rn, nested_fd_offset]), nested_fd as u32);
        }
        if let Some((off, val)) = os_event {
            if copy_back >= off.saturating_add(8) {
                put64(r, sum(&[rn, off]), val);
            }
        }
        if ctl_cmd == RM_TIME_CORRELATION && le32(r, IOCTL_RESP_LEN + NVOS54_STATUS) == Some(0) {
            rebase_time_correlation(env, part(r, rn, copy_back)?);
        }
        if env.copy_to_user_failed(user_nested, part_ref(r, rn, copy_back)?) {
            ret = -EFAULT;
        }
    }

    let at = sum(&[rn, h.nested_len as usize]);
    if plan.n != 0 && h.deep_len > 0 {
        // Each segment RM writes goes back to its own pointer.
        let back = h.deep_len as usize;
        if has(used, at, back) && deep::copy_back(&plan, env, part_ref(r, at, back)?).is_err() {
            ret = -EFAULT;
        }
    } else if deep_len > 0 && h.deep_len > 0 {
        let copy_back = dl.min(h.deep_len as usize);
        if has(used, at, copy_back)
            && env.copy_to_user_failed(deep_user_ptr, part_ref(r, at, copy_back)?)
        {
            ret = -EFAULT;
        }
    }
    Ok(ret)
}

/// `nvgpu_ioctl_idle_channels()`: NV_ESC_RM_IDLE_CHANNELS. A channel list
/// goes with its three arrays as deep segments, to a backend that takes
/// them; anything else goes as the flat block it is.
pub fn idle_channels<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, sz: u32) -> i32 {
    if sz as usize != IDLE_CHANNELS_SIZE || !env.caps().deep_segs {
        return simple(env, cmd, uarg, sz);
    }
    let mut params = [0u8; IDLE_CHANNELS_SIZE];
    if let Err(e) = env.copy_from_user(&mut params, uarg) {
        return e;
    }
    let rule = env.deep_idle_channels();
    let flags = le32(&params, IDLE_CHANNELS_FLAGS).unwrap_or(0);
    let mask = (1u32 << (IDLE_CHANNELS_LIST_HI - IDLE_CHANNELS_LIST_LO + 1)) - 1;
    let channel = (flags >> IDLE_CHANNELS_LIST_LO) & mask;
    let count_at = rule
        .ptrs
        .first()
        .map_or(0, |p| usize::from(p.counts[0].offset));
    let count = le32(&params, count_at).unwrap_or(0);
    // The block already read is the one sent, whichever way it goes.
    if channel != IDLE_CHANNELS_LIST || count == 0 || count > IDLE_CHANNELS_MAX {
        return simple_bytes(env, cmd, uarg, &params);
    }
    let plan = deep::plan(&rule, &params);
    if plan.n == 0 {
        return simple_bytes(env, cmd, uarg, &params);
    }
    match idle_channels_segmented(env, cmd, uarg, &params, &plan) {
        Ok(r) | Err(r) => r,
    }
}

fn idle_channels_segmented<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    params: &[u8; IDLE_CHANNELS_SIZE],
    plan: &deep::Plan,
) -> Result<i32, Errno> {
    let bytes = plan.bytes as usize;
    let mut req = alloc(env, sum(&[IOCTL_REQ_LEN, IDLE_CHANNELS_SIZE, bytes]))?;
    let mut resp = alloc(env, IOCTL_RESP_LEN + IDLE_CHANNELS_SIZE)?;
    {
        let r = req.as_mut();
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            IDLE_CHANNELS_SIZE as u32,
            0,
            0,
            DEEP_SEGMENTED,
            plan.bytes,
        ));
        part(r, IOCTL_REQ_LEN, IDLE_CHANNELS_SIZE)?.fill_from(params);
    }
    deep::fill(
        plan,
        env,
        part(req.as_mut(), IOCTL_REQ_LEN + IDLE_CHANNELS_SIZE, bytes)?,
    )?;
    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;
    // RM only reads the arrays: the block, with RM's status, is all that
    // comes back.
    let data_len = h.data_len as usize;
    if data_len != 0
        && data_len <= IDLE_CHANNELS_SIZE
        && has(used, IOCTL_RESP_LEN, data_len)
        && env.copy_to_user_failed(uarg, part_ref(resp.as_ref(), IOCTL_RESP_LEN, data_len)?)
    {
        ret = -EFAULT;
    }
    Ok(ret)
}

/// `nvgpu_ioctl_rm_alloc()`: NV_ESC_RM_ALLOC and its class parameters,
/// sized from the class when the caller left the size zero.
pub fn rm_alloc<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, sz: u32) -> i32 {
    match rm_alloc_inner(env, cmd, uarg, sz, None) {
        Ok(r) | Err(r) => r,
    }
}

/// [`rm_alloc`] for an argument already copied in (`data`, `_IOC_SIZE`
/// bytes), whose NVOS64 is what is sent.
pub fn rm_alloc_bytes<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64, data: &[u8]) -> i32 {
    match rm_alloc_inner(env, cmd, uarg, data.len() as u32, Some(data)) {
        Ok(r) | Err(r) => r,
    }
}

fn rm_alloc_inner<E: Env + ?Sized>(
    env: &mut E,
    cmd: u32,
    uarg: u64,
    sz: u32,
    data: Option<&[u8]>,
) -> Result<i32, Errno> {
    if (sz as usize) < NVOS64_SIZE {
        return Err(-EINVAL);
    }
    let mut params = [0u8; NVOS64_SIZE];
    match data.and_then(|d| d.get(..NVOS64_SIZE)) {
        Some(d) => params.fill_from(d),
        None => env.copy_from_user(&mut params, uarg)?,
    }
    let hclass = le32(&params, 12).unwrap_or(0);
    let user_alloc = le64(&params, 16).unwrap_or(0);
    let mut nested_size = le32(&params, 32).unwrap_or(0);
    // No parameters: RM sizes them from the class (rmapiParamsCopyInit) and
    // with a NULL pointer takes none -- or refuses the class that needs them
    // -- whatever paramsSize says. So none are sent, and the host is told a
    // size of 0; the caller's comes back as it was.
    let mut caller_psize = 0u32;
    if user_alloc == 0 && nested_size != 0 {
        caller_psize = nested_size;
        put32(&mut params, 32, 0);
        nested_size = 0;
    }
    // paramsSize 0 with a pointer: the host's RM sizes it from the class,
    // so we must too to know how much to copy.
    if user_alloc != 0 && nested_size == 0 {
        nested_size = env.class_param_size(hclass);
    }
    if nested_size > NESTED_MAX {
        return Err(-EINVAL);
    }
    let nz = nested_size as usize;
    let proc = env.caps().proc_ids;
    let req_total = sum(&[
        IOCTL_REQ_LEN,
        NVOS64_SIZE,
        nz,
        if proc { PROC_ID_LEN } else { 0 },
    ]);
    let resp_max = sum(&[IOCTL_RESP_LEN, NVOS64_SIZE, nz]);
    let mut req = alloc(env, req_total)?;
    let mut resp = alloc(env, resp_max)?;
    let at = IOCTL_REQ_LEN + NVOS64_SIZE;
    {
        let r = req.as_mut();
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            NVOS64_SIZE as u32,
            NVOS64_SIZE as u32,
            nested_size,
            0,
            0,
        ));
        part(r, IOCTL_REQ_LEN, NVOS64_SIZE)?.fill_from(&params);
    }
    if proc {
        let id = env.proc_id();
        part(req.as_mut(), sum(&[at, nz]), PROC_ID_LEN)?.fill_from(&id);
    }

    let mut event_fd: i32 = -1;
    let mut os_event: Option<u64> = None;
    if user_alloc != 0 && nz > 0 {
        let r = req.as_mut();
        env.copy_from_user(part(r, at, nz)?, user_alloc)?;

        // An event object names the file the event is delivered on inside
        // these parameters: NV0005_ALLOC_PARAMETERS.data at 16, swapped for
        // the backend's handle for that file.
        if (hclass == CLASS_EVENT || hclass == CLASS_EVENT_OS_EVENT) && nz >= NV0005_DATA_OFFSET + 4
        {
            event_fd = le32(r, at + NV0005_DATA_OFFSET).unwrap_or(0) as i32;
            // -1 is "no descriptor"; any other negative is refused.
            if event_fd < -1 {
                return Err(-EBADF);
            }
            if event_fd >= 0 {
                match env.handle_for_fd(event_fd) {
                    Ok(h) => {
                        put32(r, at + NV0005_DATA_OFFSET, h);
                    }
                    Err(_) => {
                        env.warn(Warn::AllocEventFd {
                            class: hclass,
                            fd: event_fd,
                        });
                        return Err(-EBADF);
                    }
                }
            }
        }

        // NV_EVENT_BUFFER's OS event, as for a waiter's.
        if hclass == CLASS_EVENT_BUFFER && nz >= EVENT_BUFFER_NOTIFICATION_OFFSET + 8 {
            match os_event_in(env, part(r, at + EVENT_BUFFER_NOTIFICATION_OFFSET, 8)?) {
                Ok(v) => os_event = Some(v),
                Err((e, val)) => {
                    env.warn(Warn::EventBufferOsEvent { val });
                    return Err(e);
                }
            }
        }
    }

    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;
    // As for RM_CONTROL: a failed reply is a bare header, nothing to copy.
    if !has(used, IOCTL_RESP_LEN, NVOS64_SIZE) {
        return Ok(ret);
    }
    let r = resp.as_mut();
    if caller_psize != 0 {
        put32(r, IOCTL_RESP_LEN + 32, caller_psize);
    }
    env.copy_to_user(uarg, part_ref(r, IOCTL_RESP_LEN, NVOS64_SIZE)?)
        .map_err(|_| -EFAULT)?;

    if user_alloc != 0 && h.nested_len > 0 {
        let copy_back = nz.min(h.nested_len as usize);
        let rn = IOCTL_RESP_LEN + NVOS64_SIZE;
        if let Some(val) = os_event {
            if copy_back >= EVENT_BUFFER_NOTIFICATION_OFFSET + 8 {
                put64(r, rn + EVENT_BUFFER_NOTIFICATION_OFFSET, val);
            }
        }
        // The event's data comes back holding the backend's handle; the
        // caller passed its descriptor, and RM leaves the field alone, so
        // that is what goes back -- on a failed RM status too.
        if event_fd >= 0 && copy_back >= NV0005_DATA_OFFSET + 4 {
            put32(r, rn + NV0005_DATA_OFFSET, event_fd as u32);
        }
        if has(used, rn, copy_back)
            && env.copy_to_user_failed(user_alloc, part_ref(r, rn, copy_back)?)
        {
            ret = -EFAULT;
        }
    }
    Ok(ret)
}

/// `nvgpu_ioctl_modeset()`: one NVKMS ioctl to a v1 backend, the outer
/// struct and the parameter block it points at, with REGISTER_SURFACE's
/// descriptor swapped for the backend's handle.
pub fn modeset_v1<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64) -> i32 {
    match modeset_v1_inner(env, cmd, uarg) {
        Ok(r) | Err(r) => r,
    }
}

fn modeset_v1_inner<E: Env + ?Sized>(env: &mut E, cmd: u32, uarg: u64) -> Result<i32, Errno> {
    // NVKMS takes one ioctl, NVKMS_IOCTL_CMD with exactly NvKmsIoctlParams
    // behind it; anything else is -ENOTTY (nvidia-modeset-linux.c).
    if ioc_nr(cmd) != 0 || (cmd >> 16) & 0x3fff != NVKMS_OUTER_SIZE as u32 {
        return Err(-ENOTTY);
    }
    let mut outer = [0u8; NVKMS_OUTER_SIZE];
    env.copy_from_user(&mut outer, uarg)?;
    let user_nested = le64(&outer, 8).unwrap_or(0);
    let nested_size = le32(&outer, 4).unwrap_or(0);
    if nested_size > NESTED_MAX {
        return Err(-EINVAL);
    }
    // A size with no parameters: NVKMS copies the request in from the
    // address and fails the call (nvkms.c nvKmsIoctl).
    if user_nested == 0 && nested_size != 0 {
        return Err(-EPERM);
    }
    let nz = nested_size as usize;
    let mut req = alloc(env, sum(&[IOCTL_REQ_LEN, NVKMS_OUTER_SIZE, nz]))?;
    let mut resp = alloc(env, sum(&[IOCTL_RESP_LEN, NVKMS_OUTER_SIZE, nz]))?;
    let at = IOCTL_REQ_LEN + NVKMS_OUTER_SIZE;
    {
        let r = req.as_mut();
        part(r, 0, IOCTL_REQ_LEN)?.fill_from(&ioctl_req_header(
            env.handle(),
            cmd,
            NVKMS_OUTER_SIZE as u32,
            NVKMS_OUTER_SIZE as u32,
            nested_size,
            0,
            0,
        ));
        part(r, IOCTL_REQ_LEN, NVKMS_OUTER_SIZE)?.fill_from(&outer);
    }
    if user_nested != 0 && nz > 0 {
        let r = req.as_mut();
        env.copy_from_user(part(r, at, nz)?, user_nested)?;
        // REGISTER_SURFACE names its memory by descriptor when useFd is set.
        if le32(&outer, 0) == Some(NVKMS_REGISTER_SURFACE) && nz >= NVKMS_SURFACE_FD_OFFSET + 8 {
            let use_fd = le32(r, at + 4).unwrap_or(0);
            if use_fd != 0 {
                let fd = le32(r, at + NVKMS_SURFACE_FD_OFFSET).unwrap_or(0) as i32;
                match env.handle_for_fd(fd) {
                    Ok(h) => {
                        put64(r, at + NVKMS_SURFACE_FD_OFFSET, u64::from(h));
                    }
                    Err(_) => {
                        env.warn(Warn::SurfaceFd { fd });
                        return Err(-EBADF);
                    }
                }
            }
        }
    }
    let (h, used) = round_trip(env, req.as_ref(), resp.as_mut())?;
    let mut ret = h.status;
    if !has(used, IOCTL_RESP_LEN, NVKMS_OUTER_SIZE) {
        return Ok(ret);
    }
    let r = resp.as_ref();
    env.copy_to_user(uarg, part_ref(r, IOCTL_RESP_LEN, NVKMS_OUTER_SIZE)?)
        .map_err(|_| -EFAULT)?;
    if user_nested != 0 && h.nested_len > 0 {
        let copy_back = nz.min(h.nested_len as usize);
        let rn = IOCTL_RESP_LEN + NVKMS_OUTER_SIZE;
        if has(used, rn, copy_back)
            && env.copy_to_user_failed(user_nested, part_ref(r, rn, copy_back)?)
        {
            ret = -EFAULT;
        }
    }
    Ok(ret)
}
