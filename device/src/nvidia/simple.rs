//! Ioctls forwarded as one flat parameter block.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------
    // Simple ioctl
    // ------------------------------------------------------------------

    pub(super) fn dispatch_simple(
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

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            // Special handling: NV_ESC_SYS_PARAMS (0xd6) - retry on EBUSY
            if escape == 0xd6 && errno == libc::EBUSY && retry_with_v2 {
                log::debug!("NV_ESC_SYS_PARAMS: got EBUSY, retrying with Cmd=2");
                param_buf[0] = 2; // Try V2
                if let Err(errno2) = self.host.ioctl(host_fd, request, &mut param_buf) {
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
                        log::debug!("CHECK_VERSION_STR response: {:?}", version);
                    }
                    0xc8 => {
                        log::debug!("CARD_INFO response[0..128]: {:02x?}", preview);
                    }
                    0xd6 => {
                        log::debug!("SYS_PARAMS response[0..128]: {:02x?}", preview);
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
                        log::debug!(
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
                        log::debug!(
                            "RM_ALLOC response: status={:#x}, data[4..32]={:02x?}",
                            status,
                            &param_buf[4..std::cmp::min(32, param_buf.len())]
                        );
                    }
                    _ => {}
                }
            }
            if escape == 0x57 || escape == 0x58 {
                log::debug!(
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
                log::debug!(
                    "VID_HEAP_CONTROL: response[{}]={:02x?}",
                    param_buf.len(),
                    &param_buf[..std::cmp::min(param_buf.len(), 184)]
                );
            }
        }
        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }
}
