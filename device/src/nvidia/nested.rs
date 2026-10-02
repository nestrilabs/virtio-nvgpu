//! Ioctls whose parameters point at a second block: RM_CONTROL, RM_ALLOC,
//! and the NVKMS and DRM calls shaped like them.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------
    // Nested-pointer ioctl (RM_CONTROL, RM_ALLOC..)
    // ------------------------------------------------------------------

    pub(super) fn dispatch_nested(
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

        // NVOS64 carries a second pointer: `pRightsRequested`, an access mask
        // RM reads from the caller's address space when it is not null. The
        // guest's value there is an address in this process, and nothing in
        // this project asks for rights, so the host sees null and the caller
        // gets its own value back.
        const NVOS64_RIGHTS: usize = 24;
        let rights = if escape == 0x2b && outer.len() >= NVOS64_RIGHTS + 8 {
            let saved: [u8; 8] = outer[NVOS64_RIGHTS..NVOS64_RIGHTS + 8]
                .try_into()
                .expect("just checked the length");
            if saved != [0u8; 8] {
                log::debug!("RM_ALLOC: pRightsRequested sent as null, not the guest's value");
            }
            outer[NVOS64_RIGHTS..NVOS64_RIGHTS + 8].fill(0);
            Some(saved)
        } else {
            None
        };

        // Log RM_CONTROL/RM_ALLOC for debugging Vulkan init
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
            let mut host_guard = match crate::guarded::GuardPool::lease(&self.guards, nested_size) {
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
            // and puts the guest's value back before replying.
            if escape == 0x2B && outer.len() >= 16 {
                let h_class = u32::from_le_bytes(outer[12..16].try_into().unwrap());
                const NV0005_DATA: usize = 16;
                if matches!(h_class, 0x05 | 0x79) && host_buf.len() >= NV0005_DATA + 4 {
                    let guest_handle_val = i32::from_le_bytes(
                        host_buf[NV0005_DATA..NV0005_DATA + 4].try_into().unwrap(),
                    );
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

            // Pointers RM dereferences inside the parameter block.
            //
            // For the controls `abi::rmctrl` describes, nothing a guest put in
            // a pointer field is forwarded: the length comes from the count in
            // the guest's own block, the buffer is this process's, and the
            // guest's value goes back into the field before the reply. The
            // bytes a guest wants read travel as segments of the deep block.
            // See `rmctrl.rs`.
            let segs = deep_in.and_then(|(off, bytes)| {
                (off as u32 == protocol::segments::SEGMENTED)
                    .then(|| protocol::segments::Segments::parse(bytes))
                    .flatten()
            });
            // A control this release's table describes. When the table is an
            // older release's, a control *any* release describes and this one
            // does not is refused: a newer release can copy through a pointer
            // the older table is silent about, and silence is not a reading.
            let mut drift_refusal = None;
            let described = if escape == 0x2a && outer.len() >= NVOS54_CMD + 4 {
                let cmd = u32::from_le_bytes(outer[NVOS54_CMD..NVOS54_CMD + 4].try_into().unwrap());
                let found = self
                    .rmctrl
                    .and_then(|sel| abi::rmctrl::lookup(sel.table, cmd));
                // No table at all is the one case where nothing can be said
                // about any control, so nothing is forwarded. It cannot happen
                // through `set_host_driver_version`, which refuses to start
                // without one, but this crate is built into other VMMs and a
                // caller that never calls it would otherwise get the behaviour
                // M3 removed.
                if self.rmctrl.is_none() {
                    log::warn!(
                        "RM_CONTROL cmd={cmd:#010x}: no RM pointer table for this host, so \
                         nothing can be said about the pointers in it; refused"
                    );
                    drift_refusal = Some(rmctrl::NV_ERR_NOT_SUPPORTED);
                } else if found.is_none()
                    && self.rmctrl.is_some_and(|sel| !sel.exact)
                    && abi::rmctrl::in_any_table(cmd)
                {
                    log::warn!(
                        "RM_CONTROL cmd={cmd:#010x}: another release describes a pointer in it \
                         and this host's table does not; refused"
                    );
                    drift_refusal = Some(rmctrl::NV_ERR_NOT_SUPPORTED);
                }
                found
            } else {
                None
            };

            if let Some(status) = drift_refusal {
                if outer.len() >= NVOS54_TOTAL {
                    outer[NVOS54_STATUS..NVOS54_STATUS + 4].copy_from_slice(&status.to_le_bytes());
                }
                outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);
                if let Some(saved) = rights {
                    outer[NVOS64_RIGHTS..NVOS64_RIGHTS + 8].copy_from_slice(&saved);
                }
                let mut combined = outer;
                combined.extend_from_slice(host_buf);
                return self.write_ioctl_resp_deep(resp_buf, cookie, &combined, 0);
            }
            // A guest from v0.1 describes one pointer in the request struct
            // instead of sending a segment table. It is read as the segment it
            // is, so a backend ahead of the module in a rootfs still serves
            // the call.
            let legacy = deep_in.filter(|(off, _)| *off as u32 != protocol::segments::SEGMENTED);
            let mut embedded: Option<rmctrl::Embedded> = None;
            if let Some(entry) = described {
                let have = deep_in.map(|(_, b)| b.len()).unwrap_or(0);
                let refused = match rmctrl::plan(entry, host_buf, segs, legacy) {
                    Err(status) => Some(status),
                    Ok(slots) if !rmctrl::reply_fits(&slots, have, segs.is_some()) => {
                        log::warn!(
                            "RM_CONTROL cmd={:#010x}: the guest left {have} bytes for what RM \
                             writes back, which does not hold it",
                            entry.cmd
                        );
                        Some(rmctrl::NV_ERR_NOT_SUPPORTED)
                    }
                    Ok(slots) => {
                        match rmctrl::Embedded::install(
                            &slots,
                            host_buf,
                            segs,
                            legacy,
                            &self.guards,
                        ) {
                            Ok(e) => {
                                embedded = Some(e);
                                None
                            }
                            Err(status) => Some(status),
                        }
                    }
                };
                if let Some(status) = refused {
                    // RM's own refusal, in the status word of the parameter
                    // block, with the ioctl itself succeeding. An errno here
                    // reads to a driver as the call never having happened, and
                    // it retries or hangs rather than doing without the
                    // feature (fork gotcha 6).
                    log::warn!(
                        "RM_CONTROL cmd={:#010x}: refused, status={status:#x}",
                        entry.cmd
                    );
                    if outer.len() >= NVOS54_TOTAL {
                        outer[NVOS54_STATUS..NVOS54_STATUS + 4]
                            .copy_from_slice(&status.to_le_bytes());
                    }
                    outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);
                    let mut combined = outer;
                    combined.extend_from_slice(host_buf);
                    return self.write_ioctl_resp_deep(resp_buf, cookie, &combined, 0);
                }
            }

            // Give the pointer inside the nested block a host address.
            //
            // The buffer has to outlive the call, and the guest's own pointer
            // value has to go back in afterwards: userspace compares what it
            // gets back with what it sent, and a host address there is both
            // meaningless and a leak of our layout.
            let mut deep_buf: Vec<u8> = Vec::new();
            let mut deep_saved: Option<(usize, [u8; 8])> = None;
            if let Some((ptr_off, bytes)) = deep_in.filter(|_| embedded.is_none() && segs.is_none())
            {
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
                let status =
                    u32::from_le_bytes(outer[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap());
                if status == NV_OK {
                    log::debug!(
                        "RM_CONTROL cmd=0x{:08x} paramsSize={} -> NV_OK",
                        cmd,
                        params_size
                    );
                } else if log::log_enabled!(log::Level::Debug) {
                    // A refusal tells us nothing on its own; the argument RM
                    // objected to is in the params. Show the head of them.
                    // Debug, not warn: drivers probe for features by asking
                    // and being refused, so this is the normal path, and it
                    // ran on every such call.
                    let head: Vec<String> = host_buf
                        .iter()
                        .take(64)
                        .map(|b| format!("{b:02x}"))
                        .collect();
                    log::debug!(
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

            // The caller's own pointer value goes back, not ours and not zero.
            outer[ptr_offset..ptr_offset + 8].copy_from_slice(&caller_ptr);
            if let Some(saved) = rights {
                outer[NVOS64_RIGHTS..NVOS64_RIGHTS + 8].copy_from_slice(&saved);
            }

            // Build response: outer + updated nested params + what the
            // pointer inside them addresses.
            if let Some((ptr_off, guest_ptr)) = deep_saved {
                host_buf[ptr_off..ptr_off + 8].copy_from_slice(&guest_ptr);
            }
            // Only what the guest allocated room for goes back, not the pad.
            let mut deep_reply = deep_in.map(|(_, b)| b.len()).unwrap_or(0);

            // Each buffer RM wrote goes back as its own segment, at the offset
            // of the pointer that named it, so the guest driver can put each
            // one where its caller's own pointer addresses.
            if let Some(e) = &embedded {
                e.restore(host_buf);
                let out = e.reply();
                if out.is_empty() {
                    deep_reply = 0;
                } else if segs.is_none() {
                    // A v0.1 guest copies the deep block straight back to its
                    // one pointer, so it goes back raw. `reply_fits` allowed
                    // this only for a single buffer that fits.
                    deep_buf = out[0].1.to_vec();
                    deep_reply = deep_buf.len();
                } else {
                    deep_buf = vec![0u8; deep_reply];
                    if protocol::segments::encode(&mut deep_buf, &out).is_none() {
                        // `reply_fits` said it would. Send an empty table
                        // rather than a partial one: the guest then copies
                        // nothing back, instead of copying something shaped
                        // like an answer.
                        log::error!("RM_CONTROL: {} bytes did not hold the reply", deep_reply);
                        deep_buf.fill(0);
                    }
                }
            }
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
            if let Some(saved) = rights {
                outer[NVOS64_RIGHTS..NVOS64_RIGHTS + 8].copy_from_slice(&saved);
            }
            self.write_ioctl_resp(resp_buf, cookie, &outer)
        }
    }
}
