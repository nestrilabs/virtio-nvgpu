// SPDX-License-Identifier: GPL-2.0-only
//! The guest module's untrusted-input parsers, as a host crate.
//!
//! Everything in [`guest`] is also compiled into the guest kernel module
//! (`driver/nvgpu_rs.rs` includes the same sources with `#[path]`), where it
//! is the code that reads what a guest process hands the module: IOCTL2's
//! schema walk and the host message built from it, RM escapes' parameter
//! blocks and what their pointers reach, deep segments, and the OS-descriptor
//! registrations. It has no kernel dependency and no `unsafe`: it reaches the
//! caller's memory, the transport and the module's own hooks only through the
//! traits it defines, which the kernel implements (`driver/nvgpu_rs.rs`) and
//! the tests fake.
//!
//! Built here as an ordinary crate for its unit tests, the differential test
//! against the C it replaces (`driver/rust/difftest`), and fuzzing.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod guest;
