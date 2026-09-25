// crates/device/src/lib.rs
//
// VMM backend for virtio-gpu-nv.
//
// Integrates with libkrun's virtio device infrastructure.  The backend holds
// real host file descriptors for `/dev/nvidia*` and dispatches messages
// received from the guest driver over virtqueues.

pub mod error;
pub mod exec;
pub mod fence;
pub mod guarded;
pub mod handle_table;
pub mod host;
pub mod hostfd;
#[cfg(test)]
mod i2_e2e;
pub mod kms;
pub mod mmap;
pub mod nvidia;
pub mod nvkms;
pub mod policy;
pub mod privfd;
pub mod pump;
pub mod replay;
pub mod rmmem;
pub mod schema;
pub mod semsurf;
pub mod session;
pub mod shm;
pub mod userspace;
pub mod virtio;
pub mod wl;
pub mod xfer;
