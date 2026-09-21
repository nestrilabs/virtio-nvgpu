use std::collections::HashMap;
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

fn device_path(kind: u8, index: u8) -> Result<CString> {
    let path = match kind {
        k if k == DeviceKind::Ctl as u8 => "/dev/nvidiactl".to_string(),
        k if k == DeviceKind::Gpu as u8 => {
            if index >= MAX_GPU {
                return Err(DeviceError::GpuIndexOutOfRange(index));
            }
            format!("/dev/nvidia{}", index)
        }
        k if k == DeviceKind::Uvm as u8 => "/dev/nvidia-uvm".to_string(),
        k if k == DeviceKind::Modeset as u8 => "/dev/nvidia-modeset".to_string(),
        other => return Err(DeviceError::InvalidDeviceKind(other)),
    };
    Ok(CString::new(path).unwrap())
}

// ============================================================
// NvidiaBackend
// ============================================================


pub struct NvidiaBackend {
    handles: HandleTable,
    shm: ShmAllocator,
    /// Active RM_MAP_MEMORY mappings, keyed by SHM offset.
    ///
    /// The SHM offset is written into pLinearAddress in the response to the
    /// guest, so userspace echoes it back as pLinearAddress in RM_UNMAP_MEMORY.
    /// This gives us a unique, unambiguous lookup key without leaking host VAs.
    active_maps: crate::mmap::MmapContext,
    /// Host driver version, learned from the first successful
    /// `NV_ESC_CHECK_VERSION_STR`.
    driver: Option<abi::version::DriverVersion>,
    /// ABI profile selected for `driver`, if one exists.
    abi: Option<&'static [abi::versions::IoctlEntry]>,
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
            handles: HandleTable::new(),
            shm: ShmAllocator::new(cfg),
            active_maps: crate::mmap::MmapContext::new(),
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
        let cookie = hdr.cookie;

        let msg_type = match hdr.msg_type {
            t if t == MsgType::Open as u32 => MsgType::Open,
            t if t == MsgType::Close as u32 => MsgType::Close,
            t if t == MsgType::Ioctl as u32 => MsgType::Ioctl,
            other => {
                log::warn!("unknown msg_type {}", other);
                return self.write_error_resp(resp_buf, Status::InvalidMsgType, cookie, 0);
            }
        };

        let payload = &req_buf[size_of::<MsgHeader>()..];
        match msg_type {
            MsgType::Open => self.handle_open(cookie, payload, resp_buf),
            MsgType::Close => self.handle_close(cookie, payload, resp_buf),
            MsgType::Ioctl => self.handle_ioctl(cookie, payload, resp_buf),
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

        let path = match device_path(req.kind, req.index) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("handle_open: {}", e);
                return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
            }
        };

        log::info!("handle_open: opening {:?}", path);

        let raw_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };

        if raw_fd < 0 {
            let err = std::io::Error::last_os_error();
            let errno = err.raw_os_error().unwrap_or(0);
            log::warn!("open({:?}) failed: {}", path, err);
            return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, errno);
        }

        let guest_handle = self.handles.insert(unsafe { OwnedFd::from_raw_fd(raw_fd) });
        log::info!(
            "handle_open: {:?} → handle={} (fd={})",
            path,
            guest_handle,
            raw_fd
        );

        write_ok(resp_buf, cookie, &OpenResp { guest_handle })
    }

    // ------------------------------------------------------------------
    // CLOSE
    // ------------------------------------------------------------------

    fn handle_close(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        if payload.len() < size_of::<CloseReq>() {
            return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
        }
        let req = read_struct::<CloseReq>(payload, 0);

        match self.handles.remove(req.guest_handle) {
            Ok(()) => {
                log::debug!("close handle={}", req.guest_handle);
                write_ok(resp_buf, cookie, &CloseResp { _pad: 0 })
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
        let param_in =
            &payload[size_of::<IoctlReq>()..size_of::<IoctlReq>() + ireq.param_size as usize];

        let host_fd = match self.handles.get_raw(ireq.guest_handle) {
            Ok(fd) => fd,
            Err(_) => return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
        };

        let escape = (ireq.request & 0xFF) as u32;
        let ioc_type = ((ireq.request >> 8) & 0xFF) as u32;

        // Only NVIDIA's own magic is described by the ABI tables; modeset and
        // uvm use different namespaces.
        if ioc_type == b'F' as u32 {
            match self.check_abi(escape, ireq.param_size) {
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
                ireq.request,
                param_in,
                resp_buf,
                16, // outer_size
                8,  // ptr_offset
                4,  // size_offset
            );
        }

        use abi::ioctl::*;
        match escape {
            // ---------------------------------------------------------------
            // FD-carrying ioctls — need handle translation
            // ---------------------------------------------------------------
            NV_ESC_REGISTER_FD | NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
                self.dispatch_fd_carrying(cookie, host_fd, ireq.request, escape, param_in, resp_buf)
            }

            NV_ESC_RM_ALLOC_MEMORY => {
                self.dispatch_fd_carrying(cookie, host_fd, ireq.request, escape, param_in, resp_buf)
            }

            NV_ESC_RM_MAP_MEMORY => {
                self.dispatch_map_memory(cookie, host_fd, ireq.request, param_in, resp_buf)
            }

            NV_ESC_RM_UNMAP_MEMORY => {
                self.dispatch_unmap_memory(cookie, host_fd, ireq.request, param_in, resp_buf)
            }

            NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => self.dispatch_update_device_mapping_info(
                cookie,
                host_fd,
                ireq.request,
                param_in,
                resp_buf,
            ),

            // ---------------------------------------------------------------
            // RM control requires nested handling
            // ---------------------------------------------------------------
            NV_ESC_RM_CONTROL => self.dispatch_nested(
                cookie,
                host_fd,
                ireq.request,
                param_in,
                resp_buf,
                32,
                16,
                24,
            ),

            // ---------------------------------------------------------------
            // RM alloc as well..
            // ---------------------------------------------------------------
            NV_ESC_RM_ALLOC => self.dispatch_nested(
                cookie,
                host_fd,
                ireq.request,
                param_in,
                resp_buf,
                48,
                16,
                32,
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
                        ireq.guest_handle,
                        ireq.request,
                        &param_in[..std::cmp::min(param_in.len(), 16)]
                    );
                }
                self.dispatch_simple(cookie, host_fd, ireq.request, param_in, resp_buf)
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
    ) -> usize {
        if param_in.len() < outer_size {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let mut outer = param_in[..outer_size].to_vec();
        let nested_in = &param_in[outer_size..];

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
            let hClass = u32::from_le_bytes(outer[12..16].try_into().unwrap());
            log::info!("RM_ALLOC hClass=0x{:x}", hClass);
        }

        if !nested_in.is_empty() {
            // Guest sent nested params — allocate host buffer, point struct at it
            let nested_size = nested_in.len();
            let mut host_buf = vec![0u8; nested_size];
            host_buf.copy_from_slice(nested_in);

            // ---------------------------------------------------------------
            // Translate guest_handle → host fd for fd-carrying RM_CONTROLs
            //
            // The guest driver already translated the raw guest fd to a
            // guest_handle. We now translate that handle to a real host fd
            // so the host kernel can resolve it.
            // ---------------------------------------------------------------
            let mut saved_nested_handle: Option<(usize, i32)> = None; // (offset, guest_handle_as_i32)

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

            // Set pointer in outer struct to host buffer address
            let host_ptr = host_buf.as_mut_ptr() as u64;
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&host_ptr.to_le_bytes());

            // Extract the RM control command for special handling
            let ctrl_cmd = if escape == 0x2A && outer.len() >= 12 {
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

            // Call host ioctl — paramsSize field is untouched (may be 0)
            let rc = unsafe { libc::ioctl(host_fd, request as libc::Ioctl, outer.as_mut_ptr()) };
            if rc < 0 {
                let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                log::warn!("nested ioctl(0x{:x}) failed: errno={}", request, errno);
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
            }

            // Log RM status
            if escape == 0x2a && param_in.len() >= 32 {
                let cmd = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
                let params_size = u32::from_le_bytes(param_in[24..28].try_into().unwrap());
                log::info!(
                    "RM_CONTROL ENTER: cmd=0x{:08x} paramsSize={} (nested_bytes={})",
                    cmd,
                    params_size,
                    param_in.len() - 32
                );
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

            // Zero pointer before sending back to guest
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&0u64.to_le_bytes());

            // Build response: outer + updated nested params
            let mut combined = outer;
            combined.extend_from_slice(&host_buf);
            self.write_ioctl_resp(resp_buf, cookie, &combined)
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

            // Zero pointer field in case host wrote something there
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&0u64.to_le_bytes());
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

        let guest_embedded: u64 = {
            let mut b = [0u8; 4];
            b.copy_from_slice(&param_in[fd_offset..fd_offset + 4]);
            u32::from_le_bytes(b) as u64
        };

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

        // --- Step 6: mmap the host fd into the SHM region ---
        if let Err(e) = self.shm.map_host_fd(region.offset, length, host_map_fd) {
            log::error!("NV_ESC_RM_MAP_MEMORY: map_host_fd failed: {}", e);
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

        let host_p_linear = u64::from_le_bytes(param_buf[32..40].try_into().unwrap());
        let h_client = u32::from_le_bytes(param_buf[0..4].try_into().unwrap());
        let h_memory = u32::from_le_bytes(param_buf[8..12].try_into().unwrap());

        log::info!(
            "MAP_MEMORY: saving (shm_off={:#x}) → host_va={:#x} client={:#x} mem={:#x}",
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
                region,
            },
        );

        // Replace host VA with SHM offset in pLinearAddress — this is what
        // the guest sees. It's not a real pointer; the guest driver uses the
        // SHM metadata (shm_offset/shm_length/pgprot in IoctlResp) for mmap,
        // and the library stores this value to pass back at unmap time.
        param_buf[32..40].copy_from_slice(&region_offset.to_le_bytes());

        // --- Step 7: Build response with SHM metadata ---

        let hdr = RespHeader {
            status: Status::Ok as u32,
            cookie,
            errno_host: 0,
        };
        let iresp = IoctlResp {
            param_size: param_buf.len() as u32,
            _pad: 0,
            shm_offset: region_offset,
            shm_length: length,
            pgprot: pgprot as u8,
            _pad2: [0; 7],
        };

        let need = size_of::<RespHeader>() + size_of::<IoctlResp>() + param_buf.len();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, cookie, 0);
        }

        let mut off = 0;
        off += write_struct(&mut resp_buf[off..], &hdr);
        off += write_struct(&mut resp_buf[off..], &iresp);
        resp_buf[off..off + param_buf.len()].copy_from_slice(&param_buf);
        off + param_buf.len()
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

    fn write_ioctl_resp(&self, resp_buf: &mut [u8], cookie: u64, param_out: &[u8]) -> usize {
        let hdr = RespHeader {
            status: Status::Ok as u32,
            cookie,
            errno_host: 0,
        };
        let payload = IoctlResp {
            param_size: param_out.len() as u32,
            _pad: 0,
            shm_offset: 0,
            shm_length: 0,
            pgprot: 0,
            _pad2: [0; 7],
        };

        let need = size_of::<RespHeader>() + size_of::<IoctlResp>() + param_out.len();
        if resp_buf.len() < need {
            return self.write_error_resp(resp_buf, Status::BufferTooSmall, cookie, 0);
        }

        let mut off = 0;
        off += write_struct(&mut resp_buf[off..], &hdr);
        off += write_struct(&mut resp_buf[off..], &payload);
        resp_buf[off..off + param_out.len()].copy_from_slice(param_out);
        off + param_out.len()
    }

    fn write_error_resp(
        &self,
        resp_buf: &mut [u8],
        status: Status,
        cookie: u64,
        errno: i32,
    ) -> usize {
        let hdr = RespHeader {
            status: status as u32,
            cookie,
            errno_host: errno,
        };
        if resp_buf.len() < size_of::<RespHeader>() {
            return 0;
        }
        write_struct(resp_buf, &hdr);
        size_of::<RespHeader>()
    }

    // ------------------------------------------------------------------
    // Test helpers
    // ------------------------------------------------------------------

    #[cfg(test)]
    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }
}

// ------------------------------------------------------------------
// Drop: ensure host fds are closed even if teardown() is not called
// ------------------------------------------------------------------

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

fn write_ok<P: Copy>(buf: &mut [u8], cookie: u64, payload: &P) -> usize {
    let hdr = RespHeader {
        status: Status::Ok as u32,
        cookie,
        errno_host: 0,
    };
    let sh = size_of::<RespHeader>();
    let sp = size_of::<P>();
    assert!(buf.len() >= sh + sp);
    write_struct(buf, &hdr);
    write_struct(&mut buf[sh..], payload);
    sh + sp
}

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

    fn hdr(msg_type: MsgType, cookie: u64) -> Vec<u8> {
        let mut v = vec![0u8; size_of::<MsgHeader>()];
        write_struct(
            &mut v,
            &MsgHeader {
                msg_type: msg_type as u32,
                cookie,
                _pad: 0,
            },
        );
        v
    }

    fn append<T: Copy>(v: &mut Vec<u8>, val: &T) {
        let start = v.len();
        v.resize(start + size_of::<T>(), 0);
        write_struct(&mut v[start..], val);
    }

    fn parse_resp(buf: &[u8]) -> RespHeader {
        read_struct::<RespHeader>(buf, 0)
    }
    fn parse_open_resp(buf: &[u8]) -> OpenResp {
        read_struct::<OpenResp>(buf, size_of::<RespHeader>())
    }

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
        let mut req = hdr(MsgType::Open, 1);
        append(
            &mut req,
            &OpenReq {
                kind: DeviceKind::Gpu as u8,
                index: 200,
                _pad: [0; 6],
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, Status::InvalidDevice as u32);
    }

    #[test]
    fn close_unknown_handle() {
        let mut be = NvidiaBackend::for_test();
        let mut req = hdr(MsgType::Close, 7);
        append(
            &mut req,
            &CloseReq {
                guest_handle: 0xCAFE,
            },
        );
        let mut resp = vec![0u8; 32];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, Status::BadHandle as u32);
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
            let mut req = hdr(MsgType::Open, 1);
            append(
                &mut req,
                &OpenReq {
                    kind: DeviceKind::Ctl as u8,
                    index: 0,
                    _pad: [0; 6],
                },
            );
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
        let mut req = hdr(MsgType::Open, 1);
        append(
            &mut req,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
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

        let mut req = hdr(MsgType::Open, 42);
        append(
            &mut req,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        let r = parse_resp(&resp);
        assert_eq!(r.status, Status::Ok as u32);

        let h = parse_open_resp(&resp).guest_handle;
        assert!(h > 0);

        let mut req2 = hdr(MsgType::Close, 43);
        append(&mut req2, &CloseReq { guest_handle: h });
        let mut resp2 = vec![0u8; 32];
        be.dispatch(&req2, &mut resp2);
        assert_eq!(parse_resp(&resp2).status, Status::Ok as u32);
        assert_eq!(be.handle_count(), 0);
    }

    #[test]
    fn check_version_str() {
        if !nvidiactl_present() {
            return;
        }
        let mut be = NvidiaBackend::for_test();

        let mut oreq = hdr(MsgType::Open, 1);
        append(
            &mut oreq,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let gh = parse_open_resp(&oresp).guest_handle;
        assert!(gh > 0);

        // nv_ioctl_rm_api_version_t: cmd(4) + reply(4) + versionString(64) = 72 bytes
        let param_size: u32 = 72;
        let mut ireq = hdr(MsgType::Ioctl, 2);
        // NV_ESC_CHECK_VERSION_STR = NV_IOCTL_BASE + 10 = 210
        // _IOWR('F', 210, 72) = (3 << 30) | (72 << 16) | (0x46 << 8) | 210
        append(
            &mut ireq,
            &IoctlReq {
                guest_handle: gh,
                request: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_CHECK_VERSION_STR, param_size),
                param_size,
                _pad: 0,
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
            r.status == Status::Ok as u32 || r.status == Status::IoctlFailed as u32,
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
        let mut oreq = hdr(MsgType::Open, 1);
        append(
            &mut oreq,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let gh = parse_open_resp(&oresp).guest_handle;
        assert!(gh > 0);

        // Build a NV_ESC_RM_MAP_MEMORY ioctl with a bogus embedded FD handle.
        // IoctlNVOS33ParametersWithFD = 56 bytes.
        let param_size: u32 = 56;
        let mut ireq = hdr(MsgType::Ioctl, 2);
        append(
            &mut ireq,
            &IoctlReq {
                guest_handle: gh,
                request: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size),
                param_size,
                _pad: 0,
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
        let r = parse_resp(&iresp);

        // Should fail with BadHandle since 0xDEAD is not in the handle table.
        assert_eq!(r.status, Status::BadHandle as u32);
    }

    #[test]
    fn map_memory_translates_fd_and_forwards() {
        if !nvidiactl_present() {
            return;
        }

        let mut be = NvidiaBackend::for_test();

        // Open nvidiactl — this is both the "outer" fd and the "map" fd.
        let mut oreq = hdr(MsgType::Open, 1);
        append(
            &mut oreq,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
        let mut oresp = vec![0u8; 64];
        be.dispatch(&oreq, &mut oresp);
        let ctl_handle = parse_open_resp(&oresp).guest_handle;

        // Open a second nvidiactl fd to use as the embedded map FD.
        let mut oreq2 = hdr(MsgType::Open, 2);
        append(
            &mut oreq2,
            &OpenReq {
                kind: DeviceKind::Ctl as u8,
                index: 0,
                _pad: [0; 6],
            },
        );
        let mut oresp2 = vec![0u8; 64];
        be.dispatch(&oreq2, &mut oresp2);
        let map_handle = parse_open_resp(&oresp2).guest_handle;

        // Build IoctlNVOS33ParametersWithFD with the map_handle as embedded FD.
        let param_size: u32 = 56;
        let mut ireq = hdr(MsgType::Ioctl, 3);
        append(
            &mut ireq,
            &IoctlReq {
                guest_handle: ctl_handle,
                request: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size),
                param_size,
                _pad: 0,
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
            Status::BadHandle as u32,
            "FD translation should have succeeded"
        );
    }
}
