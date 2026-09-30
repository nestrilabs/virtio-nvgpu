// SPDX-License-Identifier: Apache-2.0
//! RM escapes: the gates every one passes (ABI profile, untranslated
//! descriptors, the allowlist, pointers, sharing), the parameter blocks
//! behind a pointer, and the descriptor fields a guest names a file in.

#![forbid(unsafe_code)]

use super::*;

/// The least a single deep block is given, whatever length the guest sent
/// for it. RM copies through the pointer what the control's own parameters
/// say, which for a single block the backend does not work out (for deep
/// segments it does, deepseg.rs): a copy longer than the guest's bytes lands
/// in the block's zeroed tail, and one past the floor and the page of slack
/// after it at the guard page (sys/guarded.rs), which the host answers
/// EFAULT.
pub(super) const DEEP_BUF_FLOOR: usize = 64 * 1024;

/// The key at `key_at` of an RM reply's parameters when RM's status word at
/// `status_at` is NV_OK.
pub(super) fn rm_served(reply: Option<&[u8]>, key_at: usize, status_at: usize) -> Option<u32> {
    let p = reply?;
    (le::u32_at(p, status_at)? == NV_OK).then_some(le::u32_at(p, key_at)?)
}

/// A descriptor field of a block the guest sent: where it is, how wide,
/// which of our handles may stand in it, and what "no descriptor" is there.
///
/// The guest driver turns the caller's descriptor into one of our handles,
/// and the host must be handed our descriptor of that file in its place.
/// Anything else in the field -- a number no handle of ours has, a handle of
/// the wrong kind, a "none" the field does not take -- is refused, never
/// forwarded: RM and UVM would look a number up among this process's
/// descriptors, every guest process's files (R2, R5).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FdField {
    pub(crate) at: usize,
    /// 4, or 8 for a 64-bit field.
    pub(crate) width: usize,
    pub(crate) accept: FdAccept,
    pub(crate) none: FdNone,
}

/// Which of our handles a descriptor field may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FdAccept {
    /// Any of our NVIDIA devices (RM takes the file as one of its own).
    Device,
    /// A control file (`/dev/nvidiactl`), the only kind RM's export and
    /// import controls resolve (`nv_get_file_private(fd, NV_TRUE, ..)`).
    ControlFile,
    /// Exactly this kind.
    Kind(HandleKind),
}

/// What says "no descriptor" in a descriptor field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FdNone {
    /// -1, which the host is handed as it is.
    MinusOne,
    /// 0 (an OS event's field: no notification).
    Zero,
    /// Any negative value, left as the guest sent it (UVM's).
    Negative,
    /// Nothing: the field must name one of our files.
    Never,
}

/// What a descriptor field holds.
pub(crate) enum FdIn<'a> {
    None,
    File { handle: u32, fd: BorrowedFd<'a> },
}

/// Declare field `f` of block `b` a descriptor and put what
/// [`NvidiaBackend::fd_field`] found in it: our descriptor of the file, or
/// the field's "none" -- -1, or 0, as the guest sent it. A `Negative` "none"
/// is not declared, and reaches the host as sent.
pub(crate) fn declare_fd(
    a: &mut Arena,
    b: BufId,
    f: FdField,
    v: &FdIn<'_>,
) -> std::result::Result<(), i32> {
    match (v, f.none) {
        (FdIn::None, FdNone::Negative) => Ok(()),
        (FdIn::None, FdNone::MinusOne) => {
            a.fd(b, f.at, f.width)?;
            a.set_no_fd(b, f.at, -1)
        }
        (FdIn::None, _) => a.fd(b, f.at, f.width).map(|_| ()),
        (FdIn::File { fd, .. }, _) => {
            a.fd(b, f.at, f.width)?;
            a.set_fd(b, f.at, *fd)
        }
    }
}

/// NVOS54 (RM_CONTROL): the parameters at `params`, `paramsSize` long.
pub(super) const RM_CONTROL_NESTED: Nested = Nested {
    outer_size: NVOS54_SIZE,
    ptr_offset: NVOS54_PARAMS,
    size_offset: NVOS54_PARAMS_SIZE,
    fd_at: None,
};

/// NVOS64 (RM_ALLOC): the class parameters at `pAllocParms`.
pub(super) const RM_ALLOC_NESTED: Nested = Nested {
    outer_size: NVOS64_SIZE,
    ptr_offset: NVOS64_P_ALLOC_PARMS,
    size_offset: NVOS64_PARAMS_SIZE,
    fd_at: None,
};

impl NvidiaBackend {
    /// Whether the host's ABI table marks `escape` as carrying a descriptor
    /// and nothing here translates it: the virtio config's
    /// FD_CARRYING_IOCTLS (the guest translates those) plus the arms of
    /// `serve_rm_escape` built on them. Today that is EXPORT_TO_DMABUF_FD alone;
    /// the check is general so that the next such escape a profile adds is
    /// refused rather than forwarded raw. Before the version is known there
    /// is no table, and `guestptr::rm_escape` refuses the one there is by
    /// number.
    pub(super) fn untranslated_fd_escape(&self, escape: u32) -> bool {
        self.abi
            .and_then(|t| abi::versions::lookup(t, escape))
            .is_some_and(|e| e.kind == abi::versions::IoctlKind::FdCarrying)
            && !crate::virtio::FD_CARRYING_IOCTLS
                .iter()
                .any(|&(nr, _)| nr == escape)
    }

    /// A v1 RM escape, on an NVIDIA device file.
    pub(super) fn serve_rm_escape(&mut self, host_fd: RawFd, req: &V1Request<'_>) -> V1 {
        use abi::ioctl::*;
        let (ireq, param_in, trailer) = (req.ireq, req.params, req.trailer);
        let (deep_in, deep_segs, deep_bytes) =
            (req.deep.single(), req.deep.segments(), req.deep.bytes());
        let request = u64::from(ireq.cmd);
        let escape = hostfd::ioc_nr(ireq.cmd);
        // Only NVIDIA's own magic is described by the ABI tables.
        let host = self
            .driver
            .map_or_else(|| "(unknown)".to_string(), |v| v.to_string());
        let refuse = match self.check_abi(escape, ireq.data_len) {
            AbiCheck::SizeMismatch { expected, actual } => {
                log::warn!(
                    "escape {escape:#04x}: guest sent {actual} bytes, host driver {host} expects \
                     {expected}"
                );
                true
            }
            AbiCheck::UnknownEscape => {
                log::warn!("escape {escape:#04x} is not in the ABI profile for host driver {host}");
                true
            }
            // A known host with no profile: one no table was measured at,
            // which the transport refuses to start on (crate::release). Its
            // layouts are nobody's to guess.
            //
            // No version at all is refused too: the transport reads it from
            // the driver before any guest call, and a library user that set
            // none (test-harness, an embedding VMM) must not learn it from
            // the guest's CHECK_VERSION_STR, whose string RM leaves as the
            // caller sent it -- the guest would choose the ABI profile. Only
            // unit tests and fuzzing may run unversioned (`unversioned_ok`).
            AbiCheck::NoProfile if self.driver.is_none() && self.unversioned_ok() => false,
            AbiCheck::NoProfile => {
                log::warn!("escape {escape:#04x}: host driver {host} has no ABI profile");
                true
            }
            AbiCheck::Ok | AbiCheck::VariableLength => false,
        };
        if refuse {
            *self.abi_refused.entry(escape).or_insert(0) += 1;
            if self.abi_policy == AbiPolicy::Enforce {
                return Err(libc::EINVAL);
            }
        }

        // With --allow-dmabuf-export, EXPORT_TO_DMABUF_FD in the form the
        // guest translates (nvidia/dmabuf.rs); refused just below otherwise.
        if self.serves_dmabuf_export(escape) {
            return self.serve_dmabuf_export(host_fd, req);
        }

        // An escape the host's table says carries a descriptor, with no
        // translation here, would reach the host with the guest's number in
        // it -- naming whatever this process has open under that number --
        // and could leave a descriptor of the host's in our table that the
        // guest never learns of (S-15). Whatever the ABI policy.
        if self.untranslated_fd_escape(escape) {
            log::warn!(
                "escape {escape:#04x} carries a descriptor the backend does not translate; refused"
            );
            return Err(libc::EOPNOTSUPP);
        }

        // Default deny: an RM control or class the host release's allowlist
        // lacks is answered here as RM answers what it does not implement
        // (rmallow.rs). Whatever the ABI policy. The controls the backend
        // answers itself below (rmctl.rs) never reach RM and keep their own
        // answers; the page-list path's classes are held to it too.
        let answered_here = escape == NV_ESC_RM_CONTROL
            && (self.rm_unix_refused(param_in).is_some()
                || crate::rmctl::host_pid_control(param_in).is_some());
        if !answered_here
            && let Err(r) = self.rmallow.check(escape, param_in, ireq.data_len as usize)
        {
            return match r {
                crate::rmallow::Refusal::Errno(errno) => Err(errno),
                crate::rmallow::Refusal::Status { at, status } => {
                    let mut out = nvos::with_status(param_in, at, status);
                    out.extend_from_slice(deep_bytes);
                    Ok(IoctlOut::deep(out, deep_bytes.len()))
                }
            };
        }

        // Memory registered by its pages rather than its address: its own
        // path, and the only one on which RM is handed an address for an OS
        // descriptor -- one of ours (osdesc.rs). Sent with an address alone,
        // the same calls are refused below.
        if let Some(list) = req.deep.page_list() {
            return self.dispatch_osdesc(host_fd, request, ireq.data_len as usize, param_in, list);
        }

        // No guest pointer reaches RM as a pointer (guestptr.rs), whatever
        // the ABI policy: the call is refused, or the plan names every field
        // RM would dereference, and the host's copy is built with each of them
        // holding only what the backend puts there (sys/block.rs); the
        // caller's values go back in the reply. The parameter pointers of
        // RM_CONTROL and RM_ALLOC are dispatch_nested's.
        let outer_len = (ireq.data_len as usize).min(param_in.len());
        // IDLE_CHANNELS' channel list, sent with its three arrays: each gets
        // a block of the call's own.
        let plan = match (escape, deep_segs) {
            (NV_ESC_RM_IDLE_CHANNELS, Some(deep)) => {
                crate::guestptr::idle_channels_list(ireq.cmd, &param_in[..outer_len], deep)
            }
            _ => crate::guestptr::rm_escape(ireq.cmd, &param_in[..outer_len]),
        };
        let plan = plan?;
        // What the allocation parameters say the class really is, and the
        // checks only they can decide (guestptr.rs `rm_alloc_params`).
        if escape == NV_ESC_RM_ALLOC {
            crate::guestptr::rm_alloc_params(&param_in[..outer_len], &param_in[outer_len..])?;
        }

        // Sharing, duplicating and naming RM objects of another client
        // (rmshare.rs): the calling guest process, and what the call may
        // name, judged on the parameters as the guest sent them. A refusal
        // is RM's own status in those parameters, and the host is not asked.
        self.current_proc = None;
        self.current_proc = self.rm_proc_id(escape, param_in, trailer)?;
        let share_pending = match self.rm_share_gate(escape, param_in, self.current_proc) {
            Ok(p) => p,
            Err(crate::rmshare::Refuse::Errno(errno)) => return Err(errno),
            Err(crate::rmshare::Refuse::Status(status)) => {
                let at = crate::rmshare::status_at(escape).unwrap_or(0);
                let mut out = nvos::with_status(param_in, at, status);
                out.extend_from_slice(deep_bytes);
                return Ok(IoctlOut::deep(out, deep_bytes.len()));
            }
        };
        // A guest stopping its own channels: no preemption event, RM's size,
        // and a rate per guest process and per VM (rmchan.rs). Its clients
        // were the share gate's, just above.
        if escape == NV_ESC_RM_CONTROL
            && let Err(status) = self.rm_chan_gate(param_in)
        {
            let mut out = nvos::with_status(param_in, NVOS54_STATUS, status);
            out.extend_from_slice(deep_bytes);
            return Ok(IoctlOut::deep(out, deep_bytes.len()));
        }
        // The opt-in allowlist groups' rules (rmgroup.rs); nothing without
        // `--rm-allow-group`. A word the group forces goes to RM in place of
        // the guest's, which the reply gets back.
        let group_fix = match self.rm_group_gate(
            host_fd,
            escape,
            param_in,
            deep_segs.is_some(),
            deep_in.is_some(),
        ) {
            Ok(fix) => fix,
            Err(status) => {
                let at = crate::rmshare::status_at(escape).unwrap_or(0);
                let mut out = nvos::with_status(param_in, at, status);
                out.extend_from_slice(deep_bytes);
                return Ok(IoctlOut::deep(out, deep_bytes.len()));
            }
        };
        let group_sent = param_in;
        let group_copy: Vec<u8>;
        let param_in: &[u8] = match group_fix {
            Some(fix) => {
                group_copy = crate::rmgroup::apply(param_in, fix);
                &group_copy
            }
            None => param_in,
        };

        // What the memory an escape makes, duplicates, frees or GPU-maps is,
        // and the coherency rewrite (rmmem.rs): the host is handed a rewritten
        // copy, and the reply gets the caller's own bits back. RM_ALLOC only in
        // the 48-byte NVOS64 form, the one the offsets are for.
        let rm_copy: Vec<u8>;
        let mut rm_pending = None;
        let param_in: &[u8] =
            if self.rmmem.watching(escape) && (escape != NV_ESC_RM_ALLOC || ireq.data_len == 48) {
                let mut v = param_in.to_vec();
                let mut p = self
                    .rmmem
                    .before(escape, &mut v)
                    .charged_to(self.rm_caller());
                // Video memory past `--vram-limit` (vidmem.rs): RM's own status
                // in the caller's block, and RM is not asked.
                if let Err((at, status)) = self.rmmem.admit(&mut p) {
                    let mut out = nvos::with_status(param_in, at, status);
                    out.extend_from_slice(deep_bytes);
                    return Ok(IoctlOut::deep(out, deep_bytes.len()));
                }
                rm_pending = Some(p);
                rm_copy = v;
                &rm_copy
            } else if self.rmmem.vram.refuses_unseen(escape, param_in) {
                return Err(libc::EINVAL);
            } else {
                param_in
            };

        let mut r = match escape {
            // ---------------------------------------------------------------
            // FD-carrying ioctls — need handle translation
            // ---------------------------------------------------------------
            NV_ESC_REGISTER_FD => {
                self.dispatch_fd_carrying(host_fd, request, escape, param_in, &plan)
            }
            // The OS events RM calls may name later (semsurf.rs).
            NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => {
                let r = self.dispatch_fd_carrying(host_fd, request, escape, param_in, &plan);
                self.semsurf_track_rm(escape, self.current_handle, param_in, served(&r));
                r
            }

            NV_ESC_RM_ALLOC_MEMORY => {
                self.dispatch_fd_carrying(host_fd, request, escape, param_in, &plan)
            }

            NV_ESC_RM_MAP_MEMORY => self.dispatch_map_memory(host_fd, request, param_in, &plan),

            NV_ESC_RM_UNMAP_MEMORY => self.dispatch_unmap_memory(host_fd, request, param_in),

            NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO => {
                self.dispatch_update_device_mapping_info(host_fd, request, param_in)
            }

            // ---------------------------------------------------------------
            // RM control requires nested handling
            // ---------------------------------------------------------------
            // Registered memory handed to a holder the backend cannot follow
            // (osdesc.rs): refused, before RM sees it.
            NV_ESC_RM_CONTROL if self.osdesc_export_refused(param_in) => {
                return Err(libc::EPERM);
            }
            // OS_UNIX controls whose descriptors nothing translates, and
            // ones RM does not define (rmctl.rs): RM's NOT_SUPPORTED.
            NV_ESC_RM_CONTROL if self.rm_unix_refused(param_in).is_some() => {
                log::warn!(
                    "RM control {} refused: it names a host descriptor nothing translates",
                    self.rm_unix_refused(param_in).unwrap_or_default()
                );
                let mut out = crate::rmctl::unsupported(param_in);
                let deep = deep_in.map_or(&[][..], |(_, b)| b);
                out.extend_from_slice(deep);
                Ok(IoctlOut::deep(out, deep.len()))
            }
            // Controls that list other RM clients' host PIDs (rmctl.rs, S-24):
            // answered here, as RM answers a caller without the privilege.
            NV_ESC_RM_CONTROL if crate::rmctl::host_pid_control(param_in).is_some() => {
                log::warn!(
                    "RM control {} refused: it lists the host's GPU processes",
                    crate::rmctl::host_pid_control(param_in).unwrap_or_default()
                );
                let mut out = crate::rmctl::refusal(param_in);
                let deep = deep_in.map_or(&[][..], |(_, b)| b);
                out.extend_from_slice(deep);
                Ok(IoctlOut::deep(out, deep.len()))
            }
            NV_ESC_RM_CONTROL => {
                let r = self.dispatch_nested(
                    host_fd,
                    request,
                    param_in,
                    &plan,
                    RM_CONTROL_NESTED,
                    req.deep,
                );
                // Counted here rather than in the forwarder, which holds only a
                // shared borrow. Only what RM served: the key is the guest's
                // u32, and a count of what RM turned down is noise an
                // allowlist is not written from (S-17).
                if let Some(cmd) = rm_served(served(&r), NVOS54_CMD, NVOS54_STATUS) {
                    self.rm_controls.add(cmd);
                }
                r
            }

            // ---------------------------------------------------------------
            // RM alloc as well..
            // ---------------------------------------------------------------
            NV_ESC_RM_ALLOC => {
                let r = self.dispatch_nested(
                    host_fd,
                    request,
                    param_in,
                    &plan,
                    RM_ALLOC_NESTED,
                    req.deep,
                );
                // As for controls, only what RM made.
                if let Some(class) = rm_served(served(&r), NVOS64_H_CLASS, NVOS64_STATUS) {
                    self.rm_classes.add(class);
                }
                // A client made here is one 0x54 may name (semsurf.rs).
                self.semsurf_track_rm(escape, self.current_handle, param_in, served(&r));
                r
            }

            // ---------------------------------------------------------------
            // Everything else — simple passthrough to host
            // ---------------------------------------------------------------
            _other => {
                let r = self.dispatch_simple(host_fd, request, param_in, &plan);
                // RM_FREE of a client: no longer one 0x54 may name.
                self.semsurf_track_rm(escape, self.current_handle, param_in, served(&r));
                r
            }
        };

        if let Some(fix) = group_fix
            && let Some(out) = served_mut(&mut r)
        {
            crate::rmgroup::restore(out, fix, group_sent);
        }
        // The reply's parameters, laid out as the request's were; none when
        // the call failed before RM ran, and then nothing is recorded.
        if let Some(p) = rm_pending
            && let Some(out) = served_mut(&mut r)
        {
            self.rmmem.after(p, out);
        }
        // What RM freed, duplicated or made anew, as registrations of memory
        // by its pages live through (osdesc.rs).
        if let Some(out) = served(&r) {
            self.osdesc_observe(escape, out);
            // A share RM took, for the duplicates it lets through.
            self.rm_share_after(escape, share_pending, out);
        }
        r
    }

    /// A call whose top-level block holds a pointer to a parameter block:
    /// RM_CONTROL, RM_ALLOC, NVKMS's v1 ioctl and nvidia-drm's GEM import
    /// and export.
    ///
    /// The host's copies are built in an arena (sys/block.rs), never by
    /// editing the guest's: the top-level block and the parameter block are
    /// the guest's bytes with every field the backend knows to be more than
    /// data declared first -- the parameter pointer, the pointers `plan`
    /// names, a descriptor the parameters carry, the pointers RM follows
    /// inside a control's parameters -- and each declared field then holds
    /// only what the backend put there: an address of another block of the
    /// call, a descriptor of its own, or 0. The reply is a copy with the
    /// caller's values back in those fields.
    pub(super) fn dispatch_nested(
        &self,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
        layout: Nested,
        // What follows the nested block: a single one, or segments
        // (deepseg.rs, RM_CONTROL only).
        deep: Deep<'_>,
    ) -> V1 {
        let Nested {
            outer_size,
            ptr_offset,
            size_offset,
            fd_at: nested_fd_offset,
        } = layout;
        let (deep_in, deep_segs) = (deep.single(), deep.segments());
        if param_in.len() < outer_size {
            return Err(libc::EINVAL);
        }
        let outer_in = &param_in[..outer_size];
        let nested_in = &param_in[outer_size..];
        let escape = (request & 0xFF) as u32;

        // An OS event named by descriptor inside RM's parameters: semaphore
        // surface waiters and NV_EVENT_BUFFER (semsurf.rs). Found before
        // anything is copied, so a call that would have the host read the
        // field from past what the guest sent is refused outright.
        let rm = nested_fd_offset.is_none() && hostfd::ioc_type(request as u32) == b'F';
        let os_event = match crate::semsurf::os_event_field(
            if rm { escape } else { 0 },
            outer_in,
            nested_in.len(),
        ) {
            Ok(f) => f,
            Err(e) => {
                log::warn!(
                    "RM call {request:#x}: its OS-event field is not in the {} bytes sent",
                    nested_in.len()
                );
                return Err(e);
            }
        };
        // Neither a waiter nor an event buffer holds a second-level pointer,
        // and one the guest claims could be aimed at the field: the address
        // written there below would reach RM as the event.
        if os_event.is_some() && (deep_in.is_some() || deep_segs.is_some()) {
            log::warn!("RM call {request:#x} names an OS event and claims a deep pointer");
            return Err(libc::EINVAL);
        }

        // The command of an RM_CONTROL and the class of an RM_ALLOC, from
        // the top-level block as the guest sent it: `None` for any other
        // call, and a block too short to hold one is refused.
        let field = |on: bool, at: usize| match on {
            true => le::u32_at(outer_in, at).map(Some).ok_or(libc::EINVAL),
            false => Ok(None),
        };
        let (cmd, class) = match (
            field(rm && escape == abi::ioctl::NV_ESC_RM_CONTROL, NVOS54_CMD),
            field(rm && escape == abi::ioctl::NV_ESC_RM_ALLOC, NVOS64_H_CLASS),
        ) {
            (Ok(cmd), Ok(class)) => (cmd, class),
            (Err(e), _) | (_, Err(e)) => return Err(e),
        };
        // For the logs only.
        let word = |b: &[u8], at: usize| le::u32_at(b, at).unwrap_or(0);
        // Log RM_CONTROL/RM_ALLOC for debugging Vulkan init. One line per
        // call, so debug: at info a guest's RM traffic was the log (S-20).
        if let Some(cmd) = cmd {
            log::debug!(
                "RM_CONTROL cmd=0x{cmd:x} (hClient={}, hObject={})",
                word(outer_in, NVOS54_H_CLIENT),
                word(outer_in, NVOS54_H_OBJECT)
            );
        }
        if let Some(class) = class {
            log::debug!("RM_ALLOC hClass=0x{class:x}");
        }

        // The size the host copies through the pointer: the outer struct's
        // own field (u64 in nvidia-drm's blocks, u32 in RM's and NVKMS's).
        let size_wide = hostfd::ioc_type(request as u32) == b'd';
        let host_size =
            le::uint_at(outer_in, size_offset, if size_wide { 8 } else { 4 }).unwrap_or(0);

        // The top-level block: sized for what the host copies, not for what
        // the guest sent (`ioctl_arg_len`), its parameter pointer declared --
        // the caller reads back the pointer it passed, never ours and never
        // zero: the host driver leaves the field alone, so a native caller
        // reads back its own -- and the escape's other pointers as `plan`
        // says.
        let mut a = Arena::new();
        let top = self.top_block(&mut a, request, outer_in, plan)?;
        a.slot(top, ptr_offset, 8, SlotKind::Ptr, Restore::Yes)?;

        if nested_in.is_empty() {
            if deep_segs.is_some() {
                log::warn!("ioctl {request:#x}: deep segments with no parameters to hold them");
                return Err(libc::EINVAL);
            }
            // No nested params: the call names no parameter block, so the
            // host is told exactly that -- a null pointer and a zero size.
            //
            // This used to forward the pointer as the guest wrote it. The
            // guest's value is an address in *its* process; the host
            // (RM_CONTROL and RM_ALLOC through param_copy.c, NVKMS through
            // nvkms_ioctl_from_kapi's copy_from_user, nvidia-drm's GEM
            // import and export through their nvkms_params_ptr) reads it as an
            // address in ours, and copies in from it and back out to it:
            // any guest could read and write the VMM's memory with a
            // pointer and a size of its choosing. A nonzero size with nothing
            // sent is refused rather than zeroed, so a caller that meant to
            // send parameters learns that none arrived.
            if host_size != 0 {
                log::warn!(
                    "ioctl {request:#x}: parameter size {host_size} but no parameters sent; \
                     refused rather than handing the host the guest's pointer"
                );
                return Err(libc::EINVAL);
            }
            if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
                log::warn!("nested ioctl(0x{request:x}) no-params failed: errno={errno}");
                let back = a.reply(top)[..outer_size].to_vec();
                return Ok(IoctlOut::failed(back, errno));
            }
            let outer = &a.bytes(top)[..outer_size];
            if let Some(cmd) = cmd {
                log::debug!(
                    "(else) RM_CONTROL cmd=0x{cmd:08x} status=0x{:x}",
                    word(outer, NVOS54_STATUS)
                );
            } else if let Some(class) = class {
                log::debug!(
                    "(else) RM_ALLOC hClass=0x{class:04x} status=0x{:x}",
                    word(outer, NVOS64_STATUS)
                );
            }
            return Ok(IoctlOut::ok(a.reply(top)[..outer_size].to_vec()));
        }

        // Guest sent nested params: a block of the call's, and the pointer
        // at it.
        let nested_size = nested_in.len();
        // Exactly as many bytes as the host will copy, or none said
        // (RM_ALLOC's zero: RM takes the class's own size, and no class
        // with a pointer in its parameters gets this far, guestptr.rs).
        // Past what was sent the host reads the zeroed slack after our
        // buffer (guarded.rs), so a pointer field the block cuts short
        // reached RM as the guest's low bytes over our zeros: seven of
        // eight bytes, any address in this process, which the scrub
        // below -- reading only the bytes sent -- never saw, and RM
        // copied in from and out to it (FIFO_GET_CHANNELLIST writes its
        // channel list there). Found by the `backend_v2` fuzz target.
        if host_size != 0 && host_size != nested_size as u64 {
            log::warn!(
                "ioctl {request:#x}: the host would copy {host_size} bytes of parameters and \
                 {nested_size} were sent; refused"
            );
            return Err(libc::EINVAL);
        }
        // Guarded rather than heap-allocated: the driver writes its answer
        // here, and if it writes more than the caller's size field claimed,
        // the fault should land on that write rather than on someone else's
        // allocation later.
        let nb = a.block(nested_in, nested_size)?;

        // An event object names the file its notifications arrive on, in
        // `NV0005_ALLOC_PARAMETERS.data` at offset 16. The guest driver has
        // already turned the caller's descriptor into one of our handles;
        // the host gets the descriptor this process holds for it, and the
        // reply the handle (the guest driver then restores the caller's own
        // descriptor over it).
        //
        // A block too short to hold the field is refused, as OS_UNIX's is:
        // RM would read the descriptor from past what was sent -- the zeroed
        // slack after our buffer, descriptor 0 of this process. -1, "no
        // descriptor", goes as it is.
        if let Some(h_class @ (0x05 | 0x79)) = class {
            let f = FdField {
                at: 16,
                width: 4,
                accept: FdAccept::Device,
                none: FdNone::MinusOne,
            };
            let set = self.fd_field(nested_in, f).inspect_err(|&e| {
                log::warn!(
                    "event class {h_class:#x}: {}",
                    if e == libc::EINVAL {
                        format!("{nested_size} parameter bytes do not hold its descriptor")
                    } else {
                        "no handle of ours for the file this event is to be delivered on".into()
                    }
                )
            });
            set.and_then(|v| declare_fd(&mut a, nb, f, &v))?;
        }

        // The same for the OS event a waiter or an event buffer names by
        // (u64) descriptor: our handle becomes the host descriptor RM
        // looks the event up by, and only for an event that is live --
        // for NV_EVENT_BUFFER a lookup that misses is a host oops, not a
        // refusal (semsurf.rs).
        if let Some((off, h_client)) = os_event {
            let (f, set) = self.os_event_fd(h_client, nested_in, off);
            set.and_then(|v| declare_fd(&mut a, nb, f, &v))?;
        }

        // A control file named by descriptor in NV0000's OS_UNIX
        // controls (rmctl.rs): the host gets our descriptor of that very
        // file, and only a control file's; a number that is not one is
        // refused, never forwarded, since RM would resolve it among every
        // guest process's files (R1, R2). -1 passes: RM refuses it itself.
        if let Some(cmd) = cmd
            && let Some(crate::rmctl::UnixCtl::Fd { at }) = crate::rmctl::unix_control(cmd)
        {
            let f = FdField {
                at,
                width: 4,
                accept: FdAccept::ControlFile,
                none: FdNone::MinusOne,
            };
            let set = self.fd_field(nested_in, f).inspect_err(|&e| {
                log::warn!(
                    "RM control {cmd:#x}: {}",
                    if e == libc::EINVAL {
                        format!("{nested_size} parameter bytes do not hold its descriptor")
                    } else {
                        "its descriptor is no control file of this VM".into()
                    }
                )
            });
            set.and_then(|v| declare_fd(&mut a, nb, f, &v))?;
        }

        // A descriptor named by position rather than by command: NVKMS's
        // import and export blocks both begin with the `memFd` naming the
        // memory. The guest driver has already turned its own descriptor
        // into one of our handles; the host gets the descriptor this process
        // holds, and the reply the guest's value.
        if let Some(off) = nested_fd_offset {
            let f = FdField {
                at: off,
                width: 4,
                accept: FdAccept::Device,
                none: FdNone::Never,
            };
            let set = self.fd_field(nested_in, f).inspect_err(|&e| {
                if e == libc::EINVAL {
                    log::warn!(
                        "ioctl {request:#x}: fd at {off} is outside {nested_size} nested bytes"
                    );
                } else {
                    log::warn!(
                        "nvkms memFd: no handle of ours; the memory to import names a file we \
                         did not open"
                    );
                }
            });
            set.and_then(|v| declare_fd(&mut a, nb, f, &v))?;
        }

        // The pointer inside the nested block, given a block of the call's:
        // the guest sends the bytes it addresses, never an address.
        //
        // The block is padded well past what the guest said it holds. The
        // length RM copies comes from a field inside the caller's own
        // parameters, read with an offset out of a table generated from one
        // driver release; when that offset is wrong for the release in use
        // -- which it demonstrably is for some commands -- the length read
        // is not the buffer's, while the driver still writes as much as the
        // command really produces. Guarded, so a guest that says 8 bytes and
        // a count of 100000 has RM write into zeroed slack or fault on the
        // guard page (EFAULT), never into our heap. Only the bytes the guest
        // asked for go back.
        let mut deep: Option<(usize, BufId)> = None;
        // A deep block RM never follows a pointer to: its bytes go back as
        // they came (see below).
        let mut deep_unread: Option<&[u8]> = None;
        if let Some((ptr_off, _)) = deep_in {
            // Only an RM control's parameters hold a pointer the guest sends
            // one block for. On anything else -- an RM_ALLOC, whose classes
            // with pointers are refused outright (guestptr.rs), nvidia-drm's
            // import and export -- the address written below would reach
            // the host as data.
            let Some(cmd) = cmd else {
                log::warn!("ioctl {request:#x}: a deep block on a call that takes none; refused");
                return Err(libc::EINVAL);
            };
            // Controls whose pointers stay zeroed (ACPI methods among
            // them; abi::rmctrl::ZEROED_CONTROLS) take no deep block.
            if abi::rmctrl::zeroed(cmd) {
                log::warn!(
                    "RM control {cmd:#010x}: a deep block for a control whose pointers are \
                     never relocated"
                );
                return Err(libc::EINVAL);
            }
            if ptr_off + 8 > nested_size {
                log::warn!(
                    "ioctl {request:#x}: pointer at {ptr_off} is outside {nested_size} nested bytes"
                );
                return Err(libc::EINVAL);
            }
        }
        // The block's address goes only where RM follows a pointer in this
        // control's parameters (the measured tables, guestptr.rs). Anywhere
        // else it would reach RM as data: SET_ZBC_COLOR_CLEAR stored it in
        // the GPU-wide ZBC table, where any tenant reads it back -- an
        // address of this process, and a clear color of the guest's making.
        // RM does not read the field as a pointer, so there is nothing to
        // copy: the field keeps the guest's bytes, as does the block, which
        // goes back unchanged -- what a native caller's buffer holds after
        // a call that never touched it (V1 GPU_GET_ID_INFO's szName, which
        // the guest driver still carries one for).
        // (A deep block reaches here only on an RM_CONTROL, whose `cmd` is
        // known.)
        let ctl = cmd.unwrap_or(0);
        if let Some((ptr_off, bytes)) =
            deep_in.filter(|&(o, _)| !crate::guestptr::control_pointers(ctl).contains(&o))
        {
            log::debug!(
                "RM control {ctl:#010x}: a deep block for {ptr_off}, where RM follows no \
                 pointer; not relocated"
            );
            deep_unread = Some(bytes);
        } else if let Some((ptr_off, bytes)) = deep_in {
            let db = a.block(bytes, bytes.len().max(DEEP_BUF_FLOOR))?;
            log::debug!(
                "deep pointer at {ptr_off}: guest says {} bytes, buffer {} bytes",
                bytes.len(),
                a.len(db)
            );
            a.slot(nb, ptr_off, 8, SlotKind::Ptr, Restore::Yes)
                .and_then(|_| a.point(nb, ptr_off, db))?;
            deep = Some((ptr_off, db));
        }

        // What several pointers of a control's parameters refer to, when the
        // guest sent it as deep segments: checked against the sizes RM will
        // copy, computed from these very parameters, and each given a block
        // of the call's (deepseg.rs). Only a control's parameters are read
        // so.
        let segs = match (deep_segs, cmd) {
            (None, _) => None,
            (Some(segs), Some(cmd)) => {
                let Some(ctl) = abi::rmctrl::deep_control(cmd) else {
                    log::warn!(
                        "RM control {cmd:#010x}: deep segments for a control whose pointers \
                         are not relocated"
                    );
                    return Err(libc::EINVAL);
                };
                let what = format!("RM control {cmd:#010x} ({})", ctl.name);
                {
                    let s = crate::deepseg::Segments::relocate(&what, ctl.ptrs, &mut a, nb, segs)?;
                    Some(s)
                }
            }
            (Some(_), None) => return Err(libc::EINVAL),
        };
        // Every other pointer RM would follow inside a control's
        // parameters: 0 for the host, the caller's value in the reply
        // (guestptr.rs). Left as sent, RM copied in from and out to that
        // address in this process.
        if let Some(cmd) = cmd {
            let relocated: Vec<usize> = match &segs {
                Some(s) => s.offsets(),
                None => deep.map(|(o, _)| o).into_iter().collect(),
            };
            crate::guestptr::scrub_control(cmd, &mut a, nb, &relocated)?;
        }

        // The top-level block's pointer at the parameters, and the call.
        a.point(top, ptr_offset, nb)?;
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            log::warn!("nested ioctl(0x{:x}) failed: errno={}", request, errno);
            // The block and its parameters as the host left them; no deep
            // block, which the guest reads only from a success.
            let mut back = a.reply(top)[..outer_size].to_vec();
            back.extend_from_slice(&a.reply(nb));
            return Ok(IoctlOut::failed(back, errno));
        }

        // RM reports two different things in two different places, and only
        // one of them is the ioctl return value. A control call routinely
        // comes back rc=0 with a failure in the NVOS54 status word, and the
        // caller believes the status, not the rc. Counting non-zero rc told
        // us every call succeeded while the ICD was reading refusals.
        let outer = a.bytes(top);
        if let Some(cmd) = cmd {
            let params_size = word(outer, NVOS54_PARAMS_SIZE);
            let status = word(outer, NVOS54_STATUS);
            // RM refusing a control is routine (userspace probes), and its
            // parameters may hold host addresses and the guest's data: the
            // command and status only, at debug.
            log::debug!(
                "RM_CONTROL cmd=0x{cmd:08x} paramsSize={params_size} -> status=0x{status:08x}"
            );
        }
        if let Some(class) = class {
            log::debug!(
                "RM_ALLOC ENTER: hClass=0x{class:04x} paramsSize={} (nested_bytes={})",
                word(outer_in, NVOS64_PARAMS_SIZE),
                nested_in.len()
            );
        }

        // The reply: outer, the parameters, and what the pointer inside
        // them addresses -- the host's bytes, with the caller's values in
        // every field declared above.
        let mut combined = a.reply(top)[..outer_size].to_vec();
        combined.extend_from_slice(&a.reply(nb));
        // Each segment's bytes as RM left them, after the table as sent.
        if let Some(segs) = &segs {
            let deep = segs.reply(&a);
            combined.extend_from_slice(&deep);
            return Ok(IoctlOut::deep(combined, deep.len()));
        }
        // Only what the guest allocated room for goes back, not the pad.
        let deep_reply = deep_in.map_or(0, |(_, b)| b.len());
        if let Some((_, db)) = deep {
            combined.extend_from_slice(&a.bytes(db)[..deep_reply]);
        }
        if let Some(bytes) = deep_unread {
            combined.extend_from_slice(bytes);
        }
        Ok(IoctlOut::deep(combined, deep_reply))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn dispatch_fd_carrying(
        &self,
        host_fd: RawFd,
        request: u64,
        escape: u32,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
    ) -> V1 {
        use abi::ioctl::*;

        let fd_offset: usize = match escape {
            NV_ESC_REGISTER_FD => REGISTER_FD_FD,
            NV_ESC_ALLOC_OS_EVENT | NV_ESC_FREE_OS_EVENT => OS_EVENT_FD,
            NV_ESC_RM_ALLOC_MEMORY => NVOS02_WITH_FD_FD,
            _ => return Err(libc::ENOTTY),
        };

        // The field is a descriptor only when the caller put one there. -1 is
        // the caller saying it has none, which several of these ioctls allow --
        // NV_ESC_RM_ALLOC_MEMORY carries it for every allocation not being made
        // on another open file. It is forwarded as it stands, because that is
        // what the host driver is being asked to read. Only -1: another
        // negative number is no descriptor either.
        let f = FdField {
            at: fd_offset,
            width: 4,
            accept: FdAccept::Device,
            none: FdNone::MinusOne,
        };
        let embedded = match self.fd_field(param_in, f) {
            Ok(v) => v,
            Err(e) => {
                if e == libc::EBADF {
                    log::warn!("fd-carrying ioctl {request:#x}: its descriptor is none of ours");
                }
                return Err(e);
            }
        };
        let none = matches!(embedded, FdIn::None);

        // The host's copy: the guest's block, the descriptor field declared
        // and holding our descriptor of the handle's file (or -1), and the
        // plan's fields.
        let mut a = Arena::new();
        let built = self
            .top_block(&mut a, request, param_in, plan)
            .and_then(|top| declare_fd(&mut a, top, f, &embedded).map(|_| top));
        let top = built?;
        if let Err(errno) = self.host_call(&mut a, host_fd, request, top) {
            if none {
                log::warn!("ioctl(0x{request:x}) with no embedded fd failed: errno={errno}");
            } else {
                log::warn!("fd-carrying ioctl(0x{:x}) failed: errno={}", request, errno);
            }
            let back = a.reply(top)[..param_in.len()].to_vec();
            return Ok(IoctlOut::failed(back, errno));
        }

        // The guest's handle back in place of our descriptor. It is not
        // what user mode wrote -- that was its own descriptor, which never
        // reached us -- so the guest driver writes the caller's value over it
        // on the way out (nvgpu_ioctl_translate_fd); RM never writes the
        // field (escape.c:393-428, 584-624), and callers read it back.
        Ok(IoctlOut::ok(a.reply(top)[..param_in.len()].to_vec()))
    }

    /// What the guest put in descriptor field `f` of `block`: its "none",
    /// or a handle of ours `f` accepts and our descriptor of it. EINVAL for
    /// a block too short to hold the field, EBADF for anything else. A
    /// 32-bit field is read as a u32, -1 being `u32::MAX`, which no handle
    /// is (handle_table.rs issues [1, i32::MAX]).
    pub(crate) fn fd_field(&self, block: &[u8], f: FdField) -> std::result::Result<FdIn<'_>, i32> {
        if !matches!(f.width, 4 | 8) {
            return Err(libc::EINVAL);
        }
        let v = le::uint_at(block, f.at, f.width).ok_or(libc::EINVAL)?;
        let sign = 1u64 << (8 * f.width - 1);
        let none = match f.none {
            FdNone::MinusOne => v == (sign << 1).wrapping_sub(1),
            FdNone::Zero => v == 0,
            FdNone::Negative => v & sign != 0,
            FdNone::Never => false,
        };
        if none {
            return Ok(FdIn::None);
        }
        let handle = u32::try_from(v).map_err(|_| libc::EBADF)?;
        match (self.handles.get(handle), f.accept) {
            (Some((fd, HandleKind::Dev(_))), FdAccept::Device)
            | (Some((fd, HandleKind::Dev(DeviceKind::Ctl))), FdAccept::ControlFile) => {
                Ok(FdIn::File { handle, fd })
            }
            (Some((fd, kind)), FdAccept::Kind(want)) if kind == want => {
                Ok(FdIn::File { handle, fd })
            }
            _ => Err(libc::EBADF),
        }
    }
}
