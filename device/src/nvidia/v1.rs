// SPDX-License-Identifier: Apache-2.0
//! The v1 IOCTL: the request read once, the route its handle's kind allows,
//! the flat DRM and NVKMS calls, and the reply serialised once.

#![forbid(unsafe_code)]

use super::*;

/// What a v1 IOCTL answers, before it is written: the parameters as the
/// host left them -- the top-level block, the nested one and the deep block,
/// laid out as the request's -- and how many of their last bytes are the
/// deep block. `errno` is 0, or what the host call failed with: the block
/// goes back all the same, as nvidia.ko's does (`write_v1`).
pub(crate) struct IoctlOut {
    params: Vec<u8>,
    deep_len: usize,
    errno: i32,
}

impl IoctlOut {
    pub(crate) fn ok(params: Vec<u8>) -> Self {
        Self::deep(params, 0)
    }

    pub(crate) fn deep(params: Vec<u8>, deep_len: usize) -> Self {
        Self {
            params,
            deep_len,
            errno: 0,
        }
    }

    pub(crate) fn failed(params: Vec<u8>, errno: i32) -> Self {
        Self {
            params,
            deep_len: 0,
            errno,
        }
    }
}

/// A v1 IOCTL's outcome: its answer, or the errno it is refused with (a bare
/// header).
pub(crate) type V1 = std::result::Result<IoctlOut, i32>;

/// The parameters of a call the host served: what every hook that follows
/// one reads, and none for a refusal or a failed call.
pub(super) fn served(r: &V1) -> Option<&[u8]> {
    match r {
        Ok(out) if out.errno == 0 => Some(&out.params),
        _ => None,
    }
}

/// As [`served`], for a hook that rewrites the reply.
pub(super) fn served_mut(r: &mut V1) -> Option<&mut [u8]> {
    match r {
        Ok(out) if out.errno == 0 => Some(&mut out.params),
        _ => None,
    }
}

/// A v1 IOCTL as the guest sent it, its blocks told apart once.
pub(super) struct V1Request<'a> {
    pub(super) ireq: IoctlReq,
    /// The top-level block and the nested one after it.
    pub(super) params: &'a [u8],
    pub(super) deep: Deep<'a>,
    /// What follows the blocks: the calling process, from a guest that says
    /// (rmshare.rs).
    pub(super) trailer: &'a [u8],
}

/// What follows a v1 IOCTL's nested block.
#[derive(Clone, Copy)]
pub(super) enum Deep<'a> {
    None,
    /// What the pointer at `ptr` inside the nested block refers to. The
    /// guest cannot send an address that means anything here, so it sends
    /// the bytes and says where the pointer sits; the call gives them a host
    /// address, and the reply carries them back.
    Single {
        ptr: usize,
        bytes: &'a [u8],
    },
    /// DEEP_SEGMENTED: what several pointers refer to, one segment each
    /// (deepseg.rs).
    Segments(&'a [u8]),
    /// DEEP_PAGE_LIST: the guest-physical pages of memory the caller
    /// registers with RM (osdesc.rs).
    PageList(&'a [u8]),
}

impl<'a> Deep<'a> {
    pub(super) fn single(self) -> Option<(usize, &'a [u8])> {
        match self {
            Deep::Single { ptr, bytes } => Some((ptr, bytes)),
            _ => None,
        }
    }

    pub(super) fn segments(self) -> Option<&'a [u8]> {
        match self {
            Deep::Segments(b) => Some(b),
            _ => None,
        }
    }

    pub(super) fn page_list(self) -> Option<&'a [u8]> {
        match self {
            Deep::PageList(b) => Some(b),
            _ => None,
        }
    }

    pub(super) fn bytes(self) -> &'a [u8] {
        match self {
            Deep::None => &[],
            Deep::Single { bytes, .. } | Deep::Segments(bytes) | Deep::PageList(bytes) => bytes,
        }
    }
}

impl<'a> V1Request<'a> {
    /// `payload` (after the header) read as a v1 IOCTL. EPROTO for one too
    /// short to be one, EINVAL for blocks longer than what was sent or a
    /// deep block the call cannot have: only an RM control's parameters and
    /// IDLE_CHANNELS' top-level block are read as segments, and a page list
    /// only on the three calls that register memory. Either is refused
    /// rather than ignored, so a guest never believes pointers were carried
    /// that were not.
    fn parse(payload: &'a [u8]) -> std::result::Result<Self, i32> {
        use abi::ioctl::*;
        let ireq = pod::read::<IoctlReq>(payload, 0).ok_or(libc::EPROTO)?;
        // The guest sends the top-level struct and the block any pointer in
        // it refers to, back to back.
        let body = &payload[size_of::<IoctlReq>()..];
        let nested_end = ireq.data_len as usize + ireq.nested_len as usize;
        let want = nested_end + ireq.deep_len as usize;
        if body.len() < want {
            log::warn!(
                "ioctl cmd={:#x}: guest promised {want} bytes and sent {}",
                ireq.cmd,
                body.len()
            );
            return Err(libc::EINVAL);
        }
        let bytes = &body[nested_end..want];
        let rm = hostfd::ioc_type(ireq.cmd) == b'F';
        let nr = hostfd::ioc_nr(ireq.cmd);
        let deep = match ireq.deep_ptr_offset {
            _ if ireq.deep_len == 0 => Deep::None,
            DEEP_SEGMENTED => {
                if !rm || !matches!(nr, NV_ESC_RM_CONTROL | NV_ESC_RM_IDLE_CHANNELS) {
                    log::warn!(
                        "ioctl cmd={:#x}: deep segments on a call that has none",
                        ireq.cmd
                    );
                    return Err(libc::EINVAL);
                }
                Deep::Segments(bytes)
            }
            DEEP_PAGE_LIST => {
                if !rm
                    || !matches!(
                        nr,
                        NV_ESC_RM_ALLOC | NV_ESC_RM_ALLOC_MEMORY | NV_ESC_RM_VID_HEAP_CONTROL
                    )
                {
                    log::warn!(
                        "ioctl cmd={:#x}: a page list on a call that registers no memory",
                        ireq.cmd
                    );
                    return Err(libc::EINVAL);
                }
                Deep::PageList(bytes)
            }
            ptr => Deep::Single {
                ptr: ptr as usize,
                bytes,
            },
        };
        Ok(Self {
            ireq,
            params: &body[..nested_end],
            deep,
            trailer: &body[want..],
        })
    }
}

/// Where a call's top-level block points at its parameters: the size of
/// the top-level block, the offset of the pointer, and the offset of the
/// size the host copies through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Nested {
    pub(super) outer_size: usize,
    pub(super) ptr_offset: usize,
    pub(super) size_offset: usize,
    /// Byte offset, inside the parameters, of a descriptor the guest sent
    /// as one of our handles and the host must see as one of our
    /// descriptors. `None` for the RM and NVKMS calls, which name their
    /// descriptors by command rather than by position.
    pub(super) fd_at: Option<usize>,
}

/// NvKmsIoctlParams: `{cmd, size, NvU64 address}`.
pub(super) const NVKMS_NESTED: Nested = Nested {
    outer_size: 16,
    ptr_offset: 8,
    size_offset: 4,
    fd_at: None,
};

/// Where a v1 IOCTL goes, once it is allowed at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum V1Route {
    /// NVIDIA's RM escapes, type 'F', checked against the ABI profile.
    Rm,
    /// nvidia-uvm, whose command numbers carry type 0.
    Uvm,
    /// nvidia-modeset, type 'm'.
    Nvkms,
    /// An nvidia-drm GEM ioctl with an NVKMS parameter block behind a pointer.
    DrmNested(Nested),
    /// A flat, pointer-free, fd-free ioctl the v1 guest sends on its render
    /// handle.
    DrmFlat,
}

/// nvidia-drm and DRM core commands the v1 guest forwards on a render handle,
/// by their full numbers (nv_drm_common_ioctl.h:71-133, drm.h:1104). A full
/// number fixes the size the host will copy, so a guest cannot pair a known
/// nr with a larger `_IOC_SIZE`.
pub(super) const DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY: u32 =
    hostfd::ioc(hostfd::IOC_RW, b'd', 0x41, 32);

pub(super) const DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY: u32 =
    hostfd::ioc(hostfd::IOC_RW, b'd', 0x49, 24);

pub(super) const DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY: u32 =
    hostfd::ioc(hostfd::IOC_RW, b'd', 0x4b, 24);

pub(super) const DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY: u32 =
    hostfd::ioc(hostfd::IOC_RW, b'd', 0x4d, 24);

/// Whether a v1 IOCTL may run on a handle of `kind`, and how.
///
/// The v1 message forwards the guest's command number with the guest's bytes
/// and none of IOCTL2's schema checks, so it is limited, by the backend's own
/// tables and by the handle's kind, to what a v1 guest actually sends: RM
/// escapes on the NVIDIA devices, UVM on the UVM devices, NVKMS on
/// nvidia-modeset, and on a render node the three nested nvidia-drm GEM calls
/// and three flat ones. Everything else is -EPERM, and every new kind -- a
/// card, a lease, a sync_file -- takes no v1 ioctl at all. Without this a
/// hostile guest kernel could send ADDFB2 as a v1 IOCTL on a KMS handle and
/// reach nvidia-drm's NULL dereference on a non-NVKMS GEM object
/// (nvidia-drm-fb.c:158-167) past every IOCTL2 check.
///
/// Three commands are refused on every handle
/// ([`hostfd::refused_everywhere`]).
///
/// -EINVAL for a known command whose size does not match what was sent.
pub(super) fn v1_route(
    kind: HandleKind,
    cmd: u32,
    data_len: u32,
) -> std::result::Result<V1Route, i32> {
    let ty = hostfd::ioc_type(cmd);
    if hostfd::refused_everywhere(cmd) {
        return Err(libc::EPERM);
    }
    let sized = |n: usize, r: V1Route| {
        if data_len as usize == n {
            Ok(r)
        } else {
            Err(libc::EINVAL)
        }
    };
    match kind {
        HandleKind::Dev(DeviceKind::Gpu(_) | DeviceKind::Ctl) if ty == b'F' => Ok(V1Route::Rm),
        HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools) if ty == 0 => Ok(V1Route::Uvm),
        HandleKind::Dev(DeviceKind::Modeset) if ty == b'm' => Ok(V1Route::Nvkms),
        // Another type on an NVIDIA device gets the device's own answer:
        // nvidia.ko's nv_validate_ioctls
        // says EINVAL (nv.c:2488-2491), nvidia-modeset ENOTTY for any
        // command but its one (nvidia-modeset-linux.c:1953-1955), and UVM
        // ENOSYS for a command it has no route for (uvm_test.c).
        HandleKind::Dev(DeviceKind::Gpu(_) | DeviceKind::Ctl) => Err(libc::EINVAL),
        HandleKind::Dev(DeviceKind::Uvm | DeviceKind::UvmTools) => Err(libc::ENOSYS),
        HandleKind::Dev(DeviceKind::Modeset) => Err(libc::ENOTTY),
        HandleKind::DriRender(_) => match cmd {
            DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY => sized(
                32,
                V1Route::DrmNested(Nested {
                    outer_size: 32,
                    ptr_offset: 8,
                    size_offset: 16,
                    // Both NVKMS blocks begin with `int memFd`.
                    fd_at: Some(0),
                }),
            ),
            DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY
            | DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY => sized(
                24,
                V1Route::DrmNested(Nested {
                    outer_size: 24,
                    ptr_offset: 8,
                    size_offset: 16,
                    fd_at: Some(0),
                }),
            ),
            hostfd::DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET
            | DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY
            | hostfd::DRM_IOCTL_GEM_CLOSE => sized(hostfd::ioc_size(cmd), V1Route::DrmFlat),
            _ => Err(libc::EPERM),
        },
        _ => Err(libc::EPERM),
    }
}

impl NvidiaBackend {
    pub(super) fn handle_ioctl(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let r = self.serve_ioctl(payload, resp_buf.len());
        self.write_v1(resp_buf, r)
    }

    /// A v1 IOCTL whose reply may be at most `cap` bytes.
    pub(super) fn serve_ioctl(&mut self, payload: &[u8], cap: usize) -> V1 {
        let req = V1Request::parse(payload)?;
        let (ireq, param_in, page_list) = (req.ireq, req.params, req.deep.page_list());

        // Room for the answer, before anything is asked of the host: a
        // success comes back with at least the top-level struct and nested
        // block (and a registration's 8-byte id), and one that did not fit
        // was answered ENOSPC after RM had acted -- for a registration, with
        // the pages pinned by RM and the guest unpinning them on the error.
        let least = size_of::<MsgHeader>()
            + size_of::<IoctlResp>()
            + param_in.len()
            + if page_list.is_some() { 8 } else { 0 };
        if cap < least {
            log::warn!(
                "ioctl cmd={:#x}: a reply of at least {least} bytes does not fit the {cap} posted",
                ireq.cmd,
            );
            return Err(libc::ENOSPC);
        }

        // How much of the response is the top-level struct. The driver copies
        // exactly this much back to userspace and reads any nested block after
        // it, so a wrong split corrupts one or the other.
        self.current_data_len = ireq.data_len;

        let request = ireq.cmd as u64;
        let (host_fd, kind) = match (
            self.handles.get_raw(self.current_handle),
            self.current_kind(),
        ) {
            (Ok(fd), Some(kind)) => (fd, kind),
            _ => return Err(libc::EBADF),
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

        let route = match v1_route(kind, ireq.cmd, ireq.data_len) {
            Ok(r) => r,
            Err(errno) => {
                log::warn!(
                    "v1 ioctl {:#010x} ({} bytes) on handle {} ({kind:?}) refused: {}",
                    ireq.cmd,
                    ireq.data_len,
                    self.current_handle,
                    if errno == libc::EPERM {
                        "not a call this kind of handle takes over v1"
                    } else {
                        "its size does not match the command"
                    }
                );
                return Err(errno);
            }
        };

        if page_list.is_some() && !matches!(route, V1Route::Rm) {
            log::warn!(
                "ioctl cmd={:#x}: a page list on handle {} ({kind:?}), which is no RM file",
                ireq.cmd,
                self.current_handle
            );
            return Err(libc::EINVAL);
        }

        match route {
            V1Route::Rm => self.serve_rm_escape(host_fd, &req),
            V1Route::Uvm => self.serve_uvm_v1(host_fd, kind, &req),
            V1Route::DrmFlat => self.serve_drm_flat_v1(host_fd, &req),
            V1Route::Nvkms => self.serve_nvkms_v1(host_fd, &req),
            V1Route::DrmNested(layout) => {
                log::debug!("drm ioctl nr={escape:#04x} ({} bytes in)", param_in.len());
                self.dispatch_nested(
                    host_fd,
                    request,
                    param_in,
                    &crate::guestptr::Plan::default(),
                    layout,
                    req.deep,
                )
            }
        }
    }

    /// One of the flat nvidia-drm and DRM calls a v1 guest makes on its
    /// render handle.
    pub(super) fn serve_drm_flat_v1(&mut self, host_fd: RawFd, req: &V1Request<'_>) -> V1 {
        let (ireq, param_in) = (req.ireq, req.params);
        let request = u64::from(ireq.cmd);
        let r = self.dispatch_simple(
            host_fd,
            request,
            param_in,
            &crate::guestptr::Plan::default(),
        );
        // A fence context is a GEM object of the file, and counts
        // against its cap until it is closed (semsurf.rs).
        // drm_gem_close: the handle first.
        if ireq.cmd == hostfd::DRM_IOCTL_GEM_CLOSE
            && served(&r).is_some()
            && let Some(gem) = le::u32_at(param_in, 0)
        {
            self.semsurf.gem_closed(self.current_handle, gem);
            self.inject.gem_closed(self.current_handle, gem);
        }
        r
    }

    /// A v1 NVKMS call, on nvidia-modeset.
    pub(super) fn serve_nvkms_v1(&mut self, host_fd: RawFd, req: &V1Request<'_>) -> V1 {
        let (ireq, param_in) = (req.ireq, req.params);
        let request = u64::from(ireq.cmd);
        let deep_in = req.deep.single();
        // NVKMS multiplexes every operation through one ioctl number,
        // so the number says nothing and the command inside says
        // everything. v1 carries one flat block and no descriptor, so
        // only commands the host's NVKMS table has with no pointer and
        // no descriptor go this way, under the same policy as IOCTL2
        // (nvkms.rs, which may rewrite the block); everything else
        // needs IOCTL2. A command number is never trusted across
        // driver versions: REGISTER_SURFACE is 16 in one release and
        // 17 in the next.
        let mut msg = param_in.to_vec();
        let ok = request == u64::from(crate::schema::NVKMS_IOCTL_IOWR)
            && ireq.data_len == 16
            && deep_in.is_none();
        let checked = if ok {
            self.recheck_granting_leases();
            self.nvkms.v1_before(self.current_handle, &mut msg)
        } else {
            Err(libc::EINVAL)
        };
        if let Err(errno) = checked {
            log::warn!(
                "v1 NVKMS call {:#x} on handle {} refused ({errno})",
                le::u32_at(&msg, 0).unwrap_or(0),
                self.current_handle
            );
            return Err(errno);
        }
        // A dpy probed too recently is answered with the last reply
        // (nvkms.rs, `dpy_probe`; S-8), laid out as the host's.
        if self.nvkms.v1_cached(self.current_handle, &mut msg) {
            return Ok(IoctlOut::ok(msg));
        }
        let mut r = self.dispatch_nested(
            host_fd,
            request,
            &msg,
            &crate::guestptr::Plan::default(),
            NVKMS_NESTED,
            Deep::None,
        );
        // The one reply the policy rewrites (ALLOC_DEVICE's display
        // coherency modes, nvkms.rs), laid out as `msg` was.
        if let Some(out) = served_mut(&mut r) {
            self.nvkms.v1_after(out);
            self.nvkms.v1_record(self.current_handle, out);
        }
        r
    }

    pub(super) fn dispatch_simple(
        &mut self,
        host_fd: RawFd,
        request: u64,
        param_in: &[u8],
        plan: &crate::guestptr::Plan<'_>,
    ) -> V1 {
        let escape = (request & 0xFF) as u32;
        log::debug!(
            "dispatch_simple: host_fd={} request=0x{:x} escape=0x{:02x} size={}",
            host_fd,
            request,
            escape,
            param_in.len()
        );

        let n_in = param_in.len();
        let mut a = Arena::new();
        let top = self.top_block(&mut a, request, param_in, plan)?;

        // NV_ESC_SYS_PARAMS and NV_ESC_CHECK_VERSION_STR go as the guest
        // sent them, and their answers come back as the host gave them
        // (SECURITY.md, "Fail closed"). SYS_PARAMS carries the caller's memory block
        // size, which RM keeps from its first caller and answers EBUSY for
        // any other; this once rewrote the block and made up a success. And
        // CHECK_VERSION_STR was rewritten to query mode ('2'), in which RM
        // skips comparing the caller's version with its own: a guest
        // userspace of another release then ran against this RM unnoticed.
        let rc = self.host_call(&mut a, host_fd, request, top);
        if let Err(errno) = rc {
            log::debug!("ioctl(0x{request:x}/0x{escape:02x}) failed: errno={errno}");
            let back = a.reply(top)[..n_in].to_vec();
            return Ok(IoctlOut::failed(back, errno));
        }
        // The host's bytes, with the caller's values in every field the plan
        // declared; only what the guest sent goes back.
        let param_buf = a.reply(top)[..n_in].to_vec();
        drop(a);
        Ok(IoctlOut::ok(param_buf))
    }

    /// Write a v1 IOCTL's outcome, the one place it is serialised: a refusal
    /// as a bare header; an answer as the header, the lengths, then the
    /// bytes.
    ///
    /// The split between the top-level struct and the nested block is taken
    /// from the request, because the guest copies exactly `data_len` bytes
    /// back to the caller's struct and reads any nested block after it. The
    /// last `deep_len` bytes are what a pointer inside the nested block
    /// addresses, declared separately so the guest knows to copy them
    /// somewhere else.
    ///
    /// A host call that failed is answered as nvidia.ko answers it: the
    /// errno, and the argument block as the host left it, which nv.c copies
    /// out on every error but EFAULT (nv.c:2869-2878) and drm_ioctl
    /// unconditionally. So a caller reads what RM wrote before failing --
    /// CHECK_VERSION_STR's reply word and RM's own version string on a
    /// mismatch, which libnvidia prints. The guest copies bytes that come with a negative status
    /// (nvgpu_rmio.c).
    pub(super) fn write_v1(&self, resp_buf: &mut [u8], r: V1) -> usize {
        let (param_out, deep_len, status) = match r {
            Err(errno) => return self.write_error(resp_buf, errno),
            Ok(IoctlOut {
                params,
                deep_len,
                errno: 0,
            }) => (params, deep_len, 0),
            Ok(IoctlOut { params, errno, .. }) => {
                let errno = errno.saturating_abs();
                if errno == libc::EFAULT || params.is_empty() {
                    return self.write_error(resp_buf, errno);
                }
                (params, 0, -errno)
            }
        };
        let data_len = (self.current_data_len as usize).min(param_out.len());
        let deep_len = deep_len.min(param_out.len() - data_len);
        let nested_len = param_out.len() - data_len - deep_len;

        let need = size_of::<MsgHeader>() + size_of::<IoctlResp>() + param_out.len();
        if resp_buf.len() < need {
            return self.write_error(resp_buf, libc::ENOSPC);
        }

        let mut off = self.write_hdr(resp_buf, self.current_handle, status);
        off += write_struct(
            &mut resp_buf[off..],
            &IoctlResp {
                data_len: data_len as u32,
                nested_len: nested_len as u32,
                deep_len: deep_len as u32,
            },
        );
        resp_buf[off..off + param_out.len()].copy_from_slice(&param_out);
        off + param_out.len()
    }
}
