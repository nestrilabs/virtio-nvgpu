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
    // Framebuffer tracking (FB_CREATE/FB_REMOVE/FB_READ), ADDFB2's unused
    // planes (FB_PLANES) and the legacy property setters (SETPROP) are
    // enforced by `xfer` itself from the entry's bits; nothing is left to
    // decide here yet.

    /// What a property's value is, by its name.
    fn kms_prop_kind(&self, name: &[u8]) -> PropKind {
        xfer::default_prop_kind(name)
    }

    fn kms_before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let _ = p;
        Ok(())
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
            | policy::FB_PLANES;
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
    fn properties_are_classified_as_the_defaults_classify_them() {
        let h = BackendHooks::new();
        for name in [
            &b"IN_FENCE_FD"[..],
            b"OUT_FENCE_PTR",
            b"NV_DRM_OUT_FENCE_PTR",
            b"CRTC_ID",
        ] {
            assert_eq!(h.prop_kind(name), xfer::default_prop_kind(name));
        }
    }
}
