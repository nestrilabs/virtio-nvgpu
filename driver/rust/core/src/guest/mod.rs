//! The parsers, shared by the host crate and the kernel module.
//!
//! Every function here takes bytes that were copied out of the caller once
//! -- through [`i2::Store::fetch`] or [`rm::Env::copy_from_user`], each range
//! exactly once -- and returns typed values, a built host message, or an
//! error. None of them writes the caller's memory except through the copy-out
//! a trait method performs, with bytes this code chose. Errors are the
//! kernel's: a negative errno.

pub mod deep;
pub mod dispatch;
pub mod i2;
pub mod osdesc;
pub mod rm;
pub mod schema;
pub mod wire;
