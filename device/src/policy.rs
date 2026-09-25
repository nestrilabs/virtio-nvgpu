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
//! Every section below starts out doing exactly what `xfer::DefaultHooks` does
//! -- refuse fences, allow only MODESET grants and revocations, pass NVKMS
//! commands that have a table entry -- so installing this object changes
//! nothing until a section is filled in. A section extends its own methods and
//! nothing else.

use std::sync::Arc;

use crate::hostfd::HandleKind;
use crate::kms;
use crate::schema::policy;
use crate::xfer::{self, Errno, Hooks, Prepared, PropKind};

/// The one policy object the backend hands every IOCTL2 (`Env::hooks`).
///
/// Shared between the queue thread (`before`, `after`, under the backend
/// mutex) and executor threads (`prop_kind`, `atomic_fence_prop`, during
/// `execute`, without it), so any state a section adds needs its own lock.
#[derive(Debug, Default)]
pub struct BackendHooks {}

impl BackendHooks {
    pub fn new() -> Self {
        Self::default()
    }

    /// As the `Arc<dyn Hooks>` `xfer::Env::hooks` returns.
    pub fn shared() -> Arc<dyn Hooks> {
        Arc::new(Self::new())
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
    // properties. Until fences are served, every one is refused: a syncobj
    // wait forwarded as it stands would park a host thread for as long as
    // the guest asked (DESIGN §6).

    fn fences_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let _ = p;
        Err(libc::EOPNOTSUPP)
    }

    fn fences_atomic_prop(&self, p: &Prepared, kind: PropKind, name: &[u8]) -> Result<(), Errno> {
        let _ = (p, kind, name);
        Err(libc::EOPNOTSUPP)
    }

    // ──────────────────────────── NVKMS ────────────────────────────
    //
    // NVKMS commands (policy::NVKMS) and nvidia-drm's GRANT/REVOKE
    // permissions (policy::GRANT, policy::REVOKE): MODESET grants only, and
    // every NVKMS command with a table entry, as `xfer::default_before`
    // decides. Grant records per handle, the commands refused outright and
    // the fresh-fd check come with the NVKMS tables.

    fn nvkms_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        xfer::default_before(p)
    }

    fn nvkms_after(&self, p: &mut Prepared, ret: i32) {
        let _ = (p, ret);
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
        if p.policy() & (policy::GRANT | policy::REVOKE | policy::NVKMS) != 0 {
            self.nvkms_after(p, ret);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
