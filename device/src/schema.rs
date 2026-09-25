//! Runtime types for the ioctl schemas both halves share (protocol v2).
//!
//! The tables themselves are generated from `gen/schema/` into
//! `gen/src/schema/generated.rs` (Rust) and `driver/gen/nvgpu_schema.h` (C) by
//! `gen/schema_gen.py`. This module defines what a table entry *means*; the
//! interpreter that walks one over a request is `xfer.rs`.
//!
//! Workstream SCHEMA owns this file. The definitions below are the agreed
//! shape; extend them, but keep the names other modules use.

/// Which kind of host file a call targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SchemaClass {
    /// A host render node: nvidia-drm GEM/fence ioctls, syncobj, core GEM.
    Render,
    /// A host card node or lease: KMS.
    Kms,
    /// `/dev/nvidia-modeset`: NVKMS, keyed by the command inside the outer struct.
    Modeset,
}
