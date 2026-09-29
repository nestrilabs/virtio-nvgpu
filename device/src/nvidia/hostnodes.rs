// SPDX-License-Identifier: Apache-2.0
//! The host's DRM nodes and the trees a guest is shown of them: GET_DEV_INFO
//! as each render node answers it, which render and card nodes belong to
//! which GPU, and the GET_PROC_FILES and GET_SYS_FILES streams.

#![forbid(unsafe_code)]

use super::*;

pub(super) const MAX_GPU: u8 = 8;

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
pub(super) fn device_path_with(device_type: u32, dri: &[DriDevice]) -> Result<CString> {
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
pub(super) const NV_DEV_INFO_PROBE_WORDS: usize = 16;

/// `_IOWR('d', DRM_COMMAND_BASE + DRM_NVIDIA_GET_DEV_INFO, u32[16])`: the
/// GET_DEV_INFO number with a 64-byte size field. The size is in the number,
/// but nvidia-drm does not check it -- drm_ioctl (drm_ioctl.c:874-915) sizes
/// its kernel buffer to the larger of the caller's and the driver's, copies
/// the caller's 64 bytes in, lets the handler write its own struct over the
/// front, and copies all 64 back.
pub(super) const DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE: libc::c_ulong =
    0xC000_6443 | ((4 * NV_DEV_INFO_PROBE_WORDS as libc::c_ulong) << 16);

/// `DRM_IO(DRM_COMMAND_BASE + DRM_NVIDIA_DMABUF_SUPPORTED)`: 0 when the node
/// has an NVKMS device behind it (nvidia_drm.modeset=1), -EINVAL otherwise
/// (nvidia-drm-drv.c:1127-1135). It has answered the same way in every
/// release since 535, so it is the one way to learn `supports_alloc` from a
/// host whose GET_DEV_INFO predates that field.
pub(super) const DRM_IOCTL_NVIDIA_DMABUF_SUPPORTED: libc::c_ulong = 0x644f;

/// The probe's filler. Every release's handler writes every field of its
/// struct (535: nvidia-drm-drv.c:684-707; 545-570 and 610 write the booleans
/// and page kinds unconditionally before the modeset=1 block, 610:1082-1117),
/// and the last field of every layout is a small number or a boolean -- so
/// the host's size is where the filler starts.
pub(super) const DEV_INFO_UNWRITTEN: u32 = 0xFFFF_FFFF;

/// The size of the struct the host wrote, from a probe buffer that was full of
/// [`DEV_INFO_UNWRITTEN`] before the ioctl.
pub(super) fn dev_info_host_size(probe: &[u32]) -> u32 {
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
pub(super) fn normalise_dev_info(
    raw: &[u32],
    size: u32,
    modeset: bool,
) -> Option<[u32; NV_DEV_INFO_WORDS]> {
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
pub(super) fn host_dev_info(path: &str) -> Option<([u32; NV_DEV_INFO_WORDS], u32)> {
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
                    let addr = slot.address();
                    let rel = format!("bus/pci/devices/{addr}/config");
                    let abs = std::path::Path::new("/sys").join(&rel);
                    match std::fs::read(&abs) {
                        Ok(live) => {
                            let content = pci_config
                                .get(addr.as_str())
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
pub(super) fn collect_into(
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
pub(super) fn write_dri_section(devices: &[DriDevice], buf: &mut [u8]) -> usize {
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
pub(super) fn write_dev_info_sizes(devices: &[DriDevice], buf: &mut [u8]) -> usize {
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
pub(super) fn write_card_section(cards: &[CardNode], buf: &mut [u8]) -> usize {
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
pub(super) fn node_dev(name: &str) -> Option<(u32, u32)> {
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

pub(super) fn enumerate_host_nodes() -> HostNodes {
    let mut nodes = HostNodes::default();
    for (index, slot) in crate::host::gpu_slots(std::path::Path::new(FileTree::Proc.root()))
        .iter()
        .enumerate()
    {
        let addr = slot.address();
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
    /// The host DRM nodes, enumerated on first use.
    pub(crate) fn host_nodes(&mut self) -> Arc<HostNodes> {
        self.nodes
            .get_or_insert_with(|| Arc::new(enumerate_host_nodes()))
            .clone()
    }

    /// Collect a tree of small files and stream them to the guest.
    ///
    /// The guest republishes these under its own `/proc/driver/nvidia`, which
    /// is where the userspace driver and NVML look before they will talk to a
    /// device at all. Without them a guest with working ioctls still reports
    /// that it cannot find a GPU.
    ///
    /// The response is a bare stream of entries with **no message header** --
    /// the driver reads from the first byte of the buffer.
    pub(super) fn handle_get_files(&mut self, tree: FileTree, resp_buf: &mut [u8]) -> usize {
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
}
