// SPDX-License-Identifier: Apache-2.0
// device/src/lib.rs
//
// The virtio-nvgpu device, with no VMM in its dependency list (README.md).
//
// The backend holds the real host file descriptors for `/dev/nvidia*` and the
// DRM nodes and dispatches the messages the guest driver sends over the
// virtqueues; a VMM adopts it through the traits here, or runs the
// vhost-user backend binary (`bin/vhost-user-nvgpu.rs`).

// Every `unsafe` of the crate is in `sys` (sys/mod.rs); scripts/check-unsafe.sh
// holds the tree to that, and each other module forbids it outright.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod closer;
pub mod deepseg;
pub mod error;
pub mod exec;
pub mod exportgate;
pub mod fence;
#[cfg(test)]
mod fuzz_seeds;
#[cfg(fuzzing)]
pub mod fuzzing;
/// The guarded buffers, where the rest of the crate has always found them.
pub mod guarded {
    pub use crate::sys::guarded::GuardedBuf;
}
pub mod guestptr;
pub mod handle_table;
pub mod host;
pub mod hostfd;
#[cfg(test)]
mod i2_e2e;
pub mod inject;
pub mod kms;
pub mod le;
pub mod mmap;
pub mod nvidia;
pub mod nvkms;
pub mod nvos;
pub mod osdesc;
pub mod pacing;
pub mod policy;
pub mod posture;
pub mod privfd;
pub mod pump;
pub mod quota;
pub mod ratelimit;
pub mod release;
#[cfg(test)]
mod replay;
pub mod rmallow;
pub mod rmctl;
pub mod rmmem;
pub mod rmshare;
pub mod sandbox;
pub mod schema;
pub mod semsurf;
pub mod session;
pub mod shm;
#[allow(unsafe_code)]
pub mod sys;
pub mod tally;
#[cfg(test)]
mod testfd;
#[cfg(test)]
mod testing;
pub mod userspace;
pub mod uvmfd;
pub mod uvmmap;
pub mod virtio;
#[cfg(feature = "vhost-user")]
pub mod vring;
pub mod wl;
pub mod xfer;
