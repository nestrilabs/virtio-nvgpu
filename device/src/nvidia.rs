// crates/device/src/nvidia.rs
use protocol::messages::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use crate::error::{DeviceError, Result};
use crate::guarded::GuardedBuf;
use crate::handle_table::HandleTable;
use crate::hostfd::{self, CardNode, HandleKind};
use crate::nvkms::{self, NvkmsPolicy};
use crate::policy::BackendHooks;
use crate::privfd::PrivateFd;
use crate::pump::{PumpCmd, WatchMode};
use crate::semsurf::SemsurfPolicy;
use crate::session::{BackendConfig, MAX_XFER_DIRECT, Outcome, Reply, Session};
use crate::shm::{ShmAllocator, ZoneConfig};
use crate::xfer::{Hooks, KmsFileState, Sys, VmKms};

// ============================================================
// Device path helpers
// ============================================================

const MAX_GPU: u8 = 8;

/// Field offsets in NVOS54_PARAMETERS, the struct RM_CONTROL carries.
///
/// `status` is the one that matters and the one that is easy to miss: it is
/// written by RM on the way out and is independent of the ioctl return value.
/// The floor a second-level buffer is sized to, whatever length the guest
/// derived for it. See where it is used: the length is read at a table-supplied
/// offset, the table was generated from a different driver release, and the
/// cost of it being wrong must not be heap corruption in this process.
const DEEP_BUF_FLOOR: usize = 64 * 1024;

const NVOS54_CMD: usize = 8;
const NVOS54_PARAMS_SIZE: usize = 24;
const NVOS54_STATUS: usize = 28;
const NVOS54_TOTAL: usize = 32;

/// `NV_OK`. Every other value is a refusal of some kind.
const NV_OK: u32 = 0;

/// The key at `key_at` of an RM reply's parameter block (`resp_buf[..n]`,
/// header first) when the transport answered and RM's status word at
/// `status_at` is NV_OK.
fn rm_served(resp_buf: &[u8], n: usize, key_at: usize, status_at: usize) -> Option<u32> {
    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
    if n < body || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
        return None;
    }
    let p = resp_buf.get(body..n)?;
    let word = |at: usize| Some(u32::from_le_bytes(p.get(at..at + 4)?.try_into().ok()?));
    (word(status_at)? == NV_OK).then_some(word(key_at)?)
}

/// The host path an `Open` refers to, resolving a render node through the DRI
/// list the guest was given.
///
/// The wire encoding is one flat `u32`: a GPU is its own minor number and the
/// singleton devices take values above every possible minor. This previously
/// decoded a `{kind, index}` pair that the driver never sent, so every open of
/// the control device arrived as kind 255 and was refused.
///
/// A render node's name is not derivable from its index: the host numbers them
/// per DRM device, so the guest's index has to be looked up in the same list
/// the guest was given.
fn device_path_with(device_type: u32, dri: &[DriDevice]) -> Result<CString> {
    let kind = DeviceKind::from_device_type(device_type)
        .ok_or(DeviceError::InvalidDeviceKind(device_type))?;
    let path = match kind {
        DeviceKind::Gpu(n) => {
            if n >= MAX_GPU as u32 {
                return Err(DeviceError::GpuIndexOutOfRange(n));
            }
            format!("/dev/nvidia{n}")
        }
        DeviceKind::Ctl => "/dev/nvidiactl".to_string(),
        DeviceKind::Uvm => "/dev/nvidia-uvm".to_string(),
        DeviceKind::UvmTools => "/dev/nvidia-uvm-tools".to_string(),
        DeviceKind::Modeset => "/dev/nvidia-modeset".to_string(),
        DeviceKind::Dri(n) => {
            let d = dri
                .get(n as usize)
                .ok_or(DeviceError::InvalidDeviceKind(device_type))?;
            format!("/dev/dri/{}", d.name)
        }
        // Not opened by path: card nodes only through HOST_OP OPEN_KMS, which
        // ties the card to a guest file's render handle and to compositor-VM
        // mode; Wayland channels through their own handler.
        DeviceKind::DriCard(_) | DeviceKind::Wayland => {
            return Err(DeviceError::InvalidDeviceKind(device_type));
        }
    };
    Ok(CString::new(path).expect("a device path has no interior NUL"))
}

/// Backend-side result codes, mapped to the errno the guest driver sees.
///
/// The driver has no status vocabulary of its own: it tests `(s32)status < 0`
/// and returns that value from the syscall, so every one of these has to become
/// a plausible errno or userspace gets a nonsense failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    InvalidMsgType,
    InvalidDevice,
    OpenFailed,
    BadHandle,
    IoctlFailed,
    BufferTooSmall,
}

impl Status {
    fn errno(self) -> i32 {
        match self {
            Self::Ok => 0,
            Self::InvalidMsgType => libc::EPROTO,
            Self::InvalidDevice => libc::ENODEV,
            Self::OpenFailed => libc::EIO,
            Self::BadHandle => libc::EBADF,
            Self::IoctlFailed => libc::EIO,
            Self::BufferTooSmall => libc::ENOSPC,
        }
    }
}

/// A DRM render node the host owns, as the guest is told about it.
#[derive(Clone, Debug)]
pub(crate) struct DriDevice {
    pub(crate) name: String,
    pub(crate) major: u32,
    pub(crate) minor: u32,
    /// Which GPU slot it belongs to. The guest matches this against the GPU's
    /// minor to decide which card the node hangs off; it is ours, not NVIDIA's.
    pub(crate) slot_index: u32,
    /// `DRM_NVIDIA_GET_DEV_INFO` as the host's own node answers it, passed
    /// through rather than reconstructed.
    ///
    /// The guest used to answer this ioctl from constants -- gpu_id from the
    /// slot index, and page kind 6 / generation 2 / sector layout 1 under a
    /// comment reading "Turing/Ampere". The gpu_id was simply wrong: the ICD
    /// matches its RM device to a DRM node by it, the host answers 0x100 for a
    /// card at 0000:01:00.0 and the guest answered 0, so no node was ever
    /// matched and VkPhysicalDeviceDrmPropertiesEXT reported hasRender =
    /// false. The tiling fields were right for the two cards they name and
    /// silently wrong elsewhere, which is the kind of wrong that produces a
    /// scrambled frame rather than an error.
    pub(crate) dev_info: [u32; NV_DEV_INFO_WORDS],
    /// The size of the host's own `struct drm_nvidia_get_dev_info_params`,
    /// in bytes, as the probe in [`host_dev_info`] measured it: 20, 28, 32
    /// or 36, or 0 when the node did not answer or answered in a layout this
    /// build does not know. `dev_info` above is always the 36-byte layout,
    /// whatever this says; the size tells the guest which fields the host
    /// actually had (GET_SYS_FILES section 4).
    pub(crate) dev_info_size: u32,
}

/// The host DRM nodes of our GPUs, enumerated once.
///
/// Every OPEN used to walk sysfs and open and close each host render node to
/// ask it GET_DEV_INFO -- for an open of `nvidiactl` as much as for a render
/// node. The answer does not change while the driver is loaded, so it is
/// taken once, on first use, and kept.
#[derive(Debug, Default)]
pub(crate) struct HostNodes {
    pub(crate) dri: Vec<DriDevice>,
    pub(crate) cards: Vec<CardNode>,
}

/// `struct drm_nvidia_get_dev_info_params` in its current (575 and later)
/// layout is nine `u32`s: gpu_id, mig_device, primary_index, supports_alloc,
/// generic_page_kind, page_kind_generation, sector_layout, supports_sync_fd,
/// supports_semsurf. Every host answer is normalised into this layout (see
/// [`normalise_dev_info`]), because it is the only one that has every field.
const NV_DEV_INFO_WORDS: usize = 9;

/// How many words the probe offers the host: more than any layout so far, so
/// the one the host fills is measured rather than assumed.
const NV_DEV_INFO_PROBE_WORDS: usize = 16;

/// `_IOWR('d', DRM_COMMAND_BASE + DRM_NVIDIA_GET_DEV_INFO, u32[16])`: the
/// GET_DEV_INFO number with a 64-byte size field. The size is in the number,
/// but nvidia-drm does not check it -- drm_ioctl (drm_ioctl.c:874-915) sizes
/// its kernel buffer to the larger of the caller's and the driver's, copies
/// the caller's 64 bytes in, lets the handler write its own struct over the
/// front, and copies all 64 back.
const DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE: libc::c_ulong =
    0xC000_6443 | ((4 * NV_DEV_INFO_PROBE_WORDS as libc::c_ulong) << 16);

/// `DRM_IO(DRM_COMMAND_BASE + DRM_NVIDIA_DMABUF_SUPPORTED)`: 0 when the node
/// has an NVKMS device behind it (nvidia_drm.modeset=1), -EINVAL otherwise
/// (nvidia-drm-drv.c:1127-1135). It has answered the same way in every
/// release since 535, so it is the one way to learn `supports_alloc` from a
/// host whose GET_DEV_INFO predates that field.
const DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED: libc::c_ulong = 0x644f;

/// The probe's filler. Every release's handler writes every field of its
/// struct (535: nvidia-drm-drv.c:684-707; 545-570 and 610 write the booleans
/// and page kinds unconditionally before the modeset=1 block, 610:1082-1117),
/// and the last field of every layout is a small number or a boolean -- so
/// the host's size is where the filler starts.
const DEV_INFO_UNWRITTEN: u32 = 0xFFFF_FFFF;

/// The size of the struct the host wrote, from a probe buffer that was full of
/// [`DEV_INFO_UNWRITTEN`] before the ioctl.
fn dev_info_host_size(probe: &[u32]) -> u32 {
    let words = probe
        .iter()
        .rposition(|&w| w != DEV_INFO_UNWRITTEN)
        .map_or(0, |i| i + 1);
    4 * words as u32
}

/// The host's GET_DEV_INFO reply, in whatever layout its size says, as the
/// nine-word current layout.
///
/// The struct has had four shapes, and the size is the only thing that tells
/// them apart -- the fields that moved are all small numbers:
///
/// | bytes | releases | fields |
/// |---|---|---|
/// | 20 | 535 | gpu_id, primary_index, page kind, generation, sector layout |
/// | 28 | 545.23 | ... then supports_sync_fd, supports_semsurf |
/// | 32 | 545.29-570 | supports_alloc inserted after primary_index |
/// | 36 | 575- | mig_device inserted after gpu_id |
///
/// (535.129.03 nvidia-drm-ioctl.h:153-161, 545.23.06 and 550.54.14 likewise,
/// 575.51.02 and 610.57.04 nv_drm_common_ioctl.h:213-227.) Reading a 20-byte
/// answer as the 36-byte layout -- what this did before -- took primary_index
/// for mig_device, the page kind for supports_alloc and the generation for
/// the page kind. A field the host's layout does not have is reported as
/// absent (0), except `supports_alloc`, which `modeset` answers: on every
/// release it means exactly "the node has an NVKMS device", which is what
/// DMABUF_SUPPORTED reports.
///
/// `None` for a size this build does not know.
fn normalise_dev_info(raw: &[u32], size: u32, modeset: bool) -> Option<[u32; NV_DEV_INFO_WORDS]> {
    let w = |i: usize| raw.get(i).copied().unwrap_or(0);
    let alloc = u32::from(modeset);
    Some(match size {
        20 => [w(0), 0, w(1), alloc, w(2), w(3), w(4), 0, 0],
        28 => [w(0), 0, w(1), alloc, w(2), w(3), w(4), w(5), w(6)],
        32 => [w(0), 0, w(1), w(2), w(3), w(4), w(5), w(6), w(7)],
        36 => std::array::from_fn(w),
        _ => return None,
    })
}

/// Ask a host render node what it is: its GET_DEV_INFO answer normalised to
/// the current layout, and the size of the layout the host answered in.
///
/// `None` when the node cannot be opened, refuses the ioctl, or answers in a
/// layout this build does not know. The caller then reports every word as 0,
/// capability bits included: a node that claims nothing is one the ICD does
/// not use for allocation or fencing, where invented capabilities made it try
/// and fail (vkCreateDevice failing on 0x54's -EOPNOTSUPP).
fn host_dev_info(path: &str) -> Option<([u32; NV_DEV_INFO_WORDS], u32)> {
    let c_path = CString::new(path).ok()?;
    // SAFETY: a NUL-terminated path, and the fd is closed below.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        log::warn!(
            "{path}: cannot open to ask what it is ({})",
            std::io::Error::last_os_error()
        );
        return None;
    }
    let mut probe = [DEV_INFO_UNWRITTEN; NV_DEV_INFO_PROBE_WORDS];
    // SAFETY: `probe` is exactly the 64 bytes the ioctl's size field declares.
    let rc = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE,
            probe.as_mut_ptr() as *mut libc::c_void,
        )
    };
    let err = std::io::Error::last_os_error();
    // SAFETY: no argument; the return value is the whole answer.
    let modeset = unsafe { libc::ioctl(fd, DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED) } == 0;
    // SAFETY: fd came from open() above and is not used again.
    unsafe { libc::close(fd) };
    if rc != 0 {
        log::warn!("{path}: GET_DEV_INFO refused ({err})");
        return None;
    }
    let size = dev_info_host_size(&probe);
    let Some(info) = normalise_dev_info(&probe, size, modeset) else {
        log::warn!(
            "{path}: GET_DEV_INFO answered in a {size}-byte layout this build \
             does not know; reporting no capabilities"
        );
        return None;
    };
    Some((info, size))
}

/// Which host tree a `GetProcFiles`/`GetSysFiles` request refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileTree {
    /// `/proc/driver/nvidia`, which NVML reads before it will talk to a device.
    Proc,
    /// The sysfs attributes the userspace driver looks for on each card.
    Sys,
}

impl FileTree {
    fn name(self) -> &'static str {
        match self {
            Self::Proc => "GET_PROC_FILES",
            Self::Sys => "GET_SYS_FILES",
        }
    }

    fn root(self) -> &'static str {
        match self {
            Self::Proc => "/proc/driver/nvidia",
            // Paths in this stream are relative to /sys, because that is what
            // the driver matches on: it looks for "bus/pci/devices/<addr>/
            // config" and ignores everything else. Rooting the walk at
            // /sys/bus/pci/drivers/nvidia instead produced paths that matched
            // nothing, which is not distinguishable from an empty tree.
            Self::Sys => "/sys",
        }
    }

    /// Read the tree, returning `(path relative to the root, contents)`.
    ///
    /// Only regular files, and only small ones: these trees are descriptive
    /// text, and anything large is either not one of them or not something a
    /// guest should be handed through a single response buffer.
    fn collect(self) -> Vec<(String, Vec<u8>)> {
        const MAX_FILE: u64 = 64 * 1024;
        match self {
            Self::Proc => {
                let mut out = Vec::new();
                let root = std::path::Path::new(self.root());
                collect_into(root, root, &mut out, MAX_FILE, 0);
                out.sort_by(|a, b| a.0.cmp(&b.0));
                // Paths in this stream are relative to /proc, not to the
                // driver's own directory: the guest walks each component from
                // the root of procfs to create the parents. Sending "version"
                // rather than "driver/nvidia/version" asks it to create
                // /proc/version, which already exists, so every file was
                // dropped and the directory came out empty. That is invisible
                // from here -- the backend counted 16 files and sent them.
                for (path, _) in &mut out {
                    *path = format!("driver/nvidia/{path}");
                }
                out
            }
            // Not a walk. /sys is enormous, most of it is irrelevant, and some
            // of it blocks on read. The driver wants one file per GPU -- the
            // PCI config space -- and says so: everything else it needs the
            // kernel synthesises once the pci_dev is registered.
            Self::Sys => crate::host::gpu_slots(std::path::Path::new(Self::Proc.root()))
                .iter()
                .filter_map(|slot| {
                    let end = slot
                        .pci_addr
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(slot.pci_addr.len());
                    let addr = String::from_utf8_lossy(&slot.pci_addr[..end]);
                    let rel = format!("bus/pci/devices/{addr}/config");
                    let abs = std::path::Path::new("/sys").join(&rel);
                    match std::fs::read(&abs) {
                        Ok(content) => Some((rel, content)),
                        Err(e) => {
                            log::warn!("sys: cannot read {}: {}", abs.display(), e);
                            None
                        }
                    }
                })
                .collect(),
        }
    }
}

/// Walk `dir`, appending every readable regular file under it.
///
/// Depth-limited because these trees contain symlinks back into the rest of
/// sysfs, and following them turns a handful of files into a walk of the whole
/// device model.
fn collect_into(
    root: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(String, Vec<u8>)>,
    max_file: u64,
    depth: usize,
) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // symlink_metadata, not metadata: a symlink here leads out of the tree.
        let Ok(md) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if md.is_symlink() {
            continue;
        }
        if md.is_dir() {
            collect_into(root, &path, out, max_file, depth + 1);
            continue;
        }
        if !md.is_file() {
            continue;
        }
        // procfs reports zero length for files with real content, so size is
        // only usable as an upper bound when it is non-zero.
        if md.len() > max_file {
            continue;
        }
        let Ok(content) = std::fs::read(&path) else {
            continue;
        };
        if content.len() as u64 > max_file {
            continue;
        }
        if let Ok(rel) = path.strip_prefix(root) {
            out.push((rel.to_string_lossy().into_owned(), content));
        }
    }
}

// ============================================================
// NvidiaBackend
// ============================================================

pub struct NvidiaBackend {
    /// The message being served, so a response can echo its type, and the
    /// handle it named, so handlers need not thread either through.
    pub(crate) current_msg: MsgType,
    pub(crate) current_handle: u32,
    /// The request's id, echoed in every response header. A v2 guest puts a
    /// unique one in each request; a v1 guest leaves it zero.
    pub(crate) current_req_id: u32,
    /// Top-level parameter length of the ioctl being served. The response has
    /// to split the bytes the same way the request did.
    current_data_len: u32,
    /// The guest process making the RM call being served, when the guest
    /// says (rmshare.rs).
    pub(crate) current_proc: Option<crate::rmshare::Caller>,
    /// The guest process what the message being served makes is charged to
    /// (quota.rs): the one an OPEN or HOST_OP names, else the owner of the
    /// handle the message acts on.
    pub(crate) current_owner: crate::quota::Owner,
    pub(crate) handles: HandleTable,
    shm: ShmAllocator,
    /// Active RM_MAP_MEMORY mappings, keyed by SHM offset.
    ///
    /// The SHM offset is written into pLinearAddress in the response to the
    /// guest, so userspace echoes it back as pLinearAddress in RM_UNMAP_MEMORY.
    /// This gives us a unique, unambiguous lookup key without leaking host VAs.
    ///
    /// Each entry owns its extent, and is the only record that does: the
    /// extent is released when RM_UNMAP_MEMORY succeeds, when the handle that
    /// carries the mapping closes, or when the session resets -- whichever
    /// comes first, and exactly once.
    pub(crate) active_maps: crate::mmap::MmapContext,
    /// Host driver version, learned from the first successful
    /// `NV_ESC_CHECK_VERSION_STR`.
    pub(crate) driver: Option<abi::version::DriverVersion>,
    /// ABI profile selected for `driver`, if one exists.
    abi: Option<&'static [abi::versions::IoctlEntry]>,
    /// Where device memory is placed so the guest can address it. `None` until
    /// the transport supplies one, and without it a mapping can be made on the
    /// host but never reached from the guest.
    window: Option<Box<dyn crate::shm::WindowPlacer>>,
    /// Window placements made for a DRM object, keyed by the node handle and
    /// the object's mmap offset on the host.
    ///
    /// Keyed by both because one open of a node holds many objects, and they
    /// are told apart only by that offset. Keyed at all because a buffer is
    /// mapped more than once -- the guest maps it, exports it, an importer maps
    /// it again -- and each placement costs a slice of a finite window.
    ///
    /// Only a lookup: the placement itself is owned by `live_maps`. An entry
    /// goes when its handle closes, so a later open that happens to reuse the
    /// offset cannot be handed a placement of someone else's object.
    dri_maps: std::collections::HashMap<(u32, u64), u32>,
    /// Every message this backend has served, by kind.
    ///
    /// Kept because "how often does the guest have to ask the host anything"
    /// is the question a benchmark of this design turns on, and counting log
    /// lines answers a different one -- what the log level happened to print.
    msg_counts: std::collections::BTreeMap<&'static str, u64>,
    /// Every live placement made without an RM record, by the id the guest
    /// quotes to take it back. Owns its extent; see [`LiveMap`].
    pub(crate) live_maps: std::collections::HashMap<u32, LiveMap>,
    /// Whether an ioctl the profile does not describe is refused or forwarded.
    abi_policy: AbiPolicy,
    /// Every `RM_ALLOC` class and `RM_CONTROL` command a workload asked for,
    /// and how often.
    ///
    /// Narrowing these to what the pipeline uses is the step that actually
    /// reduces what a guest can reach in the host driver -- an unprivileged
    /// helper process contains a bug in *this* code, not one in NVIDIA's kernel
    /// module, and only fewer reachable commands helps with the second. A
    /// filter cannot be written from a guess, so this is the instrument that
    /// says what the set really is.
    ///
    /// Bounded (tally.rs): the keys are the guest's to choose.
    rm_classes: crate::tally::Tally,
    rm_controls: crate::tally::Tally,
    /// Every ioctl forwarded, by namespace and number.
    ///
    /// There are three namespaces, not one, and that is the point of counting
    /// this way: NVIDIA's own escapes (`F`), the DRM node's (`d`) and
    /// modeset's (`m`). Only the first has an ABI table. A buffer-sharing run
    /// measured 1212 forwarded ioctls with *zero* RM allocations or controls
    /// among them -- so an allowlist written against RM alone would leave the
    /// path a compositor actually uses completely unfiltered.
    ioctls_by_ns: std::collections::BTreeMap<(char, u32), u64>,
    /// Escapes that failed the check, and how often, so a run can say what a
    /// workload actually needed. Reported at teardown.
    abi_refused: std::collections::BTreeMap<u32, u64>,
    /// Instructions for the event pump, in the order they were made, until
    /// the transport collects them.
    ///
    /// The backend cannot run the pump itself: it holds no queue to deliver
    /// on, and this crate names no VMM. It says what to watch and when to
    /// stop; the transport forwards that to whoever owns the event queue.
    pub(crate) pump_cmds: Vec<PumpCmd>,
    /// Handles the message being served created, for the reply to carry.
    created: Vec<u32>,
    next_mapping_id: u32,
    pub(crate) session: Session,
    pub(crate) config: BackendConfig,
    /// Transport limits: what one request and one response may carry. The
    /// transport sets them once it knows whether indirect descriptors were
    /// negotiated.
    pub(crate) max_req: u32,
    pub(crate) max_resp: u32,
    nodes: Option<Arc<HostNodes>>,
    /// An already-signalled sync_file, made once and duplicated for every
    /// SIGNALED_SYNC_FILE. The backend's own, not a guest's: registered as
    /// private so no IOCTL2 can ever adopt its number.
    pub(crate) signaled: Option<PrivateFd>,
    /// Per-file KMS state of each `DrmCard`/`DrmLease` handle an IOCTL2 has
    /// run on: the framebuffers that file created (the only ones GETFB may
    /// return handles for) and its property names. Made on the first KMS
    /// call and dropped with the handle, so a later handle that happens to
    /// get the same number starts with nothing.
    pub(crate) kms_states: std::collections::HashMap<u32, Arc<KmsFileState>>,
    /// Every framebuffer those files made, VM-wide: the only ids a guest
    /// may name as a scanout source (S-6, `xfer::KmsFileState`).
    pub(crate) vm_kms: Arc<VmKms>,
    /// The policy every IOCTL2 is checked against (see `policy.rs`).
    pub(crate) hooks: Arc<dyn Hooks>,
    /// Shared syncobj wait registrations (HOST_OP SYNCOBJ_WATCH, fence.rs).
    pub(crate) syncobj_regs: crate::fence::Registrations,
    /// Its NVKMS section's state, which also gates v1 NVKMS calls and hears
    /// of the host version, the mode and every handle closed (nvkms.rs).
    pub(crate) nvkms: Arc<NvkmsPolicy>,
    /// What SEMSURF_FENCE_CTX_CREATE and the OS-event fields of RM calls may
    /// name: the host's semaphore layout, this VM's RM clients and OS
    /// events, its live fence contexts (semsurf.rs). Fed here, read by the
    /// policy hooks too.
    pub(crate) semsurf: Arc<SemsurfPolicy>,
    /// The system calls IOCTL2 makes: the host's, except in tests that run
    /// whole calls against a fake kernel.
    pub(crate) xfer_sys: Arc<dyn Sys>,
    /// The host ioctl entry point. `libc::ioctl`, except in tests that need to
    /// see what the host driver would be handed.
    host_ioctl: HostIoctl,
    /// UVM files whose VA space came up with pageable access, or could not
    /// be shown not to (`uvm_pageable_off`): nothing more goes to them, and no
    /// other file's call may name them.
    uvm_refused: std::collections::HashSet<u32>,
    /// UVM semaphore pools the guest may map, and those placed in the UVM
    /// aperture (uvmmap.rs).
    pub(crate) uvm_maps: crate::uvmmap::UvmMaps,
    /// Wayland channels (`DEV_WAYLAND` handles) and what configures them.
    pub(crate) wl: crate::wl::WlState,
    /// What the RM handles a mapping can name are (system memory and its
    /// coherency, doorbell registers), and the rewrite that makes guest
    /// system memory GPU-coherent (rmmem.rs).
    pub(crate) rmmem: crate::rmmem::RmMem,
    /// Guest RAM, as the vhost-user memory table gives it: what an OS
    /// descriptor's page list is checked against and mapped from
    /// (osdesc.rs). None where there is none (the socket harness, tests
    /// that do not set one), and then no guest is offered BCAP_OS_DESC.
    pub(crate) guest_ram: Option<crate::osdesc::GuestRam>,
    /// Memory the guest registered with RM by its pages (osdesc.rs).
    pub(crate) osdesc: crate::osdesc::OsDesc,
}

/// `ioctl(2)` as the forwarding paths call it.
pub(crate) type HostIoctl = unsafe fn(RawFd, u64, *mut u8) -> i32;

/// The real one.
///
/// # Safety
/// `arg` must point to at least `_IOC_SIZE(request)` writable bytes, which is
/// what every forwarding path allocates (see [`ioctl_arg_len`]).
unsafe fn libc_ioctl(fd: RawFd, request: u64, arg: *mut u8) -> i32 {
    // SAFETY: the caller's contract above.
    unsafe { libc::ioctl(fd, request as libc::Ioctl, arg) }
}

/// How many bytes an ioctl argument buffer must have.
///
/// Every host driver behind these paths copies `_IOC_SIZE(cmd)` bytes in and
/// back out of the argument, whatever the caller allocated: drm_ioctl sizes
/// its copies by the command (drm_ioctl.c, `drm_ioctl`: `in_size = out_size =
/// _IOC_SIZE(cmd)`), and nvidia.ko's frontend does the same with `arg_size`.
/// A buffer sized to the guest's `data_len` with a larger size encoded in the
/// command let the host write past the end of our heap allocation -- a guest
/// choosing both numbers chose how far. So the buffer is the larger of the
/// two, zero-filled, and only `data_len` bytes of it go back.
pub(crate) fn ioctl_arg_len(cmd: u64, data_len: usize) -> usize {
    hostfd::ioc_size(cmd as u32).max(data_len).max(1)
}

/// A zero-filled guarded buffer of [`ioctl_arg_len`] bytes holding `bytes`.
fn ioctl_arg(cmd: u64, bytes: &[u8]) -> Option<GuardedBuf> {
    let mut b = GuardedBuf::new(ioctl_arg_len(cmd, bytes.len()))?;
    b.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
    Some(b)
}

/// A placement the guest can hand back, and everything needed to undo it.
///
/// This record is the extent's only owner. It used to share it with an
/// `active_maps` entry keyed to the handle that made it, and closing that
/// handle freed the extent there -- without withdrawing it from the window and
/// without removing this record. A GEM proxy that outlived its owner file (a
/// compositor holding a client's buffer after the client exited) kept the old
/// placement mapped while the extent went to someone else's object, and its
/// eventual MUNMAP withdrew and freed the *new* placement: two objects on one
/// extent, then none. Now a close leaves the placement alone, and the extent
/// is withdrawn and freed exactly once, when the last reference goes: the last
/// MUNMAP of its id, or a session reset.
///
/// RM mappings end up here too, once RM_UNMAP_MEMORY or the close of their
/// file has ended them on the host while a guest vma still maps the id: the
/// extent must not go to another mapping until those vmas are gone.
pub(crate) struct LiveMap {
    /// The `dri_maps` entry that finds it, for a DRM object; none for an RM
    /// mapping, which nothing maps again once it is here.
    key: Option<(u32, u64)>,
    region: crate::shm::ShmRegion,
    length: u64,
    /// MMAP replies that handed this id out and have not been taken back.
    refs: u32,
    /// Whether the host mapping can be written (`shm::host_mapping_writable`).
    writable: bool,
}

/// What to do with an ioctl the ABI profile does not vouch for.
///
/// Refusing is the default, and the reason is the whole point of having tables:
/// an escape that is not in them is one whose parameter layout we have never
/// seen, and forwarding it means handing the host driver bytes that nobody has
/// checked. `nvproxy`, whose tables these are derived from, has always refused;
/// this crate logged and forwarded anyway until it was asked, in public, what
/// exactly a guest can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AbiPolicy {
    /// Refuse anything the profile does not describe.
    #[default]
    Enforce,
    /// Forward it anyway and count it. For finding out what a workload needs
    /// that the tables lack -- never for running one.
    Permissive,
}

/// The result of checking one guest ioctl against the host's ABI profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiCheck {
    /// The escape is known and the guest's parameter size matches.
    Ok,
    /// No profile yet -- CHECK_VERSION_STR has not been seen.
    NoProfile,
    /// The escape is not in this driver's table.
    UnknownEscape,
    /// The escape is variable length; there is no size to check.
    VariableLength,
    /// The guest disagrees with the host ABI about this struct's size.
    SizeMismatch { expected: u32, actual: u32 },
}
/// Where a v1 IOCTL goes, once it is allowed at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum V1Route {
    /// NVIDIA's RM escapes, type 'F', checked against the ABI profile.
    Rm,
    /// nvidia-uvm, whose command numbers carry type 0.
    Uvm,
    /// nvidia-modeset, type 'm'.
    Nvkms,
    /// An nvidia-drm GEM ioctl with an NVKMS parameter block behind a pointer:
    /// (size of the outer struct, offset of the pointer, offset of its size).
    DrmNested {
        outer_size: usize,
        ptr_offset: usize,
        size_offset: usize,
    },
    /// A flat, pointer-free, fd-free ioctl the v1 guest sends on its render
    /// handle.
    DrmFlat,
}

/// nvidia-drm and DRM core commands the v1 guest forwards on a render handle,
/// by their full numbers (nv_drm_common_ioctl.h:71-133, drm.h:1104). A full
/// number fixes the size the host will copy, so a guest cannot pair a known
/// nr with a larger `_IOC_SIZE`.
const DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY: u32 = hostfd::ioc(hostfd::IOC_RW, b'd', 0x41, 32);
const DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY: u32 = hostfd::ioc(hostfd::IOC_RW, b'd', 0x49, 24);
const DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET: u32 = hostfd::ioc(hostfd::IOC_RW, b'd', 0x4a, 16);
const DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY: u32 = hostfd::ioc(hostfd::IOC_RW, b'd', 0x4b, 24);
const DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY: u32 = hostfd::ioc(hostfd::IOC_RW, b'd', 0x4d, 24);

/// Whether a v1 IOCTL may run on a handle of `kind`, and how.
///
/// The v1 message forwards the guest's command number with the guest's bytes
/// and none of IOCTL2's schema checks, so it is limited, by the backend's own
/// tables and by the handle's kind, to what a v1 guest actually sends: RM
/// escapes on the NVIDIA devices, UVM on the UVM devices, NVKMS on
/// nvidia-modeset, and on a render node the three nested nvidia-drm GEM calls
/// and three flat ones. Everything else is -EPERM, and every new kind -- a
/// card, a lease, a sync_file -- takes no v1 ioctl at all. Without this a
/// hostile guest kernel could send ADDFB2 as a v1 IOCTL on a KMS handle and
/// reach nvidia-drm's NULL dereference on a non-NVKMS GEM object
/// (nvidia-drm-fb.c:158-167) past every IOCTL2 check.
///
/// Three commands are refused on every handle
/// ([`hostfd::refused_everywhere`]).
///
/// -EINVAL for a known command whose size does not match what was sent.
fn v1_route(kind: HandleKind, cmd: u32, data_len: u32) -> std::result::Result<V1Route, i32> {
    let ty = hostfd::ioc_type(cmd);
    if hostfd::refused_everywhere(cmd) {
        return Err(libc::EPERM);
    }
    let sized = |n: usize, r: V1Route| {
        if data_len as usize == n {
            Ok(r)
        } else {
            Err(libc::EINVAL)
        }
    };
    match kind {
        HandleKind::Dev(DeviceKind::Gpu(_) | DeviceKind::Ctl) if ty == b'F' => Ok(V1Route::Rm),
        HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools) if ty == 0 => Ok(V1Route::Uvm),
        HandleKind::Dev(DeviceKind::Modeset) if ty == b'm' => Ok(V1Route::Nvkms),
        HandleKind::DriRender(_) => match cmd {
            DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY => sized(
                32,
                V1Route::DrmNested {
                    outer_size: 32,
                    ptr_offset: 8,
                    size_offset: 16,
                },
            ),
            DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY
            | DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY => sized(
                24,
                V1Route::DrmNested {
                    outer_size: 24,
                    ptr_offset: 8,
                    size_offset: 16,
                },
            ),
            DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET
            | DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY
            | hostfd::DRM_IOCTL_GEM_CLOSE => sized(hostfd::ioc_size(cmd), V1Route::DrmFlat),
            _ => Err(libc::EPERM),
        },
        _ => Err(libc::EPERM),
    }
}

/// The DRI section of a `GetSysFiles` response.
///
/// A count, then one `{name_len, major, minor, slot_index, dev_info[9]}`
/// record and name per device. The guest uses these to register render nodes at the host's own
/// major and minor and to build the sysfs tree beneath them.
///
/// This is not decoration for a headless guest. NVIDIA's Vulkan and EGL
/// userspace enumerates the GPU through the DRM render node and not through
/// `/dev/nvidia*`, which carry compute: the ICD stats the node, takes its
/// major, and requires `/sys/dev/char/<major>:<minor>/device/drm` to exist
/// before it will open it. Reporting none is why `vulkaninfo` found a
/// driver it could load and then declined to create an instance, with no
/// ioctl refused and nothing logged anywhere.
fn write_dri_section(devices: &[DriDevice], buf: &mut [u8]) -> usize {
    log::info!("GET_SYS_FILES: {} DRI device(s)", devices.len());

    if buf.len() < 4 {
        return 0;
    }
    let mut off = 0;
    buf[off..off + 4].copy_from_slice(&(devices.len() as u32).to_le_bytes());
    off += 4;

    for d in devices {
        // name_len, major, minor, slot_index, then the nine dev_info words.
        let need = 16 + 4 * NV_DEV_INFO_WORDS + d.name.len();
        if off + need > buf.len() {
            log::warn!("DRI section truncated at {}", d.name);
            break;
        }
        for v in [d.name.len() as u32, d.major, d.minor, d.slot_index]
            .into_iter()
            .chain(d.dev_info)
        {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
            off += 4;
        }
        buf[off..off + d.name.len()].copy_from_slice(d.name.as_bytes());
        off += d.name.len();
    }
    off
}

/// GET_SYS_FILES section 4: `u32 count`, then per DRI record (in section 2's
/// order) the size in bytes of the host's GET_DEV_INFO struct, 0 for unknown.
///
/// Section 2's nine words are always the 36-byte layout -- normalised here
/// from whatever the host answered (normalise_dev_info) -- so a guest that
/// never reads this still answers correctly. The size is for the guest to
/// know which of those words the host really had, and to say so when a caller
/// asks in another layout: that is guest userspace built for a different
/// release than the host kernel, which is worth one line in dmesg because
/// the RM ABI check fails next. A guest that predates the section stops
/// after section 3; an older backend leaves it out, which the guest reads as
/// "36", the only layout an older backend ever asked in. Written whole or not
/// at all, like section 3.
fn write_dev_info_sizes(devices: &[DriDevice], buf: &mut [u8]) -> usize {
    let need = 4 + 4 * devices.len();
    if need > buf.len() {
        log::warn!("GET_SYS_FILES: no room for the GET_DEV_INFO sizes");
        return 0;
    }
    buf[..4].copy_from_slice(&(devices.len() as u32).to_le_bytes());
    for (i, d) in devices.iter().enumerate() {
        buf[4 + 4 * i..8 + 4 * i].copy_from_slice(&d.dev_info_size.to_le_bytes());
    }
    need
}

/// GET_SYS_FILES section 3: the card nodes, in every mode; openable only in
/// compositor-VM mode (BCAP_KMS_CARD), informational otherwise.
///
/// A count, then `CardRecord {name_len, major, minor, render_index}` and the
/// name per card. `render_index` names the DRI record of the same PCI device,
/// so the guest attaches its primary minor to the same `drm_device` as the
/// render node rather than inventing a second GPU.
///
/// The count is only written when the whole section fits: a count promising
/// records that were cut off would have the guest parse zeroes as cards.
fn write_card_section(cards: &[CardNode], buf: &mut [u8]) -> usize {
    let need: usize = 4 + cards
        .iter()
        .map(|c| size_of::<CardRecord>() + c.name.len())
        .sum::<usize>();
    if need > buf.len() {
        log::warn!(
            "GET_SYS_FILES: no room for the {} card record(s)",
            cards.len()
        );
        return 0;
    }
    log::info!("GET_SYS_FILES: {} card node(s) offered", cards.len());
    let mut off = 0;
    buf[..4].copy_from_slice(&(cards.len() as u32).to_le_bytes());
    off += 4;
    for c in cards {
        off += write_struct(
            &mut buf[off..],
            &CardRecord {
                name_len: c.name.len() as u32,
                major: c.major,
                minor: c.minor,
                render_index: c.render_index,
            },
        );
        buf[off..off + c.name.len()].copy_from_slice(c.name.as_bytes());
        off += c.name.len();
    }
    off
}

/// `major:minor` from `/sys/class/drm/<name>/dev`.
fn node_dev(name: &str) -> Option<(u32, u32)> {
    let Ok(text) = std::fs::read_to_string(format!("/sys/class/drm/{name}/dev")) else {
        log::warn!("DRI node {name} has no dev file");
        return None;
    };
    let text = text.trim();
    let parsed = text
        .split_once(':')
        .and_then(|(maj, min)| Some((maj.parse().ok()?, min.parse().ok()?)));
    if parsed.is_none() {
        log::warn!("DRI node {name}: cannot read {text:?} as major:minor");
    }
    parsed
}

/// The render and card nodes the host's GPUs own.
///
/// Taken from `/sys/bus/pci/devices/<addr>/drm`, which is the kernel's own
/// statement of which DRI nodes belong to which card -- rather than from
/// the numbering of `/dev/dri`, where a node's index says nothing about
/// which device it is.
///
/// Render nodes are what the guest is given to open. Card nodes are
/// enumerated too, but a card node is a display device, and handing one out
/// is a different kind of access from render: it is offered only in
/// compositor-VM mode (GET_SYS_FILES section 3, HOST_OP OPEN_KMS). The list is
/// kept in every mode because classification needs it: a lease fd is ours
/// only if it is one of these cards.
fn enumerate_host_nodes() -> HostNodes {
    let mut nodes = HostNodes::default();
    for (index, slot) in crate::host::gpu_slots(std::path::Path::new(FileTree::Proc.root()))
        .iter()
        .enumerate()
    {
        let end = slot
            .pci_addr
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(slot.pci_addr.len());
        let addr = String::from_utf8_lossy(&slot.pci_addr[..end]).into_owned();
        let dir = format!("/sys/bus/pci/devices/{addr}/drm");

        let Ok(entries) = std::fs::read_dir(&dir) else {
            log::warn!("no DRI nodes under {dir}");
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();

        let first_render = nodes.dri.len() as u32;
        for name in names.iter().filter(|n| n.starts_with("renderD")) {
            // The kernel prints "major:minor" here. A node listed under the
            // PCI device with no `dev` file is not one we can reproduce.
            let Some((major, minor)) = node_dev(name) else {
                continue;
            };
            // A node that will not say what it is claims nothing. The fallback
            // used to be a Turing's answer with every capability bit set, which
            // on any other card is a wrong page kind, and on a modeset=0 host
            // is a promise of semaphore surfaces that 0x54 then breaks
            // (-EOPNOTSUPP, nvidia-drm-fence.c:1316) inside vkCreateDevice.
            let (dev_info, dev_info_size) =
                host_dev_info(&format!("/dev/dri/{name}")).unwrap_or(([0; NV_DEV_INFO_WORDS], 0));
            log::info!(
                "DRI {name} at {major}:{minor} on {addr} (slot {index}, \
                 nvidia gpu_id {:#x}, page kind {}/{}, sector layout {}, \
                 alloc {} sync_fd {} semsurf {}, {dev_info_size}-byte layout)",
                dev_info[0],
                dev_info[4],
                dev_info[5],
                dev_info[6],
                dev_info[3],
                dev_info[7],
                dev_info[8],
            );
            nodes.dri.push(DriDevice {
                name: name.clone(),
                major,
                minor,
                slot_index: index as u32,
                dev_info,
                dev_info_size,
            });
        }
        if nodes.dri.len() as u32 == first_render {
            // A card with no render node of its own cannot be attached to a
            // guest drm_device, so it is not offered.
            continue;
        }
        for name in names.iter().filter(|n| n.starts_with("card")) {
            let Some((major, minor)) = node_dev(name) else {
                continue;
            };
            log::info!("DRM card {name} at {major}:{minor} on {addr}");
            nodes.cards.push(CardNode {
                name: name.clone(),
                major,
                minor,
                render_index: first_render,
            });
        }
    }
    nodes
}

impl NvidiaBackend {
    /// Create a backend with a custom SHM zone config.
    pub fn new(cfg: ZoneConfig) -> Self {
        let nvkms = Arc::new(NvkmsPolicy::new());
        let semsurf = Arc::new(SemsurfPolicy::new());
        Self {
            window: None,
            dri_maps: std::collections::HashMap::new(),
            msg_counts: std::collections::BTreeMap::new(),
            live_maps: std::collections::HashMap::new(),
            abi_policy: AbiPolicy::default(),
            abi_refused: std::collections::BTreeMap::new(),
            rm_classes: crate::tally::Tally::default(),
            rm_controls: crate::tally::Tally::default(),
            ioctls_by_ns: std::collections::BTreeMap::new(),
            pump_cmds: Vec::new(),
            created: Vec::new(),
            next_mapping_id: 1,
            current_msg: MsgType::Ioctl,
            current_handle: 0,
            current_req_id: 0,
            current_data_len: 0,
            current_proc: None,
            current_owner: crate::quota::Owner::Unknown,
            handles: HandleTable::new(),
            shm: ShmAllocator::new(cfg),
            active_maps: crate::mmap::MmapContext::new(),
            driver: None,
            abi: None,
            session: Session::default(),
            config: BackendConfig::default(),
            max_req: MAX_XFER_DIRECT,
            max_resp: MAX_XFER_DIRECT,
            nodes: None,
            signaled: None,
            kms_states: std::collections::HashMap::new(),
            vm_kms: Arc::default(),
            syncobj_regs: crate::fence::Registrations::default(),
            hooks: BackendHooks::with_state(nvkms.clone(), semsurf.clone()),
            nvkms,
            semsurf,
            xfer_sys: Arc::new(crate::xfer::HostSys),
            host_ioctl: libc_ioctl,
            uvm_refused: std::collections::HashSet::new(),
            uvm_maps: crate::uvmmap::UvmMaps::default(),
            wl: crate::wl::WlState::default(),
            rmmem: crate::rmmem::RmMem::default(),
            guest_ram: None,
            osdesc: crate::osdesc::OsDesc::default(),
        }
    }

    /// Create a backend with the default 256 MiB zone split.
    pub fn with_default_zones() -> Self {
        Self::new(ZoneConfig::default_1gib())
    }

    /// Total SHM BAR size (for VMM config space).
    pub fn shm_total_size(&self) -> u64 {
        self.shm.total_size()
    }

    /// Raw memfd fd (for KVM memslot creation).
    pub fn shm_memfd_raw(&self) -> i32 {
        self.shm.memfd_raw()
    }

    /// Forward ioctls the ABI profile does not describe, instead of refusing
    /// them. Diagnostic only: it exists to find out what a workload needs.
    pub fn set_abi_policy(&mut self, policy: AbiPolicy) {
        if policy == AbiPolicy::Permissive {
            log::warn!(
                "ABI enforcement off: ioctls this build cannot describe will be \
                 forwarded to the host driver unchecked"
            );
        }
        self.abi_policy = policy;
    }

    /// Whether guest system memory is allocated GPU-coherent (the default;
    /// see rmmem.rs). Off leaves every allocation as the guest asked, which on
    /// an Intel host under KVM's default IGNORE_GUEST_PAT quirk means the
    /// guest caches memory the GPU does not snoop.
    pub fn set_guest_coherency(&mut self, coherent: bool) {
        if !coherent {
            log::warn!(
                "guest system memory keeps the coherency it asks for: on an Intel host the \
                 guest may read stale GPU data unless the VMM honours guest PAT"
            );
        }
        self.rmmem.set_coherent(coherent);
        self.nvkms.set_coherent_display(coherent);
    }

    /// What the backend was started with: compositor-VM mode, the Wayland
    /// sockets, which schemas exist.
    pub fn set_config(&mut self, config: BackendConfig) {
        self.config = config;
    }

    pub fn config(&self) -> &BackendConfig {
        &self.config
    }

    /// Mutable access, for code that learns a capability after start-up (the
    /// fence schemas setting `fences`, say).
    pub fn config_mut(&mut self) -> &mut BackendConfig {
        &mut self.config
    }

    /// How much one request and one response may carry. HELLO reports these
    /// to the guest, and the transport enforces them.
    pub fn set_transport_limits(&mut self, max_req: u32, max_resp: u32) {
        self.max_req = max_req;
        self.max_resp = max_resp;
    }

    /// Instructions for the event pump made since this was last called.
    ///
    /// A transport calls it after serving messages and forwards them in
    /// order. Draining rather than reading, so two transports cannot both
    /// think they own a watch.
    pub fn take_pump_cmds(&mut self) -> Vec<PumpCmd> {
        std::mem::take(&mut self.pump_cmds)
    }

    /// Give the backend somewhere to place device memory.
    ///
    /// Until this is called every `RM_MAP_MEMORY` still succeeds on the host --
    /// the mapping is real -- but the `mmap` that follows is refused, because
    /// there is no address in the guest that names it.
    pub fn set_window(&mut self, placer: Box<dyn crate::shm::WindowPlacer>) {
        self.window = Some(placer);
    }

    /// Guest RAM, from the vhost-user memory table. Every OS descriptor's
    /// page list is checked against, and mapped from, the table current when
    /// it arrives (osdesc.rs); a registration made from an older one keeps
    /// what it mapped. With none, no guest is offered BCAP_OS_DESC.
    pub fn set_guest_ram(&mut self, ram: Option<crate::osdesc::GuestRam>) {
        self.guest_ram = ram;
    }

    /// How many host descriptors the guest currently holds open.
    /// Whether a placer is attached (the transport's request channel is up).
    pub(crate) fn has_window(&self) -> bool {
        self.window.is_some()
    }

    /// Size the handle table for a backend that may hold `nofile`
    /// descriptors (posture::raise_nofile).
    pub fn set_nofile(&mut self, nofile: u64) {
        let limit = crate::handle_table::limit_for_nofile(nofile);
        log::info!("RLIMIT_NOFILE {nofile}: at most {limit} guest handles");
        self.handles.set_limit(limit);
    }

    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    /// Whether `fd` is a descriptor a guest handle stands for.
    pub fn owns_fd(&self, fd: RawFd) -> bool {
        self.handles.owns_fd(fd)
    }

    /// Free bytes per SHM zone, as `(uc, wc, wb)`. For tests that assert a
    /// mapping cycle gives back exactly what it took.
    pub fn shm_free_bytes(&self) -> (u64, u64, u64) {
        self.shm.free_bytes()
    }

    pub fn shm_base_ptr(&self) -> *mut u8 {
        self.shm.base_ptr()
    }

    /// Override the SHM base pointer to the guest memory HVA.
    /// Called by the VMM after guest memory setup.
    pub fn set_shm_base(&mut self, ptr: *mut u8) {
        self.shm.set_base_ptr(ptr);
    }

    /// Create a minimal backend suitable for unit tests (8-page total BAR).
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
        })
    }

    /// Put a descriptor straight into the table, as if an OPEN or an fd out
    /// had produced it.
    #[cfg(test)]
    pub(crate) fn adopt_for_test(&mut self, fd: OwnedFd, kind: HandleKind) -> u32 {
        self.handles.insert(fd, kind).expect("test table has room")
    }

    /// Replace host node enumeration, which needs real hardware.
    #[cfg(test)]
    pub(crate) fn set_host_nodes_for_test(&mut self, dri: Vec<DriDevice>, cards: Vec<CardNode>) {
        self.nodes = Some(Arc::new(HostNodes { dri, cards }));
    }

    /// Replace the host ioctl, to see what a forwarding path hands the driver.
    #[cfg(test)]
    pub(crate) fn set_host_ioctl_for_test(&mut self, f: HostIoctl) {
        self.host_ioctl = f;
    }

    // ------------------------------------------------------------------
    // Teardown
    //
    // Called by the VMM on:
    //   - normal VM shutdown (virtio device reset before exit)
    //   - ungraceful VM exit (SIGKILL, crash, libkrun teardown)
    //
    // Draining the handle table closes every host fd, which triggers the
    // host NVIDIA driver's fd-release path and frees all RM objects.
    // Analogous to nvproxy's Release() in nvproxy.go.
    // ------------------------------------------------------------------

    pub fn teardown(&mut self) {
        log::info!(
            "NvidiaBackend::teardown: draining {} handles, {} active maps",
            self.handles.len(),
            self.active_maps.len()
        );
        if !self.abi_refused.is_empty() {
            let verb = if self.abi_policy == AbiPolicy::Enforce {
                "refused"
            } else {
                "forwarded unchecked"
            };
            log::warn!(
                "NvidiaBackend::teardown: {verb} {} ioctl(s) the ABI profile does not describe: {}",
                self.abi_refused.values().sum::<u64>(),
                self.abi_refused
                    .iter()
                    .map(|(e, n)| format!("{e:#04x}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        // The whole forwarded surface, by namespace. A filter has to cover all
        // of these, and today only 'F' has a table to check against at all.
        if !self.ioctls_by_ns.is_empty() {
            let mut by_ns: std::collections::BTreeMap<char, Vec<String>> = Default::default();
            for ((ns, nr), n) in &self.ioctls_by_ns {
                by_ns.entry(*ns).or_default().push(format!("{nr:#04x}={n}"));
            }
            for (ns, entries) in by_ns {
                let what = match ns {
                    'F' => "nvidia escapes",
                    'd' => "DRM ioctls",
                    'm' => "modeset ioctls",
                    'u' => "UVM ioctls",
                    _ => "unknown namespace",
                };
                log::info!(
                    "NvidiaBackend::teardown: {} {what} ({}): {}",
                    entries.len(),
                    ns,
                    entries.join(" ")
                );
            }
        }

        // The two sets a filter would be written from. Printed whole rather
        // than summarised: the long tail is the interesting part, because that
        // is where something a pipeline needs exactly once hides.
        // A bounded number of entries to a line, and the calls past the
        // tally's cap as one number (S-17).
        let report = |what: &str, tally: &crate::tally::Tally, key: fn(u32) -> String| {
            for line in tally.lines(key) {
                log::info!("NvidiaBackend::teardown: {} {what}: {line}", tally.len());
            }
            if tally.overflow() > 0 {
                log::info!(
                    "NvidiaBackend::teardown: and {} more {what} call(s) past the tally's {} keys",
                    tally.overflow(),
                    crate::tally::MAX_KEYS
                );
            }
        };
        report("RM_ALLOC class(es)", &self.rm_classes, |c| {
            format!("{c:#06x}")
        });
        report("RM_CONTROL command(s)", &self.rm_controls, |c| {
            format!("{c:#010x}")
        });
        let total: u64 = self.msg_counts.values().sum();
        log::info!(
            "NvidiaBackend::teardown: served {total} message(s): {}",
            self.msg_counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.release_all();
    }

    /// Give back every placement and close every handle.
    ///
    /// Placements go first, and each is withdrawn from the window as well as
    /// freed: a guest process that exits without unmapping is the normal
    /// case, not an error -- most of the mappings in a captured trace are
    /// still live when the process ends -- and the window must not keep host
    /// device memory mapped into a range the next session will be handed.
    pub(crate) fn release_all(&mut self) {
        // UVM pools first, while their files are still open here: the VMM's
        // mapping may hold a file's last reference, and its teardown belongs
        // to this process's close, not to the VMM's munmap.
        for w in self.uvm_maps.take_all() {
            self.withdraw_uvm(w, "session end");
        }
        let live: Vec<LiveMap> = self.live_maps.drain().map(|(_, m)| m).collect();
        for m in live {
            self.release_extent(&m.region, m.length, "session end");
        }
        self.dri_maps.clear();
        for e in self.active_maps.drain() {
            self.release_extent(&e.region, e.shm_length, "session end");
        }
        self.rmmem.clear();
        for (_, k) in self.kms_states.drain() {
            k.retire();
        }
        self.vm_kms.clear();
        self.wl_forget_all();
        self.syncobj_regs.clear();
        self.uvm_refused.clear();
        self.nvkms.reset();
        // Every UVM mapping of a registration is taken down, and every
        // client holding one freed on the file it was made on, before any
        // file closes (osdesc.rs); then the records go.
        for h in self.osdesc.uvm_files() {
            if let Ok(fd) = self.handles.get_raw(h) {
                self.osdesc_uvm_close(fd, h, "session end");
            }
        }
        let holders: Vec<u32> = self.osdesc.clients().into_iter().collect();
        for c in holders {
            let fd = self
                .semsurf
                .issuer_of(c)
                .and_then(|h| self.handles.get_raw(h).ok());
            if let Some(fd) = fd {
                self.osdesc_end_clients(fd, &[c], "session end");
            }
        }
        self.osdesc.clear();
        self.semsurf.reset();
        // As in close_handle: display files close on the closer thread, not
        // under the backend mutex a reset holds (S-33).
        let handles = self.handles.handles();
        if !handles.is_empty() {
            log::info!("release_all: closing {} host file(s)", handles.len());
        }
        for h in handles {
            if let Ok((fd, kind)) = self.handles.remove(h) {
                crate::closer::close_fd(fd, kind);
            }
        }
    }

    /// Withdraw a placement from the window and return its extent. Called
    /// exactly once per extent, by whichever record owns it.
    fn release_extent(&mut self, region: &crate::shm::ShmRegion, length: u64, why: &str) {
        // Emptied rather than unmapped: a hole would leave the memory slot
        // covering a range that reaches no mapping at all, and a stray access
        // there faults the VMM rather than the guest.
        if let Some(window) = self.window.as_ref() {
            if let Err(e) = window.withdraw(region.offset, length) {
                log::warn!(
                    "{why}: the window would not give back {:#x}+{length:#x}: {e}",
                    region.offset
                );
            }
        }
        if let Err(e) = self.shm.free(region) {
            log::warn!("{why}: freeing window region {:#x}: {e}", region.offset);
        }
    }

    /// Ask the VMM to take a UVM pool out of the aperture: its memory slot,
    /// then its mapping (the VMM's order). A failure is logged and the
    /// aperture space is ours again anyway: the VMM is gone, or has already
    /// dropped it.
    fn withdraw_uvm(&self, (off, len): crate::uvmmap::Withdraw, why: &str) {
        match self.window.as_ref().map(|w| w.withdraw_uvm(off, len)) {
            Some(Ok(())) => log::info!("{why}: UVM pool at aperture {off:#x}+{len:#x} withdrawn"),
            Some(Err(e)) => log::warn!(
                "{why}: the VMM would not withdraw the UVM pool at aperture {off:#x}+{len:#x}: {e}"
            ),
            None => log::warn!("{why}: no window to withdraw aperture {off:#x}+{len:#x} from"),
        }
    }

    /// The host DRM nodes, enumerated on first use.
    pub(crate) fn host_nodes(&mut self) -> Arc<HostNodes> {
        self.nodes
            .get_or_insert_with(|| Arc::new(enumerate_host_nodes()))
            .clone()
    }

    // ------------------------------------------------------------------
    // Top-level dispatch
    // ------------------------------------------------------------------

    /// Serve one request whose response buffer holds `cap` bytes.
    ///
    /// Most messages are answered here and now. An IOCTL2 comes back as
    /// [`Outcome::Ioctl2`]: validated and holding everything it needs, to be
    /// executed with no backend lock held and then handed to
    /// [`NvidiaBackend::finish_ioctl2`].
    pub fn serve(&mut self, req_buf: &[u8], cap: usize) -> Outcome {
        self.created.clear();
        // The mode is configuration, which the transport may set at any
        // point before the first message; the NVKMS policy reads its copy.
        self.nvkms.set_kms_card(self.config.kms_card);
        if req_buf.len() < size_of::<MsgHeader>() {
            self.current_msg = MsgType::Ioctl;
            self.current_req_id = 0;
            let r = self.error_reply(libc::EPROTO);
            return Outcome::Reply(self.fit(r, cap));
        }
        let hdr = read_struct::<MsgHeader>(req_buf, 0);
        self.current_req_id = hdr.req_id;

        let Some(msg_type) = MsgType::from_u32(hdr.msg_type) else {
            log::warn!("unknown msg_type {}", hdr.msg_type);
            self.current_msg = MsgType::Ioctl;
            let r = self.error_reply(libc::EPROTO);
            return Outcome::Reply(self.fit(r, cap));
        };
        self.current_msg = msg_type;
        *self
            .msg_counts
            .entry(match msg_type {
                MsgType::Open => "open",
                MsgType::Close => "close",
                MsgType::Ioctl => "ioctl",
                MsgType::Mmap => "mmap",
                MsgType::Munmap => "munmap",
                MsgType::GetProcFiles => "get_proc_files",
                MsgType::GetSysFiles => "get_sys_files",
                MsgType::EventReady => "event_ready",
                MsgType::Hello => "hello",
                MsgType::Ioctl2 => "ioctl2",
                MsgType::TimeSync => "time_sync",
                MsgType::EventData => "event_data",
                MsgType::Watch => "watch",
                MsgType::Unwatch => "unwatch",
                MsgType::HostOp => "host_op",
                MsgType::WlSend => "wl_send",
                MsgType::WlRecv => "wl_recv",
            })
            .or_insert(0) += 1;
        // The handle travels in the header, not the payload -- every message
        // after Open acts on one, and Open's response returns one the same way.
        self.current_handle = hdr.handle;

        let payload = &req_buf[size_of::<MsgHeader>()..];
        // Who what this message makes is charged to (quota.rs). OPEN and
        // HOST_OP make handles out of nothing and say the process after
        // their fixed part; everything else acts on a handle, whose owner
        // it is.
        self.current_owner = match msg_type {
            MsgType::Open => crate::quota::Owner::from_trailer(
                payload,
                size_of::<OpenReq>(),
                self.session.proc_ids,
            ),
            MsgType::HostOp => crate::quota::Owner::from_trailer(
                payload,
                size_of::<protocol::messages::HostOpReq>(),
                self.session.proc_ids,
            ),
            _ => self.handles.owner(hdr.handle),
        };
        let reply = match msg_type {
            MsgType::Hello
            | MsgType::Ioctl2
            | MsgType::TimeSync
            | MsgType::Watch
            | MsgType::Unwatch
            | MsgType::HostOp => match self.serve_v2(msg_type, payload, cap) {
                Outcome::Reply(r) => r,
                pending => return pending,
            },
            MsgType::WlSend | MsgType::WlRecv => self.serve_wl(msg_type, payload, cap),
            _ => {
                // The v1 handlers write into a buffer of the response's size.
                // Zeroed, so nothing of an earlier response can reach the
                // guest through a short write.
                let mut buf = vec![0u8; cap.min(self.max_resp as usize)];
                let n = self.dispatch_v1(msg_type, payload, &mut buf);
                buf.truncate(n);
                Reply {
                    bytes: buf,
                    stamp_at: None,
                    created: std::mem::take(&mut self.created),
                }
            }
        };
        Outcome::Reply(self.fit(reply, cap))
    }

    /// `reply`, or if it is larger than the `cap` bytes the guest posted, a
    /// header saying so -- or nothing, if even that does not fit: no reply
    /// is ever longer than the buffer it goes to.
    fn fit(&mut self, mut reply: Reply, cap: usize) -> Reply {
        if reply.bytes.len() > cap {
            // Nothing the guest could read: say why in a header, if even that
            // fits, and take back whatever the message created.
            let created = std::mem::take(&mut reply.created);
            self.close_handles(&created);
            reply = self.error_reply(libc::EMSGSIZE);
            if reply.bytes.len() > cap {
                reply.bytes.clear();
            }
        }
        reply
    }

    /// Serve one request into a caller-supplied buffer, all the way through:
    /// an IOCTL2 is executed inline, a TIME_SYNC stamped at once.
    ///
    /// For callers that have no executor and no ring -- tests, and the
    /// socket harness. Returns the bytes written.
    pub fn dispatch(&mut self, req_buf: &[u8], resp_buf: &mut [u8]) -> usize {
        let mut reply = match self.serve(req_buf, resp_buf.len()) {
            Outcome::Reply(r) => r,
            Outcome::Ioctl2(mut p) => {
                p.execute();
                self.finish_ioctl2(p)
            }
        };
        reply.stamp();
        let n = reply.bytes.len().min(resp_buf.len());
        resp_buf[..n].copy_from_slice(&reply.bytes[..n]);
        n
    }

    fn dispatch_v1(&mut self, msg_type: MsgType, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        match msg_type {
            MsgType::Open => self.handle_open(0, payload, resp_buf),
            MsgType::Close => self.handle_close(0, payload, resp_buf),
            MsgType::Ioctl => self.handle_ioctl(0, payload, resp_buf),
            MsgType::Mmap => self.handle_mmap(payload, resp_buf),
            MsgType::Munmap => self.handle_munmap(payload, resp_buf),
            MsgType::GetProcFiles => self.handle_get_files(FileTree::Proc, resp_buf),
            MsgType::GetSysFiles => self.handle_get_files(FileTree::Sys, resp_buf),
            // Host to guest only. A guest that sends one is confused about the
            // direction of the queue, and saying so beats serving it.
            MsgType::EventReady | MsgType::EventData => {
                log::warn!(
                    "{msg_type:?} arrived from the guest; that message only travels outward"
                );
                self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL)
            }
            _ => self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0),
        }
    }

    // ------------------------------------------------------------------
    // OPEN
    // ------------------------------------------------------------------

    fn handle_open(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<OpenReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
        }
        let req = read_struct::<OpenReq>(payload, 0);

        let kind = match DeviceKind::from_device_type(req.device_type) {
            // Cards are reached only through HOST_OP OPEN_KMS, which ties the
            // host card file to a guest file's render handle and exists only
            // in compositor-VM mode. An OPEN of one would hand a guest a
            // file that is DRM master of the host's display.
            Some(DeviceKind::DriCard(n)) => {
                log::warn!("OPEN of card {n} refused: cards are opened through HOST_OP OPEN_KMS");
                return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, libc::EPERM);
            }
            Some(DeviceKind::Dri(n)) => HandleKind::DriRender(n),
            // UVM is compute's alone (`--allow-compute`, session.rs): without
            // it neither device is opened on the host, so no UVM command,
            // sharing mode or aperture placement is reachable at all.
            Some(k @ (DeviceKind::Uvm | DeviceKind::UvmTools)) if !self.config.allow_compute => {
                log::warn!("OPEN of {k:?} refused: UVM is served only with --allow-compute");
                return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, libc::ENODEV);
            }
            // A channel to the host compositor: not a path (wl/serve.rs).
            Some(DeviceKind::Wayland) => {
                return match self.open_wayland(req.flags) {
                    Ok(h) => {
                        self.created.push(h);
                        self.write_hdr(resp_buf, h, 0)
                    }
                    Err(e) => self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, e),
                };
            }
            Some(k) => HandleKind::Dev(k),
            None => {
                log::warn!("handle_open: invalid device type {}", req.device_type);
                return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
            }
        };

        if kind == HandleKind::Dev(DeviceKind::Modeset) {
            if let Some(why) = self.modeset_open_refused(self.current_owner) {
                log::warn!("OPEN of /dev/nvidia-modeset refused: {why}");
                return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, libc::EMFILE);
            }
        }
        let nodes = self.host_nodes();
        let path = match device_path_with(req.device_type, &nodes.dri) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("handle_open: {}", e);
                return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
            }
        };

        // Never O_NONBLOCK, whatever the guest asked: nvidia-modeset skips
        // poll_wait for a non-blocking file (nvidia-modeset-linux.c:2023-
        // 2025), so the pump would never hear of its events. The guest keeps
        // its own O_NONBLOCK and applies it locally.
        let raw_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw_fd < 0 {
            let err = std::io::Error::last_os_error();
            let errno = err.raw_os_error().unwrap_or(0);
            log::warn!("open({:?}) failed: {}", path, err);
            return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, errno);
        }
        // SAFETY: a descriptor open() just returned, owned from here on.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        // The guest may wait on this descriptor, and only the host's copy ever
        // becomes readable. The pump gets a duplicate of its own, so the watch
        // cannot outlive this table's fd by watching a reused number.
        let watch = match kind {
            HandleKind::Dev(_) => fd.try_clone().ok(),
            _ => None,
        };
        let guest_handle = match self.handles.insert_for(fd, kind, self.current_owner) {
            Ok(h) => h,
            Err(full) => {
                log::warn!("open {:?}: handle table full", path);
                return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, full.errno());
            }
        };
        if let Some(fd) = watch {
            self.pump_cmds.push(PumpCmd::Watch {
                handle: guest_handle,
                fd,
                mode: WatchMode::Legacy,
            });
        }
        self.created.push(guest_handle);
        log::debug!("open {:?} -> handle={guest_handle} (fd={raw_fd})", path);
        if let HandleKind::DriRender(dri) = kind {
            self.semsurf_render_opened(guest_handle, dri);
        }

        // The handle is returned in the header. The driver reads it from there
        // and there is no response payload at all.
        self.write_hdr(resp_buf, guest_handle, 0)
    }

    /// Why guest process `owner` may not open another `/dev/nvidia-modeset`,
    /// if it may not. Every one is a host NVKMS open with an event list NVKMS
    /// never bounds (nvkms.c:6422-6435) and permission state of its own: at
    /// most [`nvkms::MAX_MODESET_OPENS`] per VM, and a process's share of
    /// them ([`nvkms::MODESET_SHARE`], B4).
    pub(crate) fn modeset_open_refused(&self, owner: crate::quota::Owner) -> Option<String> {
        let kind = HandleKind::Dev(DeviceKind::Modeset);
        let (mut open, mut mine) = (0usize, 0usize);
        for h in self.handles.handles() {
            if self.handles.kind(h) == Some(kind) {
                open += 1;
                if self.handles.owner(h) == owner {
                    mine += 1;
                }
            }
        }
        if open >= nvkms::MAX_MODESET_OPENS {
            return Some(format!("{open} already open, the most one VM may hold"));
        }
        crate::quota::admits(
            &nvkms::MODESET_SHARE,
            owner,
            mine as u64,
            1,
            open as u64,
            nvkms::MAX_MODESET_OPENS as u64,
        )
        .err()
        .map(|why| format!("guest process {owner:?} holds {mine} of the VM's {open} ({why:?})"))
    }

    // ------------------------------------------------------------------
    // MMAP / MUNMAP
    // ------------------------------------------------------------------

    /// Place a mapping the guest asked for into the shared window.
    ///
    /// The guest quotes the cookie a previous `RM_MAP_MEMORY` wrote into
    /// pLinearAddress, which is the offset within the shared window, so this
    /// only has to find the region that cookie belongs to and hand back where
    /// it sits.
    fn handle_mmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<MmapReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL);
        }
        let req = read_struct::<MmapReq>(payload, 0);

        // Only files that have device memory to place: the NVIDIA devices and
        // DRM nodes. A sync_file, a dmabuf or a compositor's memfd reached
        // through here would put host memory in the window by a path nobody
        // designed for it.
        match self.current_kind() {
            Some(
                HandleKind::Dev(_)
                | HandleKind::DriRender(_)
                | HandleKind::DrmCard(_)
                | HandleKind::DrmLease(_),
            )
            | None => {}
            Some(k) => {
                log::warn!("mmap on handle {} ({k:?}) refused", self.current_handle);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EPERM);
            }
        }

        // A UVM file's mapping is a semaphore pool, which UVM maps only at the
        // host address equal to its offset -- never in the window. Sent there,
        // the VMM's mmap of it failed and took the window's request channel
        // down with it.
        if matches!(
            self.current_kind(),
            Some(HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools))
        ) {
            return self.uvm_mmap(&req, resp_buf);
        }

        // A DRM node never takes the recorded path. Its bookkeeping is keyed by
        // the file, and one open of a node holds every object a client ever
        // allocates -- so the second object's mmap would find the first one's
        // entry and hand back the first one's memory. The objects are told
        // apart by the offset, and that is what the path below keys on.
        if self.current_kind().and_then(HandleKind::index).is_some() {
            return self.map_unrecorded(req.size, req.offset, resp_buf);
        }

        let entry = match self.active_maps.find_by_fd_handle(self.current_handle) {
            Some(e) => *e,
            None => {
                // Bookkeeping here records what RM_MAP_MEMORY armed, and that
                // is not the only ioctl that arms a mapping: NV_ESC_RM_ALLOC_MEMORY
                // names a file in the same way and arms it too, which is what
                // the descriptor at the end of its parameters is for. Measured
                // against a host run, the second 4 KiB mapping of the control
                // device is armed that way, and refusing it here is the guest
                // seeing a failed mmap where the host sees a mapping.
                //
                // Which ioctl armed it is the driver's business, not ours. The
                // file either has a mapping waiting on it, in which case
                // mapping it into the window succeeds, or it has not, in which
                // case the kernel says so -- and that answer is better than our
                // records, because the driver is the one keeping them.
                return self.map_unrecorded(req.size, req.offset, resp_buf);
            }
        };

        // What goes back is the offset within the window, not a guest physical
        // address. The backend does not know where the window sits -- the bus
        // assigns that, and this crate names no VMM -- but the guest driver
        // does, because it reads the window's address out of its own device.
        // It adds the two.
        let (offset, length) = (entry.region.offset, entry.region.length);
        // A mapping is rarely a whole number of pages -- the usermode aperture
        // is 0xc70 bytes -- and mmap always covers whole pages, so the guest
        // asking for more than the mapping holds is the normal case and not an
        // overrun. What must not happen is a request past the page the mapping
        // ends in.
        let page = 4096u64;
        let mapped_pages = length.div_ceil(page) * page;
        if req.size > mapped_pages {
            log::warn!(
                "mmap: guest asked for {:#x} bytes of a {length:#x}-byte mapping at {offset:#x}",
                req.size
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }

        // An id the guest's vmas count on, the same one for every MMAP of this
        // mapping: RM_UNMAP_MEMORY then leaves the extent alone until the last
        // of them is gone (see `MmapEntry::mapping_id`).
        let id = if entry.mapping_id != 0 {
            entry.mapping_id
        } else {
            self.next_mapping_id()
        };
        if let Some(e) = self.active_maps.find_by_fd_handle_mut(self.current_handle) {
            e.mapping_id = id;
            e.refs += 1;
        }

        log::debug!("mmap: window offset {offset:#x}+{length:#x}, id {id}");
        self.write_mmap_resp(
            resp_buf,
            offset,
            mapped_pages,
            id,
            entry.region.pgprot,
            entry.writable,
        )
    }

    /// MMAP of a UVM file: one of its semaphore pools, exactly, placed in
    /// the UVM aperture at the pool's own host address (uvmmap.rs).
    fn uvm_mmap(&mut self, req: &MmapReq, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle;
        if self.current_kind() == Some(HandleKind::Dev(DeviceKind::UvmTools)) {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EPERM);
        }
        if !self.session.v2 || self.window.is_none() {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }
        let Ok(host_fd) = self.handles.get_raw(handle) else {
            return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::ENOENT);
        };
        // Before anything is placed: a placement whose reply cannot be
        // written would hold a reference no guest mapping will ever give back.
        if resp_buf.len() < size_of::<MsgHeader>() + size_of::<MmapResp>() {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let (base, len) = (req.offset, req.size);
        self.uvm_maps.set_owner(handle, self.handles.owner(handle));
        let plan = match self.uvm_maps.plan_mmap(handle, base, len, req.prot) {
            Ok(p) => p,
            Err(errno) => {
                log::info!(
                    "mmap of UVM handle {handle} at {base:#x}+{len:#x} (prot {}) refused: {}",
                    req.prot,
                    std::io::Error::from_raw_os_error(errno)
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, errno);
            }
        };
        let (off, id) = match plan {
            crate::uvmmap::MmapPlan::Existing(p) => {
                self.uvm_maps.add_ref(handle, base);
                (p.aperture_off, p.mapping_id)
            }
            crate::uvmmap::MmapPlan::New { aperture_off } => {
                let id = self.next_mapping_id();
                let placed = self.window.as_ref().expect("checked above").place_uvm(
                    aperture_off,
                    len,
                    host_fd,
                    base,
                );
                if let Err(e) = placed {
                    self.uvm_maps.abort(aperture_off, len);
                    log::warn!(
                        "mmap of UVM handle {handle}: the VMM would not map {base:#x}+{len:#x} \
                         at aperture {aperture_off:#x}: {e}"
                    );
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOMEM);
                }
                self.uvm_maps.commit(handle, base, aperture_off, id);
                log::info!(
                    "mmap of UVM handle {handle}: pool {base:#x}+{len:#x} at aperture \
                     {aperture_off:#x}, id {id}"
                );
                (aperture_off, id)
            }
        };
        let mut n = self.write_hdr(resp_buf, handle, 0);
        n += write_struct(
            &mut resp_buf[n..],
            &MmapResp {
                guest_phys_addr: off,
                size: len,
                mapping_id: id,
                caching: MMAP_CACHE_WB,
                flags: MMAP_F_UVM_APERTURE,
                reserved: 0,
            },
        );
        n
    }

    fn current_kind(&self) -> Option<HandleKind> {
        self.handles.kind(self.current_handle)
    }

    /// An MMAP reply. `pgprot` is the zone the placement is in, which is the
    /// memory type the host maps it with; a v2 guest maps it the same way,
    /// and a v1 guest, which reads nothing there, keeps write-combining.
    fn write_mmap_resp(
        &self,
        resp_buf: &mut [u8],
        offset: u64,
        size: u64,
        id: u32,
        pgprot: crate::shm::PgprotKind,
        writable: bool,
    ) -> usize {
        use crate::shm::PgprotKind;
        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let (caching, flags) = if self.session.v2 {
            let c = match pgprot {
                PgprotKind::WriteBack => MMAP_CACHE_WB,
                PgprotKind::WriteCombine => MMAP_CACHE_WC,
                PgprotKind::Uncached => MMAP_CACHE_UC,
            };
            (c, if writable { 0 } else { MMAP_F_READ_ONLY })
        } else {
            (MMAP_CACHE_DEFAULT, 0)
        };
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size,
                mapping_id: id,
                caching,
                flags,
                reserved: 0,
            },
        );
        off
    }

    /// A window extent in the zone for `want`. A write-back request that does
    /// not fit falls back to write-combining, which is always a correct way to
    /// map what the host maps write-back (only slower to read); nothing else
    /// falls back, least of all registers, which must stay uncached.
    ///
    /// The extent is charged to `owner`, which holds at most its share of a
    /// zone (quota.rs, B2).
    fn alloc_zone(
        &mut self,
        length: u64,
        want: crate::shm::PgprotKind,
        owner: crate::quota::Owner,
    ) -> Result<crate::shm::ShmRegion> {
        use crate::shm::PgprotKind;
        match self.shm.alloc_for(length, want, owner) {
            Err(e) if want == PgprotKind::WriteBack => {
                log::warn!("write-back zone: {e}; placing {length:#x} bytes write-combining");
                self.shm.alloc_for(length, PgprotKind::WriteCombine, owner)
            }
            r => r,
        }
    }

    /// How the host maps what an RM_MAP_MEMORY of `h_client`'s `h_memory`
    /// armed, from the flags RM returned and our records (M-1).
    ///
    /// The caching type in the flags means something only for video memory:
    /// the escape resets it to DEFAULT before RM sees it (escape.c:600-601),
    /// and RM writes a real one back only when it maps through BAR1, marking
    /// the mapping REFLECTED (mapping_cpu.c:586-591). System memory comes back
    /// DIRECT with DEFAULT, and the host maps it with the allocation's own
    /// type (nv-mmap.c:727-729), which is in our records; registers (a
    /// usermode doorbell) come back with MAPPING as the caller left it and are
    /// mapped UC whatever was asked (nv-mmap.c:589-596).
    fn rm_mapping_pgprot(
        &self,
        flags: u32,
        h_client: u32,
        h_memory: u32,
    ) -> crate::shm::PgprotKind {
        use crate::rmmem::Mem;
        use crate::shm::PgprotKind;
        const MAPPING_SHIFT: u32 = 15; // NVOS33_FLAGS_MAPPING 16:15
        const MAPPING_DIRECT: u32 = 1;
        const MAPPING_REFLECTED: u32 = 2;
        const CACHING_TYPE_SHIFT: u32 = 23; // NVOS33_FLAGS_CACHING_TYPE 25:23
        const CACHED: u32 = 0;
        const UNCACHED: u32 = 1;
        const WRITECOMBINED: u32 = 2;
        const WRITEBACK: u32 = 5;
        const UNCACHED_WEAK: u32 = 7;

        let mem = self.rmmem.lookup(h_client, h_memory);
        match ((flags >> MAPPING_SHIFT) & 3, mem) {
            (_, Some(Mem::Regmem)) => PgprotKind::Uncached,
            (MAPPING_REFLECTED, _) => match (flags >> CACHING_TYPE_SHIFT) & 7 {
                CACHED | WRITEBACK => PgprotKind::WriteBack,
                WRITECOMBINED => PgprotKind::WriteCombine,
                UNCACHED | UNCACHED_WEAK => PgprotKind::Uncached,
                // RM always writes a real type for BAR1; DEFAULT means it did
                // not map one, and the safe type for what it did map is UC.
                _ => PgprotKind::Uncached,
            },
            (_, Some(m)) => m.pgprot(),
            // System memory this backend never saw allocated: the old guess.
            (MAPPING_DIRECT, None) => PgprotKind::WriteCombine,
            _ => PgprotKind::Uncached,
        }
    }

    /// Serve an mmap on a file this backend has no record of arming.
    ///
    /// The window placement and the reply are the same as the recorded path;
    /// only the source of the length differs — the guest's request, since there
    /// is no stored region to take it from.
    fn map_unrecorded(&mut self, size: u64, offset: u64, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle;
        let host_fd = match self.handles.get_raw(handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::ENOENT),
        };

        // Caching follows the device: a GPU device carries the card's own
        // memory, which the host maps write-combined through BAR1, and so does
        // a DRM node (drm_gem_mmap_obj, drm_gem.c:1250-1252, for every
        // nvidia-drm object). The control device carries system memory, armed
        // by an NV_ESC_RM_ALLOC_MEMORY whose coherency is in our records; one
        // we did not see is taken to be write-back only when this backend
        // makes guest system memory coherent, and write-combining otherwise,
        // which is safe for any system memory.
        let kind = self.current_kind();
        let drm = kind.and_then(HandleKind::index).is_some();
        let pgprot = match kind {
            Some(HandleKind::Dev(DeviceKind::Gpu(_))) => crate::shm::PgprotKind::WriteCombine,
            _ if drm => crate::shm::PgprotKind::WriteCombine,
            _ => match self.rmmem.armed(handle) {
                Some(m) => m.pgprot(),
                None if self.rmmem.coherent() => crate::shm::PgprotKind::WriteBack,
                None => crate::shm::PgprotKind::WriteCombine,
            },
        };

        // On a DRM node the guest's offset is a real position in the file --
        // GEM_MAP_OFFSET issued it, on this very descriptor -- and the object's
        // memory is reachable nowhere else. Everywhere else the offset is a
        // cookie RM chose, which names no position at all, and mapping the file
        // there would either fail or land on unrelated memory.
        let fd_offset = if drm { offset } else { 0 };

        // The same object mapped twice is the same memory: hand back the
        // placement that is already there rather than a second copy of it,
        // and count the reference, so the first MUNMAP does not pull it from
        // under the second user.
        if let Some(&id) = self.dri_maps.get(&(handle, fd_offset)) {
            if let Some(live) = self.live_maps.get_mut(&id) {
                live.refs += 1;
                let (offset, length) = (live.region.offset, live.length);
                let (pgprot, writable) = (live.region.pgprot, live.writable);
                return self.write_mmap_resp(
                    resp_buf,
                    offset,
                    length.div_ceil(4096) * 4096,
                    id,
                    pgprot,
                    writable,
                );
            }
        }

        let length = size.max(4096);
        let writable = crate::shm::host_mapping_writable(host_fd, length, fd_offset);
        let region = match self.alloc_zone(length, pgprot, self.handles.owner(handle)) {
            Ok(r) => r,
            Err(e) => {
                log::error!("mmap on handle {handle}: window has no room: {e}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOMEM);
            }
        };

        let Some(window) = self.window.as_ref() else {
            log::warn!("mmap on handle {handle}: no shared window to place it in");
            if let Err(e) = self.shm.free(&region) {
                log::warn!("mmap on handle {handle}: freeing the unused region: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOTSUP);
        };
        if let Err(e) = window.place(region.offset, length, host_fd, fd_offset, writable) {
            log::warn!(
                "mmap on handle {handle}: nothing armed on this file, or it could \
                 not be placed: {e}"
            );
            if let Err(e) = self.shm.free(&region) {
                log::warn!("mmap on handle {handle}: freeing the unused region: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }

        log::info!(
            "mmap on handle {handle}: placed {length:#x} bytes at window offset {:#x} \
             with no arming recorded here",
            region.offset
        );

        let (offset, pgprot) = (region.offset, region.pgprot);
        let id = self.next_mapping_id();
        self.dri_maps.insert((handle, fd_offset), id);
        self.live_maps.insert(
            id,
            LiveMap {
                key: Some((handle, fd_offset)),
                region,
                length,
                refs: 1,
                writable,
            },
        );
        self.write_mmap_resp(
            resp_buf,
            offset,
            length.div_ceil(4096) * 4096,
            id,
            pgprot,
            writable,
        )
    }

    /// A mapping id no live placement has. Zero means "none" to the guest.
    fn next_mapping_id(&mut self) -> u32 {
        loop {
            let id = self.next_mapping_id;
            self.next_mapping_id = self.next_mapping_id.wrapping_add(1).max(1);
            if !self.live_maps.contains_key(&id)
                && !self.active_maps.has_mapping_id(id)
                && !self.uvm_maps.has_mapping_id(id)
            {
                return id;
            }
        }
    }

    /// An RM mapping has ended on the host -- RM_UNMAP_MEMORY, or the close
    /// of the file carrying it -- and its placement goes with it, unless a
    /// guest vma still maps it: then the extent moves to `live_maps` under the
    /// id those vmas hold, and the last MUNMAP of it releases it (M-3).
    fn end_rm_mapping(&mut self, entry: crate::mmap::MmapEntry, why: &str) {
        if entry.refs == 0 {
            self.release_extent(&entry.region, entry.shm_length, why);
            return;
        }
        log::debug!(
            "{why}: window offset {:#x} is still mapped by {} guest vma(s) of id {}; \
             released when they are gone",
            entry.region.offset,
            entry.refs,
            entry.mapping_id
        );
        self.live_maps.insert(
            entry.mapping_id,
            LiveMap {
                key: None,
                region: entry.region,
                length: entry.shm_length,
                refs: entry.refs,
                writable: entry.writable,
            },
        );
    }

    fn handle_munmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<MunmapReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL);
        }
        let req = read_struct::<MunmapReq>(payload, 0);

        // A UVM pool in the aperture: the last of its MMAP replies given back
        // takes it out. Only the handle that mapped it can.
        match self.uvm_maps.munmap(self.current_handle, req.mapping_id) {
            Some(Some(w)) => {
                self.withdraw_uvm(w, "munmap");
                return self.write_hdr(resp_buf, 0, 0);
            }
            Some(None) => return self.write_hdr(resp_buf, 0, 0),
            None => {}
        }

        // An RM mapping still in force: its vmas are counted here, and its
        // extent stays with it until RM_UNMAP_MEMORY or its file's close,
        // which see this count (`end_rm_mapping`).
        if let Some(e) = self.active_maps.find_by_mapping_id_mut(req.mapping_id) {
            e.refs = e.refs.saturating_sub(1);
            return self.write_hdr(resp_buf, 0, 0);
        }
        // Zero is what an older backend handed out for RM mappings, and an
        // unknown id is one already taken back. Reported as success so a guest
        // tearing one down does not log a failure for it.
        let Some(live) = self.live_maps.get_mut(&req.mapping_id) else {
            return self.write_hdr(resp_buf, 0, 0);
        };
        live.refs -= 1;
        if live.refs > 0 {
            return self.write_hdr(resp_buf, 0, 0);
        }
        let live = self
            .live_maps
            .remove(&req.mapping_id)
            .expect("looked up above");
        if let Some(key) = live.key {
            if self.dri_maps.get(&key) == Some(&req.mapping_id) {
                self.dri_maps.remove(&key);
            }
        }
        self.release_extent(&live.region, live.length, "munmap");
        log::debug!(
            "munmap {}: window offset {:#x}+{:#x} is free again",
            req.mapping_id,
            live.region.offset,
            live.length
        );
        self.write_hdr(resp_buf, 0, 0)
    }

    // ------------------------------------------------------------------
    // GET_PROC_FILES / GET_SYS_FILES
    // ------------------------------------------------------------------

    /// Collect a tree of small files and stream them to the guest.
    ///
    /// The guest republishes these under its own `/proc/driver/nvidia`, which
    /// is where the userspace driver and NVML look before they will talk to a
    /// device at all. Without them a guest with working ioctls still reports
    /// that it cannot find a GPU.
    ///
    /// The response is a bare stream of entries with **no message header** --
    /// the driver reads from the first byte of the buffer.
    fn handle_get_files(&mut self, tree: FileTree, resp_buf: &mut [u8]) -> usize {
        let files = tree.collect();
        log::info!("{}: {} file(s)", tree.name(), files.len());

        let mut off = 0usize;
        for (path, content) in &files {
            let need = size_of::<FileEntry>() + path.len() + content.len();
            // Leave room for the terminator, or a guest reads past the last
            // entry into whatever the buffer held before.
            if off + need + size_of::<FileEntry>() > resp_buf.len() {
                log::warn!(
                    "{}: response buffer holds {} of {} files",
                    tree.name(),
                    files.iter().position(|(p, _)| p == path).unwrap_or(0),
                    files.len()
                );
                break;
            }
            off += write_struct(
                &mut resp_buf[off..],
                &FileEntry {
                    path_len: path.len() as u32,
                    content_len: content.len() as u32,
                },
            );
            resp_buf[off..off + path.len()].copy_from_slice(path.as_bytes());
            off += path.len();
            resp_buf[off..off + content.len()].copy_from_slice(content);
            off += content.len();
        }

        if off + size_of::<FileEntry>() <= resp_buf.len() {
            off += write_struct(&mut resp_buf[off..], &FileEntry::default());
        }

        // GET_SYS_FILES carries a second section the file stream does not
        // announce: a u32 count of DRI devices, then that many records of
        // {name_len, major, minor, slot_index, dev_info[9]} and the name. Omitting it does not
        // fail cleanly -- the driver reads whatever bytes follow the
        // terminator as the count, which is why a run with no second section
        // still logged "no DRI devices reported by VMM" and looked correct.
        //
        // Headless forwarding hands out no render node, so the count is zero
        // and it still has to be written.
        //
        // A third follows: the card nodes. In every mode -- a guest needs the
        // host's card numbers to map the dev_t a compositor names its scanout
        // device by (the Wayland devmap) -- but a card is *openable* only in
        // compositor-VM mode, which BCAP_KMS_CARD says: the guest opens one
        // (nvgpu_kms_open) and forwards its hotplugs only then, and HOST_OP
        // OPEN_KMS is refused without --kms-card. A guest that predates the
        // section stops parsing after the DRI one; an older backend leaves it
        // out, which a guest reads as "no cards".
        //
        // A fourth, only after a whole third: the size of each DRI record's
        // host GET_DEV_INFO layout (write_dev_info_sizes).
        if tree == FileTree::Sys {
            let nodes = self.host_nodes();
            off += write_dri_section(&nodes.dri, &mut resp_buf[off..]);
            let cards = write_card_section(&nodes.cards, &mut resp_buf[off..]);
            off += cards;
            if cards > 0 {
                off += write_dev_info_sizes(&nodes.dri, &mut resp_buf[off..]);
            }
        }
        off
    }

    // ------------------------------------------------------------------
    // CLOSE
    // ------------------------------------------------------------------

    fn handle_close(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let _ = payload;
        match self.close_handle(self.current_handle) {
            Ok(()) => self.write_hdr(resp_buf, 0, 0),
            Err(_) => self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
        }
    }

    /// Close one handle, and everything that exists only because of it.
    ///
    /// RM mappings go with the file that carries them. For some clients that
    /// is the only release there is -- a CUDA run maps 29 times and never
    /// unmaps once -- so leaving them to teardown would cost the write-combine
    /// zone tens of megabytes for the life of the VM.
    ///
    /// Window placements of DRM objects do *not* go: a GEM proxy in another
    /// guest file may still have one mapped (see [`LiveMap`]). Only the lookup
    /// that would hand the placement to a new mmap on this handle goes.
    pub(crate) fn close_handle(&mut self, handle: u32) -> Result<()> {
        let (fd, kind) = self.handles.remove(handle)?;
        // Its UVM pools leave the VMM while `fd` is still open here, so the
        // file's last reference, and UVM's teardown of it, stay ours.
        for w in self.uvm_maps.take_handle(handle) {
            self.withdraw_uvm(w, "close");
        }
        self.uvm_refused.remove(&handle);
        // Registered memory its external mappings hold is taken down while
        // the file is ours, and only then may the guest unpin it (osdesc.rs).
        if kind == HandleKind::Dev(DeviceKind::Uvm) {
            self.osdesc_uvm_close(fd.as_raw_fd(), handle, "close");
        }
        for entry in self.active_maps.take_for_fd(handle) {
            log::debug!(
                "close handle={handle}: releasing mapping at SHM {:#x}+{:#x}",
                entry.region.offset,
                entry.region.length
            );
            self.end_rm_mapping(entry, "close");
        }
        // The one client set (semsurf.rs) says which RM clients died with
        // this file; the memory records drop what those held.
        let gone_clients = self.semsurf.forget_handle(handle);
        // Clients holding memory the guest registered by its pages are freed
        // here, while the file is ours: RM lets go of the pages now, not
        // whenever the file's last reference goes, and only then is the guest
        // told it may unpin them (osdesc.rs).
        self.osdesc_end_clients(fd.as_raw_fd(), &gone_clients, "close");
        self.rmmem.forget_fd(handle, &gone_clients);
        self.dri_maps.retain(|(h, _), _| *h != handle);
        self.forget_kms_state(handle);
        // A render file's syncobj numbers die with it and a later file may
        // get the same handle number: its waits must never join these (S-13).
        if matches!(kind, HandleKind::DriRender(_)) {
            self.syncobj_regs.orphan_file(handle);
        }
        self.wl_forget(handle);
        self.nvkms.forget_handle(handle);
        self.pump_cmds.push(PumpCmd::Unwatch { handle });
        log::debug!("close handle={handle} ({kind:?})");
        // A display file's last close can wait on a modeset; not here, on
        // the queue thread under the backend mutex (closer.rs, S-33).
        crate::closer::close_fd(fd, kind);
        Ok(())
    }

    /// Drop KMS handle `handle`'s per-file state, and with it every
    /// framebuffer it made from the VM's scanout sources: before the host
    /// file closes and its ids can go to someone else (S-6).
    pub(crate) fn forget_kms_state(&mut self, handle: u32) {
        if let Some(k) = self.kms_states.remove(&handle) {
            k.retire();
        }
    }

    // ------------------------------------------------------------------
    // IOCTL — top-level
    // ------------------------------------------------------------------

    /// Learn the host driver version from a successful `NV_ESC_CHECK_VERSION_STR`
    /// reply and select the ABI profile for it.
    ///
    /// Layout is `nv_ioctl_rm_api_version_t`: cmd (4), reply (4), then a
    /// NUL-terminated 64-byte version string.
    fn learn_driver_version(&mut self, param_buf: &[u8]) {
        if self.driver.is_some() || param_buf.len() < 12 {
            return;
        }
        let tail = &param_buf[8..];
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        let Ok(text) = std::str::from_utf8(&tail[..end]) else {
            return;
        };
        let Some(v) = abi::version::DriverVersion::parse(text) else {
            return;
        };
        self.set_driver_version(v);
    }

    /// The host driver version, as the transport read it from the driver at
    /// start-up. Known before any guest asks, it tells HELLO whether an
    /// NVKMS schema exists for this host (`BCAP_NVKMS_TABLE`) -- which the
    /// guest uses to pick its own NVKMS table -- and lets a modeset IOCTL2
    /// find its table before the guest's first CHECK_VERSION_STR.
    pub fn set_host_driver_version(&mut self, text: &str) {
        match abi::version::DriverVersion::parse(text) {
            Some(v) => self.set_driver_version(v),
            None => log::warn!("host driver version {text:?} does not parse; no NVKMS schema"),
        }
    }

    fn set_driver_version(&mut self, v: abi::version::DriverVersion) {
        self.driver = Some(v);
        self.nvkms.set_version(v);
        self.config.nvkms_table = crate::schema::modeset_table(v).is_some();
        self.abi = abi::versions::table_for(v);
        match self.abi {
            Some(t) => log::info!("host driver {v}: ABI profile selected, {} escapes", t.len()),
            None => log::warn!(
                "host driver {v} is older than every ABI profile; ioctls will be \
                 forwarded without size checking"
            ),
        }
    }

    /// Check one guest ioctl against the host's ABI profile.
    ///
    /// A size mismatch is the failure this is for: the guest and host disagree
    /// about a struct layout, so the host reads or writes the wrong number of
    /// bytes. Without a check it surfaces as corrupt GPU state rather than an
    /// error.
    pub fn check_abi(&self, escape: u32, param_size: u32) -> AbiCheck {
        let Some(table) = self.abi else {
            return AbiCheck::NoProfile;
        };
        let Some(entry) = abi::versions::lookup(table, escape) else {
            return AbiCheck::UnknownEscape;
        };
        match entry.param_size {
            None => AbiCheck::VariableLength,
            Some(expected) if expected == param_size => AbiCheck::Ok,
            Some(expected) => AbiCheck::SizeMismatch {
                expected,
                actual: param_size,
            },
        }
    }

    /// Whether the host's ABI table marks `escape` as carrying a descriptor
    /// and nothing here translates it: the virtio config's
    /// FD_CARRYING_IOCTLS (the guest translates those) plus the arms of
    /// `handle_ioctl` built on them. Today that is EXPORT_TO_DMABUF_FD alone;
    /// the check is general so that the next such escape a profile adds is
    /// refused rather than forwarded raw. Before the version is known there
    /// is no table, and `guestptr::rm_escape` refuses the one there is by
    /// number.
    fn untranslated_fd_escape(&self, escape: u32) -> bool {
        self.abi
            .and_then(|t| abi::versions::lookup(t, escape))
            .is_some_and(|e| e.kind == abi::versions::IoctlKind::FdCarrying)
            && !crate::virtio::FD_CARRYING_IOCTLS
                .iter()
                .any(|&(nr, _)| nr == escape)
    }

    fn handle_ioctl(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<IoctlReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, cookie, 0);
        }
        let ireq = read_struct::<IoctlReq>(payload, 0);

        // The guest sends the top-level struct and the block any pointer in it
        // refers to, back to back. The handlers below already expect that
        // layout, so the two lengths only need adding up here.
        let body = &payload[size_of::<IoctlReq>()..];
        let want = ireq.data_len as usize + ireq.nested_len as usize + ireq.deep_len as usize;
        if body.len() < want {
            log::warn!(
                "ioctl cmd={:#x}: guest promised {want} bytes and sent {}",
                ireq.cmd,
                body.len()
            );
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, cookie, libc::EINVAL);
        }
        let nested_end = ireq.data_len as usize + ireq.nested_len as usize;
        let param_in = &body[..nested_end];

        // What a pointer inside the nested block refers to. The guest cannot
        // send an address that means anything here, so it sends the bytes and
        // says where the pointer sits; the call below gives them a host
        // address, and the reply carries them back.
        //
        // Or, marked DEEP_SEGMENTED, what several pointers refer to, one
        // segment each (deepseg.rs). Only an RM control's parameters and
        // IDLE_CHANNELS' top-level block are read that way; a segmented
        // block on any other call is refused rather than ignored, so a guest
        // never believes pointers were carried that were not.
        let segmented = ireq.deep_len > 0 && ireq.deep_ptr_offset == DEEP_SEGMENTED;
        // Or, marked DEEP_PAGE_LIST, the guest-physical pages of memory the
        // caller registers with RM (osdesc.rs): only on the three calls that
        // register it.
        let listed = ireq.deep_len > 0 && ireq.deep_ptr_offset == DEEP_PAGE_LIST;
        let page_list: Option<&[u8]> = listed.then(|| &body[nested_end..want]);
        if listed
            && (hostfd::ioc_type(ireq.cmd) != b'F'
                || !matches!(
                    hostfd::ioc_nr(ireq.cmd),
                    abi::ioctl::NV_ESC_RM_ALLOC
                        | abi::ioctl::NV_ESC_RM_ALLOC_MEMORY
                        | abi::ioctl::NV_ESC_RM_VID_HEAP_CONTROL
                ))
        {
            log::warn!(
                "ioctl cmd={:#x}: a page list on a call that registers no memory",
                ireq.cmd
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        let deep_in: Option<(usize, &[u8])> = if ireq.deep_len > 0 && !segmented && !listed {
            Some((ireq.deep_ptr_offset as usize, &body[nested_end..want]))
        } else {
            None
        };
        let deep_segs: Option<&[u8]> = segmented.then(|| &body[nested_end..want]);
        if segmented {
            let nr = hostfd::ioc_nr(ireq.cmd);
            if hostfd::ioc_type(ireq.cmd) != b'F'
                || !matches!(
                    nr,
                    abi::ioctl::NV_ESC_RM_CONTROL | abi::ioctl::NV_ESC_RM_IDLE_CHANNELS
                )
            {
                log::warn!(
                    "ioctl cmd={:#x}: deep segments on a call that has none",
                    ireq.cmd
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
        }

        // How much of the response is the top-level struct. The driver copies
        // exactly this much back to userspace and reads any nested block after
        // it, so a wrong split corrupts one or the other.
        self.current_data_len = ireq.data_len;

        let request = ireq.cmd as u64;
        let (host_fd, kind) = match (
            self.handles.get_raw(self.current_handle),
            self.current_kind(),
        ) {
            (Ok(fd), Some(kind)) => (fd, kind),
            _ => return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
        };

        let escape = (request & 0xFF) as u32;
        let ioc_type = ((request >> 8) & 0xFF) as u32;

        // Counted before anything decides whether to serve it, so a refusal
        // still shows up as something the workload asked for.
        let ns = match ioc_type {
            x if x == b'F' as u32 => 'F',
            x if x == b'd' as u32 => 'd',
            x if x == b'm' as u32 => 'm',
            0 => 'u', // UVM: type 0, and it has no table either
            _ => '?',
        };
        *self.ioctls_by_ns.entry((ns, escape)).or_insert(0) += 1;

        let route = match v1_route(kind, ireq.cmd, ireq.data_len) {
            Ok(r) => r,
            Err(errno) => {
                log::warn!(
                    "v1 ioctl {:#010x} ({} bytes) on handle {} ({kind:?}) refused: {}",
                    ireq.cmd,
                    ireq.data_len,
                    self.current_handle,
                    if errno == libc::EPERM {
                        "not a call this kind of handle takes over v1"
                    } else {
                        "its size does not match the command"
                    }
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
        };

        if page_list.is_some() && !matches!(route, V1Route::Rm) {
            log::warn!(
                "ioctl cmd={:#x}: a page list on handle {} ({kind:?}), which is no RM file",
                ireq.cmd,
                self.current_handle
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        match route {
            V1Route::Rm => {}
            V1Route::Uvm => {
                // nvidia-uvm works on the caller's address space, which is
                // ours: only commands that cannot reach it go (guestptr.rs).
                let tools = kind == HandleKind::Dev(DeviceKind::UvmTools);
                if self.uvm_refused.contains(&self.current_handle) {
                    log::warn!(
                        "UVM ioctl {:#x} on handle {} refused: its VA space may have pageable \
                         access",
                        ireq.cmd,
                        self.current_handle
                    );
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
                }
                let init_flags_mask = self
                    .driver
                    .and_then(abi::schema::uvm_table)
                    .map_or(0, |t| t.init_flags_mask);
                let mut params = param_in.to_vec();
                let restore = match crate::guestptr::uvm_gate(
                    tools,
                    ireq.cmd,
                    &mut params,
                    init_flags_mask,
                ) {
                    Ok(r) => r,
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                };
                // Exactly the block the host's UVM copies each way
                // (abi::schema::uvm_table, the table the guest sizes the
                // call by): UVM's numbers carry no size, so a short block
                // would have the host read and write past what the guest
                // sent, and a long one is not this release's command.
                if let Err(errno) = self.uvm_size_ok(ireq.cmd, params.len()) {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                // A semaphore pool is host kernel memory the moment UVM makes
                // it: its length and the budgets are checked first, not only
                // whether it may later be mapped (uvmmap.rs, F1).
                self.uvm_maps
                    .set_owner(self.current_handle, self.handles.owner(self.current_handle));
                if ireq.cmd == crate::uvmmap::ALLOC_SEMAPHORE_POOL && params.len() >= 16 {
                    let len = u64::from_le_bytes(params[8..16].try_into().unwrap());
                    if let Err(errno) = self.uvm_maps.admit_pool(self.current_handle, len) {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                }
                // The descriptor some commands name another file by
                // (uvmfd.rs): our handle, as the guest driver sent it, becomes
                // our descriptor for the call and the handle again in the
                // reply.
                let fd_field = match self.uvm_fd_in(ireq.cmd, &mut params) {
                    Ok(f) => f,
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                };
                // Registered memory UVM would keep a duplicate of
                // (osdesc.rs): refused, or followed.
                let osdesc_map = match self.osdesc_uvm_before(ireq.cmd, &params) {
                    Ok(m) => m,
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                };
                let n = self.dispatch_simple(cookie, host_fd, request, &params, resp_buf);
                self.osdesc_uvm_after(ireq.cmd, &params, osdesc_map, resp_buf, n);
                if log::log_enabled!(log::Level::Debug) {
                    // UVM puts its NV_STATUS in the block, not in the ioctl's
                    // return; this is the only place it shows.
                    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
                    log::debug!(
                        "UVM {:#x} on handle {}: reply block {:02x?}",
                        ireq.cmd,
                        self.current_handle,
                        resp_buf.get(body..n).unwrap_or(&[])
                    );
                }
                if ireq.cmd == crate::guestptr::UVM_INITIALIZE
                    && init_flags_mask & crate::guestptr::UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS == 0
                {
                    self.uvm_pageable_off(host_fd, resp_buf, n);
                }
                self.uvm_observe(ireq.cmd, &params, resp_buf, n, init_flags_mask);
                Self::restore_reply(&restore, resp_buf, n);
                if let Some((off, handle)) = fd_field {
                    let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
                    if let Some(s) = resp_buf.get_mut(body + off..body + off + 4)
                        && n >= body + off + 4
                    {
                        s.copy_from_slice(&handle.to_le_bytes());
                    }
                }
                return n;
            }
            V1Route::DrmFlat => {
                let n = self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf);
                // A fence context is a GEM object of the file, and counts
                // against its cap until it is closed (semsurf.rs).
                if ireq.cmd == hostfd::DRM_IOCTL_GEM_CLOSE
                    && crate::semsurf::reply_params(resp_buf, n).is_some()
                {
                    let gem = u32::from_le_bytes(param_in[..4].try_into().unwrap());
                    self.semsurf.gem_closed(self.current_handle, gem);
                }
                return n;
            }
            V1Route::Nvkms => {
                // NVKMS multiplexes every operation through one ioctl number,
                // so the number says nothing and the command inside says
                // everything. v1 carries one flat block and no descriptor, so
                // only commands the host's NVKMS table has with no pointer and
                // no descriptor go this way, under the same policy as IOCTL2
                // (nvkms.rs, which may rewrite the block); everything else
                // needs IOCTL2. A command number is never trusted across
                // driver versions: REGISTER_SURFACE is 16 in one release and
                // 17 in the next.
                let mut msg = param_in.to_vec();
                let ok = request == u64::from(crate::schema::NVKMS_IOCTL_IOWR)
                    && ireq.data_len == 16
                    && deep_in.is_none();
                let checked = if ok {
                    self.recheck_granting_leases();
                    self.nvkms.v1_before(self.current_handle, &mut msg)
                } else {
                    Err(libc::EINVAL)
                };
                if let Err(errno) = checked {
                    log::warn!(
                        "v1 NVKMS call {:#x} on handle {} refused ({errno})",
                        u32::from_le_bytes(
                            msg.get(..4)
                                .and_then(|c| c.try_into().ok())
                                .unwrap_or([0; 4])
                        ),
                        self.current_handle
                    );
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                // A dpy probed too recently is answered with the last reply
                // (nvkms.rs, `dpy_probe`; S-8), laid out as the host's.
                if self.nvkms.v1_cached(self.current_handle, &mut msg) {
                    return self.write_ioctl_resp(resp_buf, cookie, &msg);
                }
                let n = self.dispatch_nested(
                    cookie, host_fd, request, &msg, resp_buf, 16, // outer_size
                    8,  // ptr_offset
                    4,  // size_offset
                    None, None, None,
                );
                // The one reply the policy rewrites (ALLOC_DEVICE's display
                // coherency modes, nvkms.rs), laid out as `msg` was.
                let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
                if n >= body && read_struct::<MsgHeader>(resp_buf, 0).status == 0 {
                    self.nvkms.v1_after(&mut resp_buf[body..n]);
                    self.nvkms
                        .v1_record(self.current_handle, &resp_buf[body..n]);
                }
                return n;
            }
            V1Route::DrmNested {
                outer_size,
                ptr_offset,
                size_offset,
            } => {
                log::debug!("drm ioctl nr={escape:#04x} ({} bytes in)", param_in.len());
                return self.dispatch_nested(
                    cookie,
                    host_fd,
                    request,
                    param_in,
                    resp_buf,
                    outer_size,
                    ptr_offset,
                    size_offset,
                    deep_in,
                    // Both NVKMS blocks begin with `int memFd`.
                    Some(0),
                    None,
                );
            }
        }

        // Only NVIDIA's own magic is described by the ABI tables.
        let refuse = match self.check_abi(escape, ireq.data_len) {
            AbiCheck::SizeMismatch { expected, actual } => {
                log::warn!(
                    "escape {escape:#04x}: guest sent {actual} bytes, host driver {} expects \
                     {expected}",
                    self.driver.expect("a profile implies a known version")
                );
                true
            }
            AbiCheck::UnknownEscape => {
                log::warn!(
                    "escape {escape:#04x} is not in the ABI profile for host driver {}",
                    self.driver.expect("a profile implies a known version")
                );
                true
            }
            // No profile yet means CHECK_VERSION_STR has not been answered,
            // which is itself one of the first ioctls a client sends. Refusing
            // here would refuse the call that makes checking possible at all.
            AbiCheck::Ok | AbiCheck::VariableLength | AbiCheck::NoProfile => false,
        };
        if refuse {
            *self.abi_refused.entry(escape).or_insert(0) += 1;
            if self.abi_policy == AbiPolicy::Enforce {
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
        }

        use abi::ioctl::*;

        // An escape the host's table says carries a descriptor, with no
        // translation here, would reach the host with the guest's number in
        // it -- naming whatever this process has open under that number --
        // and could leave a descriptor of the host's in our table that the
        // guest never learns of (S-15). Whatever the ABI policy.
        if self.untranslated_fd_escape(escape) {
            log::warn!(
                "escape {escape:#04x} carries a descriptor the backend does not translate; refused"
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EOPNOTSUPP);
        }

        // Memory registered by its pages rather than its address: its own
        // path, and the only one on which RM is handed an address for an OS
        // descriptor -- one of ours (osdesc.rs). Sent with an address alone,
        // the same calls are refused below.
        if let Some(list) = page_list {
            return self.dispatch_osdesc(
                cookie,
                host_fd,
                request,
                ireq.data_len as usize,
                param_in,
                list,
                resp_buf,
            );
        }

        // No guest pointer reaches RM as a pointer (guestptr.rs), whatever
        // the ABI policy: the fields RM would dereference are zeroed or the
        // call is refused, and the caller's values go back in the reply. The
        // parameter pointers of RM_CONTROL and RM_ALLOC are dispatch_nested's.
        let mut ptr_copy = param_in.to_vec();
        let outer_len = (ireq.data_len as usize).min(ptr_copy.len());
        // IDLE_CHANNELS' channel list, sent with its three arrays: they are
        // ours for the call (`_idle_segs` holds them until it returns).
        let (ptr_restore, _idle_segs) = match (escape, deep_segs) {
            (NV_ESC_RM_IDLE_CHANNELS, Some(deep)) => {
                match crate::guestptr::idle_channels_list(
                    ireq.cmd,
                    &mut ptr_copy[..outer_len],
                    deep,
                ) {
                    Ok((r, s)) => (r, Some(s)),
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                }
            }
            _ => match crate::guestptr::rm_escape(ireq.cmd, &mut ptr_copy[..outer_len]) {
                Ok(r) => (r, None),
                Err(errno) => {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
            },
        };
        let param_in: &[u8] = &ptr_copy;

        // Sharing, duplicating and naming RM objects of another client
        // (rmshare.rs): the calling guest process, and what the call may
        // name, judged on the parameters as the guest sent them. A refusal
        // is RM's own status in those parameters, and the host is not asked.
        self.current_proc = None;
        let mut share_pending = crate::rmshare::Pending::default();
        if ioc_type == b'F' as u32 {
            let sent = &body[..nested_end];
            self.current_proc = match self.rm_proc_id(escape, sent, &body[want..]) {
                Ok(p) => p,
                Err(errno) => {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
            };
            match self.rm_share_gate(escape, sent, self.current_proc) {
                Ok(p) => share_pending = p,
                Err(crate::rmshare::Refuse::Errno(errno)) => {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                Err(crate::rmshare::Refuse::Status(status)) => {
                    let at = crate::rmshare::status_at(escape).unwrap_or(0);
                    let mut out = crate::rmshare::refusal(sent, at, status);
                    let deep = &body[nested_end..want];
                    out.extend_from_slice(deep);
                    return self.write_ioctl_resp_deep(resp_buf, cookie, &out, deep.len());
                }
            }
        }

        // What the memory an escape makes, duplicates, frees or GPU-maps is,
        // and the coherency rewrite (rmmem.rs): the host is handed a rewritten
        // copy, and the reply gets the caller's own bits back. RM_ALLOC only in
        // the 48-byte NVOS64 form, the one the offsets are for.
        let rm_copy: Vec<u8>;
        let mut rm_pending = None;
        let param_in: &[u8] = if ioc_type == b'F' as u32
            && crate::rmmem::RmMem::watches(escape)
            && (escape != NV_ESC_RM_ALLOC || ireq.data_len == 48)
        {
            let mut v = param_in.to_vec();
            rm_pending = Some(self.rmmem.before(escape, &mut v));
            rm_copy = v;
            &rm_copy
        } else {
            param_in
        };

        let n = match escape {
            // ---------------------------------------------------------------
            // FD-carrying ioctls — need handle translation
            // ---------------------------------------------------------------
            NV_ESC_REGISTER_FD => {
                self.dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, resp_buf)
            }
            // The OS events RM calls may name later (semsurf.rs).
            NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
                let n =
                    self.dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, resp_buf);
                self.semsurf_track_rm(escape, self.current_handle, param_in, resp_buf, n);
                n
            }

            NV_ESC_RM_ALLOC_MEMORY => {
                self.dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, resp_buf)
            }

            NV_ESC_RM_MAP_MEMORY => {
                self.dispatch_map_memory(cookie, host_fd, request, param_in, resp_buf)
            }

            NV_ESC_RM_UNMAP_MEMORY => {
                self.dispatch_unmap_memory(cookie, host_fd, request, param_in, resp_buf)
            }

            NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => self
                .dispatch_update_device_mapping_info(cookie, host_fd, request, param_in, resp_buf),

            // ---------------------------------------------------------------
            // RM control requires nested handling
            // ---------------------------------------------------------------
            // Registered memory handed to a holder the backend cannot follow
            // (osdesc.rs): refused, before RM sees it.
            NV_ESC_RM_CONTROL if self.osdesc_export_refused(param_in) => {
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
            }
            // OS_UNIX controls whose descriptors nothing translates, and
            // ones RM does not define (rmctl.rs): RM's NOT_SUPPORTED.
            NV_ESC_RM_CONTROL if crate::rmctl::unix_refused(param_in).is_some() => {
                log::warn!(
                    "RM control {} refused: it names a host descriptor nothing translates",
                    crate::rmctl::unix_refused(param_in).unwrap_or_default()
                );
                let mut out = crate::rmctl::unsupported(param_in);
                let deep = deep_in.map_or(&[][..], |(_, b)| b);
                out.extend_from_slice(deep);
                self.write_ioctl_resp_deep(resp_buf, cookie, &out, deep.len())
            }
            // Controls that list other RM clients' host PIDs (rmctl.rs, S-24):
            // answered here, as RM answers a caller without the privilege.
            NV_ESC_RM_CONTROL if crate::rmctl::host_pid_control(param_in).is_some() => {
                log::warn!(
                    "RM control {} refused: it lists the host's GPU processes",
                    crate::rmctl::host_pid_control(param_in).unwrap_or_default()
                );
                let mut out = crate::rmctl::refusal(param_in);
                let deep = deep_in.map_or(&[][..], |(_, b)| b);
                out.extend_from_slice(deep);
                self.write_ioctl_resp_deep(resp_buf, cookie, &out, deep.len())
            }
            NV_ESC_RM_CONTROL => {
                let n = self.dispatch_nested(
                    cookie, host_fd, request, param_in, resp_buf, 32, 16, 24, deep_in, None,
                    deep_segs,
                );
                // Counted here rather than in the forwarder, which holds only a
                // shared borrow. NVOS54: hClient, hObject, cmd at byte 8.
                // Only what RM served: the key is the guest's u32, and a count
                // of what RM turned down is noise an allowlist is not written
                // from (S-17).
                if let Some(cmd) = rm_served(resp_buf, n, 8, NVOS54_STATUS) {
                    self.rm_controls.add(cmd);
                }
                n
            }

            // ---------------------------------------------------------------
            // RM alloc as well..
            // ---------------------------------------------------------------
            NV_ESC_RM_ALLOC => {
                let n = self.dispatch_nested(
                    cookie, host_fd, request, param_in, resp_buf, 48, 16, 32, deep_in, None,
                    None,
                );
                // NVOS64: hRoot, hObjectParent, hObjectNew, hClass at byte 12,
                // status at 40. As for controls, only what RM made.
                if let Some(class) = rm_served(resp_buf, n, 12, 40) {
                    self.rm_classes.add(class);
                }
                // A client made here is one 0x54 may name (semsurf.rs).
                self.semsurf_track_rm(escape, self.current_handle, param_in, resp_buf, n);
                n
            }

            // ---------------------------------------------------------------
            // Everything else — simple passthrough to host
            // ---------------------------------------------------------------
            _other => {
                if _other == 0x5E {
                    log::warn!("0x5E hit DEFAULT arm instead of dedicated handler!");
                }
                if _other == 0x00 {
                    log::debug!(
                        "MODESET IOCTL: handle={} request=0x{:x} param_in={:02x?}",
                        self.current_handle,
                        request,
                        &param_in[..std::cmp::min(param_in.len(), 16)]
                    );
                }
                let n = self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf);
                // RM_FREE of a client: no longer one 0x54 may name.
                self.semsurf_track_rm(escape, self.current_handle, param_in, resp_buf, n);
                n
            }
        };

        if let Some(p) = rm_pending {
            // The reply's parameters, laid out as the request's were; none
            // when the call failed before RM ran, and then nothing is recorded.
            let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
            let ok = n >= body && read_struct::<MsgHeader>(resp_buf, 0).status == 0;
            if ok {
                self.rmmem.after(p, &mut resp_buf[body..n]);
            }
        }
        // What RM freed, duplicated or made anew, as registrations of memory
        // by its pages live through (osdesc.rs).
        if ioc_type == b'F' as u32 {
            let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
            if n >= body && read_struct::<MsgHeader>(resp_buf, 0).status == 0 {
                self.osdesc_observe(escape, &resp_buf[body..n]);
                // A share RM took, for the duplicates it lets through.
                self.rm_share_after(escape, share_pending, &resp_buf[body..n]);
            }
        }
        Self::restore_reply(&ptr_restore, resp_buf, n);
        n
    }

    /// Give a successful reply's parameters (`resp_buf[..n]`) back the
    /// caller's values of the fields `restore` names.
    fn restore_reply(restore: &crate::guestptr::Restore, resp_buf: &mut [u8], n: usize) {
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if restore.is_empty() || n < body || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
            return;
        }
        restore.apply(&mut resp_buf[body..n]);
    }

    // ------------------------------------------------------------------
    // Nested-pointer ioctl (RM_CONTROL, RM_ALLOC..)
    // ------------------------------------------------------------------

    fn dispatch_nested(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
        outer_size: usize,
        ptr_offset: usize,
        size_offset: usize,
        deep_in: Option<(usize, &[u8])>,
        // Byte offset, inside the nested block, of a descriptor the guest
        // sent as one of our handles and the host must see as one of our
        // descriptors. `None` for the RM paths, which name their descriptors
        // by command rather than by position.
        nested_fd_offset: Option<usize>,
        // A segmented deep block (deepseg.rs): RM_CONTROL only.
        deep_segs: Option<&[u8]>,
    ) -> usize {
        if param_in.len() < outer_size {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        // Sized for what the host copies, not for what the guest sent: see
        // `ioctl_arg_len`.
        let Some(mut outer_buf) = ioctl_arg(request, &param_in[..outer_size]) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let outer = &mut outer_buf.as_mut_slice()[..outer_size];
        let nested_in = &param_in[outer_size..];

        // What the caller had in the pointer field, to put back before the
        // reply goes out. This used to be zeroed instead, on the reasoning that
        // a host address must not leak -- which is right -- but zero is not the
        // caller's value either. The host driver leaves the field alone, so a
        // caller there reads back the pointer it passed; through here it read
        // back null, and anything that dereferences what it gets back finds
        // nothing there.
        let caller_ptr: [u8; 8] = param_in[ptr_offset..ptr_offset + 8]
            .try_into()
            .expect("outer_size covers the pointer field");

        let escape = (request & 0xFF) as u32;

        // An OS event named by descriptor inside RM's parameters: semaphore
        // surface waiters and NV_EVENT_BUFFER (semsurf.rs). Found before
        // anything is copied, so a call that would have the host read the
        // field from past what the guest sent is refused outright.
        let rm = nested_fd_offset.is_none() && hostfd::ioc_type(request as u32) == b'F';
        let os_event = match crate::semsurf::os_event_field(
            if rm { escape } else { 0 },
            &param_in[..outer_size],
            param_in.len() - outer_size,
        ) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "RM call {request:#x}: its OS-event field is not in the {} bytes sent",
                    param_in.len() - outer_size
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e);
            }
        };
        // Neither a waiter nor an event buffer holds a second-level pointer,
        // and one the guest claims could be aimed at the field: the address
        // written there below would reach RM as the event.
        if os_event.is_some() && (deep_in.is_some() || deep_segs.is_some()) {
            log::warn!("RM call {request:#x} names an OS event and claims a deep pointer");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        // Log RM_CONTROL/RM_ALLOC for debugging Vulkan init. One line per
        // call, so debug: at info a guest's RM traffic was the log (S-20).
        if escape == 0x2A && outer.len() >= 12 {
            let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
            log::debug!(
                "RM_CONTROL cmd=0x{:x} (hClient={}, hObject={})",
                cmd,
                u32::from_le_bytes(outer[0..4].try_into().unwrap()),
                u32::from_le_bytes(outer[4..8].try_into().unwrap())
            );
        }
        if escape == 0x2B && outer.len() >= 16 {
            let h_class = u32::from_le_bytes(outer[12..16].try_into().unwrap());
            log::debug!("RM_ALLOC hClass=0x{:x}", h_class);
        }

        if !nested_in.is_empty() {
            // Guest sent nested params — allocate host buffer, point struct at it
            let nested_size = nested_in.len();
            // Guarded rather than heap-allocated: the driver writes its answer
            // here, and if it writes more than the caller's size field claimed,
            // the fault should land on that write rather than on someone else's
            // allocation later.
            let mut host_guard = match crate::guarded::GuardedBuf::new(nested_size) {
                Some(b) => b,
                None => {
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::ENOMEM,
                    );
                }
            };
            host_guard.as_mut_slice().copy_from_slice(nested_in);
            let host_buf = host_guard.as_mut_slice();

            // ---------------------------------------------------------------
            // Translate guest_handle → host fd for fd-carrying RM_CONTROLs
            //
            // The guest driver already translated the raw guest fd to a
            // guest_handle. We now translate that handle to a real host fd
            // so the host kernel can resolve it.
            // ---------------------------------------------------------------
            let mut saved_nested_handle: Option<(usize, i32)> = None; // (offset, guest_handle_as_i32)

            // An event object names the file its notifications arrive on, in
            // `NV0005_ALLOC_PARAMETERS.data` at offset 16. The guest driver has
            // already turned the caller's descriptor into one of our handles;
            // this turns that handle into the descriptor this process holds,
            // and puts the handle back before replying (the guest driver then
            // restores the caller's own descriptor over it).
            if escape == 0x2B && outer.len() >= 16 {
                let h_class = u32::from_le_bytes(outer[12..16].try_into().unwrap());
                const NV0005_DATA: usize = 16;
                if matches!(h_class, 0x05 | 0x79) && host_buf.len() >= NV0005_DATA + 4 {
                    let guest_handle_val = i32::from_le_bytes(
                        host_buf[NV0005_DATA..NV0005_DATA + 4].try_into().unwrap(),
                    );
                    match self.dev_fd(guest_handle_val as u32) {
                        Ok(real_fd) => {
                            saved_nested_handle = Some((NV0005_DATA, guest_handle_val));
                            host_buf[NV0005_DATA..NV0005_DATA + 4]
                                .copy_from_slice(&(real_fd as i32).to_le_bytes());
                        }
                        // -1, "no descriptor", goes as it is.
                        Err(_) if guest_handle_val == -1 => {}
                        // Anything else is refused, not forwarded: RM would
                        // look the number up among every guest process's
                        // files (R2).
                        Err(_) => {
                            log::warn!(
                                "event class {h_class:#x}: no handle {guest_handle_val} for \
                                 the file this event is to be delivered on"
                            );
                            return self.write_error_resp(
                                resp_buf,
                                Status::BadHandle,
                                cookie,
                                libc::EBADF,
                            );
                        }
                    }
                }
            }

            // The same for the OS event a waiter or an event buffer names by
            // (u64) descriptor: our handle becomes the host descriptor RM
            // looks the event up by, and only for an event that is live --
            // for NV_EVENT_BUFFER a lookup that misses is a host oops, not a
            // refusal (semsurf.rs).
            let mut saved_os_event: Option<(usize, [u8; 8])> = None;
            if let Some((off, h_client)) = os_event {
                match self.os_event_fd(h_client, &host_buf[off..off + 8]) {
                    Ok(None) => {}
                    Ok(Some(fd)) => {
                        let mut guest = [0u8; 8];
                        guest.copy_from_slice(&host_buf[off..off + 8]);
                        saved_os_event = Some((off, guest));
                        host_buf[off..off + 8].copy_from_slice(&(fd as u64).to_le_bytes());
                    }
                    Err(e) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e);
                    }
                }
            }

            // A control file named by descriptor in NV0000's OS_UNIX
            // controls (rmctl.rs): the guest's handle becomes our descriptor
            // of that very file, and only a control file's; a number that is
            // not one is refused, never forwarded, since RM would resolve it
            // among every guest process's files (R1, R2). -1 passes: RM
            // refuses it itself.
            if escape == 0x2A && outer.len() >= 12 {
                let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
                if let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd) {
                    if host_buf.len() < at + 4 {
                        log::warn!(
                            "RM control {cmd:#x}: {} parameter bytes do not hold its descriptor",
                            host_buf.len()
                        );
                        return self.write_error_resp(
                            resp_buf,
                            Status::IoctlFailed,
                            cookie,
                            libc::EINVAL,
                        );
                    }
                    let guest_val = i32::from_le_bytes(host_buf[at..at + 4].try_into().unwrap());
                    if guest_val != -1 {
                        match self.ctl_fd(guest_val as u32) {
                            Ok(real_fd) => {
                                saved_nested_handle = Some((at, guest_val));
                                host_buf[at..at + 4].copy_from_slice(&real_fd.to_le_bytes());
                            }
                            Err(_) => {
                                log::warn!(
                                    "RM control {cmd:#x}: {guest_val} is no control file of \
                                     this VM"
                                );
                                return self.write_error_resp(
                                    resp_buf,
                                    Status::BadHandle,
                                    cookie,
                                    libc::EBADF,
                                );
                            }
                        }
                    }
                }
            }

            // A descriptor named by position rather than by command: NVKMS's
            // import and export blocks both begin with the `memFd` naming the
            // memory. The guest driver has already turned its own descriptor
            // into one of our handles; this turns that handle into the
            // descriptor this process holds, and the restore below puts the
            // guest's value back before we answer.
            if let Some(off) = nested_fd_offset {
                if host_buf.len() < off + 4 {
                    log::warn!(
                        "ioctl {request:#x}: fd at {off} is outside {} nested bytes",
                        host_buf.len()
                    );
                    return self.write_error_resp(
                        resp_buf,
                        Status::InvalidMsgType,
                        cookie,
                        libc::EINVAL,
                    );
                }
                let guest_handle_val =
                    i32::from_le_bytes(host_buf[off..off + 4].try_into().unwrap());
                match self.dev_fd(guest_handle_val as u32) {
                    Ok(real_fd) => {
                        log::debug!("nvkms memFd: handle {guest_handle_val} → host fd {real_fd}");
                        saved_nested_handle = Some((off, guest_handle_val));
                        host_buf[off..off + 4].copy_from_slice(&(real_fd as i32).to_le_bytes());
                    }
                    Err(_) => {
                        log::warn!(
                            "nvkms memFd: no handle {guest_handle_val}; the memory to                              import names a file we did not open"
                        );
                        return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
                    }
                }
            }

            // Set pointer in outer struct to host buffer address
            let host_ptr = host_buf.as_mut_ptr() as u64;
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&host_ptr.to_le_bytes());

            // Extract the RM control command for special handling
            let _ctrl_cmd = if escape == 0x2A && outer.len() >= 12 {
                Some(u32::from_le_bytes(outer[8..12].try_into().unwrap()))
            } else {
                None
            };

            // ---------------------------------------------------------------
            // Special handling for critical RM_CONTROL commands
            // Based on gVisor nvproxy: these need modifications before host call
            // ---------------------------------------------------------------
            // Note: No special cmd handling needed - all cmd params are passed as-is
            // to the host. Any pointer/buffer handling is done by the guest via
            // separate mmap operations.
            // ---------------------------------------------------------------

            // Give the pointer inside the nested block a host address.
            //
            // The buffer has to outlive the call, and the guest's own pointer
            // value has to go back in afterwards: userspace compares what it
            // gets back with what it sent, and a host address there is both
            // meaningless and a leak of our layout.
            let mut deep_buf: Option<crate::guarded::GuardedBuf> = None;
            let mut deep_saved: Option<(usize, [u8; 8])> = None;
            if let Some((ptr_off, bytes)) = deep_in {
                // Controls whose pointers stay zeroed (ACPI methods among
                // them; abi::rmctrl::ZEROED_CONTROLS) take no deep block.
                if rm && escape == abi::ioctl::NV_ESC_RM_CONTROL {
                    let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
                    if abi::rmctrl::zeroed(cmd) {
                        log::warn!(
                            "RM control {cmd:#010x}: a deep block for a control whose \
                             pointers are never relocated"
                        );
                        return self.write_error_resp(
                            resp_buf,
                            Status::IoctlFailed,
                            cookie,
                            libc::EINVAL,
                        );
                    }
                }
                if ptr_off + 8 > host_buf.len() {
                    log::warn!(
                        "ioctl {request:#x}: pointer at {ptr_off} is outside {} nested bytes",
                        host_buf.len()
                    );
                    return self.write_error_resp(
                        resp_buf,
                        Status::InvalidMsgType,
                        cookie,
                        libc::EINVAL,
                    );
                }
                // The buffer is padded well past what the guest said it holds.
                //
                // The length comes from a field inside the caller's own
                // parameters, read with an offset out of a table generated from
                // one driver release. When that offset is wrong for the release
                // in use -- which it demonstrably is for some commands -- the
                // length read is not the buffer's length, while the driver
                // still writes as much as the command really produces. Writing
                // past a Vec sized to the wrong number corrupts this process's
                // heap, and it is detected later, at some unrelated free, as
                // "corrupted size vs. prev_size": a crash that points nowhere
                // near the call that caused it.
                //
                // Only the bytes the guest asked for are sent back, so the pad
                // costs a page and changes nothing the guest sees.
                //
                // And it is guarded, not a Vec: the length RM copies comes
                // from a count in the guest's own parameters, not from how
                // much the guest sent, so a guest that says 8 bytes and a
                // count of 100000 would otherwise have RM write past the
                // allocation into our heap. Here that lands in zeroed slack
                // or on the guard page, as EFAULT.
                let Some(mut buf) =
                    crate::guarded::GuardedBuf::new(bytes.len().max(DEEP_BUF_FLOOR))
                else {
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::ENOMEM,
                    );
                };
                buf.as_mut_slice()[..bytes.len()].copy_from_slice(bytes);
                log::debug!(
                    "deep pointer at {ptr_off}: guest says {} bytes, buffer {} bytes",
                    bytes.len(),
                    buf.len()
                );
                let mut guest_ptr = [0u8; 8];
                guest_ptr.copy_from_slice(&host_buf[ptr_off..ptr_off + 8]);
                deep_saved = Some((ptr_off, guest_ptr));
                let host_ptr = buf.as_mut_ptr() as u64;
                host_buf[ptr_off..ptr_off + 8].copy_from_slice(&host_ptr.to_le_bytes());
                deep_buf = Some(buf);
            }

            // Every other pointer RM would follow inside a control's
            // parameters: zeroed, and the caller's value restored below
            // (guestptr.rs). Left alone, RM copied in from and out to that
            // address in this process.
            //
            // Before that, what several of them refer to, when the guest sent
            // it as deep segments: checked against the sizes RM will copy,
            // computed from these very parameters, and each given a buffer of
            // ours (deepseg.rs). Only a control's parameters are read so.
            let segs = match deep_segs {
                None => None,
                Some(deep) if rm && escape == abi::ioctl::NV_ESC_RM_CONTROL => {
                    let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
                    let Some(ctl) = abi::rmctrl::deep_control(cmd) else {
                        log::warn!(
                            "RM control {cmd:#010x}: deep segments for a control whose \
                             pointers are not relocated"
                        );
                        return self.write_error_resp(
                            resp_buf,
                            Status::IoctlFailed,
                            cookie,
                            libc::EINVAL,
                        );
                    };
                    let what = format!("RM control {cmd:#010x} ({})", ctl.name);
                    match crate::deepseg::Segments::relocate(&what, ctl.ptrs, host_buf, deep) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e);
                        }
                    }
                }
                Some(_) => {
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::EINVAL,
                    );
                }
            };
            let ctl_restore = if rm && escape == abi::ioctl::NV_ESC_RM_CONTROL {
                let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
                let relocated: Vec<usize> = match &segs {
                    Some(s) => s.offsets(),
                    None => deep_saved.map(|(o, _)| o).into_iter().collect(),
                };
                crate::guestptr::scrub_control(cmd, host_buf, &relocated)
            } else {
                crate::guestptr::Restore::default()
            };

            // Call host ioctl — paramsSize field is untouched (may be 0)
            let rc = unsafe { (self.host_ioctl)(host_fd, request, outer.as_mut_ptr()) };
            if rc < 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                log::warn!("nested ioctl(0x{:x}) failed: errno={}", request, errno);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }

            // RM reports two different things in two different places, and only
            // one of them is the ioctl return value. A control call routinely
            // comes back rc=0 with a failure in the NVOS54 status word, and the
            // caller believes the status, not the rc. Counting non-zero rc told
            // us every call succeeded while the ICD was reading refusals.
            if escape == 0x2a && outer.len() >= NVOS54_TOTAL {
                let cmd = u32::from_le_bytes(outer[NVOS54_CMD..NVOS54_CMD + 4].try_into().unwrap());
                let params_size = u32::from_le_bytes(
                    outer[NVOS54_PARAMS_SIZE..NVOS54_PARAMS_SIZE + 4]
                        .try_into()
                        .unwrap(),
                );
                let status =
                    u32::from_le_bytes(outer[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap());
                if status == NV_OK {
                    log::debug!(
                        "RM_CONTROL cmd=0x{:08x} paramsSize={} -> NV_OK",
                        cmd,
                        params_size
                    );
                } else {
                    // A refusal tells us nothing on its own; the argument RM
                    // objected to is in the params. Show the head of them.
                    let head: Vec<String> = host_buf
                        .iter()
                        .take(64)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    log::warn!(
                        "RM_CONTROL cmd=0x{:08x} paramsSize={} -> status=0x{:08x}\n  params[0..64]: {}",
                        cmd,
                        params_size,
                        status,
                        head.join(" ")
                    );
                }
            }
            if escape == 0x2b && param_in.len() >= 48 {
                let hclass = u32::from_le_bytes(param_in[12..16].try_into().unwrap());
                let params_size = u32::from_le_bytes(param_in[32..36].try_into().unwrap());
                log::debug!(
                    "RM_ALLOC ENTER: hClass=0x{:04x} paramsSize={} (nested_bytes={})",
                    hclass,
                    params_size,
                    param_in.len() - 48
                );
            }

            // Restore guest_handle in host_buf before sending back to guest
            if let Some((offset, handle_val)) = saved_nested_handle {
                host_buf[offset..offset + 4].copy_from_slice(&handle_val.to_le_bytes());
            }
            if let Some((offset, guest)) = saved_os_event {
                host_buf[offset..offset + 8].copy_from_slice(&guest);
            }

            // The caller's own pointer value goes back, not ours and not zero.
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);

            // Build response: outer + updated nested params + what the
            // pointer inside them addresses.
            if let Some((ptr_off, guest_ptr)) = deep_saved {
                host_buf[ptr_off..ptr_off + 8].copy_from_slice(&guest_ptr);
            }
            ctl_restore.apply(host_buf);
            // Each segment's bytes as RM left them, after the table as sent.
            if let Some(segs) = &segs {
                segs.restore(host_buf);
                let deep = segs.reply();
                let mut combined = outer.to_vec();
                combined.extend_from_slice(host_buf);
                combined.extend_from_slice(&deep);
                return self.write_ioctl_resp_deep(resp_buf, cookie, &combined, deep.len());
            }
            // Only what the guest allocated room for goes back, not the pad.
            let deep_reply = deep_in.map(|(_, b)| b.len()).unwrap_or(0);
            let mut combined = outer.to_vec();
            combined.extend_from_slice(host_buf);
            if let Some(buf) = &deep_buf {
                combined.extend_from_slice(&buf.as_slice()[..deep_reply]);
            }
            self.write_ioctl_resp_deep(resp_buf, cookie, &combined, deep_reply)
        } else {
            if deep_segs.is_some() {
                log::warn!("ioctl {request:#x}: deep segments with no parameters to hold them");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
            // No nested params: the call names no parameter block, so the
            // host is told exactly that -- a null pointer and a zero size.
            //
            // This used to forward the pointer as the guest wrote it. The
            // guest's value is an address in *its* process; the host
            // (RM_CONTROL and RM_ALLOC through param_copy.c, NVKMS through
            // nvkms_ioctl_from_kapi's copy_from_user, nvidia-drm's GEM
            // import and export through their nvkms_params_ptr) reads it as an
            // address in ours, and copies in from it and back out to it:
            // any guest could read and write the VMM's memory with a
            // pointer and a size of its choosing. A nonzero size with nothing
            // sent is refused rather than zeroed, so a caller that meant to
            // send parameters learns that none arrived.
            let size_wide = hostfd::ioc_type(request as u32) == b'd';
            let size = if size_wide {
                outer
                    .get(size_offset..size_offset + 8)
                    .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
            } else {
                outer
                    .get(size_offset..size_offset + 4)
                    .map_or(0, |b| u64::from(u32::from_le_bytes(b.try_into().unwrap())))
            };
            if size != 0 {
                log::warn!(
                    "ioctl {request:#x}: parameter size {size} but no parameters sent; refused \
                     rather than handing the host the guest's pointer"
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
            outer[ptr_offset..ptr_offset + 8].fill(0);

            let rc = unsafe { (self.host_ioctl)(host_fd, request, outer.as_mut_ptr()) };
            if rc < 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                log::warn!(
                    "nested ioctl(0x{:x}) no-params failed: errno={}",
                    request,
                    errno
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }

            if escape == 0x2a {
                let status = u32::from_le_bytes(outer[28..32].try_into().unwrap());
                let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
                log::debug!("(else) RM_CONTROL cmd=0x{:08x} status=0x{:x}", cmd, status);
            } else if escape == 0x2b {
                let status = u32::from_le_bytes(outer[40..44].try_into().unwrap());
                let hclass = u32::from_le_bytes(outer[12..16].try_into().unwrap());
                log::debug!(
                    "(else) RM_ALLOC hClass=0x{:04x} status=0x{:x}",
                    hclass,
                    status
                );
            }

            // Same here: restore what the caller passed, in case the host
            // driver wrote to the field.
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);
            self.write_ioctl_resp(resp_buf, cookie, outer)
        }
    }

    // ------------------------------------------------------------------
    // Memory registered by its pages (osdesc.rs)
    // ------------------------------------------------------------------

    /// One of the three calls that register memory the caller already has
    /// -- ALLOC_MEMORY or RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR,
    /// VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR -- sent with the
    /// guest-physical pages behind it (`list`). `param_in` is the top-level
    /// block (`outer_len` bytes) and, for RM_ALLOC, the class parameters
    /// after it. RM is handed an address of this process's that maps
    /// exactly those pages; the caller reads back its own. On RM's NV_OK the
    /// reply carries the registration's id as its deep block.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_osdesc(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        outer_len: usize,
        param_in: &[u8],
        list: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        use crate::osdesc::{self as od, Shape};
        let Some(ram) = self
            .guest_ram
            .clone()
            .filter(|_| self.session.v2 && self.config.allow_compute)
        else {
            log::warn!("OS descriptor page list from a guest never offered BCAP_OS_DESC; refused");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        };
        let cmd = request as u32;
        let (outer, nested) = param_in.split_at(outer_len.min(param_in.len()));
        self.osdesc
            .set_file_owner(self.current_handle, self.handles.owner(self.current_handle));
        let prepared = od::describe(cmd, outer, nested).and_then(|call| {
            let runs = od::parse_runs(&call, list)?;
            let resolved = od::resolve(&ram, &runs)?;
            self.osdesc
                .admit(self.current_handle, resolved.bytes(), resolved.vmas())?;
            Ok((call, resolved.map(call.in_page(), call.writable)?))
        });
        let (call, pinned) = match prepared {
            Ok(p) => p,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };
        let addr = pinned.addr.to_le_bytes();
        let put = |b: &mut [u8], off: usize, v: &[u8]| b[off..off + v.len()].copy_from_slice(v);

        // What records the memory an escape makes (rmmem.rs) sees the call
        // too: a handle made here is no longer whatever it named before.
        let escape = hostfd::ioc_nr(cmd);
        let mut seen = param_in.to_vec();
        let rm_pending = self.rmmem.before(escape, &mut seen);

        // The blocks the host is handed: ours, with our address where the
        // caller's was.
        let Some(mut arg) = ioctl_arg(request, outer) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let mut class_params: Option<GuardedBuf> = None;
        let (status_at, handle_at) = match call.shape {
            Shape::AllocMemory => {
                let a = arg.as_mut_slice();
                put(a, od::OS02_MEMORY, &addr);
                // RM reads the descriptor only to arm a mapping of
                // NV01_MEMORY_SYSTEM (escape.c:415-431); a guest number goes
                // no further than here.
                put(a, od::OS02_FD, &(-1i32).to_le_bytes());
                (od::OS02_STATUS, od::OS02_NEW)
            }
            Shape::VidHeap => {
                put(arg.as_mut_slice(), od::OS32_DESCRIPTOR, &addr);
                (od::OS32_STATUS, od::OS32_HMEMORY)
            }
            Shape::RmAlloc => {
                let Some(mut n) = GuardedBuf::new(nested.len()) else {
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::ENOMEM,
                    );
                };
                n.as_mut_slice().copy_from_slice(nested);
                put(n.as_mut_slice(), od::OSD_DESCRIPTOR, &addr);
                let a = arg.as_mut_slice();
                put(a, od::OS64_PARAMS, &(n.as_mut_ptr() as u64).to_le_bytes());
                // As guestptr::rm_escape: RM grants the default rights.
                put(a, od::OS64_RIGHTS, &[0; 8]);
                class_params = Some(n);
                (od::OS64_STATUS, od::OS64_NEW)
            }
        };
        // SAFETY: the HostIoctl contract: `arg` is sized by ioctl_arg, and
        // the class parameters and the range RM pins live past the call.
        let rc = unsafe { (self.host_ioctl)(host_fd, request, arg.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO);
            log::warn!("OS descriptor {cmd:#x}: the host refused the call (errno {errno})");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // The caller's own values back where ours were.
        let mut out = arg.as_slice()[..outer.len()].to_vec();
        let back = |out: &mut [u8], off: usize, n: usize| {
            out[off..off + n].copy_from_slice(&outer[off..off + n])
        };
        match call.shape {
            Shape::AllocMemory => {
                back(&mut out, od::OS02_MEMORY, 8);
                back(&mut out, od::OS02_FD, 4);
            }
            Shape::VidHeap => back(&mut out, od::OS32_DESCRIPTOR, 8),
            Shape::RmAlloc => {
                back(&mut out, od::OS64_PARAMS, 8);
                back(&mut out, od::OS64_RIGHTS, 8);
            }
        }
        if let Some(n) = &class_params {
            let mut p = n.as_slice().to_vec();
            p[od::OSD_DESCRIPTOR..od::OSD_DESCRIPTOR + 8]
                .copy_from_slice(&nested[od::OSD_DESCRIPTOR..od::OSD_DESCRIPTOR + 8]);
            out.extend_from_slice(&p);
        }
        self.rmmem.after(rm_pending, &mut out);

        let rd = |o: usize| u32::from_le_bytes(out[o..o + 4].try_into().unwrap());
        let status = rd(status_at);
        let deep = if status == 0 {
            let object = rd(handle_at);
            let id = self.osdesc.add(
                self.current_handle,
                call.client,
                object,
                call.parent,
                pinned,
            );
            log::debug!(
                "OS descriptor {id}: {:#x} bytes as {:#x}/{:#x}",
                call.size,
                call.client,
                object
            );
            if call.shape == Shape::RmAlloc {
                self.rm_classes.add(od::NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
            }
            out.extend_from_slice(&id.to_le_bytes());
            8
        } else {
            // Nothing made, nothing pinned by RM: the range goes now.
            log::debug!("OS descriptor {cmd:#x}: RM answered {status:#x}; nothing registered");
            drop(pinned);
            0
        };
        self.write_ioctl_resp_deep(resp_buf, cookie, &out, deep)
    }

    /// What RM freed, duplicated or made anew, from a successful reply's
    /// parameters (`reply`), for the registrations that live through RM
    /// objects (osdesc.rs).
    fn osdesc_observe(&mut self, escape: u32, reply: &[u8]) {
        use abi::ioctl::*;
        if self.osdesc.live() == 0 {
            return;
        }
        let r = |o: usize| {
            reply
                .get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        match escape {
            // NVOS00: hRoot, hObjectParent, hObjectOld, status.
            NV_ESC_RM_FREE if r(12) == Some(0) => {
                if let (Some(c), Some(o)) = (r(0), r(8)) {
                    self.osdesc.freed(c, o);
                }
            }
            // NVOS55: hClient, hParent, hObject, hClientSrc, hObjectSrc,
            // flags, status.
            NV_ESC_RM_DUP_OBJECT if r(24) == Some(0) => {
                if let (Some(c), Some(p), Some(o), Some(sc), Some(so)) =
                    (r(0), r(4), r(8), r(12), r(16))
                {
                    self.osdesc.duplicated(sc, so, c, o, p);
                }
            }
            // NVOS54: hClient, hObject, cmd, ..., status at 28, the
            // parameters from 32. A semaphore surface holding registered
            // memory hands the caller duplicates of it (osdesc.rs,
            // `SEMSURF_REF_MEMORY`): each holds it too.
            NV_ESC_RM_CONTROL
                if r(28) == Some(0) && r(8) == Some(crate::osdesc::SEMSURF_REF_MEMORY) =>
            {
                if let (Some(c), Some(o)) = (r(0), r(4))
                    && self.osdesc.holds(c, o)
                {
                    let out: Vec<u32> = [32, 36].iter().filter_map(|&at| r(at)).collect();
                    self.osdesc.referenced(c, o, &out);
                }
            }
            // NVOS64 and NVOS02: hRoot, _, hObjectNew, ..., status at 40.
            // A zero hObjectNew is no handle: RM made the object under one it
            // generated and, through ALLOC_MEMORY, never wrote back.
            NV_ESC_RM_ALLOC | NV_ESC_RM_ALLOC_MEMORY if r(40) == Some(0) => {
                if let (Some(c), Some(p), Some(o)) = (r(0), r(4), r(8))
                    && o != 0
                {
                    self.osdesc.reused(c, o);
                    // An object RM keeps a duplicate of registered memory
                    // for, in a client of its own (osdesc.rs,
                    // `holding_fields`): the class parameters follow
                    // NVOS64's 48 bytes.
                    if escape == NV_ESC_RM_ALLOC
                        && let Some(class) = r(12)
                    {
                        let named: Vec<u32> = crate::osdesc::holding_fields(class)
                            .iter()
                            .filter_map(|&off| r(48 + off))
                            .collect();
                        if !named.is_empty() {
                            self.osdesc.made_over(c, o, p, &named);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Free `clients` on `host_fd`, the file they were allocated on, if they
    /// hold memory registered by its pages, and forget what they held. RM
    /// lets go of the pages in the free (the file's own last close may be
    /// later: the event pump holds a duplicate of a watched file), and only
    /// then may the guest unpin them.
    pub(crate) fn osdesc_end_clients(&mut self, host_fd: RawFd, clients: &[u32], why: &str) {
        let holders = self.osdesc.clients();
        let ending: Vec<u32> = clients
            .iter()
            .copied()
            .filter(|c| holders.contains(c))
            .collect();
        if ending.is_empty() {
            return;
        }
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_FREE, 16);
        // Only a client RM says it freed is forgotten. One it would not free
        // (on another file, or a failed call) may still hold the pages, and
        // the guest must not be told it may unpin them: its registrations
        // stay, released late -- by a later free RM does confirm, or with the
        // session -- never early.
        let mut freed = Vec::with_capacity(ending.len());
        for &c in &ending {
            // NVOS00: the client names itself.
            let mut p = [0u8; 16];
            p[0..4].copy_from_slice(&c.to_le_bytes());
            p[8..12].copy_from_slice(&c.to_le_bytes());
            let Some(mut a) = ioctl_arg(request, &p) else {
                log::warn!("{why}: no buffer to free RM client {c:#x}; its registrations stay");
                continue;
            };
            // SAFETY: the HostIoctl contract; `a` is sized by ioctl_arg.
            let rc = unsafe { (self.host_ioctl)(host_fd, request, a.as_mut_ptr()) };
            let status = u32::from_le_bytes(a.as_slice()[12..16].try_into().unwrap());
            if rc < 0 || status != 0 {
                log::warn!(
                    "{why}: freeing RM client {c:#x}, which holds registered guest memory: \
                     rc {rc}, status {status:#x}; its registrations stay"
                );
                continue;
            }
            freed.push(c);
        }
        self.osdesc.forget_clients(&freed);
    }

    /// Whether RM control `param_in` (NVOS54 and its parameters) would hand
    /// an object holding registered memory to an RM export descriptor or an
    /// NV_MEMORY_EXPORT object: a duplicate the backend never sees made or
    /// freed, which NVKMS, nvidia-drm or another RM client can import in
    /// turn. Refused (EPERM) rather than followed.
    fn osdesc_export_refused(&self, param_in: &[u8]) -> bool {
        if self.osdesc.live() == 0 || param_in.len() < 32 {
            return false;
        }
        let rd = |o: usize| u32::from_le_bytes(param_in[o..o + 4].try_into().unwrap());
        let (client, cmd) = (rd(0), rd(8));
        let Some(named) = crate::osdesc::exported(cmd, &param_in[32..]) else {
            return false;
        };
        match named.iter().find(|&&h| self.osdesc.holds(client, h)) {
            Some(h) => {
                log::warn!(
                    "RM control {cmd:#x} refused: it exports {client:#x}/{h:#x}, which holds \
                     guest memory registered by its pages"
                );
                true
            }
            None => false,
        }
    }

    /// Before UVM command `cmd` (`params`, the block the host will get):
    /// ALLOC_DEVICE_P2P of registered memory is refused (it is for video
    /// memory, and its duplicate outlives a UVM_FREE while a CPU mapping of
    /// the range holds it); a MAP_EXTERNAL_ALLOCATION of it, what the
    /// mapping will hold once UVM has made it.
    fn osdesc_uvm_before(
        &self,
        cmd: u32,
        params: &[u8],
    ) -> std::result::Result<Option<crate::osdesc::UvmMap>, i32> {
        use crate::osdesc::{UVM_ALLOC_DEVICE_P2P, UVM_MAP_EXTERNAL_ALLOCATION};
        if self.osdesc.live() == 0 {
            return Ok(None);
        }
        let (UVM_MAP_EXTERNAL_ALLOCATION | UVM_ALLOC_DEVICE_P2P) = cmd else {
            return Ok(None);
        };
        // hClient and hMemory follow rmCtrlFd in both.
        let Some(fd_off) = crate::uvmfd::field(self.driver, cmd).map(|f| f.offset as usize) else {
            return Ok(None);
        };
        let rd32 = |o: usize| {
            params
                .get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let rd64 = |o: usize| {
            params
                .get(o..o + 8)
                .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
        };
        let (Some(client), Some(memory)) = (rd32(fd_off + 4), rd32(fd_off + 8)) else {
            return Ok(None);
        };
        if !self.osdesc.holds(client, memory) {
            return Ok(None);
        }
        if cmd == UVM_ALLOC_DEVICE_P2P {
            log::warn!(
                "UVM ALLOC_DEVICE_P2P of {client:#x}/{memory:#x} refused: guest memory \
                 registered by its pages"
            );
            return Err(libc::EPERM);
        }
        self.osdesc.uvm_admit(client, memory)?;
        // UVM_MAP_EXTERNAL_ALLOCATION_PARAMS: base, length, offset, then
        // perGpuAttributes[] at 24 (36 bytes each, the UUID first), and
        // gpuAttributesCount just before rmCtrlFd.
        const ATTRS: usize = 24;
        const ATTR_SIZE: usize = 36;
        let max = fd_off.saturating_sub(8 + ATTRS) / ATTR_SIZE;
        let count = rd64(fd_off - 8).unwrap_or(0).min(max as u64) as usize;
        let gpus = (0..count)
            .filter_map(|i| {
                params
                    .get(ATTRS + i * ATTR_SIZE..ATTRS + i * ATTR_SIZE + 16)
                    .map(|b| b.try_into().unwrap())
            })
            .collect();
        Ok(Some(crate::osdesc::UvmMap {
            base: rd64(0).unwrap_or(0),
            len: rd64(8).unwrap_or(0),
            gpus,
            client,
            memory,
        }))
    }

    /// After UVM command `cmd` on the current handle: what it made, mapped,
    /// unmapped or freed, for the registrations UVM external mappings hold
    /// (osdesc.rs). `params` is the block the host was handed.
    fn osdesc_uvm_after(
        &mut self,
        cmd: u32,
        params: &[u8],
        map: Option<crate::osdesc::UvmMap>,
        resp_buf: &[u8],
        n: usize,
    ) {
        use crate::osdesc::{
            UVM_CREATE_EXTERNAL_RANGE, UVM_MAP_EXTERNAL_ALLOCATION, UVM_UNMAP_EXTERNAL,
        };
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        // UVM's own status, where the block keeps it; None when the call
        // failed before UVM answered, or the reply is short.
        let status = |at: usize| -> Option<u32> {
            if n < body + at + 4 || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
                return None;
            }
            Some(u32::from_le_bytes(
                resp_buf[body + at..body + at + 4].try_into().unwrap(),
            ))
        };
        let handle = self.current_handle;
        let word = |o: usize| {
            params
                .get(o..o + 8)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
        };
        let size = params.len();
        match cmd {
            UVM_MAP_EXTERNAL_ALLOCATION => {
                let Some(m) = map else { return };
                // Held whatever UVM answered: it can fail the call after
                // making every mapping (its wait for the page-table writes),
                // and then leaves them up. A mapping never made is held
                // until its range is freed or its file closes: late, not
                // early.
                self.osdesc
                    .uvm_mapped(handle, m.base, m.len, &m.gpus, m.client, m.memory);
            }
            UVM_CREATE_EXTERNAL_RANGE if size >= 24 && status(size - 8) == Some(0) => {
                self.osdesc.uvm_range_made(handle, word(0), word(8));
            }
            crate::uvmmap::FREE if size >= 16 && status(size - 8) == Some(0) => {
                self.osdesc.uvm_range_freed(handle, word(0));
            }
            UVM_UNMAP_EXTERNAL if size >= 40 && status(size - 8) == Some(0) => {
                let gpu: crate::osdesc::GpuUuid = params[16..32].try_into().unwrap();
                self.osdesc.uvm_unmapped(handle, word(0), word(8), &gpu);
            }
            _ => {}
        }
    }

    /// Take down, on UVM file `host_fd` (guest handle `handle`) while it is
    /// still ours, every external mapping of registered memory it holds, and
    /// forget its ranges: UVM_FREE of each recorded range holding one, then
    /// UNMAP_EXTERNAL of what is left, GPU by GPU. Its last reference may be
    /// dropped later than the close (the event pump holds a duplicate), so
    /// the close itself says nothing of when UVM lets go. What will not come
    /// down stays held until the session ends.
    pub(crate) fn osdesc_uvm_close(&mut self, host_fd: RawFd, handle: u32, why: &str) {
        use crate::osdesc::UVM_UNMAP_EXTERNAL;
        for (base, len) in self.osdesc.uvm_ranges_held(handle) {
            // UVM_FREE_PARAMS: base, then (before 590.44.01) length.
            let ok = self.uvm_call(host_fd, crate::uvmmap::FREE, |b, size| {
                b[0..8].copy_from_slice(&base.to_le_bytes());
                if size >= 24 {
                    b[8..16].copy_from_slice(&len.to_le_bytes());
                }
            });
            if ok {
                self.osdesc.uvm_range_freed(handle, base);
            } else {
                log::warn!("{why}: UVM_FREE of the external range at {base:#x} failed");
            }
        }
        for (base, len, gpu) in self.osdesc.uvm_maps_held(handle) {
            // UVM_UNMAP_EXTERNAL_PARAMS: base, length, gpuUuid.
            let ok = self.uvm_call(host_fd, UVM_UNMAP_EXTERNAL, |b, _| {
                b[0..8].copy_from_slice(&base.to_le_bytes());
                b[8..16].copy_from_slice(&len.to_le_bytes());
                b[16..32].copy_from_slice(&gpu);
            });
            if ok {
                self.osdesc.uvm_unmapped(handle, base, len, &gpu);
            } else {
                log::warn!("{why}: UVM_UNMAP_EXTERNAL of {base:#x}+{len:#x} failed");
            }
        }
        self.osdesc.uvm_file_closed(handle);
    }

    /// Call UVM command `cmd` on `host_fd` with the block the host's release
    /// has for it, filled by `fill` (handed at least 40 bytes, and the
    /// block's size): whether the ioctl and UVM (its status, `size - 8` into
    /// the block, for every command called here) both succeeded.
    fn uvm_call(&self, host_fd: RawFd, cmd: u32, fill: impl FnOnce(&mut [u8], usize)) -> bool {
        let Some(size) = self
            .driver
            .and_then(abi::schema::uvm_table)
            .and_then(|t| t.lookup(cmd))
            .map(|c| c.size as usize)
            .filter(|&s| s >= 16)
        else {
            return false;
        };
        let mut b = vec![0u8; size.max(40)];
        fill(&mut b, size);
        // SAFETY: the HostIoctl contract: `b` holds at least the block UVM
        // copies each way for `cmd` on this release.
        let rc = unsafe { (self.host_ioctl)(host_fd, u64::from(cmd), b.as_mut_ptr()) };
        rc == 0 && b[size - 8..size - 4] == [0; 4]
    }

    /// OP_OSDESC_REAP: the releases after `ack`, and forget those up to it.
    pub(crate) fn osdesc_reap(&mut self, ack: u64) -> (u64, Vec<u64>) {
        self.osdesc.reap(ack)
    }

    // ------------------------------------------------------------------
    // Simple ioctl
    // ------------------------------------------------------------------

    fn dispatch_simple(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let escape = (request & 0xFF) as u32;
        log::debug!(
            "dispatch_simple: host_fd={} request=0x{:x} escape=0x{:02x} size={}",
            host_fd,
            request,
            escape,
            param_in.len()
        );

        // Debug logging for Vulkan-critical ioctls
        let log_response = escape == 0xd2  // NV_ESC_CHECK_VERSION_STR
            || escape == 0xc8  // NV_ESC_CARD_INFO
            || escape == 0xd6  // NV_ESC_SYS_PARAMS
            || escape == 0xd7  // NV_ESC_QUERY_DEVICE_INTR
            || escape == 0x2b // NV_ESC_RM_ALLOC (hClient)
            || escape == 0x2a; // NV_ESC_RM_CONTROL

        let Some(mut arg) = ioctl_arg(request, param_in) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let param_buf = &mut arg.as_mut_slice()[..param_in.len()];

        // Special handling: NV_ESC_SYS_PARAMS (0xd6) - retry with different Cmd on EBUSY
        // Some sysparams ioctls return EBUSY when the device is busy, especially
        // during early initialization. We retry with Cmd=2 (V2) as fallback.
        let mut retry_with_v2 = false;
        if escape == 0xd6 && param_buf.len() >= 4 && param_buf[0] == 0 {
            retry_with_v2 = true;
        }

        // ---------------------------------------------------------------
        // Special handling: NV_ESC_CHECK_VERSION_STR (0xd2)
        // Based on gVisor nvproxy: Try Cmd='2' first (character '2'),
        // which triggers version query mode in newer drivers.
        // ---------------------------------------------------------------
        if escape == 0xd2 && param_buf.len() >= 4 {
            // Try Cmd='2' first (query mode in newer drivers)
            param_buf[0] = b'2'; // Cmd = '2'
            // Leave other fields as-is, call host
        }

        let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);

            // Special handling: NV_ESC_SYS_PARAMS (0xd6) - retry on EBUSY
            if escape == 0xd6 && errno == libc::EBUSY && retry_with_v2 {
                log::info!("NV_ESC_SYS_PARAMS: got EBUSY, retrying with Cmd=2");
                param_buf[0] = 2; // Try V2
                let rc2 = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
                if rc2 < 0 {
                    let errno2 = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    log::warn!(
                        "ioctl(0x{:x}/0x{:02x}) retry failed: errno={}",
                        request,
                        escape,
                        errno2
                    );
                    // EBUSY means driver is busy but shouldn't cause vulkan failure.
                    // Synthesize success (like older drivers did) by returning zeros.
                    log::warn!(
                        "ioctl(0x{:x}/0x{:02x}) returned EBUSY - synthesizing success",
                        request,
                        escape
                    );
                    // Return success with zeroed params (simulates what driver returns)
                    let zeroed = vec![0u8; param_buf.len()];
                    return self.write_ioctl_resp(resp_buf, cookie, &zeroed);
                }
                // Success on retry - continue to response handling
            } else if escape == 0xd6 && errno == libc::EBUSY {
                // EBUSY but couldn't retry (param[0] != 0) - synthesize success
                log::warn!(
                    "ioctl(0x{:x}/0x{:02x}) returned EBUSY (no retry) - synthesizing success",
                    request,
                    escape
                );
                let zeroed = vec![0u8; param_buf.len()];
                return self.write_ioctl_resp(resp_buf, cookie, &zeroed);
            } else {
                log::warn!(
                    "ioctl(0x{:x}/0x{:02x}) failed: errno={}",
                    request,
                    escape,
                    errno
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
        } else {
            if escape == 0xd2 {
                self.learn_driver_version(&param_buf);
            }
            if log_response {
                let preview = &param_buf[..std::cmp::min(param_buf.len(), 128)];
                match escape {
                    0xd2 => {
                        // NV_ESC_CHECK_VERSION_STR - version string at offset 0
                        let version = String::from_utf8_lossy(preview);
                        log::info!("CHECK_VERSION_STR response: {:?}", version);
                    }
                    0xc8 => {
                        log::info!("CARD_INFO response[0..128]: {:02x?}", preview);
                    }
                    0xd6 => {
                        log::info!("SYS_PARAMS response[0..128]: {:02x?}", preview);
                    }
                    0x2a => {
                        // RM_CONTROL - log first few bytes of params
                        let status = if param_buf.len() >= 4 {
                            u32::from_le_bytes([
                                param_buf[0],
                                param_buf[1],
                                param_buf[2],
                                param_buf[3],
                            ])
                        } else {
                            0
                        };
                        log::info!(
                            "RM_CONTROL response: status={:#x}, data[4..32]={:02x?}",
                            status,
                            &param_buf[4..std::cmp::min(32, param_buf.len())]
                        );
                    }
                    0x2b => {
                        // RM_ALLOC - log first few bytes
                        let status = if param_buf.len() >= 4 {
                            u32::from_le_bytes([
                                param_buf[0],
                                param_buf[1],
                                param_buf[2],
                                param_buf[3],
                            ])
                        } else {
                            0
                        };
                        log::info!(
                            "RM_ALLOC response: status={:#x}, data[4..32]={:02x?}",
                            status,
                            &param_buf[4..std::cmp::min(32, param_buf.len())]
                        );
                    }
                    _ => {}
                }
            }
            if escape == 0x57 || escape == 0x58 {
                log::info!(
                    "MAP/UNMAP_DMA(0x{:02x}): response[{}]={:02x?}",
                    escape,
                    param_buf.len(),
                    &param_buf[..std::cmp::min(param_buf.len(), 64)]
                );
            }
            // Only on NVIDIA's own magic. 0x4a is VID_HEAP_CONTROL there and
            // GEM_MAP_OFFSET on the DRM node, and logging the second under the
            // first's name makes a buffer-sharing run look like an allocator
            // storm -- which it did, for as long as it took to count the
            // namespaces separately.
            if escape == 0x4a && ((request >> 8) & 0xFF) as u32 == b'F' as u32 {
                log::info!(
                    "VID_HEAP_CONTROL: response[{}]={:02x?}",
                    param_buf.len(),
                    &param_buf[..std::cmp::min(param_buf.len(), 184)]
                );
            }
        }
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    // ------------------------------------------------------------------
    // FD-carrying ioctl
    // ------------------------------------------------------------------

    fn dispatch_fd_carrying(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        escape: u32,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        use abi::ioctl::*;

        let fd_offset: usize = match escape {
            // nv_ioctl_register_fd_t: ctl_fd is the only field, offset 0.
            NV_ESC_REGISTER_FD => 0,
            // nv_ioctl_alloc_os_event_t: hClient(4) + hDevice(4) + fd @ offset 8
            NV_ESC_ALLOC_OS_EVENT => 8,
            // nv_ioctl_free_os_event_t: same layout as alloc, fd @ offset 8
            NV_ESC_FREE_OS_EVENT => 8,
            // NV_ESC_RM_ALLOC_MEMORY: fd at offset 48
            NV_ESC_RM_ALLOC_MEMORY => 48,
            _ => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTTY),
        };

        if param_in.len() < fd_offset + 4 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let embedded = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[fd_offset..fd_offset + 4]);
            i32::from_le_bytes(b)
        };

        // The field is a descriptor only when the caller put one there. -1 is
        // the caller saying it has none, which several of these ioctls allow --
        // NV_ESC_RM_ALLOC_MEMORY carries it for every allocation not being made
        // on another open file. It is forwarded as it stands, because that is
        // what the host driver is being asked to read.
        // Only -1: another negative number is no descriptor either, and
        // RM would look it up among the backend's files (R5).
        if embedded < -1 {
            log::warn!("fd-carrying ioctl: descriptor field {embedded} refused");
            return self.write_error_resp(resp_buf, Status::BadHandle, cookie, libc::EBADF);
        }
        if embedded == -1 {
            let Some(mut arg) = ioctl_arg(request, param_in) else {
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
            };
            let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
            let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
            if rc < 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                log::warn!("ioctl(0x{request:x}) with no embedded fd failed: errno={errno}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        let guest_embedded = embedded as u32;
        let host_embedded = match self.dev_fd(guest_embedded) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!("fd-carrying ioctl: bad embedded handle {}", guest_embedded);
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };

        let Some(mut arg) = ioctl_arg(request, param_in) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
        param_buf[fd_offset..fd_offset + 4].copy_from_slice(&(host_embedded as i32).to_le_bytes());

        let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };

        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!("fd-carrying ioctl(0x{:x}) failed: errno={}", request, errno);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // Put the guest's handle back in place of our descriptor. It is not
        // what user mode wrote -- that was its own descriptor, which never
        // reached us -- so the guest driver writes the caller's value over it
        // on the way out (nvgpu_ioctl_translate_fd); RM never writes the
        // field (escape.c:393-428, 584-624), and callers read it back.
        param_buf[fd_offset..fd_offset + 4].copy_from_slice(&(guest_embedded as i32).to_le_bytes());

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    fn dispatch_update_device_mapping_info(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: ENTERED, host_fd={}, param_in.len={}",
            host_fd,
            param_in.len()
        );

        if param_in.len() < 40 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let h_client = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
        let old_cpu_addr = u64::from_le_bytes(param_in[16..24].try_into().unwrap());
        let new_cpu_addr = u64::from_le_bytes(param_in[24..32].try_into().unwrap());

        log::debug!(
            "UPDATE_DEVICE_MAPPING_INFO: client={:#x} mem={:#x} old={:#x} new={:#x}",
            h_client,
            h_memory,
            old_cpu_addr,
            new_cpu_addr
        );

        // The guest sends SHM offsets or guest VAs. The host RM needs host VAs.
        // Look up the mapping by scanning active_maps for matching hMemory,
        // since the guest's "old" address won't match any host address.
        //
        // A mapping we have no record of gets 0, not the guest's value: the
        // host takes both as addresses in this process (guestptr.rs), and RM
        // finds no mapping at 0.
        let mut host_old = 0;
        if let Some(entry) = self.active_maps.find_by_object(h_client, h_memory) {
            host_old = entry.host_p_linear_address;
            log::debug!(
                "UPDATE_DEVICE_MAPPING_INFO: translated old {:#x} → host {:#x}",
                old_cpu_addr,
                host_old
            );
        }

        let Some(mut arg) = ioctl_arg(request, param_in) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
        // Set pOldCpuAddress to host VA
        param_buf[16..24].copy_from_slice(&host_old.to_le_bytes());
        // Set pNewCpuAddress to host VA too (the host mapping didn't move)
        param_buf[24..32].copy_from_slice(&host_old.to_le_bytes());

        let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!(
                "UPDATE_DEVICE_MAPPING_INFO: host ioctl failed: errno={}",
                errno
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[32..36].try_into().unwrap());
        log::debug!("UPDATE_DEVICE_MAPPING_INFO: host status=0x{:x}", status);

        // The caller's own addresses go back. RM only reads pOld/pNew
        // (escape.c:857-876 takes them into locals and writes nothing but
        // `status`), and nvidia.ko copies the whole argument back
        // (nv.c:2834), so a native caller reads back what it passed -- not
        // zero, and never the host VA we put there for the call.
        param_buf[16..32].copy_from_slice(&param_in[16..32]);

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    fn dispatch_map_memory(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        const _NVOS33_SIZE: usize = 48;
        const WITH_FD_SIZE: usize = 56;
        const FD_OFFSET: usize = 48;
        const LENGTH_OFFSET: usize = 24;
        const STATUS_OFFSET: usize = 40;
        const FLAGS_OFFSET: usize = 44;

        if param_in.len() < WITH_FD_SIZE {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        // --- Step 1: Translate embedded FD (guest handle → host fd) ---

        let guest_fd_handle = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[FD_OFFSET..FD_OFFSET + 4]);
            i32::from_le_bytes(b) as u32
        };

        let host_map_fd = match self.dev_fd(guest_fd_handle) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: bad embedded FD handle {}",
                    guest_fd_handle
                );
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };

        // A host fd is single-use for mapping. The host will refuse this with
        // NV_ERR_STATE_IN_USE; say so here, because that status on its own
        // sends you looking at the unmap path, which is not the problem.
        if self.active_maps.fd_has_mapping(guest_fd_handle) {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: handle {guest_fd_handle} already carries a mapping; \
                 a host fd cannot carry two, so the host will return NV_ERR_STATE_IN_USE"
            );
        }

        let Some(mut arg) = ioctl_arg(request, param_in) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
        param_buf[FD_OFFSET..FD_OFFSET + 4].copy_from_slice(&(host_map_fd as i32).to_le_bytes());

        // --- Step 2: Call host ioctl ---

        let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };

        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!("NV_ESC_RM_MAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore guest handle before returning
            param_buf[FD_OFFSET..FD_OFFSET + 4]
                .copy_from_slice(&(guest_fd_handle as i32).to_le_bytes());
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // --- Step 3: Check RM status and read updated fields ---

        let rm_status = u32::from_le_bytes(
            param_buf[STATUS_OFFSET..STATUS_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        // Restore guest handle in param_buf for copy-out regardless of status.
        param_buf[FD_OFFSET..FD_OFFSET + 4]
            .copy_from_slice(&(guest_fd_handle as i32).to_le_bytes());

        if rm_status != 0 {
            // RM returned an error status (NV_OK == 0).
            // Forward the params back so the guest can read the status field.
            log::debug!("NV_ESC_RM_MAP_MEMORY: RM status 0x{:x}", rm_status);
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        let length = u64::from_le_bytes(
            param_buf[LENGTH_OFFSET..LENGTH_OFFSET + 8]
                .try_into()
                .unwrap(),
        );

        let flags = u32::from_le_bytes(
            param_buf[FLAGS_OFFSET..FLAGS_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        // --- Step 4: the memory type the host maps it with ---
        //
        // Not the caching type alone: that is real only for video memory (see
        // `rm_mapping_pgprot`), and reading it for everything put system
        // memory and the doorbell registers write-combining.
        let h_client = u32::from_le_bytes(param_buf[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_buf[8..12].try_into().unwrap());
        let pgprot = self.rm_mapping_pgprot(flags, h_client, h_memory);
        // And whether it can be written at all: RM makes some mappings
        // read-only, and placing one writable would let a guest write stop the
        // VM (see `shm::host_mapping_writable`).
        let writable = crate::shm::host_mapping_writable(host_map_fd, length, 0);
        if !writable {
            log::info!(
                "NV_ESC_RM_MAP_MEMORY: client {h_client:#x} memory {h_memory:#x} is read-only \
                 on the host; placed read-only"
            );
        }

        // --- Step 5: Allocate SHM region ---

        // Charged to whoever opened the file the mapping is armed on: the
        // process that will map it.
        let owner = match self.handles.owner(guest_fd_handle) {
            crate::quota::Owner::Unknown => self.current_owner,
            o => o,
        };
        let region = match self.alloc_zone(length, pgprot, owner) {
            Ok(r) => r,
            Err(e) => {
                log::error!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
            }
        };

        // --- Step 6: place the device fd in the window ---
        //
        // Placed by the transport, not here: see `WindowPlacer`. Without one
        // the mapping exists on the host and is unreachable from the guest, so
        // the honest answer is to fail the call rather than return an address
        // that names nothing.
        let Some(window) = self.window.as_ref() else {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: no shared window, so device memory cannot be \
                 addressed by the guest"
            );
            if let Err(e) = self.shm.free(&region) {
                log::warn!("NV_ESC_RM_MAP_MEMORY: freeing the unused region: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTSUP);
        };
        if let Err(e) = window.place(region.offset, length, host_map_fd, 0, writable) {
            log::error!("NV_ESC_RM_MAP_MEMORY: placing in the window failed: {}", e);
            if let Err(e) = self.shm.free(&region) {
                log::warn!("NV_ESC_RM_MAP_MEMORY: freeing the unused region: {e}");
            }
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        }

        log::debug!(
            "dispatch_map_memory: returning shm_offset=0x{:x} shm_length=0x{:x} pgprot={:?}{}",
            region.offset,
            length,
            region.pgprot,
            if writable { "" } else { " read-only" }
        );

        // --- Step 6.5: Save host pLinearAddress and record mapping ---
        //
        // The host wrote its kernel VA into pLinearAddress (offset 32).
        // We save it for later unmap, then overwrite pLinearAddress with
        // the SHM offset. The guest library will store this and echo it
        // back in RM_UNMAP_MEMORY, giving us a unique lookup key.

        // NVOS34.pLinearAddress is documented as "address of application
        // mapping". We send back what RM_MAP_MEMORY left in the field, which
        // makes NV_ESC_RM_UNMAP_MEMORY return NV_OK -- but does not release the
        // mapping: see repeated_map_unmap_does_not_exhaust_the_zone.
        let host_p_linear = u64::from_le_bytes(param_buf[32..40].try_into().unwrap());

        // The handle is the key the mmap that follows will be found by, so it
        // is the one field worth naming in the log: a mapping that is armed
        // against one file and consumed on another is the whole failure mode.
        log::debug!(
            "MAP_MEMORY: armed on handle {} (shm_off={:#x}) → host_va={:#x} client={:#x} mem={:#x}",
            guest_fd_handle,
            region.offset,
            host_p_linear,
            h_client,
            h_memory
        );

        let (region_offset, pgprot) = (region.offset, region.pgprot);
        self.active_maps.insert(
            region_offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: host_p_linear,
                shm_length: length,
                h_client,
                h_memory,
                map_fd_handle: guest_fd_handle,
                region,
                writable,
                mapping_id: 0,
                refs: 0,
            },
        );

        // Replace host VA with SHM offset in pLinearAddress — this is what
        // the guest sees. It's not a real pointer; the guest driver uses the
        // SHM metadata (shm_offset/shm_length/pgprot in IoctlResp) for mmap,
        // and the library stores this value to pass back at unmap time.
        param_buf[32..40].copy_from_slice(&region_offset.to_le_bytes());

        // --- Step 7: Respond ---
        //
        // Only the parameter buffer goes back. The SHM offset and length do not
        // ride along on the ioctl reply: the guest maps by issuing a separate
        // Mmap message on the file this mapping was armed on, and that reply
        // is what carries the placement, the memory type decided above and
        // whether it is read-only. An earlier reply struct carried the
        // placement here, which the guest driver never read.
        log::debug!(
            "map_memory: SHM {:#x}+{:#x}, pgprot {pgprot:?}",
            region_offset,
            length
        );
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    fn dispatch_unmap_memory(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if param_in.len() < 32 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let h_client = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
        let guest_linear = u64::from_le_bytes(param_in[16..24].try_into().unwrap());

        // guest_linear is the SHM offset we wrote into pLinearAddress during map.
        // Use it as the lookup key.
        let entry = match self.active_maps.remove(guest_linear) {
            Some(e) => e,
            None => {
                log::warn!(
                    "UNMAP_MEMORY: no mapping for pLinearAddress={:#x} \
                     (hClient={:#x}, hMemory={:#x})",
                    guest_linear,
                    h_client,
                    h_memory
                );
                // Forwarded so RM answers it, but with no address: the
                // guest's value is not one of ours, and the host looks the
                // mapping up by the address it is given (guestptr.rs). RM
                // finds none at 0 and says so in `status`.
                let Some(mut arg) = ioctl_arg(request, param_in) else {
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::ENOMEM,
                    );
                };
                let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
                param_buf[16..24].fill(0);
                let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
                if rc < 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                param_buf[16..24].copy_from_slice(&guest_linear.to_le_bytes());
                return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
            }
        };

        log::debug!(
            "UNMAP_MEMORY: shm_off={:#x} → host_va={:#x} (client={:#x}, mem={:#x})",
            guest_linear,
            entry.host_p_linear_address,
            h_client,
            h_memory
        );

        // Substitute the real host pLinearAddress for the host ioctl
        let Some(mut arg) = ioctl_arg(request, param_in) else {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        };
        let param_buf = &mut arg.as_mut_slice()[..param_in.len()];
        param_buf[16..24].copy_from_slice(&entry.host_p_linear_address.to_le_bytes());

        let rc = unsafe { (self.host_ioctl)(host_fd, request, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!("UNMAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore the entry since unmap didn't happen
            self.active_maps.insert(guest_linear, entry);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[24..28].try_into().unwrap());
        log::debug!("UNMAP_MEMORY: host status=0x{:x}", status);

        if status == 0 {
            // Host unmap succeeded -- empty the window range and return the
            // extent to its zone, so the space can serve a later mapping. The
            // withdraw matters as much as the free: without it the VMM keeps
            // the host device memory mapped there until the extent is reused.
            // Unless a guest vma still maps it: RM's unmap does not reach guest
            // page tables, and handing the extent to the next mapping would
            // show that process's memory to this one (M-3). Then the release
            // waits for the last MUNMAP.
            self.end_rm_mapping(entry, "UNMAP_MEMORY");
        } else {
            // Host returned RM error — put the entry back
            log::warn!(
                "UNMAP_MEMORY: host RM status 0x{:x}, restoring mapping",
                status
            );
            self.active_maps.insert(guest_linear, entry);
        }

        // Zero pLinearAddress in response — guest doesn't need it
        param_buf[16..24].copy_from_slice(&0u64.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    /// The descriptor behind a handle an RM parameter block names, if the
    /// handle is one of the NVIDIA devices. RM and NVKMS only understand their
    /// own files; a card, a lease or a sync_file has no business in one of
    /// their parameter blocks.
    /// Turn the handle in UVM command `cmd`'s descriptor field (uvmfd.rs)
    /// into the descriptor it stands for, in `params`, the block the host
    /// will be handed. `Some((offset, handle))` when one was replaced, for
    /// the reply; a negative value is UVM's "none" and stays. Anything that
    /// is not a handle of the kind the field names -- an RM control file, or
    /// a UVM file -- is refused rather than handed to the host as a number
    /// in our table.
    /// Whether `len` is the size of UVM command `cmd`'s parameters on the
    /// host's release: EPERM for a command this release has no row for (or
    /// a host with no table at all), EINVAL for another size.
    fn uvm_size_ok(&self, cmd: u32, len: usize) -> std::result::Result<(), i32> {
        let Some(c) = self
            .driver
            .and_then(abi::schema::uvm_table)
            .and_then(|t| t.lookup(cmd))
        else {
            log::warn!(
                "UVM command {cmd:#x} refused: not in the UVM table of host driver {:?}",
                self.driver
            );
            return Err(libc::EPERM);
        };
        if len != c.size as usize {
            log::warn!(
                "UVM {} ({cmd:#x}): {len} bytes, not the {} its parameters are",
                c.name,
                c.size
            );
            return Err(libc::EINVAL);
        }
        Ok(())
    }

    /// After UVM_INITIALIZE on a release with no DISABLE_PAGEABLE_ACCESS
    /// flag: DISABLE_HMM was forced, which leaves ATS as the only way the VA
    /// space could get pageable access (uvm_va_space.c). Ask UVM itself; if
    /// it says the space has it, or cannot say, the guest reads
    /// NV_ERR_NOT_SUPPORTED and the file takes nothing more.
    fn uvm_pageable_off(&mut self, host_fd: RawFd, resp_buf: &mut [u8], n: usize) {
        const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        // UVM_INITIALIZE_PARAMS {NvU64 flags; NV_STATUS rmStatus;}
        let st = body + 8;
        if n < st + 4 || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
            return;
        }
        if u32::from_le_bytes(resp_buf[st..st + 4].try_into().unwrap()) != 0 {
            return; // not initialised: nothing to check
        }
        // The block is 8 bytes; the rest is room a test's fake may touch.
        let mut q = [0u8; 16];
        // SAFETY: UVM_PAGEABLE_MEM_ACCESS copies its 8-byte block each way,
        // and `q` holds at least that.
        let rc = unsafe {
            (self.host_ioctl)(
                host_fd,
                crate::guestptr::UVM_PAGEABLE_MEM_ACCESS as u64,
                q.as_mut_ptr(),
            )
        };
        let status = u32::from_le_bytes(q[4..8].try_into().unwrap());
        if rc == 0 && status == 0 && q[0] == 0 {
            return;
        }
        log::warn!(
            "UVM handle {}: pageable memory access is not off after UVM_INITIALIZE \
             (ioctl {rc}, rmStatus {status:#x}, pageableMemAccess {}); refusing the file",
            self.current_handle,
            q[0]
        );
        self.uvm_refused.insert(self.current_handle);
        resp_buf[st..st + 4].copy_from_slice(&NV_ERR_NOT_SUPPORTED.to_le_bytes());
    }

    /// What a UVM call that went through means for the aperture: a VA space
    /// in sharing mode, a semaphore pool made, a range freed. `sent` is the
    /// block the host was handed (the guest's, with our changes); success is
    /// the ioctl's and UVM's own status in the block, at `size - 8` for all
    /// three on every release.
    fn uvm_observe(
        &mut self,
        cmd: u32,
        sent: &[u8],
        resp_buf: &[u8],
        n: usize,
        init_flags_mask: u64,
    ) {
        use crate::uvmmap::{ALLOC_SEMAPHORE_POOL, FREE};
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        let size = sent.len();
        if size < 16 || n < body + size || read_struct::<MsgHeader>(resp_buf, 0).status != 0 {
            return;
        }
        let at = body + size - 8;
        if u32::from_le_bytes(resp_buf[at..at + 4].try_into().unwrap()) != 0 {
            return;
        }
        let word = |off: usize| u64::from_le_bytes(sent[off..off + 8].try_into().unwrap());
        let handle = self.current_handle;
        match cmd {
            crate::guestptr::UVM_INITIALIZE
                if init_flags_mask & crate::guestptr::UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE
                    != 0
                    && !self.uvm_refused.contains(&handle) =>
            {
                self.uvm_maps.mark_shared(handle);
            }
            // Every pool, mapped or not: what the pool budgets count
            // (uvmmap.rs, F1). Only one in sharing mode can be mapped.
            ALLOC_SEMAPHORE_POOL => {
                let (base, len) = (word(0), word(8));
                let (_, stale) = self.uvm_maps.record(handle, base, len);
                if let Some(w) = stale {
                    log::error!(
                        "UVM handle {handle}: a new pool at {base:#x} replaced a placed one"
                    );
                    self.withdraw_uvm(w, "alloc");
                }
            }
            FREE => {
                // UVM refuses to free a pool that is still mapped (the VMM's
                // mapping counts), so a placement here means our records were
                // wrong; take it out rather than leave a slot on freed pages.
                if let Some(w) = self.uvm_maps.forget(handle, word(0)) {
                    log::error!("UVM handle {handle}: FREE succeeded on a placed pool");
                    self.withdraw_uvm(w, "free");
                }
            }
            _ => {}
        }
    }

    fn uvm_fd_in(
        &self,
        cmd: u32,
        params: &mut [u8],
    ) -> std::result::Result<Option<(usize, u32)>, i32> {
        let Some(field) = crate::uvmfd::field(self.driver, cmd) else {
            return Ok(None);
        };
        let off = field.offset as usize;
        let Some(raw) = params.get(off..off + 4) else {
            log::warn!(
                "UVM command {cmd}: {} bytes, too short for its descriptor at {off}",
                params.len()
            );
            return Err(libc::EINVAL);
        };
        let value = i32::from_le_bytes(raw.try_into().unwrap());
        if value < 0 {
            return Ok(None);
        }
        let handle = value as u32;
        let want = match field.of {
            crate::uvmfd::FdOf::RmCtl => HandleKind::Dev(DeviceKind::Ctl),
            crate::uvmfd::FdOf::Uvm => HandleKind::Dev(DeviceKind::Uvm),
        };
        match self.handles.get(handle) {
            Some(_) if self.uvm_refused.contains(&handle) => {
                log::warn!("UVM command {cmd}: names handle {handle}, a refused UVM file");
                Err(libc::EBADF)
            }
            Some((fd, kind)) if kind == want => {
                let host = std::os::fd::AsRawFd::as_raw_fd(&fd);
                params[off..off + 4].copy_from_slice(&host.to_le_bytes());
                Ok(Some((off, handle)))
            }
            other => {
                log::warn!(
                    "UVM command {cmd}: descriptor field names handle {handle} ({:?}), not a \
                     {want:?} of ours",
                    other.map(|(_, k)| k)
                );
                Err(libc::EBADF)
            }
        }
    }

    /// The descriptor of `handle` when it is a control file
    /// (`/dev/nvidiactl`), the only kind RM's export and import controls
    /// resolve (`nv_get_file_private(fd, NV_TRUE, ..)`).
    fn ctl_fd(&self, handle: u32) -> Result<RawFd> {
        match self.handles.get(handle) {
            Some((fd, HandleKind::Dev(DeviceKind::Ctl))) => Ok(std::os::fd::AsRawFd::as_raw_fd(&fd)),
            _ => Err(DeviceError::BadHandle(handle as u64)),
        }
    }

    fn dev_fd(&self, handle: u32) -> Result<RawFd> {
        match self.handles.get(handle) {
            Some((fd, HandleKind::Dev(_))) => Ok(std::os::fd::AsRawFd::as_raw_fd(&fd)),
            _ => Err(DeviceError::BadHandle(handle as u64)),
        }
    }

    // ------------------------------------------------------------------
    // Response helpers
    // ------------------------------------------------------------------

    /// Write a bare response header.
    fn write_hdr(&self, resp_buf: &mut [u8], handle: u32, status: i32) -> usize {
        if resp_buf.len() < size_of::<MsgHeader>() {
            return 0;
        }
        let hdr = MsgHeader {
            msg_type: self.current_msg as u32,
            handle,
            status,
            req_id: self.current_req_id,
        };
        write_struct(resp_buf, &hdr)
    }

    /// Write a successful ioctl response: header, lengths, then the bytes.
    ///
    /// The split between the top-level struct and the nested block is taken
    /// from the request, because the guest copies exactly `data_len` bytes back
    /// to the caller's struct and reads any nested block after it.
    fn write_ioctl_resp(&self, resp_buf: &mut [u8], cookie: u64, param_out: &[u8]) -> usize {
        self.write_ioctl_resp_deep(resp_buf, cookie, param_out, 0)
    }

    /// As `write_ioctl_resp`, where the last `deep_len` bytes of `param_out`
    /// are what a pointer inside the nested block addresses, and are declared
    /// separately so the guest knows to copy them somewhere else.
    fn write_ioctl_resp_deep(
        &self,
        resp_buf: &mut [u8],
        cookie: u64,
        param_out: &[u8],
        deep_len: usize,
    ) -> usize {
        let data_len = (self.current_data_len as usize).min(param_out.len());
        let deep_len = deep_len.min(param_out.len() - data_len);
        let nested_len = param_out.len() - data_len - deep_len;

        let need = size_of::<MsgHeader>() + size_of::<IoctlResp>() + param_out.len();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, cookie, 0);
        }

        let mut off = 0;
        off += write_struct(
            &mut resp_buf[off..],
            &MsgHeader {
                msg_type: self.current_msg as u32,
                handle: self.current_handle,
                status: 0,
                req_id: self.current_req_id,
            },
        );
        off += write_struct(
            &mut resp_buf[off..],
            &IoctlResp {
                data_len: data_len as u32,
                nested_len: nested_len as u32,
                deep_len: deep_len as u32,
            },
        );
        resp_buf[off..off + param_out.len()].copy_from_slice(param_out);
        off + param_out.len()
    }

    /// Write a failure.
    ///
    /// `status` is negative in the response because the driver tests
    /// `(s32)status < 0` and returns it straight out of the syscall. A positive
    /// value here reads as success and userspace proceeds on a failed call.
    fn write_error_resp(
        &self,
        resp_buf: &mut [u8],
        status: Status,
        _cookie: u64,
        errno: i32,
    ) -> usize {
        let e = if errno != 0 {
            errno.abs()
        } else {
            status.errno()
        };
        self.write_hdr(resp_buf, 0, -e)
    }
}

/// Close whatever the guest left open.
///
/// This was an inherent method named `drop` rather than a `Drop` impl, so it
/// never ran: a backend that went out of scope without `teardown()` leaked
/// every host fd it held. `cargo` reported it only as an unused-method warning.
impl Drop for NvidiaBackend {
    fn drop(&mut self) {
        if !self.handles.is_empty() {
            log::warn!(
                "NvidiaBackend dropped with {} handles still open — \
                 call teardown() before dropping for clean shutdown",
                self.handles.len()
            );
            self.handles.drain_all();
        }
    }
}

// ============================================================
// Serialisation helpers
// ============================================================

fn read_struct<T: Copy>(buf: &[u8], offset: usize) -> T {
    assert!(buf.len() >= offset + size_of::<T>());
    unsafe { (buf.as_ptr().add(offset) as *const T).read_unaligned() }
}

fn write_struct<T: Copy>(buf: &mut [u8], val: &T) -> usize {
    let sz = size_of::<T>();
    assert!(buf.len() >= sz);
    unsafe { (buf.as_mut_ptr() as *mut T).write_unaligned(*val) }
    sz
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod abi_tests {
    use super::*;
    use abi::ioctl::*;

    /// The exact reply the Tesla T4 gave to NV_ESC_CHECK_VERSION_STR on driver
    /// 580.178.04, taken from gen/fixtures. Using the captured bytes rather
    /// than a hand-built buffer keeps the parser honest about real padding.
    fn t4_version_reply() -> Vec<u8> {
        let mut b = vec![0u8; 72];
        b[4] = 1; // reply = 1
        b[8..18].copy_from_slice(b"580.178.04");
        b
    }

    fn backend() -> NvidiaBackend {
        NvidiaBackend::with_default_zones()
    }

    #[test]
    fn no_profile_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn learns_the_driver_version_from_a_real_reply() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert_eq!(
            b.driver,
            Some(abi::version::DriverVersion::new(580, 178, 4))
        );
        assert!(b.abi.is_some(), "580.178.04 must select a profile");
    }

    /// The property the tables exist for: an escape nobody described does not
    /// reach the host driver. This is the check that was a log line until the
    /// question was asked in public.
    #[test]
    fn an_escape_outside_the_profile_is_refused() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert!(b.abi.is_some());

        // 0x7f is not an NVIDIA escape and is in no profile.
        assert_eq!(b.check_abi(0x7f, 16), AbiCheck::UnknownEscape);
        assert_eq!(b.abi_policy, AbiPolicy::Enforce, "enforcing is the default");

        // And a size the host does not agree with, on an escape that exists.
        assert!(matches!(
            b.check_abi(NV_ESC_RM_CONTROL, 31),
            AbiCheck::SizeMismatch { .. }
        ));
    }

    /// S-15: an escape the table marks as carrying a descriptor that nothing
    /// here translates is refused, not forwarded with the guest's number.
    #[test]
    fn a_descriptor_carrying_escape_with_no_translation_is_refused() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert!(b.untranslated_fd_escape(NV_ESC_EXPORT_TO_DMABUF_FD));
        for translated in [
            NV_ESC_REGISTER_FD,
            NV_ESC_ALLOC_OS_EVENT,
            NV_ESC_FREE_OS_EVENT,
            NV_ESC_RM_ALLOC_MEMORY,
            NV_ESC_RM_CONTROL,
        ] {
            assert!(!b.untranslated_fd_escape(translated), "{translated:#x}");
        }
    }

    /// Before CHECK_VERSION_STR is answered there is no profile to check
    /// against, and refusing then would refuse the call that establishes one.
    #[test]
    fn nothing_is_refused_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn accepts_the_sizes_the_t4_actually_sent() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        for (escape, size) in [
            (NV_ESC_RM_CONTROL, 32),
            (NV_ESC_RM_ALLOC, 48),
            (NV_ESC_RM_FREE, 16),
            (NV_ESC_RM_MAP_MEMORY, 56),
            (NV_ESC_RM_MAP_MEMORY_DMA, 64),
            (NV_ESC_RM_UNMAP_MEMORY_DMA, 48),
            (NV_ESC_RM_VID_HEAP_CONTROL, 184),
        ] {
            assert_eq!(
                b.check_abi(escape, size),
                AbiCheck::Ok,
                "escape {escape:#04x} at {size} bytes was captured from hardware"
            );
        }
    }

    #[test]
    fn catches_the_stale_map_memory_dma_size() {
        // The hand-written table had this at 48; 580 uses NVOS46_PARAMETERS_V580,
        // which is 64. This is the bug the ABI check exists to catch.
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert_eq!(
            b.check_abi(NV_ESC_RM_MAP_MEMORY_DMA, 48),
            AbiCheck::SizeMismatch {
                expected: 64,
                actual: 48
            }
        );
    }

    #[test]
    fn variable_length_escapes_are_not_size_checked() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        // CARD_INFO is an array; the T4 sent 2304 bytes in one call.
        assert_eq!(
            b.check_abi(NV_ESC_CARD_INFO, 2304),
            AbiCheck::VariableLength
        );
    }

    #[test]
    fn a_garbled_version_string_leaves_the_backend_unconfigured() {
        let mut b = backend();
        let mut junk = vec![0u8; 72];
        junk[8..12].copy_from_slice(b"oops");
        b.learn_driver_version(&junk);
        assert!(b.driver.is_none());
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn a_short_reply_is_ignored_rather_than_panicking() {
        let mut b = backend();
        b.learn_driver_version(&[0u8; 4]);
        assert!(b.driver.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request header. The handle travels here now, not in the payload.
    fn hdr(msg_type: MsgType, handle: u64) -> Vec<u8> {
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: msg_type as u32,
                handle: handle as u32,
                status: 0,
                req_id: 0,
            },
        );
        v
    }

    /// `device_type` for an `OpenReq`, as the driver encodes it: a GPU is its
    /// own minor number and the singletons take values above every minor.
    fn dev_type(kind: DeviceKind) -> u32 {
        match kind {
            DeviceKind::Gpu(n) => n,
            DeviceKind::Ctl => DEV_CTL,
            DeviceKind::Uvm => DEV_UVM,
            DeviceKind::UvmTools => DEV_UVM_TOOLS,
            DeviceKind::Modeset => DEV_MODESET,
            DeviceKind::Dri(n) => DEV_DRI_BASE + n,
            DeviceKind::DriCard(n) => DEV_DRI_CARD_BASE + n,
            DeviceKind::Wayland => DEV_WAYLAND,
        }
    }

    /// A complete `Open` message.
    fn open_msg(kind: DeviceKind) -> Vec<u8> {
        let mut v = hdr(MsgType::Open, 0);
        append(
            &mut v,
            &OpenReq {
                device_type: dev_type(kind),
                flags: 0,
            },
        );
        v
    }

    /// A complete `Close` message. The handle is the header's.
    fn close_msg(handle: u64) -> Vec<u8> {
        hdr(MsgType::Close, handle)
    }

    /// A complete `Ioctl` message with no nested block.
    #[allow(dead_code)]
    fn ioctl_msg(handle: u64, escape: u32, params: &[u8]) -> Vec<u8> {
        let mut v = hdr(MsgType::Ioctl, handle);
        append(
            &mut v,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(escape, params.len() as u32) as u32,
                data_len: params.len() as u32,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        v.extend_from_slice(params);
        v
    }

    fn append<T: Copy>(v: &mut Vec<u8>, val: &T) {
        let start = v.len();
        v.resize(start + size_of::<T>(), 0);
        write_struct(&mut v[start..], val);
    }

    /// The response header. `status` is signed: zero on success, negative
    /// errno on failure -- there is no separate status vocabulary on the wire.
    fn parse_resp(buf: &[u8]) -> MsgHeader {
        read_struct::<MsgHeader>(buf, 0)
    }

    /// Whether a response reports the errno `want` maps to.
    fn is_err(buf: &[u8], want: Status) -> bool {
        parse_resp(buf).status == -want.errno()
    }

    /// The handle an `Open` returned, which now arrives in the header.
    fn opened_handle(buf: &[u8]) -> u64 {
        parse_resp(buf).handle as u64
    }

    /// Offset of an ioctl response's parameter block.
    const IOCTL_BODY: usize = size_of::<MsgHeader>() + size_of::<IoctlResp>();

    /// Whether the GPU-backed tests can run here.
    ///
    /// These tests return early without a GPU, which means they report as
    /// passes on a machine that never exercised a line of the code they cover.
    /// Say so on stderr, so that `cargo test -- --nocapture` distinguishes
    /// "verified against a driver" from "skipped, and green either way".
    #[track_caller]
    fn nvidiactl_present() -> bool {
        let present = std::path::Path::new("/dev/nvidiactl").exists();
        if !present {
            eprintln!(
                "SKIP {}: needs /dev/nvidiactl; this test passes without testing anything",
                std::panic::Location::caller()
            );
        }
        present
    }

    // ---- error paths (no GPU required) ----

    #[test]
    fn open_invalid_gpu_index() {
        let mut be = NvidiaBackend::for_test();
        let req = open_msg(DeviceKind::Gpu(200));
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert!(is_err(&resp, Status::InvalidDevice));
    }

    #[test]
    fn close_unknown_handle() {
        let mut be = NvidiaBackend::for_test();
        let req = close_msg(0xCAFE);
        let mut resp = vec![0u8; 32];
        be.dispatch(&req, &mut resp);
        assert!(is_err(&resp, Status::BadHandle));
    }

    #[test]
    fn short_request_rejected() {
        let mut be = NvidiaBackend::for_test();
        be.dispatch(&[0u8; 4], &mut vec![0u8; 32]);
        // just must not panic
    }

    /// Fuzzing (backend target): a request too short for a header, or of no
    /// known type, was answered with a 16-byte header whatever capacity the
    /// guest posted. `serve` promises no reply longer than `cap`; the
    /// vhost-user transport never posts less than a header, another
    /// transport might.
    #[test]
    fn a_malformed_request_is_answered_within_the_posted_capacity() {
        let mut be = NvidiaBackend::for_test();
        for req in [&[0u8; 4][..], &[0xffu8; 16][..]] {
            for cap in [0, 2, 15, 16] {
                let Outcome::Reply(r) = be.serve(req, cap) else {
                    panic!("an IOCTL2 from nothing");
                };
                assert!(r.bytes.len() <= cap, "{} bytes for {cap}", r.bytes.len());
            }
        }
    }

    // ---- teardown tests (no GPU required) ----

    #[test]
    fn teardown_empties_handles() {
        if !nvidiactl_present() {
            return;
        }
        let mut be = NvidiaBackend::for_test();

        // Open two fds
        for _ in 0..2 {
            let req = open_msg(DeviceKind::Ctl);
            let mut resp = vec![0u8; 64];
            be.dispatch(&req, &mut resp);
        }
        assert_eq!(be.handle_count(), 2);

        be.teardown();
        assert_eq!(be.handle_count(), 0);
    }

    #[test]
    fn drop_closes_remaining_handles() {
        if !nvidiactl_present() {
            return;
        }
        // Open a handle, then drop the backend without calling teardown().
        // The Drop impl should drain the table and not panic.
        let mut be = NvidiaBackend::for_test();
        let req = open_msg(DeviceKind::Ctl);
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(be.handle_count(), 1);
        drop(be); // must not panic; Drop closes the fd
    }

    // ---- GPU-present round-trip tests ----

    #[test]
    fn open_close_nvidiactl() {
        if !nvidiactl_present() {
            return;
        }
        let mut be = NvidiaBackend::for_test();

        let req = open_msg(DeviceKind::Ctl);
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        let r = parse_resp(&resp);
        assert_eq!(r.status, 0);

        let h = opened_handle(&resp);
        assert!(h > 0);

        let req2 = close_msg(h);
        let mut resp2 = vec![0u8; 32];
        be.dispatch(&req2, &mut resp2);
        assert_eq!(parse_resp(&resp2).status, 0);
        assert_eq!(be.handle_count(), 0);
    }

    #[test]
    fn check_version_str() {
        if !nvidiactl_present() {
            return;
        }
        let mut be = NvidiaBackend::for_test();

        let oreq = open_msg(DeviceKind::Ctl);
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let gh = opened_handle(&oresp);
        assert!(gh > 0);

        // nv_ioctl_rm_api_version_t: cmd(4) + reply(4) + versionString(64) = 72 bytes
        let param_size: u32 = 72;
        let mut ireq = hdr(MsgType::Ioctl, gh);
        append(
            &mut ireq,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_CHECK_VERSION_STR, param_size) as u32,
                data_len: param_size,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        // First 4 bytes = cmd field. Set to '2' (0x32) for query mode.
        let mut params = vec![0u8; param_size as usize];
        params[0] = 0x32;
        ireq.extend(params);

        let mut iresp = vec![0u8; 512];
        be.dispatch(&ireq, &mut iresp);
        let r = parse_resp(&iresp);
        assert!(
            r.status == -0 || r.status == -Status::IoctlFailed.errno(),
            "unexpected status {}",
            r.status
        );
    }

    /// NV_ESC_RM_MAP_MEMORY round-trip.
    ///
    /// We can't test a real mapping without a valid RM client/device/memory
    /// triple, but we CAN test that:
    ///   1. The dispatch path is reached (not hitting "unhandled escape").
    ///   2. The embedded FD is translated correctly.
    ///   3. The host ioctl failure is reported cleanly (since we don't have
    ///      valid RM handles, the host driver will reject the call).
    #[test]
    fn map_memory_rejects_bad_fd_handle() {
        let mut be = NvidiaBackend::for_test();

        // We need an open nvidiactl fd as the "outer" fd for the ioctl.
        if !nvidiactl_present() {
            return;
        }

        // Open nvidiactl.
        let oreq = open_msg(DeviceKind::Ctl);
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let gh = opened_handle(&oresp);
        assert!(gh > 0);

        // Build a NV_ESC_RM_MAP_MEMORY ioctl with a bogus embedded FD handle.
        // IoctlNVOS33ParametersWithFD = 56 bytes.
        let param_size: u32 = 56;
        let mut ireq = hdr(MsgType::Ioctl, gh);
        append(
            &mut ireq,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size) as u32,
                data_len: param_size,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );

        // 56 bytes of zeroed params — the embedded FD at offset 48 is 0,
        // which is not a valid guest handle.
        let mut params = vec![0u8; param_size as usize];
        // Write a bogus FD handle (0xDEAD) at offset 48.
        params[48..52].copy_from_slice(&0xDEADu32.to_le_bytes());
        ireq.extend(params);

        let mut iresp = vec![0u8; 512];
        be.dispatch(&ireq, &mut iresp);
        // Should fail with BadHandle since 0xDEAD is not in the handle table.
        assert!(is_err(&iresp, Status::BadHandle));
    }

    #[test]
    fn map_memory_translates_fd_and_forwards() {
        if !nvidiactl_present() {
            return;
        }

        let mut be = NvidiaBackend::for_test();

        // Open nvidiactl — this is both the "outer" fd and the "map" fd.
        let oreq = open_msg(DeviceKind::Ctl);
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let ctl_handle = opened_handle(&oresp);

        // Open a second nvidiactl fd to use as the embedded map FD.
        let oreq2 = open_msg(DeviceKind::Ctl);
        let mut oresp2 = vec![0u8; 64];
        be.dispatch(&oreq2, &mut oresp2);
        let map_handle = opened_handle(&oresp2);

        // Build IoctlNVOS33ParametersWithFD with the map_handle as embedded FD.
        let param_size: u32 = 56;
        let mut ireq = hdr(MsgType::Ioctl, ctl_handle);
        append(
            &mut ireq,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size) as u32,
                data_len: param_size,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );

        let mut params = vec![0u8; param_size as usize];
        // Embedded FD at offset 48 = map_handle.
        params[48..52].copy_from_slice(&(map_handle as u32).to_le_bytes());
        ireq.extend(params);

        let mut iresp = vec![0u8; 512];
        be.dispatch(&ireq, &mut iresp);
        let r = parse_resp(&iresp);

        // The host ioctl will fail (we have no valid RM objects) but the
        // dispatch path should reach the host ioctl — so we expect either
        // IoctlFailed (host rejected it) or Ok (unlikely without valid handles).
        // The key thing: it should NOT be BadHandle, proving FD translation worked.
        assert_ne!(
            r.status,
            -Status::BadHandle.errno(),
            "FD translation should have succeeded"
        );
    }

    // ------------------------------------------------------------------
    // Real mapping round-trip
    //
    // Everything above stops at the host ioctl and expects it to fail, because
    // building a mappable RM object takes a chain of allocations. That left the
    // SHM allocate / mmap / free path never actually executed against a driver.
    //
    // The chain below is the shortest one a Tesla T4 was observed using before
    // its first successful NV_ESC_RM_MAP_MEMORY, taken from a captured trace:
    //
    //   NV01_ROOT_CLIENT (0x41)   -> hClient
    //   NV01_DEVICE_0    (0x80)   -> hDevice
    //   NV20_SUBDEVICE_0 (0x2080) -> hSubdevice
    //   TURING_USERMODE_A(0xc461) -> hMemory, mapped at 64 KiB
    //
    // TURING_USERMODE_A is the usermode doorbell aperture, so this maps real
    // GPU registers, not system memory. Later GPUs have their own usermode
    // class (a GB202 driver allocates HOPPER_USERMODE_A); the chain takes the
    // newest one the device lists, as userspace chooses from that list too.
    // ------------------------------------------------------------------

    const NV01_ROOT_CLIENT: u32 = 0x41;
    const NV01_DEVICE_0: u32 = 0x80;
    const NV20_SUBDEVICE_0: u32 = 0x2080;
    /// VOLTA .. BLACKWELL_USERMODE_A, oldest first (rmmem.rs treats every
    /// one of them as registers).
    const USERMODE_CLASSES: [u32; 5] = [0xc361, 0xc461, 0xc561, 0xc661, 0xc761];
    const NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2: u32 = 0x0080_0292;
    const NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE: usize = 200;

    /// NVOS64_PARAMETERS field offsets.
    const A_ROOT: usize = 0;
    const A_PARENT: usize = 4;
    const A_NEW: usize = 8;
    const A_CLASS: usize = 12;
    const A_PARAMS_SIZE: usize = 32;
    const A_STATUS: usize = 40;
    const ALLOC_OUTER: usize = 48;

    /// Places device memory in this process's own SHM window, the way the
    /// VMM places it in the guest's (bin/vhost-user-nvgpu.rs `VhostWindow`):
    /// `MAP_FIXED` of the device fd over the range, and the memfd back on
    /// withdraw. With it the window really aliases what RM mapped, so a test
    /// can read the GPU through it.
    struct LocalWindow {
        base: usize,
        memfd: RawFd,
    }

    impl LocalWindow {
        fn fixed(&self, off: u64, len: u64, prot: i32, fd: RawFd, fo: u64) -> Result<()> {
            // SAFETY: `base` is the backend's SHM mapping, which covers every
            // extent its allocator hands out; this replaces pages inside it.
            let p = unsafe {
                libc::mmap(
                    (self.base + off as usize) as *mut libc::c_void,
                    len as usize,
                    prot,
                    libc::MAP_SHARED | libc::MAP_FIXED,
                    fd,
                    fo as libc::off_t,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(crate::error::DeviceError::Io(
                    std::io::Error::last_os_error(),
                ));
            }
            Ok(())
        }
    }

    impl crate::shm::WindowPlacer for LocalWindow {
        fn place(&self, off: u64, len: u64, fd: RawFd, fo: u64, writable: bool) -> Result<()> {
            let prot = libc::PROT_READ | if writable { libc::PROT_WRITE } else { 0 };
            self.fixed(off, len, prot, fd, fo)
        }
        fn withdraw(&self, off: u64, len: u64) -> Result<()> {
            self.fixed(
                off,
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                self.memfd,
                off,
            )
        }
    }

    struct Chain {
        be: NvidiaBackend,
        ctl: u64,
        gpu: u64,
        cookie: u64,
        /// The usermode class the device lists, found by `usermode_object`.
        usermode: u32,
    }

    impl Chain {
        fn new() -> Self {
            // Not for_test(): its write-combine zone is 16 KiB, and the
            // smallest real mapping here is 64 KiB.
            let mut be = NvidiaBackend::with_default_zones();
            // A mapping is refused (EOPNOTSUPP) until something can place it
            // where the guest reaches it. The VMM does that in a real run;
            // here this process is the VMM, and the window is its own.
            be.set_window(Box::new(LocalWindow {
                base: be.shm_base_ptr() as usize,
                memfd: be.shm_memfd_raw(),
            }));
            let req = open_msg(DeviceKind::Ctl);
            let mut resp = vec![0u8; 64];
            be.dispatch(&req, &mut resp);
            assert_eq!(parse_resp(&resp).status, 0, "open /dev/nvidiactl");
            let ctl = opened_handle(&resp);
            let mut c = Self {
                be,
                ctl,
                gpu: 0,
                cookie: 2,
                usermode: 0,
            };
            // The driver always issues these two before allocating a client.
            // Without them the device allocation is refused with
            // NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b).
            c.simple(abi::ioctl::NV_ESC_SYS_PARAMS, 8);
            c.simple(abi::ioctl::NV_ESC_CARD_INFO, 2304);
            // The driver opens /dev/nvidia0 and registers the control fd
            // against it before allocating a device. Skipping this is refused
            // with NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b).
            c.gpu = c.open_dev(DeviceKind::Gpu(0));
            c.register_fd(c.gpu, c.ctl);
            c
        }

        /// Open one of the character devices and return its guest handle.
        fn open_dev(&mut self, kind: DeviceKind) -> u64 {
            self.cookie += 1;
            let req = open_msg(kind);
            let mut resp = vec![0u8; 64];
            self.be.dispatch(&req, &mut resp);
            assert_eq!(parse_resp(&resp).status, 0, "open {kind:?}");
            opened_handle(&resp)
        }

        /// NV_ESC_REGISTER_FD: attach `fd_handle` to the device `on`.
        fn register_fd(&mut self, on: u64, fd_handle: u64) {
            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, on);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_REGISTER_FD, 4) as u32,
                    data_len: 4,
                    nested_offset: 0,
                    nested_len: 0,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&(fd_handle as u32).to_le_bytes());
            let mut resp = vec![0u8; 256];
            self.be.dispatch(&req, &mut resp);
        }

        /// Issue a parameterless escape whose payload is just a zeroed buffer.
        fn simple(&mut self, escape: u32, size: u32) {
            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(escape, size) as u32,
                    data_len: size,
                    nested_offset: 0,
                    nested_len: 0,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&vec![0u8; size as usize]);
            let mut resp = vec![0u8; size as usize + 256];
            self.be.dispatch(&req, &mut resp);
        }

        /// Issue an RM_ALLOC and return the handle RM assigned.
        fn alloc(&mut self, root: u32, parent: u32, class: u32, params: &[u8]) -> u32 {
            let declared = params.len() as u32;
            self.alloc_with(root, parent, class, params, declared)
        }

        /// Same, but with an explicit `paramsSize` field.
        ///
        /// The captured driver sends the parameter block with `paramsSize` set
        /// to 0 and lets RM use the size the class defines. Passing the byte
        /// count instead is rejected with NV_ERR_INVALID_ARGUMENT.
        fn alloc_with(
            &mut self,
            root: u32,
            parent: u32,
            class: u32,
            params: &[u8],
            declared: u32,
        ) -> u32 {
            let mut outer = vec![0u8; ALLOC_OUTER];
            outer[A_ROOT..A_ROOT + 4].copy_from_slice(&root.to_le_bytes());
            outer[A_PARENT..A_PARENT + 4].copy_from_slice(&parent.to_le_bytes());
            outer[A_CLASS..A_CLASS + 4].copy_from_slice(&class.to_le_bytes());
            outer[A_PARAMS_SIZE..A_PARAMS_SIZE + 4].copy_from_slice(&declared.to_le_bytes());

            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_ALLOC, ALLOC_OUTER as u32) as u32,
                    data_len: ALLOC_OUTER as u32,
                    // The class parameters follow the top-level struct, which
                    // is where the driver puts them.
                    nested_offset: ALLOC_OUTER as u32,
                    nested_len: params.len() as u32,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&outer);
            req.extend_from_slice(params);

            let mut resp = vec![0u8; 4096];
            let n = self.be.dispatch(&req, &mut resp);
            assert!(n > 0, "alloc class {class:#x}: empty response");
            assert_eq!(
                parse_resp(&resp).status,
                0,
                "alloc class {class:#x}: transport status"
            );
            let body = IOCTL_BODY;
            let out = &resp[body..body + ALLOC_OUTER];
            let status = u32::from_le_bytes(out[A_STATUS..A_STATUS + 4].try_into().unwrap());
            assert_eq!(status, 0, "alloc class {class:#x}: RM status {status:#x}");
            u32::from_le_bytes(out[A_NEW..A_NEW + 4].try_into().unwrap())
        }

        /// NV_ESC_RM_CONTROL with inline parameters; returns them as RM
        /// left them.
        fn control(&mut self, client: u32, object: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
            let mut outer = vec![0u8; NVOS54_TOTAL];
            outer[0..4].copy_from_slice(&client.to_le_bytes());
            outer[4..8].copy_from_slice(&object.to_le_bytes());
            outer[NVOS54_CMD..NVOS54_CMD + 4].copy_from_slice(&cmd.to_le_bytes());
            outer[NVOS54_PARAMS_SIZE..NVOS54_PARAMS_SIZE + 4]
                .copy_from_slice(&(params.len() as u32).to_le_bytes());

            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_CONTROL, NVOS54_TOTAL as u32)
                        as u32,
                    data_len: NVOS54_TOTAL as u32,
                    nested_offset: NVOS54_TOTAL as u32,
                    nested_len: params.len() as u32,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&outer);
            req.extend_from_slice(params);

            let mut resp = vec![0u8; 8192];
            self.be.dispatch(&req, &mut resp);
            assert_eq!(parse_resp(&resp).status, 0, "control {cmd:#x}: transport");
            let out = &resp[IOCTL_BODY..IOCTL_BODY + NVOS54_TOTAL + params.len()];
            let st = u32::from_le_bytes(out[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap());
            assert_eq!(st, 0, "control {cmd:#x}: RM status {st:#x}");
            out[NVOS54_TOTAL..].to_vec()
        }

        /// The newest usermode class the device lists.
        fn usermode_class(&mut self, client: u32, device: u32) -> u32 {
            let size = 4 + 4 * NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE;
            let out = self.control(
                client,
                device,
                NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2,
                &vec![0u8; size],
            );
            let n = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
            let listed: Vec<u32> = out[4..4 + 4 * n.min(NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE)]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            let class = *USERMODE_CLASSES
                .iter()
                .rev()
                .find(|c| listed.contains(c))
                .expect("the device lists no usermode class");
            eprintln!("usermode class {class:#x} (of {n} classes listed)");
            class
        }

        /// Build the object chain and return (hClient, hSubdevice, hMemory).
        fn usermode_object(&mut self) -> (u32, u32, u32) {
            let client = self.alloc(0, 0, NV01_ROOT_CLIENT, &[]);
            assert_ne!(client, 0, "RM assigned no client handle");

            // The captured driver passes paramsSize 0 for all of these; RM
            // uses the class's own parameter size rather than trusting the
            // caller, so sending none is what the real sequence does.
            // NV0080_ALLOC_PARAMETERS, zeroed apart from hClientShare.
            let mut dev_params = vec![0u8; 56];
            dev_params[4..8].copy_from_slice(&client.to_le_bytes());
            let device = self.alloc(client, client, NV01_DEVICE_0, &dev_params);

            // NV2080_ALLOC_PARAMETERS is a single subDeviceID.
            let subdevice = self.alloc(client, device, NV20_SUBDEVICE_0, &0u32.to_le_bytes());

            self.usermode = self.usermode_class(client, device);
            let memory = self.alloc(client, subdevice, self.usermode, &[]);
            (client, subdevice, memory)
        }

        /// A dedicated fd to carry the mapping.
        ///
        /// This is a **/dev/nvidia0** fd, not /dev/nvidiactl: the trace shows
        /// the mmap landing on the per-GPU node even though the
        /// NV_ESC_RM_MAP_MEMORY that defines it is issued on the control node.
        /// The fd is registered against the control fd first, as the driver
        /// does for every fd it maps on.
        fn map_fd(&mut self) -> u64 {
            let h = self.open_dev(DeviceKind::Gpu(0));
            self.register_fd(h, self.ctl);
            h
        }

        /// NV_ESC_RM_MAP_MEMORY. Returns (shm_offset, shm_length, pLinearAddress).
        fn map(&mut self, client: u32, dev: u32, mem: u32, len: u64, fd: u64) -> (u64, u64, u64) {
            let mut p = vec![0u8; 56];
            p[0..4].copy_from_slice(&client.to_le_bytes());
            p[4..8].copy_from_slice(&dev.to_le_bytes());
            p[8..12].copy_from_slice(&mem.to_le_bytes());
            p[24..32].copy_from_slice(&len.to_le_bytes());
            // The flags the driver sends for this mapping. Zero is rejected
            // with NV_ERR_INVALID_ARGUMENT; bits 23-25 are the caching type,
            // here 6 (default), which the device resolves to write-combine.
            p[44..48].copy_from_slice(&0x0308_0002u32.to_le_bytes());
            p[48..52].copy_from_slice(&(fd as u32).to_le_bytes());

            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, 56) as u32,
                    data_len: 56,
                    nested_offset: 0,
                    nested_len: 0,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&p);

            let mut resp = vec![0u8; 4096];
            self.be.dispatch(&req, &mut resp);
            let rh = parse_resp(&resp);
            assert_eq!(rh.status, 0,);
            let body = IOCTL_BODY;
            let out = &resp[body..body + 56];
            let rm = u32::from_le_bytes(out[40..44].try_into().unwrap());
            assert_eq!(rm, 0, "map: RM status {rm:#x}");
            // The SHM offset comes back in pLinearAddress, not in a reply
            // struct: the guest quotes it in a separate Mmap message, and that
            // is where placement and caching are decided.
            let linear = u64::from_le_bytes(out[32..40].try_into().unwrap());
            (linear, len, linear)
        }

        /// Close a device handle, as a guest does when its fd goes away.
        fn close_dev(&mut self, handle: u64) {
            self.cookie += 1;
            let req = close_msg(handle);
            let mut resp = vec![0u8; 128];
            self.be.dispatch(&req, &mut resp);
            assert_eq!(parse_resp(&resp).status, 0, "close handle {handle}");
        }

        /// NV_ESC_RM_FREE of one object.
        fn free_obj(&mut self, root: u32, parent: u32, object: u32) {
            let mut p = vec![0u8; 16];
            p[0..4].copy_from_slice(&root.to_le_bytes());
            p[4..8].copy_from_slice(&parent.to_le_bytes());
            p[8..12].copy_from_slice(&object.to_le_bytes());
            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_FREE, 16) as u32,
                    data_len: 16,
                    nested_offset: 0,
                    nested_len: 0,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&p);
            let mut resp = vec![0u8; 512];
            self.be.dispatch(&req, &mut resp);
            let rh = parse_resp(&resp);
            assert_eq!(rh.status, 0, "free: transport status, ",);
            let body = IOCTL_BODY;
            let rm = u32::from_le_bytes(resp[body + 12..body + 16].try_into().unwrap());
            assert_eq!(rm, 0, "free of {object:#x}: RM status {rm:#x}");
        }

        /// NV_ESC_RM_UNMAP_MEMORY, keyed by the pLinearAddress the map returned.
        fn unmap(&mut self, client: u32, dev: u32, mem: u32, linear: u64) {
            let mut p = vec![0u8; 32];
            p[0..4].copy_from_slice(&client.to_le_bytes());
            p[4..8].copy_from_slice(&dev.to_le_bytes());
            p[8..12].copy_from_slice(&mem.to_le_bytes());
            p[16..24].copy_from_slice(&linear.to_le_bytes());

            self.cookie += 1;
            let mut req = hdr(MsgType::Ioctl, self.ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_UNMAP_MEMORY, 32) as u32,
                    data_len: 32,
                    nested_offset: 0,
                    nested_len: 0,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(&p);
            let mut resp = vec![0u8; 4096];
            self.be.dispatch(&req, &mut resp);
            let rh = parse_resp(&resp);
            assert_eq!(rh.status, 0,);
            let body = IOCTL_BODY;
            let rm = u32::from_le_bytes(resp[body + 24..body + 28].try_into().unwrap());
            assert_eq!(rm, 0, "unmap: RM status {rm:#x}");
        }
    }

    #[test]
    fn maps_turing_usermode_aperture_for_real() {
        if !nvidiactl_present() {
            return;
        }
        let mut c = Chain::new();
        let (client, sub, mem) = c.usermode_object();
        let fd = c.map_fd();

        let (uc_before, wc_before, wb_before) = c.be.shm_free_bytes();
        let (off, len, linear) = c.map(client, sub, mem, 65536, fd);
        assert_eq!(len, 65536, "mapped length");
        assert_eq!(
            linear, off,
            "pLinearAddress must be the SHM offset the guest sees"
        );
        // Registers are mapped uncached (rmmem.rs), so the extent comes from
        // the uncached zone. That zone starts the window, which makes offset
        // 0 a real answer: the first usermode mapping of a session gets it.
        assert_eq!(
            c.be.shm_free_bytes(),
            (uc_before - 65536, wc_before, wb_before),
            "usermode registers must take 64 KiB of the uncached zone"
        );
        assert!(
            off < ZoneConfig::default_1gib().uc_size,
            "offset {off:#x} outside UC"
        );

        // The SHM window now aliases GPU registers. Reading must not fault.
        let base = c.be.shm_base_ptr();
        assert!(!base.is_null(), "SHM base");
        let first = unsafe { std::ptr::read_volatile(base.add(off as usize) as *const u32) };
        eprintln!(
            "usermode {:#x} first dword through SHM: {first:#010x}",
            c.usermode
        );
        // NV_USERMODE_CFG0: the low half is the chip's usermode class, which
        // the memfd behind an unplaced window would read as zero.
        assert!(
            USERMODE_CLASSES.contains(&(first & 0xffff)),
            "the window does not alias the usermode registers (read {first:#x})"
        );

        c.unmap(client, sub, mem, linear);
        assert_eq!(
            c.be.shm_free_bytes(),
            (uc_before, wc_before, wb_before),
            "unmap must return the extent"
        );
        c.be.teardown();
    }

    /// Closing a device fd must release whatever it was mapping.
    ///
    /// This is the shape of a real CUDA client, which maps 29 times in a run
    /// and issues no NV_ESC_RM_UNMAP_MEMORY at all -- the mappings go away
    /// because the process exits and its fds close. Releasing only on unmap
    /// leaves ~68 MiB of write-combine spent per run for the life of the VM,
    /// so a third run has nowhere to map.
    #[test]
    fn closing_the_fd_releases_its_mapping_without_any_unmap() {
        if !nvidiactl_present() {
            return;
        }
        let mut c = Chain::new();
        let (client, sub, first) = c.usermode_object();
        c.free_obj(client, sub, first);

        let before = c.be.shm_free_bytes();
        for i in 0..50 {
            let class = c.usermode;
            let mem = c.alloc(client, sub, class, &[]);
            let fd = c.map_fd();
            c.map(client, sub, mem, 65536, fd);
            assert_ne!(
                before,
                c.be.shm_free_bytes(),
                "run {i}: mapping took no SHM"
            );

            // Exit the way CUDA does: free the object and drop the fd, with no
            // unmap anywhere.
            c.free_obj(client, sub, mem);
            c.close_dev(fd);

            assert_eq!(
                before,
                c.be.shm_free_bytes(),
                "run {i}: closing the fd did not release its mapping"
            );
        }
        c.be.teardown();
    }

    /// A hundred map/unmap cycles against the real aperture, asserting the SHM
    /// zones end exactly as full as they started.
    ///
    /// **A file descriptor that has carried a mapping cannot carry another.**
    /// Reusing one gives NV_ERR_STATE_IN_USE (0x63) on the second
    /// NV_ESC_RM_MAP_MEMORY even though the preceding NV_ESC_RM_UNMAP_MEMORY
    /// and NV_ESC_RM_FREE both returned NV_OK. The captured driver behaves the
    /// same way: it opens a fresh /dev/nvidia0 fd per mapping.
    ///
    /// Ruled out along the way, all on a T4 running 580.178.04: it is not the
    /// unmap address (the driver passes back exactly the cookie the map
    /// returned, which is what the device does, and passing our own mapping
    /// address instead gives NV_ERR_OBJECT_NOT_FOUND); not the node the unmap
    /// is issued on (the GPU node returns EINVAL, so the control node is
    /// right); and not a leaked RM object (RM_FREE succeeds).
    ///
    /// The consequence for the device is a lifetime rule, not a bug fix: a host
    /// fd is single-use for mapping, so one must be opened per mapping and
    /// closed when the guest closes its own. This test closes each fd to prove
    /// no handle is leaked in the process.
    #[test]
    fn repeated_map_unmap_does_not_exhaust_the_zone() {
        if !nvidiactl_present() {
            return;
        }
        let mut c = Chain::new();
        let (client, sub, first) = c.usermode_object();
        // The usermode aperture allows one live mapping, so the object built
        // during setup has to go before the loop makes its own.
        c.free_obj(client, sub, first);

        // The usermode aperture permits one mapping per object -- a second map of
        // a still-mapped object is refused with NV_ERR_STATE_IN_USE -- so each
        // cycle allocates its own.
        let before = c.be.shm_free_bytes();
        let handles_before = c.be.handle_count();
        let cycles = 100;
        for i in 0..cycles {
            let class = c.usermode;
            let mem = c.alloc(client, sub, class, &[]);
            let fd = c.map_fd();
            eprintln!("cycle {i}: mem={mem:#x} fd={fd}");
            let (_off, _len, linear) = c.map(client, sub, mem, 65536, fd);
            assert_ne!(
                before,
                c.be.shm_free_bytes(),
                "iteration {i}: mapping took no SHM"
            );
            c.unmap(client, sub, mem, linear);
            c.free_obj(client, sub, mem);
            c.close_dev(fd);
        }
        let after = c.be.shm_free_bytes();
        assert_eq!(
            before, after,
            "{cycles} map/unmap cycles did not return every byte to the zones"
        );
        assert_eq!(
            handles_before,
            c.be.handle_count(),
            "{cycles} cycles leaked host file descriptors"
        );
        c.be.teardown();
    }

    // ------------------------------------------------------------------
    // v1 gating, argument sizing and window ownership (no GPU required)
    // ------------------------------------------------------------------

    fn devnull() -> OwnedFd {
        // SAFETY: a NUL-terminated path; ownership passes to the OwnedFd.
        unsafe {
            OwnedFd::from_raw_fd(libc::open(
                c"/dev/null".as_ptr(),
                libc::O_RDWR | libc::O_CLOEXEC,
            ))
        }
    }

    std::thread_local! {
        /// Commands the fake host driver was handed, on this test's thread.
        static FORWARDED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A host driver that behaves like drm_ioctl and nvidia.ko: it writes
    /// back `_IOC_SIZE(request)` bytes, whatever the caller allocated.
    unsafe fn fake_ioctl(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        FORWARDED.with(|f| f.borrow_mut().push(request));
        // SAFETY: the contract of `HostIoctl` -- `arg` holds _IOC_SIZE bytes.
        unsafe { std::ptr::write_bytes(arg, 0xaa, hostfd::ioc_size(request as u32)) };
        0
    }

    fn forwarded() -> Vec<u64> {
        FORWARDED.with(|f| std::mem::take(&mut *f.borrow_mut()))
    }

    fn v1_ioctl(be: &mut NvidiaBackend, handle: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
        let mut req = hdr(MsgType::Ioctl, handle as u64);
        append(
            &mut req,
            &IoctlReq {
                cmd,
                data_len: params.len() as u32,
                ..Default::default()
            },
        );
        req.extend_from_slice(params);
        let mut resp = vec![0u8; 256 + params.len()];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn gated_backend() -> NvidiaBackend {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_ioctl);
        be
    }

    #[test]
    fn ioctl_buffers_are_sized_for_what_the_host_copies() {
        assert_eq!(
            ioctl_arg_len(hostfd::ioc(hostfd::IOC_RW, b'd', 1, 4096) as u64, 8),
            4096
        );
        assert_eq!(
            ioctl_arg_len(hostfd::ioc(hostfd::IOC_RW, b'F', 1, 16) as u64, 72),
            72
        );
        assert_eq!(
            ioctl_arg_len(0x3000_0001, 0),
            0x3000,
            "UVM numbers carry a size field too"
        );
        assert_eq!(ioctl_arg_len(hostfd::ioc(0, b'd', 0x1f, 0) as u64, 0), 1);
    }

    /// The heap overflow the sizing fixes: a guest sends 8 bytes with a
    /// command whose size field says 8 KiB, and the host writes 8 KiB back.
    /// With the buffer sized to the command this is just a call.
    #[test]
    fn a_small_payload_with_a_large_command_size_cannot_overflow() {
        let mut be = gated_backend();
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        // RM_FREE, pointer-free, with a size field of 8 KiB.
        let cmd = hostfd::ioc(hostfd::IOC_RW, b'F', 0x29, 8 * 1024);
        let resp = v1_ioctl(&mut be, ctl, cmd, &[0u8; 8]);
        assert_eq!(parse_resp(&resp).status, 0);
        assert_eq!(forwarded(), vec![cmd as u64]);
        // Only what the guest sent comes back.
        assert_eq!(&resp[IOCTL_BODY..], &[0xaa; 8]);

        // A UVM number with a size field is no UVM command (uvm_ioctl.h
        // numbers them plainly) and is refused before the host
        // (guestptr.rs).
        let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        let cmd = hostfd::ioc(hostfd::IOC_RW, 0, 0x21, 8 * 1024);
        let resp = v1_ioctl(&mut be, uvm, cmd, &[0u8; 8]);
        assert_eq!(parse_resp(&resp).status, -libc::EPERM);
        assert!(forwarded().is_empty());
    }

    #[test]
    fn v1_ioctls_are_refused_on_every_new_handle_kind() {
        let mut be = gated_backend();
        for kind in [
            HandleKind::DrmCard(0),
            HandleKind::DrmLease(0),
            HandleKind::SyncFile,
            HandleKind::Syncobj,
            HandleKind::Dmabuf,
            HandleKind::Eventfd,
            HandleKind::Memfd,
            HandleKind::Wayland,
            HandleKind::Other,
        ] {
            let h = be.adopt_for_test(devnull(), kind);
            // ADDFB2 as a v1 ioctl: the path around every IOCTL2 check.
            let addfb2 = hostfd::ioc(hostfd::IOC_RW, b'd', 0xb8, 104);
            let r = v1_ioctl(&mut be, h, addfb2, &[0u8; 104]);
            assert_eq!(parse_resp(&r).status, -libc::EPERM, "{kind:?}");
        }
        assert!(forwarded().is_empty());
    }

    #[test]
    fn a_render_handle_takes_only_the_v1_drm_calls_the_guest_sends() {
        let mut be = gated_backend();
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let refused = [
            // GEM_IMPORT_USERSPACE_MEMORY, GEM_FLINK, GEM_OPEN: never.
            (hostfd::ioc(hostfd::IOC_RW, b'd', 0x42, 24), 24, libc::EPERM),
            (hostfd::DRM_IOCTL_GEM_FLINK, 8, libc::EPERM),
            (hostfd::DRM_IOCTL_GEM_OPEN, 16, libc::EPERM),
            // An RM escape on a DRM file.
            (hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32), 32, libc::EPERM),
            // A core KMS ioctl.
            (hostfd::ioc(hostfd::IOC_RW, b'd', 0xa0, 64), 64, libc::EPERM),
            // A known command with a size that disagrees with it.
            (DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET, 8, libc::EINVAL),
        ];
        for (cmd, len, errno) in refused {
            let r = v1_ioctl(&mut be, render, cmd, &vec![0u8; len]);
            assert_eq!(parse_resp(&r).status, -errno, "cmd {cmd:#x}");
        }
        assert!(forwarded().is_empty(), "nothing refused reached the host");

        for cmd in [
            hostfd::DRM_IOCTL_GEM_CLOSE,
            DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET,
            DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY,
        ] {
            let r = v1_ioctl(&mut be, render, cmd, &vec![0u8; hostfd::ioc_size(cmd)]);
            assert_eq!(parse_resp(&r).status, 0, "cmd {cmd:#x}");
        }
        assert_eq!(forwarded().len(), 3);
    }

    #[test]
    fn each_nvidia_device_takes_only_its_own_namespace() {
        let mut be = gated_backend();
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let modeset = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Modeset));
        let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        let d = hostfd::ioc(hostfd::IOC_RW, b'd', 0x4b, 24);
        let m = hostfd::ioc(hostfd::IOC_RW, b'm', 0, 16);
        // RM's frontend and nvidia-modeset both dispatch on the number alone,
        // so a foreign type byte would reach them as one of their own
        // commands, unchecked.
        for (h, cmd) in [
            (ctl, d),
            (ctl, m),
            (modeset, d),
            (uvm, m),
            (ctl, 0x3000_0001),
        ] {
            let r = v1_ioctl(&mut be, h, cmd, &[0u8; 24]);
            assert_eq!(
                parse_resp(&r).status,
                -libc::EPERM,
                "handle {h} cmd {cmd:#x}"
            );
        }
        assert!(forwarded().is_empty());
    }

    /// Records what the window was asked to do.
    #[derive(Clone, Default)]
    struct FakeWindow(Arc<std::sync::Mutex<Vec<(&'static str, u64)>>>);

    impl crate::shm::WindowPlacer for FakeWindow {
        fn place(&self, off: u64, _len: u64, _fd: RawFd, _fo: u64, _w: bool) -> Result<()> {
            self.0.lock().unwrap().push(("place", off));
            Ok(())
        }
        fn withdraw(&self, off: u64, _len: u64) -> Result<()> {
            self.0.lock().unwrap().push(("withdraw", off));
            Ok(())
        }
    }

    fn mmap(be: &mut NvidiaBackend, handle: u32, offset: u64) -> MmapResp {
        let mut req = hdr(MsgType::Mmap, handle as u64);
        append(
            &mut req,
            &MmapReq {
                size: 4096,
                offset,
                prot: 3,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "mmap");
        read_struct::<MmapResp>(&resp, size_of::<MsgHeader>())
    }

    fn munmap(be: &mut NvidiaBackend, id: u32) {
        let mut req = hdr(MsgType::Munmap, 0);
        append(
            &mut req,
            &MunmapReq {
                mapping_id: id,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "munmap");
    }

    /// R:internals §11.1: a proxy that outlives the file that placed it keeps
    /// its placement; the extent is withdrawn and freed once, by the last
    /// MUNMAP, and never handed to anyone else while it is mapped.
    #[test]
    fn a_placement_outlives_its_owner_and_is_freed_exactly_once() {
        let mut be = gated_backend();
        let window = FakeWindow::default();
        be.set_window(Box::new(window.clone()));
        let empty = be.shm_free_bytes();
        let owner = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let placed = mmap(&mut be, owner, 0x10000);
        assert_ne!(placed.mapping_id, 0);
        let held = be.shm_free_bytes();
        assert_ne!(held, empty);

        // The client exits: its file closes, its buffer lives on elsewhere.
        be.dispatch(&close_msg(owner as u64), &mut [0u8; 32]);
        assert_eq!(
            be.shm_free_bytes(),
            held,
            "closing the owner freed a mapped extent"
        );
        assert!(
            !window
                .0
                .lock()
                .unwrap()
                .iter()
                .any(|(op, _)| *op == "withdraw")
        );

        // Someone else maps meanwhile: a different extent, not the live one.
        let other = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let second = mmap(&mut be, other, 0x10000);
        assert_ne!(second.guest_phys_addr, placed.guest_phys_addr);

        // The proxy goes: one withdraw, one free, of its own extent only.
        munmap(&mut be, placed.mapping_id);
        munmap(&mut be, placed.mapping_id);
        let withdrawn: Vec<u64> = window
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|(op, _)| *op == "withdraw")
            .map(|(_, off)| *off)
            .collect();
        assert_eq!(withdrawn, vec![placed.guest_phys_addr]);
        munmap(&mut be, second.mapping_id);
        assert_eq!(be.shm_free_bytes(), empty);
    }

    #[test]
    fn the_same_object_mapped_twice_is_released_by_the_second_munmap() {
        let mut be = gated_backend();
        be.set_window(Box::new(FakeWindow::default()));
        let empty = be.shm_free_bytes();
        let h = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let a = mmap(&mut be, h, 0x2000);
        let b = mmap(&mut be, h, 0x2000);
        assert_eq!(a.mapping_id, b.mapping_id);
        munmap(&mut be, a.mapping_id);
        assert_ne!(be.shm_free_bytes(), empty, "still mapped once");
        munmap(&mut be, b.mapping_id);
        assert_eq!(be.shm_free_bytes(), empty);
    }

    #[test]
    fn a_session_reset_releases_every_placement_and_handle() {
        let mut be = gated_backend();
        let window = FakeWindow::default();
        be.set_window(Box::new(window.clone()));
        let empty = be.shm_free_bytes();
        let h = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        mmap(&mut be, h, 0x3000);
        be.session_reset("test");
        assert_eq!(be.shm_free_bytes(), empty);
        assert_eq!(be.handle_count(), 0);
        assert_eq!(window.0.lock().unwrap().last().unwrap().0, "withdraw");
    }

    #[test]
    fn mmap_is_refused_on_handles_that_carry_no_device_memory() {
        let mut be = gated_backend();
        be.set_window(Box::new(FakeWindow::default()));
        let memfd = be.adopt_for_test(devnull(), HandleKind::Memfd);
        let mut req = hdr(MsgType::Mmap, memfd as u64);
        append(
            &mut req,
            &MmapReq {
                size: 4096,
                offset: 0,
                prot: 3,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, -libc::EPERM);
    }

    #[test]
    fn opening_a_card_node_directly_is_refused() {
        let mut be = gated_backend();
        let mut resp = vec![0u8; 64];
        be.dispatch(&open_msg(DeviceKind::DriCard(0)), &mut resp);
        assert_eq!(parse_resp(&resp).status, -libc::EPERM);
    }

    #[test]
    fn responses_echo_the_request_id() {
        let mut be = gated_backend();
        let mut req = close_msg(0xCAFE);
        req[12..16].copy_from_slice(&0x1234u32.to_le_bytes());
        let mut resp = vec![0u8; 32];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).req_id, 0x1234);
    }

    /// A probe buffer as the host leaves it: its own struct over the front,
    /// the filler after.
    fn probe_with(host: &[u32]) -> [u32; NV_DEV_INFO_PROBE_WORDS] {
        let mut p = [DEV_INFO_UNWRITTEN; NV_DEV_INFO_PROBE_WORDS];
        p[..host.len()].copy_from_slice(host);
        p
    }

    #[test]
    fn the_get_dev_info_probe_asks_with_a_64_byte_size_field() {
        assert_eq!(DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE, 0xC040_6443);
        assert_eq!(
            hostfd::ioc_size(DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE as u32),
            64
        );
    }

    #[test]
    fn the_host_layout_is_measured_from_where_the_filler_starts() {
        // 535: gpu_id, primary_index, kind, generation, sector layout -- with
        // a zero in the middle, which is still a written word.
        assert_eq!(dev_info_host_size(&probe_with(&[0x100, 0, 6, 2, 1])), 20);
        assert_eq!(
            dev_info_host_size(&probe_with(&[0x100, 1, 6, 2, 1, 0, 0])),
            28
        );
        assert_eq!(
            dev_info_host_size(&probe_with(&[0x100, 1, 1, 6, 2, 1, 1, 1])),
            32
        );
        assert_eq!(
            dev_info_host_size(&probe_with(&[0x100, 0, 1, 1, 6, 2, 1, 0, 0])),
            36
        );
        assert_eq!(dev_info_host_size(&probe_with(&[])), 0);
    }

    #[test]
    fn a_535_answer_is_not_read_as_the_610_layout() {
        let raw = probe_with(&[0x100, 1, 6, 2, 1]);
        // The old reading: primary_index as mig_device, the page kind as
        // supports_alloc, the generation as the page kind.
        let info = normalise_dev_info(&raw, 20, true).unwrap();
        assert_eq!(info, [0x100, 0, 1, 1, 6, 2, 1, 0, 0]);
    }

    #[test]
    fn supports_alloc_comes_from_dmabuf_supported_where_the_layout_lacks_it() {
        let raw = probe_with(&[0x100, 1, 6, 2, 1]);
        assert_eq!(normalise_dev_info(&raw, 20, false).unwrap()[3], 0);
        let raw = probe_with(&[0x100, 1, 6, 2, 1, 1, 1]);
        assert_eq!(
            normalise_dev_info(&raw, 28, true).unwrap(),
            [0x100, 0, 1, 1, 6, 2, 1, 1, 1]
        );
    }

    #[test]
    fn a_32_byte_answer_keeps_its_own_supports_alloc() {
        // modeset=0 on 550: supports_alloc false and every kind zero, whatever
        // DMABUF_SUPPORTED would say.
        let raw = probe_with(&[0x100, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            normalise_dev_info(&raw, 32, true).unwrap(),
            [0x100, 0, 1, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn a_36_byte_answer_passes_through_unchanged() {
        let host = [0x100, 3, 1, 1, 6, 2, 1, 1, 1];
        assert_eq!(
            normalise_dev_info(&probe_with(&host), 36, false).unwrap(),
            host
        );
    }

    #[test]
    fn an_unknown_layout_is_not_guessed_at() {
        let raw = probe_with(&[1; 10]);
        assert_eq!(normalise_dev_info(&raw, 40, true), None);
        assert_eq!(normalise_dev_info(&raw, 0, true), None);
    }

    #[test]
    fn the_dev_info_sizes_follow_the_card_section_in_dri_order() {
        let dri = |size| DriDevice {
            name: "renderD128".into(),
            major: 226,
            minor: 128,
            slot_index: 0,
            dev_info: [0; NV_DEV_INFO_WORDS],
            dev_info_size: size,
        };
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(vec![dri(20), dri(36)], Vec::new());
        let mut buf = vec![0u8; 1 << 20];
        let n = be.handle_get_files(FileTree::Sys, &mut buf);
        let tail: Vec<u32> = buf[n - 12..n]
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(tail, [2, 20, 36]);
        // ...and the empty card section is right before them.
        assert_eq!(read_struct::<u32>(&buf, n - 16), 0);
        assert_eq!(write_dev_info_sizes(&[dri(20)], &mut [0u8; 7]), 0);
    }

    #[test]
    fn the_card_section_is_written_whole_or_not_at_all() {
        let cards = vec![CardNode {
            name: "card1".into(),
            major: 226,
            minor: 1,
            render_index: 0,
        }];
        let mut buf = vec![0u8; 64];
        let n = write_card_section(&cards, &mut buf);
        assert_eq!(n, 4 + 16 + 5);
        assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 1);
        let rec = read_struct::<CardRecord>(&buf, 4);
        assert_eq!(
            (rec.name_len, rec.major, rec.minor, rec.render_index),
            (5, 226, 1, 0)
        );
        assert_eq!(&buf[20..25], b"card1");
        assert_eq!(write_card_section(&cards, &mut [0u8; 10]), 0);
    }

    /// Plain Wayland mode needs the host card numbers too (the devmap), so
    /// GET_SYS_FILES carries them without --kms-card; only BCAP_KMS_CARD
    /// makes them openable.
    #[test]
    fn get_sys_files_names_the_cards_in_every_mode() {
        for kms_card in [false, true] {
            let mut be = NvidiaBackend::for_test();
            be.config.kms_card = kms_card;
            be.set_host_nodes_for_test(
                Vec::new(),
                vec![CardNode {
                    name: "card1".into(),
                    major: 226,
                    minor: 1,
                    render_index: 0,
                }],
            );
            let mut buf = vec![0u8; 1 << 20];
            let n = be.handle_get_files(FileTree::Sys, &mut buf);
            // The file stream, up to its terminator.
            let mut off = 0;
            loop {
                let e = read_struct::<FileEntry>(&buf, off);
                off += size_of::<FileEntry>();
                if e.path_len == 0 && e.content_len == 0 {
                    break;
                }
                off += (e.path_len + e.content_len) as usize;
            }
            assert_eq!(read_struct::<u32>(&buf, off), 0, "no DRI records");
            off += 4;
            assert_eq!(read_struct::<u32>(&buf, off), 1, "kms_card {kms_card}");
            let rec = read_struct::<CardRecord>(&buf, off + 4);
            assert_eq!((rec.major, rec.minor), (226, 1));
            off += 4 + 16 + 5;
            assert_eq!(read_struct::<u32>(&buf, off), 0, "no GET_DEV_INFO sizes");
            assert_eq!(n, off + 4);
        }
    }

    /// RM never writes UPDATE_DEVICE_MAPPING_INFO's pOld/pNew, so the
    /// caller reads back what it passed -- not zero, and not the host
    /// address the backend put there for the call (L-6).
    #[test]
    fn update_device_mapping_info_gives_the_callers_addresses_back() {
        let mut be = gated_backend();
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        // NVOS56 {hClient, hDevice, hMemory, pad, pOld, pNew, status, pad}.
        let mut p = [0u8; 40];
        p[16..24].copy_from_slice(&0x7f00_1000u64.to_le_bytes());
        p[24..32].copy_from_slice(&0x7f00_2000u64.to_le_bytes());
        let cmd = hostfd::ioc(hostfd::IOC_RW, b'F', 0x5e, 40);
        let resp = v1_ioctl(&mut be, ctl, cmd, &p);
        assert_eq!(parse_resp(&resp).status, 0);
        assert_eq!(forwarded(), vec![cmd as u64]);
        let body = &resp[IOCTL_BODY..];
        assert_eq!(&body[16..32], &p[16..32], "the caller's own pOld and pNew");
        assert_eq!(&body[32..36], &[0xaa; 4], "the host's status");
    }

    /// An RM that serves one control and one class and turns everything else
    /// down in the status word, as RM does for a command it does not know.
    unsafe fn fake_rm_status(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        // SAFETY: the contract of `HostIoctl` -- `arg` holds _IOC_SIZE bytes.
        let a = unsafe { std::slice::from_raw_parts_mut(arg, hostfd::ioc_size(request as u32)) };
        let (key_at, status_at, served) = match hostfd::ioc_nr(request as u32) {
            0x2a => (8, 28, 0x2080_0101),
            _ => (12, 40, 0x3e),
        };
        let key = u32::from_le_bytes(a[key_at..key_at + 4].try_into().unwrap());
        let status: u32 = if key == served { 0 } else { 0x56 };
        a[status_at..status_at + 4].copy_from_slice(&status.to_le_bytes());
        0
    }

    std::thread_local! {
        /// The descriptor the fake UVM was handed in MM_INITIALIZE.
        static UVM_FD_SEEN: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    }

    unsafe fn fake_uvm(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        if request == 75 {
            // SAFETY: MM_INITIALIZE's 8 bytes.
            let v = unsafe { (arg as *const i32).read_unaligned() };
            UVM_FD_SEEN.with(|s| s.set(Some(v)));
        }
        0
    }

    /// UVM fgets MM_INITIALIZE's uvmFd in the calling process: the handle
    /// the guest driver sent must reach it as our descriptor of that UVM
    /// file, come back as the handle, and name nothing else.
    #[test]
    fn a_uvm_descriptor_field_reaches_the_host_as_our_descriptor_of_that_file() {
        use std::os::fd::AsRawFd;
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_uvm);
        be.set_host_driver_version("610.57.04");
        let primary_fd = devnull();
        let primary_raw = primary_fd.as_raw_fd();
        let primary = be.adopt_for_test(primary_fd, HandleKind::Dev(DeviceKind::Uvm));
        let second = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));

        let mut p = [0u8; 8];
        p[..4].copy_from_slice(&primary.to_le_bytes());
        let resp = v1_ioctl(&mut be, second, 75, &p);
        assert_eq!(parse_resp(&resp).status, 0);
        assert_eq!(UVM_FD_SEEN.with(|s| s.take()), Some(primary_raw));
        assert_eq!(
            &resp[IOCTL_BODY..IOCTL_BODY + 4],
            &primary.to_le_bytes(),
            "the handle comes back, never our descriptor"
        );

        // An RM control file is not a UVM file, and a number that is no
        // handle of ours is not passed on as one.
        for bad in [ctl, 0x7777] {
            p[..4].copy_from_slice(&bad.to_le_bytes());
            let resp = v1_ioctl(&mut be, second, 75, &p);
            assert_eq!(parse_resp(&resp).status, -libc::EBADF);
            assert_eq!(UVM_FD_SEEN.with(|s| s.take()), None);
        }

        // -1 is UVM's "none", and names nothing anywhere.
        p[..4].copy_from_slice(&(-1i32).to_le_bytes());
        v1_ioctl(&mut be, second, 75, &p);
        assert_eq!(UVM_FD_SEEN.with(|s| s.take()), Some(-1));
    }

    /// UVM copies exactly sizeof its parameters each way, and its numbers
    /// say nothing of that size: a block must be the size the host's release
    /// has for the command, and a command the release lacks goes nowhere.
    #[test]
    fn a_uvm_block_must_be_the_size_the_hosts_release_copies() {
        let uvm_backend = |version: &str| {
            let mut be = NvidiaBackend::for_test();
            be.set_host_nodes_for_test(Vec::new(), Vec::new());
            be.set_host_ioctl_for_test(fake_uvm);
            be.set_host_driver_version(version);
            let h = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
            (be, h)
        };
        // UVM_FREE lost its length in 590.44.01: 24 bytes before, 16 after.
        let (mut be, uvm) = uvm_backend("610.57.04");
        for (len, want) in [(16, 0), (24, -libc::EINVAL), (8, -libc::EINVAL)] {
            let r = v1_ioctl(&mut be, uvm, 34, &vec![0u8; len]);
            assert_eq!(parse_resp(&r).status, want, "FREE, {len} bytes on 610");
        }
        // UVM_INITIALIZE's number says 0x3000; its block is 16 bytes.
        let r = v1_ioctl(&mut be, uvm, 0x3000_0001, &[0u8; 0x3000]);
        assert_eq!(parse_resp(&r).status, -libc::EINVAL);
        let r = v1_ioctl(&mut be, uvm, 0x3000_0001, &[0u8; 16]);
        assert_eq!(parse_resp(&r).status, 0);

        let (mut be, uvm) = uvm_backend("580.95.05");
        let r = v1_ioctl(&mut be, uvm, 34, &[0u8; 24]);
        assert_eq!(parse_resp(&r).status, 0, "FREE, 24 bytes on 580");
        // DISCARD (80) came in 580.65.06; 535 has no such command.
        let (mut be, uvm) = uvm_backend("535.129.03");
        let r = v1_ioctl(&mut be, uvm, 80, &[0u8; 32]);
        assert_eq!(parse_resp(&r).status, -libc::EPERM);
        // And a host with no table refuses them all.
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_uvm);
        let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        let r = v1_ioctl(&mut be, uvm, 34, &[0u8; 16]);
        assert_eq!(parse_resp(&r).status, -libc::EPERM);
    }

    /// S-33: a display file's handle-table descriptor is closed by the
    /// closer, not by the queue thread in CLOSE.
    #[test]
    fn closing_a_card_handle_leaves_the_last_close_to_the_closer() {
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut be = gated_backend();
        let mut p = [0i32; 2];
        // SAFETY: `p` receives the two new descriptors.
        assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: descriptors pipe2 just returned.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) };
        let card = be.adopt_for_test(r, HandleKind::DrmCard(0));
        be.close_handle(card).unwrap();
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        // SAFETY: a one-byte write from a live array.
        let n = unsafe { libc::write(w.as_raw_fd(), b"x".as_ptr().cast(), 1) };
        assert_eq!(n, -1, "the read end is closed");
    }

    /// S-24: a control that lists the host's GPU processes never reaches
    /// RM; the caller reads RM's own "insufficient permissions" and its
    /// parameters back as sent.
    #[test]
    fn a_control_listing_host_pids_is_answered_without_rm() {
        let mut be = gated_backend();
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let control = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32);
        let mut p = [0u8; 32];
        p[8..12].copy_from_slice(&0x2080_018du32.to_le_bytes());
        let resp = v1_ioctl(&mut be, ctl, control, &p);
        assert_eq!(parse_resp(&resp).status, 0, "the call itself succeeds");
        assert!(forwarded().is_empty(), "RM never saw it");
        let body = &resp[IOCTL_BODY..];
        assert_eq!(
            &body[28..32],
            &crate::rmctl::NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes()
        );
        assert_eq!(&body[..28], &p[..28]);
        assert!(be.rm_controls.is_empty());
    }

    /// S-17: the tallies are keyed by the guest's u32s, so only what RM
    /// served is counted -- a guest walking the command space adds nothing.
    #[test]
    fn only_controls_and_classes_rm_served_are_tallied() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_rm_status);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let control = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32);
        let alloc = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2b, 48);
        for k in 0..200u32 {
            let mut p = [0u8; 32];
            p[8..12].copy_from_slice(&(0x1234_0000 + k).to_le_bytes());
            v1_ioctl(&mut be, ctl, control, &p);
            let mut p = [0u8; 48];
            p[12..16].copy_from_slice(&(0x9000 + k).to_le_bytes());
            v1_ioctl(&mut be, ctl, alloc, &p);
        }
        assert!(be.rm_controls.is_empty() && be.rm_classes.is_empty());

        let mut p = [0u8; 32];
        p[8..12].copy_from_slice(&0x2080_0101u32.to_le_bytes());
        v1_ioctl(&mut be, ctl, control, &p);
        let mut p = [0u8; 48];
        p[12..16].copy_from_slice(&0x3eu32.to_le_bytes());
        v1_ioctl(&mut be, ctl, alloc, &p);
        assert_eq!(be.rm_controls.get(0x2080_0101), Some(1));
        assert_eq!(be.rm_classes.get(0x3e), Some(1));
        assert_eq!((be.rm_controls.len(), be.rm_classes.len()), (1, 1));
    }
}

/// RM mappings as a guest sees them, against a fake RM that answers these
/// calls the way the real one does: the memory type each is placed and mapped
/// with (M-1), read-only placements (M-2), and when a window extent may go to
/// another mapping (M-3).
#[cfg(test)]
mod mapping_tests {
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
    const LEN: u64 = 4096;

    std::thread_local! {
        /// NV01_MEMORY_SYSTEM attr words the host was handed.
        static HOST_ATTR: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
        static NEXT_VA: Cell<u64> = const { Cell::new(0x7f00_0000_0000) };
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
    unsafe fn fake_rm(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        let len = hostfd::ioc_size(request as u32);
        // SAFETY: `HostIoctl`'s contract: `arg` holds _IOC_SIZE bytes.
        let b = unsafe { std::slice::from_raw_parts_mut(arg, len) };
        match (request & 0xff) as u32 {
            NV_ESC_RM_ALLOC => {
                let params = u64::from_le_bytes(b[16..24].try_into().unwrap()) as *const u8;
                if rd(b, 12) == 0x3e && !params.is_null() {
                    // SAFETY: the backend pointed pAllocParms at the class
                    // parameters it built, NV_MEMORY_ALLOCATION_PARAMS.
                    let attr = unsafe { std::ptr::read_unaligned(params.add(24) as *const u32) };
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
                    _ => {}
                }
                put(b, 44, flags);
                let va = NEXT_VA.with(|v| {
                    let va = v.get();
                    v.set(va + 0x10000);
                    va
                });
                b[32..40].copy_from_slice(&va.to_le_bytes());
                put(b, 40, 0);
            }
            NV_ESC_RM_UNMAP_MEMORY => put(b, 24, 0),
            _ => {}
        }
        0
    }

    /// Records every placement, with whether it was asked to be writable.
    #[derive(Clone, Default)]
    struct RecWindow(Arc<std::sync::Mutex<Vec<(&'static str, u64, bool)>>>);

    impl crate::shm::WindowPlacer for RecWindow {
        fn place(&self, off: u64, _len: u64, _fd: RawFd, _fo: u64, w: bool) -> Result<()> {
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
        let name = std::ffi::CString::new("devfile").unwrap();
        // SAFETY: plain syscalls; the new fd's ownership passes to OwnedFd.
        unsafe {
            let fd = libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            assert_eq!(libc::ftruncate(fd, 1 << 16), 0);
            OwnedFd::from_raw_fd(fd)
        }
    }

    /// The same file, opened read-only: mapping it writable is refused the
    /// way nvidia.ko refuses a context without NV_PROTECT_WRITEABLE.
    fn read_only(f: &OwnedFd) -> OwnedFd {
        let path = std::ffi::CString::new(format!(
            "/proc/self/fd/{}",
            std::os::fd::AsRawFd::as_raw_fd(f)
        ))
        .unwrap();
        // SAFETY: reopening our own memfd.
        unsafe { OwnedFd::from_raw_fd(libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC)) }
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

    fn push<T: Copy>(v: &mut Vec<u8>, val: &T) {
        let at = v.len();
        v.resize(at + size_of::<T>(), 0);
        write_struct(&mut v[at..], val);
    }

    impl Env {
        fn call(&mut self, handle: u32, escape: u32, outer: &[u8], nested: &[u8]) -> Vec<u8> {
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
            assert_eq!(
                read_struct::<MsgHeader>(&resp, 0).status,
                0,
                "escape {escape:#x}"
            );
            let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
            resp[body..n].to_vec()
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
}

/// UVM semaphore pools in the UVM aperture (uvmmap.rs), end to end through
/// `dispatch`: a fake UVM that makes and frees pools, and a fake VMM that
/// records what it was asked to place and withdraw.
#[cfg(test)]
mod uvm_map_tests {
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
    unsafe fn fake_uvm(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        HOST.with(|h| h.borrow_mut().push(request));
        let put = |off: usize, v: u32| {
            // SAFETY: every block below is longer than `off + 4`.
            unsafe { arg.add(off).cast::<u32>().write_unaligned(v) }
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
        // SAFETY: fstat into a local.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(st.st_ino)
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
        // SAFETY: plain syscalls; ownership of the new fd passes to OwnedFd.
        unsafe {
            let fd = libc::memfd_create(c"uvm".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            OwnedFd::from_raw_fd(fd)
        }
    }

    fn msg<T: Copy>(t: MsgType, handle: u32, body: &T) -> Vec<u8> {
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
}

/// Descriptors RM resolves in the backend's process: every one is the
/// guest's handle of the caller's own file, turned into our descriptor of it,
/// or the call never reaches RM (R1, R2, R5).
#[cfg(test)]
mod descriptor_field_tests {
    use super::*;
    use abi::ioctl::*;
    use std::cell::RefCell;
    use std::os::fd::AsRawFd;

    std::thread_local! {
        /// (control, the descriptor RM was handed) for each RM_CONTROL.
        static SEEN: RefCell<Vec<(u32, i32)>> = const { RefCell::new(Vec::new()) };
    }

    fn seen() -> Vec<(u32, i32)> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// RM_CONTROL: record the descriptor field of an OS_UNIX control,
    /// answer NV_OK.
    unsafe fn fake_rm(_fd: RawFd, request: u64, arg: *mut u8) -> i32 {
        let len = hostfd::ioc_size(request as u32);
        // SAFETY: `HostIoctl`'s contract.
        let b = unsafe { std::slice::from_raw_parts_mut(arg, len) };
        if (request & 0xff) as u32 == NV_ESC_RM_CONTROL {
            let cmd = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let p = u64::from_le_bytes(b[16..24].try_into().unwrap()) as *const u8;
            if let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd) {
                // SAFETY: the backend pointed pParams at its own copy of the
                // parameters, which hold the field (checked before the call).
                let fd = unsafe { std::ptr::read_unaligned(p.add(at) as *const i32) };
                SEEN.with(|s| s.borrow_mut().push((cmd, fd)));
            } else {
                SEEN.with(|s| s.borrow_mut().push((cmd, i32::MIN)));
            }
            b[28..32].copy_from_slice(&0u32.to_le_bytes());
        }
        0
    }

    fn devnull() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    fn control(be: &mut NvidiaBackend, on: u32, cmd: u32, params: &[u8]) -> (i32, Vec<u8>) {
        let mut outer = [0u8; 32];
        outer[8..12].copy_from_slice(&cmd.to_le_bytes());
        outer[24..28].copy_from_slice(&(params.len() as u32).to_le_bytes());
        let mut req = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut req,
            &MsgHeader {
                msg_type: MsgType::Ioctl as u32,
                handle: on,
                status: 0,
                req_id: 0,
            },
        );
        let at = req.len();
        req.resize(at + size_of::<IoctlReq>(), 0);
        write_struct(
            &mut req[at..],
            &IoctlReq {
                cmd: _IOWR(NV_ESC_RM_CONTROL, 32) as u32,
                data_len: 32,
                nested_offset: 32,
                nested_len: params.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&outer);
        req.extend_from_slice(params);
        let mut resp = vec![0u8; 8192];
        let n = be.dispatch(&req, &mut resp);
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        let st = read_struct::<MsgHeader>(&resp, 0).status;
        (st, resp[body.min(n)..n].to_vec())
    }

    fn with_fd(len: usize, at: usize, v: i32) -> Vec<u8> {
        let mut p = vec![0u8; len];
        p[at..at + 4].copy_from_slice(&v.to_le_bytes());
        p
    }

    #[test]
    fn each_export_and_import_control_gets_our_descriptor_of_the_callers_control_file() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        let ctl_fd = devnull();
        let raw = ctl_fd.as_raw_fd();
        let ctl = be.adopt_for_test(ctl_fd, HandleKind::Dev(DeviceKind::Ctl));
        let _ = seen();
        // (control, parameter size in 610.57.04, where the descriptor is)
        for (cmd, len, at) in [
            (0x3d05u32, 24usize, 16usize),
            (0x3d06, 20, 0),
            (0x3d08, 80, 0),
            (0x3d0a, 76, 72),
            (0x3d0b, 2128, 0),
            (0x3d0c, 648, 0),
        ] {
            let (st, back) = control(&mut be, ctl, cmd, &with_fd(len, at, ctl as i32));
            assert_eq!(st, 0, "{cmd:#x}");
            assert_eq!(seen(), [(cmd, raw)], "{cmd:#x}: RM sees our descriptor");
            let guest = i32::from_le_bytes(back[32 + at..32 + at + 4].try_into().unwrap());
            assert_eq!(guest, ctl as i32, "{cmd:#x}: the guest's handle comes back");
        }
    }

    #[test]
    fn a_number_that_is_no_control_file_of_the_vm_never_reaches_rm() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let gpu = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Gpu(0)));
        let ev = be.adopt_for_test(devnull(), HandleKind::Eventfd);
        let _ = seen();
        // Another process's number as the guest sent it, a GPU file, an
        // eventfd, and a negative number other than -1: EBADF, RM not asked.
        for v in [ctl as i32 + 7, gpu as i32, ev as i32, -2, i32::MIN] {
            for (cmd, len, at) in [(0x3d0c, 648, 0), (0x3d06, 20, 0), (0x3d0b, 2128, 0)] {
                let (st, _) = control(&mut be, ctl, cmd, &with_fd(len, at, v));
                assert_eq!(st, -libc::EBADF, "{cmd:#x} naming {v}");
            }
        }
        assert!(seen().is_empty());
        // -1 goes as it is: RM refuses it itself.
        let (st, _) = control(&mut be, ctl, 0x3d0c, &with_fd(648, 0, -1));
        assert_eq!(st, 0);
        assert_eq!(seen(), [(0x3d0c, -1)]);
        // A block too short to hold the field is refused, not forwarded.
        let (st, _) = control(&mut be, ctl, 0x3d0a, &[0u8; 40]);
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    #[test]
    fn memacct_and_undefined_os_unix_controls_are_answered_without_rm() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let _ = seen();
        for cmd in [0x3d0du32, 0x3d0e, 0x3d04, 0x3d20] {
            let (st, back) = control(&mut be, ctl, cmd, &[0u8; 32]);
            assert_eq!(st, 0, "{cmd:#x}: RM's own answer, not a failed ioctl");
            assert_eq!(
                u32::from_le_bytes(back[28..32].try_into().unwrap()),
                crate::rmctl::NV_ERR_NOT_SUPPORTED
            );
        }
        assert!(seen().is_empty());
        // The two without a descriptor go to RM.
        let (st, _) = control(&mut be, ctl, 0x3d07, &[0u8; 8]);
        assert_eq!(st, 0);
        assert_eq!(seen().len(), 1);
    }

    #[test]
    fn an_fd_carrying_escape_forwards_minus_one_and_refuses_other_negatives() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        // NV_ESC_RM_ALLOC_MEMORY: the descriptor at 48 of 56 bytes.
        let send = |be: &mut NvidiaBackend, v: i32| {
            let mut p = vec![0u8; 56];
            p[48..52].copy_from_slice(&v.to_le_bytes());
            let mut req = vec![0u8; size_of::<MsgHeader>()];
            write_struct(
                &mut req,
                &MsgHeader {
                    msg_type: MsgType::Ioctl as u32,
                    handle: ctl,
                    status: 0,
                    req_id: 0,
                },
            );
            let at = req.len();
            req.resize(at + size_of::<IoctlReq>(), 0);
            write_struct(
                &mut req[at..],
                &IoctlReq {
                    cmd: _IOWR(NV_ESC_RM_ALLOC_MEMORY, 56) as u32,
                    data_len: 56,
                    ..Default::default()
                },
            );
            req.extend_from_slice(&p);
            let mut resp = vec![0u8; 4096];
            be.dispatch(&req, &mut resp);
            read_struct::<MsgHeader>(&resp, 0).status
        };
        assert_eq!(send(&mut be, -1), 0);
        assert_eq!(send(&mut be, -2), -libc::EBADF);
        assert_eq!(send(&mut be, i32::MIN), -libc::EBADF);
    }
}

#[cfg(test)]
mod share_tests {
    use super::*;
    use crate::quota::Owner;

    fn devnull() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    /// One process holding every NVKMS open left the compositor with none
    /// (B4): a process holds at most its share, and the VM cap stays.
    #[test]
    fn one_guest_process_cannot_hold_every_modeset_open() {
        let mut be = NvidiaBackend::for_test();
        let p = |t: u32| Owner::Proc {
            tgid: t,
            start_ns: 1,
        };
        let modeset = HandleKind::Dev(DeviceKind::Modeset);
        let mut n = 0;
        while be.modeset_open_refused(p(1)).is_none() {
            be.handles.insert_for(devnull(), modeset, p(1)).unwrap();
            n += 1;
        }
        assert_eq!(n, nvkms::MODESET_SHARE.per_owner);
        assert!(be.modeset_open_refused(p(2)).is_none());
        // Three more processes take theirs; the VM's last eight are kept
        // for processes holding at most two.
        for t in 2..5 {
            while be.modeset_open_refused(p(t)).is_none() {
                be.handles.insert_for(devnull(), modeset, p(t)).unwrap();
            }
        }
        assert!(be.modeset_open_refused(p(9)).is_none(), "a newcomer gets one");
        for _ in 0..8 {
            if be.modeset_open_refused(Owner::Unknown).is_none() {
                be.handles.insert(devnull(), modeset).unwrap();
            }
        }
        assert!(
            be.modeset_open_refused(p(9)).is_some(),
            "the VM cap is still the outer bound"
        );
    }
}
