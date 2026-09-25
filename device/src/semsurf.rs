//! Semaphore surfaces, and the RM objects a VM owns that name them.
//!
//! nvidia-drm's SEMSURF_FENCE_CTX_CREATE (0x54) is the one fence call whose
//! arguments reach host kernel memory before anything RM checks them. Its
//! nested block names an RM semaphore surface as `{hClient,
//! hSemaphoreSurface, size}` (nvkms-kapi-private.h:61-65), and KAPI dups that
//! object into NVKMS's own client *at kernel privilege* (nvRmApiDupObject2,
//! nvkms-kapi-sync.c:240-252): the dup skips the client access check a
//! userspace dup gets (rs_client.c:540-569), so any client on the host is
//! reachable -- a compositor's, another VM's -- which NVKMS itself forbids its
//! own userspace (nvkms.c:2722-2730). It then kernel-maps `size` bytes of the
//! surface's memory (:275-306; RM refuses a size past the memory,
//! mapping_cpu.c:262-278), and nvidia-drm adds `index * stride` to that
//! mapping with no bound and no overflow check (nvidia-drm-fence.c:1257-1261)
//! and READ_ONCEs the result on every FENCE_CREATE and every timeout
//! (:716-740). A guest choosing `index` chooses a host kernel address to
//! read -- an oracle for any word, or an oops.
//!
//! So the backend decides three things before the host sees a 0x54, on its
//! own copy of the arguments (the copy the host is handed):
//!
//! - **the index is inside the surface.** `stride` and the max-submitted
//!   offset are the host's own answer to
//!   NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT, which is where
//!   nvidia-drm gets them (nvkms-kapi.c:1157-1179, nvidia-drm-drv.c:803-806),
//!   asked once per GPU on a private RM client of the backend's
//!   ([`query_layout`]). Unknown -- the query failed, or the host says
//!   semaphore surfaces are unsupported (stride 0, legacy SLI,
//!   mem_mgr_ctrl.c:748-750) -- is EOPNOTSUPP, which is also what the host
//!   answers a 0x54 it cannot serve (nvidia-drm-fence.c:1316-1318);
//! - **the client is this VM's**: one allocated through our RM path and not
//!   freed since ([`SemsurfPolicy::client_allocated`]);
//! - **there is room**: each context costs a host kthread, a timer, an NVKMS
//!   dup and a kernel mapping (nvidia-drm-fence.c:1233-1310,
//!   nvidia-drm-os-interface.c:166-176), none of it bounded by the host, so
//!   the backend counts live ones per file and per session.
//!
//! The same RM bookkeeping serves the OS events a guest names inside RM
//! parameters (NVIDIA's `notificationHandle`, a descriptor number the host
//! looks up by `(fd, hClient)` in the list ALLOC_OS_EVENT fills, os.c:
//! 1789-1815): REGISTER/UNREGISTER_WAITER on a semaphore surface (0xda0003,
//! 0xda0005; ctrl00da.h:207-212, 251-255) and the NV_EVENT_BUFFER allocation
//! (0x90cd, cl90cd.h:164-180). The guest turns its descriptor into our handle
//! and the backend turns that into the host descriptor, as for every other
//! descriptor in RM parameters. For 0x90cd a translation that misses is not a
//! refusal but a host oops: eventbufferConstruct ignores the failed lookup
//! and keeps the raw number as the event pointer it later dereferences
//! (event_buffer.c:463-477, 495-505). So a nonzero handle there must name an
//! OS event the backend saw allocated and has not seen freed.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, MutexGuard};

use crate::hostfd::{IOC_RW, ioc};
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;
use crate::xfer::{Errno, Prepared};

/// `DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE`
/// (`struct drm_nvidia_semsurf_fence_ctx_create_params`, 32 bytes:
/// `u64 index; u64 nvkms_params_ptr; u64 nvkms_params_size; u32 handle;
/// u32 __pad;`, nv_drm_common_ioctl.h:355-365).
pub const SEMSURF_FENCE_CTX_CREATE: u32 = ioc(IOC_RW, b'd', 0x54, 32);
const CTX_INDEX_AT: usize = 0;
const CTX_PARAMS_PTR_AT: usize = 8;
const CTX_HANDLE_AT: usize = 24;

/// `NvKmsKapiPrivImportSemaphoreSurfaceParams`: `{NvHandle hClient;
/// NvHandle hSemaphoreSurface; NvU64 semaphoreSurfaceSize;}`. KAPI takes
/// exactly this size and nothing else (nvkms-kapi-sync.c:198-203).
const IMPORT_PARAMS_SIZE: usize = 16;

/// Live fence contexts one render file may hold, and one session. The ICD
/// makes one per VkDevice (R:fences §1.3); a compositor a handful.
pub const CTX_CAP_PER_FILE: usize = 16;
pub const CTX_CAP_PER_SESSION: usize = 256;

/// `NV01_ROOT`, `NV01_ROOT_NON_PRIV`, `NV01_ROOT_CLIENT`: the classes a
/// client is allocated as (escape.c:473-481 turns all three into the last).
pub const ROOT_CLASSES: [u32; 3] = [0x0, 0x1, 0x41];

/// `NV_EVENT_BUFFER`, and where its `notificationHandle` sits.
pub const NV_EVENT_BUFFER: u32 = 0x90cd;
const EVENT_BUFFER_NOTIFICATION_AT: usize = 40;

/// `NV_SEMAPHORE_SURFACE_CTRL_CMD_{REGISTER,UNREGISTER}_WAITER` and where
/// each keeps its `notificationHandle`.
pub const SEMSURF_REGISTER_WAITER: u32 = 0xda0003;
pub const SEMSURF_UNREGISTER_WAITER: u32 = 0xda0005;

/// What nvidia-drm needs to find semaphore `index` of a surface: the size of
/// one semaphore's slot, and where in the slot the max-submitted word of a
/// 32-bit semaphore sits (NV2080_CTRL_FB_GET_SEMAPHORE_SURFACE_LAYOUT_PARAMS
/// `size` and `maxSubmittedSemaphoreValueOffset`, ctrl2080fb.h:2649-2654).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub stride: u64,
    pub max_submitted: u64,
}

/// Whether semaphore `index` of a surface kernel-mapped for `size` bytes can
/// be read where nvidia-drm will read it: its slot `[index * stride, +stride)`
/// inside the mapping, and the two 8-byte words it READ_ONCEs -- the value at
/// the slot's start and, when the surface has max-submitted memory, the word
/// `max_submitted` into the slot (nvidia-drm-fence.c:1257-1261, 716-740).
/// That second mapping is of the same `size` (nvkms-kapi-sync.c:291-298), so
/// one bound covers both. Everything in checked arithmetic: the host's is
/// not, and a wrap is exactly the address a guest would aim with.
pub fn check_index(layout: Layout, index: u64, size: u64) -> Result<(), Errno> {
    if layout.stride == 0 {
        return Err(libc::EOPNOTSUPP);
    }
    let slot = index.checked_mul(layout.stride).ok_or(libc::EINVAL)?;
    let within = |end: Option<u64>| end.is_some_and(|e| e <= size);
    if !within(slot.checked_add(layout.stride))
        || !within(slot.checked_add(8))
        || !within(
            slot.checked_add(layout.max_submitted)
                .and_then(|o| o.checked_add(8)),
        )
    {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn rd(b: &[u8], at: usize, width: usize) -> Option<u64> {
    let s = b.get(at..at + width)?;
    let mut w = [0u8; 8];
    w[..width].copy_from_slice(s);
    Some(u64::from_le_bytes(w))
}

// ───────────────────────────── the policy ─────────────────────────────

/// The per-session state the checks above need. Shared by the backend's RM
/// path (queue thread, which feeds it) and the IOCTL2 policy hooks
/// (`policy.rs`, which read it), hence the lock.
#[derive(Default)]
pub struct SemsurfPolicy {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The host's layout per render node (DRI index), once asked. A fact
    /// about the host, so it outlives a session reset.
    layouts: HashMap<u32, Layout>,
    /// Every live render handle, with its DRI index.
    renders: HashMap<u32, u32>,
    /// Every RM client this VM allocated and has not freed, with the handle
    /// of the file it was allocated through (the client dies with that
    /// file, RmFreeUnusedClients, osapi.c:546-583).
    clients: HashMap<u32, u32>,
    /// Every live OS event, `(hClient, handle of the file its fd names)`,
    /// with the handle ALLOC_OS_EVENT was issued on (the event dies with
    /// that file too, osapi.c:583).
    os_events: HashMap<(u32, u32), u32>,
    /// Live fence contexts: GEM handles per render handle.
    ctxs: HashMap<u32, HashSet<u32>>,
}

impl SemsurfPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The host's layout for render node `dri`, if it has been learned.
    pub fn layout(&self, dri: u32) -> Option<Layout> {
        self.lock().layouts.get(&dri).copied()
    }

    pub fn set_layout(&self, dri: u32, layout: Layout) {
        self.lock().layouts.insert(dri, layout);
    }

    /// A render node was opened as `handle`.
    pub fn render_opened(&self, handle: u32, dri: u32) {
        self.lock().renders.insert(handle, dri);
    }

    /// RM_ALLOC of a client class succeeded through `issuer`.
    pub fn client_allocated(&self, issuer: u32, h_client: u32) {
        self.lock().clients.insert(h_client, issuer);
    }

    /// RM_FREE of a client was asked for. Forgotten whatever RM answered: a
    /// client kept past a free it survived only costs the guest a refusal,
    /// one kept past a free it did not would let the next owner of the
    /// number be named. Its OS events go with it (escape.c:521-527).
    pub fn client_freed(&self, h_client: u32) {
        let mut g = self.lock();
        g.clients.remove(&h_client);
        g.os_events.retain(|&(c, _), _| c != h_client);
    }

    pub fn owns_client(&self, h_client: u32) -> bool {
        self.lock().clients.contains_key(&h_client)
    }

    /// ALLOC_OS_EVENT succeeded on `issuer` for `(h_client, event)`.
    pub fn os_event_allocated(&self, issuer: u32, h_client: u32, event: u32) {
        self.lock().os_events.insert((h_client, event), issuer);
    }

    /// FREE_OS_EVENT was asked for (forgotten whatever RM answered, as for
    /// clients).
    pub fn os_event_freed(&self, h_client: u32, event: u32) {
        self.lock().os_events.remove(&(h_client, event));
    }

    pub fn os_event_live(&self, h_client: u32, event: u32) -> bool {
        self.lock().os_events.contains_key(&(h_client, event))
    }

    /// `handle` was closed: whatever was allocated through it, named by it
    /// or counted against it is gone (or no longer ours to name). Returns
    /// the RM clients that died with it, for the memory records (rmmem.rs),
    /// which keep no client set of their own.
    pub fn forget_handle(&self, handle: u32) -> Vec<u32> {
        let mut g = self.lock();
        g.renders.remove(&handle);
        let gone: Vec<u32> = g
            .clients
            .iter()
            .filter(|&(_, &issuer)| issuer == handle)
            .map(|(&c, _)| c)
            .collect();
        g.clients.retain(|_, issuer| *issuer != handle);
        g.os_events
            .retain(|&(_, event), issuer| *issuer != handle && event != handle);
        g.ctxs.remove(&handle);
        gone
    }

    /// The session is gone. The layouts stay: they are the host's.
    pub fn reset(&self) {
        let mut g = self.lock();
        g.renders.clear();
        g.clients.clear();
        g.os_events.clear();
        g.ctxs.clear();
    }

    /// A GEM handle of render file `render` was closed.
    pub fn gem_closed(&self, render: u32, gem: u32) {
        let mut g = self.lock();
        if let Some(set) = g.ctxs.get_mut(&render) {
            set.remove(&gem);
            if set.is_empty() {
                g.ctxs.remove(&render);
            }
        }
    }

    /// Live fence contexts of `render`, and of the session.
    pub fn ctx_counts(&self, render: u32) -> (usize, usize) {
        let g = self.lock();
        (
            g.ctxs.get(&render).map_or(0, HashSet::len),
            g.ctxs.values().map(HashSet::len).sum(),
        )
    }

    /// Whether a 0x54 on render file `target`, for semaphore `index` of the
    /// surface `params` (the nested block, exactly as the host will read it)
    /// names, may reach the host.
    pub fn admit(&self, target: u32, index: u64, params: &[u8]) -> Result<(), Errno> {
        let g = self.lock();
        let layout = g
            .renders
            .get(&target)
            .and_then(|dri| g.layouts.get(dri))
            .copied()
            .ok_or(libc::EOPNOTSUPP)?;
        if params.len() != IMPORT_PARAMS_SIZE {
            return Err(libc::EINVAL);
        }
        let h_client = rd(params, 0, 4).unwrap_or(0) as u32;
        let size = rd(params, 8, 8).unwrap_or(0);
        if !g.clients.contains_key(&h_client) {
            log::warn!(
                "SEMSURF_FENCE_CTX_CREATE names RM client {h_client:#x}, which this VM did not \
                 allocate; refused"
            );
            return Err(libc::EPERM);
        }
        check_index(layout, index, size).inspect_err(|_| {
            log::warn!(
                "SEMSURF_FENCE_CTX_CREATE: semaphore {index} does not fit a {size}-byte surface \
                 of {}-byte slots; refused",
                layout.stride
            )
        })?;
        let mine = g.ctxs.get(&target).map_or(0, HashSet::len);
        let all: usize = g.ctxs.values().map(HashSet::len).sum();
        if mine >= CTX_CAP_PER_FILE || all >= CTX_CAP_PER_SESSION {
            log::warn!(
                "SEMSURF_FENCE_CTX_CREATE on handle {target}: {mine} contexts in the file, {all} \
                 in the session (caps {CTX_CAP_PER_FILE}, {CTX_CAP_PER_SESSION}); refused"
            );
            return Err(libc::ENOSPC);
        }
        Ok(())
    }

    /// `Hooks::before` for 0x54: its index, client and room, from the
    /// backend's copy of the call.
    pub fn ctx_create_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let arg = p.buffer(0).ok_or(libc::EINVAL)?;
        let index = rd(arg, CTX_INDEX_AT, 8).ok_or(libc::EINVAL)?;
        // A NULL or empty block gets no buffer (and the host a NULL
        // pointer); KAPI would refuse it, and so do we, before it gets
        // there.
        let params = p
            .pointee(0, CTX_PARAMS_PTR_AT)
            .and_then(|b| p.buffer(b))
            .ok_or(libc::EINVAL)?;
        self.admit(p.target(), index, params)
    }

    /// `Hooks::after` for 0x54: count the context it made. Only for a
    /// render handle still live, so a call that finished after a reset (or
    /// after its file closed) leaves no count behind for a later handle of
    /// the same number.
    pub fn ctx_create_after(&self, p: &Prepared, ret: i32) {
        if ret != 0 {
            return;
        }
        let Some(gem) = p.buffer(0).and_then(|a| rd(a, CTX_HANDLE_AT, 4)) else {
            return;
        };
        let target = p.target();
        let mut g = self.lock();
        if gem != 0 && g.renders.contains_key(&target) {
            g.ctxs.entry(target).or_default().insert(gem as u32);
        }
    }
}

/// Where an RM call names an OS event by descriptor, and for which client:
/// `(offset in the nested block, hClient)`, or `None` for a call that names
/// none. `outer` is the NVOS54 (RM_CONTROL, 0x2a) or NVOS64 (RM_ALLOC, 0x2b)
/// block, `nested_len` what the guest sent for its parameters. EINVAL when
/// the field is not in what the guest sent: the host would read it from past
/// our copy, or -- with no copy at all -- from wherever the guest's pointer
/// lands in this process.
pub fn os_event_field(
    escape: u32,
    outer: &[u8],
    nested_len: usize,
) -> Result<Option<(usize, u32)>, Errno> {
    let off = match escape {
        0x2a => match rd(outer, 8, 4).map(|c| c as u32) {
            Some(SEMSURF_REGISTER_WAITER) => 24,
            Some(SEMSURF_UNREGISTER_WAITER) => 16,
            _ => return Ok(None),
        },
        0x2b if rd(outer, 12, 4) == Some(u64::from(NV_EVENT_BUFFER)) => {
            EVENT_BUFFER_NOTIFICATION_AT
        }
        _ => return Ok(None),
    };
    if nested_len < off + 8 {
        return Err(libc::EINVAL);
    }
    let h_client = rd(outer, 0, 4).ok_or(libc::EINVAL)? as u32;
    Ok(Some((off, h_client)))
}

// ───────────────────────── asking the host its layout ─────────────────────────

/// The RM calls [`query_layout`] makes, so it can be followed without a GPU.
pub trait Rm {
    /// Open a device node for the backend itself.
    fn open(&self, path: &str) -> io::Result<PrivateFd>;
    /// RM_ALLOC (NVOS64) of `class` under `parent` in client `root` (0, 0
    /// for a new client); RM chooses the handle. `Err` carries the errno or
    /// the RM status, for the log.
    fn alloc(
        &self,
        ctl: RawFd,
        root: u32,
        parent: u32,
        class: u32,
        params: Option<&mut [u8]>,
    ) -> Result<u32, String>;
    /// RM_CONTROL (NVOS54) `cmd` on `object`; `params` is exactly the
    /// command's parameter struct.
    fn control(
        &self,
        ctl: RawFd,
        client: u32,
        object: u32,
        cmd: u32,
        params: &mut [u8],
    ) -> Result<(), String>;
}

const NV01_ROOT_CLIENT: u32 = 0x41;
const NV01_DEVICE_0: u32 = 0x80;
const NV20_SUBDEVICE_0: u32 = 0x2080;
/// ctrl0000gpu.h:172-185: `{gpuId, gpuFlags, deviceInstance,
/// subDeviceInstance, sliStatus, boardId, gpuInstance, numaId}`.
const NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2: u32 = 0x205;
/// ctrl2080fb.h:2642-2654: `{u64 maxSubmittedSemaphoreValueOffset; u64
/// monitoredFenceThresholdOffset; u64 size; u32 caps;}`.
const NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT: u32 = 0x2080_1352;

/// Ask the host RM, as nvidia-drm's KAPI does, what a semaphore surface on
/// the GPU `/dev/nvidia{gpu_minor}` (RM gpuId `gpu_id`, as its render node's
/// GET_DEV_INFO reports it, nvidia-drm-drv.c:1092) looks like.
///
/// On a client of the backend's own, which dies with the private control
/// file when this returns: a root client, the device the gpuId maps to
/// (GET_ID_INFO_V2 names its instance), its subdevice, and the control.
/// The GPU's own node is held open meanwhile because a device allocation
/// is refused to a process that has no file of that GPU open
/// (device.c:141-149, nv_is_gpu_accessible).
pub fn query_layout(rm: &dyn Rm, gpu_minor: u32, gpu_id: u32) -> Result<Layout, String> {
    let ctl = rm
        .open("/dev/nvidiactl")
        .map_err(|e| format!("/dev/nvidiactl: {e}"))?;
    let _gpu = rm
        .open(&format!("/dev/nvidia{gpu_minor}"))
        .map_err(|e| format!("/dev/nvidia{gpu_minor}: {e}"))?;
    let ctl = ctl.as_raw_fd();
    let client = rm.alloc(ctl, 0, 0, NV01_ROOT_CLIENT, None)?;

    let mut id = [0u8; 32];
    id[0..4].copy_from_slice(&gpu_id.to_le_bytes());
    rm.control(
        ctl,
        client,
        client,
        NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2,
        &mut id,
    )
    .map_err(|e| format!("GPU_GET_ID_INFO_V2 of gpuId {gpu_id:#x}: {e}"))?;
    let device_instance = rd(&id, 8, 4).unwrap_or(0) as u32;
    let subdevice_instance = rd(&id, 12, 4).unwrap_or(0) as u32;

    // NV0080_ALLOC_PARAMETERS {deviceId, ...}: the rest zero is a device
    // with default VA space, as any client makes one.
    let mut dev = [0u8; 56];
    dev[0..4].copy_from_slice(&device_instance.to_le_bytes());
    let device = rm
        .alloc(ctl, client, client, NV01_DEVICE_0, Some(&mut dev))
        .map_err(|e| format!("device {device_instance}: {e}"))?;
    let mut sub = [0u8; 4];
    sub.copy_from_slice(&subdevice_instance.to_le_bytes());
    let subdevice = rm
        .alloc(ctl, client, device, NV20_SUBDEVICE_0, Some(&mut sub))
        .map_err(|e| format!("subdevice {subdevice_instance}: {e}"))?;

    let mut layout = [0u8; 32];
    rm.control(
        ctl,
        client,
        subdevice,
        NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT,
        &mut layout,
    )
    .map_err(|e| format!("FB_GET_SEMAPHORE_SURFACE_LAYOUT: {e}"))?;
    Ok(Layout {
        stride: rd(&layout, 16, 8).unwrap_or(0),
        max_submitted: rd(&layout, 0, 8).unwrap_or(0),
    })
}

/// The host's RM.
pub struct HostRm;

/// RM_ALLOC and RM_CONTROL, `_IOWR('F', nr, NVOS64/NVOS54)`.
const NV_ESC_RM_ALLOC_64: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
const NV_ESC_RM_CONTROL: u32 = ioc(IOC_RW, b'F', 0x2a, 32);

/// An NVOS64: `{hRoot, hObjectParent, hObjectNew, hClass, NvP64
/// pAllocParms, NvP64 pRightsRequested, paramsSize, flags, status}`
/// (nvos.h:478-490).
pub fn nvos64(root: u32, parent: u32, class: u32, params: u64, params_size: u32) -> [u8; 48] {
    let mut a = [0u8; 48];
    a[0..4].copy_from_slice(&root.to_le_bytes());
    a[4..8].copy_from_slice(&parent.to_le_bytes());
    a[12..16].copy_from_slice(&class.to_le_bytes());
    a[16..24].copy_from_slice(&params.to_le_bytes());
    a[32..36].copy_from_slice(&params_size.to_le_bytes());
    a
}

/// An NVOS54: `{hClient, hObject, cmd, flags, NvP64 params, paramsSize,
/// status}` (nvos.h:2230-2239).
pub fn nvos54(client: u32, object: u32, cmd: u32, params: u64, params_size: u32) -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&client.to_le_bytes());
    a[4..8].copy_from_slice(&object.to_le_bytes());
    a[8..12].copy_from_slice(&cmd.to_le_bytes());
    a[16..24].copy_from_slice(&params.to_le_bytes());
    a[24..28].copy_from_slice(&params_size.to_le_bytes());
    a
}

fn rm_ioctl(fd: RawFd, request: u32, arg: &mut [u8]) -> Result<(), String> {
    loop {
        // SAFETY: `arg` is _IOC_SIZE(request) bytes; any pointer in it
        // addresses a live buffer of the caller's, as large as the host
        // copies (see `alloc`).
        let r = unsafe { libc::ioctl(fd, request as libc::Ioctl, arg.as_mut_ptr()) };
        if r >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EINTR) {
            return Err(e.to_string());
        }
    }
}

impl Rm for HostRm {
    fn open(&self, path: &str) -> io::Result<PrivateFd> {
        let c = CString::new(path).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        // SAFETY: a NUL-terminated path; the result is owned below.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor open() just returned; nothing else owns it.
        Ok(PrivateFd::new(unsafe { OwnedFd::from_raw_fd(fd) }))
    }

    fn alloc(
        &self,
        ctl: RawFd,
        root: u32,
        parent: u32,
        class: u32,
        params: Option<&mut [u8]>,
    ) -> Result<u32, String> {
        // RM copies in -- and back out -- as many bytes as *its* table says
        // the class takes (rmapiGetClassAllocParamSize, resource_desc.c:
        // 181-212), whatever paramsSize says. A host whose struct grew would
        // write past ours, so the host is handed a page of our own.
        let mut page = vec![0u8; 4096];
        let (ptr, size) = match params {
            Some(p) => {
                page[..p.len()].copy_from_slice(p);
                (page.as_mut_ptr() as u64, p.len() as u32)
            }
            None => (0, 0),
        };
        let mut a = nvos64(root, parent, class, ptr, size);
        rm_ioctl(ctl, NV_ESC_RM_ALLOC_64, &mut a)?;
        match rd(&a, 40, 4).unwrap_or(0) as u32 {
            0 => Ok(rd(&a, 8, 4).unwrap_or(0) as u32),
            s => Err(format!("RM status {s:#x}")),
        }
    }

    fn control(
        &self,
        ctl: RawFd,
        client: u32,
        object: u32,
        cmd: u32,
        params: &mut [u8],
    ) -> Result<(), String> {
        // Controls copy exactly paramsSize, which RM requires to be the
        // command's own size.
        let mut a = nvos54(
            client,
            object,
            cmd,
            params.as_mut_ptr() as u64,
            params.len() as u32,
        );
        rm_ioctl(ctl, NV_ESC_RM_CONTROL, &mut a)?;
        match rd(&a, 28, 4).unwrap_or(0) as u32 {
            0 => Ok(()),
            s => Err(format!("RM status {s:#x}")),
        }
    }
}

// ─────────────────────────────── the backend ───────────────────────────────

/// The params of a successful v1 IOCTL reply the backend wrote into `resp`
/// (`n` bytes): past the header and `IoctlResp`, or `None` for a failure,
/// which is a bare header.
pub(crate) fn reply_params(resp: &[u8], n: usize) -> Option<&[u8]> {
    use protocol::messages::{IoctlResp, MsgHeader};
    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    let status = i32::from_le_bytes(resp.get(8..12)?.try_into().ok()?);
    (status == 0 && n >= body).then(|| &resp[body..n.min(resp.len())])
}

impl NvidiaBackend {
    /// A render node was opened as `handle`: learn its GPU's semaphore
    /// layout if nothing has yet, and let 0x54 on it be judged by that.
    pub(crate) fn semsurf_render_opened(&mut self, handle: u32, dri: u32) {
        if self.semsurf.layout(dri).is_none() {
            let nodes = self.host_nodes();
            if let Some(d) = nodes.dri.get(dri as usize) {
                match query_layout(&HostRm, d.slot_index, d.dev_info[0]) {
                    Ok(l) => {
                        log::info!(
                            "semaphore surfaces on {}: {}-byte slots, max-submitted at +{}",
                            d.name,
                            l.stride,
                            l.max_submitted
                        );
                        self.semsurf.set_layout(dri, l);
                    }
                    // Asked again at the next open; until then every 0x54
                    // on this node is refused.
                    Err(e) => log::warn!(
                        "semaphore surface layout of {} unknown ({e}); fence contexts \
                         on it are refused",
                        d.name
                    ),
                }
            }
        }
        self.semsurf.render_opened(handle, dri);
    }

    /// The host descriptor for the OS event a guest names at `field` (8
    /// bytes, the guest's handle for the file) under client `h_client`:
    /// `None` for 0 (no notification). EBADF for a number that is none of
    /// our devices, EINVAL for one no live OS event of that client names.
    pub(crate) fn os_event_fd(&self, h_client: u32, field: &[u8]) -> Result<Option<RawFd>, Errno> {
        let v = rd(field, 0, 8).ok_or(libc::EINVAL)?;
        if v == 0 {
            return Ok(None);
        }
        let handle = u32::try_from(v).map_err(|_| libc::EBADF)?;
        let fd = match self.handles.get(handle) {
            Some((fd, crate::hostfd::HandleKind::Dev(_))) => fd.as_raw_fd(),
            _ => {
                log::warn!("RM call names OS event handle {v:#x}, which is none of our devices");
                return Err(libc::EBADF);
            }
        };
        if !self.semsurf.os_event_live(h_client, handle) {
            log::warn!(
                "RM call names handle {handle} as an OS event of client {h_client:#x}, and no \
                 such event is live; refused"
            );
            return Err(libc::EINVAL);
        }
        Ok(Some(fd))
    }

    /// After a v1 RM call on `issuer` answered `resp` (`n` bytes): what it
    /// allocated or freed that the checks above rest on. `param_in` is what
    /// the guest sent.
    pub(crate) fn semsurf_track_rm(
        &self,
        escape: u32,
        issuer: u32,
        param_in: &[u8],
        resp: &[u8],
        n: usize,
    ) {
        use abi::ioctl::*;
        let word = |b: &[u8], at: usize| rd(b, at, 4).map(|v| v as u32);
        match escape {
            NV_ESC_RM_ALLOC => {
                let is_client = word(param_in, 12).is_some_and(|c| ROOT_CLASSES.contains(&c));
                if let Some(out) = reply_params(resp, n).filter(|_| is_client) {
                    if word(out, 40) == Some(0) {
                        if let Some(h) = word(out, 8).filter(|&h| h != 0) {
                            self.semsurf.client_allocated(issuer, h);
                        }
                    }
                }
            }
            // NVOS00 {hRoot, hObjectParent, hObjectOld, status}: freeing the
            // root frees the client.
            NV_ESC_RM_FREE => {
                if let (Some(root), Some(old)) = (word(param_in, 0), word(param_in, 8)) {
                    if root == old {
                        self.semsurf.client_freed(root);
                    }
                }
            }
            // nv_ioctl_{alloc,free}_os_event_t {hClient, hDevice, fd,
            // Status}: `fd` is our handle as the guest sent it (restored in
            // the reply), -1 for none.
            NV_ESC_ALLOC_OS_EVENT => {
                if let Some(out) = reply_params(resp, n) {
                    if let (Some(c), Some(fd), Some(0)) =
                        (word(out, 0), word(out, 8), word(out, 12))
                    {
                        if (fd as i32) >= 0 {
                            self.semsurf.os_event_allocated(issuer, c, fd);
                        }
                    }
                }
            }
            NV_ESC_FREE_OS_EVENT => {
                if let (Some(c), Some(fd)) = (word(param_in, 0), word(param_in, 8)) {
                    self.semsurf.os_event_freed(c, fd);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfd::HandleKind;
    use crate::schema::SchemaClass;
    use crate::xfer;
    use std::cell::RefCell;
    use std::sync::Arc;

    const L: Layout = Layout {
        stride: 32,
        max_submitted: 24,
    };

    // ── the bound ──

    #[test]
    fn the_last_semaphore_of_a_surface_is_accepted() {
        assert_eq!(check_index(L, 0, 32), Ok(()));
        assert_eq!(check_index(L, 127, 4096), Ok(()));
    }

    #[test]
    fn a_semaphore_one_slot_past_the_surface_is_refused() {
        // (index + 1) * stride == size + 32, and a size one byte short.
        assert_eq!(check_index(L, 128, 4096), Err(libc::EINVAL));
        assert_eq!(check_index(L, 127, 4095), Err(libc::EINVAL));
        assert_eq!(check_index(L, 0, 0), Err(libc::EINVAL));
    }

    #[test]
    fn an_index_whose_offset_wraps_is_refused() {
        // index * 32 wraps to 0: the host's unchecked multiply would land
        // at the start of the mapping, or anywhere a guest aims.
        assert_eq!(check_index(L, 1 << 59, 4096), Err(libc::EINVAL));
        assert_eq!(check_index(L, u64::MAX, u64::MAX), Err(libc::EINVAL));
        assert_eq!(
            check_index(L, (u64::MAX / 32) - 1, u64::MAX),
            Ok(()),
            "a huge surface is only refused where the arithmetic would wrap"
        );
        let wide = Layout {
            stride: 1 << 40,
            max_submitted: 0,
        };
        assert_eq!(check_index(wide, 1 << 24, u64::MAX), Err(libc::EINVAL));
    }

    #[test]
    fn a_max_submitted_word_past_the_slot_is_refused() {
        // A layout whose max-submitted word sits past its own slot (no RM
        // answers this, but the bound must not assume it cannot).
        let odd = Layout {
            stride: 16,
            max_submitted: 24,
        };
        assert_eq!(check_index(odd, 0, 32), Ok(()));
        assert_eq!(check_index(odd, 1, 32), Err(libc::EINVAL));
        // And a slot narrower than the 8-byte value read at its start.
        let narrow = Layout {
            stride: 4,
            max_submitted: 0,
        };
        assert_eq!(check_index(narrow, 1, 8), Err(libc::EINVAL));
        assert_eq!(check_index(narrow, 0, 8), Ok(()));
    }

    #[test]
    fn a_host_without_semaphore_surfaces_refuses_every_index() {
        let none = Layout {
            stride: 0,
            max_submitted: 0,
        };
        assert_eq!(check_index(none, 0, 4096), Err(libc::EOPNOTSUPP));
    }

    // ── the policy ──

    const RENDER: u32 = 20;
    const CLIENT: u32 = 0xc1d0_0001;

    fn params(h_client: u32, size: u64) -> [u8; 16] {
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&h_client.to_le_bytes());
        p[4..8].copy_from_slice(&0x5e5u32.to_le_bytes());
        p[8..16].copy_from_slice(&size.to_le_bytes());
        p
    }

    fn policy() -> SemsurfPolicy {
        let s = SemsurfPolicy::new();
        s.set_layout(0, L);
        s.render_opened(RENDER, 0);
        s.client_allocated(3, CLIENT);
        s
    }

    #[test]
    fn a_context_on_the_vms_own_client_is_admitted() {
        assert_eq!(policy().admit(RENDER, 3, &params(CLIENT, 4096)), Ok(()));
    }

    #[test]
    fn a_foreign_rm_client_is_refused() {
        let s = policy();
        assert_eq!(
            s.admit(RENDER, 0, &params(0xc1d0_0002, 4096)),
            Err(libc::EPERM),
            "a client this VM never allocated: another tenant's"
        );
    }

    #[test]
    fn a_client_freed_and_reallocated_elsewhere_is_refused() {
        let s = policy();
        s.client_freed(CLIENT);
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Err(libc::EPERM));
        // Allocated again through us, it is ours again.
        s.client_allocated(4, CLIENT);
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Ok(()));
        // And a client dies with the file it was allocated through.
        s.forget_handle(4);
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Err(libc::EPERM));
    }

    #[test]
    fn a_render_node_whose_layout_is_unknown_admits_nothing() {
        let s = SemsurfPolicy::new();
        s.render_opened(RENDER, 1);
        s.client_allocated(3, CLIENT);
        assert_eq!(
            s.admit(RENDER, 0, &params(CLIENT, 4096)),
            Err(libc::EOPNOTSUPP)
        );
        assert_eq!(
            s.admit(99, 0, &params(CLIENT, 4096)),
            Err(libc::EOPNOTSUPP),
            "nor a handle that is no render node we opened"
        );
    }

    #[test]
    fn an_import_block_of_the_wrong_size_is_refused() {
        let s = policy();
        assert_eq!(
            s.admit(RENDER, 0, &params(CLIENT, 4096)[..8]),
            Err(libc::EINVAL)
        );
        assert_eq!(s.admit(RENDER, 0, &[0u8; 24]), Err(libc::EINVAL));
    }

    #[test]
    fn contexts_past_the_per_file_cap_are_refused_until_one_is_closed() {
        let s = policy();
        for gem in 1..=CTX_CAP_PER_FILE as u32 {
            assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Ok(()));
            s.lock().ctxs.entry(RENDER).or_default().insert(gem);
        }
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Err(libc::ENOSPC));
        s.gem_closed(RENDER, 5);
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Ok(()));
        s.forget_handle(RENDER);
        assert_eq!(s.ctx_counts(RENDER), (0, 0));
    }

    #[test]
    fn contexts_past_the_per_session_cap_are_refused_on_any_file() {
        let s = policy();
        {
            let mut g = s.lock();
            for f in 0..(CTX_CAP_PER_SESSION / CTX_CAP_PER_FILE) as u32 {
                g.ctxs
                    .insert(1000 + f, (1..=CTX_CAP_PER_FILE as u32).collect());
            }
        }
        assert_eq!(s.admit(RENDER, 0, &params(CLIENT, 4096)), Err(libc::ENOSPC));
        s.reset();
        assert_eq!(s.ctx_counts(RENDER), (0, 0));
    }

    #[test]
    fn a_reset_forgets_clients_events_and_files_but_not_the_hosts_layout() {
        let s = policy();
        s.os_event_allocated(3, CLIENT, 7);
        s.reset();
        assert!(!s.owns_client(CLIENT));
        assert!(!s.os_event_live(CLIENT, 7));
        assert_eq!(s.layout(0), Some(L));
    }

    #[test]
    fn os_events_end_with_their_client_their_file_or_a_free() {
        let s = policy();
        s.os_event_allocated(3, CLIENT, 7);
        assert!(s.os_event_live(CLIENT, 7));
        assert!(!s.os_event_live(0xdead, 7), "keyed by client too");
        s.os_event_freed(CLIENT, 7);
        assert!(!s.os_event_live(CLIENT, 7));

        s.os_event_allocated(3, CLIENT, 7);
        s.client_freed(CLIENT);
        assert!(!s.os_event_live(CLIENT, 7));

        s.os_event_allocated(3, CLIENT, 7);
        s.forget_handle(7);
        assert!(!s.os_event_live(CLIENT, 7), "the file it names closed");
        s.os_event_allocated(3, CLIENT, 7);
        s.forget_handle(3);
        assert!(
            !s.os_event_live(CLIENT, 7),
            "the file it was made on closed"
        );
    }

    #[test]
    fn the_os_event_field_is_found_per_command_and_class() {
        let mut ctl = [0u8; 32];
        ctl[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        ctl[8..12].copy_from_slice(&SEMSURF_REGISTER_WAITER.to_le_bytes());
        assert_eq!(os_event_field(0x2a, &ctl, 32), Ok(Some((24, CLIENT))));
        assert_eq!(os_event_field(0x2a, &ctl, 31), Err(libc::EINVAL));
        ctl[8..12].copy_from_slice(&SEMSURF_UNREGISTER_WAITER.to_le_bytes());
        assert_eq!(os_event_field(0x2a, &ctl, 24), Ok(Some((16, CLIENT))));
        assert_eq!(os_event_field(0x2a, &ctl, 0), Err(libc::EINVAL));
        ctl[8..12].copy_from_slice(&0xda0004u32.to_le_bytes());
        assert_eq!(os_event_field(0x2a, &ctl, 16), Ok(None));

        let mut alloc = [0u8; 48];
        alloc[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        alloc[12..16].copy_from_slice(&NV_EVENT_BUFFER.to_le_bytes());
        assert_eq!(os_event_field(0x2b, &alloc, 72), Ok(Some((40, CLIENT))));
        assert_eq!(
            os_event_field(0x2b, &alloc, 0),
            Err(libc::EINVAL),
            "an event buffer without parameters would be read from our memory"
        );
        alloc[12..16].copy_from_slice(&0x05u32.to_le_bytes());
        assert_eq!(os_event_field(0x2b, &alloc, 0), Ok(None));
    }

    // ── through IOCTL2's preparation ──

    struct Env(Arc<SemsurfPolicy>);

    impl xfer::Env for Env {
        fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)> {
            (handle == RENDER).then(|| {
                let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
                (fd, HandleKind::DriRender(0))
            })
        }

        fn kind(&self, handle: u32) -> Option<HandleKind> {
            (handle == RENDER).then_some(HandleKind::DriRender(0))
        }

        fn nvkms_version(&self) -> Option<abi::version::DriverVersion> {
            None
        }

        fn hooks(&self) -> Arc<dyn xfer::Hooks> {
            crate::policy::BackendHooks::with_state(Default::default(), self.0.clone())
        }
    }

    /// A 0x54 as the guest sends it: the argument and, when `block` is
    /// given, the nested import block its pointer names.
    fn ctx_create(index: u64, block: Option<&[u8]>) -> Vec<u8> {
        let mut arg = [0u8; 32];
        arg[0..8].copy_from_slice(&index.to_le_bytes());
        let mut bufs: Vec<&[u8]> = vec![];
        if let Some(b) = block {
            arg[8..16].copy_from_slice(&0x7000u64.to_le_bytes());
            arg[16..24].copy_from_slice(&(b.len() as u64).to_le_bytes());
        }
        bufs.push(&arg);
        if let Some(b) = block {
            bufs.push(b);
        }
        let pad = |n: usize| n.next_multiple_of(8);
        let data_len: usize = bufs.iter().map(|b| pad(b.len())).sum();
        let mut p = Vec::new();
        for v in [
            SEMSURF_FENCE_CTX_CREATE,
            0,
            bufs.len() as u32,
            0,
            0,
            0,
            data_len as u32,
            RENDER,
        ] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        for b in &bufs {
            p.extend_from_slice(&(b.len() as u32).to_le_bytes());
        }
        for b in &bufs {
            p.extend_from_slice(b);
            p.resize(p.len() + pad(b.len()) - b.len(), 0);
        }
        p
    }

    fn prepare(s: &Arc<SemsurfPolicy>, payload: &[u8]) -> Result<Prepared, Errno> {
        xfer::prepare(
            &Env(s.clone()),
            SchemaClass::Render,
            RENDER,
            HandleKind::DriRender(0),
            payload,
        )
    }

    #[test]
    fn an_out_of_bounds_index_never_reaches_the_host() {
        let s = Arc::new(policy());
        let ok = ctx_create(127, Some(&params(CLIENT, 4096)));
        assert!(prepare(&s, &ok).is_ok());
        for bad in [128, u64::MAX, 1 << 59] {
            let rq = ctx_create(bad, Some(&params(CLIENT, 4096)));
            assert_eq!(prepare(&s, &rq).err(), Some(libc::EINVAL), "index {bad}");
        }
        let foreign = ctx_create(0, Some(&params(0x1234, 4096)));
        assert_eq!(prepare(&s, &foreign).err(), Some(libc::EPERM));
        let none = ctx_create(0, None);
        assert_eq!(prepare(&s, &none).err(), Some(libc::EINVAL));
    }

    #[test]
    fn a_created_context_counts_until_its_gem_is_closed() {
        let s = Arc::new(policy());
        let mut p = prepare(&s, &ctx_create(0, Some(&params(CLIENT, 4096)))).unwrap();
        // What the host writes back: the context's GEM handle.
        p.buffer_mut(0).unwrap()[24..28].copy_from_slice(&9u32.to_le_bytes());
        s.ctx_create_after(&p, -libc::ENOMEM);
        assert_eq!(s.ctx_counts(RENDER), (0, 0), "a failed call made nothing");
        s.ctx_create_after(&p, 0);
        assert_eq!(s.ctx_counts(RENDER), (1, 1));
        s.gem_closed(RENDER, 9);
        assert_eq!(s.ctx_counts(RENDER), (0, 0));
        // Finished after the file went away: nothing is counted for a
        // later handle of the same number.
        s.forget_handle(RENDER);
        s.ctx_create_after(&p, 0);
        assert_eq!(s.ctx_counts(RENDER), (0, 0));
    }

    // ── asking the host ──

    /// An RM that answers like the host's for one GPU: gpuId 0x100 is
    /// device instance 2, subdevice 0; a device of any other instance is
    /// refused as a process without that GPU's file would be.
    #[derive(Default)]
    struct FakeRm {
        opened: RefCell<Vec<String>>,
        calls: RefCell<Vec<String>>,
        no_layout: bool,
    }

    impl Rm for FakeRm {
        fn open(&self, path: &str) -> io::Result<PrivateFd> {
            self.opened.borrow_mut().push(path.to_string());
            Ok(PrivateFd::new(
                std::fs::File::open("/dev/null").unwrap().into(),
            ))
        }

        fn alloc(
            &self,
            _: RawFd,
            root: u32,
            parent: u32,
            class: u32,
            params: Option<&mut [u8]>,
        ) -> Result<u32, String> {
            self.calls
                .borrow_mut()
                .push(format!("alloc {class:#x} under {parent:#x} in {root:#x}"));
            match class {
                NV01_ROOT_CLIENT => Ok(0xc100),
                NV01_DEVICE_0 => match params.map(|p| rd(p, 0, 4)) {
                    Some(Some(2)) => Ok(0xde),
                    _ => Err("RM status 0x1b".into()),
                },
                NV20_SUBDEVICE_0 => Ok(0x5b),
                _ => Err("RM status 0x1a".into()),
            }
        }

        fn control(
            &self,
            _: RawFd,
            client: u32,
            object: u32,
            cmd: u32,
            params: &mut [u8],
        ) -> Result<(), String> {
            self.calls
                .borrow_mut()
                .push(format!("control {cmd:#x} on {object:#x} in {client:#x}"));
            match cmd {
                NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2 => {
                    assert_eq!(params.len(), 32);
                    if rd(params, 0, 4) != Some(0x100) {
                        return Err("RM status 0x1f".into());
                    }
                    params[8..12].copy_from_slice(&2u32.to_le_bytes());
                    Ok(())
                }
                NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT if !self.no_layout => {
                    assert_eq!(params.len(), 32, "RM requires the exact size");
                    params[0..8].copy_from_slice(&24u64.to_le_bytes());
                    params[8..16].copy_from_slice(&16u64.to_le_bytes());
                    params[16..24].copy_from_slice(&32u64.to_le_bytes());
                    Ok(())
                }
                _ => Err("RM status 0x56".into()),
            }
        }
    }

    #[test]
    fn the_layout_is_asked_on_a_private_client_of_the_gpus_own_device() {
        let rm = FakeRm::default();
        assert_eq!(query_layout(&rm, 1, 0x100), Ok(L));
        assert_eq!(*rm.opened.borrow(), ["/dev/nvidiactl", "/dev/nvidia1"]);
        assert_eq!(
            *rm.calls.borrow(),
            [
                "alloc 0x41 under 0x0 in 0x0",
                "control 0x205 on 0xc100 in 0xc100",
                "alloc 0x80 under 0xc100 in 0xc100",
                "alloc 0x2080 under 0xde in 0xc100",
                "control 0x20801352 on 0x5b in 0xc100",
            ]
        );
    }

    #[test]
    fn a_host_that_cannot_say_leaves_the_layout_unknown() {
        let rm = FakeRm {
            no_layout: true,
            ..FakeRm::default()
        };
        assert!(query_layout(&rm, 0, 0x100).is_err());
        assert!(
            query_layout(&FakeRm::default(), 0, 0x200).is_err(),
            "a gpuId RM does not know"
        );
    }

    #[test]
    fn rm_parameter_blocks_have_the_hosts_layout() {
        let a = nvos64(1, 2, 0x80, 0x1122_3344_5566_7788, 56);
        assert_eq!(rd(&a, 0, 4), Some(1));
        assert_eq!(rd(&a, 4, 4), Some(2));
        assert_eq!(rd(&a, 8, 4), Some(0), "hObjectNew 0: RM chooses");
        assert_eq!(rd(&a, 12, 4), Some(0x80));
        assert_eq!(rd(&a, 16, 8), Some(0x1122_3344_5566_7788));
        assert_eq!(rd(&a, 24, 8), Some(0), "no rights requested");
        assert_eq!(rd(&a, 32, 4), Some(56));
        let c = nvos54(1, 2, 0x2080_1352, 0x99, 32);
        assert_eq!(
            [
                rd(&c, 0, 4),
                rd(&c, 4, 4),
                rd(&c, 8, 4),
                rd(&c, 16, 8),
                rd(&c, 24, 4)
            ],
            [Some(1), Some(2), Some(0x2080_1352), Some(0x99), Some(32)]
        );
        assert_eq!(NV_ESC_RM_ALLOC_64, 0xc030_462b);
        assert_eq!(NV_ESC_RM_CONTROL, 0xc020_462a);
        assert_eq!(SEMSURF_FENCE_CTX_CREATE, 0xc020_6454);
    }
}

/// The RM paths that feed and consult the policy, run whole through the
/// backend's v1 dispatch against a host RM that records what it was handed.
#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::hostfd::{self, HandleKind};
    use protocol::messages::{DeviceKind, MsgType};
    use std::cell::RefCell;

    const CLIENT: u32 = 0xc1d0_0001;

    std::thread_local! {
        /// (RM call, notificationHandle the host was handed) on this
        /// test's thread.
        static SEEN: RefCell<Vec<(u32, u64)>> = const { RefCell::new(Vec::new()) };
    }

    fn seen() -> Vec<(u32, u64)> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// A host RM: a client allocation gets CLIENT, an OS event and every
    /// other call succeed, and whatever notificationHandle reaches it is
    /// recorded, read through the parameter pointer as RM reads it.
    unsafe fn fake_rm(_: RawFd, request: u64, arg: *mut u8) -> i32 {
        let request = request as u32;
        // SAFETY: the HostIoctl contract, `arg` holds _IOC_SIZE bytes.
        let a = unsafe { std::slice::from_raw_parts_mut(arg, hostfd::ioc_size(request)) };
        let word = |a: &[u8], at: usize| u32::from_le_bytes(a[at..at + 4].try_into().unwrap());
        let params = |a: &[u8]| u64::from_le_bytes(a[16..24].try_into().unwrap()) as *const u8;
        let note = |what: u32, v: u64| SEEN.with(|s| s.borrow_mut().push((what, v)));
        match (hostfd::ioc_type(request), hostfd::ioc_nr(request)) {
            (b'F', 0x2b) => {
                match word(a, 12) {
                    0x41 => a[8..12].copy_from_slice(&CLIENT.to_le_bytes()),
                    // SAFETY: the backend's copy of the parameters, which
                    // it checked hold the field.
                    NV_EVENT_BUFFER => note(NV_EVENT_BUFFER, unsafe {
                        params(a).add(40).cast::<u64>().read_unaligned()
                    }),
                    _ => {}
                }
                a[40..44].fill(0);
            }
            (b'F', 0x2a) => {
                let cmd = word(a, 8);
                let off = match cmd {
                    SEMSURF_REGISTER_WAITER => 24,
                    SEMSURF_UNREGISTER_WAITER => 16,
                    _ => 0,
                };
                // SAFETY: as above.
                note(cmd, unsafe {
                    params(a).add(off).cast::<u64>().read_unaligned()
                });
                a[28..32].fill(0);
            }
            (b'F', 0xce) => {
                note(0xce, u64::from(word(a, 8)));
                a[12..16].fill(0);
            }
            _ => {}
        }
        0
    }

    fn backend() -> (NvidiaBackend, u32) {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_rm);
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let ctl = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Ctl));
        (be, ctl)
    }

    /// A v1 IOCTL: the top-level block and the nested one after it.
    fn v1(be: &mut NvidiaBackend, handle: u32, cmd: u32, outer: &[u8], nested: &[u8]) -> Vec<u8> {
        v1_deep(be, handle, cmd, outer, nested, None)
    }

    /// The same, with a second-level block the guest says the pointer at
    /// `(offset in the nested block, bytes)` addresses.
    fn v1_deep(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: Option<(u32, &[u8])>,
    ) -> Vec<u8> {
        let nested_offset = if nested.is_empty() { 0 } else { outer.len() };
        let (deep_at, deep) = deep.unwrap_or((0, &[]));
        let mut req = Vec::new();
        // MsgHeader, then IoctlReq {cmd, data_len, nested_offset,
        // nested_len, deep_ptr_offset, deep_len}.
        for v in [
            MsgType::Ioctl as u32,
            handle,
            0,
            0,
            cmd,
            outer.len() as u32,
            nested_offset as u32,
            nested.len() as u32,
            deep_at,
            deep.len() as u32,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        req.extend_from_slice(deep);
        let mut resp = vec![0u8; 4096];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn status(resp: &[u8]) -> i32 {
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    }

    const ALLOC: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
    const CONTROL: u32 = ioc(IOC_RW, b'F', 0x2a, 32);
    const FREE: u32 = ioc(IOC_RW, b'F', 0x29, 16);
    const ALLOC_OS_EVENT: u32 = ioc(IOC_RW, b'F', 0xce, 16);
    const FREE_OS_EVENT: u32 = ioc(IOC_RW, b'F', 0xcf, 16);

    fn alloc_client(be: &mut NvidiaBackend, ctl: u32) {
        let r = v1(be, ctl, ALLOC, &nvos64(0, 0, 0x41, 0, 0), &[]);
        assert_eq!(status(&r), 0);
    }

    /// ALLOC_OS_EVENT of `event` (our handle for the file) under CLIENT.
    fn alloc_os_event(be: &mut NvidiaBackend, on: u32, event: u32) {
        let mut p = [0u8; 16];
        p[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        p[8..12].copy_from_slice(&event.to_le_bytes());
        let r = v1(be, on, ALLOC_OS_EVENT, &p, &[]);
        assert_eq!(status(&r), 0);
    }

    fn waiter(event: u64) -> ([u8; 32], [u8; 32]) {
        let outer = nvos54(CLIENT, 0x5e5, SEMSURF_REGISTER_WAITER, 0x7000, 32);
        let mut nested = [0u8; 32];
        nested[24..32].copy_from_slice(&event.to_le_bytes());
        (outer, nested)
    }

    #[test]
    fn a_client_allocated_through_the_vm_is_its_own_until_freed() {
        let (mut be, ctl) = backend();
        assert!(!be.semsurf.owns_client(CLIENT));
        alloc_client(&mut be, ctl);
        assert!(be.semsurf.owns_client(CLIENT));
        // RM_FREE of the root is the client's end.
        let mut free = [0u8; 16];
        free[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        free[8..12].copy_from_slice(&CLIENT.to_le_bytes());
        assert_eq!(status(&v1(&mut be, ctl, FREE, &free, &[])), 0);
        assert!(!be.semsurf.owns_client(CLIENT));
        // And so is its file's close.
        alloc_client(&mut be, ctl);
        be.close_handle(ctl).unwrap();
        assert!(!be.semsurf.owns_client(CLIENT));
    }

    #[test]
    fn a_waiter_reaches_the_host_naming_the_hosts_descriptor() {
        let (mut be, ctl) = backend();
        alloc_os_event(&mut be, ctl, ctl);
        let host_fd = be.handles.get_raw(ctl).unwrap();
        assert_eq!(seen(), vec![(0xce, host_fd as u64)]);
        assert!(be.semsurf.os_event_live(CLIENT, ctl));

        let (outer, nested) = waiter(u64::from(ctl));
        let r = v1(&mut be, ctl, CONTROL, &outer, &nested);
        assert_eq!(status(&r), 0);
        assert_eq!(seen(), vec![(SEMSURF_REGISTER_WAITER, host_fd as u64)]);
        // The reply carries the caller's value back: header, IoctlResp,
        // the NVOS54, then the parameters.
        let at = 16 + 12 + 32 + 24;
        assert_eq!(&r[at..at + 8], &u64::from(ctl).to_le_bytes());

        // UNREGISTER keeps it at 16.
        let outer = nvos54(CLIENT, 0x5e5, SEMSURF_UNREGISTER_WAITER, 0x7000, 24);
        let mut nested = [0u8; 24];
        nested[16..24].copy_from_slice(&u64::from(ctl).to_le_bytes());
        assert_eq!(status(&v1(&mut be, ctl, CONTROL, &outer, &nested)), 0);
        assert_eq!(seen(), vec![(SEMSURF_UNREGISTER_WAITER, host_fd as u64)]);
    }

    #[test]
    fn a_waiter_without_a_notification_passes_as_it_is() {
        let (mut be, ctl) = backend();
        let (outer, nested) = waiter(0);
        assert_eq!(status(&v1(&mut be, ctl, CONTROL, &outer, &nested)), 0);
        assert_eq!(seen(), vec![(SEMSURF_REGISTER_WAITER, 0)]);
    }

    #[test]
    fn a_waiter_naming_no_live_os_event_never_reaches_the_host() {
        let (mut be, ctl) = backend();
        for (event, want) in [
            // No handle at all, and no handle's number.
            (999, libc::EBADF),
            (1 << 40, libc::EBADF),
            // A device, but no event on it.
            (u64::from(ctl), libc::EINVAL),
        ] {
            let (outer, nested) = waiter(event);
            let r = v1(&mut be, ctl, CONTROL, &outer, &nested);
            assert_eq!(status(&r), -want, "event {event:#x}");
        }
        // Freed, it is no longer one either.
        alloc_os_event(&mut be, ctl, ctl);
        let mut free = [0u8; 16];
        free[0..4].copy_from_slice(&CLIENT.to_le_bytes());
        free[8..12].copy_from_slice(&ctl.to_le_bytes());
        assert_eq!(status(&v1(&mut be, ctl, FREE_OS_EVENT, &free, &[])), 0);
        let (outer, nested) = waiter(u64::from(ctl));
        assert_eq!(
            status(&v1(&mut be, ctl, CONTROL, &outer, &nested)),
            -libc::EINVAL
        );
        // Nor does a waiter whose parameters stop short of the field.
        assert_eq!(
            status(&v1(&mut be, ctl, CONTROL, &outer, &nested[..24])),
            -libc::EINVAL
        );
        let host_fd = be.handles.get_raw(ctl).unwrap() as u64;
        assert_eq!(
            seen(),
            vec![(0xce, host_fd)],
            "only the OS event reached it"
        );
    }

    /// NV_EVENT_BUFFER keeps a notification handle RM could not look up as
    /// the event pointer it later dereferences: the host oops of H-2.
    #[test]
    fn an_event_buffer_is_allocated_only_on_a_live_os_event() {
        let (mut be, ctl) = backend();
        let outer = nvos64(CLIENT, 0x5b, NV_EVENT_BUFFER, 0x7000, 72);
        let with = |event: u64| {
            let mut n = [0u8; 72];
            n[40..48].copy_from_slice(&event.to_le_bytes());
            n
        };
        for event in [999, u64::from(ctl), 0xffff_ffff_8100_0000] {
            let r = v1(&mut be, ctl, ALLOC, &outer, &with(event));
            assert_ne!(status(&r), 0, "event {event:#x}");
        }
        assert_eq!(
            status(&v1(&mut be, ctl, ALLOC, &outer, &[])),
            -libc::EINVAL,
            "no parameters: RM would read them wherever the guest's pointer lands here"
        );
        assert!(seen().is_empty(), "none of those reached the host");

        assert_eq!(status(&v1(&mut be, ctl, ALLOC, &outer, &with(0))), 0);
        assert_eq!(seen(), vec![(NV_EVENT_BUFFER, 0)]);
        alloc_os_event(&mut be, ctl, ctl);
        let host_fd = be.handles.get_raw(ctl).unwrap() as u64;
        let r = v1(&mut be, ctl, ALLOC, &outer, &with(u64::from(ctl)));
        assert_eq!(status(&r), 0);
        assert_eq!(seen(), vec![(0xce, host_fd), (NV_EVENT_BUFFER, host_fd)]);
        // A second-level pointer aimed at the field would overwrite the
        // translated event with an address of ours after the check.
        let r = v1_deep(
            &mut be,
            ctl,
            ALLOC,
            &outer,
            &with(u64::from(ctl)),
            Some((40, &[0u8; 8])),
        );
        assert_eq!(status(&r), -libc::EINVAL);
        assert!(seen().is_empty());
        // The file the event is on closing ends it.
        be.close_handle(ctl).unwrap();
        assert!(!be.semsurf.os_event_live(CLIENT, ctl));
    }

    /// The real query, where there is a GPU: every render node's GPU answers
    /// the static layout RM has (mem_mgr_ctrl.c:757-766). Only a read-only
    /// control on a private client; nothing the guest could reach.
    #[test]
    fn the_host_answers_the_layout_on_our_private_client() {
        if !std::path::Path::new("/dev/nvidiactl").exists() {
            eprintln!("SKIPPED the_host_answers_the_layout_on_our_private_client: no GPU");
            return;
        }
        let mut be = NvidiaBackend::for_test();
        let nodes = be.host_nodes();
        for d in &nodes.dri {
            let l = query_layout(&HostRm, d.slot_index, d.dev_info[0]);
            eprintln!("{}: {l:?}", d.name);
            let l = l.unwrap();
            assert!(l.stride >= 8 && l.max_submitted + 8 <= l.stride, "{l:?}");
        }
    }

    #[test]
    fn closing_a_fence_contexts_gem_gives_its_slot_back() {
        let (mut be, _) = backend();
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let render = be.adopt_for_test(null, HandleKind::DriRender(0));
        be.semsurf.render_opened(render, 0);
        be.semsurf
            .lock()
            .ctxs
            .entry(render)
            .or_default()
            .extend([7, 8]);
        let mut close = [0u8; 8];
        close[0..4].copy_from_slice(&7u32.to_le_bytes());
        let r = v1(&mut be, render, hostfd::DRM_IOCTL_GEM_CLOSE, &close, &[]);
        assert_eq!(status(&r), 0);
        assert_eq!(be.semsurf.ctx_counts(render), (1, 1));
        be.close_handle(render).unwrap();
        assert_eq!(be.semsurf.ctx_counts(render), (0, 0));
    }
}
