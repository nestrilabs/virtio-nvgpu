//! No guest pointer reaches the host kernel as a pointer.
//!
//! The backend makes every forwarded ioctl itself, so to RM, NVKMS, nvidia-drm
//! and nvidia-uvm every user pointer in a parameter block is an address in
//! *this* process. A guest pointer left in one names whatever the backend
//! happens to have mapped there -- other guests' buffers, the window, the heap
//! -- and the host copies in from it and out to it on the guest's say-so:
//! an arbitrary read and write of the VMM. Every forwarding path therefore
//! either gives each pointer field an address of a buffer the backend owns
//! (the nested and deep blocks, `nvidia.rs` `dispatch_nested`) or writes 0
//! there, and restores what the caller had in the reply. What cannot be made
//! safe that way is refused before it reaches the host.
//!
//! This module holds the knowledge that takes: which fields of which
//! parameter blocks the host dereferences, measured from the host's own
//! sources.
//!
//! - **RM escapes** (`rm_escape`): the top-level blocks. Six escapes carry a
//!   pointer the backend has no way to relocate (IOCTL_XFER_CMD's whole
//!   argument, I2C_ACCESS, IDLE_CHANNELS' three handle arrays,
//!   ACCESS_REGISTRY's three strings, GET_EVENT_DATA's event record, and
//!   ADD_VBLANK_CALLBACK's function pointer) and are refused. RM_ALLOC's
//!   pRightsRequested is zeroed (RM then grants the default rights, which a
//!   caller could have asked for anyway); ALLOC_MEMORY's pMemory,
//!   MAP_MEMORY's pLinearAddress and VID_HEAP_CONTROL's `address` outputs are
//!   zeroed on the way in (RM only writes them, except for OS descriptors);
//!   HW_ALLOC's two opaque pointers are zeroed and restored. Classes whose
//!   allocation parameters hold pointers RM follows or calls are refused; see
//!   [`REFUSED_ALLOC_CLASSES`].
//! - **Memory named by CPU address** (`rm_escape`, the same gate):
//!   NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (0x71) through RM_ALLOC and
//!   ALLOC_MEMORY (escape.c:407-408, RmAllocOsDescriptor), and
//!   VID_HEAP_CONTROL's ALLOC_OS_DESCRIPTOR (escape.c:544-545,
//!   RmCreateOsDescriptor), hand RM an address for it to pin with
//!   `os_lock_user_pages` (escape.c:134-203) and map for the GPU -- or, as an
//!   OS_FILE_HANDLE descriptor, a dma-buf by descriptor number
//!   (osmemdesc.c:1017-1060). In the backend both name the VMM's memory and
//!   files, so the GPU would read and write whatever the backend has there.
//!   Refused with EPERM. This is how cuMemHostRegister and
//!   VK_EXT_external_memory_host register existing memory, which is therefore
//!   unsupported; ARCHITECTURE.md §5 sketches how it could be done.
//! - **RM controls** (`scrub_control`): RM follows pointers inside a
//!   control's parameters for the commands in [`CONTROL_POINTERS`], and only
//!   those (embedded_param_copy.c, rmapi_deprecated_control.c, and the
//!   handlers that copy from user themselves, mem_mgr_ctrl.c:617 and
//!   kernel_sm_debugger_session_ctrl.c:156). The guest can relocate one of
//!   them (the deep block); every other pointer field of the command is
//!   zeroed, which RM answers as a missing buffer. Two commands carry
//!   pointers the table cannot name one by one (a union selected by a type
//!   field, an array of per-op pointers) and are refused.
//! - **UVM** (`uvm_gate`): nvidia-uvm works on the calling process's address
//!   space, which is the backend's. Pageable memory access is forced off in
//!   UVM_INITIALIZE (so neither HMM nor ATS can let the GPU fault in the
//!   VMM's pages), and only commands that name UVM's own ranges, RM handles or
//!   GPU state go through; the tools device and every command that copies
//!   to or from a CPU buffer, pins one, or populates pages of the backend's
//!   own address space are refused.

use abi::ioctl::*;

use crate::hostfd;

/// A refusal: the errno the guest's ioctl returns.
pub type Errno = i32;

/// Fields of a top-level block the backend changed and the caller must read
/// back as it wrote them: `(offset, caller's bytes)`.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Restore(Vec<(usize, [u8; 8])>);

impl Restore {
    /// Put the caller's values back into `reply`, the block as the host left
    /// it.
    pub(crate) fn apply(&self, reply: &mut [u8]) {
        for (off, v) in &self.0 {
            if let Some(s) = reply.get_mut(*off..off + 8) {
                s.copy_from_slice(v);
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Zero the u64 at `off`, returning the caller's bytes if there was anything
/// but zero there.
fn take(b: &mut [u8], off: usize) -> Option<[u8; 8]> {
    let s = b.get_mut(off..off + 8)?;
    let old: [u8; 8] = s.try_into().ok()?;
    if old == [0; 8] {
        return None;
    }
    s.fill(0);
    Some(old)
}

// ───────────────────────────── RM escapes ─────────────────────────────

/// NVOS64 (nvos.h:476-490; 535 through 610 alike).
const OS64_SIZE: usize = 48;
const OS64_CLASS: usize = 12;
const OS64_RIGHTS: usize = 24;
/// NVOS54.
const OS54_SIZE: usize = 32;
const OS54_CMD: usize = 8;
/// nv_ioctl_nvos02_parameters_with_fd: NVOS02 and the fd after it.
const OS02_FD_SIZE: usize = 56;
const OS02_CLASS: usize = 12;
const OS02_MEMORY: usize = 24;
/// nv_ioctl_nvos33_parameters_with_fd.
const OS33_FD_SIZE: usize = 56;
const OS33_LINEAR: usize = 32;
/// NVOS32 (nvos.h:665-881).
const OS32_SIZE: usize = 184;
const OS32_FUNCTION: usize = 8;
/// `data.AllocSize.address`, `data.AllocTiledPitchHeight.address`: OUT.
const OS32_ALLOC_ADDRESS: usize = 120;
/// `data.AllocSizeRange.address`: OUT.
const OS32_RANGE_ADDRESS: usize = 128;
/// `data.HwAlloc.bindResultFunc`, `data.HwAlloc.pHandle`: kept, never called
/// (hw_resources.c:268-269), but pointers all the same.
const OS32_HW_BIND: usize = 96;
const OS32_HW_HANDLE: usize = 104;

const NVOS32_FUNCTION_ALLOC_SIZE: u32 = 2;
const NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT: u32 = 6;
const NVOS32_FUNCTION_ALLOC_SIZE_RANGE: u32 = 14;
const NVOS32_FUNCTION_HW_ALLOC: u32 = 19;
const NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR: u32 = 27;

/// NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: memory the caller already has, named by
/// CPU address.
pub const NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: u32 = 0x71;

/// Classes no guest may have the backend allocate, because their parameters
/// hand RM something the backend cannot vouch for as a pointer or address:
///
/// - 0x71 NV01_MEMORY_SYSTEM_OS_DESCRIPTOR: a CPU address to pin, or a
///   dma-buf descriptor by number (os_desc_mem.c:146, osmemdesc.c:1037).
/// - 0x78, 0x7e NV01_EVENT_KERNEL_CALLBACK(_EX): `data` is a kernel function
///   pointer (os.c:1568-1589). RM refuses a non-kernel caller
///   (event_api.c:75-88); refused here too, so that stays true whatever the
///   backend's privilege.
/// - 0x81-0x83 NV01_MEMORY_LIST_*: a list of physical pages, read through a
///   user pointer (cl84a0.h:106; rmapi_deprecated_allocmemory.c:401).
///   RS_FLAGS_ALLOC_PRIVILEGED, so reachable only if the backend runs as
///   root -- and then it is physical memory.
/// - 0xc1 NV_FB_SEGMENT: page arrays and CPU addresses (cl00c1.h:50-58).
/// - 0x0092 NV0092_RG_LINE_CALLBACK, 0x9010 NV9010_VBLANK_CALLBACK: kernel
///   function pointers (cl0092.h:68, cl9010.h:39); kernel-privileged in RM.
pub const REFUSED_ALLOC_CLASSES: [u32; 9] =
    [0x71, 0x78, 0x7e, 0x81, 0x82, 0x83, 0xc1, 0x0092, 0x9010];

/// ALLOC_MEMORY classes whose `pMemory` RM reads rather than writes.
const REFUSED_ALLOC_MEMORY_CLASSES: [u32; 4] = [0x71, 0x81, 0x82, 0x83];

/// Check a v1 RM escape's top-level block, `params` (the backend's own copy,
/// the bytes the host will read), and rewrite what must not reach the host.
/// `cmd` is the full ioctl number the host will be called with. `Err` is
/// the errno to answer with, and nothing reaches the host.
///
/// RM_ALLOC's and RM_CONTROL's own parameter pointer is not handled here:
/// `dispatch_nested` points it at the nested block or zeroes it.
/// UNMAP_MEMORY's and UPDATE_DEVICE_MAPPING_INFO's addresses are keys, not
/// pointers, and their dispatchers put the backend's own in.
pub(crate) fn rm_escape(cmd: u32, params: &mut [u8]) -> Result<Restore, Errno> {
    let escape = hostfd::ioc_nr(cmd);
    // The host reads the block by _IOC_SIZE (nv.c:2496); the offsets below
    // are for the one size each escape has.
    let sized = |want: usize| {
        if hostfd::ioc_size(cmd) == want && params.len() == want {
            Ok(())
        } else {
            log::warn!(
                "RM escape {escape:#04x}: {} bytes (command says {}), not the {want} its \
                 pointers are known at",
                params.len(),
                hostfd::ioc_size(cmd)
            );
            Err(libc::EINVAL)
        }
    };
    let mut restore = Restore::default();
    match escape {
        NV_ESC_IOCTL_XFER_CMD
        | NV_ESC_RM_I2C_ACCESS
        | NV_ESC_RM_IDLE_CHANNELS
        | NV_ESC_RM_ACCESS_REGISTRY
        | NV_ESC_RM_GET_EVENT_DATA
        | NV_ESC_RM_ADD_VBLANK_CALLBACK => {
            log::warn!(
                "RM escape {escape:#04x} refused: it hands the host a pointer the backend \
                 cannot give an address of its own"
            );
            return Err(libc::EPERM);
        }
        // The host installs the dma-buf it makes in the caller's table --
        // ours -- and writes its number back, or with a number already set
        // looks one up there (nv-dmabuf.c:1683-1697, 1738-1750). Nothing
        // turns either into a guest descriptor, so the export leaked a
        // descriptor pinning video memory in this process per call and
        // handed the guest a number that meant nothing to it (S-15). Refused
        // until there is a consumer to translate it for; NVIDIA's GBM and
        // CUDA's dma-buf export take this path, and fail as on a driver
        // without dma-buf support. Whatever the ABI policy: this check runs
        // before any profile is known.
        NV_ESC_EXPORT_TO_DMABUF_FD => {
            log::warn!(
                "EXPORT_TO_DMABUF_FD refused: the dma-buf it makes would be the \
                 backend's, not the guest's"
            );
            return Err(libc::EOPNOTSUPP);
        }
        NV_ESC_RM_ALLOC => {
            sized(OS64_SIZE)?;
            let class = rd32(params, OS64_CLASS).unwrap_or(0);
            if REFUSED_ALLOC_CLASSES.contains(&class) {
                log::warn!("RM_ALLOC of class {class:#x} refused (guestptr.rs)");
                return Err(libc::EPERM);
            }
            if let Some(v) = take(params, OS64_RIGHTS) {
                restore.0.push((OS64_RIGHTS, v));
            }
        }
        NV_ESC_RM_CONTROL => {
            sized(OS54_SIZE)?;
            let ctl = rd32(params, OS54_CMD).unwrap_or(0);
            if REFUSED_CONTROLS.contains(&ctl) {
                log::warn!(
                    "RM control {ctl:#010x} refused: its pointers cannot be named one by one"
                );
                return Err(libc::EPERM);
            }
        }
        NV_ESC_RM_ALLOC_MEMORY => {
            sized(OS02_FD_SIZE)?;
            let class = rd32(params, OS02_CLASS).unwrap_or(0);
            if REFUSED_ALLOC_MEMORY_CLASSES.contains(&class) {
                log::warn!("ALLOC_MEMORY of class {class:#x} refused (guestptr.rs)");
                return Err(libc::EPERM);
            }
            // OUT for every other class: RM writes it and reads nothing
            // (rmapi_deprecated_allocmemory.c:162, 174).
            take(params, OS02_MEMORY);
        }
        NV_ESC_RM_VID_HEAP_CONTROL => {
            sized(OS32_SIZE)?;
            match rd32(params, OS32_FUNCTION).unwrap_or(0) {
                NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR => {
                    log::warn!("VID_HEAP_CONTROL ALLOC_OS_DESCRIPTOR refused (guestptr.rs)");
                    return Err(libc::EPERM);
                }
                NVOS32_FUNCTION_ALLOC_SIZE | NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT => {
                    take(params, OS32_ALLOC_ADDRESS);
                }
                NVOS32_FUNCTION_ALLOC_SIZE_RANGE => {
                    take(params, OS32_RANGE_ADDRESS);
                }
                NVOS32_FUNCTION_HW_ALLOC => {
                    for off in [OS32_HW_BIND, OS32_HW_HANDLE] {
                        if let Some(v) = take(params, off) {
                            restore.0.push((off, v));
                        }
                    }
                }
                _ => {}
            }
        }
        NV_ESC_RM_MAP_MEMORY => {
            sized(OS33_FD_SIZE)?;
            // OUT: RM writes the mapping's address (Nv04MapMemory).
            take(params, OS33_LINEAR);
        }
        _ => {}
    }
    Ok(restore)
}

// ───────────────────────────── RM controls ─────────────────────────────

/// A control whose parameters hold user pointers RM follows, and where.
pub(crate) struct ControlPointers {
    pub cmd: u32,
    pub ptrs: &'static [usize],
}

const fn c(cmd: u32, ptrs: &'static [usize]) -> ControlPointers {
    ControlPointers { cmd, ptrs }
}

/// Every control RM dereferences a user pointer inside the parameters of,
/// with the pointer offsets.
///
/// The union of embeddedParamCopyIn (embedded_param_copy.c) in 535.129.03,
/// 580.95.05, 595.58.03 and 610.57.04, the deprecated V1 controls
/// (rmapi_deprecated_control.c:80-87) and the two handlers that copy from
/// user themselves. The offsets were measured with offsetof against each
/// release's SDK headers and agree in all four, except that 535 numbers the
/// NV0073 ACPI call differently (both are listed) and has no
/// busEgmPeerIds (an offset past a block's end is skipped).
pub(crate) const CONTROL_POINTERS: &[ControlPointers] = &[
    c(0x2080016e, &[8]),             // NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO
    c(0x20800123, &[8]),             // NV2080_CTRL_CMD_GPU_GET_ENGINES
    c(0x20802a01, &[8]),             // NV2080_CTRL_CMD_CE_GET_CAPS
    c(0x0080170d, &[8, 16]),         // NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST
    c(0x00801705, &[40]),            // NV0080_CTRL_CMD_FIFO_START_SELECTED_CHANNELS (535)
    c(0x00000130, &[8, 24]),         // NV0000_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD
    c(0x00730120, &[8, 24]),         // NV0073_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD
    c(0x00730168, &[8, 24]),         // the same, as 535 numbers it
    c(0x00801401, &[8]),             // NV0080_CTRL_CMD_HOST_GET_CAPS
    c(0x20800802, &[8]),             // NV2080_CTRL_CMD_BIOS_GET_INFO
    c(0x20800803, &[1048]),          // NV2080_CTRL_CMD_BIOS_GET_NBSI
    c(0x20800806, &[16]),            // NV2080_CTRL_CMD_BIOS_GET_NBSI_OBJ
    c(0x00801104, &[8]),             // NV0080_CTRL_CMD_GR_GET_INFO
    c(0x00801701, &[8]),             // NV0080_CTRL_CMD_FIFO_GET_CAPS
    c(0xa0bc0101, &[24]),            // NVA0BC_CTRL_CMD_NVENC_SW_SESSION_UPDATE_INFO
    c(0x83de0315, &[16]),            // NV83DE_CTRL_CMD_DEBUG_READ_MEMORY
    c(0x83de0316, &[16]),            // NV83DE_CTRL_CMD_DEBUG_WRITE_MEMORY
    c(0x83de0326, &[0]),             // NV83DE_CTRL_CMD_DEBUG_READ_BATCH_MEMORY
    c(0x83de0327, &[0]),             // NV83DE_CTRL_CMD_DEBUG_WRITE_BATCH_MEMORY
    c(0x402c0102, &[24]),            // NV402C_CTRL_CMD_I2C_INDEXED
    c(0x20800122, &[24]),            // NV2080_CTRL_CMD_GPU_EXEC_REG_OPS
    c(0x20802402, &[0]),             // NV2080_CTRL_CMD_NVD_GET_DUMP
    c(0x00000602, &[0]),             // NV0000_CTRL_CMD_NVD_GET_DUMP
    c(0x00410110, &[8]),             // NV0041_CTRL_CMD_GET_SURFACE_INFO
    c(0xa0830103, &[0]),             // NVA083_CTRL_CMD_VIRTUAL_DISPLAY_GET_DEFAULT_EDID
    c(0x00000127, &[160, 168]),      // NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS
    c(0x00801301, &[8]),             // NV0080_CTRL_CMD_FB_GET_CAPS
    c(0x00800201, &[8]),             // NV0080_CTRL_CMD_GPU_GET_CLASSLIST
    c(0x20800124, &[8]),             // NV2080_CTRL_CMD_GPU_GET_ENGINE_CLASSLIST
    c(0x00801102, &[8]),             // NV0080_CTRL_CMD_GR_GET_CAPS
    c(0x20800610, &[16]),            // NV2080_CTRL_CMD_I2C_ACCESS
    c(0x20801201, &[8]),             // NV2080_CTRL_CMD_GR_GET_INFO
    c(0xb06f010d, &[8]),             // NVB06F_CTRL_CMD_MIGRATE_ENGINE_CTX_DATA
    c(0xb06f010c, &[8]),             // NVB06F_CTRL_CMD_GET_ENGINE_CTX_DATA
    c(0x20802204, &[16]),            // NV2080_CTRL_CMD_RC_READ_VIRTUAL_MEM
    c(0x0080180f, &[48]),            // NV0080_CTRL_CMD_DMA_UPDATE_PDE_2
    c(0x208001e8, &[24]),            // NV2080_CTRL_CMD_GPU_RPC_GSP_TEST
    c(0x208001f2, &[8, 24, 40, 64]), // NV2080_CTRL_CMD_GSP_CRYPTO_CONTROL
    // Deprecated V1 controls, converted by RM with the user's pointers.
    c(0x00000101, &[8, 16, 24]), // NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION
    c(0x20801802, &[8]),         // NV2080_CTRL_CMD_BUS_GET_INFO
    c(0x00801c01, &[8]),         // NV0080_CTRL_CMD_BSP_GET_CAPS
    c(0x20800101, &[8]),         // NV2080_CTRL_CMD_GPU_GET_INFO
    c(0x0073136a, &[56]),        // NV0073_CTRL_CMD_DP_SET_MSA_PROPERTIES
    c(0x00801b01, &[8]),         // NV0080_CTRL_CMD_MSENC_GET_CAPS
    c(0x20801301, &[8]),         // NV2080_CTRL_CMD_FB_GET_INFO
    // Handlers that copy to user themselves (mem_mgr_ctrl.c:617).
    c(0x20801349, &[8, 24]), // NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO
];

/// Controls refused outright: their pointers are not at fixed offsets.
///
/// - NV402C_CTRL_CMD_I2C_TRANSACTION: `pMessage` sits at 24, 32 or 40
///   depending on `transType`, overlapping plain fields of the other arms
///   (embedded_param_copy.c:73-158).
/// - NV83DE_CTRL_CMD_READ_SURFACE, _WRITE_SURFACE: an array of up to
///   MAX_ACCESS_OPS ops, each with its own `pCpuVA`, copied to and from with
///   portMemExCopy*User (kernel_sm_debugger_session_ctrl.c:103-174).
pub(crate) const REFUSED_CONTROLS: [u32; 3] = [0x402c0105, 0x83de031a, 0x83de031b];

/// The pointer offsets RM follows in control `cmd`'s parameters, if any.
pub(crate) fn control_pointers(cmd: u32) -> &'static [usize] {
    CONTROL_POINTERS
        .iter()
        .find(|c| c.cmd == cmd)
        .map_or(&[], |c| c.ptrs)
}

/// Zero every pointer RM would follow in control `cmd`'s parameters,
/// `nested` (the backend's copy), except the one at `relocated`, which
/// already holds the address of a buffer of the backend's. Returns the
/// caller's values, offsets into `nested`, for the reply.
///
/// Zero is RM's "no buffer": with a nonzero count it answers
/// NV_ERR_INVALID_ARGUMENT (param_copy.c:43-53), the status a native caller
/// with a bad pointer would get. An offset past the block's end is one this
/// release's layout does not have.
pub(crate) fn scrub_control(cmd: u32, nested: &mut [u8], relocated: Option<usize>) -> Restore {
    let mut restore = Restore::default();
    for &off in control_pointers(cmd) {
        if Some(off) == relocated {
            continue;
        }
        if let Some(v) = take(nested, off) {
            log::warn!(
                "RM control {cmd:#010x}: pointer at {off} was not sent with the data it \
                 addresses; zeroed rather than handed to the host"
            );
            restore.0.push((off, v));
        }
    }
    restore
}

// ───────────────────────────── UVM ─────────────────────────────

pub const UVM_INITIALIZE: u32 = 0x3000_0001;
pub const UVM_DEINITIALIZE: u32 = 0x3000_0002;

/// UVM_INIT_FLAGS_DISABLE_HMM / _DISABLE_PAGEABLE_MIGRATIONS (uvm_types.h:64-66).
const UVM_INIT_FLAGS_DISABLE_HMM: u64 = 0x1;
/// UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS (uvm_types.h:68): with it the VA
/// space never gets pageable access, by ATS or HMM (uvm_va_space.c:190-204).
const UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS: u64 = 0x4;

/// The UVM commands (uvm_ioctl.h numbers) that go through. Each names UVM's
/// own ranges -- made by mmap of the UVM file, CREATE_EXTERNAL_RANGE or
/// ALLOC_SEMAPHORE_POOL -- RM handles, GPU UUIDs, or nothing at all; with
/// pageable access off, a VA outside UVM's ranges is refused by UVM itself.
/// None carries a CPU buffer for UVM to copy through or pin.
const UVM_ALLOWED: &[u32] = &[
    UVM_INITIALIZE,
    UVM_DEINITIALIZE,
    25, // REGISTER_GPU_VASPACE
    26, // UNREGISTER_GPU_VASPACE
    27, // REGISTER_CHANNEL
    28, // UNREGISTER_CHANNEL
    29, // ENABLE_PEER_ACCESS
    30, // DISABLE_PEER_ACCESS
    33, // MAP_EXTERNAL_ALLOCATION
    34, // FREE
    37, // REGISTER_GPU
    38, // UNREGISTER_GPU
    39, // PAGEABLE_MEM_ACCESS (a query)
    42, // SET_PREFERRED_LOCATION
    43, // UNSET_PREFERRED_LOCATION
    44, // ENABLE_READ_DUPLICATION
    45, // DISABLE_READ_DUPLICATION
    46, // SET_ACCESSED_BY
    47, // UNSET_ACCESSED_BY
    51, // MIGRATE
    65, // MAP_DYNAMIC_PARALLELISM_REGION
    66, // UNMAP_EXTERNAL
    67, // TOOLS_FLUSH_EVENTS
    68, // ALLOC_SEMAPHORE_POOL
    69, // CLEAN_UP_ZOMBIE_RESOURCES
    70, // PAGEABLE_MEM_ACCESS_ON_GPU (a query)
    72, // VALIDATE_VA_RANGE
    73, // CREATE_EXTERNAL_RANGE
    74, // MAP_EXTERNAL_SPARSE
    75, // MM_INITIALIZE
    78, // ALLOC_DEVICE_P2P
    79, // CLEAR_ALL_ACCESS_COUNTERS
    80, // DISCARD
];

/// Check a UVM ioctl, `cmd` on a UVM (`tools == false`) or UVM tools file,
/// with `params` the backend's copy of its argument.
///
/// Refused: the whole tools device (its first call, INIT_EVENT_TRACKER, pins
/// two user buffers, uvm_tools.c; nothing else works without it), and on the
/// UVM device everything not in [`UVM_ALLOWED`] -- among them
/// TOOLS_READ/WRITE_PROCESS_MEMORY (copy through `buffer`),
/// TOOLS_GET_PROCESSOR_UUID_TABLE(_V2) (copies out to `tablePtr`),
/// QUERY_RESIDENCY (two user arrays), POPULATE_PAGEABLE (faults in pages of
/// the calling process, the backend, uvm_populate_pageable.c:194-226),
/// the UVM-Lite commands, and the test ioctls.
pub(crate) fn uvm_gate(tools: bool, cmd: u32, params: &mut [u8]) -> Result<Restore, Errno> {
    if tools {
        log::warn!("UVM tools ioctl {cmd:#x} refused: the tools device pins user buffers");
        return Err(libc::EPERM);
    }
    if !UVM_ALLOWED.contains(&cmd) {
        log::warn!("UVM ioctl {cmd:#x} refused (guestptr.rs)");
        return Err(libc::EPERM);
    }
    let mut restore = Restore::default();
    if cmd == UVM_INITIALIZE {
        // UVM_INITIALIZE_PARAMS {NvU64 flags; NV_STATUS rmStatus;}
        let Some(flags) = rd64(params, 0) else {
            return Err(libc::EINVAL);
        };
        let forced = flags | UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS | UVM_INIT_FLAGS_DISABLE_HMM;
        if forced != flags {
            restore.0.push((0, flags.to_le_bytes()));
            params[..8].copy_from_slice(&forced.to_le_bytes());
        }
    }
    Ok(restore)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfd::{IOC_RW, ioc};

    fn put32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    const ALLOC: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 48);
    const CONTROL: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_CONTROL, 32);
    const ALLOC_MEMORY: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC_MEMORY, 56);
    const VID_HEAP: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_VID_HEAP_CONTROL, 184);
    const MAP_MEMORY: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_MAP_MEMORY, 56);

    #[test]
    fn escapes_with_pointers_the_backend_cannot_relocate_are_refused() {
        for (nr, size) in [
            (NV_ESC_IOCTL_XFER_CMD, 16),
            (NV_ESC_RM_I2C_ACCESS, 32),
            (NV_ESC_RM_IDLE_CHANNELS, 56),
            (NV_ESC_RM_ACCESS_REGISTRY, 72),
            (NV_ESC_RM_GET_EVENT_DATA, 16),
            (NV_ESC_RM_ADD_VBLANK_CALLBACK, 32),
        ] {
            let mut p = vec![0x11u8; size];
            assert_eq!(
                rm_escape(ioc(IOC_RW, b'F', nr, size), &mut p),
                Err(libc::EPERM),
                "escape {nr:#x}"
            );
        }
    }

    #[test]
    fn rights_requested_never_reach_the_host_and_come_back_as_sent() {
        let mut p = vec![0u8; 48];
        put32(&mut p, OS64_CLASS, 0x41);
        put64(&mut p, OS64_RIGHTS, 0x7fff_dead_b000);
        let r = rm_escape(ALLOC, &mut p).unwrap();
        assert_eq!(rd64(&p, OS64_RIGHTS), Some(0));
        let mut reply = p.clone();
        r.apply(&mut reply);
        assert_eq!(rd64(&reply, OS64_RIGHTS), Some(0x7fff_dead_b000));
    }

    #[test]
    fn classes_that_hand_rm_a_cpu_address_or_a_function_are_never_allocated() {
        for class in REFUSED_ALLOC_CLASSES {
            let mut p = vec![0u8; 48];
            put32(&mut p, OS64_CLASS, class);
            assert_eq!(
                rm_escape(ALLOC, &mut p),
                Err(libc::EPERM),
                "class {class:#x}"
            );
        }
        let mut p = vec![0u8; 48];
        put32(&mut p, OS64_CLASS, 0x3e);
        assert!(rm_escape(ALLOC, &mut p).is_ok(), "plain system memory");
    }

    #[test]
    fn an_rm_alloc_of_another_size_is_not_guessed_at() {
        let mut p = vec![0u8; 32];
        assert_eq!(
            rm_escape(ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 32), &mut p),
            Err(libc::EINVAL)
        );
        let mut p = vec![0u8; 48];
        assert_eq!(
            rm_escape(ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 32), &mut p),
            Err(libc::EINVAL),
            "the host reads by the command's size"
        );
    }

    #[test]
    fn os_descriptor_memory_is_refused_on_every_path() {
        let mut p = vec![0u8; 56];
        put32(&mut p, OS02_CLASS, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
        put64(&mut p, OS02_MEMORY, 0x7f00_0000_0000);
        assert_eq!(rm_escape(ALLOC_MEMORY, &mut p), Err(libc::EPERM));

        let mut p = vec![0u8; 184];
        put32(&mut p, OS32_FUNCTION, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
        assert_eq!(rm_escape(VID_HEAP, &mut p), Err(libc::EPERM));

        for class in 0x81..=0x83 {
            let mut p = vec![0u8; 56];
            put32(&mut p, OS02_CLASS, class);
            assert_eq!(
                rm_escape(ALLOC_MEMORY, &mut p),
                Err(libc::EPERM),
                "{class:#x}"
            );
        }
    }

    #[test]
    fn output_addresses_go_to_the_host_as_zero() {
        let mut p = vec![0u8; 56];
        put32(&mut p, OS02_CLASS, 0x3e);
        put64(&mut p, OS02_MEMORY, 0x1234_5000);
        assert!(rm_escape(ALLOC_MEMORY, &mut p).unwrap().is_empty());
        assert_eq!(rd64(&p, OS02_MEMORY), Some(0));

        let mut p = vec![0u8; 56];
        put64(&mut p, OS33_LINEAR, 0x1234_5000);
        rm_escape(MAP_MEMORY, &mut p).unwrap();
        assert_eq!(rd64(&p, OS33_LINEAR), Some(0));

        for (f, off) in [
            (NVOS32_FUNCTION_ALLOC_SIZE, OS32_ALLOC_ADDRESS),
            (NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT, OS32_ALLOC_ADDRESS),
            (NVOS32_FUNCTION_ALLOC_SIZE_RANGE, OS32_RANGE_ADDRESS),
        ] {
            let mut p = vec![0u8; 184];
            put32(&mut p, OS32_FUNCTION, f);
            put64(&mut p, off, 0x1234_5000);
            rm_escape(VID_HEAP, &mut p).unwrap();
            assert_eq!(rd64(&p, off), Some(0), "function {f}");
        }
    }

    #[test]
    fn hw_alloc_pointers_are_zeroed_and_given_back() {
        let mut p = vec![0u8; 184];
        put32(&mut p, OS32_FUNCTION, NVOS32_FUNCTION_HW_ALLOC);
        put64(&mut p, OS32_HW_BIND, 0xaaaa);
        put64(&mut p, OS32_HW_HANDLE, 0xbbbb);
        let r = rm_escape(VID_HEAP, &mut p).unwrap();
        assert_eq!(
            (rd64(&p, OS32_HW_BIND), rd64(&p, OS32_HW_HANDLE)),
            (Some(0), Some(0))
        );
        r.apply(&mut p);
        assert_eq!(
            (rd64(&p, OS32_HW_BIND), rd64(&p, OS32_HW_HANDLE)),
            (Some(0xaaaa), Some(0xbbbb))
        );
    }

    #[test]
    fn a_heap_function_without_pointers_is_left_alone() {
        let mut p = vec![0x5au8; 184];
        put32(&mut p, OS32_FUNCTION, 3); // FREE
        let before = p.clone();
        rm_escape(VID_HEAP, &mut p).unwrap();
        assert_eq!(p, before);
    }

    #[test]
    fn controls_whose_pointers_have_no_fixed_place_are_refused() {
        for ctl in REFUSED_CONTROLS {
            let mut p = vec![0u8; 32];
            put32(&mut p, OS54_CMD, ctl);
            assert_eq!(rm_escape(CONTROL, &mut p), Err(libc::EPERM), "{ctl:#x}");
        }
    }

    #[test]
    fn a_control_pointer_not_sent_with_its_data_is_zeroed_and_given_back() {
        // NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION: three pointers.
        let mut n = vec![0u8; 32];
        put64(&mut n, 8, 0x1000);
        put64(&mut n, 16, 0x2000);
        put64(&mut n, 24, 0x3000);
        let r = scrub_control(0x101, &mut n, Some(16));
        assert_eq!(
            (rd64(&n, 8), rd64(&n, 16), rd64(&n, 24)),
            (Some(0), Some(0x2000), Some(0))
        );
        r.apply(&mut n);
        assert_eq!((rd64(&n, 8), rd64(&n, 24)), (Some(0x1000), Some(0x3000)));
    }

    #[test]
    fn a_control_rm_follows_no_pointer_in_is_left_alone() {
        let mut n = vec![0x77u8; 64];
        let before = n.clone();
        assert!(scrub_control(0x20800a01, &mut n, None).is_empty());
        assert_eq!(n, before);
    }

    #[test]
    fn an_offset_past_the_block_is_a_field_this_release_lacks() {
        // 535's GET_P2P_CAPS ends at busPeerIds.
        let mut n = vec![0u8; 168];
        put64(&mut n, 160, 0x4000);
        scrub_control(0x127, &mut n, None);
        assert_eq!(rd64(&n, 160), Some(0));
    }

    #[test]
    fn every_pointer_offset_is_aligned_and_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for c in CONTROL_POINTERS {
            assert!(seen.insert(c.cmd), "{:#x} twice", c.cmd);
            assert!(!REFUSED_CONTROLS.contains(&c.cmd));
            for &o in c.ptrs {
                assert_eq!(o % 8, 0, "{:#x} at {o}", c.cmd);
            }
        }
    }

    #[test]
    fn uvm_is_initialised_without_pageable_access_whatever_the_guest_asks() {
        let mut p = vec![0u8; 16];
        put64(&mut p, 0, 0x2); // MULTI_PROCESS_SHARING_MODE
        let r = uvm_gate(false, UVM_INITIALIZE, &mut p).unwrap();
        assert_eq!(rd64(&p, 0), Some(0x7));
        r.apply(&mut p);
        assert_eq!(
            rd64(&p, 0),
            Some(0x2),
            "the caller reads back its own flags"
        );
    }

    #[test]
    fn uvm_commands_that_touch_cpu_memory_are_refused() {
        for cmd in [56, 62, 63, 64, 71, 76, 77, 81, 13, 16, 21, 35, 200, 0x7ff] {
            let mut p = vec![0u8; 64];
            assert_eq!(uvm_gate(false, cmd, &mut p), Err(libc::EPERM), "{cmd}");
        }
        for cmd in [33, 37, 51, 73, 75] {
            let mut p = vec![0u8; 64];
            assert!(uvm_gate(false, cmd, &mut p).is_ok(), "{cmd}");
        }
    }

    #[test]
    fn nothing_goes_to_the_uvm_tools_device() {
        let mut p = vec![0u8; 64];
        assert_eq!(uvm_gate(true, 67, &mut p), Err(libc::EPERM));
    }
}

/// Through the whole v1 path, against a fake host that reads the pointer
/// fields of what it is handed the way the real one would: every one must be
/// 0 or the address of a buffer of the backend's holding what the guest sent.
#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::hostfd::{HandleKind, IOC_RW, ioc};
    use crate::nvidia::NvidiaBackend;
    use protocol::messages::{DeviceKind, IoctlResp, MsgHeader, MsgType};
    use std::cell::RefCell;
    use std::os::fd::OwnedFd;

    /// A guest address: never mapped in this process, so the fake host must
    /// never be handed it.
    const GUEST_PTR: u64 = 0x4141_4141_4000;
    const GUEST_PTR2: u64 = 0x4242_4242_4000;

    /// What the fake host saw, per call.
    #[derive(Debug, Clone, PartialEq)]
    enum Seen {
        /// RM_CONTROL: the params pointer (0 or ours), paramsSize, and the
        /// pointer fields at 8, 16, 24 of the block it addresses with, for
        /// one of ours, the first 8 bytes behind it.
        Control {
            params: u64,
            size: u32,
            inner: Vec<(u64, Option<u64>)>,
        },
        /// RM_ALLOC: pAllocParms (0 or ours) and, for ours, the first 8
        /// bytes behind it; pRightsRequested.
        Alloc {
            params: u64,
            first: Option<u64>,
            rights: u64,
        },
        /// nvidia-drm GEM import: nvkms_params_ptr.
        Gem {
            ptr: u64,
        },
        /// UVM_INITIALIZE: flags.
        UvmInit {
            flags: u64,
        },
        /// UNMAP_MEMORY: pLinearAddress; UPDATE_DEVICE_MAPPING_INFO: pOld,
        /// pNew. Keys, not pointers, but addresses in this process all the
        /// same.
        Addresses(Vec<u64>),
        Other(u64),
    }

    std::thread_local! {
        static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
    }

    fn seen() -> Vec<Seen> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn word64(a: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(a[at..at + 8].try_into().unwrap())
    }

    /// Read 8 bytes behind a pointer the backend handed the host. A guest
    /// address fails the assertion before anything is read.
    fn behind(p: u64) -> u64 {
        assert!(
            p != GUEST_PTR && p != GUEST_PTR2,
            "a guest address reached the host"
        );
        // SAFETY: a nonzero pointer the backend put in the block, which is
        // its own live buffer for the length of the call.
        unsafe { (p as *const u64).read_unaligned() }
    }

    unsafe fn fake_host(_fd: std::os::fd::RawFd, request: u64, arg: *mut u8) -> i32 {
        let request = request as u32;
        let len = hostfd::ioc_size(request).max(16);
        // SAFETY: the HostIoctl contract: `arg` holds at least _IOC_SIZE
        // bytes; UVM's plain numbers carry the guest's 16.
        let a = unsafe { std::slice::from_raw_parts_mut(arg, len) };
        let s = match (hostfd::ioc_type(request), hostfd::ioc_nr(request)) {
            _ if request == UVM_INITIALIZE => Seen::UvmInit {
                flags: word64(a, 0),
            },
            (b'F', 0x2a) => {
                let params = word64(a, 16);
                let size = u32::from_le_bytes(a[24..28].try_into().unwrap());
                let mut inner = Vec::new();
                if params != 0 {
                    assert!(params != GUEST_PTR, "the guest's params pointer reached RM");
                    // SAFETY: the backend's nested block, `size` bytes.
                    let n =
                        unsafe { std::slice::from_raw_parts(params as *const u8, size as usize) };
                    for off in [8, 16, 24] {
                        if off + 8 <= n.len() {
                            let p = word64(n, off);
                            inner.push((p, (p != 0).then(|| behind(p))));
                        }
                    }
                }
                a[28..32].fill(0);
                Seen::Control {
                    params,
                    size,
                    inner,
                }
            }
            (b'F', 0x2b) => {
                let params = word64(a, 16);
                let s = Seen::Alloc {
                    params,
                    first: (params != 0).then(|| behind(params)),
                    rights: word64(a, 24),
                };
                a[40..44].fill(0);
                s
            }
            (b'd', 0x41) => Seen::Gem { ptr: word64(a, 8) },
            (b'F', 0x4f) => Seen::Addresses(vec![word64(a, 16)]),
            (b'F', 0x5e) => Seen::Addresses(vec![word64(a, 16), word64(a, 24)]),
            _ => Seen::Other(request as u64),
        };
        SEEN.with(|v| v.borrow_mut().push(s));
        0
    }

    fn backend(kind: HandleKind) -> (NvidiaBackend, u32) {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_host);
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let h = be.adopt_for_test(null, kind);
        (be, h)
    }

    /// A v1 IOCTL; returns (status, the reply's parameter bytes).
    fn v1(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: Option<(u32, &[u8])>,
    ) -> (i32, Vec<u8>) {
        let (deep_at, deep) = deep.unwrap_or((0, &[]));
        let mut req = Vec::new();
        for v in [
            MsgType::Ioctl as u32,
            handle,
            0,
            0,
            cmd,
            outer.len() as u32,
            if nested.is_empty() {
                0
            } else {
                outer.len() as u32
            },
            nested.len() as u32,
            deep_at,
            deep.len() as u32,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        req.extend_from_slice(deep);
        let mut resp = vec![0u8; 8192];
        let n = be.dispatch(&req, &mut resp);
        let status = i32::from_le_bytes(resp[8..12].try_into().unwrap());
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        (status, resp[body.min(n)..n].to_vec())
    }

    const CONTROL: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_CONTROL, 32);
    const ALLOC: u32 = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 48);
    const GEM_IMPORT: u32 = ioc(IOC_RW, b'd', 0x41, 32);

    fn nvos54(cmd: u32, params: u64, size: u32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[8..12].copy_from_slice(&cmd.to_le_bytes());
        b[16..24].copy_from_slice(&params.to_le_bytes());
        b[24..28].copy_from_slice(&size.to_le_bytes());
        b
    }

    fn ctl() -> (NvidiaBackend, u32) {
        backend(HandleKind::Dev(DeviceKind::Ctl))
    }

    #[test]
    fn a_control_sent_without_parameters_reaches_rm_with_a_null_pointer() {
        let (mut be, h) = ctl();
        let (st, reply) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(0x20800a01, GUEST_PTR, 0),
            &[],
            None,
        );
        assert_eq!(st, 0);
        assert_eq!(
            seen(),
            vec![Seen::Control {
                params: 0,
                size: 0,
                inner: vec![]
            }]
        );
        assert_eq!(
            word64(&reply, 16),
            GUEST_PTR,
            "the caller reads back its own pointer"
        );
    }

    #[test]
    fn a_parameter_size_with_no_parameters_never_reaches_rm() {
        let (mut be, h) = ctl();
        let (st, _) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(0x20800a01, GUEST_PTR, 64),
            &[],
            None,
        );
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    #[test]
    fn an_alloc_sent_without_parameters_names_neither_guest_pointer() {
        let (mut be, h) = ctl();
        let mut os64 = vec![0u8; 48];
        os64[12..16].copy_from_slice(&0x41u32.to_le_bytes());
        os64[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        os64[24..32].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        let (st, reply) = v1(&mut be, h, ALLOC, &os64, &[], None);
        assert_eq!(st, 0);
        assert_eq!(
            seen(),
            vec![Seen::Alloc {
                params: 0,
                first: None,
                rights: 0
            }]
        );
        assert_eq!(
            (word64(&reply, 16), word64(&reply, 24)),
            (GUEST_PTR, GUEST_PTR2)
        );
    }

    #[test]
    fn an_alloc_with_parameters_gets_our_block_and_still_no_rights_pointer() {
        let (mut be, h) = ctl();
        let mut os64 = vec![0u8; 48];
        os64[12..16].copy_from_slice(&0x3eu32.to_le_bytes());
        os64[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        os64[24..32].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        os64[32..36].copy_from_slice(&64u32.to_le_bytes());
        let (st, _) = v1(&mut be, h, ALLOC, &os64, &[0x5a; 64], None);
        assert_eq!(st, 0);
        match &seen()[..] {
            [
                Seen::Alloc {
                    params,
                    first: Some(first),
                    rights: 0,
                },
            ] => {
                assert_ne!(*params, GUEST_PTR);
                assert_eq!(*first, 0x5a5a_5a5a_5a5a_5a5a, "our copy of the block");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_pointer_rm_follows_in_a_control_is_ours_or_null() {
        // NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION: pointers at 8, 16, 24,
        // with the one at 16 sent along with what it addresses.
        let (mut be, h) = ctl();
        let mut nested = vec![0u8; 32];
        nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        nested[16..24].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        nested[24..32].copy_from_slice(&GUEST_PTR.to_le_bytes());
        let deep = 0x1122_3344_5566_7788u64.to_le_bytes();
        let (st, reply) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(0x101, GUEST_PTR, 32),
            &nested,
            Some((16, &deep)),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [
                Seen::Control {
                    params,
                    size: 32,
                    inner,
                },
            ] => {
                assert_ne!(*params, 0);
                assert_ne!(inner[1].0, 0);
                assert_eq!(
                    inner,
                    &vec![
                        (0, None),
                        (inner[1].0, Some(0x1122_3344_5566_7788)),
                        (0, None)
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
        // The caller's own pointers come back, in the outer block and in
        // the nested one.
        assert_eq!(word64(&reply, 16), GUEST_PTR);
        assert_eq!(word64(&reply, 32 + 8), GUEST_PTR);
        assert_eq!(word64(&reply, 32 + 16), GUEST_PTR2);
        assert_eq!(word64(&reply, 32 + 24), GUEST_PTR);
    }

    #[test]
    fn a_gem_import_without_nvkms_parameters_names_no_guest_address() {
        let (mut be, h) = backend(HandleKind::DriRender(0));
        let mut p = vec![0u8; 32];
        p[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        let (st, reply) = v1(&mut be, h, GEM_IMPORT, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen(), vec![Seen::Gem { ptr: 0 }]);
        assert_eq!(word64(&reply, 8), GUEST_PTR);

        // With a size and nothing sent it is refused.
        p[16..24].copy_from_slice(&16u64.to_le_bytes());
        let (st, _) = v1(&mut be, h, GEM_IMPORT, &p, &[], None);
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    #[test]
    fn memory_named_by_cpu_address_never_reaches_rm() {
        let (mut be, h) = ctl();
        // RM_ALLOC of NV01_MEMORY_SYSTEM_OS_DESCRIPTOR, parameters and all.
        let mut os64 = vec![0u8; 48];
        os64[12..16].copy_from_slice(&NV01_MEMORY_SYSTEM_OS_DESCRIPTOR.to_le_bytes());
        os64[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        os64[32..36].copy_from_slice(&64u32.to_le_bytes());
        let mut desc = vec![0u8; 64];
        desc[24..32].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        assert_eq!(v1(&mut be, h, ALLOC, &os64, &desc, None).0, -libc::EPERM);

        // NVOS02 ALLOC_MEMORY of the same class.
        let alloc_memory = ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC_MEMORY, 56);
        let mut os02 = vec![0u8; 56];
        os02[12..16].copy_from_slice(&NV01_MEMORY_SYSTEM_OS_DESCRIPTOR.to_le_bytes());
        os02[24..32].copy_from_slice(&GUEST_PTR.to_le_bytes());
        os02[48..52].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(
            v1(&mut be, h, alloc_memory, &os02, &[], None).0,
            -libc::EPERM
        );

        // VID_HEAP_CONTROL ALLOC_OS_DESCRIPTOR.
        let vid_heap = ioc(IOC_RW, b'F', NV_ESC_RM_VID_HEAP_CONTROL, 184);
        let mut os32 = vec![0u8; 184];
        os32[8..12].copy_from_slice(&NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR.to_le_bytes());
        os32[64..72].copy_from_slice(&GUEST_PTR.to_le_bytes());
        assert_eq!(v1(&mut be, h, vid_heap, &os32, &[], None).0, -libc::EPERM);

        assert!(seen().is_empty(), "none of them reached the host");
    }

    #[test]
    fn a_dma_buf_export_never_reaches_the_host() {
        let (mut be, h) = ctl();
        let export = ioc(IOC_RW, b'F', NV_ESC_EXPORT_TO_DMABUF_FD, 2608);
        let mut p = vec![0u8; 2608];
        p[0..4].copy_from_slice(&(-1i32).to_le_bytes());
        assert_eq!(v1(&mut be, h, export, &p, &[], None).0, -libc::EOPNOTSUPP);
        p[0..4].copy_from_slice(&3i32.to_le_bytes());
        assert_eq!(v1(&mut be, h, export, &p, &[], None).0, -libc::EOPNOTSUPP);
        assert!(seen().is_empty());
    }

    #[test]
    fn a_mapping_we_never_made_is_named_to_the_host_by_no_address() {
        let (mut be, h) = ctl();
        let unmap = ioc(IOC_RW, b'F', NV_ESC_RM_UNMAP_MEMORY, 32);
        let mut p = vec![0u8; 32];
        p[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        let (st, reply) = v1(&mut be, h, unmap, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen(), vec![Seen::Addresses(vec![0])]);
        assert_eq!(word64(&reply, 16), GUEST_PTR);

        let update = ioc(IOC_RW, b'F', NV_ESC_RM_UPDATE_DEVICE_MAPPING_INFO, 40);
        let mut p = vec![0u8; 40];
        p[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        p[24..32].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        let (st, reply) = v1(&mut be, h, update, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen(), vec![Seen::Addresses(vec![0, 0])]);
        assert_eq!(
            (word64(&reply, 16), word64(&reply, 24)),
            (GUEST_PTR, GUEST_PTR2)
        );
    }

    #[test]
    fn escapes_the_backend_cannot_make_safe_never_reach_the_host() {
        let (mut be, h) = ctl();
        let xfer = ioc(IOC_RW, b'F', NV_ESC_IOCTL_XFER_CMD, 16);
        let mut p = vec![0u8; 16];
        p[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        assert_eq!(v1(&mut be, h, xfer, &p, &[], None).0, -libc::EPERM);
        let idle = ioc(IOC_RW, b'F', NV_ESC_RM_IDLE_CHANNELS, 56);
        assert_eq!(v1(&mut be, h, idle, &[0u8; 56], &[], None).0, -libc::EPERM);
        assert!(seen().is_empty());
    }

    #[test]
    fn uvm_reaches_the_host_without_pageable_access_and_answers_with_the_callers_flags() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::Uvm));
        let mut p = vec![0u8; 16];
        p[0..8].copy_from_slice(&0x2u64.to_le_bytes());
        let (st, reply) = v1(&mut be, h, UVM_INITIALIZE, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen(), vec![Seen::UvmInit { flags: 0x7 }]);
        assert_eq!(word64(&reply, 0), 0x2);

        // TOOLS_READ_PROCESS_MEMORY copies through a CPU buffer.
        let mut p = vec![0u8; 40];
        p[0..8].copy_from_slice(&GUEST_PTR.to_le_bytes());
        assert_eq!(v1(&mut be, h, 62, &p, &[], None).0, -libc::EPERM);
        assert!(seen().is_empty());
    }

    #[test]
    fn nothing_reaches_the_uvm_tools_device() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::UvmTools));
        assert_eq!(v1(&mut be, h, 56, &[0u8; 48], &[], None).0, -libc::EPERM);
        assert!(seen().is_empty());
    }
}
