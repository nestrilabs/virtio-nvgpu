//! The differential test: the guest module's C parsers (`nvgpu_i2.c`,
//! `nvgpu_rmio.c`, compiled as they are against a userspace shim) and their
//! Rust port (`driver/rust/core`) run on the same inputs, in the same world,
//! and must do exactly the same: the same result, the same messages to the
//! backend byte for byte, the same bytes left in the caller's memory, the
//! same hooks called with the same arguments, the same handles closed, pages
//! pinned and kept, and lines logged.
//!
//! The world (`world.rs`) is the caller's memory and descriptor table, a fake
//! backend whose replies are a function of the request (`backend.rs`), and
//! the hooks (`hooks.rs`). Scenarios (`scen.rs`) are generated from a seed.

#![allow(clippy::missing_safety_doc)]

pub mod backend;
pub mod cabi;
pub mod hooks;
pub mod renv;
pub mod scen;
pub mod world;
