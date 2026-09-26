// SPDX-License-Identifier: Apache-2.0
//! The allowlist at run time: which advertised globals a guest sees, and at
//! what version.
//!
//! Enforced on the host, which is the only side that can be trusted to (the
//! guest daemon applies the same filter, but a guest kernel is free not to run
//! it). An interface is offered only when it is in `GLOBALS`, its requirement
//! is met, and at min(what the compositor advertised, the vendored XML's
//! version, the table's cap). A bind of anything else is a protocol error.

#![forbid(unsafe_code)]

use std::sync::Arc;

use crate::policy_table::{GLOBALS, Requires};
use crate::proto::{IfaceId, iface, iface_by_name};

/// Whether a particular lease-device global (by registry name) is one of our
/// GPU's. The host answers it by probing the compositor (the global itself
/// does not say which GPU it is for).
pub type LeaseCheck = Arc<dyn Fn(u32) -> bool + Send + Sync>;

#[derive(Clone)]
pub enum LeaseGate {
    Deny,
    Allow,
    Check(LeaseCheck),
}

#[derive(Clone)]
pub struct Policy {
    /// DRM files can be adopted where they are going (the guest said so in its
    /// HELLO, or on the guest side: the kernel can and a card node exists).
    pub drm_file: bool,
    pub lease: LeaseGate,
    /// Syncobjs can be carried (explicit sync): on the host, the backend
    /// serves fences and the guest's HELLO says `HELLO_G_SYNCOBJ`; on the
    /// guest, its kernel said it can name host syncobjs.
    pub fences: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            drm_file: false,
            lease: LeaseGate::Deny,
            fences: false,
        }
    }
}

impl Policy {
    /// The version to offer for global `name` of `iface_name` that the
    /// compositor advertised at `host_version`, or `None` to hide it.
    pub fn offer(&self, name: u32, iface_name: &[u8], host_version: u32) -> Option<(IfaceId, u32)> {
        let id = iface_by_name(iface_name)?;
        let spec = GLOBALS
            .iter()
            .find(|g| g.interface.as_bytes() == iface_name)?;
        let ok = match spec.requires {
            Requires::Nothing => true,
            Requires::Fences => self.fences,
            Requires::DrmFile => {
                self.drm_file
                    && match &self.lease {
                        LeaseGate::Deny => false,
                        LeaseGate::Allow => true,
                        LeaseGate::Check(f) => f(name),
                    }
            }
        };
        if !ok || host_version == 0 {
            return None;
        }
        let mut v = host_version.min(iface(id).version);
        if spec.max_version != 0 {
            v = v.min(spec.max_version);
        }
        Some((id, v))
    }
}

/// Whether `iface_name` is on the allowlist at all (ignoring requirements).
pub fn listed(iface_name: &str) -> bool {
    GLOBALS.iter().any(|g| g.interface == iface_name)
}
