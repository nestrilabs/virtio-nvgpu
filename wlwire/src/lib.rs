// SPDX-License-Identifier: Apache-2.0
//! The Wayland proxy's shared half: the codec generated from protocol XML, the
//! allowlist and descriptor policy it is checked against at build time, the
//! frame format of the guest↔host channel, and the translation engine both
//! ends run.
//!
//! See `frame.rs` for the byte-level channel format, `policy_table.rs` for
//! what is allowed and why, and `engine.rs` for what happens to a message.

// Every `unsafe` of the crate is in `sys` (sys.rs); every other module
// forbids it, and scripts/check-unsafe.sh holds the tree to that.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod blob;
pub mod budget;
pub mod closure;
pub mod engine;
pub mod frame;
pub mod job;
pub mod localin;
pub mod localout;
pub mod objects;
pub mod policy;
pub mod policy_table;
pub mod proto;
pub mod shm;
pub mod stream;
#[allow(unsafe_code)]
pub mod sys;
pub mod wire;

#[cfg(test)]
mod tests;
