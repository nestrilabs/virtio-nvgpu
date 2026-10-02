//! OPEN and CLOSE: host descriptors in and out of the handle table.

use super::*;

impl NvidiaBackend {
    // ------------------------------------------------------------------
    // OPEN
    // ------------------------------------------------------------------

    pub(super) fn handle_open(
        &mut self,
        cookie: u64,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        if payload.len() < size_of::<OpenReq>() {
            return self.write_error_resp(resp_buf, Status::InvalidDevice, cookie, 0);
        }
        let req = read_struct::<OpenReq>(payload, 0);

        // ENODEV, before any host open: the same answer a host without the
        // device gives, which every client already handles.
        use crate::caps::{COMPUTE, GRAPHICS};
        let refusal = match DeviceKind::from_device_type(req.device_type) {
            Some(DeviceKind::UvmTools) => Some(("open nvidia-uvm-tools", "(none: never served)")),
            Some(DeviceKind::Uvm) if !self.caps.has(COMPUTE) => {
                Some(("open nvidia-uvm", "compute"))
            }
            Some(DeviceKind::Modeset) if !self.caps.has(GRAPHICS) => {
                Some(("open nvidia-modeset", "graphics"))
            }
            Some(DeviceKind::Dri(_)) if !self.caps.has(GRAPHICS) => {
                Some(("open render node", "graphics"))
            }
            _ => None,
        };
        if let Some((what, needs)) = refusal {
            self.refuse_for_caps(what.to_string(), needs);
            return self.write_error_resp(resp_buf, Status::OpenFailed, cookie, libc::ENODEV);
        }

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
        // The guest may wait on this descriptor, and only the host's copy ever
        // becomes readable. Offered to the transport as watchable; duplicated
        // there rather than here, so the watch cannot outlive this table's fd.
        self.watch_added.push((guest_handle as u32, raw_fd));
        log::info!("open {:?} -> handle={guest_handle} (fd={raw_fd})", path);

        // The handle is returned in the header. The driver reads it from there
        // and there is no response payload at all.
        self.write_hdr(resp_buf, guest_handle as u32, 0)
    }

    // ------------------------------------------------------------------
    // CLOSE
    // ------------------------------------------------------------------

    pub(super) fn handle_close(
        &mut self,
        cookie: u64,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
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
                self.watch_removed.push(handle as u32);
                self.write_hdr(resp_buf, 0, 0)
            }
            Err(_) => self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0),
        }
    }
}
