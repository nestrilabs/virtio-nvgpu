// SPDX-License-Identifier: Apache-2.0
//! EXPORT_TO_DMABUF_FD with `--allow-dmabuf-export` (rmexport.rs says what
//! is checked and why): the escape served, and the dma-buf RM makes turned
//! at once into a GEM object of a render file the calling process opened,
//! which the guest makes a proxy and a guest dma-buf of.

#![forbid(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, OwnedFd};

use protocol::messages::{DmabufExportReq, DmabufExportResp, ProcId};

use super::*;
use crate::hostfd::NV_GEM_OBJECT_DMABUF;
use crate::nvos::{NV_ERR_INSUFFICIENT_PERMISSIONS, NV_OK};
use crate::quota::Owner;
use crate::rmexport::{self, Layout, RM_EXPORTER};
use crate::rmshare::Caller;

/// RM's answer for a dma-buf the backend could not make the guest's: what
/// nv_dma_buf_create answers when the kernel's side of it fails.
const NV_ERR_OPERATING_SYSTEM: u32 = 0x59;

impl NvidiaBackend {
    /// Whether `escape` is the export served here: the switch on, and the
    /// export gate marked to keep what it makes in the VM.
    pub(super) fn serves_dmabuf_export(&self, escape: u32) -> bool {
        escape == abi::ioctl::NV_ESC_EXPORT_TO_DMABUF_FD
            && self.config.allow_dmabuf_export
            && self.guest_only.get().is_some()
    }

    /// EXPORT_TO_DMABUF_FD from a guest that sent the calling process and a
    /// [`DmabufExportReq`] after the block. Every refusal before RM is RM's
    /// own status in the block, as RM answers what it will not do; one of
    /// the message itself (no caller, a render file not the caller's) is an
    /// errno.
    pub(super) fn serve_dmabuf_export(&mut self, host_fd: RawFd, req: &V1Request<'_>) -> V1 {
        let request = u64::from(req.ireq.cmd);
        // A GPU's file: RM serves it nowhere else (nv.c,
        // NV_ACTUAL_DEVICE_ONLY), and answers EINVAL.
        if !matches!(
            self.current_kind(),
            Some(HandleKind::Dev(DeviceKind::Gpu(_)))
        ) {
            return Err(libc::EINVAL);
        }
        let Some(l) = Layout::for_len(req.ireq.data_len as usize) else {
            return Err(libc::EINVAL);
        };
        let block = req.params;
        if block.len() != l.len || !req.deep.bytes().is_empty() {
            return Err(libc::EINVAL);
        }
        // Room for the answer before RM is asked: a dma-buf made and never
        // told of would be the guest's to leak.
        let least =
            size_of::<MsgHeader>() + size_of::<IoctlResp>() + l.len + size_of::<DmabufExportResp>();
        if req.cap < least {
            return Err(libc::ENOSPC);
        }
        let (Some(id), Some(to)) = (
            crate::sys::pod::read::<ProcId>(req.trailer, 0),
            crate::sys::pod::read::<DmabufExportReq>(req.trailer, size_of::<ProcId>()),
        ) else {
            log::warn!("EXPORT_TO_DMABUF_FD without the caller and its render file; refused");
            return Err(libc::EINVAL);
        };
        if !self.session.proc_ids || to.reserved != 0 {
            return Err(libc::EINVAL);
        }
        let caller = Caller::from_wire(&id, self.session.proc_euid);
        let owner = Owner::from_wire(&id);
        let answer = |status: u32| Ok(IoctlOut::ok(nvos::with_status(block, l.status, status)));

        let ask = match rmexport::check(&l, block) {
            Ok(a) => a,
            Err(status) => {
                log::warn!("EXPORT_TO_DMABUF_FD refused before RM: status {status:#x}");
                return answer(status);
            }
        };
        // hClient: this VM's, and made by the calling process. RM checks
        // nothing of it (rmexport.rs).
        let mine = self.semsurf.owns_client(ask.client)
            && self
                .semsurf
                .owner_of(ask.client)
                .is_some_and(|maker| maker.same_process(&caller));
        if !mine {
            log::warn!(
                "EXPORT_TO_DMABUF_FD of client {:#x}, which is not the calling process's; \
                 refused",
                ask.client
            );
            return answer(NV_ERR_INSUFFICIENT_PERMISSIONS);
        }
        // The render file: one the calling process opened.
        if !matches!(self.handles.kind(to.render), Some(HandleKind::DriRender(_)))
            || self.handles.owner(to.render) != owner
        {
            log::warn!(
                "EXPORT_TO_DMABUF_FD into handle {}, which is no render file of the calling \
                 process; refused",
                to.render
            );
            return Err(libc::EBADF);
        }
        if let Err(status) = self.rm_exports.admits(owner, ask.total_size) {
            return answer(status);
        }

        // RM, with the descriptor it makes declared: -1 going in, so any
        // other number there after is the one it installed.
        let mut a = Arena::new();
        let top = a.block(block, ioctl_arg_len(request, block.len()))?;
        a.fd_out(top, rmexport::FD, 4)?;
        let called = self.host_call(&mut a, host_fd, request, top);
        let made = a.claim_fd(top, rmexport::FD).and_then(|fd| self.fresh(fd));
        let mut back = a.reply(top);
        back.truncate(l.len);
        // The guest writes its own descriptor there; never one of ours.
        let _ = le::put_u32(&mut back, rmexport::FD, u32::MAX);
        if let Err(errno) = called {
            return Ok(IoctlOut::failed(back, errno));
        }
        let status = le::u32_at(&back, l.status).unwrap_or(NV_ERR_OPERATING_SYSTEM);
        let dmabuf = match (made, status) {
            (Some(fd), NV_OK) => fd,
            (None, NV_OK) => {
                log::warn!("EXPORT_TO_DMABUF_FD: RM said NV_OK and made no descriptor");
                let _ = le::put_u32(&mut back, l.status, NV_ERR_OPERATING_SYSTEM);
                return Ok(IoctlOut::ok(back));
            }
            // A refusal of RM's own, which the guest reads as it is. (RM
            // makes no descriptor with one; one that did is closed here.)
            (_, _) => return Ok(IoctlOut::ok(back)),
        };
        // From here every failure drops `dmabuf`, which closes the last
        // reference to it and undoes the export (nv_dma_buf_release).
        match self.adopt_export(dmabuf, to.render, owner, ask.total_size) {
            Ok(resp) => {
                log::debug!(
                    "EXPORT_TO_DMABUF_FD: {} bytes of client {:#x} as GEM {} of render handle {}",
                    resp.size,
                    ask.client,
                    resp.gem,
                    to.render
                );
                back.extend_from_slice(crate::sys::pod::bytes(&resp));
                Ok(IoctlOut::deep(back, size_of::<DmabufExportResp>()))
            }
            Err(why) => {
                log::warn!("EXPORT_TO_DMABUF_FD: {why}; the dma-buf RM made is closed");
                let _ = le::put_u32(&mut back, l.status, NV_ERR_OPERATING_SYSTEM);
                Ok(IoctlOut::ok(back))
            }
        }
    }

    /// A descriptor RM wrote back, if it is one RM just made: one the
    /// backend already holds is not RM's to hand over, and is neither kept
    /// nor closed (privfd.rs).
    fn fresh(&self, fd: OwnedFd) -> Option<OwnedFd> {
        let raw = fd.as_raw_fd();
        if self.handles.owns_fd(raw) || crate::privfd::is_private(raw) {
            log::warn!(
                "EXPORT_TO_DMABUF_FD: RM answered with descriptor {raw}, one the backend holds"
            );
            std::mem::forget(fd);
            return None;
        }
        Some(fd)
    }

    /// The dma-buf RM just made, checked and imported into render handle
    /// `render`, and recorded against `owner`: what the guest is told.
    /// `dmabuf` is closed however this ends.
    fn adopt_export(
        &mut self,
        dmabuf: OwnedFd,
        render: u32,
        owner: Owner,
        size: u64,
    ) -> std::result::Result<DmabufExportResp, String> {
        let host = self.export_host.clone();
        match host.exporter(dmabuf.as_fd()) {
            Some(e) if e == RM_EXPORTER => {}
            e => {
                return Err(format!(
                    "the descriptor is no dma-buf of RM's (exporter {e:?})"
                ));
            }
        }
        match host.size(dmabuf.as_fd()) {
            Ok(s) if s == size => {}
            s => return Err(format!("the dma-buf is {s:?} bytes, not {size}")),
        }
        let (fd, _) = self
            .handles
            .get(render)
            .ok_or_else(|| "the render file is gone".to_string())?;
        let gem = host
            .prime_import(fd, dmabuf.as_fd())
            .map_err(|e| format!("PRIME_FD_TO_HANDLE: {e}"))?;
        // A dma-buf just made is in no file yet, so the handle is new and
        // ours to close if the guest is not to hear of it.
        match host.identify(fd, gem) {
            Ok(NV_GEM_OBJECT_DMABUF) => {}
            t => {
                host.gem_close(fd, gem);
                return Err(format!("the import identifies as {t:?}"));
            }
        }
        self.rm_exports.record(render, gem, owner, size);
        Ok(DmabufExportResp {
            gem,
            object_type: NV_GEM_OBJECT_DMABUF,
            size,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::os::fd::{BorrowedFd, OwnedFd};
    use std::sync::Mutex;

    use abi::ioctl::NV_ESC_EXPORT_TO_DMABUF_FD;
    use protocol::messages::{
        GCAP_PROC_EUID, GCAP_PROC_ID, HELLO_F_FRESH, HelloReq, MsgType, PROTO_V2,
    };

    use super::*;
    use crate::hostfd::{IOC_RW, NV_GEM_OBJECT_NVKMS, ioc};
    use crate::nvos::{
        NV_ERR_INSUFFICIENT_RESOURCES, NV_ERR_NOT_SUPPORTED, NV01_DEVICE_0, NVOS64_STATUS,
    };
    use crate::rmexport::{EXPORTS_SHARE, ExportHost as _, H_CLIENT};
    use crate::testing::rm;

    const WIDE: usize = 2608;
    const EXPORT: u32 = ioc(IOC_RW, b'F', NV_ESC_EXPORT_TO_DMABUF_FD, WIDE);
    const ALLOC: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
    const DEVICE: u32 = 0xde7;
    const MEMORY: u32 = 0x3e3;
    const NV01_MEMORY_LOCAL_USER: u32 = 0x40;
    /// Where a v1 reply's parameters start: MsgHeader, IoctlResp.
    const BODY: usize = 16 + 12;
    const MIB: u64 = 1 << 20;

    /// nvidia-drm as the export sees it: an import holds the dma-buf (a
    /// duplicate kept here) until its GEM_CLOSE.
    #[derive(Default)]
    struct FakeDrm {
        st: Mutex<St>,
    }

    #[derive(Default)]
    struct St {
        next: u32,
        held: HashMap<u32, OwnedFd>,
        closed: Vec<u32>,
        identify_as: Option<u32>,
        fail_import: bool,
        exporter: Option<String>,
    }

    impl FakeDrm {
        fn st(&self) -> std::sync::MutexGuard<'_, St> {
            self.st.lock().unwrap()
        }
    }

    impl rmexport::ExportHost for FakeDrm {
        fn exporter(&self, fd: BorrowedFd<'_>) -> Option<String> {
            if let Some(e) = self.st().exporter.clone() {
                return Some(e);
            }
            let l = std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).ok()?;
            l.to_string_lossy()
                .starts_with("/memfd:nv_dmabuf")
                .then(|| RM_EXPORTER.to_string())
        }
        fn size(&self, fd: BorrowedFd<'_>) -> std::io::Result<u64> {
            crate::sys::fd::size(fd.as_raw_fd())
        }
        fn prime_import(&self, _: BorrowedFd<'_>, dmabuf: BorrowedFd<'_>) -> std::io::Result<u32> {
            let mut st = self.st();
            if st.fail_import {
                return Err(std::io::Error::from_raw_os_error(libc::ENOTSUP));
            }
            st.next += 1;
            let g = st.next;
            st.held.insert(g, dmabuf.try_clone_to_owned()?);
            Ok(g)
        }
        fn identify(&self, _: BorrowedFd<'_>, _: u32) -> std::io::Result<u32> {
            Ok(self.st().identify_as.unwrap_or(NV_GEM_OBJECT_DMABUF))
        }
        fn gem_close(&self, _: BorrowedFd<'_>, gem: u32) {
            let mut st = self.st();
            st.held.remove(&gem);
            st.closed.push(gem);
        }
    }

    fn pid(tgid: u32) -> ProcId {
        ProcId {
            start_ns: 1_000_000 + u64::from(tgid),
            tgid,
            euid: 1000 + tgid,
        }
    }

    fn null() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    struct Vm {
        be: NvidiaBackend,
        drm: Arc<FakeDrm>,
        ctl: u32,
        gpu: u32,
    }

    /// A v2 session whose guest sends the calling process, with the switch
    /// `on`, a control file and a GPU file.
    fn vm(on: bool) -> Vm {
        rm::seen();
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        rm::install(&mut be);
        if on {
            be.allow_dmabuf_export();
        }
        let drm = Arc::new(FakeDrm::default());
        be.export_host = drm.clone();
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID | GCAP_PROC_EUID,
            uvm_aperture_mib: 0,
        };
        let mut req = Vec::new();
        for v in [MsgType::Hello as u32, 0, 0, 1] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(crate::sys::pod::bytes(&hello));
        let mut resp = vec![0u8; 256];
        let n = be.dispatch(&req, &mut resp);
        assert!(n >= 24);
        let caps = le::u32_at(&resp, 16 + 4).unwrap();
        assert_eq!(
            caps & protocol::messages::BCAP_DMABUF_EXPORT != 0,
            on,
            "offered with the switch, and only then"
        );
        let ctl = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        let gpu = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Gpu(0)));
        Vm { be, drm, ctl, gpu }
    }

    /// A v1 IOCTL on `handle`, with `trailer` after its block.
    fn call(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        block: &[u8],
        trailer: &[u8],
    ) -> Vec<u8> {
        let mut req = Vec::new();
        for v in [
            MsgType::Ioctl as u32,
            handle,
            0,
            0,
            cmd,
            block.len() as u32,
            0,
            0,
            0,
            0,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(block);
        req.extend_from_slice(trailer);
        let mut resp = vec![0u8; 8192];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn errno(r: &[u8]) -> i32 {
        i32::from_le_bytes(r[8..12].try_into().unwrap())
    }

    impl Vm {
        /// A render file opened by `by`.
        fn render(&mut self, by: u32) -> u32 {
            self.be
                .adopt_for_test_as(null(), HandleKind::DriRender(0), Owner::from_wire(&pid(by)))
        }

        /// A client made by `by`, with a device and video memory `MEMORY`
        /// of `mib` MiB in it.
        fn client(&mut self, by: u32) -> u32 {
            let mut b = vec![0u8; 48];
            le::put_u32(&mut b, 12, 0x41);
            let r = call(
                &mut self.be,
                self.ctl,
                ALLOC,
                &b,
                crate::sys::pod::bytes(&pid(by)),
            );
            assert_eq!(errno(&r), 0);
            assert_eq!(le::u32_at(&r, BODY + NVOS64_STATUS), Some(0));
            let c = le::u32_at(&r, BODY + 8).unwrap();
            rm::with(|rm| {
                rm.alloc(c, c, DEVICE, NV01_DEVICE_0).unwrap();
                rm.alloc(c, DEVICE, MEMORY, NV01_MEMORY_LOCAL_USER).unwrap();
            });
            c
        }

        /// EXPORT_TO_DMABUF_FD of `MEMORY` of `client`, `bytes` of it, by
        /// `by` into `render`: the reply.
        fn export_block(&mut self, block: &[u8], by: Option<u32>, render: u32) -> Vec<u8> {
            let mut t = Vec::new();
            if let Some(by) = by {
                t.extend_from_slice(crate::sys::pod::bytes(&pid(by)));
                t.extend_from_slice(crate::sys::pod::bytes(&DmabufExportReq {
                    render,
                    reserved: 0,
                }));
            }
            call(&mut self.be, self.gpu, EXPORT, block, &t)
        }

        fn export(&mut self, client: u32, bytes: u64, by: u32, render: u32) -> Vec<u8> {
            let b = block(client, &[bytes]);
            self.export_block(&b, Some(by), render)
        }
    }

    fn layout() -> Layout {
        Layout::for_len(WIDE).unwrap()
    }

    /// An export's block of `sizes`, each of `MEMORY`.
    fn block(client: u32, sizes: &[u64]) -> Vec<u8> {
        let l = layout();
        let mut b = vec![0u8; WIDE];
        b[0..4].copy_from_slice(&(-1i32).to_le_bytes());
        le::put_u32(&mut b, H_CLIENT, client);
        le::put_u32(&mut b, rmexport::TOTAL_OBJECTS, sizes.len() as u32);
        le::put_u32(&mut b, rmexport::NUM_OBJECTS, sizes.len() as u32);
        le::put_u64(&mut b, rmexport::TOTAL_SIZE, sizes.iter().sum());
        for (i, s) in sizes.iter().enumerate() {
            le::put_u32(&mut b, l.handles + 4 * i, MEMORY);
            le::put_u64(&mut b, l.sizes + 8 * i, *s);
        }
        b
    }

    fn status(r: &[u8]) -> u32 {
        assert_eq!(errno(r), 0, "the ioctl succeeds; RM's status says");
        le::u32_at(r, BODY + layout().status).unwrap()
    }

    fn exported(r: &[u8]) -> Option<DmabufExportResp> {
        crate::sys::pod::read::<DmabufExportResp>(r, BODY + WIDE)
    }

    fn reached() -> bool {
        rm::seen()
            .iter()
            .any(|c| c.nr == NV_ESC_EXPORT_TO_DMABUF_FD)
    }

    #[test]
    fn the_switch_off_refuses_the_escape_as_before() {
        let mut v = vm(false);
        let c = v.client(1);
        let r0 = v.render(1);
        rm::seen();
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(errno(&r), -libc::EOPNOTSUPP);
        assert!(!reached());
        // Set without the export gate's mark, it is still refused.
        v.be.config_mut().allow_dmabuf_export = true;
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(errno(&r), -libc::EOPNOTSUPP);
        assert!(!reached());
        assert_eq!(rm::open_exports(), 0);
    }

    /// The calling process's own video memory: RM makes the dma-buf, the
    /// render file imports it, and the guest hears of the GEM object, never
    /// of a descriptor of the backend's. The backend keeps none either.
    #[test]
    fn a_process_exports_its_own_memory_into_its_own_render_file() {
        let mut v = vm(true);
        let c = v.client(1);
        let r0 = v.render(1);
        rm::seen();
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(status(&r), NV_OK);
        assert!(reached());
        assert_eq!(
            le::i32_at(&r, BODY),
            Some(-1),
            "no backend number reaches the guest"
        );
        let e = exported(&r).expect("the export's GEM object");
        assert_eq!((e.object_type, e.size), (NV_GEM_OBJECT_DMABUF, 2 * MIB));
        assert_eq!(v.be.rm_exports.len(), 1);
        assert!(v.be.rm_exports.is_export(r0, e.gem));
        // Only the import holds the dma-buf.
        assert_eq!(rm::open_exports(), 1);
        v.drm.gem_close(null().as_fd(), e.gem);
        assert_eq!(rm::open_exports(), 0);
    }

    /// Another process's client, another VM's, one the guest never said
    /// who made: RM is never asked, whatever the handles.
    #[test]
    fn no_client_but_the_callers_own_is_exported() {
        let mut v = vm(true);
        let theirs = v.client(1);
        let r2 = v.render(2);
        rm::seen();
        let r = v.export(theirs, 2 * MIB, 2, r2);
        assert_eq!(status(&r), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached());
        // A client of the host's that no guest made.
        let r = v.export(0xc1d0_0999, 2 * MIB, 2, r2);
        assert_eq!(status(&r), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached());
        // A call that does not say who makes it.
        let b = block(theirs, &[2 * MIB]);
        let r = v.export_block(&b, None, r2);
        assert_eq!(errno(&r), -libc::EINVAL);
        assert!(!reached());
        assert!(v.be.rm_exports.is_empty());
        assert_eq!(rm::open_exports(), 0);
    }

    /// The render file must be the caller's own: not another process's,
    /// not a file of another kind.
    #[test]
    fn the_render_file_is_the_callers() {
        let mut v = vm(true);
        let c = v.client(1);
        let other = v.render(2);
        rm::seen();
        let r = v.export(c, 2 * MIB, 1, other);
        assert_eq!(errno(&r), -libc::EBADF);
        let ctl = v.ctl;
        let r = v.export(c, 2 * MIB, 1, ctl);
        assert_eq!(errno(&r), -libc::EBADF);
        let r = v.export(c, 2 * MIB, 1, 9999);
        assert_eq!(errno(&r), -libc::EBADF);
        assert!(!reached());
    }

    /// A freed object is RM's to refuse, and nothing is left; the append
    /// form, a second export into one dma-buf, is refused before RM.
    #[test]
    fn a_closed_object_and_a_double_export_leave_nothing() {
        let mut v = vm(true);
        let c = v.client(1);
        let r0 = v.render(1);
        rm::with(|rm| rm.free(c, MEMORY).unwrap());
        rm::seen();
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(status(&r), rm::NV_ERR_OBJECT_NOT_FOUND);
        assert!(reached());
        assert!(exported(&r).is_none());
        assert!(v.be.rm_exports.is_empty());
        // The append form, into a number the guest names.
        let mut b = block(c, &[2 * MIB]);
        b[0..4].copy_from_slice(&3i32.to_le_bytes());
        let r = v.export_block(&b, Some(1), r0);
        assert_eq!(status(&r), NV_ERR_NOT_SUPPORTED);
        assert!(!reached());
        // One part of two.
        let mut b = block(c, &[2 * MIB]);
        le::put_u32(&mut b, rmexport::TOTAL_OBJECTS, 2);
        let r = v.export_block(&b, Some(1), r0);
        assert_eq!(status(&r), NV_ERR_NOT_SUPPORTED);
        assert!(!reached());
        assert_eq!(rm::open_exports(), 0);
    }

    /// Whatever fails after RM made the dma-buf closes it and records
    /// nothing: an import that fails, one that is not a dma-buf object, a
    /// descriptor of another exporter, of another size.
    #[test]
    fn a_failure_after_rm_made_the_dma_buf_undoes_it() {
        let mut v = vm(true);
        let c = v.client(1);
        let r0 = v.render(1);
        v.drm.st().fail_import = true;
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(status(&r), NV_ERR_OPERATING_SYSTEM);
        v.drm.st().fail_import = false;
        v.drm.st().identify_as = Some(NV_GEM_OBJECT_NVKMS);
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(status(&r), NV_ERR_OPERATING_SYSTEM);
        assert_eq!(v.drm.st().closed.len(), 1, "the import is closed again");
        v.drm.st().identify_as = None;
        v.drm.st().exporter = Some("udmabuf".into());
        let r = v.export(c, 2 * MIB, 1, r0);
        assert_eq!(status(&r), NV_ERR_OPERATING_SYSTEM);
        v.drm.st().exporter = None;
        assert!(exported(&r).is_none());
        assert_eq!(le::i32_at(&r, BODY), Some(-1));
        assert!(v.be.rm_exports.is_empty());
        assert_eq!(rm::open_exports(), 0, "every dma-buf RM made is closed");
    }

    /// A reply that could not hold the export is refused before RM makes
    /// anything.
    #[test]
    fn no_export_is_made_that_the_reply_cannot_carry() {
        let mut v = vm(true);
        let c = v.client(1);
        let r0 = v.render(1);
        rm::seen();
        let b = block(c, &[2 * MIB]);
        let mut t = crate::sys::pod::bytes(&pid(1)).to_vec();
        t.extend_from_slice(crate::sys::pod::bytes(&DmabufExportReq {
            render: r0,
            reserved: 0,
        }));
        let mut req = Vec::new();
        for x in [
            MsgType::Ioctl as u32,
            v.gpu,
            0,
            0,
            EXPORT,
            WIDE as u32,
            0,
            0,
            0,
            0,
        ] {
            req.extend_from_slice(&x.to_le_bytes());
        }
        req.extend_from_slice(&b);
        req.extend_from_slice(&t);
        let mut resp = vec![0u8; 16 + 12 + WIDE];
        let n = v.be.dispatch(&req, &mut resp);
        assert_eq!(errno(&resp[..n]), -libc::ENOSPC);
        assert!(!reached());
        // Nor on the control file, where RM serves none.
        let ctl = v.ctl;
        let r = call(&mut v.be, ctl, EXPORT, &b, &t);
        assert_eq!(errno(&r), -libc::EINVAL);
        assert!(!reached());
    }

    /// Each process holds its share of the VM's exports; closing the
    /// object, or the file, gives it back.
    #[test]
    fn the_budget_is_per_process_and_given_back() {
        let mut v = vm(true);
        let c = v.client(1);
        let c2 = v.client(2);
        let r1 = v.render(1);
        let r2 = v.render(2);
        let mut gems = Vec::new();
        for _ in 0..EXPORTS_SHARE.per_owner {
            let r = v.export(c, 4096, 1, r1);
            assert_eq!(status(&r), NV_OK);
            gems.push(exported(&r).unwrap().gem);
        }
        rm::seen();
        let r = v.export(c, 4096, 1, r1);
        assert_eq!(status(&r), NV_ERR_INSUFFICIENT_RESOURCES);
        assert!(!reached(), "refused before RM is asked");
        let r = v.export(c2, 4096, 2, r2);
        assert_eq!(status(&r), NV_OK, "another process still has its own");
        // A GEM_CLOSE on the render file gives one back.
        let mut gc = [0u8; 8];
        le::put_u32(&mut gc, 0, gems[0]);
        let r = call(&mut v.be, r1, crate::hostfd::DRM_IOCTL_GEM_CLOSE, &gc, &[]);
        assert_eq!(errno(&r), 0);
        let r = v.export(c, 4096, 1, r1);
        assert_eq!(status(&r), NV_OK);
        // The file's close gives back the rest.
        v.be.close_handle(r1).unwrap();
        assert_eq!(v.be.rm_exports.len(), 1);
        // Bytes too: half the VM's to one process.
        let r3 = v.render(1);
        let r = v.export(c, rmexport::MAX_BYTES / 2 + 4096, 1, r3);
        assert_eq!(status(&r), NV_ERR_INSUFFICIENT_RESOURCES);
    }

    /// The switch marks the one exporter every path out asks about: the
    /// backend's own gate (HOST_OP PRIME_EXPORT, Wayland) and the IOCTL2
    /// hooks (a re-home into a KMS file), which share the mark. Tried with a
    /// udmabuf standing for RM's dma-buf.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no /dev/udmabuf")]
    fn every_path_out_asks_about_the_marked_exporter() {
        let be = NvidiaBackend::for_test();
        let Ok(dev) = crate::sys::fd::open_path("/dev/udmabuf", libc::O_RDWR) else {
            eprintln!("SKIPPED every_path_out_asks_about_the_marked_exporter: no /dev/udmabuf");
            return;
        };
        let memfd =
            crate::sys::fd::memfd(c"out", libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING).unwrap();
        crate::sys::fd::ftruncate(&memfd, 4096).unwrap();
        crate::sys::fd::add_seals(&memfd, libc::F_SEAL_SHRINK).unwrap();
        let d = crate::sys::ioctl::udmabuf_create(dev.as_fd(), memfd.as_fd(), 4096, 1).unwrap();
        assert!(be.hooks.exportable(d.as_fd()));
        assert_eq!(be.export_gate().may_leave(d.as_fd()), Ok(()));
        be.guest_only.set("udmabuf").unwrap();
        assert!(!be.hooks.exportable(d.as_fd()));
        assert_eq!(be.export_gate().may_leave(d.as_fd()), Err(libc::EINVAL));
        // And the switch marks RM's.
        let mut be = NvidiaBackend::for_test();
        be.set_config(BackendConfig {
            allow_dmabuf_export: true,
            ..BackendConfig::default()
        });
        assert_eq!(be.guest_only.get(), Some(&RM_EXPORTER));
        assert!(be.serves_dmabuf_export(NV_ESC_EXPORT_TO_DMABUF_FD));
        assert!(!NvidiaBackend::for_test().serves_dmabuf_export(NV_ESC_EXPORT_TO_DMABUF_FD));
    }
}
