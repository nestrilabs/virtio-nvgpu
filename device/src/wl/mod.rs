// SPDX-License-Identifier: Apache-2.0
//! The host side of the Wayland proxy (protocol v2, ARCHITECTURE.md §14).
//!
//! A guest client's connection is one backend handle of kind `Wayland`: a
//! [`WlConn`], which owns one connection to the host compositor and runs the
//! shared translation engine (`wlwire::engine`) facing it. The guest daemon
//! runs the same engine facing the client; between them travel frames
//! (`wlwire::frame`) in WL_SEND and WL_RECV.
//!
//! This is the side that enforces anything. The guest is untrusted, so the
//! allowlist, version clamps, object and opcode checks, descriptor counts and
//! every length are applied here to what the guest sends, whatever its daemon
//! did already. The only descriptors this side ever hands the compositor are
//! ones it made itself (memfds, pipes) or exported from a guest file's own
//! host GEM object.
//!
//! Integration (the dispatcher's side is `serve.rs`, over the handle table):
//!
//! - `OPEN(DEV_WAYLAND, flags = WL_OPEN_CONNECT)` → [`WlConn::open`]; insert
//!   the connection under a `HandleKind::Wayland` handle and watch the eventfd
//!   it returns the legacy way (EV_READY with the handle as cookie; never a
//!   W_READY watch, which the guest module does not send, `serve.rs`) -- it
//!   is readable while the connection has something for the guest.
//! - `WL_SEND` → [`WlConn::send`] with a [`SendOps`] that PRIME-exports on a
//!   guest file's render handle; reply `WlSendResp`.
//! - `WL_RECV` → [`WlConn::recv`] with a [`RecvOps`] that adopts descriptors
//!   into the handle table; the reply payload is the frame.
//! - `CLOSE` → drop the `WlConn`.
//! - `--wayland-export PATH`: [`export::WlExport::bind`] once at startup;
//!   `OPEN(DEV_WAYLAND, WL_OPEN_LISTEN)` → a handle whose readiness is the
//!   listener's eventfd; `OPEN(DEV_WAYLAND, WL_OPEN_ACCEPT)` →
//!   [`WlConn::from_export`] on [`export::WlExport::accept_pending`].
//!
//! Every method may be called with the backend mutex held: the reader thread
//! never takes it, and the ops traits are called only from the caller's thread.

#![forbid(unsafe_code)]

pub mod conn;
pub mod export;
pub mod probe;
pub mod serve;

#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod tests;

pub use conn::{
    HostFds, LeaseRefusal, LeaseThrottle, QueueBudget, RecvOps, SendOps, WlConfig, WlConn, WlLimits,
};
pub use serve::WlState;
