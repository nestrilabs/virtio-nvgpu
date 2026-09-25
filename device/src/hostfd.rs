//! What a host descriptor is, and helper operations on them (protocol v2).
//!
//! Every descriptor the backend holds lives in the handle table under a
//! handle, and every handle has a kind. The kind decides which messages may
//! name it: a v1 IOCTL only reaches `Dev` and render handles, a KMS schema only
//! runs on `DrmCard`/`DrmLease`, a fence watch only on `SyncFile`. Anything the
//! guest cannot use is `Other`, and nothing can be done with it.
//!
//! Classification of a descriptor that arrives from outside (an ioctl's fd out,
//! a Wayland message) is by what the kernel says it is, never by what the guest
//! or the compositor claimed: `fstat` for DRM nodes (and the node must be an
//! nvidia-drm card node of our own GPU), `/proc/self/fd` for the anonymous
//! inodes.

use protocol::messages::DeviceKind;

/// The kind of a backend handle. Wire value in `HK_*` (protocol::messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HandleKind {
    /// One of the NVIDIA character devices, opened by the guest.
    Dev(DeviceKind),
    /// A host render node (`renderD*`), by its index in the DRI list.
    DriRender(u32),
    /// A host card node the backend opened for a guest file (compositor-VM).
    DrmCard(u32),
    /// Any other nvidia-drm card-node file of our GPU: a lease, the drm_fd of
    /// a lease device, a CREATE_LEASE result. By card index.
    DrmLease(u32),
    SyncFile,
    Syncobj,
    Dmabuf,
    Eventfd,
    Memfd,
    /// A connection to the host Wayland compositor.
    Wayland,
    /// Anything else. Held (so it can be closed) but never usable.
    Other,
}

impl HandleKind {
    /// The `HK_*` wire value.
    pub fn wire(self) -> u32 {
        use protocol::messages::*;
        match self {
            Self::Dev(_) => HK_DEV,
            Self::DriRender(_) => HK_DRI_RENDER,
            Self::DrmCard(_) => HK_DRM_CARD,
            Self::DrmLease(_) => HK_DRM_LEASE,
            Self::SyncFile => HK_SYNC_FILE,
            Self::Syncobj => HK_SYNCOBJ,
            Self::Dmabuf => HK_DMABUF,
            Self::Eventfd => HK_EVENTFD,
            Self::Memfd => HK_MEMFD,
            Self::Wayland => HK_WAYLAND,
            Self::Other => HK_OTHER,
        }
    }

    /// A host file KMS ioctls may run on.
    pub fn is_kms(self) -> bool {
        matches!(self, Self::DrmCard(_) | Self::DrmLease(_))
    }

    /// Bit for schema `kinds` masks: `1 << wire()`.
    pub fn mask_bit(self) -> u32 {
        1u32 << self.wire()
    }
}
