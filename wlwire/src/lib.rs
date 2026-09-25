//! The Wayland proxy's shared half: the codec generated from protocol XML, the
//! allowlist and descriptor policy it is checked against at build time, the
//! frame format of the guest↔host channel, and the translation engine both
//! ends run.
//!
//! See `frame.rs` for the byte-level channel format, `policy_table.rs` for
//! what is allowed and why, and `engine.rs` for what happens to a message.

pub mod blob;
pub mod closure;
pub mod engine;
pub mod frame;
pub mod localout;
pub mod objects;
pub mod policy;
pub mod policy_table;
pub mod proto;
pub mod shm;
pub mod stream;
pub mod sys;
pub mod wire;

#[cfg(test)]
mod tests;
