// SPDX-License-Identifier: Apache-2.0
//! Backend policy for IOCTL2: the decisions `xfer` leaves to its caller.
//!
//! `xfer` enforces what a schema can state -- lengths, pointers, descriptor
//! and GEM positions, framebuffer ownership, which properties carry fences --
//! and asks a [`Hooks`] object for everything that is a judgement about what a
//! guest may do with a host display: whether a syncobj wait may park a host
//! thread, which NVKMS commands and which heads a grant covers, whether a
//! fence property may be set. Three workstreams own those judgements (KMS,
//! FENCES, NVKMS), and each needs state that outlives a call (grant records,
//! eventfd registrations). They share one object, [`BackendHooks`], because
//! `xfer` asks one object at fixed points of every call and has no idea which
//! workstream an entry belongs to; the entry's `policy` bits say, and
//! [`BackendHooks::before`] routes on them.
//!
//! Each section (KMS, FENCES, NVKMS below) keeps to its own methods; the
//! backend builds the object with the state it shares with them
//! (`with_state`), and the state-less constructors exist for tests only.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::hostfd::HandleKind;
use crate::kms;
use crate::nvkms::NvkmsPolicy;
use crate::schema::policy;
use crate::semsurf::{self, SemsurfPolicy};
use crate::xfer::{Errno, Hooks, Prepared, PropKind, RunGuard};

/// The one policy object the backend hands every IOCTL2 (`Env::hooks`).
///
/// Shared between the queue thread (`before`, `after`, under the backend
/// mutex) and executor threads (`prop_kind`, `atomic_fence_prop`, during
/// `execute`, without it), so any state a section adds needs its own lock.
#[cfg_attr(test, derive(Default))]
pub struct BackendHooks {
    /// NVKMS: grant records, fresh files, the host's layout (nvkms.rs).
    /// Shared with the backend, which feeds it the host version and every
    /// handle it closes.
    nvkms: Arc<NvkmsPolicy>,
    /// FENCES: what 0x54 may name (the host's semaphore layout, the VM's
    /// RM clients, its live contexts; semsurf.rs). Shared with the backend,
    /// which feeds it from the RM path and every handle it opens and closes.
    semsurf: Arc<SemsurfPolicy>,
}

impl BackendHooks {
    /// With state of its own, shared with no backend: for tests.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// As the `Arc<dyn Hooks>` `xfer::Env::hooks` returns, for tests.
    #[cfg(test)]
    pub fn shared() -> Arc<dyn Hooks> {
        Arc::new(Self::new())
    }

    /// With the NVKMS and semaphore-surface state the backend also holds.
    pub fn with_state(nvkms: Arc<NvkmsPolicy>, semsurf: Arc<SemsurfPolicy>) -> Arc<dyn Hooks> {
        Arc::new(Self { nvkms, semsurf })
    }

    // ───────────────────────────── KMS ─────────────────────────────
    //
    // Framebuffer tracking (FB_CREATE/FB_REMOVE/FB_READ: GETFB/GETFB2 hand
    // back GEM handles only for the file's own framebuffers, per handle in
    // `xfer::KmsFileState`, RV:getfb), ADDFB2's unused planes (FB_PLANES) and
    // the refusal of fence and pointer properties on the legacy setters
    // (SETPROP, RV:setprop) are enforced by `xfer` itself from the entry's
    // bits. What is left to decide here: what counts as a fence or pointer
    // property (`kms::prop_kind`), and who may hold host DRM master.
    //
    // nvidia-drm's GRANT/REVOKE_PERMISSIONS are KMS-class too, but their
    // policy (MODESET only; revocations of dpys this handle was granted) is
    // the NVKMS section's, in `nvkms_before` below.

    /// What a property's value is, by its name: the known fence and pointer
    /// properties, and anything named like one.
    fn kms_prop_kind(&self, name: &[u8]) -> PropKind {
        kms::prop_kind(name)
    }

    fn kms_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        if p.policy() & policy::MASTER != 0 {
            self.kms_master_before(p)?;
        }
        Ok(())
    }

    /// SET_MASTER / DROP_MASTER: only on a card file the backend opened for
    /// the guest (HOST_OP OPEN_KMS, `--kms-card` only). The guest's own core
    /// has arbitrated before its hooks send either (nvgpu_kms.c); the host
    /// check alone would not do, since every host file is this process's and
    /// passes it once it was ever master (drm_auth.c:232-243). A lessee can
    /// hold neither (drm_auth.c:268, 301), and a lease is the host
    /// compositor's to arbitrate, not the guest's.
    fn kms_master_before(&self, p: &Prepared) -> Result<(), Errno> {
        match p.target_kind() {
            HandleKind::DrmCard(_) => Ok(()),
            k => {
                log::warn!(
                    "IOCTL2 {} on handle {} ({k:?}): master calls are for host cards only",
                    p.name(),
                    p.target()
                );
                Err(libc::EPERM)
            }
        }
    }

    // ──────────────────────────── FENCES ────────────────────────────
    //
    // Syncobj and nvidia-drm fence calls (policy::FENCE) and ATOMIC's fence
    // properties. Fence objects are the host's and the guest holds proxies
    // (DESIGN §6), so creating, exporting, importing and signalling them is
    // forwarded as it is. A wait is not: forwarded as it stands it would park
    // a host thread for as long as the guest asked, so every wait becomes a
    // poll and the guest sleeps on a shared eventfd registration instead
    // (`fence::before`, `fence::Registrations`); the registrations are
    // HOST_OP state, kept by the backend per session. The one call whose
    // arguments reach host kernel memory unchecked, SEMSURF_FENCE_CTX_CREATE,
    // is bounded, owned and counted by `semsurf` first.

    fn fences_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let cmd = p.cmd();
        if cmd == semsurf::SEMSURF_FENCE_CTX_CREATE {
            return self.semsurf.ctx_create_before(p);
        }
        let mut arg = p.buffer_mut(0).ok_or(libc::EINVAL)?;
        crate::fence::before(cmd, &mut arg)
    }

    /// A fence context made is counted against its file's cap until closed.
    fn fences_after(&self, p: &mut Prepared, ret: i32) {
        if p.cmd() == semsurf::SEMSURF_FENCE_CTX_CREATE {
            self.semsurf.ctx_create_after(p, ret);
        }
    }

    /// IN_FENCE_FD, OUT_FENCE_PTR and their kin on an ATOMIC commit. `xfer`
    /// does the translation -- the guest's handle becomes the host sync_file
    /// it stands for, an out-fence pointer a host s32 whose descriptor is
    /// adopted -- and refuses a value without its record; neither waits on
    /// anything (the commit's own fence waits are the display engine's,
    /// nvidia-drm-modeset.c:160-315), so each is allowed.
    fn fences_atomic_prop(&self, p: &Prepared, kind: PropKind, name: &[u8]) -> Result<(), Errno> {
        let _ = (p, kind, name);
        Ok(())
    }

    // ──────────────────────────── NVKMS ────────────────────────────
    //
    // NVKMS commands (policy::NVKMS) and nvidia-drm's GRANT/REVOKE
    // permissions (policy::GRANT, policy::REVOKE), decided by nvkms.rs:
    // commands refused by name or outside --kms-card, heads and dpys only
    // as granted, overrides scrubbed, grant files fresh, MODESET grants only
    // and revocations only through the granting handle.

    fn nvkms_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        self.nvkms.before(p)
    }

    fn nvkms_after(&self, p: &mut Prepared, ret: i32) {
        self.nvkms.after(p, ret);
    }
}

impl Hooks for BackendHooks {
    fn prop_kind(&self, name: &[u8]) -> PropKind {
        self.kms_prop_kind(name)
    }

    fn atomic_fence_prop(&self, p: &Prepared, kind: PropKind, name: &[u8]) -> Result<(), Errno> {
        self.fences_atomic_prop(p, kind, name)
    }

    /// Routed on the entry's policy bits. An entry carries one workstream's
    /// bits (the generator never mixes FENCE with GRANT/REVOKE/NVKMS), and
    /// fences go first so a mixed one would still be refused.
    fn before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let pol = p.policy();
        if pol & policy::FENCE != 0 {
            self.fences_before(p)?;
        }
        if pol & (policy::GRANT | policy::REVOKE | policy::NVKMS) != 0 {
            self.nvkms_before(p)?;
        }
        const KMS: u32 = policy::FB_CREATE
            | policy::FB_REMOVE
            | policy::FB_READ
            | policy::SETPROP
            | policy::FB_PLANES
            | policy::MASTER;
        if pol & KMS != 0 {
            self.kms_before(p)?;
        }
        Ok(())
    }

    fn after(&self, p: &mut Prepared, ret: i32) {
        if p.policy() & policy::FENCE != 0 {
            self.fences_after(p, ret);
        }
        if p.policy() & (policy::GRANT | policy::REVOKE | policy::NVKMS) != 0 {
            self.nvkms_after(p, ret);
        }
    }

    /// Only the NVKMS section gives calls a run gate (its head and dpy
    /// gates, re-checked where the call runs: S-14).
    fn at_run<'a>(&'a self, p: &Prepared) -> Result<Option<RunGuard<'a>>, Errno> {
        if p.policy() & policy::NVKMS != 0 {
            return self.nvkms.at_run(p);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::SchemaClass;
    use crate::xfer;
    use std::os::fd::OwnedFd;

    const RENDER: u32 = 20;

    /// Just enough backend for `xfer::prepare` to reach the hooks: one render
    /// handle, and this module's policy.
    struct Env;

    impl xfer::Env for Env {
        fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)> {
            (handle == RENDER).then(|| {
                let fd: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
                (fd, HandleKind::DriRender(0))
            })
        }

        fn kind(&self, handle: u32) -> Option<HandleKind> {
            (handle == RENDER).then_some(HandleKind::DriRender(0))
        }

        fn nvkms_version(&self) -> Option<abi::version::DriverVersion> {
            None
        }

        fn hooks(&self) -> Arc<dyn Hooks> {
            BackendHooks::shared()
        }
    }

    /// An IOCTL2 payload with one IN/OUT argument and no pointers followed.
    fn flat(cmd: u32, arg: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        let data_len = arg.len().next_multiple_of(8) as u32;
        for v in [cmd, 0, 1, 0, 0, 0, data_len, RENDER, arg.len() as u32] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p.extend_from_slice(arg);
        p.resize(p.len() + data_len as usize - arg.len(), 0);
        p
    }

    fn prepare(cmd: u32, arg: &[u8]) -> Result<Prepared, Errno> {
        xfer::prepare(
            &Env,
            SchemaClass::Render,
            RENDER,
            HandleKind::DriRender(0),
            &flat(cmd, arg),
        )
    }

    #[test]
    fn a_syncobj_wait_reaches_the_host_as_a_poll() {
        // count_handles 0, so the handles pointer takes no buffer.
        let mut a = [0u8; 40];
        a[8..16].copy_from_slice(&i64::MAX.to_le_bytes());
        let p = prepare(crate::fence::SYNCOBJ_WAIT, &a).unwrap();
        assert_eq!(&p.buffer(0).unwrap()[8..16], &[0; 8]);
    }

    #[test]
    fn fence_calls_that_do_not_wait_are_forwarded() {
        let create = crate::hostfd::ioc(crate::hostfd::IOC_RW, b'd', 0xbf, 8);
        assert!(prepare(create, &[0; 8]).is_ok());
        let transfer = [0u8; 32];
        assert!(prepare(crate::fence::SYNCOBJ_TRANSFER, &transfer).is_ok());
    }

    #[test]
    fn fence_calls_that_would_wait_or_leak_are_refused() {
        let mut transfer = [0u8; 32];
        transfer[24] = crate::fence::WAIT_FOR_SUBMIT as u8;
        assert_eq!(
            prepare(crate::fence::SYNCOBJ_TRANSFER, &transfer).err(),
            Some(libc::EINVAL)
        );
        // SYNCOBJ_EVENTFD: an fd field, "none" here, so it gets as far as the
        // policy -- which never lets it through.
        let mut eventfd = [0u8; 24];
        eventfd[16..20].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            prepare(crate::fence::SYNCOBJ_EVENTFD, &eventfd).err(),
            Some(libc::EPERM)
        );
    }

    #[test]
    fn atomic_fence_properties_are_allowed_now_that_fences_are_served() {
        let h = BackendHooks::new();
        let create = crate::hostfd::ioc(crate::hostfd::IOC_RW, b'd', 0xbf, 8);
        let p = prepare(create, &[0; 8]).unwrap();
        for (kind, name) in [
            (PropKind::FenceFd, &b"IN_FENCE_FD"[..]),
            (PropKind::OutPtr, b"OUT_FENCE_PTR"),
        ] {
            assert_eq!(h.atomic_fence_prop(&p, kind, name), Ok(()));
        }
    }

    #[test]
    fn known_properties_are_classified_as_the_defaults_classify_them() {
        let h = BackendHooks::new();
        for name in [
            &b"IN_FENCE_FD"[..],
            b"OUT_FENCE_PTR",
            b"WRITEBACK_OUT_FENCE_PTR",
            b"NV_DRM_OUT_FENCE_PTR",
            b"CRTC_ID",
        ] {
            assert_eq!(h.prop_kind(name), xfer::default_prop_kind(name));
        }
    }

    #[test]
    fn a_property_named_like_a_pointer_or_descriptor_is_never_plain() {
        let h = BackendHooks::new();
        assert_eq!(h.prop_kind(b"VENDOR_FUTURE_PTR"), PropKind::OutPtr);
        assert_eq!(h.prop_kind(b"VENDOR_FUTURE_FD"), PropKind::FenceFd);
        assert_eq!(h.prop_kind(b"NV_PLANE_DEGAMMA_LUT"), PropKind::Plain);
    }
}
