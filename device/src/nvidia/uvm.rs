//! UVM: the second ioctl interface, and what a guest may send to it.
//!
//! `/dev/nvidia-uvm` has its own numbering, its own structs, and none of RM's
//! privilege machinery -- `uvm_ioctl.h` carries no flags word, so there is
//! nothing to read about who may call what. What [`abi::uvm`] does know is the
//! shape of each call, and shape is what is checked here: a command the host
//! release does not define is refused, and one that arrives at any size but
//! its own is refused, because UVM reads its own `sizeof` out of the block
//! whatever the guest declared.
//!
//! The descriptors are the part that cannot be forwarded at all. Several UVM
//! calls carry a file descriptor *inside* their parameters, and a descriptor
//! is a number in one process's table and nothing in another's. Left alone,
//! the guest's `rmCtrlFd` names whichever of the backend's files happens to
//! sit at that number -- a file the guest never opened and may not reach. Each
//! one is translated to this backend's own, and checked to be a file this VM
//! opened *of the kind the call wants*: a call asking for `nvidiactl` handed a
//! UVM file is a different call than the one UVM would act on.

use super::*;

/// `UVM_PAGEABLE_MEM_ACCESS`. Its block is `pageableMemAccess` (`NvBool`) then
/// `rmStatus`; the answer is the first byte.
const UVM_PAGEABLE_MEM_ACCESS: u64 = 0x27;
const UVM_PAGEABLE_MEM_ACCESS_SIZE: usize = 8;

/// `UVM_INITIALIZE`, whose flags word the backend decides. Not an escape in
/// the `UVM_IOCTL_BASE` range: `uvm_linux_ioctl.h` numbers it separately.
const UVM_INITIALIZE: u64 = 0x3000_0001;

impl NvidiaBackend {
    /// Serve one ioctl on a `/dev/nvidia-uvm` file.
    pub(super) fn dispatch_uvm(
        &mut self,
        cookie: u64,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        resp_buf: &mut [u8],
    ) -> usize {
        let handle = self.current_handle as u64;

        // A file whose VA space was found to allow pageable access serves
        // nothing further. See `uvm_pageable_access_is_off`: the host file is
        // already initialised by then and cannot be un-initialised, so the
        // refusal has to attach to the handle.
        if self.uvm_denied.contains(&handle) {
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
        }

        let Some(sel) = self.uvm else {
            self.note_allow_refusal(
                format!("UVM {request:#x}"),
                "the host driver release is not known yet, so nothing says what this call is"
                    .into(),
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
        };

        let Ok(num) = u32::try_from(request) else {
            self.note_allow_refusal(
                format!("UVM {request:#x}"),
                "not a UVM command number".into(),
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTTY);
        };

        let Some(entry) = sel.entry(num) else {
            self.note_allow_refusal(
                format!("UVM {num:#x}"),
                if sel.exact {
                    "the host release does not define it".into()
                } else {
                    "no release this backend knows defines it the same way".to_string()
                },
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::ENOTTY);
        };

        // UVM copies its own struct's worth of bytes in and out regardless of
        // what the guest declared, so a short block is a read past what
        // arrived and a long one is a write the guest will not copy back.
        if param_in.len() != entry.params_size as usize {
            self.note_allow_refusal(
                format!("UVM {num:#x}"),
                format!(
                    "parameters are {} bytes, UVM's are {}",
                    param_in.len(),
                    entry.params_size
                ),
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL);
        }

        let mut param_buf = param_in.to_vec();

        // Every descriptor is replaced with ours before the call and put back
        // before the reply, so userspace reads back the number it wrote.
        let mut restore: Vec<(usize, [u8; 4])> = Vec::new();
        for slot in entry.fds {
            let raw = i32::from_le_bytes(param_buf[slot.at..slot.at + 4].try_into().unwrap());
            let host = match self.uvm_descriptor(slot, raw, &param_buf) {
                Ok(fd) => fd,
                Err(why) => {
                    self.note_allow_refusal(format!("UVM {num:#x}"), why);
                    return self.write_error_resp(resp_buf, Status::BadHandle, cookie, 0);
                }
            };
            restore.push((slot.at, raw.to_le_bytes()));
            param_buf[slot.at..slot.at + 4].copy_from_slice(&host.to_le_bytes());
        }

        // The flags are the backend's, not the guest's: every guest process's
        // UVM file is opened here, so a VA space tied to the caller's mm would
        // be tied to *this* process's, and with HMM or pageable access on the
        // GPU could fault in the backend's own pages.
        if request == UVM_INITIALIZE {
            let flags = sel.init_flags();
            let asked = u64::from_le_bytes(param_buf[0..8].try_into().unwrap());
            if asked != flags {
                log::debug!("UVM_INITIALIZE: guest asked flags {asked:#x}, sent {flags:#x}");
            }
            param_buf[0..8].copy_from_slice(&flags.to_le_bytes());
        }

        if let Err(errno) = self.host.ioctl(host_fd, request, &mut param_buf) {
            log::warn!("UVM {num:#x} failed: errno={errno}");
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, errno);
        }

        for (at, bytes) in restore {
            param_buf[at..at + 4].copy_from_slice(&bytes);
        }

        // Asked only once the file is initialised, because before that UVM has
        // no VA space to answer about.
        if request == UVM_INITIALIZE
            && sel.pageable_must_be_off()
            && !self.uvm_pageable_access_is_off(host_fd)
        {
            self.uvm_denied.insert(handle);
            self.note_allow_refusal(
                "UVM_INITIALIZE".into(),
                format!(
                    "this release has no flag to disable pageable access and the VA space \
                     reports it on, so the GPU could fault on host memory the guest never \
                     registered (host driver {})",
                    self.driver.expect("a UVM table implies a known release")
                ),
            );
            return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EPERM);
        }

        self.write_ioctl_resp(resp_buf, cookie, &param_buf)
    }

    /// This backend's descriptor for one the guest put inside a UVM call.
    fn uvm_descriptor(
        &self,
        slot: &abi::uvm::FdSlot,
        raw: i32,
        params: &[u8],
    ) -> std::result::Result<i32, String> {
        use abi::uvm::Fd;

        // There is no dma-buf this backend handed out, so there is nothing a
        // guest could be naming here. Refusing says so; forwarding the number
        // would hand UVM whichever of our files sits at it.
        if slot.kind == Fd::Foreign {
            return Err(
                "it imports a descriptor this backend did not open, and nothing here \
                        can translate it"
                    .into(),
            );
        }

        // -1 is "none", and has to go through as -1.
        //
        // An earlier reading of this said UVM resolves the descriptor in every
        // call that carries one, so there was no "none" to pass through. That
        // is not what the code does: every consumer of `rm_control_fd` is a
        // `(void)` beside "TODO: Bug 1624521: This interface needs to use
        // rm_control_fd to do validation" (`uvm_va_space.c`,
        // `uvm_user_channel.c`), and UVM itself passes -1 for its own internal
        // lookups. CUDA passes -1 to UVM_REGISTER_GPU on a GPU with no SMC
        // partition, and refusing it stopped `cuInit` outright.
        //
        // Nothing is given away by forwarding it: -1 names no file in any
        // process. Any other negative number is still refused, because it is
        // neither "none" nor anything this backend could translate.
        //
        // The client check below cannot apply here, because it asks which file
        // a client was made on and there is no file. What it would have ruled
        // out -- a client this VM was never given -- is already ruled out by
        // RM: clients resolve per process, this process serves one guest, and
        // every client in it is that guest's.
        if raw == -1 {
            return Ok(-1);
        }
        if raw < 0 {
            return Err(format!(
                "it needs a descriptor at byte {} and was sent {raw}",
                slot.at
            ));
        }

        let guest = raw as u64;
        let fd = self
            .handles
            .get_raw(guest)
            .map_err(|_| format!("descriptor {guest} is not one this VM opened"))?;

        let want = match slot.kind {
            Fd::Ctl => DeviceKind::Ctl,
            Fd::Uvm => DeviceKind::Uvm,
            Fd::Foreign => unreachable!("returned above"),
        };
        match self.handle_kinds.get(&guest) {
            Some(k) if *k == want => {}
            other => {
                return Err(format!(
                    "it wants {want:?} at byte {} and descriptor {guest} is {other:?}",
                    slot.at
                ));
            }
        }

        // UVM hands the pair to RM, and RM resolves the client through the
        // file. A client made on another of this VM's files, or one RM never
        // issued here at all, is a different call than the one checked above.
        if let Some(at) = slot.handle {
            let client = u32::from_le_bytes(params[at..at + 4].try_into().unwrap());
            if !self.vram.issued(guest, client) {
                return Err(format!(
                    "client {client:#x} at byte {at} was not made on descriptor {guest}"
                ));
            }
        }
        Ok(fd)
    }

    /// Whether this UVM file's VA space refuses pageable access.
    ///
    /// Only asked on releases with no `UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS`
    /// to ask with. A call that fails is read as "not off": the point is to
    /// establish that it is off, and an unanswered question establishes
    /// nothing.
    fn uvm_pageable_access_is_off(&self, host_fd: RawFd) -> bool {
        let mut p = [0u8; UVM_PAGEABLE_MEM_ACCESS_SIZE];
        if let Err(errno) = self
            .host
            .ioctl(host_fd, UVM_PAGEABLE_MEM_ACCESS, &mut p[..])
        {
            log::warn!("UVM_PAGEABLE_MEM_ACCESS failed: errno={errno}");
            return false;
        }
        let status = u32::from_le_bytes(p[4..8].try_into().unwrap());
        if status != NV_OK {
            log::warn!("UVM_PAGEABLE_MEM_ACCESS: rmStatus {status:#x}");
            return false;
        }
        p[0] == 0
    }
}
