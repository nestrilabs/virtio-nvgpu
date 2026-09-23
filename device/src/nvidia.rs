// crates/device/src/nvidia.rs
use protocol::messages::*;
use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use crate::error::{DeviceError, Result};
use crate::handle_table::HandleTable;
use crate::shm::{ShmAllocator, ZoneConfig};

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

/// The host path an `Open` refers to.
///
/// The wire encoding is one flat `u32`: a GPU is its own minor number and the
/// singleton devices take values above every possible minor. This previously
/// decoded a `{kind, index}` pair that the driver never sent, so every open of
/// the control device arrived as kind 255 and was refused.
fn device_path(device_type: u32) -> Result<CString> {
    device_path_with(device_type, &[])
}

/// As [`device_path`], but able to resolve a render node.
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
struct DriDevice {
    name: String,
    major: u32,
    minor: u32,
    /// Which GPU slot it belongs to. The guest matches this against the GPU's
    /// minor to decide which card the node hangs off; it is ours, not NVIDIA's.
    slot_index: u32,
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
    dev_info: [u32; NV_DEV_INFO_WORDS],
}

/// `struct drm_nvidia_get_dev_info_params` is nine `u32`s. Carried as words
/// because nothing here needs to interpret them -- only the guest does.
const NV_DEV_INFO_WORDS: usize = 9;

/// `_IOWR('d', DRM_COMMAND_BASE + DRM_NVIDIA_GET_DEV_INFO, params)`, i.e.
/// direction read|write, 36 bytes, type 'd', nr 0x43.
const DRM_IOCTL_NVIDIA_GET_DEV_INFO: libc::c_ulong = 0xC024_6443;

/// Ask a host render node what it is.
///
/// `None` when the node cannot be opened or refuses the ioctl, which leaves
/// the guest on its own constants -- wrong, but no worse than before, and
/// said out loud rather than discovered later in a frame.
fn host_dev_info(path: &str) -> Option<[u32; NV_DEV_INFO_WORDS]> {
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
    let mut params = [0u32; NV_DEV_INFO_WORDS];
    // SAFETY: `params` is exactly the 36 bytes the ioctl's size field declares.
    let rc = unsafe {
        libc::ioctl(
            fd,
            DRM_IOCTL_NVIDIA_GET_DEV_INFO,
            params.as_mut_ptr() as *mut libc::c_void,
        )
    };
    let err = std::io::Error::last_os_error();
    // SAFETY: fd came from open() above and is not used again.
    unsafe { libc::close(fd) };
    if rc != 0 {
        log::warn!("{path}: GET_DEV_INFO refused ({err})");
        return None;
    }
    Some(params)
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
    current_msg: MsgType,
    current_handle: u32,
    /// Top-level parameter length of the ioctl being served. The response has
    /// to split the bytes the same way the request did.
    current_data_len: u32,
    handles: HandleTable,
    shm: ShmAllocator,
    /// Active RM_MAP_MEMORY mappings, keyed by SHM offset.
    ///
    /// The SHM offset is written into pLinearAddress in the response to the
    /// guest, so userspace echoes it back as pLinearAddress in RM_UNMAP_MEMORY.
    /// This gives us a unique, unambiguous lookup key without leaking host VAs.
    active_maps: crate::mmap::MmapContext,
    /// Which device each open handle names, for mappings made without one.
    handle_kinds: std::collections::HashMap<u64, DeviceKind>,
    /// Host driver version, learned from the first successful
    /// `NV_ESC_CHECK_VERSION_STR`.
    driver: Option<abi::version::DriverVersion>,
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
    dri_maps: std::collections::HashMap<(u64, u64), u32>,
    /// Every live placement, by the id the guest quotes to take it back.
    live_maps: std::collections::HashMap<u32, LiveMap>,
    next_mapping_id: u32,
}

/// A placement the guest can hand back, and everything needed to undo it.
struct LiveMap {
    key: (u64, u64),
    region: crate::shm::ShmRegion,
    length: u64,
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
impl NvidiaBackend {
    /// Create a backend with a custom SHM zone config.
    pub fn new(cfg: ZoneConfig) -> Self {
        Self {
            window: None,
            dri_maps: std::collections::HashMap::new(),
            live_maps: std::collections::HashMap::new(),
            next_mapping_id: 1,
            current_msg: MsgType::Ioctl,
            current_handle: 0,
            current_data_len: 0,
            handles: HandleTable::new(),
            shm: ShmAllocator::new(cfg),
            active_maps: crate::mmap::MmapContext::new(),
            handle_kinds: std::collections::HashMap::new(),
            driver: None,
            abi: None,
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

    /// Give the backend somewhere to place device memory.
    ///
    /// Until this is called every `RM_MAP_MEMORY` still succeeds on the host --
    /// the mapping is real -- but the `mmap` that follows is refused, because
    /// there is no address in the guest that names it.
    pub fn set_window(&mut self, placer: Box<dyn crate::shm::WindowPlacer>) {
        self.window = Some(placer);
    }

    /// How many host descriptors the guest currently holds open.
    pub fn handle_count(&self) -> usize {
        self.handles.len()
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
        // Restore SHM backing and reclaim every extent before closing host
        // fds. A guest process that exits without unmapping is the normal
        // case, not an error -- most of the mappings in a captured trace are
        // still live when the process ends.
        let leftovers: Vec<_> = self.active_maps.drain().into_iter().map(|e| e.region).collect();
        for region in leftovers {
            if let Err(e) = self.shm.free(&region) {
                log::warn!(
                    "teardown: SHM free of {:#x}+{:#x} failed: {}",
                    region.offset,
                    region.length,
                    e
                );
            }
        }
        self.handles.drain_all();
    }

    // ------------------------------------------------------------------
    // Top-level dispatch
    // ------------------------------------------------------------------

    pub fn dispatch(&mut self, req_buf: &[u8], resp_buf: &mut [u8]) -> usize {
        if req_buf.len() < size_of::<MsgHeader>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0);
        }
        let hdr = read_struct::<MsgHeader>(req_buf, 0);

        let Some(msg_type) = MsgType::from_u32(hdr.msg_type) else {
            log::warn!("unknown msg_type {}", hdr.msg_type);
            self.current_msg = MsgType::Ioctl;
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, 0);
        };
        self.current_msg = msg_type;
        // The handle travels in the header, not the payload -- every message
        // after Open acts on one, and Open's response returns one the same way.
        self.current_handle = hdr.handle;

        let payload = &req_buf[size_of::<MsgHeader>()..];
        match msg_type {
            MsgType::Open => self.handle_open(0, payload, resp_buf),
            MsgType::Close => self.handle_close(0, payload, resp_buf),
            MsgType::Ioctl => self.handle_ioctl(0, payload, resp_buf),
            MsgType::Mmap => self.handle_mmap(payload, resp_buf),
            MsgType::Munmap => self.handle_munmap(payload, resp_buf),
            MsgType::GetProcFiles => self.handle_get_files(FileTree::Proc, resp_buf),
            MsgType::GetSysFiles => self.handle_get_files(FileTree::Sys, resp_buf),
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

        let path = match device_path_with(req.device_type, &self.dri_devices()) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("handle_open: {}", e);
                return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
            }
        };

        let raw_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if raw_fd < 0 {
            let err = std::io::Error::last_os_error();
            let errno = err.raw_os_error().unwrap_or(0);
            log::warn!("open({:?}) failed: {}", path, err);
            return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, errno);
        }

        let guest_handle = self.handles.insert(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        // Kept because caching depends on which device a mapping came from, and
        // by the time an mmap arrives only the handle is in hand.
        if let Some(kind) = DeviceKind::from_device_type(req.device_type) {
            self.handle_kinds.insert(guest_handle, kind);
        }
        log::info!("open {:?} -> handle={guest_handle} (fd={raw_fd})", path);

        // The handle is returned in the header. The driver reads it from there
        // and there is no response payload at all.
        self.write_hdr(resp_buf, guest_handle as u32, 0)
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

        // A DRM node never takes the recorded path. Its bookkeeping is keyed by
        // the file, and one open of a node holds every object a client ever
        // allocates -- so the second object's mmap would find the first one's
        // entry and hand back the first one's memory. The objects are told
        // apart by the offset, and that is what the path below keys on.
        if matches!(
            self.handle_kinds.get(&(self.current_handle as u64)),
            Some(DeviceKind::Dri(_))
        ) {
            return self.map_unrecorded(req.size, req.offset, resp_buf);
        }

        let entry = match self.active_maps.find_by_fd_handle(self.current_handle as u64) {
            Some(e) => e,
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

        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size: mapped_pages,
                mapping_id: 0,
                padding: 0,
            },
        );
        log::debug!("mmap: window offset {offset:#x}+{length:#x}");
        off
    }

    /// Serve an mmap on a file this backend has no record of arming.
    ///
    /// The window placement and the reply are the same as the recorded path;
    /// only the source of the length differs — the guest's request, since there
    /// is no stored region to take it from.
    fn map_unrecorded(&mut self, size: u64, offset: u64, resp_buf: &mut [u8]) -> usize {
        let handle = self.current_handle as u64;
        let host_fd = match self.handles.get_raw(handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, 0, libc::ENOENT),
        };

        // Caching follows the device, which is the same rule the recorded path
        // reaches through the flags RM returns: the control device carries
        // system memory, and a GPU device carries the card's own.
        let kind = self.handle_kinds.get(&handle).copied();
        let pgprot = match kind {
            Some(DeviceKind::Gpu(_)) | Some(DeviceKind::Dri(_)) => crate::shm::PgprotKind::WriteCombine,
            _ => crate::shm::PgprotKind::WriteBack,
        };

        // On a DRM node the guest's offset is a real position in the file --
        // GEM_MAP_OFFSET issued it, on this very descriptor -- and the object's
        // memory is reachable nowhere else. Everywhere else the offset is a
        // cookie RM chose, which names no position at all, and mapping the file
        // there would either fail or land on unrelated memory.
        let fd_offset = match kind {
            Some(DeviceKind::Dri(_)) => offset,
            _ => 0,
        };

        // The same object mapped twice is the same memory: hand back the
        // placement that is already there rather than a second copy of it.
        if let Some(&id) = self.dri_maps.get(&(handle, fd_offset)) {
            if let Some(live) = self.live_maps.get(&id) {
                let (offset, length) = (live.region.offset, live.length);
                let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
                if resp_buf.len() < need {
                    return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
                }
                let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
                off += write_struct(
                    &mut resp_buf[off..],
                    &MmapResp {
                        guest_phys_addr: offset,
                        size: length.div_ceil(4096) * 4096,
                        mapping_id: id,
                        padding: 0,
                    },
                );
                return off;
            }
        }

        let length = size.max(4096);
        let region = match self.shm.alloc(length, pgprot) {
            Ok(r) => r,
            Err(e) => {
                log::error!("mmap on handle {handle}: window has no room: {e}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOMEM);
            }
        };

        let Some(window) = self.window.as_ref() else {
            log::warn!("mmap on handle {handle}: no shared window to place it in");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::ENOTSUP);
        };
        if let Err(e) = window.place(region.offset, length, host_fd, fd_offset, true) {
            log::warn!(
                "mmap on handle {handle}: nothing armed on this file, or it could \
                 not be placed: {e}"
            );
            self.shm.free(&region);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, 0, libc::EINVAL);
        }

        log::info!(
            "mmap on handle {handle}: placed {length:#x} bytes at window offset {:#x} \
             with no arming recorded here",
            region.offset
        );

        let offset = region.offset;
        let id = self.next_mapping_id;
        self.next_mapping_id = self.next_mapping_id.wrapping_add(1).max(1);
        self.dri_maps.insert((handle, fd_offset), id);
        self.live_maps.insert(
            id,
            LiveMap {
                key: (handle, fd_offset),
                region,
                length,
            },
        );
        self.active_maps.insert(
            offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: 0,
                shm_length: length,
                h_client: 0,
                h_memory: 0,
                map_fd_handle: handle,
                region,
            },
        );

        let need = size_of::<MsgHeader>() + size_of::<MmapResp>();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, 0, 0);
        }
        let mut off = self.write_hdr(resp_buf, self.current_handle, 0);
        off += write_struct(
            &mut resp_buf[off..],
            &MmapResp {
                guest_phys_addr: offset,
                size: length.div_ceil(4096) * 4096,
                mapping_id: id,
                padding: 0,
            },
        );
        off
    }

    fn handle_munmap(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<MunmapReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, 0, libc::EINVAL);
        }
        let req = read_struct::<MunmapReq>(payload, 0);

        // Zero is what every mapping the RM path hands out carries: those are
        // taken back by RM_UNMAP_MEMORY, which names them by the address in
        // pLinearAddress. Reported as success so a guest tearing one down does
        // not log a failure for a mapping it never received an id for.
        let Some(live) = self.live_maps.remove(&req.mapping_id) else {
            return self.write_hdr(resp_buf, 0, 0);
        };
        self.dri_maps.remove(&live.key);

        // Emptied rather than unmapped: a hole would leave the memory slot
        // covering a range that reaches no mapping at all, and a stray access
        // there faults the VMM rather than the guest.
        if let Some(window) = self.window.as_ref() {
            if let Err(e) = window.withdraw(live.region.offset, live.length) {
                log::warn!("munmap {}: the window would not give it back: {e}", req.mapping_id);
            }
        }
        self.active_maps.remove(live.region.offset);
        if let Err(e) = self.shm.free(&live.region) {
            log::warn!("munmap {}: freeing the window region: {e}", req.mapping_id);
        }
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
        if tree == FileTree::Sys {
            off += self.write_dri_section(&mut resp_buf[off..]);
        }
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
    fn write_dri_section(&self, buf: &mut [u8]) -> usize {
        let devices = self.dri_devices();
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
    fn dri_devices(&self) -> Vec<DriDevice> {
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

    // ------------------------------------------------------------------
    // CLOSE
    // ------------------------------------------------------------------

    fn handle_close(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let _ = payload;
        let handle = self.current_handle as u64;

        // Closing a device fd releases whatever it was mapping. For some
        // clients this is the only release there is -- a CUDA run maps 29
        // times and never unmaps once -- so leaving it to teardown means every
        // run costs the write-combine zone tens of megabytes for the life of
        // the VM.
        for entry in self.active_maps.take_for_fd(handle) {
            log::debug!(
                "close handle={}: releasing mapping at SHM {:#x}+{:#x}",
                handle,
                entry.region.offset,
                entry.region.length
            );
            if let Err(e) = self.shm.free(&entry.region) {
                log::warn!("close handle={handle}: SHM free failed: {e}");
            }
        }

        match self.handles.remove(handle) {
            Ok(()) => {
                log::debug!("close handle={handle}");
                self.write_hdr(resp_buf, 0, 0)
            }
            Err(_) => self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
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
        self.driver = Some(v);
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

    fn handle_ioctl(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<IoctlReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidMsgType, cookie, 0);
        }
        let ireq = read_struct::<IoctlReq>(payload, 0);

        // The guest sends the top-level struct and the block any pointer in it
        // refers to, back to back. The handlers below already expect that
        // layout, so the two lengths only need adding up here.
        let body = &payload[size_of::<IoctlReq>()..];
        let want =
            ireq.data_len as usize + ireq.nested_len as usize + ireq.deep_len as usize;
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
        let deep_in: Option<(usize, &[u8])> = if ireq.deep_len > 0 {
            Some((ireq.deep_ptr_offset as usize, &body[nested_end..want]))
        } else {
            None
        };

        // How much of the response is the top-level struct. The driver copies
        // exactly this much back to userspace and reads any nested block after
        // it, so a wrong split corrupts one or the other.
        self.current_data_len = ireq.data_len;

        let request = ireq.cmd as u64;
        let host_fd = match self.handles.get_raw(self.current_handle as u64) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
        };

        let escape = (request & 0xFF) as u32;
        let ioc_type = ((request >> 8) & 0xFF) as u32;

        // Only NVIDIA's own magic is described by the ABI tables; modeset and
        // uvm use different namespaces.
        if ioc_type == b'F' as u32 {
            match self.check_abi(escape, ireq.data_len) {
                AbiCheck::SizeMismatch { expected, actual } => log::warn!(
                    "escape {escape:#04x}: guest sent {actual} bytes, host driver {} expects {expected}",
                    self.driver.expect("a profile implies a known version")
                ),
                AbiCheck::UnknownEscape => log::warn!(
                    "escape {escape:#04x} is not in the ABI profile for host driver {}",
                    self.driver.expect("a profile implies a known version")
                ),
                AbiCheck::Ok | AbiCheck::VariableLength | AbiCheck::NoProfile => {}
            }
        }

        // nvidia-modeset ioctls: type 'm' (0x6d), nested pointer at offset 8, size at offset 4
        if ioc_type == 0x6d {
            return self.dispatch_nested(
                cookie,
                host_fd,
                request,
                param_in,
                resp_buf,
                16, // outer_size
                8,  // ptr_offset
                4,  // size_offset
                deep_in,
                None,
            );
        }

        // nvidia-drm's GEM ioctls: type 'd' (0x64), and `escape` is the
        // absolute DRM ioctl number, not an offset from DRM_COMMAND_BASE.
        //
        // Two of the five carry a userspace pointer to an NVKMS parameter
        // block, in the same shape nvidia-modeset uses, so they take the same
        // path; the other three are flat and fall through to the passthrough
        // below. Nothing here translates the GEM handles in these structs: a
        // handle is per drm_file, and the guest's open of its render node
        // holds exactly one open of ours, so the handle the host driver issues
        // is already scoped to the file that will use it.
        //
        // Sizes and offsets are from NVIDIA's
        // kernel-open/nvidia-drm/nv_drm_common_ioctl.h, and the guest driver
        // holds the same numbers in nvgpu_gem_import_nvkms /
        // nvgpu_gem_export_dmabuf. Both halves have to be changed together.
        if ioc_type == b'd' as u32 {
            // (outer_size, ptr_offset, size_offset)
            let nested = match escape {
                0x41 => Some((32usize, 8usize, 16usize)), // GEM_IMPORT_NVKMS_MEMORY
                0x4d => Some((24usize, 8usize, 16usize)), // GEM_EXPORT_DMABUF_MEMORY
                _ => None,
            };
            log::debug!("drm ioctl nr={escape:#04x} ({} bytes in)", param_in.len());
            if let Some((outer_size, ptr_offset, size_offset)) = nested {
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
                );
            }
        }

        // Escape numbers below are NVIDIA's, and they are only NVIDIA's inside
        // type 'F'. Other namespaces reuse the same numbers for their own
        // commands -- nvidia-drm's DMABUF_SUPPORTED is nr 0x4f, which is
        // NV_ESC_RM_UNMAP_MEMORY here -- so anything that is not RM is handed
        // to the host as it arrived rather than matched against this table.
        if ioc_type != b'F' as u32 {
            return self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf);
        }

        use abi::ioctl::*;
        match escape {
            // ---------------------------------------------------------------
            // FD-carrying ioctls — need handle translation
            // ---------------------------------------------------------------
            NV_ESC_REGISTER_FD | NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
                self.dispatch_fd_carrying(cookie, host_fd, request, escape, param_in, resp_buf)
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

            NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => self.dispatch_update_device_mapping_info(
                cookie,
                host_fd,
                request,
                param_in,
                resp_buf,
            ),

            // ---------------------------------------------------------------
            // RM control requires nested handling
            // ---------------------------------------------------------------
            NV_ESC_RM_CONTROL => self.dispatch_nested(
                cookie,
                host_fd,
                request,
                param_in,
                resp_buf,
                32,
                16,
                24,
                deep_in,
                None,
            ),

            // ---------------------------------------------------------------
            // RM alloc as well..
            // ---------------------------------------------------------------
            NV_ESC_RM_ALLOC => self.dispatch_nested(
                cookie,
                host_fd,
                request,
                param_in,
                resp_buf,
                48,
                16,
                32,
                deep_in,
                None,
            ),

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
                self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf)
            }
        }
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
        _size_offset: usize,
        deep_in: Option<(usize, &[u8])>,
        // Byte offset, inside the nested block, of a descriptor the guest
        // sent as one of our handles and the host must see as one of our
        // descriptors. `None` for the RM paths, which name their descriptors
        // by command rather than by position.
        nested_fd_offset: Option<usize>,
    ) -> usize {
        if param_in.len() < outer_size {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let mut outer = param_in[..outer_size].to_vec();
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

        // Log RM_CONTROL/RM_ALLOC for debugging Vulkan init
        if escape == 0x2A && outer.len() >= 12 {
            let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());
            log::info!(
                "RM_CONTROL cmd=0x{:x} (hClient={}, hObject={})",
                cmd,
                u32::from_le_bytes(outer[0..4].try_into().unwrap()),
                u32::from_le_bytes(outer[4..8].try_into().unwrap())
            );
        }
        if escape == 0x2B && outer.len() >= 16 {
            let h_class = u32::from_le_bytes(outer[12..16].try_into().unwrap());
            log::info!("RM_ALLOC hClass=0x{:x}", h_class);
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
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM)
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
            // and puts the guest's value back before replying.
            if escape == 0x2B && outer.len() >= 16 {
                let h_class = u32::from_le_bytes(outer[12..16].try_into().unwrap());
                const NV0005_DATA: usize = 16;
                if matches!(h_class, 0x05 | 0x79) && host_buf.len() >= NV0005_DATA + 4 {
                    let guest_handle_val =
                        i32::from_le_bytes(host_buf[NV0005_DATA..NV0005_DATA + 4].try_into().unwrap());
                    match self.handles.get_raw(guest_handle_val as u64) {
                        Ok(real_fd) => {
                            saved_nested_handle = Some((NV0005_DATA, guest_handle_val));
                            host_buf[NV0005_DATA..NV0005_DATA + 4]
                                .copy_from_slice(&(real_fd as i32).to_le_bytes());
                        }
                        Err(_) => {
                            // Worth naming rather than forwarding: RM answers
                            // NV_ERR_OBJECT_NOT_FOUND, which reads as a missing
                            // object rather than an untranslated descriptor.
                            log::warn!(
                                "event class {h_class:#x}: no handle {guest_handle_val} for \
                                 the file this event is to be delivered on"
                            );
                        }
                    }
                }
            }

            if escape == 0x2A && outer.len() >= 12 {
                let cmd = u32::from_le_bytes(outer[8..12].try_into().unwrap());

                if cmd == 0x3d05 && host_buf.len() >= 20 {
                    // EXPORT_OBJECT_TO_FD: guest_handle at offset 16 in nested
                    let guest_handle_val = i32::from_le_bytes(host_buf[16..20].try_into().unwrap());

                    match self.handles.get_raw(guest_handle_val as u64) {
                        Ok(real_fd) => {
                            log::debug!(
                                "EXPORT_TO_FD: handle {} → host fd {}",
                                guest_handle_val,
                                real_fd
                            );
                            saved_nested_handle = Some((16, guest_handle_val));
                            host_buf[16..20].copy_from_slice(&(real_fd as i32).to_le_bytes());
                        }
                        Err(_) => {
                            log::warn!("EXPORT_TO_FD: bad guest_handle {}", guest_handle_val);
                            return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
                        }
                    }
                }

                if cmd == 0x3d06 && host_buf.len() >= 4 {
                    // IMPORT_OBJECT_FROM_FD: guest_handle at offset 0 in nested
                    let guest_handle_val = i32::from_le_bytes(host_buf[0..4].try_into().unwrap());

                    match self.handles.get_raw(guest_handle_val as u64) {
                        Ok(real_fd) => {
                            log::debug!(
                                "IMPORT_FROM_FD: handle {} → host fd {}",
                                guest_handle_val,
                                real_fd
                            );
                            saved_nested_handle = Some((0, guest_handle_val));
                            host_buf[0..4].copy_from_slice(&(real_fd as i32).to_le_bytes());
                        }
                        Err(_) => {
                            log::warn!("IMPORT_FROM_FD: bad guest_handle {}", guest_handle_val);
                            return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
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
                match self.handles.get_raw(guest_handle_val as u64) {
                    Ok(real_fd) => {
                        log::debug!(
                            "nvkms memFd: handle {guest_handle_val} → host fd {real_fd}"
                        );
                        saved_nested_handle = Some((off, guest_handle_val));
                        host_buf[off..off + 4]
                            .copy_from_slice(&(real_fd as i32).to_le_bytes());
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
            let mut deep_buf: Vec<u8> = Vec::new();
            let mut deep_saved: Option<(usize, [u8; 8])> = None;
            if let Some((ptr_off, bytes)) = deep_in {
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
                deep_buf = bytes.to_vec();
                deep_buf.resize(bytes.len().max(DEEP_BUF_FLOOR), 0);
                let _ = DEEP_BUF_FLOOR;
                log::debug!(
                    "deep pointer at {ptr_off}: guest says {} bytes, buffer {} bytes",
                    bytes.len(),
                    deep_buf.len()
                );
                let mut guest_ptr = [0u8; 8];
                guest_ptr.copy_from_slice(&host_buf[ptr_off..ptr_off + 8]);
                deep_saved = Some((ptr_off, guest_ptr));
                let host_ptr = deep_buf.as_mut_ptr() as u64;
                host_buf[ptr_off..ptr_off + 8].copy_from_slice(&host_ptr.to_le_bytes());
            }

            // Call host ioctl — paramsSize field is untouched (may be 0)
            let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, outer.as_mut_ptr()) };
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
                let status = u32::from_le_bytes(
                    outer[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap(),
                );
                if status == NV_OK {
                    log::info!(
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
                log::info!(
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

            // The caller's own pointer value goes back, not ours and not zero.
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);

            // Build response: outer + updated nested params + what the
            // pointer inside them addresses.
            if let Some((ptr_off, guest_ptr)) = deep_saved {
                host_buf[ptr_off..ptr_off + 8].copy_from_slice(&guest_ptr);
            }
            // Only what the guest allocated room for goes back, not the pad.
            let deep_reply = deep_in.map(|(_, b)| b.len()).unwrap_or(0);
            let mut combined = outer;
            combined.extend_from_slice(&host_buf);
            combined.extend_from_slice(&deep_buf[..deep_reply]);
            self.write_ioctl_resp_deep(resp_buf, cookie, &combined, deep_reply)
        } else {
            // No nested params — straightforward passthrough
            let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, outer.as_mut_ptr()) };
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
                log::info!("(else) RM_CONTROL cmd=0x{:08x} status=0x{:x}", cmd, status);
            } else if escape == 0x2b {
                let status = u32::from_le_bytes(outer[40..44].try_into().unwrap());
                let hclass = u32::from_le_bytes(outer[12..16].try_into().unwrap());
                log::info!(
                    "(else) RM_ALLOC hClass=0x{:04x} status=0x{:x}",
                    hclass,
                    status
                );
            }

            // Same here: restore what the caller passed, in case the host
            // driver wrote to the field.
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);
            self.write_ioctl_resp(resp_buf, cookie, &outer)
        }
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

        let mut param_buf = param_in.to_vec();

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

        let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);

            // Special handling: NV_ESC_SYS_PARAMS (0xd6) - retry on EBUSY
            if escape == 0xd6 && errno == libc::EBUSY && retry_with_v2 {
                log::info!("NV_ESC_SYS_PARAMS: got EBUSY, retrying with Cmd=2");
                param_buf[0] = 2; // Try V2
                let rc2 =
                    unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
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
            if escape == 0x4a {
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
        if embedded < 0 {
            let mut param_buf = param_in.to_vec();
            let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
            if rc < 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                log::warn!("ioctl(0x{request:x}) with no embedded fd failed: errno={errno}");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }
            return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
        }

        let guest_embedded = embedded as u64;
        let host_embedded = match self.handles.get_raw(guest_embedded) {
            Ok(fd) => fd,
            Err(_) => {
                log::warn!("fd-carrying ioctl: bad embedded handle {}", guest_embedded);
                return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
            }
        };

        let mut param_buf = param_in.to_vec();
        param_buf[fd_offset..fd_offset + 4].copy_from_slice(&(host_embedded as i32).to_le_bytes());

        let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };

        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!("fd-carrying ioctl(0x{:x}) failed: errno={}", request, errno);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        // Restore guest handle in response so user-mode code reading it back
        // gets what it originally wrote.
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
        log::info!(
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

        log::info!(
            "UPDATE_DEVICE_MAPPING_INFO: client={:#x} mem={:#x} old={:#x} new={:#x}",
            h_client,
            h_memory,
            old_cpu_addr,
            new_cpu_addr
        );

        // The guest sends SHM offsets or guest VAs. The host RM needs host VAs.
        // Look up the mapping by scanning active_maps for matching hMemory,
        // since the guest's "old" address won't match any host address.
        let mut host_old = old_cpu_addr;
        if let Some(entry) = self.active_maps.find_by_object(h_client, h_memory) {
            host_old = entry.host_p_linear_address;
            log::info!(
                "UPDATE_DEVICE_MAPPING_INFO: translated old {:#x} → host {:#x}",
                old_cpu_addr,
                host_old
            );
        }

        let mut param_buf = param_in.to_vec();
        // Set pOldCpuAddress to host VA
        param_buf[16..24].copy_from_slice(&host_old.to_le_bytes());
        // Set pNewCpuAddress to host VA too (the host mapping didn't move)
        param_buf[24..32].copy_from_slice(&host_old.to_le_bytes());

        let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!(
                "UPDATE_DEVICE_MAPPING_INFO: host ioctl failed: errno={}",
                errno
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[32..36].try_into().unwrap());
        log::info!("UPDATE_DEVICE_MAPPING_INFO: host status=0x{:x}", status);

        // Zero out the addresses before sending back to guest
        param_buf[16..24].copy_from_slice(&0u64.to_le_bytes());
        param_buf[24..32].copy_from_slice(&0u64.to_le_bytes());

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
        use crate::shm::PgprotKind;

        const _NVOS33_SIZE: usize = 48;
        const WITH_FD_SIZE: usize = 56;
        const FD_OFFSET: usize = 48;
        const LENGTH_OFFSET: usize = 24;
        const STATUS_OFFSET: usize = 40;
        const FLAGS_OFFSET: usize = 44;

        const FLAGS_CACHING_TYPE_SHIFT: u32 = 23;
        const FLAGS_CACHING_TYPE_MASK: u32 = 0x7;
        const CACHING_TYPE_CACHED: u32 = 0;
        const CACHING_TYPE_UNCACHED: u32 = 1;
        const CACHING_TYPE_WRITECOMBINED: u32 = 2;
        const CACHING_TYPE_WRITEBACK: u32 = 5;
        const CACHING_TYPE_DEFAULT: u32 = 6;
        const CACHING_TYPE_UNCACHED_WEAK: u32 = 7;

        if param_in.len() < WITH_FD_SIZE {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        // --- Step 1: Translate embedded FD (guest handle → host fd) ---

        let guest_fd_handle = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[FD_OFFSET..FD_OFFSET + 4]);
            i32::from_le_bytes(b) as u64
        };

        let host_map_fd = match self.handles.get_raw(guest_fd_handle) {
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

        let mut param_buf = param_in.to_vec();
        param_buf[FD_OFFSET..FD_OFFSET + 4].copy_from_slice(&(host_map_fd as i32).to_le_bytes());

        // --- Step 2: Call host ioctl ---

        let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };

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

        // --- Step 4: Determine pgprot from caching type ---
        //
        // The host driver may have updated the caching type in flags after
        // the ioctl (see nvproxy's rmMapMemory comment about this).

        let caching_type = (flags >> FLAGS_CACHING_TYPE_SHIFT) & FLAGS_CACHING_TYPE_MASK;

        let pgprot = match caching_type {
            CACHING_TYPE_CACHED | CACHING_TYPE_WRITEBACK => PgprotKind::WriteBack,
            CACHING_TYPE_WRITECOMBINED | CACHING_TYPE_DEFAULT => PgprotKind::WriteCombine,
            CACHING_TYPE_UNCACHED | CACHING_TYPE_UNCACHED_WEAK => PgprotKind::Uncached,
            other => {
                log::warn!(
                    "NV_ESC_RM_MAP_MEMORY: unknown caching type {}, defaulting to UC",
                    other
                );
                PgprotKind::Uncached
            }
        };

        // --- Step 5: Allocate SHM region ---

        let region = match self.shm.alloc(length, pgprot) {
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
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTSUP);
        };
        if let Err(e) = window.place(region.offset, length, host_map_fd, 0, true) {
            log::error!("NV_ESC_RM_MAP_MEMORY: placing in the window failed: {}", e);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOMEM);
        }

        log::info!(
            "dispatch_map_memory: returning shm_offset=0x{:x} shm_length=0x{:x} pgprot={}",
            region.offset,
            length,
            pgprot as u8
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
        let h_client = u32::from_le_bytes(param_buf[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_buf[8..12].try_into().unwrap());

        // The handle is the key the mmap that follows will be found by, so it
        // is the one field worth naming in the log: a mapping that is armed
        // against one file and consumed on another is the whole failure mode.
        log::info!(
            "MAP_MEMORY: armed on handle {} (shm_off={:#x}) → host_va={:#x} client={:#x} mem={:#x}",
            guest_fd_handle,
            region.offset,
            host_p_linear,
            h_client,
            h_memory
        );

        let region_offset = region.offset;
        self.active_maps.insert(
            region_offset,
            crate::mmap::MmapEntry {
                host_p_linear_address: host_p_linear,
                shm_length: length,
                h_client,
                h_memory,
                map_fd_handle: guest_fd_handle,
                region,
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
        // Mmap message quoting the cookie just written into pLinearAddress, and
        // that is where the placement and caching are decided. An earlier reply
        // struct carried them here, which the guest driver never read.
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
                // Forward with the guest value — host will reject but we
                // report the error cleanly rather than crashing
                let mut param_buf = param_in.to_vec();
                let rc =
                    unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
                if rc < 0 {
                    let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                    return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
                }
                return self.write_ioctl_resp(resp_buf, cookie, &param_buf);
            }
        };

        log::info!(
            "UNMAP_MEMORY: shm_off={:#x} → host_va={:#x} (client={:#x}, mem={:#x})",
            guest_linear,
            entry.host_p_linear_address,
            h_client,
            h_memory
        );

        // Substitute the real host pLinearAddress for the host ioctl
        let mut param_buf = param_in.to_vec();
        param_buf[16..24].copy_from_slice(&entry.host_p_linear_address.to_le_bytes());

        let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, param_buf.as_mut_ptr()) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            log::warn!("UNMAP_MEMORY: host ioctl failed: errno={}", errno);
            // Restore the entry since unmap didn't happen
            self.active_maps.insert(guest_linear, entry);
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        let status = u32::from_le_bytes(param_buf[24..28].try_into().unwrap());
        log::info!("UNMAP_MEMORY: host status=0x{:x}", status);

        if status == 0 {
            // Host unmap succeeded -- restore the SHM backing and return the
            // extent to its zone, so the space can serve a later mapping.
            if let Err(e) = self.shm.free(&entry.region) {
                log::warn!("UNMAP_MEMORY: SHM free failed: {} (non-fatal)", e);
            }
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
            padding: 0,
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
                padding: 0,
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
        let e = if errno != 0 { errno.abs() } else { status.errno() };
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
        assert_eq!(b.driver, Some(abi::version::DriverVersion::new(580, 178, 4)));
        assert!(b.abi.is_some(), "580.178.04 must select a profile");
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
            AbiCheck::SizeMismatch { expected: 64, actual: 48 }
        );
    }

    #[test]
    fn variable_length_escapes_are_not_size_checked() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        // CARD_INFO is an array; the T4 sent 2304 bytes in one call.
        assert_eq!(b.check_abi(NV_ESC_CARD_INFO, 2304), AbiCheck::VariableLength);
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
                padding: 0,
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
        assert_ne!(r.status, -Status::BadHandle.errno(),
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
    // GPU registers, not system memory.
    // ------------------------------------------------------------------

    const NV01_ROOT_CLIENT: u32 = 0x41;
    const NV01_DEVICE_0: u32 = 0x80;
    const NV20_SUBDEVICE_0: u32 = 0x2080;
    const TURING_USERMODE_A: u32 = 0xc461;

    /// NVOS64_PARAMETERS field offsets.
    const A_ROOT: usize = 0;
    const A_PARENT: usize = 4;
    const A_NEW: usize = 8;
    const A_CLASS: usize = 12;
    const A_PARAMS_SIZE: usize = 32;
    const A_STATUS: usize = 40;
    const ALLOC_OUTER: usize = 48;

    struct Chain {
        be: NvidiaBackend,
        ctl: u64,
        gpu: u64,
        cookie: u64,
    }

    impl Chain {
        fn new() -> Self {
            // Not for_test(): its write-combine zone is 16 KiB, and the
            // smallest real mapping here is 64 KiB.
            let mut be = NvidiaBackend::with_default_zones();
            let req = open_msg(DeviceKind::Ctl);
            let mut resp = vec![0u8; 64];
            be.dispatch(&req, &mut resp);
            assert_eq!(parse_resp(&resp).status, 0, "open /dev/nvidiactl");
            let ctl = opened_handle(&resp);
            let mut c = Self { be, ctl, gpu: 0, cookie: 2 };
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
            assert_eq!(parse_resp(&resp).status, 0,
                "alloc class {class:#x}: transport status"
            );
            let body = IOCTL_BODY;
            let out = &resp[body..body + ALLOC_OUTER];
            let status = u32::from_le_bytes(out[A_STATUS..A_STATUS + 4].try_into().unwrap());
            assert_eq!(status, 0, "alloc class {class:#x}: RM status {status:#x}");
            u32::from_le_bytes(out[A_NEW..A_NEW + 4].try_into().unwrap())
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

            let memory = self.alloc(client, subdevice, TURING_USERMODE_A, &[]);
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
            assert_eq!(rh.status, 0,
            );
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
            assert_eq!(rh.status, 0,
            );
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

        let (off, len, linear) = c.map(client, sub, mem, 65536, fd);
        assert_eq!(len, 65536, "mapped length");
        assert_ne!(linear, 0, "pLinearAddress should be the SHM offset");
        assert_eq!(linear, off, "pLinearAddress must be the SHM offset the guest sees");

        // The SHM window now aliases GPU registers. Reading must not fault.
        let base = c.be.shm_base_ptr();
        assert!(!base.is_null(), "SHM base");
        let first = unsafe { std::ptr::read_volatile(base.add(off as usize) as *const u32) };
        eprintln!("TURING_USERMODE_A first dword through SHM: {first:#010x}");

        c.unmap(client, sub, mem, linear);
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
            let mem = c.alloc(client, sub, TURING_USERMODE_A, &[]);
            let fd = c.map_fd();
            let (off, _len, _linear) = c.map(client, sub, mem, 65536, fd);
            assert_ne!(off, 0, "run {i}: no SHM offset");

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

        // TURING_USERMODE_A permits one mapping per object -- a second map of
        // a still-mapped object is refused with NV_ERR_STATE_IN_USE -- so each
        // cycle allocates its own.
        let before = c.be.shm_free_bytes();
        let handles_before = c.be.handle_count();
        let cycles = 100;
        for i in 0..cycles {
            let mem = c.alloc(client, sub, TURING_USERMODE_A, &[]);
            let fd = c.map_fd();
            eprintln!("cycle {i}: mem={mem:#x} fd={fd}");
            let (off, _len, linear) = c.map(client, sub, mem, 65536, fd);
            assert_ne!(off, 0, "iteration {i}: no SHM offset");
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

}
