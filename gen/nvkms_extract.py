#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Extract the NVKMS and nvidia-drm ioctl layouts of NVIDIA driver releases.

/dev/nvidia-modeset takes one ioctl whose argument names a command and a
pointer to that command's `NvKms*Params`. Both the command numbers and the
params layouts move between driver releases (REGISTER_SURFACE is 16 in some,
17 in others), and a forwarder that patches fds and pointers at the wrong
offsets hands the host kernel guest garbage. nvidia-drm's private ioctls are
numbered more stably, but their structs and the set that is registered at all
still change. So the layouts are measured, per release, from NVIDIA's own
headers -- never transcribed.

For one driver version this:

1. fetches the headers from the open-gpu-kernel-modules tag (a tarball
   streamed through a path filter: only the interface and common headers, plus
   the two .c files whose tables are the ground truth -- nvkms.c's ioctl
   dispatch table and nvidia-drm-drv.c's ioctl registration table);
2. takes the command -> params mapping from nvkms.c's dispatch table exactly
   as the kernel builds it, and the nvidia-drm ioctls from their header
   (nv_drm_common_ioctl.h, or nvidia-drm-ioctl.h before 595) and the
   registration table in nvidia-drm-drv.c;
3. compiles and runs a C probe against those headers for every number it
   reports: enum values, sizeof/offsetof, array counts, constants, and the
   plane count of every surface format (by linking NVIDIA's own
   nvkms-format.c);
4. parses the preprocessed headers and walks every params struct (and every
   pointee the spec below names) for fields that look like user pointers or
   fds, and refuses to write anything if the spec does not account for each
   one, or names a field the headers no longer have.

The spec (SPEC and DRM_SPEC below) is the only hand-written part: which
fields are pointers, fds, counts and conditions, and what they mean. It is
written once against the field *names*; the offsets always come from the
compiler. A new release that adds a pointer or fd field fails here, loudly,
instead of producing a table that silently forwards it.

    ./nvkms_extract.py all                     # every version in VERSIONS
    ./nvkms_extract.py extract 610.57.04       # one version (fetches if needed)
    ./nvkms_extract.py extract 610.57.04 --src ../../nvidia-driver
    ./nvkms_extract.py check                   # regenerate, compare with nvkms/
    ./nvkms_extract.py diff                    # print the cross-version summary
    ./nvkms_extract.py selftest                # prove the refusals below still fire

Needs Python 3.8+, a C compiler (gcc) with Linux uapi headers (<drm/drm.h>),
and network access for fetching (headers are cached, see --cache). Output:
gen/nvkms/<version>.json, format documented in gen/nvkms/README.md. The
probes run on the build machine, so the numbers are for its ABI: x86_64
(LP64) is what is checked in.
"""

import argparse
import fnmatch
import hashlib
import json
import os
import platform
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
OUT_DIR = HERE / "nvkms"
REPO = "NVIDIA/open-gpu-kernel-modules"
FORMAT = "virtio-nvgpu/nvkms-layout/1"
DEFAULT_CACHE = Path(os.environ.get("NVKMS_EXTRACT_CACHE",
                                    Path(tempfile.gettempdir()) / "ogkm"))

# The releases the project cares about, and why. Order is release order; the
# diff summary compares neighbours.
VERSIONS = {
    "535.129.03": "ABI profile gen/src/versions/v535_129_03.rs",
    "580.178.04": "ABI profile gen/src/versions/v580_178_04.rs",
    "595.71.05": "ABI profile gen/src/versions/v595_71_05.rs",
    "595.99.02": "RTX 3060 box (README: every benchmark number)",
    "610.57.04": "nvidia-driver/ source tree the display work is written against",
    "615.71.09": "RTX A2000 box (README)",
}

# Paths (relative to the repository root) pulled out of a release tarball.
# "*" never crosses a directory.
FETCH = [
    "version.mk",
    "src/nvidia-modeset/interface/*",
    "src/nvidia-modeset/src/nvkms.c",
    "src/nvidia-modeset/lib/nvkms-format.c",
    "src/nvidia-modeset/kapi/interface/*",
    "src/common/sdk/nvidia/inc/*",
    "src/common/inc/*",
    "src/common/unix/common/inc/*",
    "kernel-open/nvidia-drm/*ioctl*.h",
    "kernel-open/nvidia-drm/nvidia-drm-drv.c",
]

INCLUDE_DIRS = [
    "src/nvidia-modeset/interface",
    "src/nvidia-modeset/kapi/interface",
    "src/common/sdk/nvidia/inc",
    "src/common/inc",
    "src/common/unix/common/inc",
]


class ExtractError(Exception):
    """A header, table or field the extractor relies on is not what it expects."""


# --------------------------------------------------------------------------
# The spec.
#
# Paths are C member designators. A node's path, and every path it refers to
# (counts, conditions, sizes), are relative to the node's scope: the params
# struct at the top level, the array element inside ARR, the pointee inside
# PTR's elem. The compiler resolves every one of them; nothing here is an
# offset.
# --------------------------------------------------------------------------

def VAL(path, role, note=None):
    """A plain field a forwarder or its policy needs to read or rewrite.

    role: target (dpy/head/disp/device/surface a request acts on), policy
    (a field the backend must vet or sanitise), count, cond, status, info.
    """
    return {"kind": "field", "path": path, "role": role, "note": note}


def FD(path, fd_kinds, cond=(), note=None):
    """An fd the kernel resolves with fget() in the caller: must be translated."""
    return {"kind": "fd_in", "path": path, "fd_kinds": list(fd_kinds),
            "cond": list(cond), "note": note}


def FDOUT(path, cond=(), note=None):
    """An fd the kernel installs in the caller's table and reports here."""
    return {"kind": "fd_out", "path": path, "cond": list(cond), "note": note}


def GEMIN(path, cond=(), note=None):
    return {"kind": "gem_in", "path": path, "cond": list(cond), "note": note}


def GEMOUT(path, cond=(), note=None):
    return {"kind": "gem_out", "path": path, "cond": list(cond), "note": note}


def SYNCOBJIN(path, note=None):
    """A DRM syncobj handle in the calling drm_file."""
    return {"kind": "syncobj_in", "path": path, "cond": [], "note": note}


def KPTR(path, note):
    """A kernel pointer in a kernel-client-only command: never forwardable."""
    return {"kind": "kernel_ptr", "path": path, "note": note}


def UVA(path, note):
    """A raw user virtual address the kernel pins or maps in the caller's mm."""
    return {"kind": "user_va", "path": path, "note": note}


def PTR(path, direction, length, present, elem=None, elem_fields=(), note=None):
    """A user pointer (NvU64/__u64) the kernel copies from/to during the call.

    length: see L_* below. present: when the kernel dereferences it
    ("nonzero": pointer != 0; "always"; ("count_nonzero"|"size_nonzero", ref)).
    elem: the C type the pointee is an array of (or is), whose own pointer/fd
    fields are listed in elem_fields relative to one element.
    """
    return {"kind": "ptr", "path": path, "dir": direction, "len": length,
            "present": present, "elem": elem, "elem_fields": list(elem_fields),
            "note": note}


def ARR(path, fields, valid=("all",), note=None):
    """An inline array; fields are relative to one element.

    valid: which elements the kernel interprets -- ("all",),
    ("count", ref, max), ("format_num_planes", format_ref, when_conds).
    """
    return {"kind": "array", "path": path, "fields": list(fields),
            "valid": valid, "note": note}


def ver(node, since=None, before=None):
    """Limit a spec node to releases in [since, before): for fields NVIDIA renamed
    or added. Bounds are releases this extractor has seen; a release between two
    of them that disagrees fails the extraction instead of being guessed at."""
    node["since"], node["before"] = since, before
    return node


def L_CONST(ctype):
    return ("const", ctype)


def L_COUNT(count, ctype, minimum, maximum):
    """count * sizeof(ctype) bytes; count read from the field `count`."""
    return ("count", count, ctype, minimum, maximum)


def L_BYTES(size, maximum, written=None):
    """`size` bytes, capped at `maximum`; `written` = bytes actually written."""
    return ("bytes", size, maximum, written)


def L_SIZE_EQ(size, ctype):
    """`size` bytes, and the kernel refuses anything but sizeof(ctype)."""
    return ("size_eq", size, ctype)


# fd kinds, as the kernel checks them.
MODESET_FRESH = "modeset_fresh"          # /dev/nvidia-modeset never ioctl'd (type Undefined)
MODESET_UNICAST = "modeset_unicast"      # /dev/nvidia-modeset already of type UnicastEvent
NVIDIACTL_EXPORTED = "nvidiactl_exported"     # /dev/nvidiactl an RM object was exported to; the kernel imports it
NVIDIACTL_EXPORT_TARGET = "nvidiactl_export_target"  # /dev/nvidiactl the kernel exports an RM object into
DMABUF = "dmabuf"
SYNC_FILE = "sync_file"


def modeset_granted(what):
    return f"modeset_grant_{what}"       # /dev/nvidia-modeset of type Grant<What>


SYNCPT_PRE = [("specified", "nonzero"), ("val.useSyncpt", "nonzero"),
              ("val.u.syncpts.pre.type", "eq", "NVKMS_SYNCPT_TYPE_FD")]


def lut_ramps(prefix, since=None, before=None):
    """The two LUT ramp pointers of a struct NvKmsSetLutCommonParams at prefix."""
    ramps = dict(direction="in", length=L_CONST("struct NvKmsLutRamps"),
                 present="nonzero", elem="struct NvKmsLutRamps",
                 note="copied in by CopyInLutParams whenever non-zero")
    return [ver(PTR(f"{prefix}.input.pRamps", **ramps), since, before),
            ver(PTR(f"{prefix}.output.pRamps", **ramps), since, before)]


# NvKmsFlipCommonParams gained its LUT (and SET_MODE heads lost theirs, moving
# into the embedded flip) between 535 and 580.
FLIP_LUT_SINCE = "580.178.04"


def flip_common(prefix):
    """struct NvKmsFlipCommonParams, as embedded in FLIP heads and SET_MODE heads."""
    return lut_ramps(f"{prefix}.lut", since=FLIP_LUT_SINCE) + [
        ARR(f"{prefix}.layer", [
            FD("syncObjects.val.u.syncpts.pre.u.fd", [SYNC_FILE],
               cond=[(f"syncObjects.{c[0]}",) + tuple(c[1:]) for c in SYNCPT_PRE],
               note="Tegra syncpoints only; dGPU rejects useSyncpt"),
            VAL("syncObjects.specified", "cond"),
            VAL("syncObjects.val.useSyncpt", "policy",
                note="non-zero is a Tegra-only path; a dGPU host fails the flip"),
            VAL("syncObjects.val.u.syncpts.requestedPostType", "cond"),
            VAL("completionNotifier.val.awaken", "policy",
                note="makes the flip's completion broadcast FLIP_OCCURRED to every "
                     "open with flip permission on the head (nvkms-evo3.c, "
                     "nvSendFlipOccurredEventEvo) -- nvidia-drm's own included"),
        ]),
    ]


def post_syncpt(prefix):
    return ARR(prefix, [
        FDOUT("postSyncpt.u.fd",
              cond=[("postSyncpt.type", "eq", "NVKMS_SYNCPT_TYPE_FD")],
              note="Tegra only; produced when requestedPostType was FD"),
    ])


# NvKmsFlipPermissions / NvKmsModesetPermissions were per (disp, head) until
# they became per head (one disp per device) between 580 and 595.
PERM_PER_HEAD_SINCE = "595.71.05"


def permissions(prefix, role):
    return [
        VAL(f"{prefix}.type", role),
        ver(ARR(f"{prefix}.flip.head", [VAL("layerMask", role)]),
            since=PERM_PER_HEAD_SINCE),
        ver(ARR(f"{prefix}.modeset.head", [VAL("dpyIdList", role)]),
            since=PERM_PER_HEAD_SINCE),
        ver(ARR(f"{prefix}.flip.disp", [ARR("head", [VAL("layerMask", role)])]),
            before=PERM_PER_HEAD_SINCE),
        ver(ARR(f"{prefix}.modeset.disp", [ARR("head", [VAL("dpyIdList", role)])]),
            before=PERM_PER_HEAD_SINCE),
    ]


def info_string(written):
    return PTR("request.pInfoString", "out",
               L_BYTES("request.infoStringSize",
                       "NVKMS_MODE_VALIDATION_MAX_INFO_STRING_LENGTH", written),
               ("size_nonzero", "request.infoStringSize"),
               note="ignored (and zeroed) when infoStringSize is 0; the kernel "
                    "copies out reply.infoStringLenWritten bytes")


# Keyed by the dispatch table's _func name (NvKms<func>Params). Every command in
# a release's dispatch table must have an entry, even an empty one.
SPEC = {
    "AllocDevice": [
        VAL("request.versionString", "policy", "must equal the host NV_VERSION_STRING"),
        ver(VAL("request.deviceId", "target", "a bare RM GPU id"), before="580.178.04"),
        ver(VAL("request.deviceId.rmDeviceId", "target"), since="580.178.04"),
        ver(VAL("request.deviceId.migDevice", "target"), since="580.178.04"),
        ver(VAL("request.sliMosaic", "policy"), before="580.178.04"),
        ver(VAL("request.tryInferSliMosaicFromExistingDevice", "policy"),
            before="580.178.04"),
        VAL("request.no3d", "policy"),
        VAL("request.enableConsoleHotplugHandling", "policy",
            "global side effect on the device"),
        ARR("request.registryKeys", [VAL("name", "policy"), VAL("value", "policy")]),
        VAL("reply.status", "status"),
        VAL("reply.deviceHandle", "info"),
        VAL("reply.numDisps", "info"),
        VAL("reply.dispHandles", "info"),
        VAL("reply.supportsSyncpts", "info"),
        ver(VAL("reply.vtFbBaseAddress", "policy", "only filled for kernel clients"),
            since="580.178.04"),
        ver(VAL("reply.vtFbSize", "policy", "only filled for kernel clients"),
            since="580.178.04"),
    ],
    "FreeDevice": [],
    "QueryDisp": [],
    "QueryConnectorStaticData": [],
    "QueryConnectorDynamicData": [],
    "QueryDpyStaticData": [],
    "QueryDpyDynamicData": [
        VAL("request.dispHandle", "target"),
        VAL("request.dpyId", "target"),
        VAL("request.forceConnected", "policy", "persists in the dpy (nvkms-dpy.c)"),
        VAL("request.forceDisconnected", "policy", "persists in the dpy"),
        ver(VAL("request.overrideEdid", "policy", "persists in the dpy"),
            before="615.71.09"),
        ver(VAL("request.ignoreEdid", "policy"), before="615.71.09"),
        ver(VAL("request.overrideMetadata", "policy",
                "overrideEdid renamed: edid.buffer is any display metadata"),
            since="615.71.09"),
        ver(VAL("request.ignoreMetadata", "policy", "ignoreEdid renamed"),
            since="615.71.09"),
        VAL("request.ignoreEdidChecksum", "policy"),
        VAL("request.allowDVISpecPClkOverride", "policy", "persists in the dpy"),
        VAL("request.dpInbandStereoSignaling", "policy"),
        VAL("request.disableACPIBrightnessHotkeys", "policy"),
        VAL("request.edid.bufferSize", "policy"),
        VAL("request.edid.buffer", "policy"),
    ],
    "ValidateModeIndex": [
        VAL("request.dpyId", "target"),
        VAL("request.infoStringSize", "count"),
        info_string("reply.infoStringLenWritten"),
        VAL("reply.infoStringLenWritten", "count"),
    ],
    "ValidateMode": [
        VAL("request.dpyId", "target"),
        VAL("request.infoStringSize", "count"),
        info_string("reply.infoStringLenWritten"),
        VAL("reply.infoStringLenWritten", "count"),
    ],
    "SetMode": [
        VAL("request.deviceHandle", "target"),
        VAL("request.commit", "policy", "FALSE = validate only, needs no permission"),
        VAL("request.requestedDispsBitMask", "target"),
        ARR("request.disp", [
            VAL("requestedHeadsBitMask", "target"),
            ARR("head", [VAL("dpyIdList", "target")] + flip_common("flip")
                + lut_ramps("lut", before=FLIP_LUT_SINCE)),
        ], note="SetModePrepUser copies the LUT ramps of all disps and heads, "
                "whatever the requested bitmasks say"),
        VAL("reply.status", "status"),
        ARR("reply.disp", [ARR("head", [post_syncpt("flipReply.layer")])]),
    ],
    "SetCursorImage": [
        VAL("request.deviceHandle", "target"),
        VAL("request.dispHandle", "target"),
        VAL("request.head", "target"),
        VAL("request.common.surfaceHandle", "target"),
    ],
    "MoveCursor": [
        VAL("request.deviceHandle", "target"),
        VAL("request.dispHandle", "target"),
        VAL("request.head", "target"),
    ],
    "SetLut": [
        VAL("request.deviceHandle", "target"),
        VAL("request.dispHandle", "target"),
        VAL("request.head", "target"),
    ] + lut_ramps("request.common"),
    "CheckLutNotifier": [
        VAL("request.waitForCompletion", "policy", "TRUE blocks in the kernel"),
    ],
    "IdleBaseChannel": [],
    "Flip": [
        VAL("request.deviceHandle", "target"),
        PTR("request.pFlipHead", "in",
            L_COUNT("request.numFlipHeads", "struct NvKmsFlipRequestOneHead",
                    1, "NV_MAX_FLIP_REQUEST_HEADS"),
            "always", elem="struct NvKmsFlipRequestOneHead",
            elem_fields=[VAL("sd", "target"), VAL("head", "target")]
            + flip_common("flip"),
            note="FlipPrepUser refuses numFlipHeads outside [1, max]"),
        VAL("request.numFlipHeads", "count"),
        VAL("request.commit", "policy"),
        ver(VAL("reply.flipResult", "status"), since="580.178.04"),
        ver(VAL("reply.vrrSemaphoreIndex", "info"), before="580.178.04"),
        ARR("reply.flipHead", [post_syncpt("layer")]),
    ],
    "DeclareDynamicDpyInterest": [],
    "RegisterSurface": [
        VAL("request.deviceHandle", "target"),
        VAL("request.useFd", "cond", "NvBool: only the low byte counts"),
        VAL("request.rmClient", "policy", "honoured only when !useFd, which user clients may not use"),
        VAL("request.format", "count"),
        ARR("request.planes", [FD("u.fd", [NVIDIACTL_EXPORTED, DMABUF])],
            valid=("format_num_planes", "request.format", [("request.useFd", "nonzero")]),
            note="planes [0, numPlanes(format)) are read, as fds when useFd"),
        VAL("reply.surfaceHandle", "info"),
    ],
    "UnregisterSurface": [
        ver(VAL("request.skipSync", "policy", "user clients must pass 0"),
            since="580.178.04"),
    ],
    "GrantSurface": [FD("request.fd", [MODESET_FRESH])],
    "AcquireSurface": [FD("request.fd", [modeset_granted("surface")])],
    "ReleaseSurface": [],
    "SetDpyAttribute": [
        VAL("request.deviceHandle", "target"),
        VAL("request.dispHandle", "target"),
        VAL("request.dpyId", "target"),
        VAL("request.attribute", "policy"),
        VAL("request.value", "policy"),
    ],
    "GetDpyAttribute": [],
    "GetDpyAttributeValidValues": [],
    "SetDispAttribute": [
        VAL("request.deviceHandle", "target"),
        VAL("request.dispHandle", "target"),
        VAL("request.attribute", "policy"),
        VAL("request.value", "policy"),
    ],
    "GetDispAttribute": [],
    "GetDispAttributeValidValues": [],
    "QueryFrameLock": [],
    "SetFrameLockAttribute": [
        VAL("request.frameLockHandle", "target"),
        VAL("request.attribute", "policy"),
        VAL("request.value", "policy"),
    ],
    "GetFrameLockAttribute": [],
    "GetFrameLockAttributeValidValues": [],
    "GetNextEvent": [
        VAL("reply.valid", "status"),
        VAL("reply.event", "info"),
        ver(KPTR("reply.event.u.dpyCpTopologyChanged.topology",
                 "kernel pointer, only in DPY_CP_TOPOLOGY_CHANGED events, which "
                 "NVKMS never queues for (or lets DECLARE_EVENT_INTEREST ask for "
                 "on) a user client"), since="615.71.09"),
    ],
    "DeclareEventInterest": [
        VAL("request.interestMask", "policy", "bit (1 << NVKMS_EVENT_TYPE_*)"),
    ],
    "ClearUnicastEvent": [FD("request.unicastEventFd", [MODESET_UNICAST])],
    "SetLayerPosition": [
        VAL("request.deviceHandle", "target"),
        VAL("request.requestedDispsBitMask", "target"),
        ARR("request.disp", [
            VAL("requestedHeadsBitMask", "target"),
            ARR("head", [VAL("requestedLayerBitMask", "target"),
                         VAL("layerPosition", "policy")]),
        ]),
    ],
    "GrabOwnership": [VAL("request.deviceHandle", "target")],
    "ReleaseOwnership": [VAL("request.deviceHandle", "target")],
    "GrantPermissions": [
        FD("request.fd", [MODESET_FRESH]),
        VAL("request.deviceHandle", "target"),
    ] + permissions("request.permissions", "policy"),
    "AcquirePermissions": [
        FD("request.fd", [modeset_granted("permissions")]),
        VAL("reply.deviceHandle", "target"),
    ] + permissions("reply.permissions", "target"),
    "RevokePermissions": [
        VAL("request.deviceHandle", "target"),
        VAL("request.permissionsTypeBitmask", "policy"),
    ] + permissions("request.permissions", "policy"),
    "QueryDpyCRC32": [],
    "RegisterDeferredRequestFifo": [
        VAL("request.surfaceHandle", "target", "NVKMS CPU-maps this surface"),
    ],
    "UnregisterDeferredRequestFifo": [],
    "AllocSwapGroup": [],
    "FreeSwapGroup": [],
    "JoinSwapGroup": [
        VAL("request.numMembers", "count"),
        ARR("request.member", [
            FD("unicastEvent.fd", [MODESET_FRESH, MODESET_UNICAST],
               cond=[("unicastEvent.specified", "nonzero")]),
            VAL("unicastEvent.specified", "cond"),
        ], valid=("count", "request.numMembers", "NVKMS_MAX_SWAPGROUPS")),
    ],
    "LeaveSwapGroup": [],
    "SetSwapGroupClipList": [
        VAL("request.nClips", "count"),
        PTR("request.pClipList", "in",
            L_COUNT("request.nClips", "struct NvKmsRect", 0, None),
            ("count_nonzero", "request.nClips"), elem="struct NvKmsRect",
            note="nClips == 0 means no list; the pointer must then be ignored"),
    ],
    "GrantSwapGroup": [FD("request.fd", [MODESET_FRESH])],
    "AcquireSwapGroup": [FD("request.fd", [modeset_granted("swap_group")])],
    "ReleaseSwapGroup": [],
    "SwitchMux": [],
    "GetMuxState": [],
    "EnableVblankSyncObject": [],
    "DisableVblankSyncObject": [],
    "NotifyVblank": [
        VAL("request.head", "target"),
        FD("request.unicastEvent.fd", [MODESET_FRESH, MODESET_UNICAST]),
    ],
    "SetFlipLockGroup": [],
    "EnableVblankSemControl": [
        VAL("request.surfaceHandle", "target", "NVKMS CPU-writes this surface every vblank"),
        VAL("request.surfaceOffset", "policy"),
    ],
    "DisableVblankSemControl": [],
    "AccelVblankSemControls": [],
    "FramebufferConsoleDisabled": [],
    "RegisterVblankIntrCallback": [
        KPTR("request.pCallback", "kernel function pointer; kernel clients only"),
        VAL("request.param", "policy"),
    ],
    "UnregisterVblankIntrCallback": [],
    # Gone after 580: NVKMS exported its own (device-global) VRR semaphore
    # surface into a caller's nvidiactl fd, and let any client signal it.
    "ExportVrrSemaphoreSurface": [
        VAL("request.deviceHandle", "target"),
        FD("request.memFd", [NVIDIACTL_EXPORT_TARGET],
           note="RM EXPORT_OBJECT_TO_FD of NVKMS's VRR semaphore memory into this fd"),
    ],
    "VrrSignalSemaphore": [
        VAL("request.deviceHandle", "target"),
        VAL("request.vrrSemaphoreIndex", "policy", "device-global semaphore"),
    ],
}

# Structs whose every top-level member must be listed here, because a policy
# filter reads them field by field: a member added in a new release could be a
# new way to change global state, and must be looked at before it is let
# through.
EXHAUSTIVE = {
    "struct NvKmsQueryDpyDynamicDataRequest": {
        "deviceHandle", "dispHandle", "dpyId", "forceConnected",
        "forceDisconnected", "overrideEdid", "ignoreEdid", "overrideMetadata",
        "ignoreMetadata", "ignoreEdidChecksum",
        "allowDVISpecPClkOverride", "dpInbandStereoSignaling",
        "disableACPIBrightnessHotkeys", "edid",
    },
    "struct NvKmsAllocDeviceRequest": {
        "versionString", "deviceId", "sliMosaic", "tryInferSliMosaicFromExistingDevice",
        "no3d", "enableConsoleHotplugHandling", "registryKeys",
    },
}

NVKMS_CONSTANTS = [
    "NV_MAX_SUBDEVICES", "NV_MAX_HEADS", "NVKMS_MAX_HEADS_PER_DISP",
    "NVKMS_MAX_LAYERS_PER_HEAD", "NV_MAX_FLIP_REQUEST_HEADS", "NVKMS_MAX_SWAPGROUPS",
    "NV_MAX_DEVICES", "NVKMS_LUT_ARRAY_SIZE", "NVKMS_MAX_PLANES_PER_SURFACE",
    "NVKMS_MAX_EYES", "NVKMS_MODE_VALIDATION_MAX_INFO_STRING_LENGTH",
    "NVKMS_NVIDIA_DRIVER_VERSION_STRING_LENGTH", "NVKMS_MAX_DEVICE_REGISTRY_KEYS",
    "NVKMS_EDID_BUFFER_SIZE", "NVKMS_SYNCPT_TYPE_NONE", "NVKMS_SYNCPT_TYPE_RAW",
    "NVKMS_SYNCPT_TYPE_FD", "NV_KMS_PERMISSIONS_TYPE_FLIPPING",
    "NV_KMS_PERMISSIONS_TYPE_MODESET", ("NV_KMS_PERMISSIONS_TYPE_SUB_OWNER", "580.178.04"),
]


def _consts(names, version):
    """Constant names that exist in `version`: (name, since) entries are gated."""
    out = []
    for n in names:
        if isinstance(n, tuple):
            if _vkey(version) < _vkey(n[1]):
                continue
            n = n[0]
        out.append(n)
    return out

NVKMS_TYPES = [
    "struct NvKmsIoctlParams", "struct NvKmsLutRamps", "struct NvKmsRect",
    "struct NvKmsFlipCommonParams", "struct NvKmsFlipRequestOneHead",
    "struct NvKmsSetModeOneHeadRequest", "struct NvKmsSetModeOneDispRequest",
    "struct NvKmsFlipCommonReplyOneHead", "struct NvKmsPermissions",
    "struct NvKmsEvent", "struct NvKmsDeferredRequestFifo", "NVDpyIdList", "NVDpyId",
]

# nvidia-drm: keyed by the DRM_NVIDIA_<name> suffix.
KAPI_IMPORT = "struct NvKmsKapiPrivImportMemoryParams"
KAPI_EXPORT = "struct NvKmsKapiPrivExportMemoryParams"
DRM_SPEC = {
    "GET_CRTC_CRC32": [VAL("crtc_id", "target", "KMS object id; lease-filtered")],
    "GEM_IMPORT_NVKMS_MEMORY": [
        VAL("mem_size", "info"),
        PTR("nvkms_params_ptr", "in", L_SIZE_EQ("nvkms_params_size", KAPI_IMPORT),
            "always", elem=KAPI_IMPORT,
            elem_fields=[FD("memFd", [NVIDIACTL_EXPORTED])]),
        VAL("nvkms_params_size", "count"),
        GEMOUT("handle"),
    ],
    "GEM_IMPORT_USERSPACE_MEMORY": [
        VAL("size", "info"),
        UVA("address", "pinned with pin_user_pages in the caller's mm"),
        GEMOUT("handle"),
    ],
    "GET_DEV_INFO": [],
    "FENCE_SUPPORTED": [],
    "PRIME_FENCE_CONTEXT_CREATE": [
        GEMOUT("handle"),
        VAL("index", "info"),
        VAL("size", "info"),
        PTR("import_mem_nvkms_params_ptr", "in",
            L_SIZE_EQ("import_mem_nvkms_params_size", KAPI_IMPORT), "always",
            elem=KAPI_IMPORT, elem_fields=[FD("memFd", [NVIDIACTL_EXPORTED])]),
        VAL("import_mem_nvkms_params_size", "count"),
        PTR("event_nvkms_params_ptr", "in",
            L_SIZE_EQ("event_nvkms_params_size",
                      "struct NvKmsKapiPrivAllocateChannelEventParams"), "always",
            elem="struct NvKmsKapiPrivAllocateChannelEventParams",
            note="RM client and channel handles (host-valid, no translation)"),
        VAL("event_nvkms_params_size", "count"),
    ],
    "GEM_PRIME_FENCE_ATTACH": [
        GEMIN("handle"),
        GEMIN("fence_context_handle", note="must live in the same drm_file as handle"),
    ],
    "GET_CLIENT_CAPABILITY": [],
    "GEM_EXPORT_NVKMS_MEMORY": [
        GEMIN("handle"),
        PTR("nvkms_params_ptr", "in", L_SIZE_EQ("nvkms_params_size", KAPI_EXPORT),
            "always", elem=KAPI_EXPORT, elem_fields=[FD("memFd", [NVIDIACTL_EXPORT_TARGET])]),
        VAL("nvkms_params_size", "count"),
    ],
    "GEM_MAP_OFFSET": [GEMIN("handle"), VAL("offset", "info", "host fake mmap offset")],
    "GEM_ALLOC_NVKMS_MEMORY": [GEMOUT("handle"), VAL("memory_size", "info"),
                               VAL("flags", "info")],
    "GET_CRTC_CRC32_V2": [VAL("crtc_id", "target")],
    "GEM_EXPORT_DMABUF_MEMORY": [
        GEMIN("handle"),
        PTR("nvkms_params_ptr", "in", L_SIZE_EQ("nvkms_params_size", KAPI_EXPORT),
            "always", elem=KAPI_EXPORT, elem_fields=[FD("memFd", [NVIDIACTL_EXPORT_TARGET])]),
        VAL("nvkms_params_size", "count"),
    ],
    "GEM_IDENTIFY_OBJECT": [GEMIN("handle"), VAL("object_type", "info")],
    "DMABUF_SUPPORTED": [],
    "GET_DPY_ID_FOR_CONNECTOR_ID": [VAL("connectorId", "target"), VAL("dpyId", "info")],
    "GET_CONNECTOR_ID_FOR_DPY_ID": [VAL("dpyId", "target"), VAL("connectorId", "info")],
    "GRANT_PERMISSIONS": [
        FD("fd", [MODESET_FRESH]),
        VAL("dpyId", "target"),
        ver(VAL("type", "policy", "NV_DRM_PERMISSIONS_TYPE_*: 2 MODESET, 3 SUB_OWNER"),
            since="580.178.04"),
    ],
    "REVOKE_PERMISSIONS": [VAL("dpyId", "target"),
                           ver(VAL("type", "policy"), since="580.178.04")],
    "SEMSURF_FENCE_CTX_CREATE": [
        VAL("index", "info"),
        PTR("nvkms_params_ptr", "in",
            L_SIZE_EQ("nvkms_params_size", "struct NvKmsKapiPrivImportSemaphoreSurfaceParams"),
            "always", elem="struct NvKmsKapiPrivImportSemaphoreSurfaceParams",
            note="RM handles only"),
        VAL("nvkms_params_size", "count"),
        GEMOUT("handle"),
    ],
    "SEMSURF_FENCE_CREATE": [GEMIN("fence_context_handle"),
                             VAL("wait_value", "info"), FDOUT("fd")],
    "SEMSURF_FENCE_WAIT": [GEMIN("fence_context_handle"), FD("fd", [SYNC_FILE])],
    "SEMSURF_FENCE_ATTACH": [GEMIN("handle"), GEMIN("fence_context_handle"),
                             VAL("shared", "policy")],
    "GET_DRM_FILE_UNIQUE_ID": [VAL("id", "info")],
    "SEMSURF_EXPORT_TO_SYNCOBJ_POINT": [
        GEMIN("fence_context_handle"),
        SYNCOBJIN("syncobj_handle"),
        VAL("wait_value", "info"),
        VAL("syncobj_point", "info"),
    ],
    "SYNCOBJ_GET_SYNCFD": [
        SYNCOBJIN("syncobj_handle"),
        FDOUT("fd"),
        VAL("syncobj_point", "info"),
    ],
    "REGISTER_ROI": [VAL("region_handle", "info", "ROI id, not a GEM handle")],
    "UNREGISTER_ROI": [VAL("region_handle", "info", "ROI id, not a GEM handle")],
    "GET_CRTC_ROI_CRCS": [
        VAL("crtc_id", "target"),
        ARR("roi_crcs", [VAL("region_handle", "info", "ROI id, not a GEM handle")]),
    ],
    "GET_ROI_CAPABILITIES": [],
}

# Before the type field existed (535), a grant was always MODESET.
DRM_CONSTANTS = ["DRM_COMMAND_BASE", ("NV_DRM_PERMISSIONS_TYPE_MODESET", "580.178.04"),
                 ("NV_DRM_PERMISSIONS_TYPE_SUB_OWNER", "580.178.04")]

# Leaf-name patterns the header walk treats as needing a spec entry.
NVKMS_PTRISH = re.compile(r"^p[A-Z]")
NVKMS_FDISH = re.compile(r"(^fd$|Fd$)")
DRM_PTRISH = re.compile(r"(_ptr$|^address$)")
DRM_FDISH = re.compile(r"(^fd$|Fd$)")
DRM_HANDLEISH = re.compile(r"(^|_)handle$")


# --------------------------------------------------------------------------
# Fetching
# --------------------------------------------------------------------------

def _wanted(rel):
    d, b = os.path.split(rel)
    for pat in FETCH:
        pd, pb = os.path.split(pat)
        if d == pd and fnmatch.fnmatchcase(b, pb):
            return True
    return False


def _vkey(v):
    return tuple(int(x) for x in re.findall(r"\d+", v))


def _list_branch_tags(major):
    url = f"https://api.github.com/repos/{REPO}/git/matching-refs/tags/{major}."
    with urllib.request.urlopen(url, timeout=60) as r:
        refs = json.load(r)
    return [x["ref"].rsplit("/", 1)[1] for x in refs]


def _nearest_tag(version):
    """The existing tag of the same branch nearest to `version`, preferring older."""
    tags = _list_branch_tags(version.split(".")[0])
    if not tags:
        raise ExtractError(f"no tag of branch {version.split('.')[0]} exists in {REPO}")
    want = _vkey(version)
    older = [t for t in tags if _vkey(t) <= want]
    return max(older, key=_vkey) if older else min(tags, key=_vkey)


class _HashingReader:
    def __init__(self, f, h):
        self.f, self.h = f, h

    def read(self, n=-1):
        b = self.f.read(n)
        self.h.update(b)
        return b


def fetch(version, cache):
    """Unpack the headers of `version` into cache/<version>; returns that path."""
    dest = cache / version
    if (dest / "SOURCE.json").exists():
        return dest
    tag, note = version, None
    url = f"https://codeload.github.com/{REPO}/tar.gz/refs/tags/{tag}"
    try:
        resp = urllib.request.urlopen(url, timeout=120)
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        tag = _nearest_tag(version)
        note = f"tag {version} does not exist; nearest tag of the branch is {tag}"
        print(f"{version}: {note}", file=sys.stderr)
        url = f"https://codeload.github.com/{REPO}/tar.gz/refs/tags/{tag}"
        resp = urllib.request.urlopen(url, timeout=120)
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
                if len(parts) < 2 or not m.isfile() or not _wanted(parts[1]):
                    continue
                target = tmp / parts[1]
                target.parent.mkdir(parents=True, exist_ok=True)
                with tf.extractfile(m) as src, open(target, "wb") as out:
                    shutil.copyfileobj(src, out)
                nfiles += 1
        while reader.read(1 << 16):
            pass
    if not nfiles:
        raise ExtractError(f"{url}: no wanted files in the tarball")
    (tmp / "SOURCE.json").write_text(json.dumps({
        "requested": version, "tag": tag, "note": note, "commit": commit,
        "url": url, "tarball_sha256": h.hexdigest(), "files": nfiles,
    }, indent=2) + "\n")
    shutil.rmtree(dest, ignore_errors=True)
    tmp.rename(dest)
    return dest


# --------------------------------------------------------------------------
# A small C declaration reader, for preprocessed headers.
#
# It understands exactly what NVIDIA's interface headers use: struct/union
# definitions (named, typedef'd, inline and anonymous members), arrays,
# pointers, function-pointer typedefs and GCC attributes. It is not a C parser
# and does not need to be: its only jobs are to list member names, and to find
# every leaf a params struct reaches. Offsets never come from here.
# --------------------------------------------------------------------------

def _strip_attributes(s):
    out, i = [], 0
    while True:
        j = s.find("__attribute__", i)
        if j < 0:
            out.append(s[i:])
            return "".join(out)
        out.append(s[i:j])
        k = s.index("(", j)
        depth = 0
        while True:
            if s[k] == "(":
                depth += 1
            elif s[k] == ")":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        i = k + 1


def _match_brace(s, i):
    """Index of the '}' matching the '{' at s[i]."""
    depth = 0
    for k in range(i, len(s)):
        if s[k] == "{":
            depth += 1
        elif s[k] == "}":
            depth -= 1
            if depth == 0:
                return k
    raise ExtractError("unbalanced braces in preprocessed header")


def _split_top(s, sep):
    parts, depth, cur = [], 0, []
    for ch in s:
        if ch in "{([":
            depth += 1
        elif ch in "})]":
            depth -= 1
        if ch == sep and depth == 0:
            parts.append("".join(cur))
            cur = []
        else:
            cur.append(ch)
    parts.append("".join(cur))
    return parts


_DECL = re.compile(r"(\**)\s*([A-Za-z_]\w*)\s*((?:\[[^\]]*\]\s*)*)(?::\s*\w+)?\s*$")
_FNPTR = re.compile(r"\(\s*\*\s*([A-Za-z_]\w*)\s*\)\s*\(")


class Headers:
    def __init__(self, text):
        text = _strip_attributes(text)
        text = re.sub(r"\b(__extension__|__restrict|volatile)\b", " ", text)
        self.structs = {}    # "struct X" / "union X" -> members
        self.typedefs = {}   # name -> "struct X" or members (list) or "fnptr"
        self._scan(text)

    def _scan(self, text):
        pos = 0
        rx = re.compile(r"\b(typedef\s+)?(struct|union|enum)\s*(\w+)?\s*\{")
        while True:
            m = rx.search(text, pos)
            if not m:
                break
            open_i = m.end() - 1
            close_i = _match_brace(text, open_i)
            body = text[open_i + 1:close_i]
            semi = text.index(";", close_i)
            tail = text[close_i + 1:semi].strip()
            kind, tag = m.group(2), m.group(3)
            if kind != "enum":
                members = self._members(body)
                if tag:
                    self.structs[f"{kind} {tag}"] = members
                if m.group(1):
                    for name in (t.strip() for t in tail.split(",")):
                        if name:
                            self.typedefs[name] = f"{kind} {tag}" if tag else members
            pos = semi + 1
        for m in re.finditer(r"\btypedef\s+(struct|union)\s+(\w+)\s+(\w+)\s*;", text):
            self.typedefs.setdefault(m.group(3), f"{m.group(1)} {m.group(2)}")
        for m in re.finditer(r"\btypedef[^;{}]*?\(\s*\*\s*(\w+)\s*\)\s*\(", text):
            self.typedefs.setdefault(m.group(1), "fnptr")

    def _members(self, body):
        out = []
        for stmt in _split_top(body, ";"):
            stmt = " ".join(stmt.split())
            if not stmt:
                continue
            m = re.match(r"(struct|union|enum)\s*(\w+)?\s*\{", stmt)
            if m:
                close_i = _match_brace(stmt, m.end() - 1)
                inner = stmt[m.end():close_i]
                decls = [d.strip() for d in stmt[close_i + 1:].split(",") if d.strip()]
                if m.group(1) == "enum":
                    for d in decls:
                        dm = _DECL.search(d)
                        out.append({"name": dm.group(2), "type": "enum",
                                    "array": bool(dm.group(3)), "ptr": False})
                    continue
                sub = self._members(inner)
                if m.group(2):
                    self.structs[f"{m.group(1)} {m.group(2)}"] = sub
                if not decls:
                    out.extend(sub)          # anonymous struct/union: hoisted
                for d in decls:
                    dm = _DECL.search(d)
                    out.append({"name": dm.group(2), "type": sub,
                                "array": bool(dm.group(3)), "ptr": bool(dm.group(1))})
                continue
            fm = _FNPTR.search(stmt)
            if fm:
                out.append({"name": fm.group(1), "type": "fnptr", "array": False,
                            "ptr": True})
                continue
            parts = _split_top(stmt, ",")
            first = _DECL.search(parts[0])
            if not first:
                raise ExtractError(f"cannot read member declaration {stmt!r}")
            base = parts[0][:first.start()].strip()
            base = re.sub(r"\bconst\b", "", base).strip()
            for i, p in enumerate(parts):
                dm = first if i == 0 else _DECL.search(p)
                out.append({"name": dm.group(2), "type": " ".join(base.split()),
                            "array": bool(dm.group(3)), "ptr": bool(dm.group(1))})
        return out

    def resolve(self, t):
        """Members of type text `t`, or None for a scalar/opaque type."""
        seen = set()
        while True:
            if isinstance(t, list):
                return t
            if t in self.structs:
                return self.structs[t]
            if t in self.typedefs and t not in seen:
                seen.add(t)
                t = self.typedefs[t]
                continue
            return None

    def members(self, ctype):
        ms = self.resolve(ctype)
        if ms is None:
            raise ExtractError(f"{ctype} is not defined in the headers")
        return ms

    def leaves(self, ctype, prefix=""):
        """(path, member) for every scalar leaf reachable from ctype."""
        for mbr in self.members(ctype):
            path = f"{prefix}{mbr['name']}" + ("[]" if mbr["array"] else "")
            sub = None if mbr["ptr"] else self.resolve(mbr["type"])
            if sub is not None:
                yield from self.leaves(sub, path + ".")
            else:
                yield path, mbr

    def is_fnptr(self, t):
        return t == "fnptr" or (isinstance(t, str) and self.typedefs.get(t) == "fnptr")


# --------------------------------------------------------------------------
# The compiler side.
# --------------------------------------------------------------------------

class Probe:
    """Collects C expressions, then compiles and runs one program printing them all."""

    def __init__(self):
        self.items = {}           # key -> C expression
        self.values = None

    def need(self, key, expr):
        if self.values is not None:
            if key not in self.values:
                raise ExtractError(f"probe value {key} was not collected")
            return self.values[key]
        self.items.setdefault(key, expr)
        return 0

    def off(self, root, expr):
        return self.need(f"off {root} {expr}", f"offsetof({root}, {expr})")

    def size(self, root, expr):
        return self.need(f"size {root} {expr}", f"sizeof(((({root} *)0)->{expr}))")

    def count(self, root, expr):
        return self.need(f"count {root} {expr}",
                         f"(sizeof(((({root} *)0)->{expr})) / "
                         f"sizeof(((({root} *)0)->{expr})[0]))")

    def sizeof(self, ctype):
        return self.need(f"sizeof {ctype}", f"sizeof({ctype})")

    def const(self, name):
        return self.need(f"const {name}", f"({name})")

    def run(self, cc, prologue, epilogue, workdir, extra_sources=(), cflags=()):
        keys = sorted(self.items)
        lines = [prologue,
                 "#define P(k, e) printf(\"%s\\t%lld\\n\", k, (long long)(e))",
                 "int main(void) {"]
        first = len("\n".join(lines).splitlines()) + 1
        for k in keys:
            lines.append(f"    P({json.dumps(k)}, {self.items[k]});")
        lines += [epilogue, "    return 0;", "}"]
        src = workdir / "probe.c"
        src.write_text("\n".join(lines) + "\n")
        exe = workdir / "probe"
        cmd = [cc, "-std=gnu11", "-O0", "-w", *cflags, "-o", str(exe), str(src),
               *map(str, extra_sources)]
        r = subprocess.run(cmd, capture_output=True, text=True)
        if r.returncode != 0:
            bad = set()
            for m in re.finditer(r"probe\.c:(\d+):\d+: error", r.stderr):
                i = int(m.group(1)) - first
                if 0 <= i < len(keys):
                    bad.add(keys[i])
            if bad:
                raise ExtractError("the headers do not have these (they were expected):\n  "
                                   + "\n  ".join(sorted(bad)))
            raise ExtractError(f"probe failed to compile:\n{r.stderr[-4000:]}")
        out = subprocess.run([str(exe)], capture_output=True, text=True, check=True).stdout
        self.values = {}
        self.extra = []
        for line in out.splitlines():
            parts = line.split("\t")
            if parts[0] == "X":
                self.extra.append(parts[1:])
            else:
                self.values[parts[0]] = int(parts[1])


def _cc():
    return os.environ.get("CC", "gcc")


def _cc_version(cc):
    r = subprocess.run([cc, "--version"], capture_output=True, text=True)
    return r.stdout.splitlines()[0] if r.returncode == 0 else cc


def _preprocess(cc, src, include_flags, workdir, name):
    f = workdir / f"{name}.c"
    f.write_text(src)
    r = subprocess.run([cc, "-E", "-P", "-std=gnu11", *include_flags, str(f)],
                       capture_output=True, text=True)
    if r.returncode != 0:
        raise ExtractError(f"preprocessing {name} failed:\n{r.stderr[-4000:]}")
    return r.stdout


# --------------------------------------------------------------------------
# Resolving the spec against a release.
# --------------------------------------------------------------------------

class Resolver:
    """Turns spec nodes into JSON with offsets, and lists the leaves they claim."""

    def __init__(self, probe, headers, version):
        self.p = probe
        self.h = headers
        self.version = _vkey(version)
        self.claimed = set()     # normalized paths of pointer/fd/gem/kptr/uva nodes
        self.reviewed = set()    # plain fields: a name that only looks like one
        self.pointees = []

    def ref(self, root, base, path):
        expr = f"{base}{path}"
        return {"path": path, "off": self.p.off(root, expr) - self._base_off(root, base),
                "size": self.p.size(root, expr)}

    def _base_off(self, root, base):
        return self.p.off(root, base[:-1]) if base else 0

    def value(self, v):
        return self.p.const(v) if isinstance(v, str) else v

    def cond(self, root, base, conds):
        out = []
        for c in conds:
            r = self.ref(root, base, c[0])
            r["op"] = c[1]
            if c[1] == "eq":
                r["value"] = self.value(c[2])
                if isinstance(c[2], str):
                    r["value_name"] = c[2]
            out.append(r)
        return out

    def active(self, nodes):
        out = []
        for n in nodes:
            lo, hi = n.get("since"), n.get("before")
            if lo and self.version < _vkey(lo):
                continue
            if hi and self.version >= _vkey(hi):
                continue
            out.append(n)
        return out

    def nodes(self, root, base, nodes, norm):
        return [self.node(root, base, n, norm) for n in self.active(nodes)]

    def node(self, root, base, n, norm):
        """root: C type the offsets are measured in; base: C designator prefix of
        the current scope ("" or "a[0].b[0]."); norm: normalized path prefix."""
        kind, path = n["kind"], n["path"]
        cpath = re.sub(r"\[\]", "[0]", path)
        full = f"{norm}{path}"
        j = {"kind": kind, "path": path}
        if kind == "array":
            e0 = f"{base}{cpath}[0]"
            j["off"] = self.p.off(root, e0) - self._base_off(root, base)
            j["count"] = self.p.count(root, f"{base}{cpath}")
            j["stride"] = self.p.size(root, e0)
            v = n["valid"]
            if v[0] == "all":
                j["valid"] = {"rule": "all"}
            elif v[0] == "count":
                j["valid"] = {"rule": "count", "count": self.ref(root, base, v[1]),
                              "max": self.value(v[2]),
                              "max_name": v[2] if isinstance(v[2], str) else None}
            elif v[0] == "format_num_planes":
                j["valid"] = {"rule": "format_num_planes",
                              "format": self.ref(root, base, v[1]),
                              "when": self.cond(root, base, v[2])}
            j["fields"] = self.nodes(root, e0 + ".", n["fields"], full + "[].")
        else:
            r = self.ref(root, base, cpath)
            j["off"], j["size"] = r["off"], r["size"]
            if kind in ("fd_in", "fd_out", "gem_in", "gem_out", "syncobj_in"):
                j["cond"] = self.cond(root, base, n["cond"])
                if kind == "fd_in":
                    j["fd_kinds"] = n["fd_kinds"]
                self.claimed.add(full)
            elif kind in ("kernel_ptr", "user_va"):
                self.claimed.add(full)
            elif kind == "field":
                j["role"] = n["role"]
                self.reviewed.add(full)
            elif kind == "ptr":
                self.claimed.add(full)
                j["dir"] = n["dir"]
                j["len"] = self.length(root, base, n["len"])
                pr = n["present"]
                j["present"] = ({"rule": pr} if isinstance(pr, str) else
                                {"rule": pr[0], "field": self.ref(root, base, pr[1])})
                if n["elem"]:
                    et = n["elem"]
                    j["elem"] = {"type": et, "size": self.p.sizeof(et),
                                 "fields": self.nodes(et, "", n["elem_fields"], full + "->")}
                    self.pointee(et, full + "->")
        if n.get("note"):
            j["note"] = n["note"]
        return j

    def pointee(self, ctype, norm):
        # A pointee's own pointer/fd leaves must be claimed like the params' are.
        self.pointees.append((ctype, norm))

    def length(self, root, base, L):
        if L[0] == "const":
            return {"rule": "const", "type": L[1], "bytes": self.p.sizeof(L[1])}
        if L[0] == "count":
            return {"rule": "count", "count": self.ref(root, base, L[1]), "type": L[2],
                    "elem_size": self.p.sizeof(L[2]), "min": L[3],
                    "max": self.value(L[4]) if L[4] is not None else None,
                    "max_name": L[4]}
        if L[0] == "bytes":
            return {"rule": "bytes", "size": self.ref(root, base, L[1]),
                    "max": self.value(L[2]), "max_name": L[2],
                    "written": self.ref(root, "", L[3]) if L[3] else None}
        if L[0] == "size_eq":
            return {"rule": "size_eq", "size": self.ref(root, base, L[1]),
                    "type": L[2], "bytes": self.p.sizeof(L[2])}
        raise ExtractError(f"unknown length rule {L!r}")


def _check_claims(headers, what, roots, res, ptr_rx, fd_rx, extra_rx=None):
    """Every pointer/fd-looking leaf reachable from `roots` must be in the spec,
    and every pointer/fd the spec names must be reachable."""
    claimed, pointees = res.claimed, res.pointees
    found = set()
    for ctype, norm in list(roots) + list(pointees):
        for path, m in headers.leaves(ctype):
            name = m["name"]
            t = m["type"]
            if (m["ptr"] or ptr_rx.search(name) or fd_rx.search(name)
                    or t == "NvP64" or headers.is_fnptr(t)
                    or (extra_rx and extra_rx.search(name))):
                found.add(norm + path)
    missing = sorted(found - claimed - res.reviewed)
    if missing:
        raise ExtractError(f"{what}: fields that look like pointers, fds or handles "
                           f"are not classified in the spec:\n  " + "\n  ".join(missing))
    stale = sorted(c for c in claimed if c not in found)
    if stale:
        raise ExtractError(f"{what}: the spec classifies fields the headers do not "
                           f"reach:\n  " + "\n  ".join(stale))


# --------------------------------------------------------------------------
# Table readers.
# --------------------------------------------------------------------------

def _nvkms_dispatch(nvkms_c):
    """[(cmd enum name, func, custom_user)] from nvkms.c's dispatch[] initializer."""
    text = nvkms_c.read_text(errors="replace")
    m = re.search(r"\}\s*dispatch\[\]\s*=\s*\{(.*?)\n\s*\};", text, re.S)
    if not m:
        raise ExtractError(f"{nvkms_c}: dispatch[] table not found")
    body = re.sub(r"#define.*?(?<!\\)\n", "\n", m.group(1).replace("\\\n", " "))
    entries = re.findall(r"\b(ENTRY|ENTRY_CUSTOM_USER)\s*\(\s*(NVKMS_IOCTL_\w+)\s*,\s*(\w+)\s*\)",
                         body)
    if not entries:
        raise ExtractError(f"{nvkms_c}: no ENTRY() in dispatch[]")
    return [(cmd, func, macro == "ENTRY_CUSTOM_USER") for macro, cmd, func in entries]


def _drm_ioctls(header, drv_c):
    """nvidia-drm ioctls: name -> {nr, struct, macro}, and the registered ones."""
    text = header.read_text(errors="replace")
    nrs = {m.group(1): int(m.group(2), 0) for m in
           re.finditer(r"#define\s+DRM_NVIDIA_(\w+)\s+(0x[0-9a-fA-F]+|\d+)", text)}
    defs = {}
    for m in re.finditer(r"#define\s+DRM_IOCTL_NVIDIA_(\w+)\s*\\?\s*DRM_(IOWR|IOW|IOR|IO)\s*\((.*?)\)\s*\n",
                         text.replace("\\\n", " "), re.S):
        args = m.group(3)
        sm = re.search(r"struct\s+(\w+)", args)
        defs[m.group(1)] = {"macro": f"DRM_IOCTL_NVIDIA_{m.group(1)}",
                            "dir": m.group(2), "struct": f"struct {sm.group(1)}" if sm else None}
    drv = drv_c.read_text(errors="replace")
    t = re.search(r"nv_drm_ioctls\[\]\s*=\s*\{(.*?)\n\};", drv, re.S)
    if not t:
        raise ExtractError(f"{drv_c}: nv_drm_ioctls[] not found")
    registered = {}
    body = t.group(1)
    for m in re.finditer(r"DRM_IOCTL_DEF_DRV\s*\(\s*NVIDIA_(\w+)\s*,\s*(\w+)\s*,\s*([^)]*)\)", body):
        # The preprocessor conditionals enclosing the entry: it is registered
        # only in builds where they hold (535 gated most of them on conftest
        # results).
        pre = body[:m.start()]
        conds = []
        for line in pre.splitlines():
            s = line.strip()
            if s.startswith("#if"):
                conds.append(s)
            elif s.startswith("#else") and conds:
                conds[-1] = f"#else of {conds[-1]}"
            elif s.startswith("#endif") and conds:
                conds.pop()
        flags = sorted(f.strip() for f in m.group(3).split("|") if f.strip() and f.strip() != "0")
        registered[m.group(1)] = {"handler": m.group(2), "flags": flags,
                                  "conditional": conds or None}
    return nrs, defs, registered


def _enum_names(api_h, enum):
    text = re.sub(r"/\*.*?\*/|//[^\n]*", "", api_h.read_text(errors="replace"), flags=re.S)
    m = re.search(r"enum\s+" + enum + r"\s*\{(.*?)\}", text, re.S)
    if not m:
        raise ExtractError(f"{api_h}: enum {enum} not found")
    return [e.split("=")[0].strip() for e in m.group(1).split(",") if e.strip()]


# --------------------------------------------------------------------------
# One release.
# --------------------------------------------------------------------------

def extract(version, src, source_info):
    src = Path(src)
    cc = _cc()
    # NV_LINUX as the Linux kernel modules are built: nvidia-drm's header
    # defines DRM_IOCTL_NVIDIA_{FENCE,DMABUF}_SUPPORTED as 0 without it. The
    # NVKMS interface headers have no platform conditionals.
    inc = ["-DNV_LINUX"] + [f"-I{src / d}" for d in INCLUDE_DIRS]
    for d in INCLUDE_DIRS:
        if not (src / d).is_dir():
            raise ExtractError(f"{src / d}: missing (incomplete source tree?)")
    api_h = src / "src/nvidia-modeset/interface/nvkms-api.h"
    nvkms_c = src / "src/nvidia-modeset/src/nvkms.c"
    fmt_c = src / "src/nvidia-modeset/lib/nvkms-format.c"
    # nvidia-drm-ioctl.h up to 580, nv_drm_common_ioctl.h (shared with other
    # OSes) from 595.
    drm_h = next((src / "kernel-open/nvidia-drm" / n for n in
                  ("nv_drm_common_ioctl.h", "nvidia-drm-ioctl.h")
                  if (src / "kernel-open/nvidia-drm" / n).exists()), None)
    if drm_h is None:
        raise ExtractError(f"{src}/kernel-open/nvidia-drm: no ioctl header")
    drv_c = src / "kernel-open/nvidia-drm/nvidia-drm-drv.c"

    enum_cmds = _enum_names(api_h, "NvKmsIoctlCommand")
    dispatch = _nvkms_dispatch(nvkms_c)
    event_types = _enum_names(src / "src/nvidia-modeset/interface/nvkms-api-types.h",
                              "NvKmsEventType")
    unknown = sorted({f for _, f, _ in dispatch} - SPEC.keys())
    if unknown:
        raise ExtractError(f"{version}: commands with no spec entry (review them "
                           f"and add one to SPEC): {', '.join(unknown)}")
    for cmd, _, _ in dispatch:
        if cmd not in enum_cmds:
            raise ExtractError(f"{version}: dispatch names {cmd}, not in NvKmsIoctlCommand")

    with tempfile.TemporaryDirectory(prefix="nvkms-extract-") as wd:
        wd = Path(wd)
        nvkms_includes = ('#include <stdio.h>\n#include <stddef.h>\n'
                          '#include "nvkms-api.h"\n#include "nvkms-ioctl.h"\n'
                          '#include "nvkms-kapi-private.h"\n#include "nvkms-format.h"\n'
                          '#include "nvUnixVersion.h"\n')
        hdr = Headers(_preprocess(cc, nvkms_includes, inc, wd, "nvkms_pp"))

        # ---- NVKMS probe
        probe = Probe()
        res = Resolver(probe, hdr, version)

        def build():
            res.claimed, res.reviewed, res.pointees = set(), set(), []
            cmds = []
            by_func = {f: (c, cu) for c, f, cu in dispatch}
            for name in enum_cmds:
                j = {"nr": probe.const(name), "name": name[len("NVKMS_IOCTL_"):]}
                if name not in {c for c, _, _ in dispatch}:
                    j["dispatch"] = False
                    j["note"] = "no dispatch entry: the kernel refuses it"
                    cmds.append(j)
                    continue
                func = next(f for c, f, _ in dispatch if c == name)
                P = f"struct NvKms{func}Params"
                j.update({
                    "dispatch": True, "func": func, "params": P,
                    "custom_user": by_func[func][1],
                    "size": probe.sizeof(P),
                    "request": {"offset": probe.off(P, "request"),
                                "size": probe.sizeof(f"struct NvKms{func}Request")},
                    "reply": {"offset": probe.off(P, "reply"),
                              "size": probe.sizeof(f"struct NvKms{func}Reply")},
                })
                j["fields"] = res.nodes(P, "", SPEC[func], j["name"] + ":")
                cmds.append(j)
            return cmds

        build()
        consts = {c: probe.const(c) for c in _consts(NVKMS_CONSTANTS, version)}
        types = {t: probe.sizeof(t) for t in NVKMS_TYPES}
        for e in event_types:
            probe.const(e)
        probe.need("ioctl", "NVKMS_IOCTL_IOWR")
        for part in ("cmd", "size", "address"):
            probe.off("struct NvKmsIoctlParams", part)
            probe.size("struct NvKmsIoctlParams", part)
        fmt_loop = (
            '    for (int f = NvKmsSurfaceMemoryFormatMin; f <= NvKmsSurfaceMemoryFormatMax; f++) {\n'
            '        const NvKmsSurfaceMemoryFormatInfo *i = nvKmsGetSurfaceMemoryFormatInfo(f);\n'
            '        printf("X\\tfmt\\t%d\\t%s\\t%d\\n", f, i->name ? i->name : "", (int)i->numPlanes);\n'
            '    }\n'
            '    printf("X\\tversion\\t%s\\n", NV_VERSION_STRING);\n')
        probe.run(cc, nvkms_includes + '#include <sys/ioctl.h>\n', fmt_loop, wd,
                  extra_sources=[fmt_c], cflags=inc)
        commands = build()
        consts = {c: probe.const(c) for c in _consts(NVKMS_CONSTANTS, version)}
        types = {t: probe.sizeof(t) for t in NVKMS_TYPES}
        roots = [(c["params"], c["name"] + ":") for c in commands if c.get("dispatch")]
        _check_claims(hdr, f"{version} NVKMS", roots, res, NVKMS_PTRISH, NVKMS_FDISH)
        for st, known in EXHAUSTIVE.items():
            have = {m["name"] for m in hdr.members(st)}
            new = sorted(have - known)
            if new:
                raise ExtractError(f"{version}: {st} has members the policy has not "
                                   f"reviewed: {', '.join(new)} (add them to EXHAUSTIVE "
                                   f"and SPEC)")
        header_version = next(x[1] for x in probe.extra if x[0] == "version")
        formats = [{"value": int(x[1]), "name": x[2], "num_planes": int(x[3])}
                   for x in probe.extra if x[0] == "fmt"]
        nvkms = {
            "ioctl": {"request": probe.values["ioctl"],
                      "request_hex": f"0x{probe.values['ioctl']:08x}",
                      "outer_size": types["struct NvKmsIoctlParams"],
                      "outer": {p: {"off": probe.off("struct NvKmsIoctlParams", p),
                                    "size": probe.size("struct NvKmsIoctlParams", p)}
                                for p in ("cmd", "size", "address")}},
            "constants": consts,
            "types": types,
            "event_types": {e: probe.const(e) for e in event_types},
            "surface_formats": formats,
            "exhaustive_members": {st: sorted(m["name"] for m in hdr.members(st))
                                   for st in EXHAUSTIVE},
            "commands": sorted(commands, key=lambda c: c["nr"]),
        }
        if header_version != version and header_version != source_info.get("tag"):
            raise ExtractError(f"headers say NV_VERSION_STRING {header_version!r}, "
                               f"expected {version!r}")

        # ---- nvidia-drm probe
        nrs, defs, registered = _drm_ioctls(drm_h, drv_c)
        unknown = sorted(set(nrs) - DRM_SPEC.keys())
        if unknown:
            raise ExtractError(f"{version}: nvidia-drm ioctls with no spec entry: "
                               + ", ".join(unknown))
        drm_includes = ('#include <stdio.h>\n#include <stddef.h>\n#include <stdint.h>\n'
                        f'#include "{drm_h.name}"\n#include "nvkms-kapi-private.h"\n')
        drm_inc = [f"-I{drm_h.parent}"] + inc
        dhdr = Headers(_preprocess(cc, drm_includes, drm_inc, wd, "drm_pp"))
        dprobe = Probe()
        dres = Resolver(dprobe, dhdr, version)

        def dbuild():
            dres.claimed, dres.reviewed, dres.pointees = set(), set(), []
            out = []
            for name in sorted(nrs, key=lambda n: nrs[n]):
                d = defs.get(name)
                j = {"nr": dprobe.const(f"DRM_NVIDIA_{name}") + dprobe.const("DRM_COMMAND_BASE"),
                     "name": name}
                reg = registered.get(name)
                j["registered"] = reg is not None
                if reg:
                    j["flags"] = reg["flags"]
                    if reg["conditional"]:
                        j["conditional"] = reg["conditional"]
                if d is None:
                    j["note"] = "no DRM_IOCTL_NVIDIA_ macro"
                    out.append(j)
                    continue
                j["cmd"] = dprobe.need(f"cmd {name}", d["macro"])
                j["dir"] = d["dir"]
                j["struct"] = d["struct"]
                if d["struct"]:
                    j["size"] = dprobe.sizeof(d["struct"])
                    j["fields"] = dres.nodes(d["struct"], "", DRM_SPEC[name], name + ":")
                else:
                    j["size"] = 0
                    j["fields"] = []
                out.append(j)
            return out

        dbuild()
        for c in _consts(DRM_CONSTANTS, version):
            dprobe.const(c)
        dprobe.run(cc, drm_includes, "", wd, cflags=drm_inc)
        drm = dbuild()
        for j in drm:
            if "cmd" in j:
                j["cmd_hex"] = f"0x{j['cmd'] & 0xffffffff:08x}"
        droots = []
        for j in drm:
            if j.get("struct"):
                droots.append((j["struct"], j["name"] + ":"))
        _check_claims(dhdr, f"{version} nvidia-drm", droots, dres,
                      DRM_PTRISH, DRM_FDISH, DRM_HANDLEISH)

    return {
        "format": FORMAT,
        "driver_version": version,
        "why": VERSIONS.get(version),
        "source": source_info,
        "abi": {"arch": platform.machine(), "data_model": "LP64", "compiler": _cc_version(cc)},
        "nvkms": nvkms,
        "nvidia_drm": {
            "header": f"kernel-open/nvidia-drm/{drm_h.name}",
            "command_base": dprobe.const("DRM_COMMAND_BASE"),
            "constants": {c: dprobe.const(c) for c in _consts(DRM_CONSTANTS, version)},
            "ioctls": drm,
        },
    }


# --------------------------------------------------------------------------
# 610.57.04 against an independent hand measurement taken while the display work
# was designed (probes against the 610.57.04 headers; the notes are not shipped).
#
# These numbers were measured independently (a hand-written offsetof program
# against the same headers) before this extractor existed. They are checked so
# that a bug in the spec resolution cannot pass silently for the release the
# display work is written against.
# --------------------------------------------------------------------------

RESEARCH_610 = {
    # name: (nr, size, req size, rep size, rep offset)
    "ALLOC_DEVICE": (0, 1440, 620, 816, 624), "FREE_DEVICE": (1, 8, 4, 4, 4),
    "QUERY_DISP": (2, 172, 8, 164, 8), "QUERY_CONNECTOR_STATIC_DATA": (3, 44, 12, 32, None),
    "QUERY_CONNECTOR_DYNAMIC_DATA": (4, 20, 12, 8, None),
    "QUERY_DPY_STATIC_DATA": (5, 96, 12, 84, None),
    "QUERY_DPY_DYNAMIC_DATA": (6, 37168, 2072, 35096, None),
    "VALIDATE_MODE_INDEX": (7, 736, 224, 512, None), "VALIDATE_MODE": (8, 656, 304, 352, None),
    "SET_MODE": (9, 186784, 170840, 15944, 170840), "SET_CURSOR_IMAGE": (10, 52, 48, 4, None),
    "MOVE_CURSOR": (11, 20, 16, 4, None), "SET_LUT": (12, 72, 64, 4, None),
    "CHECK_LUT_NOTIFIER": (13, 20, 16, 1, 16), "IDLE_BASE_CHANNEL": (14, 36, 20, 16, None),
    "FLIP": (15, 3104, 24, 3080, 24), "DECLARE_DYNAMIC_DPY_INTEREST": (16, 20, 16, 4, None),
    "REGISTER_SURFACE": (17, 152, 144, 4, 144), "UNREGISTER_SURFACE": (18, 16, 12, 4, None),
    "GRANT_SURFACE": (19, 16, 12, 4, None), "ACQUIRE_SURFACE": (20, 12, 4, 8, None),
    "RELEASE_SURFACE": (21, 12, 8, 4, None), "SET_DPY_ATTRIBUTE": (22, 32, 24, 4, None),
    "GET_DPY_ATTRIBUTE": (23, 24, 16, 8, None),
    "GET_DPY_ATTRIBUTE_VALID_VALUES": (24, 40, 16, 24, None),
    "SET_DISP_ATTRIBUTE": (25, 32, 24, 4, None), "GET_DISP_ATTRIBUTE": (26, 24, 12, 8, 16),
    "GET_DISP_ATTRIBUTE_VALID_VALUES": (27, 40, 12, 24, 16),
    "QUERY_FRAMELOCK": (28, 20, 4, 16, None), "SET_FRAMELOCK_ATTRIBUTE": (29, 24, None, None, None),
    "GET_FRAMELOCK_ATTRIBUTE": (30, 16, None, None, None),
    "GET_FRAMELOCK_ATTRIBUTE_VALID_VALUES": (31, 32, None, None, None),
    "GET_NEXT_EVENT": (32, 48, 4, 40, 8), "DECLARE_EVENT_INTEREST": (33, 8, 4, 4, None),
    "CLEAR_UNICAST_EVENT": (34, 8, 4, 4, None), "SET_LAYER_POSITION": (37, 1196, 1192, 4, None),
    "GRAB_OWNERSHIP": (38, 8, None, None, None), "RELEASE_OWNERSHIP": (39, 8, None, None, None),
    "GRANT_PERMISSIONS": (40, 32, 28, 4, None), "ACQUIRE_PERMISSIONS": (41, 28, 4, 24, None),
    "REVOKE_PERMISSIONS": (42, 32, 28, 4, None), "QUERY_DPY_CRC32": (43, 36, 12, 24, None),
    "REGISTER_DEFERRED_REQUEST_FIFO": (44, 12, 8, 4, None),
    "UNREGISTER_DEFERRED_REQUEST_FIFO": (45, 12, None, None, None),
    "ALLOC_SWAP_GROUP": (46, 12, None, None, None), "FREE_SWAP_GROUP": (47, 12, None, None, None),
    "JOIN_SWAP_GROUP": (48, 2568, 2564, 4, None), "LEAVE_SWAP_GROUP": (49, 1032, None, None, None),
    "SET_SWAP_GROUP_CLIP_LIST": (50, 32, 24, 4, None), "GRANT_SWAP_GROUP": (51, 16, 12, 4, None),
    "ACQUIRE_SWAP_GROUP": (52, 12, 4, 8, None), "RELEASE_SWAP_GROUP": (53, 12, None, None, None),
    "SWITCH_MUX": (54, 24, None, None, None), "GET_MUX_STATE": (55, 16, None, None, None),
    "ENABLE_VBLANK_SYNC_OBJECT": (56, 20, None, None, None),
    "DISABLE_VBLANK_SYNC_OBJECT": (57, 20, None, None, None),
    "NOTIFY_VBLANK": (58, 20, 16, 4, None), "SET_FLIPLOCK_GROUP": (59, 72, None, None, None),
    "ENABLE_VBLANK_SEM_CONTROL": (60, 32, 24, 4, None),
    "DISABLE_VBLANK_SEM_CONTROL": (61, 16, None, None, None),
    "ACCEL_VBLANK_SEM_CONTROLS": (62, 16, None, None, None),
    "FRAMEBUFFER_CONSOLE_DISABLED": (63, 8, None, None, None),
    "REGISTER_VBLANK_INTR_CALLBACK": (64, 40, 32, 4, None),
    "UNREGISTER_VBLANK_INTR_CALLBACK": (65, 20, None, None, None),
}

# (command, flattened path, offset from the start of Params) from §4/§5.
RESEARCH_610_OFFSETS = [
    ("ALLOC_DEVICE", "reply.deviceHandle", 628), ("ALLOC_DEVICE", "reply.dispHandles", 644),
    ("ALLOC_DEVICE", "request.deviceId.rmDeviceId", 32),
    ("VALIDATE_MODE_INDEX", "request.pInfoString", 216),
    ("VALIDATE_MODE_INDEX", "request.infoStringSize", 208),
    ("VALIDATE_MODE_INDEX", "reply.infoStringLenWritten", 532),
    ("VALIDATE_MODE", "request.pInfoString", 296), ("VALIDATE_MODE", "request.infoStringSize", 288),
    ("VALIDATE_MODE", "reply.infoStringLenWritten", 456),
    ("SET_LUT", "request.common.input.pRamps", 32), ("SET_LUT", "request.common.output.pRamps", 48),
    ("FLIP", "request.pFlipHead", 8), ("FLIP", "request.numFlipHeads", 16),
    ("FLIP", "request.commit", 20),
    ("REGISTER_SURFACE", "request.useFd", 4), ("REGISTER_SURFACE", "request.rmClient", 8),
    ("GRANT_SURFACE", "request.fd", 8), ("ACQUIRE_SURFACE", "request.fd", 0),
    ("CLEAR_UNICAST_EVENT", "request.unicastEventFd", 0),
    ("GRANT_PERMISSIONS", "request.fd", 0), ("GRANT_PERMISSIONS", "request.deviceHandle", 4),
    ("GRANT_PERMISSIONS", "request.permissions.type", 8),
    ("ACQUIRE_PERMISSIONS", "request.fd", 0),
    ("GRANT_SWAP_GROUP", "request.fd", 8), ("ACQUIRE_SWAP_GROUP", "request.fd", 0),
    ("NOTIFY_VBLANK", "request.unicastEvent.fd", 12),
    ("SET_SWAP_GROUP_CLIP_LIST", "request.nClips", 8),
    ("SET_SWAP_GROUP_CLIP_LIST", "request.pClipList", 16),
    ("REGISTER_VBLANK_INTR_CALLBACK", "request.pCallback", 16),
    ("REGISTER_VBLANK_INTR_CALLBACK", "request.param", 24),
    ("ENABLE_VBLANK_SEM_CONTROL", "request.surfaceHandle", 8),
    ("ENABLE_VBLANK_SEM_CONTROL", "request.surfaceOffset", 16),
]


# nvidia-drm, from the same hand measurement (also measured with a hand-written
# probe): name -> (absolute nr, cmd, size, {field: offset}).
RESEARCH_610_DRM = {
    "GET_CRTC_CRC32": (0x40, 0xc0086440, 8, {}),
    "GEM_IMPORT_NVKMS_MEMORY": (0x41, 0xc0206441, 32, {
        "mem_size": 0, "nvkms_params_ptr": 8, "nvkms_params_size": 16, "handle": 24}),
    "GEM_IMPORT_USERSPACE_MEMORY": (0x42, 0xc0186442, 24, {"address": 8, "handle": 16}),
    "GET_DEV_INFO": (0x43, 0xc0246443, 36, {}),
    "FENCE_SUPPORTED": (0x44, 0x00006444, 0, {}),
    "PRIME_FENCE_CONTEXT_CREATE": (0x45, 0xc0306445, 48, {
        "handle": 0, "index": 4, "size": 8, "import_mem_nvkms_params_ptr": 16,
        "import_mem_nvkms_params_size": 24, "event_nvkms_params_ptr": 32,
        "event_nvkms_params_size": 40}),
    "GEM_PRIME_FENCE_ATTACH": (0x46, 0x40106446, 16, {"handle": 0, "fence_context_handle": 4}),
    "GET_CLIENT_CAPABILITY": (0x48, 0xc0106448, 16, {}),
    "GEM_EXPORT_NVKMS_MEMORY": (0x49, 0xc0186449, 24, {
        "handle": 0, "nvkms_params_ptr": 8, "nvkms_params_size": 16}),
    "GEM_MAP_OFFSET": (0x4a, 0xc010644a, 16, {"handle": 0}),
    "GEM_ALLOC_NVKMS_MEMORY": (0x4b, 0xc018644b, 24, {
        "handle": 0, "memory_size": 8, "flags": 16}),
    "GET_CRTC_CRC32_V2": (0x4c, 0xc01c644c, 28, {}),
    "GEM_EXPORT_DMABUF_MEMORY": (0x4d, 0xc018644d, 24, {"handle": 0, "nvkms_params_ptr": 8}),
    "GEM_IDENTIFY_OBJECT": (0x4e, 0xc008644e, 8, {"handle": 0}),
    "DMABUF_SUPPORTED": (0x4f, 0x0000644f, 0, {}),
    "GET_DPY_ID_FOR_CONNECTOR_ID": (0x50, 0xc0086450, 8, {}),
    "GET_CONNECTOR_ID_FOR_DPY_ID": (0x51, 0xc0086451, 8, {}),
    "GRANT_PERMISSIONS": (0x52, 0xc00c6452, 12, {"fd": 0, "dpyId": 4, "type": 8}),
    "REVOKE_PERMISSIONS": (0x53, 0xc0086453, 8, {"dpyId": 0, "type": 4}),
    "SEMSURF_FENCE_CTX_CREATE": (0x54, 0xc0206454, 32, {
        "index": 0, "nvkms_params_ptr": 8, "nvkms_params_size": 16, "handle": 24}),
    "SEMSURF_FENCE_CREATE": (0x55, 0xc0186455, 24, {
        "fence_context_handle": 0, "wait_value": 8, "fd": 16}),
    "SEMSURF_FENCE_WAIT": (0x56, 0x40186456, 24, {"fence_context_handle": 0, "fd": 4}),
    "SEMSURF_FENCE_ATTACH": (0x57, 0x40186457, 24, {
        "handle": 0, "fence_context_handle": 4, "shared": 12}),
    "GET_DRM_FILE_UNIQUE_ID": (0x58, 0xc0086458, 8, {}),
    "REGISTER_ROI": (0x59, 0xc0186459, 24, {}),
    "UNREGISTER_ROI": (0x5a, 0x4008645a, 8, {}),
    "GET_CRTC_ROI_CRCS": (0x5b, 0xcc28645b, 3112, {}),
    "GET_ROI_CAPABILITIES": (0x5c, 0xc020645c, 32, {}),
}
# §2: 0x47 is a hole and the ROI ioctls are defined but not registered.
RESEARCH_610_DRM_UNREGISTERED = {"REGISTER_ROI", "UNREGISTER_ROI", "GET_CRTC_ROI_CRCS",
                                 "GET_ROI_CAPABILITIES"}


def flatten(fields, base=0, prefix=""):
    """(normalized path, kind, absolute offset of element 0, node) for every node."""
    for f in fields:
        path = prefix + f["path"]
        yield path, f["kind"], base + f["off"], f
        if f["kind"] == "array":
            yield from flatten(f["fields"], base + f["off"], path + "[].")
        if f["kind"] == "ptr" and f.get("elem"):
            yield from flatten(f["elem"]["fields"], 0, path + "->")


def check_research_610(doc):
    problems = []
    cmds = {c["name"]: c for c in doc["nvkms"]["commands"]}
    for name, (nr, size, req, rep, rep_at) in RESEARCH_610.items():
        c = cmds.get(name)
        if c is None:
            problems.append(f"{name}: missing")
            continue
        got = (c["nr"], c.get("size"), c.get("request", {}).get("size"),
               c.get("reply", {}).get("size"),
               c.get("reply", {}).get("offset"))
        want = (nr, size, req, rep, rep_at)
        for label, g, w in zip(("nr", "size", "request", "reply", "reply@"), got, want):
            if w is not None and g != w:
                problems.append(f"{name}.{label}: extracted {g}, research {w}")
    for name, path, off in RESEARCH_610_OFFSETS:
        got = {p: o for p, _, o, _ in flatten(cmds[name]["fields"])}
        if got.get(path) != off:
            problems.append(f"{name} {path}: extracted {got.get(path)}, research {off}")
    # Structured facts from §4/§5.
    t = doc["nvkms"]["types"]
    c = doc["nvkms"]["constants"]
    facts = [
        ("sizeof NvKmsLutRamps", t["struct NvKmsLutRamps"], 6144),
        ("sizeof NvKmsFlipRequestOneHead", t["struct NvKmsFlipRequestOneHead"], 4952),
        ("sizeof NvKmsFlipCommonParams", t["struct NvKmsFlipCommonParams"], 4944),
        ("NV_MAX_SUBDEVICES", c["NV_MAX_SUBDEVICES"], 8), ("NV_MAX_HEADS", c["NV_MAX_HEADS"], 4),
        ("NVKMS_MAX_LAYERS_PER_HEAD", c["NVKMS_MAX_LAYERS_PER_HEAD"], 8),
        ("NV_MAX_FLIP_REQUEST_HEADS", c["NV_MAX_FLIP_REQUEST_HEADS"], 32),
        ("NVKMS_MAX_SWAPGROUPS", c["NVKMS_MAX_SWAPGROUPS"], 128),
        ("NVKMS_LUT_ARRAY_SIZE", c["NVKMS_LUT_ARRAY_SIZE"], 1024),
    ]
    flip = {p: (o, n) for p, _, o, n in flatten(cmds["FLIP"]["fields"])}
    facts += [
        ("FLIP elem lut.input.pRamps", flip["request.pFlipHead->flip.lut.input.pRamps"][0], 88),
        ("FLIP elem lut.output.pRamps", flip["request.pFlipHead->flip.lut.output.pRamps"][0], 104),
        ("FLIP elem layer[0] syncpt fd",
         flip["request.pFlipHead->flip.layer[].syncObjects.val.u.syncpts.pre.u.fd"][0], 8 + 276),
        ("FLIP elem layer stride", flip["request.pFlipHead->flip.layer"][1]["stride"], 592),
    ]
    sm = {p: (o, n) for p, _, o, n in flatten(cmds["SET_MODE"]["fields"])}
    disp, head = sm["request.disp"][1], sm["request.disp[].head"][1]
    base_in = sm["request.disp[].head[].flip.lut.input.pRamps"][0]
    base_out = sm["request.disp[].head[].flip.lut.output.pRamps"][0]
    facts += [("SET_MODE input pRamps base", base_in, 384),
              ("SET_MODE output pRamps base", base_out, 400),
              ("SET_MODE disp stride", disp["stride"], 21352),
              ("SET_MODE head stride", head["stride"], 5336),
              ("SET_MODE disps", disp["count"], 8), ("SET_MODE heads", head["count"], 4)]
    rs = {p: (o, n) for p, _, o, n in flatten(cmds["REGISTER_SURFACE"]["fields"])}
    planes = rs["request.planes"][1]
    facts += [("REGISTER_SURFACE plane fds",
               [rs["request.planes[].u.fd"][0] + i * planes["stride"] for i in range(planes["count"])],
               [16, 48, 80])]
    js = {p: (o, n) for p, _, o, n in flatten(cmds["JOIN_SWAP_GROUP"]["fields"])}
    facts += [("JOIN_SWAP_GROUP member fd", js["request.member[].unicastEvent.fd"][0], 16),
              ("JOIN_SWAP_GROUP member stride", js["request.member"][1]["stride"], 20),
              ("JOIN_SWAP_GROUP specified", js["request.member[].unicastEvent.specified"][0], 20)]
    for label, g, w in facts:
        if g != w:
            problems.append(f"{label}: extracted {g}, research {w}")
    drm = {i["name"]: i for i in doc["nvidia_drm"]["ioctls"]}
    if set(drm) != set(RESEARCH_610_DRM):
        problems.append(f"nvidia-drm ioctl set: extracted {sorted(drm)}, "
                        f"research {sorted(RESEARCH_610_DRM)}")
    for name, (nr, cmd, size, offs) in RESEARCH_610_DRM.items():
        i = drm.get(name)
        if i is None:
            continue
        for label, g, w in (("nr", i["nr"], nr), ("cmd", i.get("cmd"), cmd),
                            ("size", i.get("size"), size),
                            ("registered", i["registered"],
                             name not in RESEARCH_610_DRM_UNREGISTERED)):
            if g != w:
                problems.append(f"nvidia-drm {name}.{label}: extracted {g}, research {w}")
        got = {p: o for p, _, o, _ in flatten(i.get("fields", []))}
        for path, off in offs.items():
            if got.get(path) != off:
                problems.append(f"nvidia-drm {name} {path}: extracted {got.get(path)}, "
                                f"research {off}")
    if problems:
        raise ExtractError("610.57.04 disagrees with the independent hand "
                           "measurement of its NVKMS and nvidia-drm layouts:\n  " + "\n  ".join(problems))


# --------------------------------------------------------------------------
# Cross-version summary.
# --------------------------------------------------------------------------

TRANSLATED = ("ptr", "fd_in", "fd_out", "gem_in", "gem_out", "syncobj_in",
              "kernel_ptr", "user_va", "array")


def _nodes(fields, prefix=""):
    """path -> description of each node, offsets relative to the node's scope, so
    that a parent moving does not repeat itself in every descendant."""
    out = {}
    for f in fields:
        path = prefix + f["path"]
        if f["kind"] not in TRANSLATED:
            out[path] = "field"
            continue
        d = f"{f['kind']} @{f['off']}"
        if f["kind"] == "array":
            d += f" ×{f['count']} stride {f['stride']}"
            out.update(_nodes(f["fields"], path + "[]."))
        if f["kind"] == "ptr":
            L = f["len"]
            d += (f", {L['rule']} of {L['elem_size']} B" if L.get("elem_size")
                  else f", {L['rule']} {L['bytes']} B" if L.get("bytes") else f", {L['rule']}")
            if f.get("elem"):
                out.update(_nodes(f["elem"]["fields"], path + "->"))
        out[path] = d
    return out


def _node_changes(a, b):
    na, nb = _nodes(a), _nodes(b)
    ch = []
    for k in sorted(set(na) | set(nb)):
        x, y = na.get(k), nb.get(k)
        if x == y:
            continue
        if x is None or y is None:
            ch.append(f"`{k}` {'added' if x is None else 'removed'}")
        elif "field" not in (x, y):
            ch.append(f"`{k}` {x} → {y}")
    return ch


def _runs(renum):
    """Compress [(name, old, new)] into runs sharing one shift."""
    renum = sorted(renum, key=lambda r: r[1])
    runs = []
    for r in renum:
        if (runs and r[2] - r[1] == runs[-1][-1][2] - runs[-1][-1][1]
                and r[1] == runs[-1][-1][1] + 1):
            runs[-1].append(r)
        else:
            runs.append([r])
    out = []
    for run in runs:
        d = run[0][2] - run[0][1]
        if len(run) == 1:
            out.append(f"`{run[0][0]}` {run[0][1]}→{run[0][2]}")
        else:
            out.append(f"`{run[0][0]}`…`{run[-1][0]}` ({run[0][1]}–{run[-1][1]}) {d:+d}")
    return out


def diff_docs(a, b):
    lines = []
    ca = {c["name"]: c for c in a["nvkms"]["commands"]}
    cb = {c["name"]: c for c in b["nvkms"]["commands"]}
    for n in sorted(cb.keys() - ca.keys(), key=lambda n: cb[n]["nr"]):
        lines.append(f"- NVKMS `{n}` added as {cb[n]['nr']}")
    for n in sorted(ca.keys() - cb.keys(), key=lambda n: ca[n]["nr"]):
        lines.append(f"- NVKMS `{n}` removed (was {ca[n]['nr']})")
    renum = [(n, ca[n]["nr"], cb[n]["nr"]) for n in ca.keys() & cb.keys()
             if ca[n]["nr"] != cb[n]["nr"]]
    if renum:
        lines.append("- NVKMS renumbered: " + "; ".join(_runs(renum)))
    for n in sorted(ca.keys() & cb.keys(), key=lambda n: cb[n]["nr"]):
        x, y = ca[n], cb[n]
        ch = []
        if x.get("size") != y.get("size"):
            ch.append(f"size {x.get('size')} → {y.get('size')}")
        for half in ("request", "reply"):
            hx, hy = x.get(half), y.get(half)
            if hx and hy and hx != hy:
                ch.append(f"{half} {hx['size']}@{hx['offset']} → {hy['size']}@{hy['offset']}")
        ch += _node_changes(x.get("fields", []), y.get("fields", []))
        if ch:
            lines.append(f"- NVKMS `{n}`: " + "; ".join(ch))
    for sect, label in (("constants", ""), ("types", "sizeof "), ("event_types", "")):
        for k in sorted(set(a["nvkms"][sect]) | set(b["nvkms"][sect])):
            x, y = a["nvkms"][sect].get(k), b["nvkms"][sect].get(k)
            if x != y:
                lines.append(f"- {label}`{k}` {'absent' if x is None else x} → "
                             f"{'absent' if y is None else y}")
    fa = {f["value"]: f for f in a["nvkms"]["surface_formats"]}
    fb = {f["value"]: f for f in b["nvkms"]["surface_formats"]}
    for v in sorted(set(fa) | set(fb)):
        if fa.get(v) != fb.get(v):
            lines.append(f"- surface format {v}: {fa.get(v)} → {fb.get(v)}")
    for st in sorted(set(a["nvkms"]["exhaustive_members"]) | set(b["nvkms"]["exhaustive_members"])):
        x = set(a["nvkms"]["exhaustive_members"].get(st, []))
        y = set(b["nvkms"]["exhaustive_members"].get(st, []))
        if x != y:
            lines.append(f"- `{st}` members: added {sorted(y - x)}, removed {sorted(x - y)}")
    da = {i["name"]: i for i in a["nvidia_drm"]["ioctls"]}
    db = {i["name"]: i for i in b["nvidia_drm"]["ioctls"]}
    for n in sorted(set(da) | set(db), key=lambda n: (db.get(n) or da.get(n))["nr"]):
        x, y = da.get(n), db.get(n)
        if x is None or y is None:
            z = y or x
            lines.append(f"- nvidia-drm `{n}` {'added' if x is None else 'removed'}: "
                         f"nr 0x{z['nr']:02x}, {z.get('size')} B, "
                         f"{'registered' if z['registered'] else 'not registered'}")
            continue
        ch = []
        for k in ("nr", "cmd_hex", "size", "registered", "flags", "conditional"):
            if x.get(k) != y.get(k):
                ch.append(f"{k} {x.get(k)} → {y.get(k)}")
        ch += _node_changes(x.get("fields", []), y.get("fields", []))
        if ch:
            lines.append(f"- nvidia-drm `{n}`: " + "; ".join(ch))
    if a["nvidia_drm"].get("header") != b["nvidia_drm"].get("header"):
        lines.append(f"- nvidia-drm header {a['nvidia_drm'].get('header')} → "
                     f"{b['nvidia_drm'].get('header')}")
    return lines


def diff_summary(docs):
    out = []
    order = sorted(docs, key=_vkey)
    for a, b in zip(order, order[1:]):
        lines = diff_docs(docs[a], docs[b])
        out.append(f"#### {a} → {b}\n")
        out.extend(lines or ["- nothing a forwarder depends on changed"])
        out.append("")
    return "\n".join(out).rstrip() + "\n"


# --------------------------------------------------------------------------
# CLI
# --------------------------------------------------------------------------

def _source(version, src, cache, local):
    if local:
        info = {"requested": version, "tag": None, "note": None, "commit": None,
                "local_tree": str(Path(local).resolve())}
        mk = Path(local) / "version.mk"
        if mk.exists():
            m = re.search(r"NVIDIA_VERSION\s*=\s*(\S+)", mk.read_text())
            info["tag"] = m.group(1) if m else None
        return Path(local), info
    d = fetch(version, cache)
    info = json.loads((d / "SOURCE.json").read_text())
    # The tarball's bytes are GitHub's to change; the commit is the identity.
    info.pop("files", None)
    info.pop("tarball_sha256", None)
    return d, info


def _write(doc, out_dir):
    out_dir.mkdir(parents=True, exist_ok=True)
    path = out_dir / f"{doc['driver_version']}.json"
    path.write_text(json.dumps(doc, indent=1) + "\n")
    return path


def _one(version, cache, local):
    src, info = _source(version, None, cache, local)
    try:
        doc = extract(version, src, info)
    except ExtractError as e:
        msg = str(e)
        raise ExtractError(msg if msg.startswith(version) else f"{version}: {msg}") from None
    if version == "610.57.04":
        check_research_610(doc)
    return doc


def selftest(cache, local):
    """The refusals are the point of this tool, so test that they happen.

    Each case breaks the spec the way a new driver release would break it, and
    requires extraction of 610.57.04 to fail naming the problem.
    """
    import copy
    src, info = _source("610.57.04", None, cache, local)
    check_research_610(extract("610.57.04", src, info))
    saved = copy.deepcopy((SPEC, DRM_SPEC, EXHAUSTIVE))

    def expect(label, needle, mutate):
        mutate()
        try:
            extract("610.57.04", src, info)
        except ExtractError as e:
            if needle not in str(e):
                raise AssertionError(f"{label}: failed, but not naming {needle!r}:\n{e}")
        else:
            raise AssertionError(f"{label}: extraction succeeded")
        finally:
            SPEC.clear()
            SPEC.update(copy.deepcopy(saved[0]))
            DRM_SPEC.clear()
            DRM_SPEC.update(copy.deepcopy(saved[1]))
            EXHAUSTIVE.clear()
            EXHAUSTIVE.update(copy.deepcopy(saved[2]))
        print(f"ok: {label}")

    expect("an fd the spec forgets is refused", "GRANT_SURFACE:request.fd",
           lambda: SPEC.__setitem__("GrantSurface", []))
    expect("a pointer inside a pointee the spec forgets is refused",
           "FLIP:request.pFlipHead->flip.lut.output.pRamps",
           lambda: SPEC["Flip"][1]["elem_fields"].pop(3))
    expect("an fd inside an inline array the spec forgets is refused",
           "JOIN_SWAP_GROUP:request.member[].unicastEvent.fd",
           lambda: SPEC.__setitem__("JoinSwapGroup", []))
    expect("a field the headers do not have is refused", "request.noSuchField",
           lambda: SPEC["GrantSurface"].append(VAL("request.noSuchField", "info")))
    expect("an fd the spec puts on a field that is not one is refused",
           "MOVE_CURSOR:request.head",
           lambda: SPEC["MoveCursor"].append(FD("request.head", [MODESET_FRESH])))
    expect("a command with no spec entry is refused", "Flip",
           lambda: SPEC.pop("Flip"))
    expect("an nvidia-drm ioctl with no spec entry is refused", "GRANT_PERMISSIONS",
           lambda: DRM_SPEC.pop("GRANT_PERMISSIONS"))
    expect("an nvidia-drm fd the spec forgets is refused", "SEMSURF_FENCE_WAIT:fd",
           lambda: DRM_SPEC.__setitem__("SEMSURF_FENCE_WAIT", []))
    expect("an unreviewed member of a policy-filtered request is refused",
           "forceConnected",
           lambda: EXHAUSTIVE["struct NvKmsQueryDpyDynamicDataRequest"].discard(
               "forceConnected"))
    print("selftest passed")


README_BEGIN = "<!-- nvkms_extract.py diff: begin -->"
README_END = "<!-- nvkms_extract.py diff: end -->"


def _update_readme(summary):
    readme = OUT_DIR / "README.md"
    if not readme.exists():
        return
    text = readme.read_text()
    i, j = text.find(README_BEGIN), text.find(README_END)
    if i < 0 or j < 0:
        return
    text = text[:i + len(README_BEGIN)] + "\n\n" + summary + "\n" + text[j:]
    readme.write_text(text)


def _load_all():
    return {p.stem: json.loads(p.read_text()) for p in sorted(OUT_DIR.glob("*.json"))}


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--cache", type=Path, default=DEFAULT_CACHE,
                    help=f"where fetched headers go (default {DEFAULT_CACHE})")
    sub = ap.add_subparsers(dest="cmd", required=True)
    e = sub.add_parser("extract", help="extract one version")
    e.add_argument("version")
    e.add_argument("--src", help="use a local open-gpu-kernel-modules tree instead of fetching")
    e.add_argument("-o", "--out", type=Path, default=OUT_DIR)
    sub.add_parser("all", help="extract every version in VERSIONS, update README summary")
    sub.add_parser("check", help="regenerate every version and compare with gen/nvkms/")
    sub.add_parser("diff", help="print the cross-version summary of gen/nvkms/*.json")
    t = sub.add_parser("selftest", help="check the extractor refuses what it must (610.57.04)")
    t.add_argument("--src", help="a local 610.57.04 tree instead of fetching")
    f = sub.add_parser("fetch", help="only fetch the headers of a version")
    f.add_argument("version")
    a = ap.parse_args()
    try:
        if a.cmd == "fetch":
            print(fetch(a.version, a.cache))
        elif a.cmd == "extract":
            print(_write(_one(a.version, a.cache, a.src), a.out))
        elif a.cmd == "all":
            for v in VERSIONS:
                print(_write(_one(v, a.cache, None), OUT_DIR))
            _update_readme(diff_summary(_load_all()))
        elif a.cmd == "check":
            bad = []
            for v in VERSIONS:
                want = json.loads((OUT_DIR / f"{v}.json").read_text())
                got = json.loads(json.dumps(_one(v, a.cache, None)))
                got["abi"]["compiler"] = want["abi"]["compiler"]
                if got != want:
                    bad.append(v)
            if bad:
                sys.exit(f"out of date: {', '.join(bad)} (run ./nvkms_extract.py all)")
            print("gen/nvkms is up to date")
        elif a.cmd == "diff":
            print(diff_summary(_load_all()))
        elif a.cmd == "selftest":
            selftest(a.cache, a.src)
    except ExtractError as err:
        sys.exit(f"nvkms_extract: {err}")


if __name__ == "__main__":
    main()
