// SPDX-License-Identifier: Apache-2.0
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
//! - **RM escapes** (`rm_escape`): the top-level blocks. Five escapes carry
//!   a pointer the backend has no way to relocate (IOCTL_XFER_CMD's whole
//!   argument, I2C_ACCESS, ACCESS_REGISTRY's three strings, GET_EVENT_DATA's
//!   event record, and ADD_VBLANK_CALLBACK's function pointer) and are
//!   refused. IDLE_CHANNELS' three handle arrays are zeroed for one channel,
//!   which never reads them, and relocated for a list when the guest sends
//!   them as deep segments (`idle_channels_list`); a list without them is
//!   refused. RM_ALLOC's
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
//!   Refused with EPERM when they come this way, with an address alone. A
//!   guest offered BCAP_OS_DESC sends the guest-physical pages behind the
//!   address instead, and `osdesc.rs` hands RM the backend's own mapping of
//!   exactly those pages; that is how cuMemHostRegister and
//!   VK_EXT_external_memory_host work here.
//! - **RM controls** (`scrub_control`): RM follows pointers inside a
//!   control's parameters for the commands in `abi::rmctrl`, and only those
//!   (embedded_param_copy.c, rmapi_deprecated_control.c, and the handlers
//!   that copy from user themselves, mem_mgr_ctrl.c:617 and
//!   kernel_sm_debugger_session_ctrl.c:156), as gen/rmctrl_extract.py
//!   measures them per release. The guest can relocate one of
//!   them (the deep block), or, for a control whose every pointer has RM's
//!   size measured (`abi::rmctrl::DEEP_CONTROLS`), each of them as a deep
//!   segment the backend sizes itself (`deepseg.rs`); every other pointer
//!   field of the command is zeroed, which RM answers as a missing buffer,
//!   and the controls in `abi::rmctrl::ZEROED_CONTROLS` (ACPI methods among
//!   them) take no deep block at all. Two commands carry
//!   pointers the table cannot name one by one (a union selected by a type
//!   field, an array of per-op pointers) and are refused.
//! - **UVM** (`uvm_gate`): nvidia-uvm works on the calling process's address
//!   space, which is the backend's. Pageable memory access is forced off in
//!   UVM_INITIALIZE (so neither HMM nor ATS can let the GPU fault in the
//!   VMM's pages) -- with the flags the host's release has, multi-process
//!   sharing mode among them, and checked with UVM's own query where it
//!   lacks the one that says so outright --
//!   and only commands that name UVM's own ranges, RM handles or
//!   GPU state go through; the tools device and every command that copies
//!   to or from a CPU buffer, pins one, or populates pages of the backend's
//!   own address space are refused.

#![forbid(unsafe_code)]

use std::os::fd::BorrowedFd;

use abi::ioctl::*;

use crate::hostfd;
use crate::sys::block::{Arena, BufId, Restore, SlotKind};

/// A refusal: the errno the guest's ioctl returns.
pub type Errno = i32;

/// What the host is to find in one field of a top-level block the guest
/// sent, other than the guest's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopSlot {
    /// A pointer the host would follow: 0 for the host, and the caller's
    /// value back in the reply.
    Zeroed,
    /// An address the host only writes: 0 for the host, and what the host
    /// wrote back in the reply.
    Out,
    /// A value of the backend's (`width` bytes); the caller's own back in
    /// the reply.
    Forced { value: u64, width: usize },
    /// The descriptor behind handle `handle` of the backend's table
    /// (`width` bytes); the caller's value (the handle) back in the reply.
    Handle { handle: u32, width: usize },
}

/// How the host's copy of a top-level block differs from the guest's: every
/// field named here is declared in the arena before the call (sys/block.rs),
/// so none of them holds a byte the guest wrote. Everything else is the
/// guest's data.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan<'g> {
    pub slots: Vec<(usize, TopSlot)>,
    /// IDLE_CHANNELS' channel list, sent as deep segments (deepseg.rs).
    pub idle: Option<&'g [u8]>,
}

impl Plan<'_> {
    fn zeroed(&mut self, off: usize) {
        self.slots.push((off, TopSlot::Zeroed));
    }

    fn out(&mut self, off: usize) {
        self.slots.push((off, TopSlot::Out));
    }

    /// Declare the plan's fields in block `top` of `a`; `fd_of` names the
    /// descriptor behind a handle (None: not one the field may name).
    pub(crate) fn declare<'f>(
        &self,
        a: &mut Arena,
        top: BufId,
        fd_of: &dyn Fn(u32) -> Option<BorrowedFd<'f>>,
    ) -> Result<(), Errno> {
        for &(off, s) in &self.slots {
            match s {
                TopSlot::Zeroed => {
                    a.slot(top, off, 8, SlotKind::Ptr, Restore::IfSet)?;
                }
                TopSlot::Out => {
                    a.ptr_out(top, off)?;
                }
                TopSlot::Forced { value, width } => {
                    a.value(top, off, width, Restore::Yes)?;
                    a.set_value(top, off, value)?;
                }
                TopSlot::Handle { handle, width } => {
                    a.fd(top, off, width)?;
                    a.set_fd(top, off, fd_of(handle).ok_or(libc::EBADF)?)?;
                }
            }
        }
        if let Some(deep) = self.idle {
            use abi::rmctrl::IDLE_CHANNELS;
            let segs = crate::deepseg::Segments::relocate(
                "RM_IDLE_CHANNELS",
                IDLE_CHANNELS.ptrs,
                a,
                top,
                deep,
            )?;
            // An array not sent is not handed to RM as the guest's address;
            // RM then fails the copy from null, as it would a bad pointer.
            for p in IDLE_CHANNELS.ptrs {
                if !segs.offsets().contains(&p.ptr) {
                    a.slot(top, p.ptr, 8, SlotKind::Ptr, Restore::IfSet)?;
                }
            }
        }
        Ok(())
    }
}

fn rd32(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn rd64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
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
///   Sent with its guest-physical pages it goes through `osdesc.rs` instead,
///   and never gets here.
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
/// - 0xf1 NV_IMEX_SESSION, 0xf9 NV_MEMORY_FABRIC_IMPORT_V2, 0xfd
///   NV_MEMORY_MULTICAST_FABRIC: `pOsEvent` is an OS event by descriptor,
///   which RM looks up in the backend's event list under hClient
///   (osUserHandleToKernelPtr, os.c:1789-1815; imex_session_api.c:278,
///   mem_fabric_import_v2.c:458, mem_multicast_fabric.c:478), and none of
///   the three could work if it were translated like NV_EVENT_BUFFER's.
///   An IMEX session makes its caller the host's one IMEX daemon
///   (fabricSetImexEvent: "only one IMEX instance listening to events"),
///   holding NV_RM_CAP_SYS_FABRIC_IMEX_MGMT through `capDescriptor`, a
///   second descriptor, of a /dev/nvidia-caps node: host-wide fabric
///   management, never a VM's. A fabric import, and a multicast object made
///   from an export packet, need the client subscribed to an IMEX channel
///   (mem_fabric_import_v2.c:600, mem_multicast_fabric.c:1343), which
///   NV0000_CTRL_CMD_CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL does from a
///   /dev/nvidia-caps-imex-channels descriptor the backend never holds. A
///   prime multicast object (cuMulticastCreate) needs an NVSwitch fabric the
///   hosts this serves do not have, and its ATTACH_GPU names a GPU by yet
///   another descriptor (ctrl00fd.h). So they are refused whole rather than
///   half translated.
pub const REFUSED_ALLOC_CLASSES: [u32; 12] = [
    0x71, 0x78, 0x7e, 0x81, 0x82, 0x83, 0xc1, 0x0092, 0x9010, 0xf1, 0xf9, 0xfd,
];

/// ALLOC_MEMORY classes whose `pMemory` RM reads rather than writes.
const REFUSED_ALLOC_MEMORY_CLASSES: [u32; 4] = [0x71, 0x81, 0x82, 0x83];

/// Check a v1 RM escape's top-level block, `params` (the guest's, as sent),
/// and say which of its fields the host must not be handed as sent: the
/// plan the host's copy is built by (`Plan::declare`). `cmd` is the full
/// ioctl number the host will be called with. `Err` is the errno to answer
/// with, and nothing reaches the host.
///
/// RM_ALLOC's and RM_CONTROL's own parameter pointer is not named here:
/// `dispatch_nested` points it at the nested block or leaves it null.
/// UNMAP_MEMORY's and UPDATE_DEVICE_MAPPING_INFO's addresses are keys, not
/// pointers, and their dispatchers put the backend's own in.
pub(crate) fn rm_escape(cmd: u32, params: &[u8]) -> Result<Plan<'static>, Errno> {
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
    let mut plan = Plan::default();
    match escape {
        // NVOS30: hClient, hDevice, hChannel, numChannels, then phClients,
        // phDevices, phChannels at 16/24/32, flags at 40. RM reads the three
        // arrays only for a channel list (NVOS30_FLAGS_CHANNEL, bits 7:4, is
        // LIST and numChannels is not 0: RmDeprecatedIdleChannels); the one-
        // channel form never follows them. That is the form the Vulkan and
        // GL drivers use as they tear a device down, a dozen times a run, so
        // it goes, with the pointers zeroed; a list is still refused.
        NV_ESC_RM_IDLE_CHANNELS => {
            sized(56)?;
            let num = rd32(params, 12).unwrap_or(0);
            let flags = rd32(params, 40).unwrap_or(0);
            if (flags >> 4) & 0xf == 0 && num != 0 {
                log::warn!(
                    "RM_IDLE_CHANNELS refused: a list of {num} channels is three arrays the \
                     backend cannot give addresses of its own"
                );
                return Err(libc::EPERM);
            }
            for off in [16, 24, 32] {
                plan.zeroed(off);
            }
        }
        NV_ESC_IOCTL_XFER_CMD
        | NV_ESC_RM_I2C_ACCESS
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
            plan.zeroed(OS64_RIGHTS);
        }
        NV_ESC_RM_CONTROL => {
            sized(OS54_SIZE)?;
            let ctl = rd32(params, OS54_CMD).unwrap_or(0);
            if abi::rmctrl::refused(ctl) {
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
            plan.out(OS02_MEMORY);
        }
        NV_ESC_RM_VID_HEAP_CONTROL => {
            sized(OS32_SIZE)?;
            match rd32(params, OS32_FUNCTION).unwrap_or(0) {
                NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR => {
                    log::warn!("VID_HEAP_CONTROL ALLOC_OS_DESCRIPTOR refused (guestptr.rs)");
                    return Err(libc::EPERM);
                }
                NVOS32_FUNCTION_ALLOC_SIZE | NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT => {
                    plan.out(OS32_ALLOC_ADDRESS);
                }
                NVOS32_FUNCTION_ALLOC_SIZE_RANGE => {
                    plan.out(OS32_RANGE_ADDRESS);
                }
                NVOS32_FUNCTION_HW_ALLOC => {
                    plan.zeroed(OS32_HW_BIND);
                    plan.zeroed(OS32_HW_HANDLE);
                }
                _ => {}
            }
        }
        NV_ESC_RM_MAP_MEMORY => {
            sized(OS33_FD_SIZE)?;
            // OUT: RM writes the mapping's address (Nv04MapMemory).
            plan.out(OS33_LINEAR);
        }
        _ => {}
    }
    Ok(plan)
}

/// IDLE_CHANNELS' channel list, sent with its three arrays as deep segments
/// (`deep`, deepseg.rs): checked here against the count in `params` (the
/// guest's NVOS30, as sent); each array gets a block of the call's arena,
/// sized from that count exactly as RmDeprecatedIdleChannels sizes its
/// copies, when the plan is declared, and the caller's pointers go back in
/// the reply. The one-channel form, and a list sent without its arrays, are
/// `rm_escape`'s.
pub(crate) fn idle_channels_list<'g>(
    cmd: u32,
    params: &[u8],
    deep: &'g [u8],
) -> Result<Plan<'g>, Errno> {
    use abi::rmctrl::{
        IDLE_CHANNELS, IDLE_CHANNELS_FLAGS, IDLE_CHANNELS_LIST, IDLE_CHANNELS_LIST_BITS,
        IDLE_CHANNELS_SIZE,
    };
    if hostfd::ioc_nr(cmd) != NV_ESC_RM_IDLE_CHANNELS
        || hostfd::ioc_size(cmd) != IDLE_CHANNELS_SIZE
        || params.len() != IDLE_CHANNELS_SIZE
    {
        log::warn!("RM_IDLE_CHANNELS: {} bytes, not NVOS30's", params.len());
        return Err(libc::EINVAL);
    }
    let count = IDLE_CHANNELS.ptrs[0].counts[0].offset;
    let num = rd32(params, count).unwrap_or(0);
    let (lo, hi) = IDLE_CHANNELS_LIST_BITS;
    let channel =
        (rd32(params, IDLE_CHANNELS_FLAGS).unwrap_or(0) >> lo) & ((1 << (hi - lo + 1)) - 1);
    // Only a list reads the arrays: segments for anything else are not a
    // mistake to paper over.
    if channel != IDLE_CHANNELS_LIST || num == 0 {
        log::warn!("RM_IDLE_CHANNELS: arrays sent for a call that reads none");
        return Err(libc::EINVAL);
    }
    if num > protocol::messages::IDLE_CHANNELS_MAX {
        log::warn!(
            "RM_IDLE_CHANNELS refused: a list of {num} channels, over {}",
            protocol::messages::IDLE_CHANNELS_MAX
        );
        return Err(libc::EINVAL);
    }
    Ok(Plan {
        slots: Vec::new(),
        idle: Some(deep),
    })
}

// ───────────────────────────── RM controls ─────────────────────────────

// Every control RM dereferences a user pointer inside the parameters of,
// with the pointer offsets, and the controls whose pointers have no fixed
// place (refused).
//
// Measured, not transcribed: `gen/rmctrl_extract.py` takes the commands
// and fields from each release's embeddedParamCopyIn
// (embedded_param_copy.c), its deprecated V1 control table
// (rmapi_deprecated_control.c) and the handlers that copy user memory
// themselves (mem_mgr_ctrl.c:617; kernel_sm_debugger_session_ctrl.c's
// READ/WRITE_SURFACE, an array of per-op pointers, and
// NV402C_CTRL_CMD_I2C_TRANSACTION, whose pointer moves with a union arm,
// are the refused ones), compiles the offsets against that release's SDK
// headers, and fails if a release has an RM file copying user memory that
// none of those account for. The table is the union over 535.129.03,
// 580.178.04, 595.71.05, 595.99.02, 610.57.04 and 615.71.09 (an offset past
// a block's end is one that release lacks, and is skipped).
// (abi::rmctrl::CONTROL_POINTERS and REFUSED_CONTROLS.)

/// The pointer offsets RM follows in control `cmd`'s parameters, if any.
pub(crate) fn control_pointers(cmd: u32) -> &'static [usize] {
    abi::rmctrl::pointers(cmd).map_or(&[], |c| c.ptrs)
}

/// Declare every pointer RM would follow in control `cmd`'s parameters --
/// block `nested` of `a` -- except those at `relocated`, which already hold
/// the address of a block of the call: each is 0 for the host, and the
/// caller's value comes back in the reply.
///
/// Zero is RM's "no buffer": with a nonzero count it answers
/// NV_ERR_INVALID_ARGUMENT (param_copy.c:43-53), the status a native caller
/// with a bad pointer would get. An offset past the block's end is one this
/// release's layout does not have.
///
/// A pointer RM follows that overlaps a field already declared -- a deep
/// pointer the guest placed across it, say -- refuses the call (EINVAL):
/// what RM would read there is partly the guest's bytes and partly an
/// address of ours, which is neither. Found by the `backend_v2` target.
pub(crate) fn scrub_control(
    cmd: u32,
    a: &mut Arena,
    nested: BufId,
    relocated: &[usize],
) -> Result<(), Errno> {
    let len = a.len(nested);
    for &off in control_pointers(cmd) {
        if relocated.contains(&off) || off + 8 > len {
            continue;
        }
        match a.slot(nested, off, 8, SlotKind::Ptr, Restore::IfSet) {
            Ok(v) if v != 0 => log::warn!(
                "RM control {cmd:#010x}: pointer at {off} was not sent with the data it \
                 addresses; zeroed rather than handed to the host"
            ),
            Ok(_) => {}
            Err(_) => {
                log::warn!(
                    "RM control {cmd:#010x}: the pointer at {off} overlaps another field of \
                     the call; refused"
                );
                return Err(libc::EINVAL);
            }
        }
    }
    Ok(())
}

// ───────────────────────────── UVM ─────────────────────────────

pub const UVM_INITIALIZE: u32 = 0x3000_0001;
pub const UVM_DEINITIALIZE: u32 = 0x3000_0002;
/// UVM_PAGEABLE_MEM_ACCESS: {NvBool pageableMemAccess; NV_STATUS rmStatus;}.
pub const UVM_PAGEABLE_MEM_ACCESS: u32 = 39;

/// UVM_INIT_FLAGS_DISABLE_HMM / _DISABLE_PAGEABLE_MIGRATIONS (uvm_types.h:64-66).
pub const UVM_INIT_FLAGS_DISABLE_HMM: u64 = 0x1;
/// UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS (uvm_types.h:68): with it the VA
/// space never gets pageable access, by ATS or HMM (uvm_va_space.c:190-204).
/// New in 610.43.02; an older UVM refuses the whole call if it is set
/// (uvm_va_space_create, `flags & ~UVM_INIT_FLAGS_MASK`), so it is forced
/// only where the host's table says the release takes it. Before that,
/// DISABLE_HMM is the only switch, and ATS the other way in: the backend asks
/// UVM afterwards whether the VA space has pageable access (nvidia.rs).
pub const UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS: u64 = 0x4;
/// UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE (uvm_types.h:67): the VA space
/// is tied to no process's mm. Then any process may mmap the file
/// (uvm.c:784), which is what lets the VMM map a semaphore pool the guest
/// needs to reach (uvmmap.rs), and pageable access is off on every release
/// (uvm_va_space.c:2079, :2095, which return before looking at HMM or ATS).
/// In every measured release's mask (0x3 up to 595, 0x7 from 610.43.02).
pub const UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE: u64 = 0x2;

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
    // Range groups, until 610.43.02 removed them (CUDA before that creates
    // one in cuInit): group ids, and spans that must be UVM's own managed
    // ranges end to end (uvm_range_group.c, uvm_api_set_range_group).
    23, // CREATE_RANGE_GROUP
    24, // DESTROY_RANGE_GROUP
    31, // SET_RANGE_GROUP
    40, // PREVENT_MIGRATION_RANGE_GROUPS (at most 32 ids, inline)
    41, // ALLOW_MIGRATION_RANGE_GROUPS
    53, // MIGRATE_RANGE_GROUP
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
///
/// `init_flags_mask` is the host release's UVM_INIT_FLAGS_MASK: of the
/// flags that turn pageable access off, UVM_INITIALIZE gets those it takes.
pub(crate) fn uvm_gate(
    tools: bool,
    cmd: u32,
    params: &[u8],
    init_flags_mask: u64,
) -> Result<Plan<'static>, Errno> {
    if tools {
        log::warn!("UVM tools ioctl {cmd:#x} refused: the tools device pins user buffers");
        return Err(libc::EPERM);
    }
    if !UVM_ALLOWED.contains(&cmd) {
        log::warn!("UVM ioctl {cmd:#x} refused (guestptr.rs)");
        return Err(libc::EPERM);
    }
    let mut plan = Plan::default();
    if cmd == UVM_INITIALIZE {
        // UVM_INITIALIZE_PARAMS {NvU64 flags; NV_STATUS rmStatus;}
        let Some(flags) = rd64(params, 0) else {
            return Err(libc::EINVAL);
        };
        // Sharing mode on top: one VA-space shape for every guest, whether or
        // not it maps a pool, and it only takes things away (pageable access,
        // the tie to our mm). MM_INITIALIZE then answers
        // NV_WARN_NOTHING_TO_DO (uvm.c:80-84), which CUDA takes in its stride.
        let off = (UVM_INIT_FLAGS_DISABLE_PAGEABLE_ACCESS
            | UVM_INIT_FLAGS_DISABLE_HMM
            | UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE)
            & init_flags_mask;
        if off & UVM_INIT_FLAGS_DISABLE_HMM == 0 {
            log::warn!("UVM_INITIALIZE refused: the host's UVM cannot be told to leave HMM off");
            return Err(libc::EPERM);
        }
        // Bits the host's UVM does not know go no further: it would refuse
        // the whole call for one (a guest built for a newer release may ask
        // for DISABLE_PAGEABLE_ACCESS by name), and what they could ask for
        // is what the backend has just decided. The caller reads its own
        // flags back either way.
        let forced = (flags | off) & init_flags_mask;
        plan.slots.push((
            0,
            TopSlot::Forced {
                value: forced,
                width: 8,
            },
        ));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfd::{IOC_RW, ioc};
    use abi::rmctrl::{CONTROL_POINTERS, REFUSED_CONTROLS};

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

    /// What the host is handed, and what the caller reads back, of block
    /// `p` built under `plan` (the host writing nothing).
    fn built(plan: &Plan<'_>, p: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut a = Arena::new();
        let top = a.block(p, p.len()).unwrap();
        plan.declare(&mut a, top, &|_| None).unwrap();
        (a.bytes(top).to_vec(), a.reply(top))
    }

    /// The same for a control's parameters `n`, scrubbed.
    fn scrubbed(cmd: u32, n: &[u8], relocated: &[usize]) -> (Vec<u8>, Vec<u8>) {
        let mut a = Arena::new();
        let b = a.block(n, n.len()).unwrap();
        scrub_control(cmd, &mut a, b, relocated).unwrap();
        (a.bytes(b).to_vec(), a.reply(b))
    }

    #[test]
    fn escapes_with_pointers_the_backend_cannot_relocate_are_refused() {
        for (nr, size) in [
            (NV_ESC_IOCTL_XFER_CMD, 16),
            (NV_ESC_RM_I2C_ACCESS, 32),
            (NV_ESC_RM_ACCESS_REGISTRY, 72),
            (NV_ESC_RM_GET_EVENT_DATA, 16),
            (NV_ESC_RM_ADD_VBLANK_CALLBACK, 32),
        ] {
            let p = vec![0x11u8; size];
            assert_eq!(
                rm_escape(ioc(IOC_RW, b'F', nr, size), &p),
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
        let (host, reply) = built(&rm_escape(ALLOC, &p).unwrap(), &p);
        assert_eq!(rd64(&host, OS64_RIGHTS), Some(0));
        assert_eq!(rd64(&reply, OS64_RIGHTS), Some(0x7fff_dead_b000));
    }

    #[test]
    fn classes_that_hand_rm_a_cpu_address_or_a_function_are_never_allocated() {
        for class in REFUSED_ALLOC_CLASSES {
            let mut p = vec![0u8; 48];
            put32(&mut p, OS64_CLASS, class);
            assert_eq!(rm_escape(ALLOC, &p), Err(libc::EPERM), "class {class:#x}");
        }
        let mut p = vec![0u8; 48];
        put32(&mut p, OS64_CLASS, 0x3e);
        assert!(rm_escape(ALLOC, &p).is_ok(), "plain system memory");
    }

    /// IMEX and fabric memory name an OS event by descriptor (pOsEvent), and
    /// what they are for -- the host's IMEX daemon, memory over an NVLink
    /// fabric -- is not a VM's to have: none reaches the host.
    #[test]
    fn imex_sessions_and_fabric_memory_are_never_allocated() {
        for class in [0xf1, 0xf9, 0xfd] {
            let mut p = vec![0u8; 48];
            put32(&mut p, OS64_CLASS, class);
            assert_eq!(rm_escape(ALLOC, &p), Err(libc::EPERM), "class {class:#x}");
        }
    }

    #[test]
    fn an_rm_alloc_of_another_size_is_not_guessed_at() {
        let p = vec![0u8; 32];
        assert_eq!(
            rm_escape(ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 32), &p),
            Err(libc::EINVAL)
        );
        let p = vec![0u8; 48];
        assert_eq!(
            rm_escape(ioc(IOC_RW, b'F', NV_ESC_RM_ALLOC, 32), &p),
            Err(libc::EINVAL),
            "the host reads by the command's size"
        );
    }

    #[test]
    fn os_descriptor_memory_is_refused_on_every_path() {
        let mut p = vec![0u8; 56];
        put32(&mut p, OS02_CLASS, NV01_MEMORY_SYSTEM_OS_DESCRIPTOR);
        put64(&mut p, OS02_MEMORY, 0x7f00_0000_0000);
        assert_eq!(rm_escape(ALLOC_MEMORY, &p), Err(libc::EPERM));

        let mut p = vec![0u8; 184];
        put32(&mut p, OS32_FUNCTION, NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR);
        assert_eq!(rm_escape(VID_HEAP, &p), Err(libc::EPERM));

        for class in 0x81..=0x83 {
            let mut p = vec![0u8; 56];
            put32(&mut p, OS02_CLASS, class);
            assert_eq!(rm_escape(ALLOC_MEMORY, &p), Err(libc::EPERM), "{class:#x}");
        }
    }

    #[test]
    fn output_addresses_go_to_the_host_as_zero() {
        let mut p = vec![0u8; 56];
        put32(&mut p, OS02_CLASS, 0x3e);
        put64(&mut p, OS02_MEMORY, 0x1234_5000);
        let plan = rm_escape(ALLOC_MEMORY, &p).unwrap();
        assert_eq!(
            plan.slots,
            [(OS02_MEMORY, TopSlot::Out)],
            "the host's answer goes back"
        );
        assert_eq!(rd64(&built(&plan, &p).0, OS02_MEMORY), Some(0));

        let mut p = vec![0u8; 56];
        put64(&mut p, OS33_LINEAR, 0x1234_5000);
        let plan = rm_escape(MAP_MEMORY, &p).unwrap();
        assert_eq!(rd64(&built(&plan, &p).0, OS33_LINEAR), Some(0));

        for (f, off) in [
            (NVOS32_FUNCTION_ALLOC_SIZE, OS32_ALLOC_ADDRESS),
            (NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT, OS32_ALLOC_ADDRESS),
            (NVOS32_FUNCTION_ALLOC_SIZE_RANGE, OS32_RANGE_ADDRESS),
        ] {
            let mut p = vec![0u8; 184];
            put32(&mut p, OS32_FUNCTION, f);
            put64(&mut p, off, 0x1234_5000);
            let plan = rm_escape(VID_HEAP, &p).unwrap();
            assert_eq!(rd64(&built(&plan, &p).0, off), Some(0), "function {f}");
        }
    }

    #[test]
    fn hw_alloc_pointers_are_zeroed_and_given_back() {
        let mut p = vec![0u8; 184];
        put32(&mut p, OS32_FUNCTION, NVOS32_FUNCTION_HW_ALLOC);
        put64(&mut p, OS32_HW_BIND, 0xaaaa);
        put64(&mut p, OS32_HW_HANDLE, 0xbbbb);
        let (host, reply) = built(&rm_escape(VID_HEAP, &p).unwrap(), &p);
        assert_eq!(
            (rd64(&host, OS32_HW_BIND), rd64(&host, OS32_HW_HANDLE)),
            (Some(0), Some(0))
        );
        assert_eq!(
            (rd64(&reply, OS32_HW_BIND), rd64(&reply, OS32_HW_HANDLE)),
            (Some(0xaaaa), Some(0xbbbb))
        );
    }

    #[test]
    fn a_heap_function_without_pointers_is_left_alone() {
        let mut p = vec![0x5au8; 184];
        put32(&mut p, OS32_FUNCTION, 3); // FREE
        let plan = rm_escape(VID_HEAP, &p).unwrap();
        assert!(plan.slots.is_empty());
        assert_eq!(built(&plan, &p).0, p);
    }

    #[test]
    fn controls_whose_pointers_have_no_fixed_place_are_refused() {
        for &(ctl, _) in REFUSED_CONTROLS {
            let mut p = vec![0u8; 32];
            put32(&mut p, OS54_CMD, ctl);
            assert_eq!(rm_escape(CONTROL, &p), Err(libc::EPERM), "{ctl:#x}");
        }
    }

    #[test]
    fn a_control_pointer_not_sent_with_its_data_is_zeroed_and_given_back() {
        // NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION: three pointers.
        let mut n = vec![0u8; 32];
        put64(&mut n, 8, 0x1000);
        put64(&mut n, 16, 0x2000);
        put64(&mut n, 24, 0x3000);
        let (host, reply) = scrubbed(0x101, &n, &[16]);
        assert_eq!(
            (rd64(&host, 8), rd64(&host, 16), rd64(&host, 24)),
            (Some(0), Some(0x2000), Some(0))
        );
        assert_eq!(
            (rd64(&reply, 8), rd64(&reply, 24)),
            (Some(0x1000), Some(0x3000))
        );
    }

    #[test]
    fn a_control_rm_follows_no_pointer_in_is_left_alone() {
        let n = vec![0x77u8; 64];
        assert_eq!(scrubbed(0x20800a01, &n, &[]), (n.clone(), n));
    }

    #[test]
    fn an_offset_past_the_block_is_a_field_this_release_lacks() {
        // 535's GET_P2P_CAPS ends at busPeerIds.
        let mut n = vec![0u8; 168];
        put64(&mut n, 160, 0x4000);
        assert_eq!(rd64(&scrubbed(0x127, &n, &[]).0, 160), Some(0));
    }

    #[test]
    fn every_pointer_offset_is_aligned_and_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for c in CONTROL_POINTERS {
            assert!(seen.insert(c.cmd), "{:#x} twice", c.cmd);
            assert!(!abi::rmctrl::refused(c.cmd));
            for &o in c.ptrs {
                assert_eq!(o % 8, 0, "{:#x} at {o}", c.cmd);
            }
        }
    }

    #[test]
    fn uvm_is_initialised_without_pageable_access_whatever_the_guest_asks() {
        let mut p = vec![0u8; 16];
        put64(&mut p, 0, 0x2); // MULTI_PROCESS_SHARING_MODE
        let (host, reply) = built(&uvm_gate(false, UVM_INITIALIZE, &p, 0x7).unwrap(), &p);
        assert_eq!(rd64(&host, 0), Some(0x7));
        assert_eq!(
            rd64(&reply, 0),
            Some(0x2),
            "the caller reads back its own flags"
        );
    }

    /// A UVM before 610.43.02 refuses UVM_INITIALIZE outright if any bit
    /// outside its mask is set: only DISABLE_HMM is forced there.
    #[test]
    fn uvm_is_only_given_the_flags_its_release_takes() {
        let mut p = vec![0u8; 16];
        put64(&mut p, 0, 0x2);
        let (host, _) = built(&uvm_gate(false, UVM_INITIALIZE, &p, 0x3).unwrap(), &p);
        assert_eq!(rd64(&host, 0), Some(0x3));
        // A guest asking for DISABLE_PAGEABLE_ACCESS by name is not handed
        // to a UVM that would refuse the call for it.
        let mut p = vec![0u8; 16];
        put64(&mut p, 0, 0x6);
        let (host, reply) = built(&uvm_gate(false, UVM_INITIALIZE, &p, 0x3).unwrap(), &p);
        assert_eq!(rd64(&host, 0), Some(0x3));
        assert_eq!(rd64(&reply, 0), Some(0x6));
        // One that cannot even leave HMM off is not initialised at all.
        let p = vec![0u8; 16];
        assert_eq!(uvm_gate(false, UVM_INITIALIZE, &p, 0x2), Err(libc::EPERM));
    }

    /// Multi-process sharing mode is forced wherever the release takes it,
    /// whatever the guest asked, and the guest reads back its own flags.
    #[test]
    fn uvm_is_initialised_in_sharing_mode_where_the_release_has_it() {
        for (mask, want) in [(0x3, 0x3), (0x7, 0x7)] {
            let p = vec![0u8; 16];
            let (host, reply) = built(&uvm_gate(false, UVM_INITIALIZE, &p, mask).unwrap(), &p);
            assert_eq!(rd64(&host, 0), Some(want), "mask {mask:#x}");
            assert_ne!(want & UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE, 0);
            assert_eq!(
                rd64(&reply, 0),
                Some(0),
                "the caller reads back its own flags"
            );
        }
        // A release without it (none measured) is not handed the bit.
        let p = vec![0u8; 16];
        let (host, _) = built(&uvm_gate(false, UVM_INITIALIZE, &p, 0x1).unwrap(), &p);
        assert_eq!(rd64(&host, 0), Some(0x1));
    }

    /// Every measured release takes sharing mode: the UVM aperture depends
    /// on it (uvmmap.rs), and a release without it would quietly lose CUDA.
    #[test]
    fn every_measured_release_takes_sharing_mode() {
        for t in abi::schema::UVM_TABLES {
            assert_ne!(
                t.init_flags_mask & UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE,
                0,
                "{}",
                t.name
            );
        }
    }

    #[test]
    fn uvm_commands_that_touch_cpu_memory_are_refused() {
        for cmd in [56, 62, 63, 64, 71, 76, 77, 81, 13, 16, 21, 35, 200, 0x7ff] {
            let p = vec![0u8; 64];
            assert_eq!(uvm_gate(false, cmd, &p, 0x7), Err(libc::EPERM), "{cmd}");
        }
        for cmd in [33, 37, 51, 73, 75] {
            let p = vec![0u8; 64];
            assert!(uvm_gate(false, cmd, &p, 0x7).is_ok(), "{cmd}");
        }
    }

    /// The generated UVM table (gen/uvm_extract.py's COMMANDS) sizes exactly
    /// the commands let through here: one with no size would be refused by
    /// both halves anyway, and one sized but not let through is a list that
    /// drifted.
    #[test]
    fn every_uvm_command_let_through_has_a_measured_size_and_nothing_else_does() {
        use std::collections::BTreeSet;
        let allowed: BTreeSet<u32> = UVM_ALLOWED.iter().copied().collect();
        let tables = abi::schema::UVM_TABLES;
        // Some commands are only in older releases (range groups), so it is
        // every release's commands together that must be the list.
        let measured: BTreeSet<u32> = tables
            .iter()
            .flat_map(|t| t.cmds.iter().map(|c| c.cmd))
            .collect();
        assert_eq!(allowed, measured);
        for t in tables {
            for c in t.cmds {
                assert!(allowed.contains(&c.cmd), "{} {}", t.name, c.name);
            }
        }
    }

    #[test]
    fn nothing_goes_to_the_uvm_tools_device() {
        let p = vec![0u8; 64];
        assert_eq!(uvm_gate(true, 67, &p, 0x7), Err(libc::EPERM));
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
    use protocol::messages::{
        DEEP_SEGMENTED, DeviceKind, IDLE_CHANNELS_MAX, IoctlResp, MsgHeader, MsgType,
    };
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
        /// UVM_PAGEABLE_MEM_ACCESS, the backend's own question.
        UvmPageable,
        /// UNMAP_MEMORY: pLinearAddress; UPDATE_DEVICE_MAPPING_INFO: pOld,
        /// pNew. Keys, not pointers, but addresses in this process all the
        /// same.
        Addresses(Vec<u64>),
        Other(u64),
        /// RM's own copies through a call's arrays, as RM makes them: for
        /// FIFO_GET_CHANNELLIST (control 0x80170d) and IDLE_CHANNELS (escape
        /// 0x41), each pointer and the `count` u32s read behind it (none
        /// for a null one).
        Lists {
            call: u32,
            arrays: Vec<(u64, Vec<u32>)>,
        },
    }

    std::thread_local! {
        static SEEN: RefCell<Vec<Seen>> = const { RefCell::new(Vec::new()) };
        /// What the fake's UVM_PAGEABLE_MEM_ACCESS answers.
        static PAGEABLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn seen() -> Vec<Seen> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn word64(a: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(a[at..at + 8].try_into().unwrap())
    }

    /// Read 8 bytes behind a pointer the backend handed the host. A guest
    /// address fails the assertion before anything is read.
    fn behind(o: &crate::sys::block::Others<'_>, p: u64) -> u64 {
        assert!(
            p != GUEST_PTR && p != GUEST_PTR2,
            "a guest address reached the host"
        );
        // A nonzero pointer the backend put in the block: a block of the
        // call's own (peek fails on anything else).
        o.peek(p, 8)
    }

    /// NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST.
    const CHANNELLIST: u32 = 0x0080_170d;

    /// Read `count` u32s behind a pointer the backend handed the host, as RM
    /// copies them in.
    fn read_u32s(o: &crate::sys::block::Others<'_>, p: u64, count: usize) -> Vec<u32> {
        assert!(
            p != GUEST_PTR && p != GUEST_PTR2,
            "a guest address reached the host"
        );
        // A nonzero pointer the backend put in the block, and `count` is the
        // size RM copies, which the backend sized that block to.
        let b = o
            .read(p, 4 * count)
            .expect("a block of the call, as long as RM copies");
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect()
    }

    fn fake_host(
        _fd: std::os::fd::RawFd,
        request: u64,
        arg: &mut crate::sys::block::Arg<'_>,
    ) -> i32 {
        let request = request as u32;
        // At least _IOC_SIZE bytes; UVM's plain numbers carry the guest's
        // block, 16 bytes or fewer.
        let (a, mut others) = arg.split();
        let len = hostfd::ioc_size(request).max(16).min(a.len());
        let a = &mut a[..len];
        let s = match (hostfd::ioc_type(request), hostfd::ioc_nr(request)) {
            _ if request == UVM_INITIALIZE => Seen::UvmInit {
                flags: word64(a, 0),
            },
            _ if request == UVM_PAGEABLE_MEM_ACCESS => {
                a[0] = PAGEABLE.with(|p| p.get()) as u8;
                a[4..8].fill(0);
                Seen::UvmPageable
            }
            (b'F', 0x2a) if word64(a, 8) as u32 == CHANNELLIST => {
                // RM: numChannels u32s in through both lists, and the
                // channel list back out (embedded_param_copy.c:292-306).
                let params = word64(a, 16);
                assert!(params != 0 && params != GUEST_PTR);
                // The backend's nested block, 24 bytes.
                let n = others.read(params, 24).expect("the nested block");
                let count = u32::from_le_bytes(n[..4].try_into().unwrap()) as usize;
                let arrays = [8, 16]
                    .map(|off| {
                        let p = word64(&n, off);
                        (
                            p,
                            if p == 0 {
                                vec![]
                            } else {
                                read_u32s(&others, p, count)
                            },
                        )
                    })
                    .to_vec();
                let out = word64(&n, 16);
                if out != 0 {
                    for i in 0..count {
                        // As read_u32s: RM's own size, in our block.
                        others.poke(out + 4 * i as u64, 4, u64::from(0xc0de_0000 + i as u32));
                    }
                }
                a[28..32].fill(0);
                Seen::Lists {
                    call: CHANNELLIST,
                    arrays,
                }
            }
            (b'F', 0x2a) => {
                let params = word64(a, 16);
                let size = u32::from_le_bytes(a[24..28].try_into().unwrap());
                let mut inner = Vec::new();
                if params != 0 {
                    assert!(params != GUEST_PTR, "the guest's params pointer reached RM");
                    // The backend's nested block, `size` bytes.
                    let n = others
                        .read(params, size as usize)
                        .expect("the nested block");
                    for off in [8, 16, 24] {
                        if off + 8 <= n.len() {
                            let p = word64(&n, off);
                            inner.push((p, (p != 0).then(|| behind(&others, p))));
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
                    first: (params != 0).then(|| behind(&others, params)),
                    rights: word64(a, 24),
                };
                a[40..44].fill(0);
                s
            }
            (b'd', 0x41) => Seen::Gem { ptr: word64(a, 8) },
            (b'F', 0x41) => {
                let count = u32::from_le_bytes(a[12..16].try_into().unwrap()) as usize;
                let list = (u32::from_le_bytes(a[40..44].try_into().unwrap()) >> 4) & 0xf == 0;
                if list && count != 0 {
                    // RmDeprecatedIdleChannels: numChannels u32s in through
                    // each of the three.
                    Seen::Lists {
                        call: 0x41,
                        arrays: [16, 24, 32]
                            .map(|off| {
                                let p = word64(a, off);
                                (
                                    p,
                                    if p == 0 {
                                        vec![]
                                    } else {
                                        read_u32s(&others, p, count)
                                    },
                                )
                            })
                            .to_vec(),
                    }
                } else {
                    for off in [16, 24, 32] {
                        assert_eq!(
                            word64(a, off),
                            0,
                            "IDLE_CHANNELS pointer at {off} reached RM"
                        );
                    }
                    Seen::Other(request as u64)
                }
            }
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
        let (st, _, body) = v1_resp(be, handle, cmd, outer, nested, deep);
        (st, body)
    }

    /// As `v1`, with the reply's IoctlResp.
    fn v1_resp(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        deep: Option<(u32, &[u8])>,
    ) -> (i32, IoctlResp, Vec<u8>) {
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
        let mut r = IoctlResp::default();
        if n >= body {
            let w = |i: usize| {
                let at = size_of::<MsgHeader>() + 4 * i;
                u32::from_le_bytes(resp[at..at + 4].try_into().unwrap())
            };
            (r.data_len, r.nested_len, r.deep_len) = (w(0), w(1), w(2));
        }
        (status, r, resp[body.min(n)..n].to_vec())
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
            // GPU_ACQUIRE_COMPUTE_MODE_RESERVATION: no parameters.
            &nvos54(0x2080_0145, GUEST_PTR, 0),
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
            &nvos54(0x2080_0145, GUEST_PTR, 64),
            &[],
            None,
        );
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    /// Fuzzing (`backend_v2`, `dind`): a deep pointer the guest places
    /// across a pointer RM follows (FIFO_GET_CHANNELLIST's at 8 and 16, the
    /// deep one at 12) would have RM read four bytes of the guest's and four
    /// of our address as one pointer. 12 is no pointer RM follows, so the
    /// block is not relocated at all (review 2026-09-26, backend 7): RM
    /// reads each of its two pointers as 0, no buffer, and nothing of ours.
    #[test]
    fn a_deep_pointer_across_a_pointer_rm_follows_never_reaches_rm() {
        let (mut be, h) = ctl();
        let mut nested = vec![0u8; 24];
        nested[..4].copy_from_slice(&2u32.to_le_bytes());
        nested[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        nested[16..24].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        let (st, _) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(CHANNELLIST, GUEST_PTR, 24),
            &nested,
            Some((12, &[0u8; 8])),
        );
        assert_eq!(st, 0);
        assert!(
            matches!(&seen()[..], [Seen::Lists { call: CHANNELLIST, arrays }]
                if arrays == &[(0, vec![]), (0, vec![])]),
            "RM saw two null pointers"
        );
    }

    /// Fuzzing (`backend_v2`): FIFO_GET_CHANNELLIST's parameters sent 23
    /// bytes long with paramsSize 24. The scrub read the 23 bytes, found no
    /// room for the pointer at 16 and left it; RM reads 24, the last from our
    /// buffer's zeroed slack, and followed seven bytes of the guest's as an
    /// address here -- copying the handles in from it and the channel list
    /// out to it. A size the host would copy that is not what was sent never
    /// reaches the host, either way round.
    #[test]
    fn a_block_shorter_or_longer_than_the_size_rm_copies_never_reaches_rm() {
        let (mut be, h) = ctl();
        let mut nested = vec![0u8; 24];
        nested[..4].copy_from_slice(&3u32.to_le_bytes());
        nested[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        for (sent, size) in [(23, 24), (17, 24), (24, 23), (24, 32)] {
            let (st, _) = v1(
                &mut be,
                h,
                CONTROL,
                &nvos54(CHANNELLIST, GUEST_PTR2, size),
                &nested[..sent],
                None,
            );
            assert_eq!(st, -libc::EINVAL, "{sent} bytes sent, paramsSize {size}");
            assert!(seen().is_empty(), "{sent} bytes sent, paramsSize {size}");
        }
        // Sent whole, the pointer is RM's to follow only as a buffer of ours,
        // or not at all.
        let (st, _) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(CHANNELLIST, GUEST_PTR2, 24),
            &nested,
            None,
        );
        assert_eq!(st, 0);
        match seen().as_slice() {
            [Seen::Lists { arrays, .. }] => {
                assert!(arrays.iter().all(|(p, _)| *p != GUEST_PTR));
            }
            other => panic!("{other:?}"),
        }
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

    /// FIFO_GET_CHANNELLIST's parameters: numChannels, the handle list at 8
    /// and the channel list at 16.
    fn channellist(count: u32) -> Vec<u8> {
        let mut n = vec![0u8; 24];
        n[..4].copy_from_slice(&count.to_le_bytes());
        n[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        n[16..24].copy_from_slice(&GUEST_PTR2.to_le_bytes());
        n
    }

    fn u32s(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_le_bytes()).collect()
    }

    /// The v1 path an old guest takes: one pointer sent with what it
    /// addresses (the single deep block), every other pointer RM follows
    /// zeroed, and the caller's own values back in the reply.
    #[test]
    fn every_pointer_rm_follows_in_a_control_is_ours_or_null() {
        let (mut be, h) = ctl();
        let deep = u32s(&[7, 8]);
        let (st, reply) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(CHANNELLIST, GUEST_PTR, 24),
            &channellist(2),
            Some((16, &deep)),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [Seen::Lists { arrays, .. }] => {
                assert_eq!(arrays[0], (0, vec![]), "the handle list, not sent, is null");
                assert_ne!(arrays[1].0, 0);
                assert_eq!(arrays[1].1, vec![7, 8], "our copy of the channel list");
            }
            other => panic!("{other:?}"),
        }
        // The caller's own pointers come back, in the outer block and in
        // the nested one, and the one list with what RM wrote.
        assert_eq!(word64(&reply, 16), GUEST_PTR);
        assert_eq!(&reply[32..32 + 24], &channellist(2)[..]);
        assert_eq!(&reply[32 + 24..], &u32s(&[0xc0de_0000, 0xc0de_0001])[..]);
    }

    /// Both of FIFO_GET_CHANNELLIST's lists, as deep segments: each reaches
    /// RM as a buffer of ours holding exactly what the guest sent, RM copies
    /// numChannels u32s through each, and the reply carries the segments
    /// back, laid out as sent, with the channel list as RM wrote it. This is
    /// the call cuCtxCreate makes; with the lists zeroed RM refused it.
    #[test]
    fn each_list_of_a_control_reaches_rm_as_our_buffer_holding_what_the_guest_sent() {
        let (mut be, h) = ctl();
        let handles = u32s(&[0xcafe_0001, 0xcafe_0002, 0xcafe_0003]);
        let deep = crate::deepseg::build(&[(8, &handles), (16, &[0; 12])]);
        let (st, resp, reply) = v1_resp(
            &mut be,
            h,
            CONTROL,
            &nvos54(CHANNELLIST, GUEST_PTR, 24),
            &channellist(3),
            Some((DEEP_SEGMENTED, &deep)),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [Seen::Lists { arrays, .. }] => {
                assert_eq!(arrays[0].1, vec![0xcafe_0001, 0xcafe_0002, 0xcafe_0003]);
                assert_eq!(arrays[1].1, vec![0, 0, 0]);
                let (a, b) = (arrays[0].0, arrays[1].0);
                assert!(a != 0 && b != 0 && a.abs_diff(b) >= 12, "two buffers");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!((resp.data_len, resp.nested_len), (32, 24));
        assert_eq!(resp.deep_len as usize, deep.len());
        assert_eq!(word64(&reply, 16), GUEST_PTR);
        assert_eq!(&reply[32..56], &channellist(3)[..], "the caller's pointers");
        let back = &reply[56..];
        assert_eq!(&back[..24], &deep[..24], "the table as sent");
        assert_eq!(&back[24..36], &handles[..]);
        assert_eq!(
            &back[36..],
            &u32s(&[0xc0de_0000, 0xc0de_0001, 0xc0de_0002])[..]
        );
    }

    /// The backend sizes every segment itself, from the parameters RM is
    /// handed, and a guest that says otherwise -- a short list, a long one,
    /// a pointer RM does not follow, a count it changed -- reaches nothing.
    #[test]
    fn deep_segments_that_are_not_rms_size_never_reach_rm() {
        let (mut be, h) = ctl();
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        for deep in [
            crate::deepseg::build(&[(8, &[0; 8]), (16, &[0; 12])]),
            crate::deepseg::build(&[(8, &[0; 12]), (16, &[0; 16])]),
            crate::deepseg::build(&[(8, &[0; 12]), (0, &[0; 12])]),
            crate::deepseg::build(&[(8, &[0; 12]), (8, &[0; 12])]),
        ] {
            let (st, _) = v1(
                &mut be,
                h,
                CONTROL,
                &nvos54(CHANNELLIST, GUEST_PTR, 24),
                &channellist(3),
                Some((DEEP_SEGMENTED, &deep)),
            );
            assert_eq!(st, -libc::EINVAL, "{deep:?}");
        }
        // Segments for a control whose pointers are never relocated (an
        // ACPI method), a single deep block for one, and segments on a call
        // that is not a control.
        let mut acpi = vec![0u8; 40];
        acpi[8..16].copy_from_slice(&GUEST_PTR.to_le_bytes());
        acpi[16..18].copy_from_slice(&4u16.to_le_bytes());
        let deep = crate::deepseg::build(&[(8, &[0; 4])]);
        let (st, _) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(0x130, GUEST_PTR, 40),
            &acpi,
            Some((DEEP_SEGMENTED, &deep)),
        );
        assert_eq!(st, -libc::EINVAL);
        let (st, _) = v1(
            &mut be,
            h,
            CONTROL,
            &nvos54(0x130, GUEST_PTR, 40),
            &acpi,
            Some((8, &[0; 4])),
        );
        assert_eq!(st, -libc::EINVAL);
        let mut os64 = vec![0u8; 48];
        os64[12..16].copy_from_slice(&0x3eu32.to_le_bytes());
        let (st, _) = v1(
            &mut be,
            h,
            ALLOC,
            &os64,
            &[0; 16],
            Some((DEEP_SEGMENTED, &crate::deepseg::build(&[(8, &[0; 8])]))),
        );
        assert_eq!(st, -libc::EINVAL);
        assert!(seen().is_empty());
    }

    /// NVOS30 for a list of `count` channels, its arrays at the guest's
    /// addresses.
    fn idle_list(count: u32) -> Vec<u8> {
        let mut p = vec![0u8; 56];
        p[12..16].copy_from_slice(&count.to_le_bytes());
        for off in [16, 24, 32] {
            p[off..off + 8].copy_from_slice(&GUEST_PTR.to_le_bytes());
        }
        p
    }

    /// IDLE_CHANNELS for a list, sent with its three arrays: each reaches RM
    /// as a buffer of ours holding what the guest sent, numChannels u32s as
    /// RmDeprecatedIdleChannels copies, and the caller reads its pointers
    /// back. The Vulkan and GL drivers do this as a device goes.
    #[test]
    fn idle_channels_for_a_list_carries_its_three_arrays() {
        let (mut be, h) = ctl();
        let idle = ioc(IOC_RW, b'F', NV_ESC_RM_IDLE_CHANNELS, 56);
        let (c, d, ch) = (u32s(&[1, 1, 1]), u32s(&[2, 2, 2]), u32s(&[10, 11, 12]));
        let deep = crate::deepseg::build(&[(16, &c), (24, &d), (32, &ch)]);
        let (st, reply) = v1(
            &mut be,
            h,
            idle,
            &idle_list(3),
            &[],
            Some((DEEP_SEGMENTED, &deep)),
        );
        assert_eq!(st, 0);
        match &seen()[..] {
            [Seen::Lists { call: 0x41, arrays }] => {
                let got: Vec<_> = arrays.iter().map(|(_, v)| v.clone()).collect();
                assert_eq!(got, vec![vec![1, 1, 1], vec![2, 2, 2], vec![10, 11, 12]]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(&reply[..56], &idle_list(3)[..], "the caller's pointers");

        // Sizes the backend did not compute, a list over the bound, and
        // arrays for the one-channel form, which reads none.
        for (p, deep) in [
            (
                idle_list(3),
                crate::deepseg::build(&[(16, &c), (24, &d), (32, &[0; 8])]),
            ),
            (
                idle_list(IDLE_CHANNELS_MAX + 1),
                crate::deepseg::build(&[(16, &[0; 4])]),
            ),
            (
                {
                    let mut p = idle_list(1);
                    p[40..44].copy_from_slice(&0x10u32.to_le_bytes());
                    p
                },
                crate::deepseg::build(&[(16, &[0; 4])]),
            ),
        ] {
            let (st, _) = v1(&mut be, h, idle, &p, &[], Some((DEEP_SEGMENTED, &deep)));
            assert_eq!(st, -libc::EINVAL);
        }
        assert!(seen().is_empty());
        // A list with no arrays sent is still refused (an old guest).
        let (st, _) = v1(&mut be, h, idle, &idle_list(3), &[], None);
        assert_eq!(st, -libc::EPERM);
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
        // IDLE_CHANNELS with a channel list (flags' CHANNEL field 0, LIST).
        let idle = ioc(IOC_RW, b'F', NV_ESC_RM_IDLE_CHANNELS, 56);
        let mut p = vec![0u8; 56];
        p[12..16].copy_from_slice(&1u32.to_le_bytes());
        p[16..24].copy_from_slice(&GUEST_PTR.to_le_bytes());
        assert_eq!(v1(&mut be, h, idle, &p, &[], None).0, -libc::EPERM);
        assert!(seen().is_empty());
    }

    /// IDLE_CHANNELS for one channel never follows its three array
    /// pointers: it goes, with them zeroed, and the caller reads back its own.
    #[test]
    fn idle_channels_for_one_channel_goes_without_its_pointers() {
        let (mut be, h) = ctl();
        let idle = ioc(IOC_RW, b'F', NV_ESC_RM_IDLE_CHANNELS, 56);
        let mut p = vec![0u8; 56];
        p[12..16].copy_from_slice(&1u32.to_le_bytes());
        for off in [16, 24, 32] {
            p[off..off + 8].copy_from_slice(&GUEST_PTR.to_le_bytes());
        }
        p[40..44].copy_from_slice(&0x10u32.to_le_bytes()); // CHANNEL_SINGLE
        let (st, reply) = v1(&mut be, h, idle, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(seen().len(), 1, "it reached the host");
        for off in [16, 24, 32] {
            assert_eq!(
                word64(&reply, off),
                GUEST_PTR,
                "the caller's pointer at {off}"
            );
        }
    }

    #[test]
    fn uvm_reaches_the_host_without_pageable_access_and_answers_with_the_callers_flags() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::Uvm));
        // UVM blocks are sized by the host's release (nvidia.rs, uvm_size_ok).
        be.set_host_driver_version("610.57.04");
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

    /// Before 610.43.02 UVM refuses DISABLE_PAGEABLE_ACCESS (the whole call
    /// fails with NV_ERR_INVALID_ARGUMENT, and CUDA with it): the backend
    /// forces DISABLE_HMM alone and asks UVM whether pageable access is off.
    #[test]
    fn uvm_before_610_43_is_initialised_with_its_own_flags_and_checked() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::Uvm));
        be.set_host_driver_version("595.99.02");
        let mut p = vec![0u8; 16];
        p[0..8].copy_from_slice(&0x2u64.to_le_bytes());
        let (st, reply) = v1(&mut be, h, UVM_INITIALIZE, &p, &[], None);
        assert_eq!(st, 0);
        assert_eq!(
            seen(),
            vec![Seen::UvmInit { flags: 0x3 }, Seen::UvmPageable]
        );
        assert_eq!(
            word64(&reply, 0),
            0x2,
            "the caller reads back its own flags"
        );
        assert_eq!(&reply[8..12], &[0; 4], "NV_OK");
        // And the file takes the next call.
        assert_eq!(v1(&mut be, h, 39, &[0u8; 8], &[], None).0, 0);
        assert_eq!(seen(), vec![Seen::UvmPageable]);
    }

    /// If UVM says the VA space has pageable access anyway, the guest reads
    /// NV_ERR_NOT_SUPPORTED, the file takes nothing more, and no other call
    /// may name it.
    #[test]
    fn a_uvm_file_with_pageable_access_is_refused() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::Uvm));
        be.set_host_driver_version("595.99.02");
        PAGEABLE.with(|p| p.set(true));
        let (st, reply) = v1(&mut be, h, UVM_INITIALIZE, &[0u8; 16], &[], None);
        PAGEABLE.with(|p| p.set(false));
        assert_eq!(st, 0);
        assert_eq!(
            &reply[8..12],
            &0x56u32.to_le_bytes(),
            "NV_ERR_NOT_SUPPORTED"
        );
        seen();
        assert_eq!(v1(&mut be, h, 39, &[0u8; 8], &[], None).0, -libc::EPERM);
        // MM_INITIALIZE on a second file, naming the refused one.
        let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
        let h2 = be.adopt_for_test(null, HandleKind::Dev(DeviceKind::Uvm));
        let mut mm = vec![0u8; 8];
        mm[0..4].copy_from_slice(&(h as i32).to_le_bytes());
        assert_eq!(v1(&mut be, h2, 75, &mm, &[], None).0, -libc::EBADF);
        assert!(seen().is_empty(), "nothing reached the host");
    }

    #[test]
    fn nothing_reaches_the_uvm_tools_device() {
        let (mut be, h) = backend(HandleKind::Dev(DeviceKind::UvmTools));
        assert_eq!(v1(&mut be, h, 56, &[0u8; 48], &[], None).0, -libc::EPERM);
        assert!(seen().is_empty());
    }
}
