//! IOCTL: the ABI check and the routing of each namespace to its handler.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------
    // IOCTL — top-level
    // ------------------------------------------------------------------

    /// Learn the host driver version from a successful `NV_ESC_CHECK_VERSION_STR`
    /// reply and select the ABI profile for it.
    ///
    /// Layout is `nv_ioctl_rm_api_version_t`: cmd (4), reply (4), then a
    /// NUL-terminated 64-byte version string.
    pub(super) fn learn_driver_version(&mut self, param_buf: &[u8]) {
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

    /// Refuse an RM_ALLOC whose class needs a capability this guest lacks.
    ///
    /// The ioctl succeeds and RM's own status word says INVALID_CLASS, which is
    /// what RM answers for a class the GPU does not have. Drivers probe for
    /// engines that way and fall back; an errno instead reads as a broken
    /// device. The host driver is not called.
    pub(super) fn refuse_alloc_class(
        &mut self,
        cookie: u64,
        class: u32,
        bit: u32,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        const NVOS64_STATUS: usize = 40;
        const NV_ERR_INVALID_CLASS: u32 = 0x22;
        let needs = if bit == crate::caps::VIDEO {
            "video"
        } else {
            "graphics"
        };
        self.refuse_for_caps(format!("RM_ALLOC class {class:#06x}"), needs);
        let mut out = param_in.to_vec();
        if out.len() < NVOS64_STATUS + 4 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        out[NVOS64_STATUS..NVOS64_STATUS + 4].copy_from_slice(&NV_ERR_INVALID_CLASS.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &out)
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

    pub(super) fn handle_ioctl(
        &mut self,
        cookie: u64,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
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

        // Only NVIDIA's own magic is described by the ABI tables; modeset and
        // uvm use different namespaces.
        if ioc_type == b'F' as u32 {
            // Only NVIDIA's own magic is described by the tables. UVM (type 0)
            // and modeset ('m') are forwarded with no equivalent check, which
            // is a gap and not a decision.
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
                // which is itself one of the first ioctls a client sends.
                // Refusing here would refuse the call that makes checking
                // possible at all.
                AbiCheck::Ok | AbiCheck::VariableLength | AbiCheck::NoProfile => false,
            };

            if refuse {
                *self.abi_refused.entry(escape).or_insert(0) += 1;
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
        }

        // nvidia-modeset ioctls: type 'm' (0x6d), nested pointer at offset 8, size at offset 4
        if ioc_type == 0x6d {
            // NVKMS multiplexes every operation through one ioctl number, so
            // the number says nothing and the command inside says everything.
            // Logged because two of them came back EPERM in a guest while the
            // same client on the host got zero for all of them, and an ioctl
            // number alone cannot say which two.
            if param_in.len() >= 4 {
                let nvkms_cmd = u32::from_le_bytes(param_in[0..4].try_into().unwrap());
                log::debug!("NVKMS cmd={nvkms_cmd} (0x{nvkms_cmd:x})");
            }
            // REGISTER_SURFACE carries one of our handles where NVKMS expects a
            // descriptor, because a guest's descriptor number means nothing
            // here. Told where it sits, the forwarder puts our own descriptor
            // back. See the driver's side of this, which explains why it only
            // shows up on some driver versions.
            let nvkms_fd_offset = if param_in.len() >= 4
                && u32::from_le_bytes(param_in[0..4].try_into().unwrap()) == NVKMS_REGISTER_SURFACE
            {
                Some(NVKMS_SURFACE_FD_OFFSET)
            } else {
                None
            };

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
                nvkms_fd_offset,
            );
        }

        // nvidia-drm's GEM ioctls: type 'd' (0x64), and `escape` is the
        // absolute DRM ioctl number, not an offset from DRM_COMMAND_BASE.
        //
        // Three of them carry a userspace pointer to an NVKMS parameter block,
        // in the same shape nvidia-modeset uses, so they take the same path;
        // the flat ones fall through to the passthrough below.
        //
        // A missing entry here does not refuse anything: the ioctl is
        // forwarded with the guest's own pointer still in it and a memFd that
        // means nothing in this process, and the host answers EINVAL from
        // somewhere far away. 0x49 was absent for exactly that reason and cost
        // a round of chasing the host's own dmesg to find. Nothing here translates the GEM handles in these structs: a
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
                0x49 => Some((24usize, 8usize, 16usize)), // GEM_EXPORT_NVKMS_MEMORY
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
        // UVM_INITIALIZE goes with the backend's flags, not the guest's
        // (uvm_init_flags). Before the host release is known nothing says which
        // bits it takes, so the call is refused: a CUDA client learns the
        // release on nvidiactl before it opens UVM.
        if request == UVM_INITIALIZE_CMD {
            let Some(host) = self.driver else {
                log::warn!("UVM_INITIALIZE before the host driver release is known; refused");
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
            };
            if param_in.len() < 16 {
                return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
            }
            let asked = u64::from_le_bytes(param_in[0..8].try_into().unwrap());
            let flags = uvm_init_flags(host);
            if asked != flags {
                log::info!("UVM_INITIALIZE: guest asked flags {asked:#x}, sent {flags:#x}");
            }
            let mut p = param_in.to_vec();
            p[0..8].copy_from_slice(&flags.to_le_bytes());
            return self.dispatch_simple(cookie, host_fd, request, &p, resp_buf);
        }

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

            NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => self
                .dispatch_update_device_mapping_info(cookie, host_fd, request, param_in, resp_buf),

            // ---------------------------------------------------------------
            // RM control requires nested handling
            // ---------------------------------------------------------------
            NV_ESC_RM_CONTROL => {
                // Counted here rather than in the forwarder, which holds only a
                // shared borrow. NVOS54: hClient, hObject, cmd at byte 8.
                if param_in.len() >= 12 {
                    let cmd = u32::from_le_bytes(param_in[8..12].try_into().unwrap());
                    *self.rm_controls.entry(cmd).or_insert(0) += 1;
                }
                self.dispatch_nested(
                    cookie, host_fd, request, param_in, resp_buf, 32, 16, 24, deep_in, None,
                )
            }

            // ---------------------------------------------------------------
            // RM alloc as well..
            // ---------------------------------------------------------------
            NV_ESC_RM_ALLOC => {
                // NVOS64: hRoot, hObjectParent, hObjectNew, hClass at byte 12.
                if param_in.len() >= 16 {
                    let class = u32::from_le_bytes(param_in[12..16].try_into().unwrap());
                    *self.rm_classes.entry(class).or_insert(0) += 1;
                    if let Some(bit) = crate::caps::Caps::for_class(class) {
                        if !self.caps.has(bit) {
                            return self.refuse_alloc_class(cookie, class, bit, param_in, resp_buf);
                        }
                    }
                }
                self.dispatch_nested(
                    cookie, host_fd, request, param_in, resp_buf, 48, 16, 32, deep_in, None,
                )
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
                self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf)
            }
        }
    }
}
