//! Runtime types for the ioctl schemas both halves share (protocol v2).
//!
//! The tables themselves are generated from `gen/schema/` into
//! `gen/src/schema/generated.rs` (Rust) and `driver/gen/nvgpu_schema.h` (C) by
//! `gen/schema_gen.py`, and their types live beside them in the `abi` crate
//! (`abi::schema`). This module ties them to the backend: which handle kinds
//! a class runs on, and whether a handle may stand in a descriptor field. The
//! interpreter that walks an entry over a request is `xfer.rs`.

#![forbid(unsafe_code)]

use protocol::messages::DeviceKind;

use crate::hostfd::HandleKind;

pub use abi::schema::*;

/// Which kind of host file a call targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaClass {
    /// A host render node: nvidia-drm GEM/fence ioctls, syncobj, core GEM.
    Render,
    /// A host card node or lease: KMS.
    Kms,
    /// `/dev/nvidia-modeset`: NVKMS, keyed by the command inside the outer struct.
    Modeset,
}

impl SchemaClass {
    /// The generated tables' spelling of the same class.
    pub fn table_class(self) -> Class {
        match self {
            Self::Render => Class::Render,
            Self::Kms => Class::Kms,
            Self::Modeset => Class::Modeset,
        }
    }

    /// Whether a handle of `kind` is the kind of host file this class runs
    /// on. A render schema on a card, or a KMS schema on a render node, is a
    /// request the guest should never build, and nothing about it is checked
    /// against the right table.
    pub fn runs_on(self, kind: HandleKind) -> bool {
        match self {
            Self::Render => matches!(kind, HandleKind::DriRender(_)),
            Self::Kms => kind.is_kms(),
            Self::Modeset => kind == HandleKind::Dev(DeviceKind::Modeset),
        }
    }
}

/// Whether a handle of `kind` may stand in a descriptor field that allows
/// `kinds` (an `FdIn` mask: bit n for `HK_*` n, `HK_DEV`'s bit for any of our
/// devices, and the `K_DEV_*` bits for one device each).
pub fn kind_allowed(kinds: u32, kind: HandleKind) -> bool {
    if kinds & kind.mask_bit() != 0 {
        return true;
    }
    let dev_bit = match kind {
        HandleKind::Dev(DeviceKind::Ctl) => K_DEV_CTL,
        HandleKind::Dev(DeviceKind::Modeset) => K_DEV_MODESET,
        HandleKind::Dev(DeviceKind::Gpu(_)) => K_DEV_GPU,
        _ => 0,
    };
    kinds & dev_bit != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_bit_admits_that_device_and_no_other() {
        let ctl = HandleKind::Dev(DeviceKind::Ctl);
        let modeset = HandleKind::Dev(DeviceKind::Modeset);
        assert!(kind_allowed(K_DEV_CTL, ctl));
        assert!(!kind_allowed(K_DEV_CTL, modeset));
        assert!(!kind_allowed(K_DEV_CTL, HandleKind::SyncFile));
    }

    #[test]
    fn the_any_device_bit_admits_every_device() {
        let any = HandleKind::Dev(DeviceKind::Uvm).mask_bit();
        assert!(kind_allowed(any, HandleKind::Dev(DeviceKind::Modeset)));
        assert!(!kind_allowed(any, HandleKind::Dmabuf));
    }

    #[test]
    fn a_class_runs_only_on_its_own_kind_of_file() {
        assert!(SchemaClass::Kms.runs_on(HandleKind::DrmLease(0)));
        assert!(!SchemaClass::Kms.runs_on(HandleKind::DriRender(0)));
        assert!(SchemaClass::Render.runs_on(HandleKind::DriRender(1)));
        assert!(SchemaClass::Modeset.runs_on(HandleKind::Dev(DeviceKind::Modeset)));
        assert!(!SchemaClass::Modeset.runs_on(HandleKind::Dev(DeviceKind::Ctl)));
    }
}
