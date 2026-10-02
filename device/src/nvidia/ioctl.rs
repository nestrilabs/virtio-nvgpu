//! IOCTL: the ABI check and the routing of each namespace to its handler.

use super::*;

/// `NV01_ROOT`, `NV01_ROOT_NON_PRIV` and `NV01_ROOT_CLIENT`: the classes
/// whose allocation makes a client.
const ROOT_CLASSES: [u32; 3] = [0x0, 0x1, 0x41];

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
        // The RM pointer table goes with it. A caller that learns the release
        // this way rather than through `set_host_driver_version` -- another
        // VMM embedding this crate -- would otherwise have an ABI profile and
        // no pointer table, and every control that carries a pointer would go
        // through undescribed, which is what this crate stopped doing in M3.
        self.rmctrl = abi::rmctrl::select(v);
        self.rmallow = abi::rmallow::select(v);
        self.uvm = abi::uvm::select(v);
        self.osdesc = abi::osdesc::select(v);
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

    /// Whether RM exports this control to an unprivileged caller.
    ///
    /// Returns why not, for the log and the teardown tally. The order matters:
    /// the deny list is consulted before the table, because every entry on it
    /// *is* in the table -- RM marks them non-privileged and means it.
    fn rm_control_refusal(&self, cmd: u32, declared: u32, sent: u32) -> Option<String> {
        if let Some(name) = abi::rmallow::denied(cmd) {
            return Some(format!("{name} answers about the host, not this guest"));
        }
        // No allowlist at all means nothing can be said about any control.
        // Unreachable through `set_host_driver_version`, which refuses to
        // start without one; reachable by another VMM embedding this crate.
        let Some(sel) = self.rmallow else {
            return Some("no RM allowlist for this host".into());
        };
        let Some(rule) = sel.ctrl_rule(cmd) else {
            return Some(if sel.exact {
                "RM does not serve it to an unprivileged caller".into()
            } else {
                // Either RM never served it, or the table is an older
                // release's and the releases disagree. Both end here.
                "no release this backend knows serves it to an unprivileged caller".into()
            });
        };
        match rule {
            // RM sizes this from its own descriptor, so the block has to be
            // exactly that size and the guest has to have sent all of it.
            abi::rmallow::CtrlAllow::Exact(want) => {
                if declared != want {
                    return Some(format!(
                        "parameters are declared {declared} bytes, RM's are {want}"
                    ));
                }
            }
            // RM reads nothing here -- it hands the block to GSP firmware --
            // so there is no size to require, only the ceiling RM puts on a
            // parameter copy for an unprivileged caller.
            abi::rmallow::CtrlAllow::UpTo(max) => {
                if declared > max {
                    return Some(format!(
                        "parameters are declared {declared} bytes, more than the {max} RM copies"
                    ));
                }
            }
        }
        (sent != declared)
            .then(|| format!("parameters are declared {declared} bytes and {sent} arrived"))
    }

    /// Whether RM lets an unprivileged caller allocate this class.
    ///
    /// `have` is the length of the allocation parameters the guest actually
    /// sent -- the nested block, which is what `pAllocParms` addresses -- and
    /// not the `paramsSize` in its NVOS64. RM never reads that field to size an
    /// allocation: `NV_ESC_RM_ALLOC` also accepts NVOS21, which has no such
    /// field, and RM takes the size from its own resource descriptor either
    /// way. NVIDIA's userspace leaves it zero, so checking it refused every
    /// workload at its first VA space (`FERMI_VASPACE_A`).
    fn rm_class_refusal(&self, class: u32, have: usize) -> Option<String> {
        let Some(sel) = self.rmallow else {
            return Some("no RM allowlist for this host".into());
        };
        let Some(entry) = sel.class_entry(class) else {
            return Some(if sel.exact {
                "RM does not let an unprivileged caller allocate it".into()
            } else {
                "no release this backend knows lets an unprivileged caller allocate it".into()
            });
        };
        // RS_OPTIONAL means the parameters may be absent entirely; RS_REQUIRED
        // means they may not. Either way, if they are there they are the size
        // RM's own struct is: RM reads that many bytes through pAllocParms, so
        // a shorter block is a read past what the guest sent.
        if have == 0 {
            return entry
                .params_required
                .then(|| "RM requires allocation parameters and none were sent".to_string());
        }
        (have != entry.params_size as usize).then(|| {
            format!(
                "allocation parameters are {have} bytes, RM's are {}",
                entry.params_size
            )
        })
    }

    /// Answer an RM_CONTROL the allowlist refused, without calling the host.
    ///
    /// The status goes in the parameter block and the ioctl succeeds. An errno
    /// here makes NVIDIA's userspace retry or hang; a status it reads is an
    /// answer it knows what to do with.
    pub(super) fn refuse_control(
        &mut self,
        cookie: u64,
        cmd: u32,
        why: String,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        const NVOS54_STATUS: usize = 28;
        const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
        self.note_allow_refusal(format!("RM_CONTROL cmd {cmd:#010x}"), why);
        let mut out = param_in.to_vec();
        if out.len() < NVOS54_STATUS + 4 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        out[NVOS54_STATUS..NVOS54_STATUS + 4].copy_from_slice(&NV_ERR_NOT_SUPPORTED.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &out)
    }

    /// Answer an RM_ALLOC the allowlist refused, without calling the host.
    pub(super) fn refuse_alloc(
        &mut self,
        cookie: u64,
        class: u32,
        why: String,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        const NV_ERR_INVALID_CLASS: u32 = 0x22;
        self.note_allow_refusal(format!("RM_ALLOC class {class:#06x}"), why);
        let mut out = param_in.to_vec();
        if out.len() < NVOS64_STATUS + 4 {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }
        out[NVOS64_STATUS..NVOS64_STATUS + 4].copy_from_slice(&NV_ERR_INVALID_CLASS.to_le_bytes());
        self.write_ioctl_resp(resp_buf, cookie, &out)
    }

    /// Count a refusal, and say why the first time.
    pub(super) fn note_allow_refusal(&mut self, what: String, why: String) {
        let n = self.allow_refused.entry(what.clone()).or_insert(0);
        *n += 1;
        if *n == 1 {
            log::warn!("{what} refused: {why}");
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

    pub(super) fn handle_ioctl(
        &mut self,
        cookie: u64,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let n = self.serve_ioctl(cookie, payload, resp_buf);
        self.note_clients(payload, &resp_buf[..n]);
        self.note_registrations(payload, &resp_buf[..n]);
        n
    }

    /// Release the guest memory a freed object had registered.
    ///
    /// Separate from `note_clients`, and deliberately not restricted to the
    /// control file: `NV_ESC_RM_ALLOC_MEMORY` is `NV_ACTUAL_DEVICE_ONLY` in
    /// NVIDIA's escape layer, so a registration can be made on `/dev/nvidia0`
    /// and freed on `/dev/nvidiactl`. Keyed on RM's handles, which say the
    /// same thing on either file.
    fn note_registrations(&mut self, payload: &[u8], resp: &[u8]) {
        if self.registrations.is_empty() {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'F' as u64 || request & 0xFF != 0x29 {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        // NVOS00: hRoot, hObjectParent, hObjectOld, status.
        let out = &resp[head..];
        if out.len() < 16 {
            return;
        }
        let word = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        if word(12) == NV_OK {
            self.release_registrations(word(0), word(8));
        }
    }

    /// Keep the record of which RM clients were made on which control file,
    /// read from what RM answered rather than what the guest asked: a root
    /// allocation's handle is RM's to choose, and only a call that succeeded
    /// made or freed anything.
    ///
    /// UVM calls name a control file and a client side by side, and RM takes
    /// the pair on trust from UVM. This record is what lets the backend say
    /// the client is one this VM was given on that file.
    fn note_clients(&mut self, payload: &[u8], resp: &[u8]) {
        let file = self.current_handle as u64;
        if self.handle_kinds.get(&file) != Some(&DeviceKind::Ctl) {
            return;
        }
        let req = read_struct::<IoctlReq>(payload, 0);
        let request = req.cmd as u64;
        if (request >> 8) & 0xFF != b'F' as u64 {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let out = &resp[head..];
        let word = |at: usize| u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
        match (request & 0xFF, out.len()) {
            // NVOS21 is 32 bytes with status at 28; NVOS64 is 48 with status
            // at 40. Both have hObjectNew at 8 and hClass at 12.
            (0x2b, n) if n >= 32 => {
                let status = if n >= 48 { word(40) } else { word(28) };
                if status == NV_OK && ROOT_CLASSES.contains(&word(12)) {
                    self.vram.client_opened(file, word(8));
                }
            }
            // NVOS00: hRoot, hObjectParent, hObjectOld, status. Freeing the
            // root frees the client.
            (0x29, n) if n >= 16 => {
                if word(12) == NV_OK && word(8) == word(0) {
                    self.vram.client_freed(word(0));
                }
            }
            _ => {}
        }
    }

    fn serve_ioctl(&mut self, cookie: u64, payload: &[u8], resp_buf: &mut [u8]) -> usize {
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
        let on_uvm = self.handle_kinds.get(&(self.current_handle as u64)) == Some(&DeviceKind::Uvm);
        let ns = match ioc_type {
            _ if on_uvm => 'u',
            x if x == b'F' as u32 => 'F',
            x if x == b'd' as u32 => 'd',
            x if x == b'm' as u32 => 'm',
            _ => '?',
        };
        // UVM is counted by its whole number, not by the low byte: 0x30000001
        // and 1 are different commands that share one. Everywhere else the low
        // byte is the escape and the rest is encoding.
        let counted = if on_uvm { request as u32 } else { escape };
        *self.ioctls_by_ns.entry((ns, counted)).or_insert(0) += 1;

        // A UVM file takes UVM's numbering, and the number alone cannot say so:
        // UVM_INITIALIZE is 0x30000001 and UVM_RESERVE_VA is 1, and the two
        // agree in every byte an ioctl type is read from. The file the guest
        // sent it on is what distinguishes them, so that is what decides.
        if on_uvm {
            return self.dispatch_uvm(cookie, host_fd, request, param_in, resp_buf);
        }

        // Only NVIDIA's own magic is described by the ABI tables; modeset uses
        // a different namespace.
        if ioc_type == b'F' as u32 {
            // Only NVIDIA's own magic is described by the tables. Modeset
            // ('m') is forwarded with no equivalent check, which is a gap and
            // not a decision. UVM has a table of its own: see `dispatch_uvm`.
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
                // The older of the two allocation escapes, and it carries its
                // class inside the parameters rather than beside them.
                if let Some(route) = self.registration_by_address(escape, None, param_in) {
                    return self.serve_registration(
                        cookie, host_fd, request, route, 0, param_in, deep_in, resp_buf,
                    );
                }
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
                    // RM's own rule, applied on the guest's behalf. Both
                    // numbers are checked: RM sizes the copy from NVOS54's
                    // paramsSize at byte 24, so that has to be RM's size, and
                    // the guest has to have actually sent that many bytes, or
                    // RM reads past the end of what arrived.
                    let declared = if param_in.len() >= 28 {
                        u32::from_le_bytes(param_in[24..28].try_into().unwrap())
                    } else {
                        0
                    };
                    let sent = ireq.nested_len;
                    if let Some(why) = self.rm_control_refusal(cmd, declared, sent) {
                        return self.refuse_control(cookie, cmd, why, param_in, resp_buf);
                    }
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
                    // The cap is a decision somebody made; this is RM's. Both
                    // have to pass. The allocation parameters are the nested
                    // block, the bytes `pAllocParms` addresses. `deep_in` is
                    // one level further in: a pointer inside those parameters.
                    let have = ireq.nested_len as usize;
                    if let Some(why) = self.rm_class_refusal(class, have) {
                        return self.refuse_alloc(cookie, class, why, param_in, resp_buf);
                    }
                    // RM marks this class non-privileged, so the allowlist
                    // above lets it through: RM is right, for a caller whose
                    // address space is its own. Here it is not.
                    let nested = &param_in[(ireq.data_len as usize).min(param_in.len())..];
                    if let Some(route) = self.registration_by_address(escape, Some(class), nested) {
                        return self.serve_registration(
                            cookie,
                            host_fd,
                            request,
                            route,
                            ireq.data_len as usize,
                            param_in,
                            deep_in,
                            resp_buf,
                        );
                    }
                }
                self.dispatch_nested(
                    cookie, host_fd, request, param_in, resp_buf, 48, 16, 32, deep_in, None,
                )
            }

            // The heap ioctl is a union, and exactly one of its functions
            // names memory by a CPU address. The other twenty are ordinary
            // heap operations and go through as they always have.
            NV_ESC_RM_VID_HEAP_CONTROL => {
                if let Some(route) = self.registration_by_address(escape, None, param_in) {
                    return self.serve_registration(
                        cookie, host_fd, request, route, 0, param_in, deep_in, resp_buf,
                    );
                }
                self.dispatch_simple(cookie, host_fd, request, param_in, resp_buf)
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
