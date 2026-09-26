#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Which RM controls and classes a guest may reach: a default-deny allowlist.

The backend makes every RM_ALLOC and RM_CONTROL itself, as a client of the
host's RM like any other user process. Without an allowlist every control
RM exports and every class it can make was one guest ioctl away, and RM's
own privilege check was the only filter. This script is where the backend's
allowlist comes from (device/src/rmallow.rs holds it to it). It has two
halves, as the other tables here do:

- **The measured half.** For each driver release it reads, from NVIDIA's own
  sources at the tag:
  * every exported RM control (the NVOC exported-method tables of
    src/nvidia/generated/g_*_nvoc.c): its method id, RMCTRL flags, the NVOC
    class exporting it, and its parameter type, whose size a C probe
    compiled against the release's SDK headers measures;
  * its name, from the SDK's FINN-evaluated `NVxxxx_CTRL_CMD_*` defines;
  * whether its parameters carry a pointer, a descriptor, a process id or
    an OS event (read from the parameter struct, nested structs included),
    which the policy uses to refuse what names host resources;
  * the deprecated V1 controls RM converts before its tables are consulted
    (rmapi_deprecated_control.c), each with the V2 control it becomes;
  * every allocatable class (src/nvidia/src/kernel/rmapi/resource_list.h):
    its number (g_allclasses.h), the NVOC class implementing it, its
    RS_FLAGS, and whether any GPU this RM supports has it
    (g_gpu_class_list.c);
  * the parameter-block offsets the backend reads (NVOS02/05/21/32/39/54/64
    status, class and function fields) from nvos.h, by the same probe.
- **The judgement half** (POLICY below): which of those a guest may use.
  See the comment there; README.md, "RM allowlist", has the reasoning.

    ./rmallow_extract.py all                # fetch + measure every release, render
    ./rmallow_extract.py extract 610.57.04  # one release (fetches if needed)
    ./rmallow_extract.py extract 610.57.04 --src ../../nvidia-driver
    ./rmallow_extract.py render             # gen/rmallow/*.json -> gen/src/rmallow/generated.rs
    ./rmallow_extract.py check              # re-measure everything; fail if stale
    ./rmallow_extract.py report             # the policy's counts per release

`render` needs nothing but Python and is what the Rust staleness test runs.
`extract`, `all` and `check` need gcc and network access the first time
(sources cached in $RMALLOW_EXTRACT_CACHE, default $TMPDIR/ogkm-rmallow).
x86_64 (LP64).
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
OUT_DIR = HERE / "rmallow"
RUST_OUT = HERE / "src" / "rmallow" / "generated.rs"
OBSERVED = OUT_DIR / "observed.txt"
REPO = "NVIDIA/open-gpu-kernel-modules"
FORMAT = "virtio-nvgpu/rm-allowlist/1"
DEFAULT_CACHE = Path(os.environ.get("RMALLOW_EXTRACT_CACHE",
                                    Path(tempfile.gettempdir()) / "ogkm-rmallow"))

# The releases measured: gen/rmctrl's set (gen/nvkms/README.md says why each).
VERSIONS = [
    "535.129.03",
    "580.178.04",
    "595.71.05",
    "595.99.02",
    "610.57.04",
    "615.71.09",
]

SDK = "src/common/sdk/nvidia/inc"
RESOURCE_LIST = "src/nvidia/src/kernel/rmapi/resource_list.h"
ALLCLASSES = "src/nvidia/generated/g_allclasses.h"
CLASS_LIST = "src/nvidia/generated/g_gpu_class_list.c"
DEPRECATED = "src/nvidia/interface/deprecated/rmapi_deprecated_control.c"
DEPRECATED_H = "src/nvidia/interface/deprecated/rmapi_deprecated.h"
DEFERRED = "src/nvidia/src/kernel/gpu/deferred_api.c"
CONTROL_H = "src/nvidia/inc/kernel/rmapi/control.h"

FETCH = [
    "version.mk",
    SDK + "/**",
    "src/common/inc/**",
    RESOURCE_LIST, ALLCLASSES, CLASS_LIST, DEPRECATED, DEPRECATED_H, DEFERRED, CONTROL_H,
]
# Of these, only the NVOC files with an exported-method table are kept.
FETCH_IF = re.compile(r"src/nvidia/generated/g_\w+_nvoc\.c$")
HAS_METHODS = b"__nvoc_exported_method_def_"
INCLUDE_DIRS = [SDK, "src/common/inc"]


class ExtractError(Exception):
    """A source, table or field the extractor relies on is not what it expects."""


# --------------------------------------------------------------------------
# RMCTRL and RS flags (src/nvidia/inc/kernel/rmapi/control.h,
# src/nvidia/src/kernel/rmapi/resource_desc_flags.h). Their values are what
# the tables hold; the checks below are RM's own (rmControlValidateClient-
# PrivilegeAccess, serverControl_ValidateCookie, _serverAllocValidatePrivilege).
# --------------------------------------------------------------------------

def parse_rmctrl_flags(root):
    """RMCTRL_FLAGS_* values of this release's control.h: they are not
    stable (NON_PRIVILEGED is 0x10 in 535 and 0x8 in 580 on)."""
    text = (root / CONTROL_H).read_text(errors="replace")
    got = {m.group(1): int(m.group(2), 16) for m in
           re.finditer(r"#define\s+RMCTRL_FLAGS_(\w+)\s+(0x[0-9a-fA-F]+)\b", text)}
    for need in ("PRIVILEGED", "NON_PRIVILEGED", "INTERNAL", "PRIVILEGED_IF_RS_ACCESS_DISABLED"):
        if not got.get(need):
            raise ExtractError(f"control.h: no RMCTRL_FLAGS_{need}")
    if got.get("KERNEL_PRIVILEGED", 0) != 0:
        raise ExtractError("control.h: KERNEL_PRIVILEGED is no longer 0")
    return got


def ctrl_privilege(flags, table):
    """Who RM lets call a control with these flags: 'user' (NON_PRIVILEGED
    and nothing stricter), 'admin' (PRIVILEGED, or PRIVILEGED_IF_RS_ACCESS_
    DISABLED on a host that turns access rights off), 'kernel' (neither
    flag: KERNEL_PRIVILEGED is 0) or 'internal' (INTERNAL: RM itself only)."""
    if flags & table["INTERNAL"]:
        return "internal"
    if flags & table["PRIVILEGED"] or flags & table["PRIVILEGED_IF_RS_ACCESS_DISABLED"]:
        return "admin"
    if flags & table["NON_PRIVILEGED"]:
        return "user"
    return "kernel"


RS_FLAGS = {
    "RS_FLAGS_ALLOC_NON_PRIVILEGED": "user",
    "RS_FLAGS_ALLOC_PRIVILEGED": "admin",
    "RS_FLAGS_ALLOC_KERNEL_PRIVILEGED": "kernel",
    "RS_FLAGS_INTERNAL_ONLY": "internal",
}


def class_privilege(flag_words):
    """RM's rule for an RS_ENTRY's flags: INTERNAL_ONLY is RM's own; no
    privilege flag at all is refused to everyone; KERNEL beats PRIVILEGED
    beats NON_PRIVILEGED."""
    got = {RS_FLAGS[w] for w in flag_words if w in RS_FLAGS}
    for level in ("internal", "kernel", "admin", "user"):
        if level in got:
            return level
    return "none"


# RM_GSS_LEGACY_MASK (rmapi_deprecated.h): a control with bit 15 set goes,
# on a GSP host, straight to GSP-RM (RmGssLegacyRpcCmd) with no lookup in
# CPU-RM's tables, so no RMCTRL flag is checked for it on the CPU side; only
# the PRIVILEGED mask (0xC000) is.
GSS_LEGACY_MASK = 0x8000
GSS_LEGACY_PRIVILEGED = 0xC000
# NV2081_BINAPI: an unprivileged object whose every control CPU-RM sends to
# GSP-RM as it is (binary_api.c, binapiControl), with no table lookup.
BINAPI_CLASS = 0x2081

# GSS legacy controls held to the parameter size NVIDIA's own userspace was
# seen sending, per release it was measured on: RM's CPU side takes any size
# for them (RmGssLegacyRpcCmd copies paramsSize bytes and forwards them to
# GSP-RM), so without this a guest could hand GSP-RM a block of any length.
# Measured with an LD_PRELOAD ioctl logger around the native runs that use
# them (TESTING-RIG.md, "Application pass"); every block was all zeros in, and
# out held clock rates or zeros -- no pointer, descriptor or process id.
# Their ranges are NVIDIA's own NV2080_CTRL_*_LEGACY_NON_PRIVILEGED (0x81 GPU,
# 0x90 CLK, 0xa0 PERF; ctrl2080base.h).
GSS_LEGACY_SIZES = {
    "595.99.02": {
        # NVENC session setup (libnvidia-encode, through Vulkan Video's
        # encoders and through CUDA): 0x8165 (1 byte) then 0x8163 (4) at
        # open, 0x8164 (4) at close. Three concurrent sessions each sent and
        # got 0: the value is no GPU-wide session id one client could name
        # for another's session; RM keys the session by the calling client.
        0x20808163: 4,
        0x20808164: 4,
        0x20808165: 1,
        # CUDA runtime initialisation (cudart's cudaGetDeviceCount and every
        # first CUDA runtime call; also OpenCL): the clock domains (CLK,
        # 8 bytes, out a domain mask) and the current clocks (PERF: 532
        # bytes, out the GPU and memory clocks in kHz; 4 bytes, zeros).
        # Refused, cudart fails with "initialization error".
        0x20809001: 8,
        0x2080a026: 532,
        0x2080a084: 4,
    },
}


# --------------------------------------------------------------------------
# The judgement half.
#
# A control or class reaches RM only if it passes every one of these, per
# release:
#
# 1. RM exports it (an NVOC method or a deprecated converter; an RS_ENTRY
#    some supported GPU has), or it was observed served on hardware.
# 2. RM would let an unprivileged user process call it: NON_PRIVILEGED and
#    nothing stricter. PRIVILEGED, KERNEL_PRIVILEGED and INTERNAL ones RM
#    refuses the backend anyway; not forwarding them means their handler's
#    lookup path, and any bug before the check, is out of reach too.
# 3. A workload the project runs asks for it:
#    - OBSERVED: served by RM across the hardware runs (observed.txt: Vulkan,
#      GL, EGL, CUDA, Firefox, Chromium, mpv, gamescope, nvidia-smi);
#    - OWN_OBJECT: every user-callable control of an object the guest itself
#      allocated and RM confines to that object -- a channel, a channel
#      group, a context share, an engine object, a usermode page, a VA space,
#      a memory object, a semaphore surface -- so the other architectures'
#      and engines' variants of what was observed come with their classes;
#    - WORKLOAD: named controls and classes for workloads the project
#      targets but has not yet run, chiefly NVENC, NVDEC, NVJPG, OFA and
#      Vulkan Video, and the other GPU architectures' classes for the
#      objects observed (read from gVisor nvproxy's compute, utility,
#      graphics and video lists as a hint, and NVIDIA's naming).
# 4. It names no host resource the backend does not translate: a
#    descriptor, a process id, an OS event (DENY and the field scan below).
#
# Everything else is refused before RM (device/src/rmallow.rs).
# --------------------------------------------------------------------------

# NVOC classes whose controls act on the calling client's own object and
# nothing else of the GPU's (OWN_OBJECT): the exporting class, and the
# allocatable classes that have its controls. Their user-callable controls
# are allowed when the guest may allocate one of those.
OWN_OBJECT_NVOC = {
    "KernelChannel": {"KernelChannel"},
    "KernelChannelGroupApi": {"KernelChannelGroupApi"},
    "KernelCtxShareApi": {"KernelCtxShareApi"},
    # NV0090: the channel's (or group's) own graphics context.
    "KernelGraphicsContext": {"KernelChannel", "KernelChannelGroupApi"},
    "VaSpaceApi": {"VaSpaceApi"},
    # NV0041: every memory object's.
    "Memory": {"SystemMemory", "VideoMemory", "VirtualMemory", "VirtualMemoryRange",
               "OsDescMemory"},
    "SystemMemory": {"SystemMemory"},
    "MemoryMapper": {"MemoryMapper"},
    "SemaphoreSurface": {"SemaphoreSurface"},
    "GpuUserSharedData": {"GpuUserSharedData"},
    "ContextDma": {"ContextDma"},
}

# Controls of those objects left out of OWN_OBJECT (still allowed if
# observed or named): each reaches past the caller's own object.
OWN_OBJECT_EXCLUDE = {
    "NVA06C_CTRL_CMD_MAKE_REALTIME":
        "makes every other client's compute on the runlist preemptible to it",
    "NVA06F_CTRL_CMD_RESTART_RUNLIST":
        "preempts the whole runlist, other clients' channels on it included "
        "(ctrla06fgpfifo.h: 'may cause certain low priority channels to starve')",
}

# Allocatable classes by the NVOC class implementing them (OWN_OBJECT for
# classes): every user-allocatable class of these that some supported GPU
# has. This is how every architecture's channel, usermode, 3D, compute,
# copy and video classes are in, not only the Ampere ones observed.
OWN_OBJECT_CLASSES_NVOC = {
    "RmClientResource",            # NV01_ROOT, NV01_ROOT_NON_PRIV, NV01_ROOT_CLIENT
    "Device", "Subdevice",
    "SystemMemory", "VideoMemory", "VirtualMemory", "VirtualMemoryRange", "MemoryMapper",
    "OsDescMemory",                # only through osdesc.rs's page lists
    "ContextDma", "EventApi",
    "KernelChannel", "KernelChannelGroupApi", "KernelCtxShareApi",
    "KernelGraphicsObject", "KernelCeContext",
    "NvdecContext", "MsencContext", "NvjpgContext", "OfaContext",
    "UserModeApi", "VaSpaceApi", "ZbcApi",
    "SemaphoreSurface", "GpuUserSharedData",
}

# NVOC classes whose external classes come one per GPU architecture: of
# these only a class some GPU the release drives has is allowed.
ARCH_NVOC = {
    "KernelChannel", "KernelGraphicsObject", "KernelCeContext", "NvdecContext",
    "MsencContext", "NvjpgContext", "OfaContext", "UserModeApi",
}

# Named controls for workloads not yet run (WORKLOAD). Names, not numbers:
# a name a release lacks is skipped for it. Mostly device and subdevice
# getters the video, compute and graphics userspace drivers issue.
WORKLOAD_CONTROLS = [
    # Video (NVENC, NVDEC, NVJPG, OFA, Vulkan Video)
    "NV0080_CTRL_CMD_BSP_GET_CAPS", "NV0080_CTRL_CMD_BSP_GET_CAPS_V2",
    "NV0080_CTRL_CMD_MSENC_GET_CAPS", "NV0080_CTRL_CMD_MSENC_GET_CAPS_V2",
    "NV0080_CTRL_CMD_NVJPG_GET_CAPS_V2",
    "NV0080_CTRL_CMD_FIFO_GET_CAPS", "NV0080_CTRL_CMD_FIFO_GET_CAPS_V2",
    "NV0080_CTRL_CMD_GR_GET_CAPS", "NV0080_CTRL_CMD_GR_GET_CAPS_V2",
    "NV2080_CTRL_CMD_GPU_GET_ENCODER_CAPACITY",
    "NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_STATS",
    "NVA0BC_CTRL_CMD_NVENC_SW_SESSION_UPDATE_INFO",
    "NVA0BC_CTRL_CMD_NVENC_SW_SESSION_UPDATE_INFO_V2",
    "NV2080_CTRL_CMD_BUS_GET_INFO", "NV2080_CTRL_CMD_BUS_GET_INFO_V2",
    # Graphics
    "NV0080_CTRL_CMD_DMA_ADV_SCHED_GET_VA_CAPS", "NV0080_CTRL_CMD_DMA_GET_CAPS",
    "NV0080_CTRL_CMD_FB_GET_CAPS", "NV0080_CTRL_CMD_FB_GET_CAPS_V2",
    "NV0080_CTRL_CMD_FIFO_GET_ENGINE_CONTEXT_PROPERTIES",
    "NV0080_CTRL_CMD_GR_GET_INFO",
    "NV0080_CTRL_CMD_HOST_GET_CAPS", "NV0080_CTRL_CMD_HOST_GET_CAPS_V2",
    "NV0080_CTRL_CMD_GPU_GET_CLASSLIST", "NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2",
    "NV0080_CTRL_CMD_GPU_GET_NUM_SUBDEVICES",
    "NV0080_CTRL_CMD_GPU_GET_VIRTUALIZATION_MODE",
    "NV0080_CTRL_CMD_GPU_QUERY_SW_STATE_PERSISTENCE",
    "NV2080_CTRL_CMD_GPU_GET_ENGINES", "NV2080_CTRL_CMD_GPU_GET_ENGINES_V2",
    "NV2080_CTRL_CMD_GPU_GET_ENGINE_CLASSLIST",
    "NV2080_CTRL_CMD_GPU_GET_ENGINE_PARTNERLIST",
    "NV2080_CTRL_CMD_GPU_GET_CHIP_DETAILS", "NV2080_CTRL_CMD_GPU_GET_SKYLINE_INFO",
    "NV2080_CTRL_CMD_GR_GET_ZCULL_INFO", "NV2080_CTRL_CMD_GR_CTXSW_ZCULL_BIND",
    "NV2080_CTRL_CMD_FB_GET_FB_REGION_INFO",
    "NV2080_CTRL_CMD_BUS_GET_PCIE_CPL_ATOMICS_CAPS",
    "NV2080_CTRL_CMD_BUS_GET_PCIE_REQ_ATOMICS_CAPS",
    "NV2080_CTRL_CMD_BUS_GET_PCIE_SUPPORTED_GPU_ATOMICS",
    "NV2080_CTRL_CMD_TIMER_GET_TIME",
    "NV2080_CTRL_CMD_GPU_GET_RECOVERY_ACTION",
    "NV9096_CTRL_CMD_GET_ZBC_CLEAR_TABLE_ENTRY", "NV9096_CTRL_CMD_GET_ZBC_CLEAR_TABLE_SIZE",
    "NV9096_CTRL_CMD_SET_ZBC_COLOR_CLEAR", "NV9096_CTRL_CMD_SET_ZBC_DEPTH_CLEAR",
    "NV9096_CTRL_CMD_SET_ZBC_STENCIL_CLEAR", "NV9096_CTRL_CMD_GET_ZBC_CLEAR_TABLE",
    # Compute and utility
    "NV0000_CTRL_CMD_CLIENT_GET_ADDR_SPACE_TYPE",
    "NV0000_CTRL_CMD_CLIENT_GET_HANDLE_INFO",
    "NV0000_CTRL_CMD_GPU_GET_ACTIVE_DEVICE_IDS",
    "NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS", "NV0000_CTRL_CMD_GPU_GET_DEVICE_IDS",
    "NV0000_CTRL_CMD_GPU_GET_ID_INFO", "NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2",
    "NV0000_CTRL_CMD_GPU_GET_MEMOP_ENABLE", "NV0000_CTRL_CMD_GPU_GET_PCI_INFO",
    "NV0000_CTRL_CMD_GPU_GET_PROBED_IDS", "NV0000_CTRL_CMD_GPU_GET_UUID_FROM_GPU_ID",
    "NV0000_CTRL_CMD_GPU_GET_UUID_INFO",
    "NV0000_CTRL_CMD_GPU_QUERY_DRAIN_STATE",
    "NV0000_CTRL_CMD_GPU_ASYNC_ATTACH_ID", "NV0000_CTRL_CMD_GPU_WAIT_ATTACH_ID",
    "NV0000_CTRL_CMD_GPU_ATTACH_IDS", "NV0000_CTRL_CMD_GPU_DETACH_IDS",
    "NV0000_CTRL_CMD_SYNC_GPU_BOOST_GROUP_INFO",
    "NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION", "NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION_V2",
    "NV0000_CTRL_CMD_SYSTEM_GET_CPU_INFO", "NV0000_CTRL_CMD_SYSTEM_GET_FEATURES",
    "NV0000_CTRL_CMD_SYSTEM_GET_FABRIC_STATUS",
    "NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS", "NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_V2",
    "NV0000_CTRL_CMD_SYSTEM_GET_P2P_CAPS_MATRIX",
    "NV0000_CTRL_CMD_CLIENT_SET_INHERITED_SHARE_POLICY",
    "NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT",
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD",
    "NV0000_CTRL_CMD_OS_UNIX_GET_EXPORT_OBJECT_INFO",
    "NV0000_CTRL_CMD_OS_UNIX_CREATE_EXPORT_OBJECT_FD",
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECTS_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECTS_FROM_FD",
    "NV0000_CTRL_CMD_OS_UNIX_FLUSH_USER_CACHE",
    "NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST",
    "NV0080_CTRL_CMD_GR_SET_TPC_PARTITION_MODE",
    "NV0080_CTRL_CMD_PERF_CUDA_LIMIT_SET_CONTROL",
    "NV2080_CTRL_CMD_BIOS_GET_INFO", "NV2080_CTRL_CMD_BIOS_GET_INFO_V2",
    "NV2080_CTRL_CMD_BUS_GET_C2C_INFO", "NV2080_CTRL_CMD_BUS_GET_PCI_BAR_INFO",
    "NV2080_CTRL_CMD_BUS_GET_PCI_INFO",
    "NV2080_CTRL_CMD_CE_GET_ALL_CAPS", "NV2080_CTRL_CMD_CE_GET_CAPS",
    "NV2080_CTRL_CMD_CE_GET_CAPS_V2", "NV2080_CTRL_CMD_CE_GET_CE_PCE_MASK",
    "NV2080_CTRL_CMD_CE_GET_LCE_SHIM_INFO",
    "NV2080_CTRL_CMD_EVENT_SET_NOTIFICATION",
    "NV2080_CTRL_CMD_FB_GET_INFO", "NV2080_CTRL_CMD_FB_GET_INFO_V2",
    "NV2080_CTRL_CMD_FB_GET_GPU_CACHE_INFO",
    "NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT",
    "NV2080_CTRL_CMD_FB_QUERY_DRAM_ENCRYPTION_STATUS",
    "NV2080_CTRL_CMD_FLCN_GET_CTX_BUFFER_SIZE",
    "NV2080_CTRL_CMD_GET_GPU_FABRIC_PROBE_INFO",
    "NV2080_CTRL_CMD_GPU_GET_ACTIVE_PARTITION_IDS",
    "NV2080_CTRL_CMD_GPU_GET_COMPUTE_POLICY_CONFIG",
    "NV2080_CTRL_CMD_GPU_GET_GID_INFO", "NV2080_CTRL_CMD_GPU_GET_ID",
    "NV2080_CTRL_CMD_GPU_GET_INFO", "NV2080_CTRL_CMD_GPU_GET_INFO_V2",
    "NV2080_CTRL_CMD_GPU_GET_NAME_STRING", "NV2080_CTRL_CMD_GPU_GET_SHORT_NAME_STRING",
    "NV2080_CTRL_CMD_GPU_GET_SIMULATION_INFO",
    "NV2080_CTRL_CMD_GPU_QUERY_COMPUTE_MODE_RULES",
    "NV2080_CTRL_CMD_GPU_QUERY_ECC_STATUS",
    "NV2080_CTRL_CMD_GR_GET_CAPS_V2", "NV2080_CTRL_CMD_GR_GET_CTX_BUFFER_SIZE",
    "NV2080_CTRL_CMD_GR_GET_GLOBAL_SM_ORDER", "NV2080_CTRL_CMD_GR_GET_GPC_MASK",
    "NV2080_CTRL_CMD_GR_GET_INFO", "NV2080_CTRL_CMD_GR_GET_INFO_V2",
    "NV2080_CTRL_CMD_GR_GET_SM_ISSUE_RATE_MODIFIER",
    "NV2080_CTRL_CMD_GR_GET_SM_ISSUE_RATE_MODIFIER_V2",
    "NV2080_CTRL_CMD_GR_GET_TPC_MASK", "NV2080_CTRL_CMD_GR_SET_CTXSW_PREEMPTION_MODE",
    "NV2080_CTRL_CMD_GRMGR_GET_GR_FS_INFO",
    "NV2080_CTRL_CMD_GSP_GET_FEATURES",
    "NV2080_CTRL_CMD_MC_GET_ARCH_INFO", "NV2080_CTRL_CMD_MC_SERVICE_INTERRUPTS",
    "NV2080_CTRL_CMD_NVLINK_GET_NVLINK_CAPS", "NV2080_CTRL_CMD_NVLINK_GET_NVLINK_STATUS",
    "NV2080_CTRL_CMD_PERF_BOOST", "NV2080_CTRL_CMD_PERF_GET_CURRENT_PSTATE",
    "NV2080_CTRL_CMD_RC_GET_WATCHDOG_INFO",
    "NV2080_CTRL_CMD_TIMER_GET_GPU_CPU_TIME_CORRELATION_INFO",
    "NV2080_CTRL_CMD_TIMER_SET_GR_TICK_FREQ",
    "NV00DE_CTRL_CMD_REQUEST_DATA_POLL",
    "NV83DE_CTRL_CMD_DEBUG_CLEAR_ALL_SM_ERROR_STATES",
    "NV83DE_CTRL_CMD_DEBUG_READ_ALL_SM_ERROR_STATES",
    "NV83DE_CTRL_CMD_DEBUG_SET_EXCEPTION_MASK",
    "NV83DE_CTRL_CMD_DEBUG_SUSPEND_CONTEXT", "NV83DE_CTRL_CMD_DEBUG_RESUME_CONTEXT",
    "NV_CONF_COMPUTE_CTRL_CMD_SYSTEM_GET_CAPABILITIES",
]

# Allocatable classes named for workloads not yet run, beyond OWN_OBJECT's.
WORKLOAD_CLASSES = [
    "NVENC_SW_SESSION",        # NVENC's session accounting (NVA0BC)
    "GT200_DEBUGGER",          # CUDA's SM error reporting; observed
    "NV04_DISPLAY_COMMON",     # GL and EGL display queries; observed
    "FERMI_TWOD_A", "KEPLER_INLINE_TO_MEMORY_B",
    "NV_CONFIDENTIAL_COMPUTE",  # observed: CUDA asks whether CC is on
    "NV2081_BINAPI",
    "NV01_MEMORY_LOCAL_USER", "NV50_MEMORY_VIRTUAL",
    # VID_HEAP_CONTROL's HW_ALLOC makes one (surface compression and zcull
    # resources); what GL's heap calls are is not counted, so it stays.
    "NV01_MEMORY_HW_RESOURCES",
    # RegisterMemory: the Vulkan driver allocates one by ALLOC_MEMORY (seen on
    # the rig once the list enforced; vulkaninfo fails without it). RM lets
    # any user allocate it (RS_FLAGS_ALLOC_NON_PRIVILEGED); what makes it
    # dangerous is a CPU mapping of BAR0, which RM gives only an admin client
    # (mapping_cpu.c, ADDR_REGMEM: rmclientIsAdmin) -- and the backend is
    # never one: posture.rs refuses root and CAP_SYS_ADMIN.
    "NV01_MEMORY_LOCAL_PRIVILEGED",
]

# Refused whatever else says: each names a host resource the backend does
# not translate, or reports on the host beyond the guest's own objects.
# (The host-PID controls rmctl.rs answers itself, and the OS_UNIX ones it
# refuses, never reach this table's check; they are listed so the table
# does not claim them.)
DENY_CONTROLS = {
    "NV0000_CTRL_OS_UNIX_CMD_MEMACCT_SET_LIMITS": "a host cgroup by descriptor",
    "NV0000_CTRL_OS_UNIX_CMD_MEMACCT_GET_LIMITS": "a host cgroup by descriptor",
    "NV0000_CTRL_CMD_CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL": "an IMEX channel by descriptor",
    "NV0000_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD": "the host's firmware",
    "NV0073_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD": "the host's firmware",
    "NV2080_CTRL_CMD_GPU_GET_PIDS": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_CMD_GPU_GET_PID_INFO": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO_V2": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_GPU_GET_NVFBC_SW_SESSION_INFO": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_CMD_PERF_GET_GPUMON_PERFMON_UTIL_SAMPLES_V2": "host PIDs (rmctl.rs)",
    "NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO": "host PIDs (rmctl.rs)",
    "NV0000_CTRL_CMD_GPUACCT_GET_PROC_ACCOUNTING_INFO": "host PIDs (rmctl.rs)",
    "NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_PIDS": "host PIDs (rmctl.rs)",
    "NV0000_CTRL_CMD_GPUACCT_GET_PROC_ACCOUNTING_INFO_V2": "host PIDs (rmctl.rs)",
    "NV90CC_CTRL_CMD_HWPM_GET_RESERVATION_INFO": "host PIDs (rmctl.rs)",
    "NV0000_CTRL_CMD_NVD_GET_RCERR_RPT": "host PIDs (rmctl.rs)",
}

# A descriptor, PID or OS event in a control's parameters is a host resource
# unless the backend translates it: these are the ones it does.
TRANSLATED = {
    # rmctl.rs: the caller's own control file.
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECT_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECT_FROM_FD",
    "NV0000_CTRL_CMD_OS_UNIX_GET_EXPORT_OBJECT_INFO",
    "NV0000_CTRL_CMD_OS_UNIX_CREATE_EXPORT_OBJECT_FD",
    "NV0000_CTRL_CMD_OS_UNIX_EXPORT_OBJECTS_TO_FD",
    "NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECTS_FROM_FD",
    # semsurf.rs: must name a live OS event of the caller's.
    "NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER",
}

# A host field RM reads only where it cannot be here: allowed with it.
HOST_FIELD_IGNORED = {
    "NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_STATE":
        "pid is read only on a vGPU host with GSP plugin offload "
        "(cliresCtrlCmdGpuAcctGetAccountingState); otherwise the GPU's state",
}

# Classes refused whatever else says (guestptr.rs REFUSED_ALLOC_CLASSES has
# the reasons; OsDescMemory's 0x71 is let through only by osdesc.rs).
DENY_CLASSES = {
    "NV01_EVENT_KERNEL_CALLBACK", "NV01_EVENT_KERNEL_CALLBACK_EX",
    "NV01_MEMORY_LIST_SYSTEM", "NV01_MEMORY_LIST_FBMEM", "NV01_MEMORY_LIST_OBJECT",
    "NV_FB_SEGMENT", "NV0092_RG_LINE_CALLBACK", "NV9010_VBLANK_CALLBACK",
    "NV_IMEX_SESSION", "NV_MEMORY_FABRIC_IMPORT_V2", "NV_MEMORY_MULTICAST_FABRIC",
}

# Field names that name a host resource by number: a descriptor, a process,
# an OS event. Matched against every field of a control's parameters,
# nested structs included.
HOST_FIELD = {
    "fd": re.compile(r"^(fd|\w*Fd|\w*FD|\w*[Dd]escriptor|fds?List)$"),
    "pid": re.compile(r"^(pid\w*|\w*Pids?|\w*PID|\w*[Pp]rocess_?[Ii]d|\w*Pid(Tbl|List))$"),
    "os_event": re.compile(r"^(\w*OsEvent|\w*osEvent|hEvent[A-Z]?\w*Os\w*)$"),
}

# NVOS32 functions RM's VID_HEAP_CONTROL implements (rmapi_deprecated_
# vidheapctrl.c), each with the classes it allocates: a function is allowed
# when all of its classes are. ALLOC_SIZE's three pick one of them by flags.
VIDHEAP_FUNCTIONS = {
    "NVOS32_FUNCTION_ALLOC_SIZE": ["NV50_MEMORY_VIRTUAL", "NV01_MEMORY_LOCAL_USER", "NV01_MEMORY_SYSTEM"],
    "NVOS32_FUNCTION_ALLOC_SIZE_RANGE": ["NV50_MEMORY_VIRTUAL", "NV01_MEMORY_LOCAL_USER", "NV01_MEMORY_SYSTEM"],
    "NVOS32_FUNCTION_ALLOC_TILED_PITCH_HEIGHT": ["NV50_MEMORY_VIRTUAL", "NV01_MEMORY_LOCAL_USER", "NV01_MEMORY_SYSTEM"],
    "NVOS32_FUNCTION_FREE": [],
    "NVOS32_FUNCTION_INFO": [],
    "NVOS32_FUNCTION_RELEASE_COMPR": [],
    "NVOS32_FUNCTION_REACQUIRE_COMPR": [],
    "NVOS32_FUNCTION_GET_MEM_ALIGNMENT": [],
    "NVOS32_FUNCTION_HW_ALLOC": ["NV01_MEMORY_HW_RESOURCES"],
    "NVOS32_FUNCTION_HW_FREE": [],
    "NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR": ["NV01_MEMORY_SYSTEM_OS_DESCRIPTOR"],
}


# --------------------------------------------------------------------------
# Fetching
# --------------------------------------------------------------------------

def _wanted(rel):
    for pat in FETCH:
        if pat.endswith("/**"):
            if rel.startswith(pat[:-2]):
                return True
        elif rel == pat:
            return True
    return False


class _HashingReader:
    def __init__(self, f, h):
        self.f, self.h = f, h

    def read(self, n=-1):
        b = self.f.read(n)
        self.h.update(b)
        return b


def fetch(version, cache):
    """Unpack what the extractor reads of `version` into cache/<version>."""
    dest = cache / version
    if (dest / "SOURCE.json").exists():
        return dest
    url = f"https://codeload.github.com/{REPO}/tar.gz/refs/tags/{version}"
    try:
        resp = urllib.request.urlopen(url, timeout=300)
    except urllib.error.HTTPError as e:
        raise ExtractError(f"{url}: {e}") from e
    tmp = cache / f".{version}.partial"
    shutil.rmtree(tmp, ignore_errors=True)
    h = hashlib.sha256()
    commit, nfiles = None, 0
    with resp:
        reader = _HashingReader(resp, h)
        with tarfile.open(fileobj=reader, mode="r|gz") as tf:
            for m in tf:
                if commit is None:
                    commit = tf.pax_headers.get("comment")
                parts = m.name.split("/", 1)
                if len(parts) < 2 or not m.isfile():
                    continue
                rel = parts[1]
                keep = _wanted(rel)
                maybe = not keep and FETCH_IF.match(rel)
                if not keep and not maybe:
                    continue
                with tf.extractfile(m) as src:
                    data = src.read()
                if maybe and HAS_METHODS not in data:
                    continue
                target = tmp / rel
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                nfiles += 1
        while reader.read(1 << 16):
            pass
    if not nfiles:
        raise ExtractError(f"{url}: no wanted files in the tarball")
    (tmp / "SOURCE.json").write_text(json.dumps({
        "tag": version, "commit": commit, "url": url,
        "tarball_sha256": h.hexdigest(), "files": nfiles,
    }, indent=2) + "\n")
    shutil.rmtree(dest, ignore_errors=True)
    tmp.rename(dest)
    return dest


def local_source(src):
    return {"tag": None, "commit": None, "url": None, "tarball_sha256": None,
            "files": None, "local": True}


# --------------------------------------------------------------------------
# Reading the sources
# --------------------------------------------------------------------------

def strip_comments(s):
    s = re.sub(r"/\*.*?\*/", lambda m: " " * len(m.group(0)), s, flags=re.S)
    return re.sub(r"//[^\n]*", "", s)


def match_brace(s, i, open_="{", close="}"):
    depth = 0
    for j in range(i, len(s)):
        if s[j] == open_:
            depth += 1
        elif s[j] == close:
            depth -= 1
            if depth == 0:
                return j + 1
    raise ExtractError(f"unbalanced {open_} at {i}")


def split_args(s):
    out, depth, cur = [], 0, []
    for ch in s:
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append("".join(cur).strip())
            cur = []
        else:
            cur.append(ch)
    out.append("".join(cur).strip())
    return out


METHOD = re.compile(
    r"/\*flags=\*/\s*(0x[0-9a-fA-F]+)u,\s*"
    r"/\*accessRight=\*/\s*(0x[0-9a-fA-F]+)u,\s*"
    r"/\*methodId=\*/\s*(0x[0-9a-fA-F]+)u,\s*"
    r"/\*paramSize=\*/\s*(?:sizeof\((\w+)\)|0)[^,]*,\s*"
    r"/\*pClassInfo=\*/\s*&\(__nvoc_class_def_(\w+)\.classInfo\)"
    r"(?:\s*,\s*#if NV_PRINTF_STRINGS_ALLOWED\s*/\*func=\*/\s*\"(\w+)\")?")


def parse_methods(root):
    """Every exported method of every NVOC class: {cmd: [entry, ...]}."""
    out = {}
    files = sorted((root / "src/nvidia/generated").glob("g_*_nvoc.c"))
    for p in files:
        text = p.read_text(errors="replace")
        if "__nvoc_exported_method_def_" not in text:
            continue
        n_tables = len(re.findall(r"__nvoc_exported_method_def_\w+\[\]\s*=", text))
        n_entries = len(re.findall(r"/\*methodId=\*/", text))
        found = 0
        for m in METHOD.finditer(text):
            flags, access, cmd, ptype, nvoc, func = m.groups()
            out.setdefault(int(cmd, 16), []).append({
                "nvoc": nvoc, "flags": int(flags, 16), "access": int(access, 16),
                "params_type": ptype, "func": func})
            found += 1
        if found != n_entries or (n_tables and not found):
            raise ExtractError(f"{p.name}: read {found} of {n_entries} exported methods")
    if len(out) < 500:
        raise ExtractError(f"only {len(out)} exported controls: not the tables expected")
    return out


FINN_CMD = re.compile(
    r"#define\s+(NV\w+)\s+\((0x[0-9a-fA-F]+)[uU]?\)\s*/\*\s*finn:\s*Evaluated from\s*"
    r"\"\(FINN_\w+_INTERFACE_ID\s*<<\s*8\)")


def parse_control_names(root):
    """FINN command ids by value: {cmd: [names]} from the SDK headers."""
    out, aliases = {}, []
    for p in sorted((root / SDK / "ctrl").rglob("*.h")):
        text = p.read_text(errors="replace")
        for m in FINN_CMD.finditer(text):
            out.setdefault(int(m.group(2), 16), []).append(m.group(1))
        aliases += re.findall(r"^#define\s+(NV\w+_CMD_\w+)\s+(NV\w+_CMD_\w+)\s*$", text, flags=re.M)
    # `#define OLD NEW` names, resolved to NEW's number.
    by_name = {n: v for v, ns in out.items() for n in ns}
    for _ in range(3):
        for a, b in aliases:
            if b in by_name and a not in by_name:
                by_name[a] = by_name[b]
                out[by_name[b]].append(a)
    return out


def best_name(names, fallback):
    if not names:
        return fallback
    cmd = [n for n in names if "_CMD_" in n]
    return sorted(cmd or names, key=lambda n: (len(n), n))[0]


def parse_deprecated(root, names_by_value):
    """{v1 name: v2 name} of the deprecated controls RM converts itself."""
    text = strip_comments((root / DEPRECATED).read_text(errors="replace"))
    m = re.search(r"rmDeprecatedControlTable\[\]\s*=\s*\{", text)
    if not m:
        raise ExtractError("no rmDeprecatedControlTable")
    body = text[m.end() - 1:match_brace(text, m.end() - 1)]
    known = {n for ns in names_by_value.values() for n in ns}
    out = {}
    for row in re.finditer(r"\{\s*(NV\w+)\s*,\s*V2_CONVERTER\(_(NV\w+)\)\s*(?:,\s*\w+\s*)?\}", body):
        v1, conv = row.groups()
        if v1 != conv:
            raise ExtractError(f"deprecated {v1} converted by {conv}")
        d = re.search(r"V2_CONVERTER\(_" + re.escape(v1) + r"\)\s*\([^;{]*\)\s*\{", text)
        if not d:
            raise ExtractError(f"no converter body for {v1}")
        fb = text[d.end() - 1:match_brace(text, d.end() - 1)]
        targets = sorted({t for t in re.findall(r"\b(NV[0-9A-F]{4}_CTRL_CMD_\w+)", fb)
                          if t != v1 and t in known})
        if len(targets) != 1:
            raise ExtractError(f"deprecated {v1}: converter issues {targets}, not one control")
        out[v1] = targets[0]
    if not out:
        raise ExtractError("no deprecated controls read")
    return out


def parse_deferred(root):
    """The controls NV5080's DEFERRED_API runs later (deferred_api.c)."""
    text = strip_comments((root / DEFERRED).read_text(errors="replace"))
    return sorted(set(re.findall(r"case\s+(NV2080_CTRL_CMD_\w+)\s*:", text)))


def parse_classes(root):
    """RS_ENTRY rows of resource_list.h, and the class numbers and the chip
    class lists."""
    raw = (root / RESOURCE_LIST).read_text(errors="replace")
    text = strip_comments(raw)
    # #if nesting, to leave out what release builds do not have.
    conds, lines_cond = [], []
    for line in text.splitlines(keepends=True):
        s = line.strip()
        if s.startswith("#if"):
            conds.append(s)
        elif s.startswith("#else") and conds:
            conds[-1] = "!" + conds[-1]
        elif s.startswith("#endif") and conds:
            conds.pop()
        lines_cond.append((line, " && ".join(conds)))
    pos, cond_at = 0, []
    for line, c in lines_cond:
        cond_at.append((pos, c))
        pos += len(line)

    def cond_of(i):
        c = ""
        for p, cc in cond_at:
            if p > i:
                break
            c = cc
        return c

    entries = []
    for m in re.finditer(r"\bRS_ENTRY\s*\(", text):
        start = m.end() - 1
        args = split_args(text[start + 1:match_brace(text, start, "(", ")") - 1])
        if len(args) != 8:
            raise ExtractError(f"RS_ENTRY with {len(args)} fields at {start}")
        ext, internal, _multi, parents, param, _prio, flags, _access = args
        pm = re.match(r"RS_(REQUIRED|OPTIONAL)\((\w+)\)|RS_NONE", param.strip())
        if not pm:
            raise ExtractError(f"{ext}: alloc params {param!r}")
        cond = cond_of(start)
        entries.append({
            "name": ext.strip(), "nvoc": internal.strip(),
            "parents": re.findall(r"classId\((\w+)\)|(RS_ROOT_OBJECT)", parents),
            "params": pm.group(2), "flags": re.findall(r"RS_FLAGS_\w+", flags),
            "debug_only": bool(re.search(r"\bDEBUG\b|\bDEVELOP\b", cond)),
        })
    if len(entries) < 100:
        raise ExtractError(f"only {len(entries)} RS_ENTRY rows")
    numbers = {}
    for m in re.finditer(r"#define\s+(\w+)\s+\((0x[0-9a-fA-F]+)\)",
                         (root / ALLCLASSES).read_text(errors="replace")):
        numbers.setdefault(m.group(1), int(m.group(2), 16))
    chips = strip_comments((root / CLASS_LIST).read_text(errors="replace"))
    on_chip = set(re.findall(r"^\s+(\w+),\s*$", chips, flags=re.M))
    on_chip |= set(re.findall(r"\{\s*(\w+),\s*ENG_", chips))
    if len(on_chip) < 50:
        raise ExtractError(f"only {len(on_chip)} classes in the chip lists")
    return entries, numbers, on_chip


def parse_vidheap(root):
    text = (root / SDK / "nvos.h").read_text(errors="replace")
    got = {n: int(v) for n, v in re.findall(r"#define\s+(NVOS32_FUNCTION_\w+)\s+(\d+)\b", text)}
    missing = [f for f in VIDHEAP_FUNCTIONS if f not in got]
    if missing:
        raise ExtractError(f"nvos.h lacks {missing}")
    return {f: got[f] for f in VIDHEAP_FUNCTIONS}


# --------------------------------------------------------------------------
# Parameter structs: what their fields name, and their sizes (the probe)
# --------------------------------------------------------------------------

def struct_index(root):
    """{typedef name: (header relative to SDK, body or alias)} over the SDK."""
    inc = root / SDK
    idx = {}
    for p in sorted(inc.rglob("*.h")):
        rel = str(p.relative_to(inc))
        text = strip_comments(p.read_text(errors="replace"))
        for m in re.finditer(r"typedef\s+(struct|union)\s*(\w*)\s*\{", text):
            start = m.end() - 1
            end = match_brace(text, start)
            tail = re.match(r"\s*(\w+)\s*;", text[end:])
            if tail:
                idx.setdefault(tail.group(1), (rel, text[start + 1:end - 1]))
        for m in re.finditer(r"typedef\s+(?:struct\s+)?(\w+)\s+(\w+)\s*;", text):
            idx.setdefault(m.group(2), (rel, ("alias", m.group(1))))
    return idx


FIELD = re.compile(r"(?:NV_DECLARE_ALIGNED\s*\(\s*)?([A-Za-z_]\w*)\s*(\*?)\s*([A-Za-z_]\w*)\s*(?:\[[^\]]*\])*\s*(?:,\s*\d+\s*\))?\s*;")


def struct_fields(idx, name, depth=0, seen=None):
    """(field name, type name, is pointer) of `name`, nested structs flattened."""
    seen = seen or set()
    if name in seen or depth > 6 or name not in idx:
        return []
    seen = seen | {name}
    _, body = idx[name]
    if isinstance(body, tuple):
        return struct_fields(idx, body[1], depth + 1, seen)
    out = []
    # Anonymous nested struct/union bodies: read their fields in place.
    flat = re.sub(r"(struct|union)\s*\w*\s*\{", " ", body).replace("}", " ")
    for m in FIELD.finditer(flat):
        ty, star, fname = m.groups()
        if ty in ("struct", "union", "typedef"):
            continue
        out.append((fname, ty, bool(star) or ty in ("NvP64",)))
        out += struct_fields(idx, ty, depth + 1, seen)
    return out


def host_fields(idx, ptype):
    """What of the host a control's parameters could name: kind -> fields."""
    got = {}
    for fname, ty, ptr in struct_fields(idx, ptype):
        if ptr:
            got.setdefault("pointer", []).append(fname)
        for kind, pat in HOST_FIELD.items():
            if pat.match(fname):
                got.setdefault(kind, []).append(fname)
    return {k: sorted(set(v)) for k, v in got.items()}


OS_BLOCKS = {
    # (struct, field): offsets the backend reads of the escapes' own blocks.
    "NVOS02_PARAMETERS": ["hClass", "status"],
    "NVOS05_PARAMETERS": ["hClass", "status"],
    "NVOS21_PARAMETERS": ["hClass", "status"],
    "NVOS32_PARAMETERS": ["function", "status"],
    "NVOS39_PARAMETERS": ["hClass", "status"],
    "NVOS54_PARAMETERS": ["cmd", "paramsSize", "status"],
    "NVOS64_PARAMETERS": ["hClass", "status"],
}


def probe(root, types, workdir):
    """sizeof every type in `types` (those that compile), and the escape
    blocks' offsets: ({type: size}, {struct: {field: offset, "size": n}})."""
    idx = struct_index(root)
    todo = sorted(t for t in types if t in idx)
    headers = sorted({idx[t][0] for t in todo} | {"nvos.h"})
    bad = set()
    for _ in range(8):
        lines = ["#include <stdio.h>", "#include <stddef.h>", '#include "nvtypes.h"']
        lines += [f'#include "{h}"' for h in headers]
        lines.append("int main(void) {")
        for t in todo:
            if t not in bad:
                lines.append(f'  printf("S {t} %zu\\n", sizeof({t}));')
        for s, fields in OS_BLOCKS.items():
            lines.append(f'  printf("B {s} size %zu\\n", sizeof({s}));')
            for f in fields:
                lines.append(f'  printf("B {s} {f} %zu\\n", offsetof({s}, {f}));')
        lines.append("  return 0;\n}")
        c = workdir / "probe.c"
        c.write_text("\n".join(lines) + "\n")
        exe = workdir / "probe"
        r = subprocess.run(["gcc", "-w", "-DNV_LINUX", "-o", str(exe), str(c)]
                           + [f"-I{root / d}" for d in INCLUDE_DIRS],
                           capture_output=True, text=True)
        if r.returncode == 0:
            break
        # A type whose header needs what the SDK does not have: left out,
        # and its control's size stays unchecked.
        errs = set(re.findall(r"probe\.c:\d+:\d+: error: [^\n]*?'(\w+)'", r.stderr))
        newly = {t for t in todo if t in errs} - bad
        if not newly:
            raise ExtractError("probe does not compile:\n" + r.stderr[-4000:])
        bad |= newly
    else:
        raise ExtractError("probe does not compile after dropping types")
    out = subprocess.run([str(exe)], capture_output=True, text=True, check=True).stdout
    sizes, blocks = {}, {}
    for line in out.splitlines():
        p = line.split()
        if p[0] == "S":
            sizes[p[1]] = int(p[2])
        elif p[0] == "B":
            blocks.setdefault(p[1], {})[p[2]] = int(p[3])
    return sizes, blocks, idx


# --------------------------------------------------------------------------
# Extract one release
# --------------------------------------------------------------------------

def extract(version, root, source):
    methods = parse_methods(root)
    rmctrl_flags = parse_rmctrl_flags(root)
    names = parse_control_names(root)
    deprecated = parse_deprecated(root, names)
    deferred = parse_deferred(root)
    entries, class_numbers, on_chip = parse_classes(root)
    vidheap = parse_vidheap(root)
    by_name = {n: v for v, ns in names.items() for n in ns}

    types = {e["params_type"] for es in methods.values() for e in es if e["params_type"]}
    with tempfile.TemporaryDirectory() as d:
        sizes, blocks, idx = probe(root, types, Path(d))

    controls = []
    for cmd in sorted(methods):
        es = methods[cmd]
        ptypes = sorted({e["params_type"] for e in es if e["params_type"]})
        psizes = sorted({sizes.get(t) for t in ptypes})
        size = psizes[0] if len(psizes) == 1 and psizes[0] is not None else None
        if not ptypes:
            size = 0
        host = {}
        for t in ptypes:
            for k, v in host_fields(idx, t).items():
                host.setdefault(k, set()).update(v)
        controls.append({
            "cmd": cmd,
            "name": best_name(names.get(cmd), es[0]["func"] or f"{cmd:#010x}"),
            "nvoc": sorted({e["nvoc"] for e in es}),
            "flags": sorted({e["flags"] for e in es}),
            "privilege": sorted({ctrl_privilege(e["flags"], rmctrl_flags) for e in es}),
            "params_type": ptypes,
            "params_size": size,
            # Pointers are gen/rmctrl's to place; here only whether any.
            "pointer": bool(host.pop("pointer", None)),
            "host": {k: sorted(v) for k, v in sorted(host.items())},
        })
    known = {c["cmd"] for c in controls}
    deprecated_rows = []
    for v1, v2 in sorted(deprecated.items()):
        if v1 not in by_name or v2 not in by_name:
            raise ExtractError(f"deprecated {v1} -> {v2}: no FINN number")
        if by_name[v2] not in known:
            raise ExtractError(f"deprecated {v1} -> {v2}: {v2} is not exported")
        deprecated_rows.append({"cmd": by_name[v1], "name": v1, "v2": by_name[v2]})
    deferred_rows = []
    for n in deferred:
        if n not in by_name:
            raise ExtractError(f"deferred {n}: no FINN number")
        deferred_rows.append({"cmd": by_name[n], "name": n})

    classes = []
    for e in entries:
        if e["name"] not in class_numbers:
            raise ExtractError(f"class {e['name']}: no number in g_allclasses.h")
        classes.append({
            "class": class_numbers[e["name"]], "name": e["name"], "nvoc": e["nvoc"],
            "privilege": class_privilege(e["flags"]), "params_type": e["params"],
            "on_chip": e["name"] in on_chip, "debug_only": e["debug_only"],
        })
    classes.sort(key=lambda c: (c["class"], c["name"]))

    for s, fields in OS_BLOCKS.items():
        if s not in blocks or any(f not in blocks[s] for f in fields):
            raise ExtractError(f"probe did not measure {s}")
    return {
        "format": FORMAT, "version": version, "abi": "x86_64 LP64", "source": source,
        "rmctrl_flags": rmctrl_flags,
        "escape_blocks": blocks,
        "vidheap_functions": vidheap,
        "controls": controls,
        "deprecated": deprecated_rows,
        "deferred": deferred_rows,
        "classes": classes,
    }


# --------------------------------------------------------------------------
# The policy, applied
# --------------------------------------------------------------------------

def read_observed(path=OBSERVED):
    """observed.txt: `classes` and `controls` lines of hex numbers."""
    got = {"classes": set(), "controls": set(), "escapes": set()}
    for line in path.read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        key, *vals = line.split()
        if key in got:
            got[key] |= {int(v, 16) for v in vals}
    return got


def apply_policy(rel, observed):
    """(allowed controls {cmd: row}, allowed classes {class: row}, refused
    reasons {cmd: why}) for one release's measurement."""
    ctl = {c["cmd"]: c for c in rel["controls"]}
    by_name = {c["name"]: c["cmd"] for c in rel["controls"]}
    dep = {d["cmd"]: d for d in rel["deprecated"]}
    by_name.update({d["name"]: d["cmd"] for d in rel["deprecated"]})

    classes = {}
    for c in rel["classes"]:
        # One number may have several rows (NV01_ROOT's aliases): any row.
        classes.setdefault(c["class"], []).append(c)
    cls_by_name = {c["name"]: c["class"] for c in rel["classes"]}
    allow_cls, why_not_cls = {}, {}
    for num, rows in classes.items():
        names = {r["name"] for r in rows}
        # Clients are made by serverAllocClient, not by the class privilege
        # check, and escape.c makes every user root NV01_ROOT_CLIENT.
        user = any((r["privilege"] == "user" or r["nvoc"] == "RmClientResource")
                   and not r["debug_only"] for r in rows)
        # Per-architecture classes must be on some GPU this release drives;
        # the rest are not per-GPU (535's lists name only engine classes).
        chip = any(r["on_chip"] or r["nvoc"] not in ARCH_NVOC for r in rows)
        wanted = (num in observed["classes"]
                  or any(r["nvoc"] in OWN_OBJECT_CLASSES_NVOC for r in rows)
                  or names & set(WORKLOAD_CLASSES))
        if names & DENY_CLASSES:
            why_not_cls[num] = "denied"
        elif not user:
            why_not_cls[num] = "not user-allocatable"
        elif not chip:
            why_not_cls[num] = "no supported GPU has it"
        elif not wanted:
            why_not_cls[num] = "no workload asks for it"
        else:
            allow_cls[num] = sorted(names)[0]
    allowed_nvoc = {r["nvoc"] for num in allow_cls for r in classes[num]}

    allow, why_not = {}, {}
    workload = {by_name[n] for n in WORKLOAD_CONTROLS if n in by_name}
    for cmd, c in ctl.items():
        own = (any(OWN_OBJECT_NVOC.get(n, set()) & allowed_nvoc for n in c["nvoc"])
               and c["name"] not in OWN_OBJECT_EXCLUDE)
        wanted = cmd in observed["controls"] or cmd in workload or own
        host = set(c["host"])
        if c["name"] in DENY_CONTROLS:
            why_not[cmd] = "denied: " + DENY_CONTROLS[c["name"]]
        elif "user" not in c["privilege"]:
            why_not[cmd] = "/".join(c["privilege"])
        elif host and c["name"] not in TRANSLATED and c["name"] not in HOST_FIELD_IGNORED:
            why_not[cmd] = "names a host " + "/".join(sorted(host))
        elif not wanted:
            why_not[cmd] = "no workload asks for it"
        else:
            allow[cmd] = {"name": c["name"], "size": c["params_size"]}
    # A deprecated V1 control RM converts to its V2 before any table: it is
    # what its V2 is, with no size of its own (the V1 block is converted).
    for cmd, d in dep.items():
        if d["v2"] in allow and (cmd in observed["controls"] or cmd in workload
                                 or d["v2"] in observed["controls"] or d["v2"] in workload):
            allow[cmd] = {"name": d["name"], "size": None}
        elif cmd not in allow:
            why_not.setdefault(cmd, "its V2 is not allowed")
    # Controls RM passes to GSP-RM with no CPU-RM table to name them: those
    # with the GSS legacy bit, and every control of an NV2081_BINAPI object
    # (binapiControl). Only the observed ones, by number, and never a
    # privileged GSS legacy one.
    for cmd in sorted(observed["controls"]):
        if cmd in ctl or cmd in dep:
            continue
        if cmd >> 16 == BINAPI_CLASS:
            allow[cmd] = {"name": f"NV2081_BINAPI_{cmd & 0xffff:#06x}", "size": None}
        elif cmd & GSS_LEGACY_MASK:
            if cmd & GSS_LEGACY_PRIVILEGED == GSS_LEGACY_PRIVILEGED:
                raise ExtractError(f"observed {cmd:#010x} is a privileged GSS legacy control")
            allow[cmd] = {"name": f"GSS_LEGACY_{cmd:#010x}",
                          "size": GSS_LEGACY_SIZES.get(rel["version"], {}).get(cmd)}
        # else: a control this release does not have (it came later).
    # An observed call RM serves an unprivileged caller in this release
    # must be allowed: one refused here would be a policy bug.
    for cmd in observed["controls"]:
        c = ctl.get(cmd)
        if cmd not in allow and c and "user" in c["privilege"]:
            raise ExtractError(f"{rel['version']}: observed {c['name']} refused: {why_not.get(cmd)}")
    for num in observed["classes"]:
        rows = classes.get(num, [])
        if num not in allow_cls and any(r["privilege"] == "user" for r in rows):
            raise ExtractError(f"{rel['version']}: observed class {num:#06x} refused: "
                               f"{why_not_cls.get(num)}")
    return allow, allow_cls, why_not, why_not_cls, cls_by_name


# --------------------------------------------------------------------------
# Render
# --------------------------------------------------------------------------

def load_all(out_dir=OUT_DIR):
    data = []
    for v in VERSIONS:
        p = out_dir / f"{v}.json"
        if not p.exists():
            raise ExtractError(f"{p} missing; run `{sys.argv[0]} extract {v}`")
        data.append(json.loads(p.read_text()))
    return data


def rust_str(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def render(data, observed):
    blocks = data[0]["escape_blocks"]
    for rel in data[1:]:
        if rel["escape_blocks"] != blocks:
            raise ExtractError(f"{rel['version']}: escape block layout differs from "
                               f"{data[0]['version']}'s")
    vidheap = data[0]["vidheap_functions"]
    for rel in data[1:]:
        if rel["vidheap_functions"] != vidheap:
            raise ExtractError(f"{rel['version']}: NVOS32 function numbers differ")

    # A name in the policy no release has is a typo, not a policy.
    every = {c["name"] for rel in data for c in rel["controls"]}
    every |= {d["name"] for rel in data for d in rel["deprecated"]}
    every_cls = {c["name"] for rel in data for c in rel["classes"]}
    for n in list(WORKLOAD_CONTROLS) + list(TRANSLATED) + list(HOST_FIELD_IGNORED) \
            + list(OWN_OBJECT_EXCLUDE):
        if n not in every:
            raise ExtractError(f"policy names {n}, which no release measured has")
    for n in WORKLOAD_CLASSES:
        if n not in every_cls:
            raise ExtractError(f"policy names class {n}, which no release measured has")
    names, cls_names, releases = {}, {}, []
    for rel in data:
        allow, allow_cls, _, _, cls_by_name = apply_policy(rel, observed)
        for c in rel["controls"]:
            names.setdefault(c["cmd"], c["name"])
        for d in rel["deprecated"]:
            names.setdefault(d["cmd"], d["name"])
        for cmd, a in allow.items():
            names.setdefault(cmd, a["name"])
        for c in rel["classes"]:
            cls_names.setdefault(c["class"], c["name"])
        vh = []
        for f, num in sorted(vidheap.items(), key=lambda kv: kv[1]):
            need = VIDHEAP_FUNCTIONS[f]
            if all(cls_by_name.get(n) in allow_cls for n in need):
                vh.append((num, f))
        total_ctl = len({c["cmd"] for c in rel["controls"]} | {d["cmd"] for d in rel["deprecated"]})
        unserved = sorted(c for c in observed["controls"] if c not in allow)
        unserved_cls = sorted(c for c in observed["classes"] if c not in allow_cls)
        releases.append({
            "unserved": unserved, "unserved_cls": unserved_cls,
            "version": rel["version"], "allow": allow, "allow_cls": allow_cls,
            "vidheap": vh, "total_ctl": total_ctl,
            "total_cls": len({c["class"] for c in rel["classes"]}),
            "deferred": sorted(d["cmd"] for d in rel["deferred"]),
        })

    out = [
        "// SPDX-License-Identifier: Apache-2.0",
        "// @generated by gen/rmallow_extract.py render -- do not edit.",
        "//",
        "// The RM allowlist per release: which RM_CONTROL commands and RM_ALLOC",
        "// classes a guest may have the backend send (device/src/rmallow.rs).",
        "// Measured from NVIDIA's sources at each tag (gen/rmallow/<version>.json)",
        "// and filtered by the policy in gen/rmallow_extract.py.",
        f"// Releases: {', '.join(VERSIONS)}.",
        "",
        "/// One allowed control: its command and RM's exact parameter size, or",
        "/// `None` where the size is not RM's to check (a deprecated V1 block,",
        "/// a GSS legacy control, one with no measured type).",
        "#[derive(Clone, Copy, Debug, PartialEq, Eq)]",
        "pub struct Control {",
        "    pub cmd: u32,",
        "    pub size: Option<u32>,",
        "}",
        "",
        "/// One release's allowlist.",
        "#[derive(Debug)]",
        "pub struct Release {",
        "    pub version: (u32, u32, u32),",
        "    /// Sorted by `cmd`.",
        "    pub controls: &'static [Control],",
        "    /// Sorted.",
        "    pub classes: &'static [u32],",
        "    /// NVOS32 functions whose classes are all allowed, sorted.",
        "    pub vidheap: &'static [u32],",
        "    /// The controls NV5080 DEFERRED_API runs, sorted.",
        "    pub deferred: &'static [u32],",
        "    /// Observed controls and classes (gen/rmallow/observed.txt) this",
        "    /// release does not have, or would refuse an unprivileged caller",
        "    /// itself: the only observed ones not allowed. Sorted.",
        "    pub unserved_controls: &'static [u32],",
        "    pub unserved_classes: &'static [u32],",
        "    /// How many controls and classes the release exports in all.",
        "    pub total_controls: usize,",
        "    pub total_classes: usize,",
        "}",
        "",
        "/// Offsets of the escape blocks' fields the gate reads (nvos.h, the",
        "/// same in every release measured).",
    ]
    for s, fields in sorted(blocks.items()):
        short = s.replace("_PARAMETERS", "")
        for f, v in sorted(fields.items()):
            cname = short + "_" + re.sub(r"(?<!^)([A-Z])", r"_\1", f).upper()
            out.append(f"pub const {cname}: usize = {v};")
    out.append("")
    out.append("/// NVOS32 function numbers (nvos.h).")
    for f, num in sorted(vidheap.items(), key=lambda kv: kv[1]):
        out.append(f"pub const {f}: u32 = {num};")
    out.append("")
    out.append("/// Every release measured, oldest first.")
    out.append("pub static RELEASES: &[Release] = &[")
    for r in releases:
        a, b, c = (int(x) for x in r["version"].split("."))
        out.append("    Release {")
        out.append(f"        version: ({a}, {b}, {c}),")
        out.append("        controls: &[")
        for cmd in sorted(r["allow"]):
            size = r["allow"][cmd]["size"]
            sz = "None" if size is None else f"Some({size})"
            out.append(f"            Control {{ cmd: {cmd:#010x}, size: {sz} }},")
        out.append("        ],")
        out.append("        classes: &[")
        for num in sorted(r["allow_cls"]):
            out.append(f"            {num:#06x}, // {r['allow_cls'][num]}")
        out.append("        ],")
        out.append("        vidheap: &[" + ", ".join(f"{n}" for n, _ in r["vidheap"]) + "],")
        out.append("        deferred: &[" + ", ".join(f"{c:#010x}" for c in r["deferred"]) + "],")
        out.append("        unserved_controls: &[" + ", ".join(f"{c:#010x}" for c in r["unserved"]) + "],")
        out.append("        unserved_classes: &[" + ", ".join(f"{c:#06x}" for c in r["unserved_cls"]) + "],")
        out.append(f"        total_controls: {r['total_ctl']},")
        out.append(f"        total_classes: {r['total_cls']},")
        out.append("    },")
    out.append("];")
    out.append("")
    out.append("/// Every control any release exports, by name, for the log (sorted).")
    out.append("pub static CONTROL_NAMES: &[(u32, &str)] = &[")
    for cmd in sorted(names):
        out.append(f"    ({cmd:#010x}, {rust_str(names[cmd])}),")
    out.append("];")
    out.append("")
    out.append("/// Every class any release defines, by name, for the log (sorted).")
    out.append("pub static CLASS_NAMES: &[(u32, &str)] = &[")
    for num in sorted(cls_names):
        out.append(f"    ({num:#06x}, {rust_str(cls_names[num])}),")
    out.append("];")
    out.append("")
    return "\n".join(out)


def report(data, observed):
    for rel in data:
        allow, allow_cls, why, why_cls, _ = apply_policy(rel, observed)
        total_ctl = len({c["cmd"] for c in rel["controls"]} | {d["cmd"] for d in rel["deprecated"]})
        total_cls = len({c["class"] for c in rel["classes"]})
        reasons = {}
        for w in why.values():
            k = w.split(":")[0] if w.startswith("denied") else w
            reasons[k] = reasons.get(k, 0) + 1
        print(f"{rel['version']}: controls {len(allow)}/{total_ctl} allowed, "
              f"classes {len(allow_cls)}/{total_cls} allowed")
        print("   refused controls: " + ", ".join(f"{k} {v}" for k, v in sorted(reasons.items())))
        creasons = {}
        for w in why_cls.values():
            creasons[w] = creasons.get(w, 0) + 1
        print("   refused classes: " + ", ".join(f"{k} {v}" for k, v in sorted(creasons.items())))


def write_json(path, obj):
    """One row a line: the files stay reviewable as diffs."""
    path.parent.mkdir(parents=True, exist_ok=True)
    out = ["{"]
    keys = list(obj)
    for i, k in enumerate(keys):
        v = obj[k]
        sep = "," if i + 1 < len(keys) else ""
        if isinstance(v, list):
            out.append(f" {json.dumps(k)}: [")
            for j, row in enumerate(v):
                out.append("  " + json.dumps(row, separators=(",", ":"))
                           + ("," if j + 1 < len(v) else ""))
            out.append(" ]" + sep)
        else:
            out.append(f" {json.dumps(k)}: {json.dumps(v)}{sep}")
    out.append("}")
    path.write_text("\n".join(out) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract")
    e.add_argument("version")
    e.add_argument("--src", type=Path, help="a local open-gpu-kernel-modules tree")
    sub.add_parser("all")
    r = sub.add_parser("render")
    r.add_argument("--out", type=Path, help="write generated.rs here instead of gen/src/rmallow/")
    sub.add_parser("check")
    sub.add_parser("report")
    for p in (e, sub.choices["all"], sub.choices["check"]):
        p.add_argument("--cache", type=Path, default=DEFAULT_CACHE)
    a = ap.parse_args()
    try:
        observed = read_observed()
        if a.cmd == "extract":
            if a.src:
                root, source = a.src.resolve(), local_source(a.src.resolve())
            else:
                a.cache.mkdir(parents=True, exist_ok=True)
                root = fetch(a.version, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
            write_json(OUT_DIR / f"{a.version}.json", extract(a.version, root, source))
        elif a.cmd == "all":
            a.cache.mkdir(parents=True, exist_ok=True)
            for v in VERSIONS:
                root = fetch(v, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
                write_json(OUT_DIR / f"{v}.json", extract(v, root, source))
            RUST_OUT.parent.mkdir(parents=True, exist_ok=True)
            RUST_OUT.write_text(render(load_all(), observed))
        elif a.cmd == "render":
            text = render(load_all(), observed)
            if a.out:
                a.out.mkdir(parents=True, exist_ok=True)
                (a.out / "generated.rs").write_text(text)
            else:
                RUST_OUT.parent.mkdir(parents=True, exist_ok=True)
                RUST_OUT.write_text(text)
        elif a.cmd == "report":
            report(load_all(), observed)
        elif a.cmd == "check":
            a.cache.mkdir(parents=True, exist_ok=True)
            stale = []
            for v in VERSIONS:
                root = fetch(v, a.cache)
                source = json.loads((root / "SOURCE.json").read_text())
                fresh = extract(v, root, source)
                p = OUT_DIR / f"{v}.json"
                if not p.exists() or json.loads(p.read_text()) != fresh:
                    stale.append(str(p))
            if not RUST_OUT.exists() or RUST_OUT.read_text() != render(load_all(), observed):
                stale.append(str(RUST_OUT))
            if stale:
                print("stale: " + ", ".join(stale) + f"; run {sys.argv[0]} all", file=sys.stderr)
                return 1
            print("gen/rmallow and gen/src/rmallow/generated.rs are up to date")
    except ExtractError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
