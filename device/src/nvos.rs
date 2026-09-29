// SPDX-License-Identifier: Apache-2.0
//! RM's escape ABI, as the backend reads and writes it: where the fields of
//! the escapes' own parameter blocks are (nvos.h's NVOSxx_PARAMETERS), the
//! nv-ioctl.h wrappers that carry a descriptor after one, the few class
//! parameters the backend looks inside, and RM's statuses
//! (nvstatuscodes.h). One definition of each, for every module.
//!
//! The nvos.h offsets are measured, per release, from NVIDIA's sources by
//! gen/rmallow_extract.py (its `OS_BLOCKS`), which refuses to render a
//! release where one moved; they are re-exported here. What the generator
//! does not probe -- nv-ioctl.h is not in the SDK it compiles against, and
//! NVOS46 and the memory classes' parameters change size between releases
//! -- is written out below, with where it is from.

#![forbid(unsafe_code)]

pub use abi::rmallow::nvos::*;

// ─────────────────── nv-ioctl.h: a descriptor after the block ───────────────────

/// nv_ioctl_nvos02_parameters_with_fd (RM_ALLOC_MEMORY): NVOS02, then the
/// descriptor of the file a mapping of the memory is armed on.
pub const NVOS02_WITH_FD_SIZE: usize = 56;
pub const NVOS02_WITH_FD_FD: usize = 48;
/// nv_ioctl_nvos33_parameters_with_fd (RM_MAP_MEMORY): NVOS33, then the
/// descriptor the mapping is armed on.
pub const NVOS33_WITH_FD_SIZE: usize = 56;
pub const NVOS33_WITH_FD_FD: usize = 48;
/// nv_ioctl_{alloc,free}_os_event_t: `{hClient, hDevice, fd, Status}`.
pub const OS_EVENT_SIZE: usize = 16;
pub const OS_EVENT_H_CLIENT: usize = 0;
pub const OS_EVENT_FD: usize = 8;
pub const OS_EVENT_STATUS: usize = 12;
/// nv_ioctl_register_fd_t: the control file's descriptor alone.
pub const REGISTER_FD_FD: usize = 0;

// ─────────────────── Blocks whose size moves between releases ───────────────────

/// NVOS46 (RM_MAP_MEMORY_DMA): 56 bytes before 580, 64 since; these fields
/// are where they were.
pub const NVOS46_H_CLIENT: usize = 0;
pub const NVOS46_H_MEMORY: usize = 12;
pub const NVOS46_FLAGS: usize = 32;
/// NV_MEMORY_ALLOCATION_PARAMS (the parameters of the memory classes):
/// `attr` and `attr2`.
pub const NV_MEMORY_ALLOCATION_ATTR: usize = 24;
pub const NV_MEMORY_ALLOCATION_ATTR2: usize = 28;
/// NV_CONTEXT_DMA_ALLOCATION_PARAMS: `flags` and `hMemory`.
pub const NV_CONTEXT_DMA_ALLOCATION_FLAGS: usize = 4;
pub const NV_CONTEXT_DMA_ALLOCATION_H_MEMORY: usize = 8;

// ─────────────────── A control that runs another ───────────────────

/// NV5080_CTRL_CMD_DEFERRED_API and _V2: `{hApiHandle, cmd, flags,
/// hClientVA, hDeviceVA, union api_bundle}`, the bundle holding the
/// parameters of the control `cmd` names, which RM runs later at the
/// caller's privilege (deferred_api.c).
pub const DEFERRED_API_CONTROLS: [u32; 2] = [0x5080_0101, 0x5080_0103];
pub const DEFERRED_API_CMD: usize = 4;
pub const DEFERRED_API_H_CLIENT_VA: usize = 12;
pub const DEFERRED_API_BUNDLE: usize = 24;

// ─────────────────── Classes ───────────────────

/// NV01_ROOT, NV01_ROOT_NON_PRIV, NV01_ROOT_CLIENT: the classes a client
/// is allocated as (escape.c:473-481 turns all three into the last).
pub const NV01_ROOT: u32 = 0x00;
pub const NV01_ROOT_NON_PRIV: u32 = 0x01;
pub const NV01_ROOT_CLIENT: u32 = 0x41;
pub const ROOT_CLASSES: [u32; 3] = [NV01_ROOT, NV01_ROOT_NON_PRIV, NV01_ROOT_CLIENT];
pub const NV01_DEVICE_0: u32 = 0x80;
pub const NV20_SUBDEVICE_0: u32 = 0x2080;
/// Memory the caller already has, named by its CPU address.
pub const NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: u32 = 0x71;

// ─────────────────── Statuses (nvstatuscodes.h) ───────────────────
//
// A block's `status` is the answer that matters and the one that is easy to
// miss: RM writes it on the way out, independent of the ioctl's own return,
// and a caller believes it over the return. The backend answers the calls it
// turns away the same way, with one of these in the field and the ioctl
// succeeding (`with_status`).

/// Every other status is a refusal of some kind.
pub const NV_OK: u32 = 0;
pub const NV_ERR_ECC_ERROR: u32 = 0x0b;
pub const NV_ERR_GPU_IS_LOST: u32 = 0x0f;
pub const NV_ERR_INSUFFICIENT_RESOURCES: u32 = 0x1a;
pub const NV_ERR_INSUFFICIENT_PERMISSIONS: u32 = 0x1b;
pub const NV_ERR_INVALID_ARGUMENT: u32 = 0x1f;
pub const NV_ERR_INVALID_CLASS: u32 = 0x22;
pub const NV_ERR_INVALID_CLIENT: u32 = 0x23;
pub const NV_ERR_INVALID_OBJECT_HANDLE: u32 = 0x33;
pub const NV_ERR_INVALID_PARAM_STRUCT: u32 = 0x3a;
pub const NV_ERR_NO_MEMORY: u32 = 0x51;
pub const NV_ERR_NOT_SUPPORTED: u32 = 0x56;
pub const NV_ERR_RC_ERROR: u32 = 0x60;

/// `block` as it was sent, with RM's `status` at `status_at`: how RM
/// answers a call it turns away, the ioctl itself succeeding. A block too
/// short to hold the status goes back as it came.
pub fn with_status(block: &[u8], status_at: usize, status: u32) -> Vec<u8> {
    let mut out = block.to_vec();
    let _ = crate::le::put_u32(&mut out, status_at, status);
    out
}
