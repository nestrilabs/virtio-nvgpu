# `gen/nvkms/`: NVKMS and nvidia-drm ioctl layouts, per driver release

One JSON file per NVIDIA driver release. Each file describes what a forwarder
needs in order to carry `/dev/nvidia-modeset` (NVKMS) and nvidia-drm's private
DRM ioctls across the VM boundary:

- every command number;
- every params struct's size and its request and reply halves;
- the offset of every user pointer, every fd and every GEM handle inside those
  structs, with the rule that says when the kernel reads it and how long the
  pointee is;
- the fields the backend's NVKMS policy has to read or rewrite (ARCHITECTURE.md, "NVKMS").

This is layout **data**, not the IOCTL2 schema. The schema generator reads these
files and turns them into schema entries. The policy (which commands are
refused, which fields are sanitised) belongs to the backend and is not decided
here. The `role` and `note` values record facts about the kernel that the
policy needs. They are not rules.

Every file is produced by `../nvkms_extract.py` and checked in. Do not edit a
JSON file by hand. Change the extractor's spec and regenerate instead.

```sh
./nvkms_extract.py all        # fetch + extract every release in VERSIONS, refresh the summary below
./nvkms_extract.py check      # regenerate everything and fail if gen/nvkms/ is stale
./nvkms_extract.py selftest   # prove the refusals still fire (see "Why it fails loudly")
./nvkms_extract.py extract 610.57.04 --src ../../nvidia-driver   # from a local tree
./nvkms_extract.py diff       # print the cross-release summary
```

Requirements: Python 3.8 or later, gcc with the Linux uapi headers
(`<drm/drm.h>`), and network access the first time. Headers are cached in
`$NVKMS_EXTRACT_CACHE`, which defaults to `$TMPDIR/ogkm`; `--cache DIR`
overrides it.

## Releases covered

| release | why | tag | commit |
|---|---|---|---|
| 535.129.03 | ABI profile `gen/src/versions/v535_129_03.rs` | exists | `e573018659734be4add3e5ab3e0ad94602498ca4` |
| 580.178.04 | ABI profile `v580_178_04.rs` | exists | `c8e699821c23e4335bf23330a54a23d15cfb95e9` |
| 595.71.05 | ABI profile `v595_71_05.rs` | exists | `51edebee79919b54f498c19a0be31982cd97646e` |
| 595.99.02 | the RTX 3060 box, where every benchmark comes from | exists | `e394a404108a9ce5bba988c1dddf694dabbd4a43` |
| 610.57.04 | `nvidia-driver/`, the tree the display work is written against | exists | `e4a5faa2567f28c8eabe0ebb6422b6d0abcf37eb` |
| 615.71.09 | the RTX A2000 box | exists | `61dcc93722ecb418bb5f2e00923f05b4b8051dd1` |

Every requested tag exists in `NVIDIA/open-gpu-kernel-modules`. If a tag is
missing, the fetcher takes the nearest tag of the same branch, preferring an
older one, and records the substitution in `source.note`. For 610.57.04 the
local `nvidia-driver/` tree was also extracted: it gives output identical to
the tag's (only `source` differs).

Only a filtered subset of each release is fetched: the interface and common
headers, `nvkms.c` (for the dispatch table), `nvkms-format.c` (for plane counts),
and nvidia-drm's ioctl header and `nvidia-drm-drv.c` (for the registration
table). The subset comes from the tag's tarball, filtered by path.

## How the numbers are obtained

- **Command → params.** Command → params mapping is read from `nvkms.c`'s
  `dispatch[]` initializer (`ENTRY` / `ENTRY_CUSTOM_USER`), the same table the
  kernel indexes. `paramSize`, `requestSize`/`requestOffset` and
  `replySize`/`replyOffset` are computed by the expressions the kernel's
  `_ENTRY_WITH_USER` uses. The kernel copies in only `request`, copies out only
  `reply` (even on failure), and requires the size to match exactly.
- **Numbers.** Every number is compiled. A generated C program includes the
  release's headers and prints `offsetof`/`sizeof`, array counts, enum values
  and macro values. It also links NVIDIA's own `nvkms-format.c`, so plane
  counts come from `nvKmsGetSurfaceMemoryFormatInfo()`. It is compiled with
  `-DNV_LINUX` because the kernel modules are built that way, and nvidia-drm's
  header defines `DRM_IOCTL_NVIDIA_{FENCE,DMABUF}_SUPPORTED` as 0 without it.
- **Spec.** The spec is the one hand-written part: `SPEC` / `DRM_SPEC` in the
  extractor name fields, never offsets. Fields that NVIDIA renamed or added
  are gated with `ver(node, since=, before=)`. The bounds are releases this
  extractor has actually seen.
- **Target ABI.** x86_64 LP64 (`abi` in each file). 32-bit compat is out of
  scope.

### Why it fails loudly

The extractor refuses to write a file when any of these happens:

- the dispatch table has a command the spec has no entry for, or nvidia-drm's
  header defines an ioctl the spec has no entry for;
- the spec names a field the headers do not have. The probe fails to compile,
  and the error lists every missing designator;
- the preprocessed headers reach a leaf that looks like a pointer or an fd,
  and the spec does not classify it. The walk covers each params struct and
  each pointee the spec declares. A leaf looks like a pointer or fd if it is
  named `p[A-Z]…`, `fd` or `…Fd`, is typed `NvP64`, `T *` or a function
  pointer, or (for nvidia-drm) is named `…_ptr`, `address` or `…handle`. The
  error prints the exact path, for example
  `FLIP:request.pFlipHead->flip.lut.output.pRamps`;
- the spec classifies a field as a pointer or fd when the header walk does not
  find one there;
- a struct in `EXHAUSTIVE` (the requests a policy filters field by field:
  `QUERY_DPY_DYNAMIC_DATA`, `ALLOC_DEVICE`) has a member nobody has reviewed;
- for 610.57.04, the numbers differ from the independent measurements taken
  by hand while the display work was designed (hand-written probes against
  the 610.57.04 headers; the notes are not shipped, the numbers are in the
  extractor's `check`). All of them agree:
  every command's number, size and halves, the FLIP/SET_MODE/SET_LUT pointer
  offsets and strides, the REGISTER_SURFACE plane fds at 16/48/80, the
  JOIN_SWAP_GROUP member fds at 16+20·i, and every nvidia-drm number, size and
  offset in the §2 table.

`selftest` breaks the spec in each of these ways and checks that extraction
of 610.57.04 fails with an error naming the problem.

## File format (`"format": "virtio-nvgpu/nvkms-layout/1"`)

All integers are decimal. Offsets are in bytes.

```text
{
  format, driver_version, why,
  source: {requested, tag, note, commit, url | local_tree},
  abi:    {arch, data_model, compiler},
  nvkms: {
    ioctl: {request, request_hex, outer_size,           // _IOWR('m', 0, NvKmsIoctlParams)
            outer: {cmd|size|address: {off, size}}},
    constants:  {NAME: value},                          // NV_MAX_*, NVKMS_MAX_*, syncpt and permission types
    types:      {"struct X": sizeof},                   // NvKmsLutRamps, NvKmsFlipRequestOneHead, ...
    event_types: {NVKMS_EVENT_TYPE_*: value},           // DECLARE_EVENT_INTEREST mask bit = 1 << value
    surface_formats: [{value, name, num_planes}],       // from NVIDIA's nvkms-format.c
    exhaustive_members: {"struct X": [member, ...]},    // full top-level member list, see EXHAUSTIVE
    commands: [Command, ...]                            // sorted by nr
  },
  nvidia_drm: {
    header, command_base, constants,
    ioctls: [DrmIoctl, ...]                             // sorted by nr
  }
}

Command = {
  nr, name,                        // name without the NVKMS_IOCTL_ prefix
  dispatch: false, note            // declared in the enum but no dispatch entry: the kernel refuses it
| dispatch: true, func, params,    // params = "struct NvKms<func>Params"
  custom_user,                     // ENTRY_CUSTOM_USER: the kernel follows pointers (prepUser/doneUser)
  size,                            // must equal NvKmsIoctlParams.size
  request: {offset, size},         // the half the kernel copies in
  reply:   {offset, size},         // the half it copies out, even when the ioctl fails
  fields: [Node, ...]
}

DrmIoctl = {
  nr,                              // absolute: DRM_COMMAND_BASE + DRM_NVIDIA_*
  name, registered,                // registered: listed in nv_drm_ioctls[], else the kernel answers -EINVAL
  flags,                           // DRM_IOCTL_DEF_DRV flags, e.g. ["DRM_MASTER", "DRM_UNLOCKED"]
  conditional,                     // (optional) the #if lines around the registration
  cmd, cmd_hex, dir, struct, size, // dir: IO/IOR/IOW/IOWR; struct null and size 0 for DRM_IO
  fields: [Node, ...]
}
```

### Nodes

A node's `path` is a C designator. It is relative to the node's **scope**:
the params struct at top level, one element inside an `array`, the pointee
inside a `ptr`'s `elem`. `off` is relative to the same scope. So a node's
absolute position is the sum of its scope's offsets, plus `index × stride` for
each array it sits in. Every node has `kind`, `path` and `off`. Every node
except `array` has `size`. Any node may have `note`.

| kind | meaning | extra keys |
|---|---|---|
| `field` | plain data the forwarder or its policy reads | `role`: `target` (device/disp/head/dpy/surface acted on), `policy` (must be vetted or sanitised), `count`, `cond`, `status`, `info` |
| `fd_in` | an fd the kernel resolves with `fget()` in the caller: the forwarder must translate it | `fd_kinds`, `cond` |
| `fd_out` | the kernel installs an fd in the caller and writes its number here | `cond` |
| `gem_in` / `gem_out` | a GEM handle in the calling drm_file (nvidia-drm) | `cond` |
| `syncobj_in` | a DRM syncobj handle in the calling drm_file | `cond` |
| `ptr` | a user pointer the kernel copies from or to *during* the call | `dir` (`in`/`out`), `len`, `present`, `elem` |
| `array` | an inline array | `count`, `stride`, `valid`, `fields` |
| `kernel_ptr` | a kernel pointer in a kernel-client-only field (never forwardable) | — |
| `user_va` | a raw address the kernel pins in the caller's mm (not a copied buffer) | — |

A **ref** (used by counts, sizes and conditions) is `{path, off, size}` in the
scope of the node that uses it. The one exception is `len.written`, which is
relative to the params struct.

`cond` is a list of refs, and all of them must hold. Each ref has `op`:
`nonzero`, or `eq` together with `value` (and `value_name` if the value is a
named constant). Example: a FLIP layer's syncpoint fd exists only if
`syncObjects.specified`, `syncObjects.val.useSyncpt`, and
`syncObjects.val.u.syncpts.pre.type == NVKMS_SYNCPT_TYPE_FD`. NvBool fields
are 1 byte, so compare only `size` bytes.

`len` gives the pointee length:
- `{rule: const, type, bytes}`: always `bytes`, for example LUT ramps (6144).
- `{rule: count, count: ref, type, elem_size, min, max, max_name}`: `count ×
  elem_size`. The kernel refuses counts outside `[min, max]` when `max` is not
  null. Otherwise the limit is the count field's width.
- `{rule: bytes, size: ref, max, max_name, written}`: `size` bytes, where the
  kernel refuses sizes above `max`. For `out` pointers, `written` is the reply
  field holding how many bytes the kernel actually wrote.
- `{rule: size_eq, size: ref, type, bytes}`: the caller passes a length, and
  the kernel refuses anything but `bytes`. These are nvidia-drm's
  `nvkms_params_ptr` blocks.

`present` says when the kernel dereferences the pointer:
- `always`: the kernel always dereferences it.
- `nonzero`: the kernel dereferences it when the pointer value is non-zero.
- `{rule: size_nonzero | count_nonzero, field: ref}`: the kernel dereferences
  it only when that field is non-zero, and otherwise ignores the pointer
  value, whatever it holds.

A forwarder must write 0 into pointers that are not present.

`elem` is `{type, size, fields}`: the pointee's C type and the nodes inside
one element. FLIP's `pFlipHead` elements contain two more pointers, so FLIP
has three levels of pointers.

`valid` says which elements of an array the kernel interprets:
- `{rule: all}`: every element.
- `{rule: count, count: ref, max, max_name}`: the first `count` elements.
- `{rule: format_num_planes, format: ref, when: cond}`: the first
  `num_planes` elements, where `num_planes` comes from `surface_formats` for
  the value of `format`. The array is interpreted only when `when` holds. A
  format outside the table has 0 planes, and the kernel refuses it.

`fd_kinds` records what the kernel checks the fd against:

| kind | the kernel accepts |
|---|---|
| `modeset_fresh` | a `/dev/nvidia-modeset` fd that has never had an ioctl (NVKMS per-open type Undefined). Any ioctl on it, even a probe, makes it unusable |
| `modeset_unicast` | a `/dev/nvidia-modeset` fd already armed as a unicast-event fd |
| `modeset_grant_{surface,permissions,swap_group}` | a `/dev/nvidia-modeset` fd that a GRANT_* made into a grant fd |
| `nvidiactl_exported` | a `/dev/nvidiactl` fd an RM object was exported to (the kernel imports it) |
| `nvidiactl_export_target` | a `/dev/nvidiactl` fd the kernel exports an RM object into |
| `dmabuf` | a dma-buf |
| `sync_file` | a sync_file |

## Notable differences for forwarding

The generated summary below lists every change. These are the ones that
matter:

- **NVKMS command numbers move in both directions.**
  - `REGISTER_SURFACE` is **16** on 535 and 615, and **17** on 580–610. That
    includes 595.99.02, which the RTX 3060 box runs.
  - 580 inserted `CHECK_LUT_NOTIFIER` at 13, and 615 removed it again.
  - 595 removed the two VRR-semaphore commands, so everything above 55 moved.
  - The existing guest and backend hard-code 16, which is correct only on 535
    and 615. The command number must come from these tables, and should be
    cross-checked against the params size.
- **Params layouts change inside a branch number series.**
  - `SET_MODE`: 94,880 bytes on 535, 171,936 on 580–595, 186,784 on 610, and
    187,808 on 615.
  - `FLIP` heads: 2,056 / 4,488 / 4,952 / 4,984 bytes.
  - `ALLOC_DEVICE`: 1,080 / 1,512 / 1,440 / 1,448 bytes.
  - `VALIDATE_MODE(_INDEX)`: `pInfoString` moved in 580 and again in 595.
  - `GRANT`/`ACQUIRE`/`REVOKE_PERMISSIONS`: permissions were per (disp, head)
    through 580 (144/140/144 bytes) and per head from 595 (32/28/32 bytes).
- **Pointers.** 535 has no LUT in `NvKmsFlipCommonParams`:
  - FLIP carries only `pFlipHead`.
  - SET_MODE's ramps sit at `disp[].head[].lut`.

  From 580 the ramps are in `flip.lut`, and FLIP has three levels of pointers.
- **Fds.** One NVKMS fd existed only in 535 and 580:
  `EXPORT_VRR_SEMAPHORE_SURFACE.memFd`. NVKMS exported its device-global VRR
  semaphore into the caller's nvidiactl fd. `VRR_SIGNAL_SEMAPHORE` let any
  client signal that semaphore.
- **Kernel pointers.** 615 added an event that carries one:
  `NvKmsEventDpyCpTopologyChanged.topology`. NVKMS refuses the event bit and
  never queues the event for user clients. The event types also grew:
  `DPY_CONTENT_PROTECTION_CHANGED` is 6 and `DPY_CP_TOPOLOGY_CHANGED` is 7.
- **QUERY_DPY_DYNAMIC_DATA.** 615 renamed `overrideEdid`/`ignoreEdid` to
  `overrideMetadata`/`ignoreMetadata`, at the same offsets. A policy keyed on
  names must follow the rename.
- **nvidia-drm.**
  - In 535, `GRANT_PERMISSIONS` has no `type` field: it is 8 bytes and always
    MODESET. `REVOKE_PERMISSIONS` is 4 bytes. `GET_DEV_INFO` is 20 bytes.
  - 580 added the `SEMSURF_*` fence ioctls and `GET_DRM_FILE_UNIQUE_ID`.
  - 595 added the ROI ioctls. They are defined but not registered in any
    covered release.
  - 615 added `SEMSURF_EXPORT_TO_SYNCOBJ_POINT` (0x5d) and
    `SYNCOBJ_GET_SYNCFD` (0x5e, which produces an fd).
- 595.71.05 and 595.99.02 do not differ in anything tracked here.

## Cross-release summary (generated)

For each neighbouring pair of releases, the summary lists:

- command additions, removals and renumbering;
- changes to params sizes and to the request and reply halves;
- every pointer, fd, handle and array node that moved, changed, appeared or
  disappeared. Offsets are relative to the node's scope, so a moved parent
  does not repeat in its children;
- plain fields that appeared or disappeared (their offsets are in the JSON);
- changes to constants, type sizes, event types and surface formats.

<!-- nvkms_extract.py diff: begin -->

#### 535.129.03 → 580.178.04

- NVKMS `CHECK_LUT_NOTIFIER` added as 13
- NVKMS `SET_FLIPLOCK_GROUP` added as 60
- NVKMS `ENABLE_VBLANK_SEM_CONTROL` added as 61
- NVKMS `DISABLE_VBLANK_SEM_CONTROL` added as 62
- NVKMS `ACCEL_VBLANK_SEM_CONTROLS` added as 63
- NVKMS `VRR_SIGNAL_SEMAPHORE` added as 64
- NVKMS `FRAMEBUFFER_CONSOLE_DISABLED` added as 65
- NVKMS renumbered: `IDLE_BASE_CHANNEL`…`NOTIFY_VBLANK` (13–58) +1
- NVKMS `ALLOC_DEVICE`: size 1080 → 1512; request 616@0 → 620@0; reply 464@616 → 888@624; `reply.vtFbBaseAddress` added; `reply.vtFbSize` added; `request.deviceId` removed; `request.deviceId.migDevice` added; `request.deviceId.rmDeviceId` added; `request.registryKeys` array @40 ×16 stride 36 → array @44 ×16 stride 36; `request.sliMosaic` removed; `request.tryInferSliMosaicFromExistingDevice` removed
- NVKMS `QUERY_DPY_DYNAMIC_DATA`: size 37096 → 37160; reply 35024@2072 → 35088@2072
- NVKMS `VALIDATE_MODE_INDEX`: size 696 → 720; request 200@0 → 208@0; reply 496@200 → 512@208; `request.pInfoString` ptr @192, bytes → ptr @200, bytes
- NVKMS `VALIDATE_MODE`: size 624 → 640; request 272@0 → 288@0; reply 352@272 → 352@288; `request.pInfoString` ptr @264, bytes → ptr @280, bytes
- NVKMS `SET_MODE`: size 94880 → 171936; request 78936@0 → 155992@0; reply 15944@78936 → 15944@155992; `reply.disp` array @78944 ×8 stride 1992 → array @156000 ×8 stride 1992; `request.disp` array @16 ×8 stride 9864 → array @16 ×8 stride 19496; `request.disp[].head` array @8 ×4 stride 2464 → array @8 ×4 stride 4872; `request.disp[].head[].flip.layer` array @376 ×8 stride 248 → array @472 ×8 stride 536; `request.disp[].head[].flip.lut.input.pRamps` added; `request.disp[].head[].flip.lut.output.pRamps` added; `request.disp[].head[].lut.input.pRamps` removed; `request.disp[].head[].lut.output.pRamps` removed
- NVKMS `FLIP`: size 3104 → 3112; reply 3080@24 → 3084@24; `reply.flipHead` array @32 ×32 stride 96 → array @36 ×32 stride 96; `reply.flipResult` added; `reply.vrrSemaphoreIndex` removed; `request.pFlipHead` ptr @8, count of 2056 B → ptr @8, count of 4488 B; `request.pFlipHead->flip.layer` array @72 ×8 stride 248 → array @200 ×8 stride 536; `request.pFlipHead->flip.lut.input.pRamps` added; `request.pFlipHead->flip.lut.output.pRamps` added
- NVKMS `UNREGISTER_SURFACE`: size 12 → 16; request 8@0 → 12@0; reply 4@8 → 4@12; `request.skipSync` added
- `NV_KMS_PERMISSIONS_TYPE_SUB_OWNER` absent → 3
- sizeof `struct NvKmsFlipCommonParams` 2048 → 4480
- sizeof `struct NvKmsFlipRequestOneHead` 2056 → 4488
- sizeof `struct NvKmsSetModeOneDispRequest` 9864 → 19496
- sizeof `struct NvKmsSetModeOneHeadRequest` 2464 → 4872
- nvidia-drm `GET_CRTC_CRC32`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GEM_IMPORT_NVKMS_MEMORY`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GET_DEV_INFO`: cmd_hex 0xc0146443 → 0xc0246443; size 20 → 36
- nvidia-drm `FENCE_SUPPORTED`: conditional ['#if defined(NV_DRM_FENCE_AVAILABLE)'] → None
- nvidia-drm `PRIME_FENCE_CONTEXT_CREATE`: conditional ['#if defined(NV_DRM_FENCE_AVAILABLE)'] → None
- nvidia-drm `GEM_PRIME_FENCE_ATTACH`: cmd_hex 0x400c6446 → 0x40106446; size 12 → 16; conditional ['#if defined(NV_DRM_FENCE_AVAILABLE)'] → None
- nvidia-drm `GEM_EXPORT_NVKMS_MEMORY`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GEM_ALLOC_NVKMS_MEMORY`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GET_CRTC_CRC32_V2`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GEM_EXPORT_DMABUF_MEMORY`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GEM_IDENTIFY_OBJECT`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `DMABUF_SUPPORTED`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GET_DPY_ID_FOR_CONNECTOR_ID`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GET_CONNECTOR_ID_FOR_DPY_ID`: conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None
- nvidia-drm `GRANT_PERMISSIONS`: cmd_hex 0xc0086452 → 0xc00c6452; size 8 → 12; conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None; `type` added
- nvidia-drm `REVOKE_PERMISSIONS`: cmd_hex 0xc0046453 → 0xc0086453; size 4 → 8; conditional ['#if defined(NV_DRM_ATOMIC_MODESET_AVAILABLE)'] → None; `type` added
- nvidia-drm `SEMSURF_FENCE_CTX_CREATE` added: nr 0x54, 32 B, registered
- nvidia-drm `SEMSURF_FENCE_CREATE` added: nr 0x55, 24 B, registered
- nvidia-drm `SEMSURF_FENCE_WAIT` added: nr 0x56, 24 B, registered
- nvidia-drm `SEMSURF_FENCE_ATTACH` added: nr 0x57, 24 B, registered
- nvidia-drm `GET_DRM_FILE_UNIQUE_ID` added: nr 0x58, 8 B, registered

#### 580.178.04 → 595.71.05

- NVKMS `EXPORT_VRR_SEMAPHORE_SURFACE` removed (was 56)
- NVKMS `VRR_SIGNAL_SEMAPHORE` removed (was 64)
- NVKMS renumbered: `ENABLE_VBLANK_SYNC_OBJECT`…`ACCEL_VBLANK_SEM_CONTROLS` (57–63) -1; `FRAMEBUFFER_CONSOLE_DISABLED` 65→63
- NVKMS `QUERY_DPY_DYNAMIC_DATA`: size 37160 → 37168; reply 35088@2072 → 35096@2072
- NVKMS `VALIDATE_MODE_INDEX`: size 720 → 736; request 208@0 → 224@0; reply 512@208 → 512@224; `request.pInfoString` ptr @200, bytes → ptr @216, bytes
- NVKMS `VALIDATE_MODE`: size 640 → 656; request 288@0 → 304@0; reply 352@288 → 352@304; `request.pInfoString` ptr @280, bytes → ptr @296, bytes
- NVKMS `FLIP`: size 3112 → 3104; reply 3084@24 → 3080@24; `reply.flipHead` array @36 ×32 stride 96 → array @32 ×32 stride 96
- NVKMS `GRANT_PERMISSIONS`: size 144 → 32; request 140@0 → 28@0; reply 4@140 → 4@28; `request.permissions.flip.disp` removed; `request.permissions.flip.disp[].head` removed; `request.permissions.flip.disp[].head[].layerMask` removed; `request.permissions.flip.head` added; `request.permissions.flip.head[].layerMask` added; `request.permissions.modeset.disp` removed; `request.permissions.modeset.disp[].head` removed; `request.permissions.modeset.disp[].head[].dpyIdList` removed; `request.permissions.modeset.head` added; `request.permissions.modeset.head[].dpyIdList` added
- NVKMS `ACQUIRE_PERMISSIONS`: size 140 → 28; reply 136@4 → 24@4; `reply.permissions.flip.disp` removed; `reply.permissions.flip.disp[].head` removed; `reply.permissions.flip.disp[].head[].layerMask` removed; `reply.permissions.flip.head` added; `reply.permissions.flip.head[].layerMask` added; `reply.permissions.modeset.disp` removed; `reply.permissions.modeset.disp[].head` removed; `reply.permissions.modeset.disp[].head[].dpyIdList` removed; `reply.permissions.modeset.head` added; `reply.permissions.modeset.head[].dpyIdList` added
- NVKMS `REVOKE_PERMISSIONS`: size 144 → 32; request 140@0 → 28@0; reply 4@140 → 4@28; `request.permissions.flip.disp` removed; `request.permissions.flip.disp[].head` removed; `request.permissions.flip.disp[].head[].layerMask` removed; `request.permissions.flip.head` added; `request.permissions.flip.head[].layerMask` added; `request.permissions.modeset.disp` removed; `request.permissions.modeset.disp[].head` removed; `request.permissions.modeset.disp[].head[].dpyIdList` removed; `request.permissions.modeset.head` added; `request.permissions.modeset.head[].dpyIdList` added
- NVKMS `ALLOC_SWAP_GROUP`: size 40 → 12; request 36@0 → 8@0; reply 4@36 → 4@8
- NVKMS `SET_FLIPLOCK_GROUP`: size 328 → 72; request 324@0 → 68@0; reply 4@324 → 4@68
- sizeof `struct NvKmsPermissions` 132 → 20
- `struct NvKmsAllocDeviceRequest` members: added [], removed ['sliMosaic', 'tryInferSliMosaicFromExistingDevice']
- nvidia-drm `REGISTER_ROI` added: nr 0x59, 24 B, not registered
- nvidia-drm `UNREGISTER_ROI` added: nr 0x5a, 8 B, not registered
- nvidia-drm `GET_CRTC_ROI_CRCS` added: nr 0x5b, 3112 B, not registered
- nvidia-drm `GET_ROI_CAPABILITIES` added: nr 0x5c, 32 B, not registered
- nvidia-drm header kernel-open/nvidia-drm/nvidia-drm-ioctl.h → kernel-open/nvidia-drm/nv_drm_common_ioctl.h

#### 595.71.05 → 595.99.02

- nothing a forwarder depends on changed

#### 595.99.02 → 610.57.04

- NVKMS `REGISTER_VBLANK_INTR_CALLBACK` added as 64
- NVKMS `UNREGISTER_VBLANK_INTR_CALLBACK` added as 65
- NVKMS `ALLOC_DEVICE`: size 1512 → 1440; reply 888@624 → 816@624
- NVKMS `QUERY_DPY_STATIC_DATA`: size 92 → 96; reply 80@12 → 84@12
- NVKMS `SET_MODE`: size 171936 → 186784; request 155992@0 → 170840@0; reply 15944@155992 → 15944@170840; `reply.disp` array @156000 ×8 stride 1992 → array @170848 ×8 stride 1992; `request.disp` array @16 ×8 stride 19496 → array @16 ×8 stride 21352; `request.disp[].head` array @8 ×4 stride 4872 → array @8 ×4 stride 5336; `request.disp[].head[].flip.layer` array @472 ×8 stride 536 → array @488 ×8 stride 592
- NVKMS `FLIP`: `request.pFlipHead` ptr @8, count of 4488 B → ptr @8, count of 4952 B; `request.pFlipHead->flip.layer` array @200 ×8 stride 536 → array @216 ×8 stride 592
- sizeof `struct NvKmsFlipCommonParams` 4480 → 4944
- sizeof `struct NvKmsFlipRequestOneHead` 4488 → 4952
- sizeof `struct NvKmsSetModeOneDispRequest` 19496 → 21352
- sizeof `struct NvKmsSetModeOneHeadRequest` 4872 → 5336

#### 610.57.04 → 615.71.09

- NVKMS `CHECK_LUT_NOTIFIER` removed (was 13)
- NVKMS renumbered: `IDLE_BASE_CHANNEL`…`UNREGISTER_VBLANK_INTR_CALLBACK` (14–65) -1
- NVKMS `ALLOC_DEVICE`: size 1440 → 1448; reply 816@624 → 824@624
- NVKMS `QUERY_DPY_DYNAMIC_DATA`: `request.ignoreEdid` removed; `request.ignoreMetadata` added; `request.overrideEdid` removed; `request.overrideMetadata` added
- NVKMS `SET_MODE`: size 186784 → 187808; request 170840@0 → 171864@0; reply 15944@170840 → 15944@171864; `reply.disp` array @170848 ×8 stride 1992 → array @171872 ×8 stride 1992; `request.disp` array @16 ×8 stride 21352 → array @16 ×8 stride 21480; `request.disp[].head` array @8 ×4 stride 5336 → array @8 ×4 stride 5368; `request.disp[].head[].flip.layer` array @488 ×8 stride 592 → array @520 ×8 stride 592
- NVKMS `FLIP`: `request.pFlipHead` ptr @8, count of 4952 B → ptr @8, count of 4984 B; `request.pFlipHead->flip.layer` array @216 ×8 stride 592 → array @248 ×8 stride 592
- NVKMS `GET_NEXT_EVENT`: `reply.event.u.dpyCpTopologyChanged.topology` added
- sizeof `struct NvKmsFlipCommonParams` 4944 → 4976
- sizeof `struct NvKmsFlipRequestOneHead` 4952 → 4984
- sizeof `struct NvKmsSetModeOneDispRequest` 21352 → 21480
- sizeof `struct NvKmsSetModeOneHeadRequest` 5336 → 5368
- `NVKMS_EVENT_TYPE_DPY_CONTENT_PROTECTION_CHANGED` absent → 6
- `NVKMS_EVENT_TYPE_DPY_CP_TOPOLOGY_CHANGED` absent → 7
- `struct NvKmsQueryDpyDynamicDataRequest` members: added ['ignoreMetadata', 'overrideMetadata'], removed ['ignoreEdid', 'overrideEdid']
- nvidia-drm `SEMSURF_EXPORT_TO_SYNCOBJ_POINT` added: nr 0x5d, 24 B, registered
- nvidia-drm `SYNCOBJ_GET_SYNCFD` added: nr 0x5e, 16 B, registered

<!-- nvkms_extract.py diff: end -->
