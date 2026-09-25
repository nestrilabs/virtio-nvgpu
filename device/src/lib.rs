// crates/device/src/lib.rs
//
// VMM backend for virtio-gpu-nv.
//
// Integrates with libkrun's virtio device infrastructure.  The backend holds
// real host file descriptors for `/dev/nvidia*` and dispatches messages
// received from the guest driver over virtqueues.

pub mod error;
pub mod exec;
pub mod guarded;
pub mod handle_table;
pub mod host;
pub mod hostfd;
pub mod mmap;
pub mod nvidia;
pub mod pump;
pub mod replay;
pub mod schema;
pub mod session;
pub mod shm;
pub mod userspace;
pub mod virtio;
pub mod xfer;
