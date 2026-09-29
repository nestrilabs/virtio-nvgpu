// SPDX-License-Identifier: Apache-2.0
// device/src/nvidia.rs

#![forbid(unsafe_code)]

use protocol::messages::*;
use std::ffi::CString;
#[cfg(any(test, fuzzing))]
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::Arc;

use crate::error::{DeviceError, Result};
use crate::handle_table::HandleTable;
use crate::hostfd::{self, CardNode, HandleKind};
use crate::nvkms::{self, NvkmsPolicy};
use crate::policy::BackendHooks;
use crate::privfd::PrivateFd;
use crate::pump::{PumpCmd, WatchMode};
use crate::semsurf::SemsurfPolicy;
use crate::session::{BackendConfig, MAX_XFER_DIRECT, Outcome, Reply, Session};
use crate::shm::{ShmAllocator, ZoneConfig};
use crate::sys::block::{Arena, BufId, Restore, SlotKind};
use crate::xfer::{Hooks, KmsFileState, Sys, VmKms};

// ============================================================
// Device path helpers
// ============================================================

const MAX_GPU: u8 = 8;

/// The floor a second-level buffer is sized to, whatever length the guest
/// derived for it. See where it is used: the length is read at a table-supplied
/// offset, the table was generated from a different driver release, and the
/// cost of it being wrong must not be heap corruption in this process.
const DEEP_BUF_FLOOR: usize = 64 * 1024;

// Field offsets in NVOS54_PARAMETERS, the struct RM_CONTROL carries.
//
// `status` is the one that matters and the one that is easy to miss: it is
// written by RM on the way out and is independent of the ioctl return value.
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
    // Fuzzing (device/src/fuzzing): no real device is ever opened.
    #[cfg(fuzzing)]
    let path = {
        let _ = path;
        "/dev/null".to_string()
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

/// RM's own statuses for a refusal the backend makes in the status field,
/// the ioctl itself succeeding (nvstatuscodes.h).
pub(crate) const NV_ERR_NO_MEMORY: u32 = 0x51;

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
pub(crate) const NV_DEV_INFO_WORDS: usize = 9;

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
    #[cfg(fuzzing)]
    if path != "/dev/null" {
        return None;
    }
    let fd = match crate::sys::fd::open_path(path, libc::O_RDWR | libc::O_CLOEXEC) {
        Ok(fd) => fd,
        Err(e) => {
            log::warn!("{path}: cannot open to ask what it is ({e})");
            return None;
        }
    };
    // Exactly the 64 bytes the ioctl's size field declares; no pointer.
    let mut bytes: Vec<u8> = [DEV_INFO_UNWRITTEN; NV_DEV_INFO_PROBE_WORDS]
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    let rc = crate::sys::block::flat(
        &crate::sys::ioctl::Host,
        fd.as_raw_fd(),
        DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE,
        &mut bytes,
    );
    // No argument; the return value is the whole answer.
    let modeset = crate::sys::ioctl::no_arg(fd.as_raw_fd(), DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED)
        .is_ok_and(|r| r == 0);
    drop(fd);
    if rc != 0 {
        log::warn!(
            "{path}: GET_DEV_INFO refused ({})",
            std::io::Error::from_raw_os_error(-rc)
        );
        return None;
    }
    let mut probe = [0u32; NV_DEV_INFO_PROBE_WORDS];
    for (w, b) in probe.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *w = u32::from_le_bytes(*b);
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
    /// `pci_config`: full config spaces the launcher snapshotted, by PCI
    /// address ([`merge_pci_config`]).
    ///
    /// Only regular files, and only small ones: these trees are descriptive
    /// text, and anything large is either not one of them or not something a
    /// guest should be handed through a single response buffer.
    fn collect(
        self,
        pci_config: &std::collections::HashMap<String, Vec<u8>>,
    ) -> Vec<(String, Vec<u8>)> {
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
                        Ok(live) => {
                            let content = pci_config
                                .get(addr.as_ref())
                                .and_then(|snap| merge_pci_config(&live, snap))
                                .unwrap_or(live);
                            Some((rel, content))
                        }
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

/// The PCI config space the guest's fake device is given: the backend's own
/// read of it (`live`) and, past it, a snapshot of the whole space the
/// launcher took as root (`snap`, `--pci-config-dir`).
///
/// Linux gives a reader without CAP_SYS_ADMIN the first 64 bytes of a
/// device's config (pci-sysfs.c, `pci_read_config`), and the backend never
/// has it: the guest's device had no capability list and no PCIe extended
/// capabilities, its capability pointer at 0x34 pointing into zeros, and
/// nvidia-smi could not report the link (review 2026-09-29, parity #14).
/// The snapshot is taken only if it is of this device -- vendor, device,
/// class and subsystem as the live read has them -- and at most 4 KiB;
/// the live bytes stay authoritative for what they cover. None otherwise.
pub(crate) fn merge_pci_config(live: &[u8], snap: &[u8]) -> Option<Vec<u8>> {
    const HEADER: usize = 64;
    const MAX: usize = 4096;
    if live.len() > HEADER || snap.len() <= live.len() || snap.len() > MAX || live.len() < HEADER {
        return None;
    }
    // Vendor and device; revision and class; subsystem vendor and id.
    for r in [0..4, 8..12, 0x2c..0x30] {
        if live[r.clone()] != snap[r] {
            log::warn!("sys: the PCI config snapshot is of another device; not used");
            return None;
        }
    }
    let mut out = live.to_vec();
    out.extend_from_slice(&snap[live.len()..]);
    Some(out)
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
    /// guest, and userspace quotes it until UPDATE_DEVICE_MAPPING_INFO gives
    /// the mapping the virtual address it was mapped at, and that address
    /// after (in UPDATE and RM_UNMAP_MEMORY); an entry answers to both, for
    /// its own RM object and process only (`MmapContext::find`). No host
    /// address reaches the guest.
    ///
    /// Each entry owns its extent, and is the only record that does: the
    /// extent is released when RM_UNMAP_MEMORY succeeds, when the handle that
    /// carries the mapping closes, or when the session resets -- whichever
    /// comes first, and exactly once.
    pub(crate) active_maps: crate::mmap::MmapContext,
    /// Host driver version, as the transport read it from the driver
    /// (`set_host_driver_version`). Never learned from a guest: the string
    /// in CHECK_VERSION_STR's reply is the caller's in RM's relaxed mode.
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
    /// Each GPU's whole PCI config space, by address, as the launcher
    /// snapshotted it (`--pci-config-dir`); see [`merge_pci_config`].
    pci_config: std::collections::HashMap<String, Vec<u8>>,
    /// Whether RM escapes pass with no host version set: only the unit
    /// tests' and fuzzers' fake RMs, which no release describes. Everything
    /// else refuses them (`unversioned_ok`).
    unversioned_for_test: bool,
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
    /// Which RM controls and classes reach the host at all (rmallow.rs).
    rmallow: crate::rmallow::RmAllow,
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
    /// The host driver. The real one, except in tests that need to see what
    /// the host driver would be handed.
    host_ioctl: Box<dyn crate::sys::block::Kernel>,
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
    /// Host buffers a capture helper injected (`--inject-socket`), and the
    /// handles INJECT_OPEN made of them (inject.rs).
    pub(crate) inject: crate::inject::BackendInject,
    /// The injected objects' dma-bufs, which no export may hand out
    /// (inject.rs, `Taint`); shared with the IOCTL2 hooks.
    pub(crate) inject_taint: crate::inject::SharedTaint,
}

/// A fake host driver for tests: what the forwarding paths hand the host,
/// as the host sees it (sys/block.rs), and its answer (>= 0, or -errno).
#[cfg(any(test, fuzzing))]
pub(crate) type HostIoctl = fn(RawFd, u64, &mut crate::sys::block::Arg<'_>) -> i32;

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

impl NvidiaBackend {
    /// The host's copy of a guest's top-level block `bytes`: a guarded block
    /// of [`ioctl_arg_len`] bytes of `a` holding them, zeros after, with the
    /// fields `plan` names declared (guestptr.rs). A handle the plan names
    /// is resolved in this backend's table.
    fn top_block(
        &self,
        a: &mut Arena,
        request: u64,
        bytes: &[u8],
        plan: &crate::guestptr::Plan<'_>,
    ) -> std::result::Result<BufId, i32> {
        let top = a.block(bytes, ioctl_arg_len(request, bytes.len()))?;
        plan.declare(a, top, &|h| self.handles.get(h).map(|(fd, _)| fd))?;
        Ok(top)
    }

    /// Hand block `top` of `a` to the host driver as the argument of
    /// `request` on `fd`: its result, or the errno it failed with.
    fn host_call(
        &self,
        a: &mut Arena,
        fd: RawFd,
        request: u64,
        top: BufId,
    ) -> std::result::Result<i32, i32> {
        let r = a.call(self.host_ioctl.as_ref(), fd, request, top);
        if r < 0 { Err(-r) } else { Ok(r) }
    }
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
    /// No profile: no host version was set, or none was measured at it.
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
        // Another type on an NVIDIA device gets the device's own answer
        // (review 2026-09-29 parity #32): nvidia.ko's nv_validate_ioctls
        // says EINVAL (nv.c:2488-2491), nvidia-modeset ENOTTY for any
        // command but its one (nvidia-modeset-linux.c:1953-1955), and UVM
        // ENOSYS for a command it has no route for (uvm_test.c).
        HandleKind::Dev(DeviceKind::Gpu(_) | DeviceKind::Ctl) => Err(libc::EINVAL),
        HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools) => Err(libc::ENOSYS),
        HandleKind::Dev(DeviceKind::Modeset) => Err(libc::ENOTTY),
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
    // The count is of the records that fit, written once they are known:
    // a count of every device over fewer records had the guest parse the
    // card section after them as DRI records (review 2026-09-29 2.6).
    let mut off = 4;
    let mut n = 0u32;
    for d in devices {
        // name_len, major, minor, slot_index, then the nine dev_info words.
        let need = 16 + 4 * NV_DEV_INFO_WORDS + d.name.len();
        if off + need > buf.len() {
            log::warn!("DRI section truncated at {}", d.name);
            break;
        }
        n += 1;
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
    buf[..4].copy_from_slice(&n.to_le_bytes());
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
/// The render nodes' names (`renderD128`), in the order a guest's render
/// indices name them (`HostNodes::dri`): the capture helper's buffers are
/// checked against the same numbering (inject.rs).
pub fn host_render_names() -> Vec<String> {
    enumerate_host_nodes()
        .dri
        .into_iter()
        .map(|d| d.name)
        .collect()
}

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
        let inject_taint = crate::inject::SharedTaint::default();
        Self {
            window: None,
            dri_maps: std::collections::HashMap::new(),
            msg_counts: std::collections::BTreeMap::new(),
            live_maps: std::collections::HashMap::new(),
            abi_policy: AbiPolicy::default(),
            unversioned_for_test: cfg!(any(test, fuzzing)),
            pci_config: std::collections::HashMap::new(),
            abi_refused: std::collections::BTreeMap::new(),
            rm_classes: crate::tally::Tally::default(),
            rm_controls: crate::tally::Tally::default(),
            rmallow: crate::rmallow::RmAllow::default(),
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
            hooks: BackendHooks::with_state(nvkms.clone(), semsurf.clone())
                .with_inject_taint(inject_taint.clone()),
            inject_taint,
            nvkms,
            semsurf,
            xfer_sys: Arc::new(crate::xfer::HostSys),
            host_ioctl: Box::new(crate::sys::ioctl::Host),
            uvm_refused: std::collections::HashSet::new(),
            uvm_maps: crate::uvmmap::UvmMaps::default(),
            wl: crate::wl::WlState::default(),
            rmmem: crate::rmmem::RmMem::default(),
            guest_ram: None,
            osdesc: crate::osdesc::OsDesc::default(),
            inject: crate::inject::BackendInject::default(),
        }
    }

    /// A backend whose shared window is `cfg`: the one `ZoneConfig` the
    /// VMM's GET_SHMEM_CONFIG is answered from too (`--window-size`,
    /// `--window-owner-share`).
    pub fn with_zone_config(cfg: ZoneConfig) -> Self {
        Self::new(cfg)
    }

    /// A backend with the default window (1 GiB, half a zone per process).
    pub fn with_default_zones() -> Self {
        Self::with_zone_config(ZoneConfig::default_1gib())
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

    /// Refuse the RM controls and classes the host release's allowlist lacks
    /// (the default), or only log them (`--rm-allowlist=log`; rmallow.rs).
    pub fn set_rm_allowlist(&mut self, mode: crate::rmallow::Mode) {
        self.rmallow.set_mode(mode);
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

    /// The window's local backing (sys::mem::Window): where a test that is
    /// its own VMM places device memory, and reads it back.
    pub fn shm_window(&self) -> Arc<crate::sys::mem::Window> {
        self.shm.window()
    }

    /// Create a minimal backend suitable for unit tests (8-page total BAR).
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
            owner_percent: 50,
        })
    }

    /// Put a descriptor straight into the table, as if an OPEN or an fd out
    /// had produced it.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn adopt_for_test(&mut self, fd: OwnedFd, kind: HandleKind) -> u32 {
        self.handles.insert(fd, kind).expect("test table has room")
    }

    /// `adopt_for_test`, as opened by guest process `owner`.
    #[cfg(test)]
    pub(crate) fn adopt_for_test_as(
        &mut self,
        fd: OwnedFd,
        kind: HandleKind,
        owner: crate::quota::Owner,
    ) -> u32 {
        self.handles
            .insert_for(fd, kind, owner)
            .expect("test table has room")
    }

    /// Replace host node enumeration, which needs real hardware.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn set_host_nodes_for_test(&mut self, dri: Vec<DriDevice>, cards: Vec<CardNode>) {
        self.nodes = Some(Arc::new(HostNodes { dri, cards }));
    }

    /// Replace the host ioctl, to see what a forwarding path hands the driver.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn set_host_ioctl_for_test(&mut self, f: HostIoctl) {
        self.host_ioctl = Box::new(crate::sys::block::FnKernel(f));
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
        // How much of the window this VM used: what `--window-size` and
        // `--window-owner-share` are chosen by (DEPLOY.md), so it is kept
        // at the default log level.
        log::warn!("window use: {}", self.shm.usage_summary());
        // What crossed the boundary, how often and how fast (pacing.rs):
        // what frame pacing is judged by, at the same level.
        crate::pacing::log_summary();
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
        self.rmallow.report();
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
        let mut kms_fbs: std::collections::HashMap<u32, Vec<u32>> = self
            .kms_states
            .drain()
            .map(|(h, k)| (h, k.retire()))
            .collect();
        self.vm_kms.clear();
        self.wl_forget_all();
        self.syncobj_regs.clear();
        self.uvm_refused.clear();
        self.nvkms.reset();
        self.inject.reset();
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
            let owner = self.handles.owner(h);
            if let Ok((fd, kind)) = self.handles.remove(h) {
                if !crate::closer::slow(kind) {
                    drop(fd);
                    continue;
                }
                let fd = self.handles.closing(fd, owner, kind);
                match kms_fbs.remove(&h).filter(|f| !f.is_empty()) {
                    Some(fbs) => self.vm_kms.close_after(fbs, Box::new(fd)),
                    None => crate::closer::close(fd),
                }
            }
        }
    }

    /// Withdraw a placement from the window and return its extent. Called
    /// exactly once per extent, by whichever record owns it.
    fn release_extent(&mut self, region: &crate::shm::ShmRegion, length: u64, why: &str) {
        // Emptied rather than unmapped: a hole would leave the memory slot
        // covering a range that reaches no mapping at all, and a stray access
        // there faults the VMM rather than the guest.
        if let Some(window) = self.window.as_ref()
            && let Err(e) = window.withdraw(region.offset, length)
        {
            log::warn!(
                "{why}: the window would not give back {:#x}+{length:#x}: {e}",
                region.offset
            );
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
        #[cfg(test)]
        crate::fuzz_seeds::served(req_buf, cap);
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

        if kind == HandleKind::Dev(DeviceKind::Modeset)
            && let Some(why) = self.modeset_open_refused(self.current_owner)
        {
            log::warn!("OPEN of /dev/nvidia-modeset refused: {why}");
            return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, libc::EMFILE);
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
        let fd = match crate::sys::fd::open(&path, libc::O_RDWR | libc::O_CLOEXEC) {
            Ok(fd) => fd,
            Err(err) => {
                let errno = err.raw_os_error().unwrap_or(0);
                log::warn!("open({:?}) failed: {}", path, err);
                return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, errno);
            }
        };
        let raw_fd = fd.as_raw_fd();
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
            // Once per guest wait for a guest that arms RM readiness
            // (BCAP_ARMED_READY); every host event otherwise.
            let mode = if self.session.armed_ready && kind.readiness_is_armed() {
                WatchMode::LegacyArmed
            } else {
                WatchMode::Legacy
            };
            self.pump_cmds.push(PumpCmd::Watch {
                handle: guest_handle,
                fd,
                mode,
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
        // Files still closing are still open on the host: with the closer
        // stalled on a modeset, one process looping open/close held far
        // more than the cap (review 2026-09-29 1.12).
        let (closing, closing_mine) = self.handles.closing_modesets(owner);
        let (mut open, mut mine) = (closing as usize, closing_mine as usize);
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
                log::debug!(
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
                log::debug!(
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

    /// The memory type an RM_MAP_MEMORY of `h_client`'s `h_memory` will most
    /// likely be mapped with, known before RM answers: what
    /// `rm_mapping_pgprot` makes of the answer RM gives for it. Registers
    /// and system memory we saw allocated are in our records; anything
    /// else is taken to be video memory, which RM maps through BAR1
    /// write-combined. A wrong guess costs a second allocation, not a
    /// wrong type: the reply decides.
    fn rm_mapping_pgprot_before(&self, h_client: u32, h_memory: u32) -> crate::shm::PgprotKind {
        match self.rmmem.lookup(h_client, h_memory) {
            Some(m) => m.pgprot(),
            None => crate::shm::PgprotKind::WriteCombine,
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
        if let Some(&id) = self.dri_maps.get(&(handle, fd_offset))
            && let Some(live) = self.live_maps.get_mut(&id)
        {
            // No more than the placement holds. The guest maps as many
            // bytes as it asked for from the placement's offset, so a
            // second MMAP of the same file asking for more than the first
            // would reach past this extent into the window's next ones --
            // another guest process's device memory -- or into unplaced
            // window, whose first touch stops the VM. The recorded path
            // refuses the same (`handle_mmap`).
            let mapped = live.length.div_ceil(4096) * 4096;
            if size > mapped {
                log::warn!(
                    "mmap on handle {handle}: {size:#x} bytes asked of a {mapped:#x}-byte \
                     placement already made at file offset {fd_offset:#x}; refused"
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
            }
            // A placement made writable before the range was an injected
            // buffer's (a stale id since reused) is not handed out again.
            if live.writable && drm && self.inject.read_only(fd_offset, size.max(4096)) {
                log::warn!(
                    "mmap on handle {handle}: a writable placement at file offset \
                     {fd_offset:#x} is now an injected buffer's; refused"
                );
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EACCES);
            }
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

        let length = size.max(4096);
        // A size the guest kernel chose, refused before any arithmetic on
        // it: no zone holds more than its own size.
        if length > self.shm.largest_zone() {
            log::warn!("mmap on handle {handle}: {length:#x} bytes is more than any zone holds");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOMEM);
        }
        // An injected buffer's range is placed read-only, whichever of the
        // VM's files maps it (inject.rs): the guest's CPU does not write
        // into the host's capture buffers.
        let injected = drm && self.inject.read_only(fd_offset, length);
        let writable = !injected && crate::shm::host_mapping_writable(host_fd, length, fd_offset);
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

        log::debug!(
            "mmap on handle {handle}: placed {length:#x} bytes at window offset {:#x} \
             with no arming recorded here{}",
            region.offset,
            if injected {
                ", read-only (injected)"
            } else {
                ""
            }
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
        if let Some(key) = live.key
            && self.dri_maps.get(&key) == Some(&req.mapping_id)
        {
            self.dri_maps.remove(&key);
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
        let files = tree.collect(&self.pci_config);
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
        let owner = self.handles.owner(handle);
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
        let fbs = self.forget_kms_state(handle);
        // A render file's syncobj numbers die with it and a later file may
        // get the same handle number: its waits must never join these (S-13).
        if matches!(kind, HandleKind::DriRender(_)) {
            self.syncobj_regs.orphan_file(handle);
        }
        self.wl_forget(handle);
        self.nvkms.forget_handle(handle);
        self.inject.file_closed(handle);
        self.pump_cmds.push(PumpCmd::Unwatch { handle });
        log::debug!("close handle={handle} ({kind:?})");
        // A display file's last close can wait on a modeset; not here, on
        // the queue thread under the backend mutex (closer.rs, S-33). One
        // whose framebuffers a call in flight names closes after it (S-6).
        // Counted against the table until it is closed (handle_table.rs).
        if !crate::closer::slow(kind) {
            drop(fd);
        } else if fbs.is_empty() {
            crate::closer::close(self.handles.closing(fd, owner, kind));
        } else {
            self.vm_kms
                .close_after(fbs, Box::new(self.handles.closing(fd, owner, kind)));
        }
        Ok(())
    }

    /// Drop KMS handle `handle`'s per-file state, and with it every
    /// framebuffer it made from the VM's scanout sources: before the host
    /// file closes and its ids can go to someone else (S-6). Returns those
    /// ids: the host file must close through `vm_kms.close_after`.
    pub(crate) fn forget_kms_state(&mut self, handle: u32) -> Vec<u32> {
        self.kms_states
            .remove(&handle)
            .map(|k| k.retire())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // IOCTL — top-level
    // ------------------------------------------------------------------

    /// GPU `addr`'s whole PCI config space, as a privileged launcher read it
    /// (`--pci-config-dir`). Checked against the device's own header at each
    /// GET_SYS_FILES ([`merge_pci_config`]).
    pub fn set_pci_config(&mut self, addr: &str, config: Vec<u8>) {
        self.pci_config.insert(addr.to_string(), config);
    }

    /// The host driver version, as the transport read it from the driver at
    /// start-up. Known before any guest asks, it tells HELLO whether an
    /// NVKMS schema exists for this host (`BCAP_NVKMS_TABLE`) -- which the
    /// guest uses to pick its own NVKMS table -- and lets a modeset IOCTL2
    /// find its table before the guest's first CHECK_VERSION_STR.
    ///
    /// The transport refuses to start on a version that does not parse, or
    /// one the tables were not measured at (`crate::release`), before this.
    pub fn set_host_driver_version(&mut self, text: &str) {
        match abi::version::DriverVersion::parse(text) {
            Some(v) => self.set_driver_version(v),
            None => log::error!(
                "host driver version {text:?} does not parse; no NVKMS schema, and no RM \
                 escape passes the ABI check"
            ),
        }
    }

    /// Whether an RM escape may pass with no host version set: in the unit
    /// tests and fuzzers only, whose fake RMs no release describes.
    fn unversioned_ok(&self) -> bool {
        cfg!(any(test, fuzzing)) && self.unversioned_for_test
    }

    fn set_driver_version(&mut self, v: abi::version::DriverVersion) {
        self.driver = Some(v);
        self.rmallow.set_driver(v);
        self.nvkms.set_version(v);
        self.config.nvkms_table = crate::schema::modeset_table(v).is_some();
        self.abi = abi::versions::table_for(v);
        match self.abi {
            Some(t) => log::debug!("host driver {v}: ABI profile selected, {} escapes", t.len()),
            // `allow_unmeasured_release` may still pick the nearest; the
            // transport refuses to start on this otherwise (release.rs).
            None => log::warn!("host driver {v} has no measured ABI profile"),
        }
    }

    /// `--allow-unmeasured-release`: a host newer than the ABI profiles were
    /// measured through is size-checked against the nearest older profile,
    /// rather than refused outright. After `set_host_driver_version`.
    pub fn allow_unmeasured_release(&mut self) {
        if let (Some(v), None) = (self.driver, self.abi) {
            self.abi = abi::versions::nearest_table_for(v);
            if self.abi.is_some() {
                log::warn!(
                    "host driver {v}: ABI profile of {} (nearest older, unmeasured at {v})",
                    abi::versions::nearest_profile_version(v).expect("a table has a version")
                );
            }
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

        // Room for the answer, before anything is asked of the host: a
        // success comes back with at least the top-level struct and nested
        // block (and a registration's 8-byte id), and one that did not fit
        // was answered ENOSPC after RM had acted -- for a registration, with
        // the pages pinned by RM and the guest unpinning them on the error
        // (review 2026-09-29 1.16).
        let least = size_of::<MsgHeader>()
            + size_of::<IoctlResp>()
            + nested_end
            + if listed { 8 } else { 0 };
        if resp_buf.len() < least {
            log::warn!(
                "ioctl cmd={:#x}: a reply of at least {least} bytes does not fit the {} posted",
                ireq.cmd,
                resp_buf.len()
            );
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, cookie, 0);
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
                    return self.write_error_resp(
                        resp_buf,
                        Status::IoctlFailed,
                        cookie,
                        libc::EPERM,
                    );
                }
                let init_flags_mask = self
                    .driver
                    .and_then(abi::schema::uvm_table)
                    .map_or(0, |t| t.init_flags_mask);
                let params = param_in;
                let mut plan =
                    match crate::guestptr::uvm_gate(tools, ireq.cmd, params, init_flags_mask) {
                        Ok(p) => p,
                        Err(errno) => {
                            return self.write_error_resp(
                                resp_buf,
                                Status::IoctlFailed,
                                cookie,
                                errno,
                            );
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
                match self.uvm_fd_in(ireq.cmd, params) {
                    Ok(Some((off, handle))) => plan
                        .slots
                        .push((off, crate::guestptr::TopSlot::Handle { handle, width: 4 })),
                    Ok(None) => {}
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                }
                // The RM client whose objects UVM would duplicate: one this
                // VM made on the control file the call names (uvm_client_ok).
                if let Err(errno) = self.uvm_client_ok(ireq.cmd, params) {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                // Registered memory UVM would keep a duplicate of
                // (osdesc.rs): refused, or followed.
                let osdesc_map = match self.osdesc_uvm_before(ireq.cmd, params) {
                    Ok(m) => m,
                    Err(errno) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                    }
                };
                let n = self.dispatch_simple(cookie, host_fd, request, params, &plan, resp_buf);
                self.osdesc_uvm_after(ireq.cmd, params, osdesc_map, resp_buf, n);
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
                    && init_flags_mask & crate::guestptr::UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS
                        == 0
                {
                    self.uvm_pageable_off(host_fd, resp_buf, n);
                }
                self.uvm_observe(ireq.cmd, params, resp_buf, n, init_flags_mask);
                return n;
            }
            V1Route::DrmFlat => {
                let n = self.dispatch_simple(
                    cookie,
                    host_fd,
                    request,
                    param_in,
                    &crate::guestptr::Plan::default(),
                    resp_buf,
                );
                // A fence context is a GEM object of the file, and counts
                // against its cap until it is closed (semsurf.rs).
                if ireq.cmd == hostfd::DRM_IOCTL_GEM_CLOSE
                    && crate::semsurf::reply_params(resp_buf, n).is_some()
                {
                    let gem = u32::from_le_bytes(param_in[..4].try_into().unwrap());
                    self.semsurf.gem_closed(self.current_handle, gem);
                    self.inject.gem_closed(self.current_handle, gem);
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
                    cookie,
                    host_fd,
                    request,
                    &msg,
                    &crate::guestptr::Plan::default(),
                    resp_buf,
                    16, // outer_size
                    8,  // ptr_offset
                    4,  // size_offset
                    None,
                    None,
                    None,
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
                    &crate::guestptr::Plan::default(),
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
        let host = self
            .driver
            .map_or_else(|| "(unknown)".to_string(), |v| v.to_string());
        let refuse = match self.check_abi(escape, ireq.data_len) {
            AbiCheck::SizeMismatch { expected, actual } => {
                log::warn!(
                    "escape {escape:#04x}: guest sent {actual} bytes, host driver {host} expects \
                     {expected}"
                );
                true
            }
            AbiCheck::UnknownEscape => {
                log::warn!("escape {escape:#04x} is not in the ABI profile for host driver {host}");
                true
            }
            // A known host with no profile: one no table was measured at,
            // which the transport refuses to start on (crate::release). Its
            // layouts are nobody's to guess.
            //
            // No version at all is refused too: the transport reads it from
            // the driver before any guest call, and a library user that set
            // none (test-harness, an embedding VMM) used to learn it from
            // the guest's CHECK_VERSION_STR, whose string RM leaves as the
            // caller sent it -- the guest chose the ABI profile (review
            // 2026-09-29 1.15).
            AbiCheck::NoProfile if self.driver.is_none() && self.unversioned_ok() => false,
            AbiCheck::NoProfile => {
                log::warn!("escape {escape:#04x}: host driver {host} has no ABI profile");
                true
            }
            AbiCheck::Ok | AbiCheck::VariableLength => false,
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

        // Default deny: an RM control or class the host release's allowlist
        // lacks is answered here as RM answers what it does not implement
        // (rmallow.rs). Whatever the ABI policy. The controls the backend
        // answers itself below (rmctl.rs) never reach RM and keep their own
        // answers; the page-list path's classes are held to it too.
        if ioc_type == b'F' as u32 {
            let answered_here = escape == NV_ESC_RM_CONTROL
                && (crate::rmctl::unix_refused(param_in).is_some()
                    || crate::rmctl::host_pid_control(param_in).is_some());
            let sent = &body[..nested_end];
            if !answered_here
                && let Err(r) = self.rmallow.check(escape, sent, ireq.data_len as usize)
            {
                return match r {
                    crate::rmallow::Refusal::Errno(errno) => {
                        self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno)
                    }
                    crate::rmallow::Refusal::Status { at, status } => {
                        let mut out = crate::rmshare::refusal(sent, at, status);
                        let deep = &body[nested_end..want];
                        out.extend_from_slice(deep);
                        self.write_ioctl_resp_deep(resp_buf, cookie, &out, deep.len())
                    }
                };
            }
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
        // the ABI policy: the call is refused, or the plan names every field
        // RM would dereference, and the host's copy is built with each of them
        // holding only what the backend puts there (sys/block.rs); the
        // caller's values go back in the reply. The parameter pointers of
        // RM_CONTROL and RM_ALLOC are dispatch_nested's.
        let outer_len = (ireq.data_len as usize).min(param_in.len());
        // IDLE_CHANNELS' channel list, sent with its three arrays: each gets
        // a block of the call's own.
        let plan = match (escape, deep_segs) {
            (NV_ESC_RM_IDLE_CHANNELS, Some(deep)) => {
                crate::guestptr::idle_channels_list(ireq.cmd, &param_in[..outer_len], deep)
            }
            _ => crate::guestptr::rm_escape(ireq.cmd, &param_in[..outer_len]),
        };
        let plan = match plan {
            Ok(p) => p,
            Err(errno) => {
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
        };

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
            NV_ESC_REGISTER_FD => self
                .dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, &plan, resp_buf),
            // The OS events RM calls may name later (semsurf.rs).
            NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
                let n = self.dispatch_fd_carrying(
                    cookie, host_fd, request, escape, param_in, &plan, resp_buf,
                );
                self.semsurf_track_rm(escape, self.current_handle, param_in, resp_buf, n);
                n
            }

            NV_ESC_RM_ALLOC_MEMORY => self
                .dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, &plan, resp_buf),

            NV_ESC_RM_MAP_MEMORY => {
                self.dispatch_map_memory(cookie, host_fd, request, param_in, &plan, resp_buf)
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
                    cookie, host_fd, request, param_in, &plan, resp_buf, 32, 16, 24, deep_in, None,
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
                    cookie, host_fd, request, param_in, &plan, resp_buf, 48, 16, 32, deep_in, None,
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
                let n = self.dispatch_simple(cookie, host_fd, request, param_in, &plan, resp_buf);
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
        n
    }

    // ------------------------------------------------------------------
    // Nested-pointer ioctl (RM_CONTROL, RM_ALLOC..)
    // ------------------------------------------------------------------

    /// A call whose top-level block holds a pointer to a parameter block:
    /// RM_CONTROL, RM_ALLOC, NVKMS's v1 ioctl and nvidia-drm's GEM import
    /// and export.
    ///
    /// The host's copies are built in an arena (sys/block.rs), never by
    /// editing the guest's: the top-level block and the parameter block are
    /// the guest's bytes with every field the backend knows to be more than
    /// data declared first -- the parameter pointer, the pointers `plan`
    /// names, a descriptor the parameters carry, the pointers RM follows
    /// inside a control's parameters -- and each declared field then holds
    /// only what the backend put there: an address of another block of the
    /// call, a descriptor of its own, or 0. The reply is a copy with the
    /// caller's values back in those fields.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_nested(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
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
        let outer_in = &param_in[..outer_size];
        let nested_in = &param_in[outer_size..];
        let escape = (request & 0xFF) as u32;
        let fail = |be: &Self, resp_buf: &mut [u8], st: Status, e: i32| {
            be.write_error_resp(resp_buf, st, cookie, e)
        };

        // An OS event named by descriptor inside RM's parameters: semaphore
        // surface waiters and NV_EVENT_BUFFER (semsurf.rs). Found before
        // anything is copied, so a call that would have the host read the
        // field from past what the guest sent is refused outright.
        let rm = nested_fd_offset.is_none() && hostfd::ioc_type(request as u32) == b'F';
        let os_event = match crate::semsurf::os_event_field(
            if rm { escape } else { 0 },
            outer_in,
            nested_in.len(),
        ) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "RM call {request:#x}: its OS-event field is not in the {} bytes sent",
                    nested_in.len()
                );
                return fail(self, resp_buf, Status::IoctlFailed, e);
            }
        };
        // Neither a waiter nor an event buffer holds a second-level pointer,
        // and one the guest claims could be aimed at the field: the address
        // written there below would reach RM as the event.
        if os_event.is_some() && (deep_in.is_some() || deep_segs.is_some()) {
            log::warn!("RM call {request:#x} names an OS event and claims a deep pointer");
            return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
        }

        let word = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        // Log RM_CONTROL/RM_ALLOC for debugging Vulkan init. One line per
        // call, so debug: at info a guest's RM traffic was the log (S-20).
        if escape == 0x2A && outer_size >= 12 {
            log::debug!(
                "RM_CONTROL cmd=0x{:x} (hClient={}, hObject={})",
                word(outer_in, 8),
                word(outer_in, 0),
                word(outer_in, 4)
            );
        }
        if escape == 0x2B && outer_size >= 16 {
            log::debug!("RM_ALLOC hClass=0x{:x}", word(outer_in, 12));
        }

        // The size the host copies through the pointer: the outer struct's
        // own field (u64 in nvidia-drm's blocks, u32 in RM's and NVKMS's).
        let size_wide = hostfd::ioc_type(request as u32) == b'd';
        let host_size = if size_wide {
            outer_in
                .get(size_offset..size_offset + 8)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
        } else {
            outer_in
                .get(size_offset..size_offset + 4)
                .map_or(0, |b| u64::from(u32::from_le_bytes(b.try_into().unwrap())))
        };

        // The top-level block: sized for what the host copies, not for what
        // the guest sent (`ioctl_arg_len`), its parameter pointer declared --
        // the caller reads back the pointer it passed, never ours and never
        // zero: the host driver leaves the field alone, so a native caller
        // reads back its own -- and the escape's other pointers as `plan`
        // says.
        let mut a = Arena::new();
        let top = match self.top_block(&mut a, request, outer_in, plan) {
            Ok(t) => t,
            Err(e) => return fail(self, resp_buf, Status::IoctlFailed, e),
        };
        if let Err(e) = a.slot(top, ptr_offset, 8, SlotKind::Ptr, Restore::Yes) {
            return fail(self, resp_buf, Status::IoctlFailed, e);
        }

        if nested_in.is_empty() {
            if deep_segs.is_some() {
                log::warn!("ioctl {request:#x}: deep segments with no parameters to hold them");
                return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
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
            if host_size != 0 {
                log::warn!(
                    "ioctl {request:#x}: parameter size {host_size} but no parameters sent; \
                     refused rather than handing the host the guest's pointer"
                );
                return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
            }
            if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
                log::warn!("nested ioctl(0x{request:x}) no-params failed: errno={errno}");
                let back = a.reply(top)[..outer_size].to_vec();
                return self.write_failed_resp(resp_buf, cookie, &back, errno);
            }
            let outer = &a.bytes(top)[..outer_size];
            if escape == 0x2a {
                log::debug!(
                    "(else) RM_CONTROL cmd=0x{:08x} status=0x{:x}",
                    word(outer, 8),
                    word(outer, 28)
                );
            } else if escape == 0x2b {
                log::debug!(
                    "(else) RM_ALLOC hClass=0x{:04x} status=0x{:x}",
                    word(outer, 12),
                    word(outer, 40)
                );
            }
            return self.write_ioctl_resp(resp_buf, cookie, &a.reply(top)[..outer_size]);
        }

        // Guest sent nested params: a block of the call's, and the pointer
        // at it.
        let nested_size = nested_in.len();
        // Exactly as many bytes as the host will copy, or none said
        // (RM_ALLOC's zero: RM takes the class's own size, and no class
        // with a pointer in its parameters gets this far, guestptr.rs).
        // Past what was sent the host reads the zeroed slack after our
        // buffer (guarded.rs), so a pointer field the block cuts short
        // reached RM as the guest's low bytes over our zeros: seven of
        // eight bytes, any address in this process, which the scrub
        // below -- reading only the bytes sent -- never saw, and RM
        // copied in from and out to it (FIFO_GET_CHANNELLIST writes its
        // channel list there). Found by the `backend_v2` fuzz target.
        if host_size != 0 && host_size != nested_size as u64 {
            log::warn!(
                "ioctl {request:#x}: the host would copy {host_size} bytes of parameters and \
                 {nested_size} were sent; refused"
            );
            return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
        }
        // Guarded rather than heap-allocated: the driver writes its answer
        // here, and if it writes more than the caller's size field claimed,
        // the fault should land on that write rather than on someone else's
        // allocation later.
        let nb = match a.block(nested_in, nested_size) {
            Ok(b) => b,
            Err(e) => return fail(self, resp_buf, Status::IoctlFailed, e),
        };

        // An event object names the file its notifications arrive on, in
        // `NV0005_ALLOC_PARAMETERS.data` at offset 16. The guest driver has
        // already turned the caller's descriptor into one of our handles;
        // the host gets the descriptor this process holds for it, and the
        // reply the handle (the guest driver then restores the caller's own
        // descriptor over it).
        if escape == 0x2B && outer_size >= 16 {
            let h_class = word(outer_in, 12);
            const NV0005_DATA: usize = 16;
            // A block too short to hold the field is refused, as OS_UNIX's
            // is: RM would read the descriptor from past what was sent --
            // the zeroed slack after our buffer, descriptor 0 of this
            // process (review 2026-09-29 1.17).
            if matches!(h_class, 0x05 | 0x79) && nested_size < NV0005_DATA + 4 {
                log::warn!(
                    "event class {h_class:#x}: {nested_size} parameter bytes do not hold its \
                     descriptor"
                );
                return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
            }
            if matches!(h_class, 0x05 | 0x79) {
                let guest =
                    i32::from_le_bytes(nested_in[NV0005_DATA..NV0005_DATA + 4].try_into().unwrap());
                let set = match self.dev_fd(guest as u32) {
                    Ok(fd) => a
                        .fd(nb, NV0005_DATA, 4)
                        .and_then(|_| a.set_fd(nb, NV0005_DATA, fd)),
                    // -1, "no descriptor", goes as it is.
                    Err(_) if guest == -1 => a
                        .fd(nb, NV0005_DATA, 4)
                        .and_then(|_| a.set_no_fd(nb, NV0005_DATA, -1)),
                    // Anything else is refused, not forwarded: RM would
                    // look the number up among every guest process's
                    // files (R2).
                    Err(_) => {
                        log::warn!(
                            "event class {h_class:#x}: no handle {guest} for the file this \
                             event is to be delivered on"
                        );
                        return fail(self, resp_buf, Status::BadHandle, libc::EBADF);
                    }
                };
                if let Err(e) = set {
                    return fail(self, resp_buf, Status::IoctlFailed, e);
                }
            }
        }

        // The same for the OS event a waiter or an event buffer names by
        // (u64) descriptor: our handle becomes the host descriptor RM
        // looks the event up by, and only for an event that is live --
        // for NV_EVENT_BUFFER a lookup that misses is a host oops, not a
        // refusal (semsurf.rs).
        if let Some((off, h_client)) = os_event {
            let set = match self.os_event_fd(h_client, &nested_in[off..off + 8]) {
                Ok(None) => a.fd(nb, off, 8).map(|_| ()),
                Ok(Some(fd)) => a.fd(nb, off, 8).and_then(|_| a.set_fd(nb, off, fd)),
                Err(e) => return fail(self, resp_buf, Status::IoctlFailed, e),
            };
            if let Err(e) = set {
                return fail(self, resp_buf, Status::IoctlFailed, e);
            }
        }

        // A control file named by descriptor in NV0000's OS_UNIX
        // controls (rmctl.rs): the host gets our descriptor of that very
        // file, and only a control file's; a number that is not one is
        // refused, never forwarded, since RM would resolve it among every
        // guest process's files (R1, R2). -1 passes: RM refuses it itself.
        if escape == 0x2A && outer_size >= 12 {
            let cmd = word(outer_in, 8);
            if let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd) {
                if nested_size < at + 4 {
                    log::warn!(
                        "RM control {cmd:#x}: {nested_size} parameter bytes do not hold its \
                         descriptor"
                    );
                    return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
                }
                let guest = i32::from_le_bytes(nested_in[at..at + 4].try_into().unwrap());
                let set = if guest == -1 {
                    a.fd(nb, at, 4).and_then(|_| a.set_no_fd(nb, at, -1))
                } else {
                    match self.ctl_fd(guest as u32) {
                        Ok(fd) => a.fd(nb, at, 4).and_then(|_| a.set_fd(nb, at, fd)),
                        Err(_) => {
                            log::warn!(
                                "RM control {cmd:#x}: {guest} is no control file of this VM"
                            );
                            return fail(self, resp_buf, Status::BadHandle, libc::EBADF);
                        }
                    }
                };
                if let Err(e) = set {
                    return fail(self, resp_buf, Status::IoctlFailed, e);
                }
            }
        }

        // A descriptor named by position rather than by command: NVKMS's
        // import and export blocks both begin with the `memFd` naming the
        // memory. The guest driver has already turned its own descriptor
        // into one of our handles; the host gets the descriptor this process
        // holds, and the reply the guest's value.
        if let Some(off) = nested_fd_offset {
            if nested_size < off + 4 {
                log::warn!("ioctl {request:#x}: fd at {off} is outside {nested_size} nested bytes");
                return fail(self, resp_buf, Status::InvalidMsgType, libc::EINVAL);
            }
            let guest = i32::from_le_bytes(nested_in[off..off + 4].try_into().unwrap());
            match self.dev_fd(guest as u32) {
                Ok(fd) => {
                    log::debug!("nvkms memFd: handle {guest} → host fd {}", fd.as_raw_fd());
                    if let Err(e) = a.fd(nb, off, 4).and_then(|_| a.set_fd(nb, off, fd)) {
                        return fail(self, resp_buf, Status::IoctlFailed, e);
                    }
                }
                Err(_) => {
                    log::warn!(
                        "nvkms memFd: no handle {guest}; the memory to import names a file we \
                         did not open"
                    );
                    return fail(self, resp_buf, Status::BadHandle, 0);
                }
            }
        }

        // The pointer inside the nested block, given a block of the call's:
        // the guest sends the bytes it addresses, never an address.
        //
        // The block is padded well past what the guest said it holds. The
        // length RM copies comes from a field inside the caller's own
        // parameters, read with an offset out of a table generated from one
        // driver release; when that offset is wrong for the release in use
        // -- which it demonstrably is for some commands -- the length read
        // is not the buffer's, while the driver still writes as much as the
        // command really produces. Guarded, so a guest that says 8 bytes and
        // a count of 100000 has RM write into zeroed slack or fault on the
        // guard page (EFAULT), never into our heap. Only the bytes the guest
        // asked for go back.
        let mut deep: Option<(usize, BufId)> = None;
        // A deep block RM never follows a pointer to: its bytes go back as
        // they came (see below).
        let mut deep_unread: Option<&[u8]> = None;
        if let Some((ptr_off, _)) = deep_in {
            // Only an RM control's parameters hold a pointer the guest sends
            // one block for. On anything else -- an RM_ALLOC, whose classes
            // with pointers are refused outright (guestptr.rs), nvidia-drm's
            // import and export -- the address written below would reach
            // the host as data.
            if !(rm && escape == abi::ioctl::NV_ESC_RM_CONTROL) {
                log::warn!("ioctl {request:#x}: a deep block on a call that takes none; refused");
                return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
            }
            // Controls whose pointers stay zeroed (ACPI methods among
            // them; abi::rmctrl::ZEROED_CONTROLS) take no deep block.
            if rm
                && escape == abi::ioctl::NV_ESC_RM_CONTROL
                && abi::rmctrl::zeroed(word(outer_in, 8))
            {
                log::warn!(
                    "RM control {:#010x}: a deep block for a control whose pointers are never \
                     relocated",
                    word(outer_in, 8)
                );
                return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
            }
            if ptr_off + 8 > nested_size {
                log::warn!(
                    "ioctl {request:#x}: pointer at {ptr_off} is outside {nested_size} nested bytes"
                );
                return fail(self, resp_buf, Status::InvalidMsgType, libc::EINVAL);
            }
        }
        // The block's address goes only where RM follows a pointer in this
        // control's parameters (the measured tables, guestptr.rs). Anywhere
        // else it would reach RM as data: SET_ZBC_COLOR_CLEAR stored it in
        // the GPU-wide ZBC table, where any tenant reads it back -- an
        // address of this process, and a clear color of the guest's making.
        // RM does not read the field as a pointer, so there is nothing to
        // copy: the field keeps the guest's bytes, as does the block, which
        // goes back unchanged -- what a native caller's buffer holds after
        // a call that never touched it (V1 GPU_GET_ID_INFO's szName, which
        // the guest driver still carries one for).
        if let Some((ptr_off, bytes)) = deep_in
            .filter(|&(o, _)| !crate::guestptr::control_pointers(word(outer_in, 8)).contains(&o))
        {
            log::debug!(
                "RM control {:#010x}: a deep block for {ptr_off}, where RM follows no pointer; \
                 not relocated",
                word(outer_in, 8)
            );
            deep_unread = Some(bytes);
        } else if let Some((ptr_off, bytes)) = deep_in {
            let db = match a.block(bytes, bytes.len().max(DEEP_BUF_FLOOR)) {
                Ok(b) => b,
                Err(e) => return fail(self, resp_buf, Status::IoctlFailed, e),
            };
            log::debug!(
                "deep pointer at {ptr_off}: guest says {} bytes, buffer {} bytes",
                bytes.len(),
                a.len(db)
            );
            if let Err(e) = a
                .slot(nb, ptr_off, 8, SlotKind::Ptr, Restore::Yes)
                .and_then(|_| a.point(nb, ptr_off, db))
            {
                return fail(self, resp_buf, Status::IoctlFailed, e);
            }
            deep = Some((ptr_off, db));
        }

        // What several pointers of a control's parameters refer to, when the
        // guest sent it as deep segments: checked against the sizes RM will
        // copy, computed from these very parameters, and each given a block
        // of the call's (deepseg.rs). Only a control's parameters are read
        // so.
        let segs = match deep_segs {
            None => None,
            Some(segs) if rm && escape == abi::ioctl::NV_ESC_RM_CONTROL => {
                let cmd = word(outer_in, 8);
                let Some(ctl) = abi::rmctrl::deep_control(cmd) else {
                    log::warn!(
                        "RM control {cmd:#010x}: deep segments for a control whose pointers \
                         are not relocated"
                    );
                    return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL);
                };
                let what = format!("RM control {cmd:#010x} ({})", ctl.name);
                match crate::deepseg::Segments::relocate(&what, ctl.ptrs, &mut a, nb, segs) {
                    Ok(s) => Some(s),
                    Err(e) => return fail(self, resp_buf, Status::IoctlFailed, e),
                }
            }
            Some(_) => return fail(self, resp_buf, Status::IoctlFailed, libc::EINVAL),
        };
        // Every other pointer RM would follow inside a control's
        // parameters: 0 for the host, the caller's value in the reply
        // (guestptr.rs). Left as sent, RM copied in from and out to that
        // address in this process.
        if rm && escape == abi::ioctl::NV_ESC_RM_CONTROL {
            let relocated: Vec<usize> = match &segs {
                Some(s) => s.offsets(),
                None => deep.map(|(o, _)| o).into_iter().collect(),
            };
            if let Err(e) =
                crate::guestptr::scrub_control(word(outer_in, 8), &mut a, nb, &relocated)
            {
                return fail(self, resp_buf, Status::IoctlFailed, e);
            }
        }

        // The top-level block's pointer at the parameters, and the call.
        if let Err(e) = a.point(top, ptr_offset, nb) {
            return fail(self, resp_buf, Status::IoctlFailed, e);
        }
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("nested ioctl(0x{:x}) failed: errno={}", request, errno);
            // The block and its parameters as the host left them; no deep
            // block, which the guest reads only from a success.
            let mut back = a.reply(top)[..outer_size].to_vec();
            back.extend_from_slice(&a.reply(nb));
            return self.write_failed_resp(resp_buf, cookie, &back, errno);
        }

        // RM reports two different things in two different places, and only
        // one of them is the ioctl return value. A control call routinely
        // comes back rc=0 with a failure in the NVOS54 status word, and the
        // caller believes the status, not the rc. Counting non-zero rc told
        // us every call succeeded while the ICD was reading refusals.
        let outer = a.bytes(top);
        if escape == 0x2a && outer_size >= NVOS54_TOTAL {
            let cmd = word(outer, NVOS54_CMD);
            let params_size = word(outer, NVOS54_PARAMS_SIZE);
            let status = word(outer, NVOS54_STATUS);
            // RM refusing a control is routine (userspace probes), and its
            // parameters may hold host addresses and the guest's data: the
            // command and status only, at debug.
            log::debug!(
                "RM_CONTROL cmd=0x{cmd:08x} paramsSize={params_size} -> status=0x{status:08x}"
            );
        }
        if escape == 0x2b && param_in.len() >= 48 {
            log::debug!(
                "RM_ALLOC ENTER: hClass=0x{:04x} paramsSize={} (nested_bytes={})",
                word(param_in, 12),
                word(param_in, 32),
                param_in.len() - 48
            );
        }

        // The reply: outer, the parameters, and what the pointer inside
        // them addresses -- the host's bytes, with the caller's values in
        // every field declared above.
        let mut combined = a.reply(top)[..outer_size].to_vec();
        combined.extend_from_slice(&a.reply(nb));
        // Each segment's bytes as RM left them, after the table as sent.
        if let Some(segs) = &segs {
            let deep = segs.reply(&a);
            combined.extend_from_slice(&deep);
            return self.write_ioctl_resp_deep(resp_buf, cookie, &combined, deep.len());
        }
        // Only what the guest allocated room for goes back, not the pad.
        let deep_reply = deep_in.map_or(0, |(_, b)| b.len());
        if let Some((_, db)) = deep {
            combined.extend_from_slice(&a.bytes(db)[..deep_reply]);
        }
        if let Some(bytes) = deep_unread {
            combined.extend_from_slice(bytes);
        }
        self.write_ioctl_resp_deep(resp_buf, cookie, &combined, deep_reply)
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
        log::debug!(
            "OS descriptor {cmd:#x}: {:#x} bytes of guest RAM at {:#x} of ours",
            call.size,
            pinned.addr
        );

        // What records the memory an escape makes (rmmem.rs) sees the call
        // too: a handle made here is no longer whatever it named before.
        let escape = hostfd::ioc_nr(cmd);
        let mut seen = param_in.to_vec();
        let rm_pending = self.rmmem.before(escape, &mut seen);

        // The blocks the host is handed: the guest's, with the address RM
        // pins declared and pointed at the backend's own mapping of exactly
        // the pages named (osdesc.rs), and the caller's values in the reply.
        let mut a = Arena::new();
        let built = (|| -> std::result::Result<(BufId, Option<BufId>, usize, usize), i32> {
            let top = a.block(outer, ioctl_arg_len(request, outer.len()))?;
            match call.shape {
                Shape::AllocMemory => {
                    a.ptr(top, od::OS02_MEMORY)?;
                    a.point_span(top, od::OS02_MEMORY, &pinned.span, pinned.at)?;
                    // RM reads the descriptor only to arm a mapping of
                    // NV01_MEMORY_SYSTEM (escape.c:415-431); a guest number
                    // goes no further than here.
                    a.fd(top, od::OS02_FD, 4)?;
                    a.set_no_fd(top, od::OS02_FD, -1)?;
                    Ok((top, None, od::OS02_STATUS, od::OS02_NEW))
                }
                Shape::VidHeap => {
                    a.ptr(top, od::OS32_DESCRIPTOR)?;
                    a.point_span(top, od::OS32_DESCRIPTOR, &pinned.span, pinned.at)?;
                    Ok((top, None, od::OS32_STATUS, od::OS32_HMEMORY))
                }
                Shape::RmAlloc => {
                    let n = a.block(nested, nested.len())?;
                    a.ptr(n, od::OSD_DESCRIPTOR)?;
                    a.point_span(n, od::OSD_DESCRIPTOR, &pinned.span, pinned.at)?;
                    a.slot(top, od::OS64_PARAMS, 8, SlotKind::Ptr, Restore::Yes)?;
                    a.point(top, od::OS64_PARAMS, n)?;
                    // As guestptr::rm_escape: RM grants the default rights.
                    a.slot(top, od::OS64_RIGHTS, 8, SlotKind::Ptr, Restore::Yes)?;
                    Ok((top, Some(n), od::OS64_STATUS, od::OS64_NEW))
                }
            }
        })();
        let (top, class_params, status_at, handle_at) = match built {
            Ok(b) => b,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };
        // The class parameters and the range RM pins live past the call: the
        // arena holds the one, `pinned` the other.
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("OS descriptor {cmd:#x}: the host refused the call (errno {errno})");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // The caller's own values back where ours were.
        let mut out = a.reply(top)[..outer.len()].to_vec();
        if let Some(n) = class_params {
            out.extend_from_slice(&a.reply(n));
        }
        drop(a);
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
            //
            // A root class (NV01_ROOT and its kin) makes a new *client*
            // named hObjectNew, and RM ignores hRoot: nothing of `c` was
            // made, and forgetting `(c, o)` would release a registration RM
            // still pins (review 2026-09-29 1.3).
            NV_ESC_RM_ALLOC
                if r(40) == Some(0)
                    && r(12).is_some_and(|c| crate::semsurf::ROOT_CLASSES.contains(&c)) => {}
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
            let mut a = Arena::new();
            let top = a.small(&p);
            let rc = match self.host_call(&mut a, host_fd, request, top) {
                Ok(r) => r,
                Err(e) => -e,
            };
            let status = u32::from_le_bytes(a.bytes(top)[12..16].try_into().unwrap());
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
        let (base, len) = (rd64(0).unwrap_or(0), rd64(8).unwrap_or(0));
        let file = self.current_handle;
        self.osdesc
            .uvm_admit(file, self.handles.owner(file), client, memory, base, len)?;
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
            base,
            len,
            gpus,
            client,
            memory,
            // rmStatus, after rmCtrlFd, hClient and hMemory.
            status_at: fd_off + 12,
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
                // Held when UVM made it: NV_OK, or a failure of its wait for
                // the page-table writes, after which it leaves every mapping
                // up -- a channel's RC or ECC error, or the GPU gone
                // (uvm_map_external_allocation, uvm_channel_get_status).
                // Every other failure is answered before anything is mapped,
                // or after tearing down what the call made; and with no
                // answer at all the call never reached UVM. A mapping held
                // that was never made lasts until its range is freed or its
                // file closes (it lies in a recorded range, uvm_admit): late,
                // not early.
                const NV_ERR_ECC_ERROR: u32 = 0x0b;
                const NV_ERR_GPU_IS_LOST: u32 = 0x0f;
                const NV_ERR_RC_ERROR: u32 = 0x60;
                if let Some(0 | NV_ERR_ECC_ERROR | NV_ERR_GPU_IS_LOST | NV_ERR_RC_ERROR) =
                    status(m.status_at)
                {
                    let owner = self.handles.owner(handle);
                    self.osdesc
                        .uvm_mapped(handle, owner, m.base, m.len, &m.gpus, m.client, m.memory);
                }
            }
            UVM_CREATE_EXTERNAL_RANGE if size >= 24 && status(size - 8) == Some(0) => {
                let owner = self.handles.owner(handle);
                self.osdesc
                    .uvm_range_made_by(handle, word(0), word(8), owner);
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
        // Exactly the block UVM copies each way for `cmd` on this release;
        // none of the commands called here holds a pointer.
        let mut a = Arena::new();
        let top = a.small(&b[..size]);
        let rc = self.host_call(&mut a, host_fd, u64::from(cmd), top);
        rc == Ok(0) && a.bytes(top)[size - 8..size - 4] == [0; 4]
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
        plan: &crate::guestptr::Plan<'_>,
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

        let n_in = param_in.len();
        let mut a = Arena::new();
        let top = match self.top_block(&mut a, request, param_in, plan) {
            Ok(t) => t,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };

        // NV_ESC_SYS_PARAMS and NV_ESC_CHECK_VERSION_STR go as the guest
        // sent them, and their answers come back as the host gave them
        // (SECURITY.md §17). SYS_PARAMS carries the caller's memory block
        // size, which RM keeps from its first caller and answers EBUSY for
        // any other; this once rewrote the block and made up a success. And
        // CHECK_VERSION_STR was rewritten to query mode ('2'), in which RM
        // skips comparing the caller's version with its own: a guest
        // userspace of another release then ran against this RM unnoticed.
        let rc = self.host_call(&mut a, host_fd, request, top);
        if let Err(errno) = rc {
            log::debug!("ioctl(0x{request:x}/0x{escape:02x}) failed: errno={errno}");
            let back = a.reply(top)[..n_in].to_vec();
            return self.write_failed_resp(resp_buf, cookie, &back, errno);
        }
        // The host's bytes, with the caller's values in every field the plan
        // declared; only what the guest sent goes back.
        let param_buf = a.reply(top)[..n_in].to_vec();
        drop(a);
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    // ------------------------------------------------------------------
    // FD-carrying ioctl
    // ------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn dispatch_fd_carrying(
        &self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        escape: u32,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
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

        let embedded = i32::from_le_bytes(param_in[fd_offset..fd_offset + 4].try_into().unwrap());

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
        let host_embedded = if embedded == -1 {
            None
        } else {
            match self.dev_fd(embedded as u32) {
                Ok(fd) => Some(fd),
                Err(_) => {
                    log::warn!("fd-carrying ioctl: bad embedded handle {}", embedded as u32);
                    return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
                }
            }
        };

        // The host's copy: the guest's block, the descriptor field declared
        // and holding our descriptor of the handle's file (or -1), and the
        // plan's fields.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, plan)
            .and_then(|top| {
                a.fd(top, fd_offset, 4)?;
                match host_embedded {
                    Some(fd) => a.set_fd(top, fd_offset, fd)?,
                    None => a.set_no_fd(top, fd_offset, -1)?,
                }
                Ok(top)
            });
        let top = match built {
            Ok(t) => t,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            if embedded == -1 {
                log::warn!("ioctl(0x{request:x}) with no embedded fd failed: errno={errno}");
            } else {
                log::warn!("fd-carrying ioctl(0x{:x}) failed: errno={}", request, errno);
            }
            let back = a.reply(top)[..param_in.len()].to_vec();
            return self.write_failed_resp(resp_buf, cookie, &back, errno);
        }

        // The guest's handle back in place of our descriptor. It is not
        // what user mode wrote -- that was its own descriptor, which never
        // reached us -- so the guest driver writes the caller's value over it
        // on the way out (nvgpu_ioctl_translate_fd); RM never writes the
        // field (escape.c:393-428, 584-624), and callers read it back.
        self.write_ioctl_resp(resp_buf, cookie, &a.reply(top)[..param_in.len()])
    }

    fn dispatch_update_device_mapping_info(
        &mut self,
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

        // Where the library mapped what RM_MAP_MEMORY armed (`pNew`), told
        // to RM by the address it knew the mapping by until now (`pOld`):
        // the window offset the map returned, or the address an earlier
        // UPDATE gave it. RM moves its record of the mapping from one to
        // the other (osapi.c RmUpdateDeviceMappingInfo), and the library
        // names the mapping by `pNew` from then on -- UNMAP_MEMORY included.
        //
        // The mapping is found by the object, the process and `pOld`
        // together (`MmapContext::find`): by the object alone, two mappings
        // of it were one, and whichever came first was the one moved. If
        // `pOld` names none of the caller's, and it has exactly one mapping
        // of the object, that one is meant, as this backend always assumed.
        let caller = self.current_owner;
        let key = self
            .active_maps
            .find(h_client, h_memory, caller, old_cpu_addr)
            .or_else(|| {
                let k = self.active_maps.only_mapping_of(h_client, h_memory, caller);
                if k.is_some() {
                    log::debug!(
                        "UPDATE_DEVICE_MAPPING_INFO: pOld {old_cpu_addr:#x} names no mapping of \
                         client {h_client:#x} memory {h_memory:#x}; taking its only one"
                    );
                }
                k
            });

        // The host is handed its own address for the mapping, as both pOld
        // and pNew: its record stays where the backend's mapping is, which
        // did not move. A mapping we have no record of gets 0, not the
        // guest's value: the host takes both as addresses in this process
        // (guestptr.rs), and RM finds no mapping at 0.
        let host_old = key
            .and_then(|k| self.active_maps.find_by_offset(k))
            .map_or(0, |e| e.host_p_linear_address);
        if key.is_some() {
            log::debug!(
                "UPDATE_DEVICE_MAPPING_INFO: translated old {:#x} → host {:#x}",
                old_cpu_addr,
                host_old
            );
        }

        // pOldCpuAddress and pNewCpuAddress: the host VA (the host mapping
        // didn't move), and the caller's own in the reply. RM only reads
        // pOld/pNew (escape.c:857-876 takes them into locals and writes
        // nothing but `status`), and nvidia.ko copies the whole argument back
        // (nv.c:2834), so a native caller reads back what it passed -- not
        // zero, and never the host VA we put there for the call.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
            .and_then(|top| {
                for off in [16, 24] {
                    a.value(top, off, 8, Restore::Yes)?;
                    a.set_value(top, off, host_old)?;
                }
                Ok(top)
            });
        let top = match built {
            Ok(t) => t,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!(
                "UPDATE_DEVICE_MAPPING_INFO: host ioctl failed: errno={}",
                errno
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        let param_buf = a.reply(top)[..param_in.len()].to_vec();
        let status = u32::from_le_bytes(param_buf[32..36].try_into().unwrap());
        log::debug!("UPDATE_DEVICE_MAPPING_INFO: host status=0x{:x}", status);
        // What RM would now know the mapping by, once it has said yes.
        if status == 0
            && let Some(k) = key
        {
            self.active_maps.set_guest_va(k, new_cpu_addr);
        }
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    fn dispatch_map_memory(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
        resp_buf: &mut [u8],
    ) -> usize {
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

        let host_map = match self.dev_fd(guest_fd_handle) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: bad embedded FD handle {}",
                    guest_fd_handle
                );
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };
        let host_map_fd = host_map.as_raw_fd();

        // A host fd is single-use for mapping. The host will refuse this with
        // NV_ERR_STATE_IN_USE; say so here, because that status on its own
        // sends you looking at the unmap path, which is not the problem.
        if self.active_maps.fd_has_mapping(guest_fd_handle) {
            log::warn!(
                "NV_ESC_RM_MAP_MEMORY: handle {guest_fd_handle} already carries a mapping; \
                 a host fd cannot carry two, so the host will return NV_ERR_STATE_IN_USE"
            );
        }

        // The host's copy: the guest's NVOS33, the descriptor field holding
        // our descriptor of that file, pLinearAddress (the plan's) held at 0.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, plan)
            .and_then(|top| {
                a.fd(top, FD_OFFSET, 4)?;
                a.set_fd(top, FD_OFFSET, host_map)?;
                Ok(top)
            });
        let top = match built {
            Ok(t) => t,
            Err(e) => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e),
        };

        // --- Step 2: reserve the window extent, before RM maps anything ---
        //
        // A mapping is one transaction: the extent, the host's mapping and
        // the placement all happen, or none does. The extent is the part
        // that can be refused -- the zone is full, or this process holds its
        // share of it (quota.rs) -- so it comes first, and a refusal leaves
        // the host with nothing mapped. It used to come after RM, and each
        // refusal left a host mapping made, recorded nowhere, until the
        // memory was freed.
        //
        // Charged to whoever opened the file the mapping is armed on: the
        // process that will map it. The zone is the one the mapping will
        // most likely need (`rm_mapping_pgprot_before`); RM's reply decides,
        // and a reply that wants another zone moves the reservation there.
        let owner = match self.handles.owner(guest_fd_handle) {
            crate::quota::Owner::Unknown => self.current_owner,
            o => o,
        };
        let h_client = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
        let asked = u64::from_le_bytes(
            param_in[LENGTH_OFFSET..LENGTH_OFFSET + 8]
                .try_into()
                .unwrap(),
        );
        let expected = self.rm_mapping_pgprot_before(h_client, h_memory);
        // A zero length can be given no extent; RM refuses it on its own,
        // and says so in `status`.
        //
        // A length no zone could hold -- up to u64::MAX, the guest's to
        // choose -- is refused before any arithmetic on it. Either refusal
        // is RM's own out-of-memory answer, NV_ERR_NO_MEMORY in the status
        // with the ioctl succeeding, which is how libnvidia learns it ran
        // out; a bare errno reads as a generic OS failure.
        let reserved = if asked == 0 {
            None
        } else {
            let got = if asked > self.shm.largest_zone() {
                Err(DeviceError::Io(std::io::Error::other(format!(
                    "{asked:#x} bytes is more than any zone holds"
                ))))
            } else {
                self.alloc_zone(asked, expected, owner)
            };
            match got {
                Ok(r) => Some(r),
                Err(e) => {
                    log::warn!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                    let out = crate::rmshare::refusal(param_in, STATUS_OFFSET, NV_ERR_NO_MEMORY);
                    return self.write_ioctl_resp(resp_buf, cookie, &out);
                }
            }
        };
        // Give the reservation back, on a path where it will not be used.
        let unreserve = |be: &mut Self, r: Option<crate::shm::ShmRegion>| {
            if let Some(r) = r
                && let Err(e) = be.shm.free(&r)
            {
                log::warn!("NV_ESC_RM_MAP_MEMORY: freeing the unused region: {e}");
            }
        };

        // --- Step 3: Call host ioctl ---

        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("NV_ESC_RM_MAP_MEMORY: host ioctl failed: errno={}", errno);
            unreserve(self, reserved);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // --- Step 4: Check RM status and read updated fields ---

        // The guest handle back in the descriptor field, regardless of status.
        let mut param_buf = a.reply(top)[..param_in.len()].to_vec();
        drop(a);
        let rm_status = u32::from_le_bytes(
            param_buf[STATUS_OFFSET..STATUS_OFFSET + 4]
                .try_into()
                .unwrap(),
        );

        if rm_status != 0 {
            // RM returned an error status (NV_OK == 0).
            // Forward the params back so the guest can read the status field.
            log::debug!("NV_ESC_RM_MAP_MEMORY: RM status 0x{:x}", rm_status);
            unreserve(self, reserved);
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        // NVOS33.pLinearAddress: the host's own address for the mapping,
        // which RM knows it by (in UPDATE_DEVICE_MAPPING_INFO and UNMAP), and
        // which undoes it if what follows fails.
        let host_p_linear = u64::from_le_bytes(param_buf[32..40].try_into().unwrap());

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

        // --- Step 5: the memory type the host maps it with ---
        //
        // Not the caching type alone: that is real only for video memory (see
        // `rm_mapping_pgprot`), and reading it for everything put system
        // memory and the doorbell registers write-combining.
        let pgprot = self.rm_mapping_pgprot(flags, h_client, h_memory);
        // And whether it can be written at all: RM makes some mappings
        // read-only, and placing one writable would let a guest write stop the
        // VM (see `shm::host_mapping_writable`).
        let writable = crate::shm::host_mapping_writable(host_map_fd, length, 0);
        if !writable {
            log::debug!(
                "NV_ESC_RM_MAP_MEMORY: client {h_client:#x} memory {h_memory:#x} is read-only \
                 on the host; placed read-only"
            );
        }

        // --- Step 6: the extent RM's answer calls for ---
        //
        // Nearly always the reservation. When RM mapped the memory with
        // another type than expected (or, never seen, another length), the
        // extent moves to that type's zone; if it does not fit there, the
        // host's mapping is undone and nothing is left of the call.
        let region = match reserved {
            Some(r) if pgprot == expected && length == asked => r,
            other => {
                unreserve(self, other);
                match self.alloc_zone(length, pgprot, owner) {
                    Ok(r) => r,
                    Err(e) => {
                        log::error!("NV_ESC_RM_MAP_MEMORY: SHM alloc failed: {}", e);
                        self.undo_rm_map(host_fd, &param_buf, host_p_linear);
                        return self.write_error_resp(
                            resp_buf,
                            Status::IoctlFailed,
                            cookie,
                            libc::ENOMEM,
                        );
                    }
                }
            }
        };

        // --- Step 7: place the device fd in the window ---
        //
        // Placed by the transport, not here: see `WindowPlacer`. Without one
        // the mapping exists on the host and is unreachable from the guest, so
        // the honest answer is to fail the call rather than return an address
        // that names nothing -- and to undo the host's mapping with it.
        let placed = match self.window.as_ref() {
            None => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: no shared window, so device memory cannot be \
                     addressed by the guest"
                );
                Err(libc::ENOTSUP)
            }
            Some(window) => window
                .place(region.offset, length, host_map_fd, 0, writable)
                .map_err(|e| {
                    log::error!("NV_ESC_RM_MAP_MEMORY: placing in the window failed: {}", e);
                    libc::ENOMEM
                }),
        };
        if let Err(errno) = placed {
            unreserve(self, Some(region));
            self.undo_rm_map(host_fd, &param_buf, host_p_linear);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        log::debug!(
            "dispatch_map_memory: returning shm_offset=0x{:x} shm_length=0x{:x} pgprot={:?}{}",
            region.offset,
            length,
            region.pgprot,
            if writable { "" } else { " read-only" }
        );

        // --- Step 8: record the mapping ---
        //
        // The host wrote its own address into pLinearAddress; it is kept for
        // UPDATE_DEVICE_MAPPING_INFO and UNMAP, and the guest is given the
        // window offset in its place. The guest library stores that value
        // and quotes it back until it tells RM where it mapped the file
        // (UPDATE_DEVICE_MAPPING_INFO), and by that address afterwards; the
        // entry answers to both (`MmapContext::find`).
        //
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
                caller: self.current_owner,
                guest_va: None,
            },
        );

        // Replace host VA with SHM offset in pLinearAddress — this is what
        // the guest sees. It's not a real pointer; the guest driver uses the
        // SHM metadata (shm_offset/shm_length/pgprot in IoctlResp) for mmap,
        // and the library stores this value to pass back at unmap time.
        param_buf[32..40].copy_from_slice(&region_offset.to_le_bytes());

        // --- Step 9: Respond ---
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

    /// Undo an RM_MAP_MEMORY RM said yes to, when the backend cannot give
    /// the guest what it mapped (no extent in the zone RM's answer calls
    /// for, or no placement): NV_ESC_RM_UNMAP_MEMORY of the same object at
    /// the host's own address, on the same file, as the guest's own unmap
    /// would be sent. `nvos33` is the reply as RM left it.
    ///
    /// The descriptor the mapping was armed on stays spent (a host fd
    /// carries one mapping in its life, `repeated_map_unmap_does_not_
    /// exhaust_the_zone`); the guest closes it, as it would after any
    /// failed map.
    fn undo_rm_map(&self, host_fd: RawFd, nvos33: &[u8], host_p_linear: u64) {
        // NVOS34 {hClient, hDevice, hMemory, pad, pLinearAddress, status,
        // flags}: the object from the map, flags 0 (a user mapping).
        let mut p = [0u8; 32];
        p[0..12].copy_from_slice(&nvos33[0..12]);
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_UNMAP_MEMORY, p.len() as u32);
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, &p, &crate::guestptr::Plan::default())
            .and_then(|top| {
                a.value(top, 16, 8, Restore::No)?;
                a.set_value(top, 16, host_p_linear)?;
                Ok(top)
            });
        let status = built.and_then(|top| {
            self.host_call(&mut a, host_fd, request, top)?;
            Ok(u32::from_le_bytes(a.reply(top)[24..28].try_into().unwrap()))
        });
        match status {
            Ok(0) => log::info!(
                "NV_ESC_RM_MAP_MEMORY: undone on the host (client {:#x} memory {:#x})",
                u32::from_le_bytes(p[0..4].try_into().unwrap()),
                u32::from_le_bytes(p[8..12].try_into().unwrap()),
            ),
            Ok(s) => log::warn!(
                "NV_ESC_RM_MAP_MEMORY: the host would not undo the mapping (RM status {s:#x}); \
                 it lasts until the memory is freed"
            ),
            Err(errno) => log::warn!(
                "NV_ESC_RM_MAP_MEMORY: undoing the mapping on the host failed (errno {errno}); \
                 it lasts until the memory is freed"
            ),
        }
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

        // guest_linear is what the caller knows the mapping by: the window
        // offset the map wrote into pLinearAddress, or the address an
        // UPDATE_DEVICE_MAPPING_INFO moved it to (`MmapContext::find`), for
        // this object and this process only.
        let found = self
            .active_maps
            .find(h_client, h_memory, self.current_owner, guest_linear)
            .and_then(|k| self.active_maps.remove(k));
        let entry = match found {
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
                // finds none at 0 and says so in `status`; the caller reads
                // back its own value.
                let mut a = Arena::new();
                let built = self
                    .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
                    .and_then(|top| a.value(top, 16, 8, Restore::Yes).map(|_| top));
                let top = match built {
                    Ok(t) => t,
                    Err(e) => {
                        return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e);
                    }
                };
                if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                return self.write_ioctl_resp(resp_buf, cookie, &a.reply(top)[..param_in.len()]);
            }
        };

        log::debug!(
            "UNMAP_MEMORY: {:#x} (shm_off={:#x}) → host_va={:#x} (client={:#x}, mem={:#x})",
            guest_linear,
            entry.region.offset,
            entry.host_p_linear_address,
            h_client,
            h_memory
        );

        // The real host pLinearAddress for the host ioctl, a value of ours.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, &crate::guestptr::Plan::default())
            .and_then(|top| {
                // RM does not write NVOS34.pLinearAddress and nvidia.ko
                // copies the block back, so the caller reads its own value,
                // as on the not-found path and as L-6 did for
                // UPDATE_DEVICE_MAPPING_INFO (review 2026-09-29 2.4).
                a.value(top, 16, 8, Restore::Yes)?;
                a.set_value(top, 16, entry.host_p_linear_address)?;
                Ok(top)
            });
        let top = match built {
            Ok(t) => t,
            Err(e) => {
                self.active_maps.insert(entry.region.offset, entry);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, e);
            }
        };
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("UNMAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore the entry since unmap didn't happen
            self.active_maps.insert(entry.region.offset, entry);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        let param_buf = a.reply(top)[..param_in.len()].to_vec();
        drop(a);

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
            self.active_maps.insert(entry.region.offset, entry);
        }

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

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
            // UVM's own answer to a command it has no route for (uvm.c,
            // uvm_test_ioctl), not a permission (parity #31).
            return Err(libc::ENOSYS);
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
        let mut a = Arena::new();
        let top = a.small(&[0u8; 16]);
        let rc = match self.host_call(
            &mut a,
            host_fd,
            crate::guestptr::UVM_PAGEABLE_MEM_ACCESS as u64,
            top,
        ) {
            Ok(r) => r,
            Err(e) => -e,
        };
        let q = a.bytes(top).to_vec();
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

    /// The handle in UVM command `cmd`'s descriptor field (uvmfd.rs), in
    /// `params` (the guest's block): `Some((offset, handle))` when it names
    /// one of ours of the kind the field names, which the host is then
    /// handed as our descriptor of it (and the reply the handle again). A
    /// negative value is UVM's "none" and stays. Anything that is not a
    /// handle of the kind the field names -- an RM control file, or a UVM
    /// file -- is refused rather than handed to the host as a number in our
    /// table.
    fn uvm_fd_in(&self, cmd: u32, params: &[u8]) -> std::result::Result<Option<(usize, u32)>, i32> {
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
            Some((_, kind)) if kind == want => Ok(Some((off, handle))),
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

    /// The RM client a UVM command names beside its `rmCtrlFd`: `hClient`,
    /// the word after the descriptor in REGISTER_GPU, REGISTER_GPU_VASPACE,
    /// REGISTER_CHANNEL, MAP_EXTERNAL_ALLOCATION and ALLOC_DEVICE_P2P
    /// (uvm_ioctl.h), with the object UVM takes from it after that.
    ///
    /// UVM hands that pair to RM from a kernel client of its own
    /// (nvUvmInterfaceDupMemory, DupAddressSpace, RetainChannel), and RM's
    /// check is the PID share policy: the source client's process must be
    /// the *calling* one (cliresShareCallback, client_resource.c:219-226,
    /// for a kernel destination). Natively that keeps one process's GPU
    /// memory, VA spaces and channels out of another's UVM. Here every
    /// client of the VM is the backend's process, so RM's check passes for
    /// any of them, and one guest process could map another's memory into
    /// its own UVM VA space. UVM does not look at `rmCtrlFd` yet (the "Bug
    /// 1624521" TODOs, uvm_va_space.c:1557); the backend does what that
    /// TODO describes: the client must be one this VM allocated on the very
    /// control file `rmCtrlFd` names, which the guest driver translated from
    /// the caller's own descriptor. So the caller holds the file the client
    /// was made on -- the file RM's strict client validation keys every
    /// other use of that client to (§11, R3). A zero client names nothing
    /// (REGISTER_GPU without a partition sends -1 and 0), and passes.
    fn uvm_client_ok(&self, cmd: u32, params: &[u8]) -> std::result::Result<(), i32> {
        let Some(field) = crate::uvmfd::field(self.driver, cmd) else {
            return Ok(());
        };
        if field.of != crate::uvmfd::FdOf::RmCtl {
            return Ok(());
        }
        let off = field.offset as usize;
        let rd32 = |o: usize| {
            params
                .get(o..o + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        let (Some(fd), Some(client)) = (rd32(off), rd32(off + 4)) else {
            log::warn!("UVM command {cmd}: too short for its client at {}", off + 4);
            return Err(libc::EINVAL);
        };
        if client == 0 {
            return Ok(());
        }
        let issuer = self.semsurf.issuer_of(client);
        if (fd as i32) >= 0 && issuer == Some(fd) {
            return Ok(());
        }
        log::warn!(
            "UVM command {cmd} names RM client {client:#x}, which {}; refused",
            match issuer {
                None => "is not this VM's".to_string(),
                Some(h) if (fd as i32) < 0 => {
                    format!("was made on handle {h}, and no control file is named")
                }
                Some(h) => format!("was made on handle {h}, not on the named handle {fd}"),
            }
        );
        Err(libc::EPERM)
    }

    /// The descriptor of `handle` when it is a control file
    /// (`/dev/nvidiactl`), the only kind RM's export and import controls
    /// resolve (`nv_get_file_private(fd, NV_TRUE, ..)`).
    fn ctl_fd(&self, handle: u32) -> Result<BorrowedFd<'_>> {
        match self.handles.get(handle) {
            Some((fd, HandleKind::Dev(DeviceKind::Ctl))) => Ok(fd),
            _ => Err(DeviceError::BadHandle(handle as u64)),
        }
    }

    fn dev_fd(&self, handle: u32) -> Result<BorrowedFd<'_>> {
        match self.handles.get(handle) {
            Some((fd, HandleKind::Dev(_))) => Ok(fd),
            _ => Err(DeviceError::BadHandle(handle as u64)),
        }
    }

    /// The one gate every export of a guest GEM object asks
    /// (exportgate.rs): HOST_OP PRIME_EXPORT and the Wayland dma-buf path
    /// here, the IOCTL2 re-home through its hooks.
    pub(crate) fn export_gate(&self) -> crate::exportgate::ExportGate<'_> {
        crate::exportgate::ExportGate {
            semsurf: &self.semsurf,
            injected: Some(&self.inject),
            taint: &self.inject_taint,
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
        self.write_ioctl_resp_status(resp_buf, cookie, param_out, deep_len, 0)
    }

    /// A host call that failed with `errno`, answered as nvidia.ko answers
    /// it: the errno, and the argument block as the host left it, which
    /// nv.c copies out on every error but EFAULT (nv.c:2869-2878) and
    /// drm_ioctl unconditionally. So a caller reads what RM wrote before
    /// failing -- CHECK_VERSION_STR's reply word and RM's own version
    /// string on a mismatch, which libnvidia prints (review 2026-09-29 2.1,
    /// parity #23). The guest copies bytes that come with a negative
    /// status (nvgpu_rmio.c).
    fn write_failed_resp(
        &self,
        resp_buf: &mut [u8],
        cookie: u64,
        param_out: &[u8],
        errno: i32,
    ) -> usize {
        let errno = errno.saturating_abs();
        if errno == 0 || errno == libc::EFAULT || param_out.is_empty() {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }
        self.write_ioctl_resp_status(resp_buf, cookie, param_out, 0, -errno)
    }

    fn write_ioctl_resp_status(
        &self,
        resp_buf: &mut [u8],
        cookie: u64,
        param_out: &[u8],
        deep_len: usize,
        status: i32,
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
                status,
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
            errno.saturating_abs()
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
        #[cfg(test)]
        crate::fuzz_seeds::dropped(self);
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

fn read_struct<T: crate::sys::pod::Pod + Copy>(buf: &[u8], offset: usize) -> T {
    crate::sys::pod::read(buf, offset).expect("a buffer long enough for the struct")
}

fn write_struct<T: crate::sys::pod::Pod>(buf: &mut [u8], val: &T) -> usize {
    crate::sys::pod::write(buf, 0, val).expect("a buffer long enough for the struct")
}

// ============================================================
// Tests
// ============================================================

#[cfg(test)]
mod abi_tests {
    use super::*;
    use abi::ioctl::*;

    /// The exact reply the Tesla T4 gave to NV_ESC_CHECK_VERSION_STR on driver
    fn backend() -> NvidiaBackend {
        NvidiaBackend::with_default_zones()
    }

    #[test]
    fn no_profile_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn a_measured_version_selects_a_profile() {
        let mut b = backend();
        b.set_host_driver_version("580.178.04");
        assert_eq!(
            b.driver,
            Some(abi::version::DriverVersion::new(580, 178, 4))
        );
        assert!(b.abi.is_some(), "580.178.04 must select a profile");
    }

    /// The version is the host's, never the guest's: a CHECK_VERSION_STR
    /// reply, whose string RM leaves as the caller sent it, teaches nothing,
    /// and with no version set an RM escape is refused, not forwarded
    /// unchecked (review 2026-09-29 1.15).
    #[test]
    fn with_no_host_version_no_rm_escape_passes_and_none_teaches_one() {
        let mut b = backend();
        b.unversioned_for_test = false;
        b.set_host_ioctl_for_test(|_, _, arg| {
            let (a, _) = arg.split();
            a[4] = 1;
            a[8..18].copy_from_slice(b"580.178.04");
            0
        });
        let ctl = b.adopt_for_test(
            std::fs::File::open("/dev/null").unwrap().into(),
            HandleKind::Dev(DeviceKind::Ctl),
        );
        let mut req = Vec::new();
        for v in [MsgType::Ioctl as u32, ctl, 0, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        let cmd = abi::ioctl::_IOWR(NV_ESC_CHECK_VERSION_STR, 72) as u32;
        for v in [cmd, 72, 72, 0, 0, 0] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(&[0u8; 72]);
        let mut resp = vec![0u8; 256];
        b.dispatch(&req, &mut resp);
        assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, -libc::EINVAL);
        assert!(b.driver.is_none());
    }

    /// The property the tables exist for: an escape nobody described does not
    /// reach the host driver. This is the check that was a log line until the
    /// question was asked in public.
    #[test]
    fn an_escape_outside_the_profile_is_refused() {
        let mut b = backend();
        b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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
        b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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

    /// With no version there is no profile to check against (and outside
    /// the tests' fake RMs, `dispatch` refuses every RM escape then).
    #[test]
    fn nothing_is_checked_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn accepts_the_sizes_the_t4_actually_sent() {
        let mut b = backend();
        b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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
        b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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
        b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
        // CARD_INFO is an array; the T4 sent 2304 bytes in one call.
        assert_eq!(
            b.check_abi(NV_ESC_CARD_INFO, 2304),
            AbiCheck::VariableLength
        );
    }

    #[test]
    fn a_garbled_version_string_leaves_the_backend_unconfigured() {
        let mut b = backend();
        b.set_host_driver_version("oops");
        assert!(b.driver.is_none());
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
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

    fn append<T: crate::sys::pod::Pod>(v: &mut Vec<u8>, val: &T) {
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
        be.dispatch(&[0u8; 4], &mut [0u8; 32]);
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

    /// What the backend adds to an RM control, on the real driver: the same
    /// cheap control (GPU_GET_ATTACHED_IDS on a root client) served through
    /// `dispatch` and issued directly, each timed over many calls. A timing,
    /// not a check: `cargo test --release -p device bench_rm_control_service
    /// -- --ignored --nocapture` (BENCHMARKS.md).
    #[test]
    #[ignore]
    fn bench_rm_control_service() {
        if !nvidiactl_present() {
            return;
        }
        let mut be = NvidiaBackend::for_test();
        let mut oresp = vec![0u8; 64];
        be.dispatch(&open_msg(DeviceKind::Ctl), &mut oresp);
        let ctl = opened_handle(&oresp);
        assert!(ctl > 0);
        let ioctl = |be: &mut NvidiaBackend, nr: u32, params: &[u8], nested: &[u8], at: u32| {
            let mut req = hdr(MsgType::Ioctl, ctl);
            append(
                &mut req,
                &IoctlReq {
                    cmd: abi::ioctl::_IOWR(nr, params.len() as u32) as u32,
                    data_len: params.len() as u32,
                    nested_offset: at,
                    nested_len: nested.len() as u32,
                    deep_ptr_offset: 0,
                    deep_len: 0,
                },
            );
            req.extend_from_slice(params);
            req.extend_from_slice(nested);
            let mut resp = vec![0u8; 1024];
            be.dispatch(&req, &mut resp);
            resp
        };
        // RM_ALLOC of a root client: NVOS64 with no parameters.
        let mut alloc = [0u8; 48];
        alloc[12..16].copy_from_slice(&0x41u32.to_le_bytes());
        let r = ioctl(&mut be, abi::ioctl::NV_ESC_RM_ALLOC, &alloc, &[], 0);
        assert_eq!(parse_resp(&r).status, 0);
        let body = &r[size_of::<MsgHeader>() + size_of::<IoctlResp>()..];
        let client = u32::from_le_bytes(body[8..12].try_into().unwrap());
        assert_eq!(u32::from_le_bytes(body[40..44].try_into().unwrap()), 0);
        // NVOS54: hClient, hObject, cmd, flags, params (at 16), paramsSize, status.
        let mut ctl54 = [0u8; 32];
        ctl54[0..4].copy_from_slice(&client.to_le_bytes());
        ctl54[4..8].copy_from_slice(&client.to_le_bytes());
        ctl54[8..12].copy_from_slice(&0x0000_0201u32.to_le_bytes());
        ctl54[24..28].copy_from_slice(&128u32.to_le_bytes());
        let ids = [0u8; 128];
        let n: u32 = std::env::var("NVGPU_BENCH_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(200_000);
        for _ in 0..1000 {
            ioctl(&mut be, abi::ioctl::NV_ESC_RM_CONTROL, &ctl54, &ids, 16);
        }
        let t0 = std::time::Instant::now();
        for _ in 0..n {
            let r = ioctl(&mut be, abi::ioctl::NV_ESC_RM_CONTROL, &ctl54, &ids, 16);
            debug_assert_eq!(parse_resp(&r).status, 0);
        }
        // Natively the call takes about 1.4 us (nvgpu-bench rm-ctl).
        eprintln!(
            "RM_CONTROL GPU_GET_ATTACHED_IDS served in {:?} a call",
            t0.elapsed() / n
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
    struct LocalWindow(Arc<crate::sys::mem::Window>);

    impl crate::shm::WindowPlacer for LocalWindow {
        fn place(&self, off: u64, len: u64, fd: RawFd, fo: u64, writable: bool) -> Result<()> {
            Ok(self.0.place(off, len, fd, fo, writable)?)
        }
        fn withdraw(&self, off: u64, len: u64) -> Result<()> {
            Ok(self.0.restore(off, len)?)
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
            be.set_window(Box::new(LocalWindow(be.shm_window())));
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
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&c| u32::from_le_bytes(c))
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
        let first = c.be.shm_window().read_u32(off);
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
        crate::sys::fd::open(c"/dev/null", libc::O_RDWR | libc::O_CLOEXEC).unwrap()
    }

    std::thread_local! {
        /// Commands the fake host driver was handed, on this test's thread.
        static FORWARDED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    /// A host driver that behaves like drm_ioctl and nvidia.ko: it writes
    /// back `_IOC_SIZE(request)` bytes, whatever the caller allocated.
    fn fake_ioctl(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        FORWARDED.with(|f| f.borrow_mut().push(request));
        // The argument holds at least _IOC_SIZE bytes (sys/block.rs).
        arg.bytes()[..hostfd::ioc_size(request as u32)].fill(0xaa);
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

    std::thread_local! {
        /// The command byte CHECK_VERSION_STR reached the fake host with.
        static VERSION_CMD: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
    }

    /// RM's SYS_PARAMS for a memory block size other than its first
    /// caller's (EBUSY), and its CHECK_VERSION_STR (the reply word set).
    fn fake_sys_params_busy(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        match (request & 0xff) as u32 {
            abi::ioctl::NV_ESC_SYS_PARAMS => -libc::EBUSY,
            abi::ioctl::NV_ESC_CHECK_VERSION_STR => {
                VERSION_CMD.with(|c| c.set(Some(arg.bytes()[0])));
                arg.bytes()[4] = 1;
                0
            }
            _ => 0,
        }
    }

    /// SYS_PARAMS's EBUSY reaches the guest as EBUSY, not a made-up
    /// success; CHECK_VERSION_STR reaches RM with the guest's own command,
    /// so RM compares the guest userspace's version with its own.
    #[test]
    fn sys_params_and_check_version_go_as_sent_and_come_back_as_answered() {
        use abi::ioctl::{NV_ESC_CHECK_VERSION_STR, NV_ESC_SYS_PARAMS};
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_sys_params_busy);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));

        let sys = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_SYS_PARAMS, 8);
        let resp = v1_ioctl(&mut be, ctl, sys, &(128u64 << 20).to_le_bytes());
        assert_eq!(parse_resp(&resp).status, -libc::EBUSY);

        let check = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_CHECK_VERSION_STR, 72);
        for cmd in [0u8, b'1'] {
            let mut p = vec![0u8; 72];
            p[0] = cmd;
            p[8..17].copy_from_slice(b"595.99.02");
            let resp = v1_ioctl(&mut be, ctl, check, &p);
            assert_eq!(parse_resp(&resp).status, 0);
            assert_eq!(VERSION_CMD.with(|c| c.take()), Some(cmd), "not rewritten");
            assert_eq!(resp[IOCTL_BODY + 4], 1, "the host's reply word");
        }
    }

    /// CHECK_VERSION_STR that RM fails: the guest reads RM's reply word
    /// and its version string with the errno, as nvidia.ko copies the block
    /// out on failure, and libnvidia can say which versions disagree
    /// (review 2026-09-29 2.1, parity #23). EFAULT copies nothing.
    #[test]
    fn a_failed_call_comes_back_with_the_block_as_the_host_left_it() {
        use abi::ioctl::NV_ESC_CHECK_VERSION_STR;
        fn mismatch(_: RawFd, _: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
            let b = arg.bytes();
            b[4] = 1;
            b[8..72].fill(0);
            b[8..17].copy_from_slice(b"595.99.02");
            -libc::EINVAL
        }
        fn fault(_: RawFd, _: u64, _: &mut crate::sys::block::Arg<'_>) -> i32 {
            -libc::EFAULT
        }
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(mismatch);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let check = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_CHECK_VERSION_STR, 72);
        let mut p = vec![0u8; 72];
        p[0] = b'1';
        p[8..17].copy_from_slice(b"580.95.05");
        let resp = v1_ioctl(&mut be, ctl, check, &p);
        assert_eq!(parse_resp(&resp).status, -libc::EINVAL);
        assert_eq!(resp[IOCTL_BODY + 4], 1, "RM's reply word");
        assert_eq!(
            &resp[IOCTL_BODY + 8..IOCTL_BODY + 17],
            b"595.99.02",
            "RM's version"
        );
        be.set_host_ioctl_for_test(fault);
        let resp = v1_ioctl(&mut be, ctl, check, &p);
        assert_eq!(parse_resp(&resp).status, -libc::EFAULT);
        assert_eq!(resp.len(), size_of::<MsgHeader>(), "a bare header");
    }

    /// The DRI section's count is of the records written: a count of every
    /// device over fewer records had the guest read the card section after
    /// them as DRI records (review 2026-09-29 2.6).
    #[test]
    fn a_truncated_dri_section_counts_only_what_it_holds() {
        let dev = |name: &str| DriDevice {
            name: name.into(),
            major: 226,
            minor: 128,
            slot_index: 0,
            dev_info: [0; NV_DEV_INFO_WORDS],
            dev_info_size: 0,
        };
        let one = 16 + 4 * NV_DEV_INFO_WORDS + "renderD128".len();
        let mut buf = vec![0u8; 4 + one + 8];
        let n = write_dri_section(&[dev("renderD128"), dev("renderD129")], &mut buf);
        assert_eq!(n, 4 + one);
        assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 1);
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
        // commands, unchecked. Each is refused with the device's own answer
        // to a command it does not know (parity #32).
        for (h, cmd, errno) in [
            (ctl, d, libc::EINVAL),
            (ctl, m, libc::EINVAL),
            (modeset, d, libc::ENOTTY),
            (uvm, m, libc::ENOSYS),
            (ctl, 0x3000_0001, libc::EINVAL),
        ] {
            let r = v1_ioctl(&mut be, h, cmd, &[0u8; 24]);
            assert_eq!(parse_resp(&r).status, -errno, "handle {h} cmd {cmd:#x}");
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

    /// An MMAP of a file already placed, asking for more than the placement
    /// holds, is refused: the guest maps what it asked for from the
    /// placement's offset, so the rest would be the window's next extents --
    /// another guest process's -- or unplaced window. The control file's
    /// ALLOC_MEMORY mappings take this path (nothing records them).
    #[test]
    fn a_second_mmap_larger_than_the_placement_is_refused() {
        let mut be = gated_backend();
        be.set_window(Box::new(FakeWindow::default()));
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let first = mmap(&mut be, ctl, 0);
        let ask = |be: &mut NvidiaBackend, size: u64| {
            let mut req = hdr(MsgType::Mmap, ctl as u64);
            append(
                &mut req,
                &MmapReq {
                    size,
                    offset: 0,
                    prot: 3,
                    padding: 0,
                },
            );
            let mut resp = vec![0u8; 64];
            be.dispatch(&req, &mut resp);
            (
                parse_resp(&resp).status,
                read_struct::<MmapResp>(&resp, size_of::<MsgHeader>()),
            )
        };
        let (st, _) = ask(&mut be, 64 << 20);
        assert_eq!(st, -libc::EINVAL, "past the placement");
        // The same size, or less, is the same placement.
        let (st, again) = ask(&mut be, 4096);
        assert_eq!(st, 0);
        assert_eq!(
            (again.guest_phys_addr, again.mapping_id, again.size),
            (first.guest_phys_addr, first.mapping_id, 4096)
        );
        let (st, _) = ask(&mut be, 100);
        assert_eq!(st, 0);
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

    /// A launcher's snapshot of the whole config space extends the 64 bytes
    /// an unprivileged read gets, only if it is of the same device (parity
    /// #14).
    #[test]
    fn a_pci_config_snapshot_extends_the_live_header_of_its_own_device() {
        let mut live = vec![0u8; 64];
        live[0..4].copy_from_slice(&[0xde, 0x10, 0x85, 0x2b]);
        live[8..12].copy_from_slice(&[0xa1, 0, 0, 3]);
        live[0x2c..0x30].copy_from_slice(&[0x43, 0x10, 0x11, 0x22]);
        live[0x34] = 0x60;
        let mut snap = live.clone();
        snap[4] = 0xff; // a status bit that moved since: the live one wins
        snap.resize(4096, 0);
        snap[0x60] = 0x10; // the PCIe capability
        snap[0x100] = 0x01; // an extended capability
        let m = merge_pci_config(&live, &snap).unwrap();
        assert_eq!(m.len(), 4096);
        assert_eq!(&m[..64], &live[..]);
        assert_eq!((m[0x60], m[0x100]), (0x10, 0x01));
        let mut other = snap.clone();
        other[2] = 0x86;
        assert_eq!(merge_pci_config(&live, &other), None, "another device");
        assert_eq!(merge_pci_config(&live, &snap[..64]), None, "nothing more");
        assert_eq!(merge_pci_config(&live, &vec![0u8; 8192]), None, "too long");
        assert_eq!(
            merge_pci_config(&snap, &snap),
            None,
            "the live read was whole"
        );
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
    fn fake_rm_status(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let a = &mut arg.bytes()[..hostfd::ioc_size(request as u32)];
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

    fn fake_uvm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        if request == 75 {
            // MM_INITIALIZE's 8 bytes.
            let v = i32::from_le_bytes(arg.bytes()[..4].try_into().unwrap());
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
        assert_eq!(parse_resp(&r).status, -libc::ENOSYS, "UVM's own answer");
        // And a host with no table refuses them all.
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_uvm);
        let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        let r = v1_ioctl(&mut be, uvm, 34, &[0u8; 16]);
        assert_eq!(parse_resp(&r).status, -libc::ENOSYS);
    }

    /// S-33: a display file's handle-table descriptor is closed by the
    /// closer, not by the queue thread in CLOSE.
    #[test]
    fn closing_a_card_handle_leaves_the_last_close_to_the_closer() {
        let mut be = gated_backend();
        let (r, w) = crate::sys::fd::pipe2(libc::O_CLOEXEC).unwrap();
        let card = be.adopt_for_test(r, HandleKind::DrmCard(0));
        be.close_handle(card).unwrap();
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert!(
            crate::sys::fd::write(&w, b"x").is_err(),
            "the read end is closed"
        );
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
    /// not: RM does not write it and nvidia.ko copies the block back
    /// (review 2026-09-29 2.4). The found path zeroed it.
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

        // RM's own out-of-memory answer, in the status (#29).
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
    /// page overflowed, and the release profile aborted the backend for
    /// every process of the VM (review 2026-09-29 1.1). The same for an
    /// MMAP's size (1.10).
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

        let (status, _, _) = e.map_as(ctl, unknown, UCVID);
        assert_ne!(status, 0, "no uncached extent left");
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
        let (status, _, _) = e.map_as(ctl, p(1), VIDMEM);
        PLACE_FAILS.with(|f| f.set(false));
        assert_ne!(status, 0);
        assert!(host_maps().is_empty(), "the host's mapping was undone");
        assert_eq!(e.be.shm_free_bytes(), empty);
        assert_eq!(
            e.be.shm.held_by(crate::shm::PgprotKind::WriteCombine, p(1)),
            0
        );
        assert!(e.be.active_maps.is_empty());
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
    fn fake_rm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let len = hostfd::ioc_size(request as u32);
        let (b, others) = arg.split();
        let b = &mut b[..len];
        if (request & 0xff) as u32 == NV_ESC_RM_CONTROL {
            let cmd = u32::from_le_bytes(b[8..12].try_into().unwrap());
            let p = u64::from_le_bytes(b[16..24].try_into().unwrap());
            if let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd) {
                // The backend pointed pParams at its own copy of the
                // parameters, which hold the field (checked before the call).
                let fd = others.peek(p + at as u64, 4) as u32 as i32;
                SEEN.with(|s| s.borrow_mut().push((cmd, fd)));
            } else {
                SEEN.with(|s| s.borrow_mut().push((cmd, i32::MIN)));
            }
            b[28..32].copy_from_slice(&0u32.to_le_bytes());
        } else if (request & 0xff) as u32 == NV_ESC_RM_ALLOC {
            // (class | 1 << 31, 0): an allocation RM was asked for.
            let class = u32::from_le_bytes(b[12..16].try_into().unwrap());
            SEEN.with(|s| s.borrow_mut().push((class | 1 << 31, 0)));
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

    /// NV_ESC_RM_ALLOC (NVOS64) of `class` with `params`.
    fn alloc(be: &mut NvidiaBackend, on: u32, class: u32, params: &[u8]) -> (i32, Vec<u8>) {
        let mut outer = [0u8; 48];
        outer[0..4].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
        outer[4..8].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
        outer[8..12].copy_from_slice(&0x5000_0001u32.to_le_bytes());
        outer[12..16].copy_from_slice(&class.to_le_bytes());
        outer[32..36].copy_from_slice(&(params.len() as u32).to_le_bytes());
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
                cmd: _IOWR(NV_ESC_RM_ALLOC, 48) as u32,
                data_len: 48,
                nested_offset: 16,
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

    fn status_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// The RM allowlist (rmallow.rs) is on by default, in front of RM, for
    /// controls and classes alike, and the host's release picks its list.
    #[test]
    fn rm_calls_the_allowlist_lacks_never_reach_rm() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        be.set_host_driver_version("610.57.04");
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let _ = seen();
        // GPU_SET_POWER and EXEC_REG_OPS: RM lets any user process call
        // them; no workload the project runs does.
        for (cmd, size) in [
            (0x2080_0112u32, 4usize),
            (0x2080_0122, 16),
            (0xdead_beef, 8),
        ] {
            let (st, back) = control(&mut be, ctl, cmd, &vec![0u8; size]);
            assert_eq!(st, 0, "{cmd:#x}: RM's own answer, not a failed ioctl");
            assert_eq!(
                status_at(&back, 28),
                crate::rmallow::NV_ERR_NOT_SUPPORTED,
                "{cmd:#x}"
            );
        }
        assert!(seen().is_empty(), "none reached RM");
        // GPU_GET_INFO_V2 is 580 bytes in 610.57.04: another size is RM's
        // INVALID_PARAM_STRUCT, and the right one goes.
        let (_, back) = control(&mut be, ctl, 0x2080_0102, &[0u8; 64]);
        assert_eq!(
            status_at(&back, 28),
            crate::rmallow::NV_ERR_INVALID_PARAM_STRUCT
        );
        assert!(seen().is_empty());
        let (st, _) = control(&mut be, ctl, 0x2080_0102, &[0u8; 580]);
        assert_eq!(st, 0);
        assert_eq!(seen(), [(0x2080_0102, i32::MIN)]);
        // A class: NV40_I2C is RM's to refuse a user anyway, NV20_SUBDEVICE_DIAG
        // is not; neither is the guest's. NV01_ROOT_CLIENT is.
        for class in [0x402cu32, 0x208f] {
            let (st, back) = alloc(&mut be, ctl, class, &[]);
            assert_eq!(st, 0, "{class:#x}");
            assert_eq!(
                status_at(&back, 40),
                crate::rmallow::NV_ERR_INVALID_CLASS,
                "{class:#x}"
            );
        }
        assert!(seen().is_empty());
        let (st, _) = alloc(&mut be, ctl, 0x41, &[]);
        assert_eq!(st, 0);
        assert_eq!(seen(), [(0x41 | 1 << 31, 0)]);
    }

    #[test]
    fn in_log_mode_the_allowlist_only_says_what_it_would_refuse() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        be.set_host_driver_version("610.57.04");
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let _ = seen();
        let (st, _) = control(&mut be, ctl, 0x2080_0112, &[0u8; 4]);
        assert_eq!(st, 0);
        assert_eq!(seen(), [(0x2080_0112, i32::MIN)]);
        // And the ABI policy does not reach it: permissive or not, enforced.
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        be.set_host_driver_version("610.57.04");
        be.set_abi_policy(AbiPolicy::Permissive);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let (_, back) = control(&mut be, ctl, 0x2080_0112, &[0u8; 4]);
        assert_eq!(status_at(&back, 28), crate::rmallow::NV_ERR_NOT_SUPPORTED);
        assert!(seen().is_empty());
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
        // FLUSH_USER_CACHE carries no descriptor and goes to RM.
        // OS_GET_GPU_INFO, which RM's tables do not export, is answered as
        // RM answers it, by the allowlist (rmallow.rs).
        let (st, _) = control(&mut be, ctl, 0x3d02, &[0u8; 40]);
        assert_eq!(st, 0);
        assert_eq!(seen().len(), 1);
        let (st, back) = control(&mut be, ctl, 0x3d07, &[0u8; 8]);
        assert_eq!(st, 0);
        assert_eq!(
            u32::from_le_bytes(back[28..32].try_into().unwrap()),
            crate::rmctl::NV_ERR_NOT_SUPPORTED
        );
        assert!(seen().is_empty());
    }

    /// An event's parameters too short to hold its descriptor are refused,
    /// not sent for RM to read the field from past them (review 2026-09-29
    /// 1.17).
    #[test]
    fn an_event_block_too_short_for_its_descriptor_never_reaches_rm() {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        for class in [0x05, 0x79] {
            for len in [4, 16, 19] {
                let (st, _) = alloc(&mut be, ctl, class, &vec![0u8; len]);
                assert_eq!(st, -libc::EINVAL, "class {class:#x}, {len} bytes");
            }
        }
        assert!(seen().is_empty());
        let mut p = [0u8; 24];
        p[16..20].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(alloc(&mut be, ctl, 0x05, &p).0, 0, "-1 passes");
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
        assert!(
            be.modeset_open_refused(p(9)).is_none(),
            "a newcomer gets one"
        );
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

/// Where a deep block's address may go (review 2026-09-26, backend 7).
#[cfg(test)]
mod deep_tests {
    use super::*;
    use abi::ioctl::*;
    use std::cell::RefCell;

    /// A call RM saw: what it was (control or class), and the parameters'
    /// words at 0 and 8 as RM read them, each with whether it is an address
    /// in the call's own memory.
    type Seen = (u32, [(u64, bool); 2]);

    std::thread_local! {
        static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    }

    fn seen() -> Vec<Seen> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    /// RM: note the first two words of the parameters as the host reads
    /// them, answer NV_OK.
    fn fake_rm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let (b, others) = arg.split();
        let escape = (request & 0xff) as u32;
        let (what, status) = match escape {
            NV_ESC_RM_CONTROL => (u32::from_le_bytes(b[8..12].try_into().unwrap()), 28),
            NV_ESC_RM_ALLOC => (u32::from_le_bytes(b[12..16].try_into().unwrap()), 40),
            _ => return 0,
        };
        let p = u64::from_le_bytes(b[16..24].try_into().unwrap());
        let word = |at: u64| {
            let v = others.peek(p + at, 8);
            (v, others.reach(v, 1).is_some())
        };
        SEEN.with(|s| s.borrow_mut().push((what, [word(0), word(8)])));
        b[status..status + 4].fill(0);
        0
    }

    fn backend() -> (NvidiaBackend, u32) {
        let mut be = NvidiaBackend::for_test();
        be.set_host_ioctl_for_test(fake_rm);
        // This path's own rule: the allowlist in front of it is tested
        // with rmallow.rs.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let ctl = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Ctl));
        (be, ctl)
    }

    /// A v1 call: `outer`, its `nested` parameters, and a deep block the
    /// guest says the pointer at `deep_at` of them addresses. The reply's
    /// status, and what follows the IoctlResp.
    fn call(
        be: &mut NvidiaBackend,
        on: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: (u32, &[u8]),
    ) -> (i32, Vec<u8>) {
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
                cmd,
                data_len: outer.len() as u32,
                nested_offset: outer.len() as u32,
                nested_len: nested.len() as u32,
                deep_ptr_offset: deep.0,
                deep_len: deep.1.len() as u32,
            },
        );
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        req.extend_from_slice(deep.1);
        let mut resp = vec![0u8; 8192];
        let n = be.dispatch(&req, &mut resp);
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        let st = read_struct::<MsgHeader>(&resp, 0).status;
        (st, resp[body.min(n)..n].to_vec())
    }

    fn control(cmd: u32, size: u32) -> [u8; 32] {
        let mut o = [0u8; 32];
        o[8..12].copy_from_slice(&cmd.to_le_bytes());
        o[16..24].copy_from_slice(&0x7000u64.to_le_bytes());
        o[24..28].copy_from_slice(&size.to_le_bytes());
        o
    }

    const GUEST_PTR: u64 = 0x7fff_1234_5678;

    /// SET_ZBC_COLOR_CLEAR keeps no pointer: an address written into its
    /// color words went to RM as a clear color, into the GPU-wide ZBC
    /// table any tenant reads -- an address of this process.
    #[test]
    fn a_deep_block_puts_no_address_where_rm_follows_no_pointer() {
        const SET_ZBC_COLOR_CLEAR: u32 = 0x9096_0101;
        assert!(crate::guestptr::control_pointers(SET_ZBC_COLOR_CLEAR).is_empty());
        let (mut be, ctl) = backend();
        let mut nested = [0u8; 44];
        nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        let deep = [0xabu8; 16];
        let cmd = _IOWR(NV_ESC_RM_CONTROL, 32) as u32;
        let outer = control(SET_ZBC_COLOR_CLEAR, 44);
        let (st, back) = call(&mut be, ctl, cmd, &outer, &nested, (8, &deep));
        assert_eq!(st, 0);
        let s = seen();
        assert_eq!(s.len(), 1);
        assert_eq!(
            s[0].1[1],
            (GUEST_PTR, false),
            "RM reads the guest's own bytes there, never an address of ours"
        );
        // The caller's parameters and block come back as sent.
        assert_eq!(&back[32..32 + 44], &nested);
        assert_eq!(&back[32 + 44..], &deep);

        // Where RM does follow one (GET_SURFACE_INFO's list), it is
        // relocated as before.
        const GET_SURFACE_INFO: u32 = 0x0041_0110;
        assert_eq!(crate::guestptr::control_pointers(GET_SURFACE_INFO), &[8]);
        let mut nested = [0u8; 16];
        nested[0..4].copy_from_slice(&2u32.to_le_bytes());
        nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        let outer = control(GET_SURFACE_INFO, 16);
        let (st, back) = call(&mut be, ctl, cmd, &outer, &nested, (8, &deep));
        assert_eq!(st, 0);
        let s = seen();
        assert!(s[0].1[1].1, "the list: a block of the call");
        assert_eq!(&back[32 + 8..32 + 16], &GUEST_PTR.to_le_bytes());
    }

    /// No RM_ALLOC class takes a deep block (classes whose parameters hold
    /// pointers are refused, guestptr.rs): one sent is refused, and RM is
    /// not asked.
    #[test]
    fn an_allocation_takes_no_deep_block() {
        let (mut be, ctl) = backend();
        let mut outer = [0u8; 48];
        outer[0..4].copy_from_slice(&0xc1d0_0001u32.to_le_bytes());
        outer[12..16].copy_from_slice(&0x0080u32.to_le_bytes());
        outer[16..24].copy_from_slice(&0x7000u64.to_le_bytes());
        outer[32..36].copy_from_slice(&56u32.to_le_bytes());
        let nested = [0u8; 56];
        let cmd = _IOWR(NV_ESC_RM_ALLOC, 48) as u32;
        let (st, _) = call(&mut be, ctl, cmd, &outer, &nested, (8, &[1u8; 8]));
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
        let (st, _) = call(&mut be, ctl, cmd, &outer, &nested, (0, &[]));
        assert_eq!(st, 0, "without one it goes");
        assert_eq!(seen().len(), 1);
    }
}

/// Descriptors on their way to the closer (review 2026-09-26, backend 14).
#[cfg(test)]
mod closing_tests {
    use super::*;

    /// Holds the closer until told to go on (or two seconds pass).
    struct Hold(std::sync::mpsc::Receiver<()>);

    impl Drop for Hold {
        fn drop(&mut self) {
            let _ = self.0.recv_timeout(std::time::Duration::from_secs(2));
        }
    }

    /// A display file's last close waits on the closer, and while it does
    /// the host file is open. The handle table let it go at CLOSE, so a
    /// guest opening and closing render nodes while the closer was stuck
    /// on a modeset queued host files without bound. They count against
    /// the table until they are closed.
    #[test]
    fn files_still_closing_count_against_the_handle_table() {
        let devnull = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let mut be = NvidiaBackend::for_test();
        be.handles.set_limit(64);
        let (go, wait) = std::sync::mpsc::channel();
        crate::closer::close(Hold(wait));
        let hs: Vec<u32> = (0..64)
            .map(|_| be.adopt_for_test(devnull(), HandleKind::DriRender(0)))
            .collect();
        for h in hs {
            be.close_handle(h).unwrap();
        }
        assert_eq!(be.handle_count(), 0);
        let r = be.handles.insert(devnull(), HandleKind::Eventfd);
        go.send(()).unwrap();
        assert!(r.is_err(), "64 host files are still open");
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert!(be.handles.insert(devnull(), HandleKind::Eventfd).is_ok());
    }

    /// Modeset files still closing count against the NVKMS open caps: with
    /// the closer stalled on a modeset, one process looping open/close held
    /// far more than 64 host NVKMS opens (review 2026-09-29 1.12).
    #[test]
    fn modeset_files_still_closing_count_against_the_nvkms_caps() {
        let devnull = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let p = crate::quota::Owner::Proc {
            tgid: 77,
            start_ns: 1,
        };
        let mut be = NvidiaBackend::for_test();
        let modeset = HandleKind::Dev(DeviceKind::Modeset);
        let (go, wait) = std::sync::mpsc::channel();
        crate::closer::close(Hold(wait));
        let mut n = 0;
        while be.modeset_open_refused(p).is_none() {
            let h = be.adopt_for_test_as(devnull(), modeset, p);
            be.close_handle(h).unwrap();
            n += 1;
            assert!(n <= nvkms::MAX_MODESET_OPENS, "no cap while closing");
        }
        go.send(()).unwrap();
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert_eq!(be.modeset_open_refused(p), None, "closed, the room is back");
    }
}
