// SPDX-License-Identifier: Apache-2.0
//! v1 UVM commands: the size each takes on the host's release, the
//! descriptor and RM client some name, the pools and pageable access UVM
//! reports back.

#![forbid(unsafe_code)]

use super::*;

impl NvidiaBackend {
    /// A v1 UVM command, on a UVM or UVM-tools file.
    pub(super) fn serve_uvm_v1(
        &mut self,
        host_fd: RawFd,
        kind: HandleKind,
        req: &V1Request<'_>,
    ) -> V1 {
        let (ireq, param_in) = (req.ireq, req.params);
        let request = u64::from(ireq.cmd);
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
            return Err(libc::EPERM);
        }
        let init_flags_mask = self
            .driver
            .and_then(abi::schema::uvm_table)
            .map_or(0, |t| t.init_flags_mask);
        let params = param_in;
        let mut plan = crate::guestptr::uvm_gate(tools, ireq.cmd, params, init_flags_mask)?;
        // Exactly the block the host's UVM copies each way
        // (abi::schema::uvm_table, the table the guest sizes the
        // call by): UVM's numbers carry no size, so a short block
        // would have the host read and write past what the guest
        // sent, and a long one is not this release's command.
        self.uvm_size_ok(ireq.cmd, params.len())?;
        // A semaphore pool is host kernel memory the moment UVM makes
        // it: its length and the budgets are checked first, not only
        // whether it may later be mapped (uvmmap.rs, F1).
        self.uvm_maps
            .set_owner(self.current_handle, self.handles.owner(self.current_handle));
        // UVM_ALLOC_SEMAPHORE_POOL_PARAMS: base, length, ..., rmStatus.
        if ireq.cmd == crate::uvmmap::ALLOC_SEMAPHORE_POOL
            && let Some(len) = le::u64_at(params, 8)
        {
            // Refused in rmStatus, 8 bytes from the block's end, the
            // ioctl succeeding: UVM's own way to say it.
            if let Err(errno) = self.uvm_maps.admit_pool(self.current_handle, len) {
                let status = match errno {
                    libc::ENOMEM => NV_ERR_NO_MEMORY,
                    _ => NV_ERR_INVALID_ARGUMENT,
                };
                let out = nvos::with_status(params, params.len() - 8, status);
                return Ok(IoctlOut::ok(out));
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
            Err(errno) => return Err(errno),
        }
        // The RM client whose objects UVM would duplicate: one this
        // VM made on the control file the call names (uvm_client_ok).
        self.uvm_client_ok(ireq.cmd, params)?;
        // Registered memory UVM would keep a duplicate of
        // (osdesc.rs): refused, or followed.
        let osdesc_map = self.osdesc_uvm_before(ireq.cmd, params)?;
        let mut r = self.dispatch_simple(host_fd, request, params, &plan);
        self.osdesc_uvm_after(ireq.cmd, params, osdesc_map, served(&r));
        // UVM puts its NV_STATUS in the block, not in the ioctl's
        // return; this is the only place it shows.
        log::debug!(
            "UVM {:#x} on handle {}: reply block {:02x?}",
            ireq.cmd,
            self.current_handle,
            served(&r).unwrap_or(&[])
        );
        if ireq.cmd == crate::guestptr::UVM_INITIALIZE
            && init_flags_mask & crate::guestptr::UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS == 0
            && let Some(out) = served_mut(&mut r)
        {
            self.uvm_pageable_off(host_fd, out);
        }
        self.uvm_observe(ireq.cmd, params, served(&r), init_flags_mask);
        r
    }

    /// Whether `len` is the size of UVM command `cmd`'s parameters on the
    /// host's release: EPERM for a command this release has no row for (or
    /// a host with no table at all), EINVAL for another size.
    pub(super) fn uvm_size_ok(&self, cmd: u32, len: usize) -> std::result::Result<(), i32> {
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
            // uvm_test_ioctl), not a permission.
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
    pub(super) fn uvm_pageable_off(&mut self, host_fd: RawFd, reply: &mut [u8]) {
        // UVM_INITIALIZE_PARAMS {NvU64 flags; NV_STATUS rmStatus;}
        let st = 8;
        match le::u32_at(reply, st) {
            Some(NV_OK) => {}
            // Not initialised, or no answer: nothing to check.
            _ => return,
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
        // UVM_PAGEABLE_MEM_ACCESS_PARAMS {NvBool pageableMemAccess; NV_STATUS
        // rmStatus;}
        let q = a.bytes(top);
        let (access, status) = (q[0], le::u32_at(q, 4).expect("in the block"));
        if rc == 0 && status == NV_OK && access == 0 {
            return;
        }
        log::warn!(
            "UVM handle {}: pageable memory access is not off after UVM_INITIALIZE \
             (ioctl {rc}, rmStatus {status:#x}, pageableMemAccess {access}); refusing the file",
            self.current_handle
        );
        self.uvm_refused.insert(self.current_handle);
        le::put_u32(reply, st, NV_ERR_NOT_SUPPORTED).expect("read above");
    }

    /// What a UVM call that went through means for the aperture: a VA space
    /// in sharing mode, a semaphore pool made, a range freed. `sent` is the
    /// block the host was handed (the guest's, with our changes); success is
    /// the ioctl's and UVM's own status in the block, at `size - 8` for all
    /// three on every release.
    pub(super) fn uvm_observe(
        &mut self,
        cmd: u32,
        sent: &[u8],
        reply: Option<&[u8]>,
        init_flags_mask: u64,
    ) {
        use crate::uvmmap::{ALLOC_SEMAPHORE_POOL, FREE};
        let size = sent.len();
        let Some(reply) = reply else {
            return;
        };
        if size < 16 || reply.len() < size || le::u32_at(reply, size - 8) != Some(NV_OK) {
            return;
        }
        let word = |off: usize| le::u64_at(sent, off).expect("16 bytes at least");
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
    pub(super) fn uvm_fd_in(
        &self,
        cmd: u32,
        params: &[u8],
    ) -> std::result::Result<Option<(usize, u32)>, i32> {
        let Some(field) = crate::uvmfd::field(self.driver, cmd) else {
            return Ok(None);
        };
        let off = field.offset as usize;
        let want = match field.of {
            crate::uvmfd::FdOf::RmCtl => HandleKind::Dev(DeviceKind::Ctl),
            crate::uvmfd::FdOf::Uvm => HandleKind::Dev(DeviceKind::Uvm),
        };
        let f = FdField {
            at: off,
            width: 4,
            accept: FdAccept::Kind(want),
            none: FdNone::Negative,
        };
        match self.fd_field(params, f) {
            Ok(FdIn::None) => Ok(None),
            Ok(FdIn::File { handle, .. }) if self.uvm_refused.contains(&handle) => {
                log::warn!("UVM command {cmd}: names handle {handle}, a refused UVM file");
                Err(libc::EBADF)
            }
            Ok(FdIn::File { handle, .. }) => Ok(Some((off, handle))),
            Err(libc::EINVAL) => {
                log::warn!(
                    "UVM command {cmd}: {} bytes, too short for its descriptor at {off}",
                    params.len()
                );
                Err(libc::EINVAL)
            }
            Err(e) => {
                log::warn!("UVM command {cmd}: its descriptor field names no {want:?} of ours");
                Err(e)
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
    /// other use of that client to (SECURITY.md, R3). A zero client names nothing
    /// (REGISTER_GPU without a partition sends -1 and 0), and passes.
    pub(super) fn uvm_client_ok(&self, cmd: u32, params: &[u8]) -> std::result::Result<(), i32> {
        let Some(field) = crate::uvmfd::field(self.driver, cmd) else {
            return Ok(());
        };
        if field.of != crate::uvmfd::FdOf::RmCtl {
            return Ok(());
        }
        let off = field.offset as usize;
        let (Some(fd), Some(client)) = (le::u32_at(params, off), le::u32_at(params, off + 4))
        else {
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
}
