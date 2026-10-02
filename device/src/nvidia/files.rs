//! GET_PROC_FILES and GET_SYS_FILES: the host trees a guest republishes, and
//! the DRI section that follows them.

use super::*;

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

impl NvidiaBackend {
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
    pub(super) fn handle_get_files(&mut self, tree: FileTree, resp_buf: &mut [u8]) -> usize {
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
        if tree == FileTree::Sys {
            off += self.write_dri_section(&mut resp_buf[off..]);
            off += self.write_alloc_size_section(&mut resp_buf[off..]);
        }
        off
    }

    /// Magic word opening the allocation-size section.
    ///
    /// The DRI section above is positional: a guest reads whatever follows the
    /// file terminator as its count, so a backend that omits a section does
    /// not fail, it feeds the guest noise. A third section cannot be added
    /// positionally for the same reason -- a guest from before it would read
    /// these sizes as a DRI count. The magic is what lets a guest tell "the
    /// backend sent this" from "the backend did not".
    pub(super) const ALLOC_SIZE_MAGIC: u32 = 0x4e56_414c; // "NVAL"

    /// What RM sizes each allocation at, for the release the host is running.
    ///
    /// `NV_ESC_RM_ALLOC` usually arrives with `paramsSize` zero: RM takes the
    /// size from its own resource descriptor and NVIDIA's userspace does not
    /// bother filling the field in. The guest driver still has to know how
    /// many bytes to copy out of userspace before it can forward anything, so
    /// it has carried a table of its own, generated from one release and
    /// compiled in.
    ///
    /// That table cannot be right. `NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS`
    /// grew two words between 595 and 615, so a guest built against the older
    /// one forwards 20 bytes of a 28-byte struct and RM reads the other eight
    /// from whatever follows the buffer. It is the backend that knows which
    /// release the host runs, so the sizes come from here.
    pub(super) fn write_alloc_size_section(&self, buf: &mut [u8]) -> usize {
        let Some(sel) = self.rmallow else {
            // Nothing learned about the host yet, so nothing to say. The guest
            // keeps its own table, which is where it was before this section.
            return 0;
        };
        let with_params: Vec<_> = sel.class.iter().filter(|c| c.params_size > 0).collect();
        if buf.len() < 8 + with_params.len() * 8 {
            log::warn!("no room for the allocation-size section; the guest will use its own table");
            return 0;
        }
        let mut off = 0;
        for v in [Self::ALLOC_SIZE_MAGIC, with_params.len() as u32] {
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
            off += 4;
        }
        for c in &with_params {
            for v in [c.class_id, c.params_size] {
                buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
                off += 4;
            }
        }
        log::info!(
            "GET_SYS_FILES: RM's allocation sizes for {} class(es)",
            with_params.len()
        );
        off
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
    pub(super) fn write_dri_section(&self, buf: &mut [u8]) -> usize {
        // Without graphics there is no render node to offer, and a guest told
        // of none creates none, whatever it knows about capabilities.
        let devices = if self.caps.has(crate::caps::GRAPHICS) {
            self.dri_devices()
        } else {
            Vec::new()
        };
        log::info!("GET_SYS_FILES: {} DRI device(s)", devices.len());

        if buf.len() < 4 {
            return 0;
        }
        let mut off = 0;
        buf[off..off + 4].copy_from_slice(&(devices.len() as u32).to_le_bytes());
        off += 4;

        for d in &devices {
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

    /// The render nodes the host's GPUs own.
    ///
    /// Taken from `/sys/bus/pci/devices/<addr>/drm`, which is the kernel's own
    /// statement of which DRI nodes belong to which card -- rather than from
    /// the numbering of `/dev/dri`, where a node's index says nothing about
    /// which device it is.
    ///
    /// Only render nodes are offered. A card node is a display device and this
    /// device forwards compute and render; handing one out would be a
    /// different kind of access than the guest asked for.
    pub(super) fn dri_devices(&self) -> Vec<DriDevice> {
        let mut out = Vec::new();
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
                .filter(|n| n.starts_with("renderD"))
                .collect();
            names.sort();

            for name in names {
                // The kernel prints "major:minor" here. A node listed under the
                // PCI device with no `dev` file is not one we can reproduce.
                let Ok(text) = std::fs::read_to_string(format!("/sys/class/drm/{name}/dev")) else {
                    log::warn!("DRI node {name} has no dev file");
                    continue;
                };
                let text = text.trim();
                let Some((maj, min)) = text.split_once(':') else {
                    log::warn!("DRI node {name}: cannot read {text:?} as major:minor");
                    continue;
                };
                let (Ok(major), Ok(minor)) = (maj.parse::<u32>(), min.parse::<u32>()) else {
                    log::warn!("DRI node {name}: cannot read {text:?} as major:minor");
                    continue;
                };
                let dev_info = host_dev_info(&format!("/dev/dri/{name}")).unwrap_or_else(|| {
                    // Same shape the guest used to invent, so a refusal is no
                    // worse than the old behaviour -- but it is logged above.
                    let mut fallback = [0u32; NV_DEV_INFO_WORDS];
                    fallback[3] = 1; // supports_alloc
                    fallback[4] = 6; // generic_page_kind
                    fallback[5] = 2; // page_kind_generation
                    fallback[6] = 1; // sector_layout
                    fallback[7] = 1; // supports_sync_fd
                    fallback[8] = 1; // supports_semsurf
                    fallback
                });
                log::info!(
                    "DRI {name} at {major}:{minor} on {addr} (slot {index}, \
                     nvidia gpu_id {:#x}, page kind {}/{}, sector layout {})",
                    dev_info[0],
                    dev_info[4],
                    dev_info[5],
                    dev_info[6],
                );
                out.push(DriDevice {
                    name,
                    major,
                    minor,
                    slot_index: index as u32,
                    dev_info,
                });
            }
        }
        out
    }
}
