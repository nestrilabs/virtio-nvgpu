# Display passthrough: verification against NVK, NVIDIA and Linux references

> **A review record, not current documentation.** This is the verification
> review of branch `display-passthrough` at `fbfa3fb` (2026-09-25), kept
> because [`SECURITY.md`](../../SECURITY.md) and [`TESTING.md`](../../TESTING.md)
> cite its finding ids and its §5 on-device procedures. File and line
> references are to that commit; paths prefixed `nv:`, `linux:`, `mesa:`,
> `ogd:` and `ehk:` are checkouts of NVIDIA's open-gpu-kernel-modules
> (610.57.04), Linux 7.2.7, Mesa, open-gpu-doc and envyhooks. The status of
> each finding is in SECURITY.md's finding index, not here. §5 had two
> sections numbered 5.1 and two 5.2; the first pair is 5.0a and 5.0b now, so
> that "§5.1" and "§5.2" name the later two, as TESTING.md cites them.
>
> Paths and names that have moved since: `scripts/run-guest.sh` is
> `rig/run-guest.sh`; `device/src/nvidia.rs` is `device/src/nvidia/`
> (`mod.rs`, `v1.rs`, `rm.rs`, `rmmap.rs`, `placement.rs`, `uvm.rs`,
> `hostnodes.rs`), `device/src/inject.rs` is `device/src/inject/`, and the
> guest module's `nvgpu_main.c` was split into several files
> (`driver/README.md` has the table). `DESIGN.md` and `research/*.md` (cited
> as `R:` in some code comments) were working notes and were never shipped.

Scope: virtio-nvgpu branch `display-passthrough` at fbfa3fb. The review ran read-only through four lenses: layout/modifiers, sync, memory/caching, and submission/display. Each finding was checked by an adversarial verifier. Only findings whose verdict was `real=true` are listed as discrepancies. The severity given is the verifier's, not the original reviewer's.

Path prefixes: repo-relative = this repository; `nv:` = NVIDIA's open-gpu-kernel-modules 610.57.04 (with local `[modeset-dbg]` logging patches, so line numbers in nvidia-drm-modeset.c and nvkms-evo.c are shifted by roughly 20-30 lines); `linux:` = Linux 7.2.7; `mesa:` = Mesa; `ogd:` = open-gpu-doc; `ehk:` = envyhooks.

---

## 1. Executive summary

- **The layout and modifier path is sound on a 610 host with modeset=1.**
  - The page-kind words from GET_DEV_INFO pass through unchanged, and nothing rewrites modifiers.
  - The linux-dmabuf format table crosses byte for byte, and the dev_t rewrite is correct.
  - A guest buffer reaches the host as the host's own NVKMS GEM object, so direct-scanout eligibility is identical to bare metal.
  - NVK/nil agrees on kinds, GOB version, sector layout and the rejection of compressed modifiers.
- **The per-submission path is correct in design.** Submission is ioctl-free (USERD GPPut plus the usermode doorbell through the window). GPU VAs are not rewritten, and display passthrough adds no per-submission step.
- **The NVKMS permission model, NISO rules, notifier formats and generated FLIP/SET_MODE/REGISTER_SURFACE layouts match 610.57.04.**
- **The backend does not yet fence off several host-kernel and cross-tenant exposures reachable from an untrusted guest.**
  1. **Critical:** `SEMSURF_FENCE_CTX_CREATE` (0x54) forwards an unbounded `index`. nvidia-drm adds `index*stride` to a kernel mapping without a bounds check, which gives the guest a host-kernel read oracle and a way to crash the host.
  2. **High:**
     - 0x54 lets a guest import any host RM client's semaphore surface.
     - REGISTER_WAITER and NV_EVENT_BUFFER fds are not translated. NV_EVENT_BUFFER can oops the host.
     - NVKMS FLIP head-level fields (cursor, HDR, colorimetry) skip every permission check.
     - Non-coherent sysmem becomes WB in the guest on Intel/VMX.
     - GET_DEV_INFO overruns a 20-byte caller on a 535 host.
  3. **Medium:**
     - Per-mapping cache type is not carried to the guest, so the doorbell is WC.
     - Read-only placements are mapped writable, so a guest write kills the VM.
     - The RM window extent is reused while guest PTEs still map it.
     - Capability bits are overridden instead of masked.
     - IDENTIFY always answers NVKMS.
     - fd fields come back holding backend handles, which breaks envyhooks.
     - CRC32 ioctls on the render node reach every host CRTC.
     - FLIP_OCCURRED awaken causes a WARN flood on the host.
     - NVKMS grants outlive a lease end.
     - Semsurf contexts have no cap (one host kthread each).
- **The Intel caching problem cannot show on the test hosts.** The dev and benchmark hosts are AMD (Ryzen 9 9950X and 9850X3D). NPT honours guest PAT there.
- **Driver-version mismatch.** The dev box runs kernel module/GSP 595.99.02 (`/proc/driver/nvidia/version`), while the reference tree is 610.57.04. The semsurf and NVKMS layouts that matter are identical between the two. Any envyhooks run needs matching versions (see §5.0).
- **Rejected claims:** 7 of the reviewers' 35 claims were refuted (§4).

---

## 2. Confirmed assumptions

| # | Assumption | Our code | Reference |
|---|---|---|---|
| L1 | GET_DEV_INFO gpu_id, mig_device, generic_page_kind, page_kind_generation and sector_layout come from the host render node and pass through unchanged | device/src/nvidia.rs:158-190, 776-802; driver/nvgpu_main.c:2275-2321; driver/nvgpu_drm.c:231 | nv:kernel-open/nvidia-drm/nvidia-drm-drv.c:793-797, 815-830; nv_drm_common_ioctl.h:213-227 |
| L2 | primary_index is rewritten to the guest card minor (native meaning is dev->primary->index) | driver/nvgpu_drm.c:303-304 | nvidia-drm-drv.c:1092 |
| L3 | Page kinds are consistent: GENERIC_MEMORY 0x06 on Turing+, 16BX2 0xfe before Turing; TuringColor2D uses GOB v2 and sector layout Desktop=1 | (same RM, same GPU) | mesa:src/nouveau/headers/nvidia/hwref/turing/tu102/dev_mmu.h:113; hwref/pascal/gp100/dev_mmu.h:306; mesa:src/nouveau/nil/modifiers.rs:64-79; nil/image.rs:816-822 |
| L4 | Modifier bitfields match drm_fourcc; only c=0, s=1 are advertised; compressed modifiers are rejected twice; nothing in our path rewrites modifiers | wlwire/src/policy_table.rs:230-260 | linux:include/uapi/drm/drm_fourcc.h:1022-1029; nvidia-drm-drv.c:819-826; nvidia-drm-fb.c:184-190, 282-294; nil/modifiers.rs:333-335 |
| L5 | A guest buffer reaches the host as the same host GEM object (self-import fast path), so ADDFB2 and direct scanout eligibility match bare metal | driver/nvgpu_drm.c:~985-1000; device/src/session.rs:452-456 | nvidia-drm-gem.c:103, 148-176; nvidia-drm-drv.c:1954; nvidia-drm-fb.c:167-169, 236-245 |
| L6 | Host-side ADDFB2 checks run on the host: every handle is IDENTIFYed as NVKMS, and unused planes are zeroed | device/src/xfer.rs:1022-1035, 1239-1247 | nv:src/nvidia-modeset/kapi/src/nvkms-kapi.c:2317-2324 |
| L7 | GEM_ALLOC/IMPORT_NVKMS_MEMORY round-trip the whole struct, including `compressible`; the proxy size equals the host size | driver/nvgpu_drm.c:408-446, 883-885, 107-116, 1344-1352 | nvidia-drm-gem-nvkms-memory.c:414-421, 658 |
| L8 | CREATE_DUMB, GETFB and GETFB2 handles get the host object size through lseek on the host dma-buf; pitch and size are the host's; GETFB2 dedup matches the core | device/src/xfer.rs:1370-1439; driver/nvgpu_kms.c:1324-1343, 1670-1688; driver/nvgpu_i2.c:819-846 | nvidia-drm-gem-nvkms-memory.c:464-471; linux:drivers/gpu/drm/drm_framebuffer.c:640-652 |
| L9 | linux-dmabuf dev_t rewrite: glibc encoding, only for 8-byte arrays, render node to render node, card to primary | wlwire/src/engine.rs:128-137, 1047; policy_table.rs:230-245; driver/nvgpu_wl.c:193-220 | egl-wayland wayland-egldisplay.c:310,326; wayland-egldevice.c:80-103; wayland-eglsurface.c:1791-1799 |
| L10 | The format table crosses byte for byte into a sealed memfd; indices are unchanged; each resend is a new blob | wlwire/src/sys.rs:26-45; blob.rs:111-129 | wayland-egldisplay.c:417-431 |
| L11 | linux-dmabuf is clamped to v5, and wl_drm is not offered, so egl-wayland uses main_device plus the first matching tranche (Hyprland's BL-only scanout tranche) | policy_table.rs:106, 147-149 | wayland-egldisplay.c:720-760; wayland-eglsurface.c:1656-1671, 2297-2298 |
| S1 | nvidia-drm semsurf ABI 0x54-0x57: numbers, sizes, directions and offsets match 610 and 595 | driver/nvgpu_fence.c:1421-1464; gen/schema/nvidia_drm.py:140-173 | nv_drm_common_ioctl.h:51-54, 155-173, 355-420 |
| S2 | 0x54 nested block is {hClient, hSemaphoreSurface, u64 size}, 16 bytes, with no fd | gen/schema/nvidia_drm.py:140-149 | nvkms-kapi-private.h:61-65; nvkms-kapi-sync.c:198-202 |
| S3 | A fence context is a host GEM object; the guest proxy can't be exported or mmapped and closes with its host handle | nvgpu_fence.c:1474-1537 | nvidia-drm-fence.c:1331, 1490-1497, 1764-1776 |
| S4 | 0x56 with a signalled fence goes through a real stub sync_file, the same path as native | nvgpu_fence.c:1759-1783; device/src/hostfd.rs:588-661; session.rs:490-513 | nvidia-drm-fence.c:1571-1601, 1713-1732; linux drm_syncobj.c:571-572, 759-790 |
| S5 | 0x57 re-homes the buffer (same GEM, same resv), so host scanout implicit sync sees the fence | nvgpu_fence.c:1539-1700 | nvidia-drm-fence.c:1764-1776; linux drm_gem_atomic_helper.c:117-140 |
| S6 | Host fence proxies: status comes from FILE_INFO; a timeout's -ETIMEDOUT reaches the guest | nvgpu_fence.c:173-230; device/src/pump.rs:280-287, 719-734 | linux sync_file.c:197-211, 322-324; nvidia-drm-fence.c:39, 762, 896 |
| S7 | Unwrapping merges our own proxies into a host SYNC_MERGE, five at a time | nvgpu_fence.c:340-469 | glcore FILE_INFO at 0xa13810/0xa13872 |
| S8 | SYNCOBJ IMPORT_SYNC_FILE with a signalled fence becomes SIGNAL or TIMELINE_SIGNAL, as in native import | nvgpu_fence.c:1303-1362 | linux drm_syncobj.c:728-757, 905-915 |
| S9 | No syncobj wait parks a host thread: timeouts are zeroed, WAIT_FOR_SUBMIT and EVENTFD are refused, and the guest sleeps | device/src/fence.rs:84-160; nvgpu_fence.c:918-1111 | drm_syncobj.c:1100-1170, 1202-1225; mesa nvkmd_nouveau_ctx.c:261 |
| S10 | KMS IN_FENCE_FD and OUT_FENCE_PTR behave as native | driver/nvgpu_kms.c:1089-1144, 1348-1375; xfer.rs:1165-1172 | nvidia-drm-modeset.c:160-315; linux drm_atomic_uapi.c:473, 551-560, 1583-1584 |
| S11 | GPU-GPU and GPU-display sync never crosses the VM (host channel SEM_*, display window semaphores programmed by host NVKMS) | (design) | mesa clc86f.h:111-112, 211-251; ogd clc67e.h:98-125; nvkms-evo3.c:3549-3620; sem_surf.c:482-495 |
| S12 | Semaphore surfaces are sysmem only; the host-side fence logic uses the host kernel mapping | (design) | sem_surf.c:758-767, 823-832; nvidia-drm-fence.c:716-740 |
| M1 | vidmem through BAR1 is effectively WC in the guest, the same as native and nouveau/NVK | driver/nvgpu_drm.c:702; nvgpu_main.c:1178 | mapping_cpu.c:294-298; linux vmx.c:7825-7826; linux nouveau_bo.c:1289-1295; mesa nvkmd_nouveau_mem.c:158-161 |
| M2 | Dumb and cursor buffers on a dGPU are vidmem | — | nvidia-drm-gem-nvkms-memory.c:478-483 |
| M3 | Cached sysmem is WB and coherent under default Intel KVM (WB with IPAT) | — | nv-mmap.c:473-474, 726-751; linux vmx.c:7828-7830; virt_mem_allocator_gm107.c:2815-2818 |
| M4 | Wayland shm pools are copied (pread/pwrite), not mapped, so there is no memtype aliasing | wlwire/src/shm.rs:1-23, 269-278 | — |
| U1 | Submission is ioctl-free: GPFIFO, pushbuffer, GPPut at USERD+0x8c, doorbell at usermode+0x90, all through the window | device/src/nvidia.rs:1418-1432, 2915-2937, 4046-4069; nvgpu_main.c:1205-1207 | ogd classes/host/clc56f.h:59-61; clc361.h:33; linux nvif/chanc36f.c:16, userc361.c:40; BENCHMARKS.md:57-59 |
| U2 | Work-submit token and GPU VAs pass unchanged (MAP_MEMORY_DMA takes the default path) | nvidia.rs:2001-2010, 2037-2049 | ehk tracker/object.rs:1326-1428 |
| U3 | RM_ALLOC and RM_CONTROL return the caller's own params pointer; the guest creates /dev/nvidiactl and /dev/nvidiaN; envyhooks' mprotect/TF tracking works on our VM_PFNMAP vma | nvidia.rs:2084-2093, 2388-2389; nvgpu_main.c:625-629, 797-801, 1079-1127, 2672-2689 | ehk nvrm/mod.rs:117-135; hooks/detail/signal.rs:19-29, 97-217 |
| U4 | envyhooks' GPFIFO/USERD decode matches clc56f.h | — | ehk pushbuf.rs:84-91, 256-283; clc56f.h:265-276 |
| U5 | nv_push_dump: `nv_push_dump file.bin ARCH`; default subchannel map 0=3D 1=compute 2=i2m 3=2D 4=copy | — | mesa:src/nouveau/headers/nv_push_dump.c:93-109, 156-159; nv_push.c:55-74, 148-156 |
| D1 | SET_CURSOR_IMAGE, MOVE_CURSOR, SetDpyAttribute and SetLayerPosition are unchecked in NVKMS, so our gates are needed | device/src/nvkms.rs:485-495, 553-625 | nvkms.c:2229-2285, 3070-3085 |
| D2 | Sub-owner-only commands are refused by NVKMS; syncobject commands need Tegra | — | nvkms.c:2526, 3830, 3880, 4294, 4751, 4910, 4441-4445 |
| D3 | Permission model: modeset permission covers every layer; SET_MODE commit needs a dpy list; ACQUIRE returns the accumulated set | nvkms.rs:127-142, 678-689 | nvkms-flip.c:57-60, 95-101; nvkms-modeset.c:3940-3950; nvkms.c:3408-3509 |
| D4 | NISO notifier/semaphore: 16-byte elements within 4 KiB match the hardware fields | — | nvkms-utils-flip.c:113-165; ogd clc37e.h:97-111, clc57e.h, clc67e.h:98-125; clc37d.h:35-55 |
| D5 | Display-read surfaces are refcounted RM objects in NVKMS's client; unregistering idles the layers | — | nvkms-surface.c:930-1004 |
| D6 | REGISTER_SURFACE fd kinds nvidiactl→K_DEV_CTL and dmabuf→K_DMABUF; planes at 16/48/80 | gen/schema/nvkms.py:67-75; nvidia.rs:2178-2200 | nvkms.c:2722-2730; nvkms-surface.c:506-592 |
| D7 | Generated offsets agree with a fresh probe (FLIP layer @208, stride 592; awaken +52; useSyncpt +60; specified +96; SET_MODE flip.layer 488) | gen/nvkms/610.57.04.json; gen/schema/nvkms.py:357-365 | nvkms-api.h (a hand-written probe) |
| D8 | nvidia-drm event timing (synchronous FLIP_COMPLETE when no HW flip happens; -EBUSY; 3 s cap) is handled by reserving events up front | driver/nvgpu_kms.c:765-850, 922-956 | nvidia-drm-modeset.c:96-138, 446-470, 765-768, ~880-930 |
| D9 | Lease revoke through REVOKE_LEASE and at lessee close is native | device/src/kms.rs:349-387; nvkms.rs:291-297 | nvidia-drm-drv.c:1588-1600, 1634-1774 |
| D10 | Modeset fds must not be O_NONBLOCK (nvkms_poll skips poll_wait) | DESIGN | nvidia-modeset-linux.c:2023-2025 |

---

## 3. Real discrepancies, ranked by verified severity

Each entry gives what is wrong, the fix, and the files to change. Where two lenses found the same root cause, the entries are merged.

### Critical

**C-1. 0x54 SEMSURF_FENCE_CTX_CREATE: unbounded `index` gives the guest a host-kernel out-of-bounds read and crash primitive**
- **Ours:** device/src/fence.rs:109-160 has no case for 0x54, and gen/schema/nvidia_drm.py:140-149 checks only size. The guest forwards the struct unchanged (driver/nvgpu_fence.c:1731-1737).
- **Reference:** nvidia-drm-fence.c:1257-1261 computes `semMapping += index*stride` with no bound and no overflow check, then READ_ONCEs it (716-740) on every FENCE_CREATE and timeout. That happens before RM ever sees the index. The result is an oracle for any 64-bit kernel word, or an oops.
- **Fix:**
  1. At GPU open, query `NV2080_CTRL_CMD_FB_GET_SEMAPHORE_SURFACE_LAYOUT` on the backend's own RM client to get `stride` and `max_off`.
  2. In a 0x54 arm of `fence::before`, working on the backend's copy, require:
     - nested size == 16;
     - `index.checked_mul(stride)?.checked_add(stride)? <= semaphoreSurfaceSize`;
     - when `max_off != 0`, `index*stride + max_off + 8 <= size`.
  3. Return EINVAL on any failure. Return EOPNOTSUPP when stride is 0.
  4. Add unit tests for the wrap case, the off-by-one case and the last valid index. Fix the comment at fence.rs:109-111.
- **Files:** device/src/fence.rs, device/src/policy.rs, device/src/nvidia.rs (layout query), gen/schema/nvidia_drm.py (comment).

### High

**H-1. 0x54 imports any host RM client's semaphore surface**
- The nested hClient/hSemaphoreSurface is duplicated at kernel privilege, which skips client validation: nvkms-kapi-sync.c:240-252, rs_client.c:540-569, client.c:774-788. NVKMS itself forbids this for userspace (nvkms.c:2722-2730).
- 0x56 can then write `post` into another tenant's semaphore slot (nvidia-drm-fence.c:1571-1601), and 0x55 leaks that tenant's progress.
- **Fix:**
  - Keep a per-VM set of RM client handles. Insert on a successful RM_ALLOC of class 0x0, 0x1 or 0x41, using hObjectNew from the reply. Remove on RM_FREE of the root, on file close and on reset.
  - In the 0x54 hook, read hClient from the nested copy and return EPERM unless it is in the set.
  - Optionally, also require that hSemaphoreSurface was allocated as class 0xda.
- **Files:** device/src/nvidia.rs (RM_ALLOC/RM_FREE tracking), device/src/fence.rs or device/src/policy.rs (0x54 check), device/src/session.rs (reset).

**H-2. OS-event fd in REGISTER_WAITER/UNREGISTER_WAITER and NV_EVENT_BUFFER (0x90cd) is not translated; 0x90cd can oops the host**
- **Ours:** only 0x3d05/0x3d06 and NV0005 are translated (driver/nvgpu_main.c:576-593, 733-778; device/src/nvidia.rs:2145-2218).
- **Reference:**
  - `notificationHandle` is resolved by (fd, hClient) (sem_surf.c:1697-1718, 1855-1877; os.c:1789-1815). It sits at offset 24 for 0xda0003 and offset **16** for 0xda0005 (ctrl00da.h:207-212, 251-255).
  - For 0x90cd, eventbufferConstruct ignores a failed lookup and later dereferences the raw number as an event pointer (event_buffer.c:463-477, 495-505).
  - glcore, eglcore and libcuda contain 0xda0003.
- **Fix:**
  - Guest: for 0xda0003 (off 24) and 0xda0005 (off 16), and for RM_ALLOC 0x90cd (off 40), map a nonzero u64 fd through `nvgpu_handle_for_fd`. Return EBADF if that fails. Restore the caller's value in the reply.
  - Backend: map handle → host fd with `dev_fd()` and restore the value afterwards. Keep a set of live (hClient, host fd) OS events (ALLOC_OS_EVENT/FREE_OS_EVENT). Refuse 0x90cd and waiters whose pair is not in that set, and refuse 0x90cd outright until this is in place.
  - Add tests for translate, zero passthrough and unknown handles.
- **Files:** driver/nvgpu_main.c, driver/nvgpu_rm_intercepts.h, device/src/nvidia.rs.

**H-3. NVKMS FLIP head-level fields bypass all permission checks (cursor, HDR infoframe, colorimetry, tf, dithering, olutFpNormScale)**
- **Ours:** FLIP is checked only for useSyncpt (device/src/nvkms.rs:236-248, 364-370).
- **Reference:** nvCheckFlipPermissions counts only fields that dirty a layer (nvkms-flip.c:84-111; nvkms-hw-flip.c:238-257). A cursor-only or HDR-only flip therefore has changedLayersMask=0 and passes. The fields are then applied (nvkms-flip.c:163-190; nvkms-hw-flip.c:1128-1157, 1349-1358; nvkms-headsurface-ioctl.c:351-380).
- **Effect:** any guest can draw a cursor on, or switch HDR on, the host compositor's heads. This makes our SET_CURSOR_IMAGE and MOVE_CURSOR gates useless.
- **Fix:**
  - Outside `--kms-card`, walk each pFlipHead element (sd@0, head@4; per-head index 0 on 595+) and require `perms(target, dev).head(d, h)`. Otherwise return EPERM.
  - Add `device`, `sd` and `head` offsets to the flip layout.
  - SET_MODE needs no gate, because NVKMS ValidateRequest already enforces it (nvkms-modeset.c:3940-3966).
- **Files:** gen/schema/nvkms.py, gen/nvkms/*.json (regenerate), device/src/nvkms.rs (+ tests).

**H-4. Non-coherent sysmem (COHERENCY UNCACHED/WC, and the RM default) becomes effective WB in the guest on Intel/VMX, while the GPU and display access it without snooping (latent on the AMD test hosts)**
- **Ours:** the guest always uses WC (nvgpu_main.c:1177-1178; nvgpu_drm.c:702, 724). Nothing inspects or rewrites COHERENCY.
- **Reference:**
  - The pages are RAM (vm_insert_page, not PageReserved; nv-mmap.c:473-474, 747-752). KVM therefore returns WB|IPAT under the default IGNORE_GUEST_PAT quirk (linux:arch/x86/kvm/mmu/spte.c:109-128; vmx.c:7808-7830, 8794-8797).
  - RM gives non-cached sysmem the SYS_NONCOH aperture (virt_mem_allocator_gm107.c:2815-2822; mem.c:203, 1201-1215).
  - The result is stale semaphore and notifier reads, and pushbuffer/GPFIFO writes the GPU does not see.
- **Fix:**
  1. Backend: when the GPU reports coherent CTXDMA as allowed (NV2080_CTRL_CMD_BUS_GET_INFO, the same test as nvkms-rm.c:226-247), rewrite UNCACHED, WRITE_COMBINE or 0 to WRITE_BACK in:
     - NV01_MEMORY_SYSTEM RM_ALLOC attr[31:29];
     - VID_HEAP_CONTROL ALLOC_SIZE when the location is PCI/ANY;
     - NVOS02 ALLOC_MEMORY flags.
  2. Skip ISO=YES and NISO_DISPLAY=YES attr2, or refuse sysmem ISO when ISO coherency is FALSE.
  3. Rewrite the NVKMS ALLOC_DEVICE reply's {iso,niso}IOCoherencyModes to match.
  4. Document that on Intel with self-snoop the VMM should use KVM_CAP_DISABLE_QUIRKS2(IGNORE_GUEST_PAT). Log a startup warning on GenuineIntel.
  5. Correct ARCHITECTURE.md:167-169 and README.md:249-250.
- **Files:** device/src/nvidia.rs (RM_ALLOC / ALLOC_MEMORY / VID_HEAP_CONTROL paths), device/src/nvkms.rs (ALLOC_DEVICE reply), ARCHITECTURE.md, README.md.

**H-5. GET_DEV_INFO ignores the struct size: on a 535 host (a supported profile) the fields are scrambled and the guest writes 16 bytes past the caller's 20-byte struct**
- **Ours:** the backend always sends 0xC0246443 (36 B) and reads the reply as the 610 layout (device/src/nvidia.rs:156, 173-181, 786-795). The guest only warns on a size mismatch, patches by 610 index and copies out 36 B (driver/nvgpu_drm.c:241-245, 269-286, 303-306).
- **Reference:** the 535 layout is 20 B (ogkm/535.129.03 nvidia-drm-ioctl.h:153-161; gen/nvkms/README.md:270,316). DESIGN.md:336-338 lists 535 as a profile.
- **Fix:**
  - Backend: pick 0xC0146443 or 0xC0246443 per host version, normalise the reply to a named 9-word record (20-byte replies get supports_alloc=0), and send the host struct size with DRI_INFO.
  - Guest: switch on `_IOC_SIZE(cmd)`:
    - 36: current behaviour.
    - 20: emit `{gpu_id, guest primary_index, kind, gen, sector}`, only when the host size is 20.
    - >36: copy 36 and clear_user the rest.
    - anything else: EINVAL.
  - Never write more than `_IOC_SIZE`. Add a 20-byte `NV_GET_DEV_INFO_V535` schema entry.
- **Files:** device/src/nvidia.rs, driver/nvgpu_drm.c, gen/schema/nvidia_drm.py, driver/gen/nvgpu_schema.h (regenerate).

### Medium

**M-1. The per-mapping cache type is never carried to the guest, and the backend's classification is wrong for sysmem and regmem. The doorbell and usermode registers are WC where native is UC, and cached sysmem is WC on AMD.**
- **Ours:**
  - Everything is mapped WC in the guest.
  - The backend's zone comes from NVOS33 flags, which the host escape resets to DEFAULT before RM runs (nv:src/nvidia/arch/nvalloc/unix/src/escape.c:600-601). RM writes a real type back only for FB (mapping_cpu.c:590-591). So sysmem and regmem all land in the WC zone (device/src/nvidia.rs:2886-2903; shm.rs:143-147; fixture mapping-replay.tsv:11).
  - MmapResp has no cache field (driver/nvgpu_wire.h:127-133; protocol/src/messages.rs:323-330).
- **Reference:** the host forces UC for register and UD offsets (nv-mmap.c:589-607) and uses `at->cache_type` for sysmem (nv-mmap.c:727-729). USERMODE_A is REGMEM (usermode_api.c:48, 94-101; kernel_fifo_gv100.c:371-374).
- **Effect:** doorbell latency or combining, depending on whether the closed driver fences; NVIDIA's own UVM code does `wmb()` before the doorbell (uvm_channel.c:1158-1162). On AMD, reads of WB-intended sysmem are slow.
- **Fix:**
  - Turn `padding` into a `caching` field (0 legacy WC, 1 WB, 2 WC, 3 UC), gated on HELLO v2 or a feature bit.
  - Backend classification:
    - REFLECTED (FB): use the returned CACHING_TYPE.
    - DIRECT (sysmem): use the tracked ATTR_COHERENCY per (hClient, hMemory), propagated through DUP.
    - Otherwise, or when `NV0000_CTRL_CMD_CLIENT_GET_ADDR_SPACE_TYPE` returns REGMEM, or for usermode classes c361…c761: UC.
    - map_unrecorded: carry its WB or WC choice.
  - Guest nvgpu_mmap and GEM mmap/vmap: pgprot_noncached, pgprot_writecombine, or vm_get_page_prot for WB.
  - For WB on the window BAR, the VMM must program a WB MTRR over the WB zone. Otherwise PAT downgrades it to UC- (linux memtype.c:369-376; mtrr/generic.c:518-527). Warn at probe if the downgrade happens.
  - Fix the comment at nvidia.rs:2994-2999.
- **Files:** driver/nvgpu_wire.h, protocol/src/messages.rs, device/src/nvidia.rs, device/src/shm.rs, driver/nvgpu_main.c, driver/nvgpu_drm.c, ARCHITECTURE.md, README.md.

**M-2. Read-only host mappings are placed writable, so a guest write gives KVM_RUN -EFAULT and kills the VM**
- **Ours:** writable=true always (device/src/nvidia.rs:1541, 2932), and the guest keeps VM_MAYWRITE (nvgpu_main.c:1154, 1177).
- **Reference:** nvidia clears VM_WRITE and VM_MAYWRITE when the mmap context lacks WRITEABLE (nv-mmap.c:756-761). That happens for RUSD (NV00DE, gpu_user_shared_data.c:417-418) and for the PTIMER and MC BAR0 windows for non-admins (osapi.c:2203-2226). KVM then hits kvm_main.c:2928-3024 and mmu.c:3547-3567.
- **Fix:**
  - Backend: probe writability by mmapping PROT_READ and then trying mprotect RW (EACCES means read-only). Store the result per placement and pass it to `place()`. Report it as an MmapResp flag bit.
  - Guest: `vm_flags_clear(vma, VM_WRITE|VM_MAYWRITE)` and recompute the pgprot.
- **Files:** device/src/nvidia.rs, device/bin/vhost-user-nvgpu.rs, protocol/src/messages.rs, driver/nvgpu_wire.h, driver/nvgpu_main.c.

**M-3. RM_UNMAP_MEMORY frees and reuses a window extent while a guest VMA may still map it (a cross-process leak inside the guest)**
- **Ours:** RM placements return mapping_id 0 (device/src/nvidia.rs:1458, 1590-1596). release_extent frees immediately (nvidia.rs:1110-1125, 3088; shm.rs:328-340). There is no guest PTE zap.
- **Reference:** native sysmem pins pages until the VMA closes (nv-mmap.c:443).
- **Fix:**
  - Give RM placements a real live-map id with refs.
  - When RM_UNMAP_MEMORY arrives while refs>0, mark the extent `rm_unmapped` and defer release_extent until the last MUNMAP. Release exactly once in release_all.
  - Add a regression test: MAP→MMAP→UNMAP→MAP must not reuse the offset.
- **Files:** device/src/nvidia.rs, device/src/mmap.rs, device/src/shm.rs.

**M-4. GET_DEV_INFO capability bits (supports_alloc, supports_sync_fd, supports_semsurf) are overridden rather than ANDed with the host's answer, and FENCE_SUPPORTED lies**
- **Ours:**
  - `info[3]=nvgpu_claim_alloc` (default 1), and `info[7]=info[8]=1` whenever fences are on (driver/nvgpu_drm.c:269-286; nvgpu_main.c:146). The comment claims these are "the host's truth", but the code never uses the host's words.
  - The backend fallback invents 1/6/2/1/1/1 (device/src/nvidia.rs:778-786).
- **Reference:** both sets of bits are 0 when pDevice==NULL (modeset=0) or when semsurf_stride==0 (nvidia-drm-drv.c:1095-1116). With the bits forced on, 0x54 returns -EOPNOTSUPP (nvidia-drm-fence.c:1316-1318) and vkCreateDevice fails, which our own comment at nvgpu_drm.c:250-256 records. Verdicts: low (layout lens), medium (sync lens). Ranked medium because every Vulkan device creation fails on a modeset=0 host.
- **Fix:**
  - `info[3] = nvgpu_claim_alloc && host[3]`; zero words 4-6 when that is 0.
  - With fences on: `info[7] = info[8] = host[8]`.
  - DMABUF_SUPPORTED: `host[3] ? 0 : -EINVAL`.
  - The backend fallback reports all capability words as 0, or skips the node.
  - Rewrite the comments at nvgpu_drm.c:168-169, 273-282, 331-335.
- **Files:** driver/nvgpu_drm.c, device/src/nvidia.rs.

**M-5. Every GEM proxy answers NV_GEM_OBJECT_NVKMS to IDENTIFY, including foreign host dma-bufs imported in export mode**
- **Ours:** driver/nvgpu_drm.c:901 (the only write to obj_type) and 1143-1164; session.rs:458-468.
- **Reference:** nvidia-drm-gem.c:310-345; nvidia-drm-gem-dma-buf.c:134-163. EXPORT_NVKMS_MEMORY refuses a dma-buf object (nvidia-drm-gem-nvkms-memory.c:595-603). A host iGPU, udmabuf or v4l2 buffer therefore fails to import in the guest compositor.
- **Fix:**
  - Backend: run IDENTIFY (0xc008644e) after DmabufImport and return `[gem, size, type]`. Close the GEM and fail on UNKNOWN.
  - Guest: accept 2 or 3 result words (default NVKMS). Add an `obj_type` parameter to `nvgpu_gem_proxy_new`.
  - Do not refuse mmap for the DMABUF type (nvidia-drm-gem-dma-buf.c:55-130).
  - Make the i2_e2e Fake answer IDENTIFY per object.
- **Files:** device/src/session.rs, device/src/hostfd.rs (shared IDENTIFY helper from xfer.rs:1480-1492), driver/nvgpu_wl.c, driver/nvgpu_drm.c, i2_e2e.rs.

**M-6. fd fields of fd-carrying RM escapes (MAP_MEMORY, ALLOC_MEMORY, ALLOC_OS_EVENT, REGISTER_FD, NV0005 `data`) come back to userspace holding a backend handle. This breaks envyhooks and ABI fidelity.**
- **Ours:** driver/nvgpu_main.c:853, 872-874, 905-909, 776; device/src/nvidia.rs:2697-2699, 2863-2865, 2384-2386. The comments at nvgpu_main.c:818-821 and nvidia.rs:2697-2698 are wrong.
- **Reference:** RM never writes pApi->fd (escape.c:393-428, 584-624). envyhooks keys mmaps by the returned fd (ehk object.rs:1286-1288, 1778-1782) and `unwrap()`s at 1202, which aborts the traced process.
- **Fix:** in the guest only, memcpy `guest_fd` back into the reply payload before copy_to_user (every case, including RM-status failure). For NV0005, lift `event_fd` and restore it at `NVGPU_NV0005_DATA_OFFSET`. Fix both comments. Test with envyhooks as in §5.3.
- **Files:** driver/nvgpu_main.c, device/src/nvidia.rs (comments only).

**M-7. nvidia-drm GET_CRTC_CRC32 and _V2 are admitted on the render class and reach every host CRTC (a content and timing side channel, plus two frames of nvkms_lock per call)**
- **Ours:** gen/schema/nvidia_drm.py:44-53, 99; test at device/src/xfer.rs:2929-2931.
- **Reference:** the ioctls are DRM_RENDER_ALLOW (nvidia-drm-drv.c:1860-1865). For a file without a master, drm_crtc_find is not lease-filtered (linux drm_lease.c:109-121; drm_mode_object.c:151-155). Each call does two sync core updates (nvkms-evo.c:9301-9318).
- **Fix:**
  - Move both entries to KMS_IOCTLS only.
  - Allow them only in `--kms-card` mode, or on a real lessee handle whose lease includes the CRTC (flag the fd at CREATE_LEASE or lease_fd receipt, or check GET_LEASE). Lessor and lease-device fds see every CRTC.
  - Optionally rate-limit to one call per vblank per file.
  - Update the xfer test.
- **Files:** gen/schema/nvidia_drm.py, device/src/kms.rs or device/src/policy.rs, device/src/xfer.rs (test).

**M-8. FLIP_OCCURRED broadcast: a guest flip with `completionNotifier.awaken=1` sends events to nvidia-drm's KAPI open, which WARNs on an empty flip_list (a host dmesg flood; a panic under panic_on_warn)**
- **Ours:** awaken passes through unchanged, and there is no offset for it in the JSON or schema. DESIGN §9 lists this only as a limit.
- **Reference:** nvkms-evo3.c:3686-3695; nvkms-evo.c:4991-5055; nvkms.c:6580-6625; nvidia-drm-crtc.h:334-358. A MODESET grantee gets no flipPermissions, so it never receives the event itself (nvkms.c:3488-3498, 6605-6609).
- **Fix:**
  - Export `awaken` for the FLIP and SET_MODE layers from gen/schema/nvkms.py and regenerate.
  - Outside `--kms-card`, clear the byte at `layer.at(l)+awaken` in every FLIP head element (pFlipHead fetched mutably) and every SET_MODE disp/head/layer. Log once, rate-limited.
  - Add tests. Update DESIGN §9.
- **Files:** gen/schema/nvkms.py, gen/nvkms/*.json, device/src/nvkms.rs, DESIGN.md.

**M-9. NVKMS permissions granted through a lease survive a lease end that does not go through REVOKE_LEASE (lessor close or master drop with fbdev=1). Later, the lessee close blanks a connector the restarted host compositor may have taken back.**
- **Ours:** end_dead_leases only forgets backend records (device/src/kms.rs:445-465; nvkms.rs:295-297, 786-795). FLIP and SET_MODE are not gated on those records. The comment at kms.rs:367-372 is wrong.
- **Reference:** linux drm_auth.c:351-357 bypasses nvidia-drm's wrapper (nvidia-drm-drv.c:1750-1774). master_drop revokes only the lessor's own file (drv.c:1038-1043, 1500). The postclose disable is at drv.c:1497-1523, 1588-1600.
- **Fix:**
  1. Gate FLIP (this is H-3) and SET_MODE heads on the backend Perms records, so that lease_ended actually takes effect.
  2. When GET_LEASE returns zero objects, close the host lessee fd right away and leave a dead stub that returns ENODEV. On -EACCES, only gate.
  3. Poll granting leases on the hotplug thread and at host Wayland EOF.
  4. Fix the comment.
- **Files:** device/src/kms.rs, device/src/nvkms.rs, device/src/session.rs.

**M-10. No cap on semsurf fence contexts. Each 0x54 creates a host kthread, a timer, an NVKMS dup and a kernel mapping.**
- **Reference:** nvidia-drm-fence.c:1233-1310, 1283-1287; nvidia-drm-os-interface.c:166-176; nvkms-kapi-sync.c:240-306.
- **Fix:**
  - Track the set of `(render_handle, gem)` pairs.
  - Before the host ioctl, refuse with ENOSPC at 16 per file or 256 per session.
  - Remove entries on GEM_CLOSE, on failed gem_out or import closes, on host-file close and on reset.
  - Add tests.
- **Files:** device/src/xfer.rs, device/src/session.rs, device/src/handle_table.rs, device/src/fence.rs (tests).

### Low

**L-1. SEMSURF_FENCE_ATTACH (0x57) is not mirrored into the guest resv.** Guest implicit-sync consumers (guest Hyprland in compositor-VM mode, via EXPORT_SYNC_FILE/poll) see an empty resv. The same applies to export-mode dmabufs.
- **Fix:**
  - After a successful 0x57, issue host 0x55 with the same ctx, value and timeout. Turn the result into a guest dma_fence (refactor `nvgpu_host_fence_fd`) and `dma_resv_add_fence` it with READ if shared, else WRITE. Gate this on CAP_KMS_CARD.
  - Export mode: on a commit with no acquire point, host EXPORT_SYNC_FILE, then guest IMPORT_SYNC_FILE.
- **Files:** driver/nvgpu_fence.c, driver/nvgpu_wl.c, device/src/wl/.

**L-2. Unwrapping an unsignalled fence from outside the guest driver (0x56, IN_FENCE_FD, syncobj import, or a merge of more than 64 fences) blocks the caller, where native never blocks.** Found by both the sync and submission lenses.
- **Fix:**
  - 0x56: stay asynchronous. Take a callback on the foreign fence, then a work item runs SIGNALED_SYNC_FILE and forwards 0x56 with FD_CONSUME. Cancel on release.
  - IN_FENCE_FD: do not wait on TEST_ONLY.
  - Merges: merge in chunks rather than flagging "foreign" at NVGPU_UNWRAP_MAX.
  - Do not use a 0x55 placeholder: it has a 5 s cap and is force-signalled.
  - Fix the comment at nvgpu_fence.c:447-450. Document the remaining blocking in DESIGN §9.
- **Files:** driver/nvgpu_fence.c, driver/nvgpu_kms.c, DESIGN.md.

**L-3. GPU/CPU time correlation (0x20800406) is not rebased.**
- glcore requests OSTIME (UTC µs) at 0xa5d94d and 0xa6c54c. TSC and PLATFORM_API (MONOTONIC_RAW) would be badly off (subdevice_ctrl_timer_kernel.c:354-455).
- **Fix:**
  - Extend TIME_SYNC to carry host REALTIME and MONOTONIC_RAW as well.
  - Rewrite `cpuTime` per clock ID in the guest reply path: OSTIME uses the realtime offset in µs, PLATFORM_API uses the raw offset.
  - For TSC, return NV_ERR_NOT_SUPPORTED, or forward as PLATFORM_API and convert.
  - Never apply the monotonic offset to OSTIME.
- **Files:** device/src/session.rs, driver/nvgpu_wire.h, driver/nvgpu_xfer.c, driver/nvgpu_rm_intercepts.h / driver/nvgpu_main.c.

**L-4. A proxy fence's signal timestamp is the guest delivery time (about +0.35 ms).** The ICD reads it (glcore 0xa13870, slot 0x30 of the sync-fd ops).
- **Fix:**
  - Backend: FILE_INFO with the per-fence array (re-check the struct offsets against the uapi) to get max timestamp_ns. Send a 16-byte EV_FENCE.
  - Guest: `dma_fence_signal_timestamp(nvgpu_host_to_guest_ns(..))`, clamped to now.
  - First confirm with a uprobe that a caller actually reads the +8 result.
- **Files:** device/src/hostfd.rs, device/src/pump.rs, driver/nvgpu_wire.h, driver/nvgpu_fence.c.

**L-5. FENCE_SUPPORTED (0x44) returns 0, which means "supported", but 0x45 and 0x46 are not forwarded.** The 610 Xorg DDX (nvidia_drv.so, 0x42901 / 0x429c5 / 0x42a85) enables PRIME fencing from this answer and then silently fails.
- **Fix:** return -EINVAL. Fix the comments at driver/nvgpu_drm.c:168, 331-335 and research/fences.md:401.
- **Files:** driver/nvgpu_drm.c.

**L-6. UPDATE_DEVICE_MAPPING_INFO zeroes pOld/pNew in the reply, where RM leaves them as the caller passed them** (escape.c:857-876; nv.c:2834).
- **Fix:** `param_buf[16..32].copy_from_slice(&param_in[16..32])` in place of the zeroing at nvidia.rs:2770-2772. Add a test.
- **Files:** device/src/nvidia.rs.

**L-7. The "no crossings per frame" claim is only measured for loops that never present.**
- **Fix:**
  - Measure per-present crossings for Wayland-proxy, KMS-lease and VK_KHR_display presents (§5.10). Publish them in BENCHMARKS.md.
  - Reword README.md:44-47, 330, 333-335 and ARCHITECTURE.md:48-51.
  - Update the stale README.md:311 ("scanout out of scope").
- **Files:** BENCHMARKS.md, README.md, ARCHITECTURE.md.

**L-8. Stale proxy-size comment.** driver/nvgpu_drm.c:926-928 says the size is used to validate FB dimensions. The guest has no DRIVER_MODESET (nvgpu_drm.c:1654). Size actually bounds mmap, window placement and the dma-buf size.
- **Fix:** rewrite the comment. Optionally note the one-page under-report when there is no size field (1343-1347).
- **Files:** driver/nvgpu_drm.c.

**L-9. useSyncpt is refused even when `syncObjects.specified`=0, which is stricter than the host** (nvkms-hw-flip.c:714-718; nvkms-modeset.c:246-247). No known trigger.
- **Fix:** export `sync_specified` (+96). Refuse only when both specified and useSyncpt are set. Optionally skip disps and heads outside the SET_MODE request masks. Update the test at nvkms.rs:1322-1343.
- **Files:** gen/schema/nvkms.py, gen/nvkms/*.json, device/src/nvkms.rs.

---

## 4. Rejected claims

| Claim | Why rejected |
|---|---|
| Backend GET_DEV_INFO fallback hard-codes Turing constants | The fallback is unreachable on a live node (dev->primary is always set, linux drm_drv.c:769-783). The guest overwrites the capability words. gpu_id=0 means the ICD never associates the node. Optional: skip the node instead (folded into M-4). |
| Backend refuses ADDFB/ADDFB2 of non-NVKMS objects | Deliberate and needed. A NULL pMemory would NULL-dereference in the host IsVidmem (nvkms-kapi.c:2235-2238). A valid DMABUF FB has pSurface=NULL, and every plane rejects it (nvidia-drm-crtc.c:316-319, 1139-1142). The guard only moves -EINVAL to an earlier point. It is already documented (drm_kms.py:163-164). |
| Guest PAT memtype conflicts once placement types differ | Every guest mapping is WC today, so no conflict is reachable. The cited PAT rule was also read backwards: WB over WC is downgraded, not refused. Keep in mind as a design note for M-1. |
| 0x56 returns errors where native always returns 0 | On those paths native silently queues no wait (a GPU stall). An explicit error is more diagnosable. The ICD already has to handle non-zero returns (-EOPNOTSUPP, drm_ioctl errors). |
| REGISTER_SURFACE dma-buf planes unusable on a dGPU | Host NVKMS sees exactly the descriptor a native client would pass, so the outcome is identical to bare metal. Scanout uses the nvidiactl-export path (research/nvkms.md:783-786). |
| Ungated CHECK_LUT_NOTIFIER, NOTIFY_VBLANK, ENABLE_VBLANK_SEM_CONTROL leak timing | The same host vblank and scanline data is already available through unfiltered non-privileged RM NV0073 controls (g_disp_objs_nvoc.c:3471-3490). The lock hold is bounded and smaller than IDLE_BASE_CHANNEL or SET_MODE. The design deliberately leaves read-side commands ungated. |
| DESIGN §9 "SET_MODE reply is 187 KB" is misleading | The transport reply really is the whole 186,784 B INOUT block (nvgpu_i2.c:238-243, 711, 1059-1061). RANGE only limits copy_to_user. The request side is bounded by max_req and fails closed with -E2BIG. |

---

## 5. On-device verification plan

Run the steps in order. Each step lists its commands, then its PASS/FAIL criteria. Step 0 is the precondition for everything else.

### 5.0 Preconditions and inventory

1. **Driver version.** Record `cat /proc/driver/nvidia/version` on the host and `nvidia-smi` / guest `/proc/driver/nvidia/version` in the guest. The dev box currently reports 595.99.02, but the envyhooks bindgen and our reference tree are 610.57.04.
   - PASS: host kmod, host userspace and guest userspace are all the same version, preferably 610.57.04.
   - If you must stay on 595, rebuild envyhooks against the matching open-gpu-kernel-modules tag, then check that the envyhooks-logged `paramsSize` of AMPERE_CHANNEL_GPFIFO_A equals `size_of::<NV_CHANNEL_ALLOC_PARAMS>()` (368 on 610, see driver/gen/nvgpu_rmalloc_classes.h:99).
2. **KVM regime.**
   ```
   grep -m1 vendor_id /proc/cpuinfo; grep -o -w 'self_snoop\|ss' /proc/cpuinfo | sort -u
   strace -f -e trace=ioctl -e signal=none -p $(pidof nesbox) 2>&1 | grep -i 'KVM_ENABLE_CAP'   # at VM start
   ```
   Record AMD (NPT, guest PAT honoured) vs Intel (default IGNORE_GUEST_PAT). H-4 and M-1's Intel-only failures need an Intel host.
3. **nvidia-drm parameters.** `cat /sys/module/nvidia_drm/parameters/{modeset,fbdev}`. The expected defaults are Y/Y. M-4 and M-9 need non-default and default settings respectively.
4. **Tools.** Build or install in both host and guest: `drm_info`, `modetest` (libdrm tests), `kmscube`, `vkcube`, `vulkaninfo`, `eglgears_wayland`, `bpftrace`, `strace`. On this box only `vkcube` and `eglgears_wayland` are on PATH; get the rest via `nix shell nixpkgs#drm_info nixpkgs#libdrm nixpkgs#kmscube nixpkgs#bpftrace`.
5. **Guest mapping and PAT state.**
   ```
   dmesg | grep 'x86/PAT'; cat /proc/mtrr; cat /sys/kernel/debug/x86/pat_memtype_list | grep -i <window-BAR-range>
   ```
   Today the window range should show `write-combining`.


### 5.0a Host-safety fixes (C-1, H-1, H-2, H-3, M-7, M-8, M-10)

Verify these through the backend unit tests added with each fix (§3). Do not exercise the unfixed paths on a live host. After the fixes land, the only on-device check is a regression check: run the normal workloads in §5.2-5.9.
- PASS: no new EPERM, EINVAL or ENOSPC refusals appear in the backend log for NVIDIA's own userspace.
- PASS: host `dmesg` stays clean (no `nv_drm_crtc_dequeue_flip` WARN, no semaphore-surface notification-handle errors).

### 5.0b Layout, modifiers and GET_DEV_INFO (L1-L11, M-4, H-5)

1. **Host.** `drm_info -j /dev/dri/cardN > host.json`. Extract IN_FORMATS for each primary plane.
2. **Guest.**
   - Run `vulkaninfo --json`. Read the VkDrmFormatModifierPropertiesEXT for B8G8R8A8_UNORM.
   - Write a small GBM test: `gbm_bo_create_with_modifiers2(XRGB8888, host list)`, then `gbm_bo_get_modifier`.
   - PASS: every guest modifier is in the host IN_FORMATS list, with vendor 0x
### 5.1 Host-kernel safety findings (C-1, H-1, H-2, M-10): validate in the backend, not on a live host

These four are guest-reachable host-kernel hazards. Their fixes belong in the backend policy layer, and the right place to prove them is the backend's own `cargo test`, against the mock host ioctl harness (the same one used by the existing tests in device/src/nvidia.rs and device/src/fence.rs). Ship the fix and its unit test together; do not point an unfixed backend at a host you care about.

1. **C-1 (0x54 index bound).** Add tests in the fence policy module:
   - index that overflows `index*stride` is refused with EINVAL;
   - `(index+1)*stride == size + 1` is refused;
   - the last valid index is accepted;
   - on a `max_off != 0` layout, an index whose max-submitted read overruns is refused.
   - PASS: all four hold, and the accepted case forwards while the refused cases never issue the host ioctl.
2. **H-1 (0x54 client ownership).** Tests: a foreign hClient is refused with EPERM; a client freed and reallocated is refused; the session's own client is allowed.
3. **H-2 (waiter/event-buffer fd translation).** Tests for 0xda0003 (off 24), 0xda0005 (off 16) and RM_ALLOC 0x90cd (off 40): a live handle is translated and restored, zero passes through, and an unknown handle is rejected before any host call.
4. **M-10 (semsurf context cap).** Tests: N+1 creates on one render handle → the last gets ENOSPC; GEM_CLOSE of one lets a new create succeed; closing the render handle returns the count to 0.

Only after the fixes land, an end-to-end confirmation may be run on a **sacrificial** host (one you are willing to reboot): run the guest workloads in §5.4 and confirm host `dmesg` shows none of the nvidia-drm/NVKMS warnings the fixes target, and that the backend logs show the refusals for a deliberately malformed guest request. Treat any host oops as a FAIL of the fix.

### 5.2 nv_push_dump decode setup (for §5.4 and §5.5)

Build the decoder once:
```
git clone https://gitlab.freedesktop.org/mesa/mesa && cd mesa
meson setup b -Dvulkan-drivers=nouveau -Dgallium-drivers= -Dtools=nouveau \
  -Dplatforms= -Dglx=disabled -Degl=disabled -Dllvm=disabled
ninja -C b src/nouveau/headers/nv_push_dump
```
Usage: `nv_push_dump file.bin AMPERE_B` (take ARCH from the 3D class_id in the RM log; c797 is AMPERE_B on GA10x). Prepend the channel-init buffer that carries the SET_OBJECT methods so subchannel bindings decode correctly: `cat init.bin f.bin > g.bin; nv_push_dump g.bin AMPERE_B`.
- PASS: a captured pushbuffer decodes into `\tmthd` lines with sensible (subch, mthd) pairs and no "not a NOP entry" spam beyond the known envyhooks limitations (wrap, SYNC_WAIT, subroutine entries).

### 5.3 envyhooks build and the fd round-trip fix (M-6)

Build (outside this repository):
```
cd envyhooks                      # a checkout of envyhooks
rmdir externals/open-gpu-kernel-modules
ln -s /path/to/open-gpu-kernel-modules externals/open-gpu-kernel-modules   # the host's release
LIBCLANG_PATH=$(nix eval --raw nixpkgs#libclang.lib)/lib cargo build --release
```
Install the result as `$EHKS=/opt/ehks/libenvyhooks.so` on host and guest. If the guest glibc is older than the build host's, build inside the guest.

Pre-flight, both sides:
```
EHKS_LOG_RM_IOCTL=1 LD_PRELOAD=$EHKS vulkaninfo --summary 2>&1 >/dev/null \
 | grep -E 'IOCTL NV_ESC_RM_(MAP|ALLOC)_MEMORY' | grep -o 'fd: -\?[0-9]*' \
 | paste - - | awk '$2!=$4'
```
- FAIL (pre-fix, guest): the guest run aborts inside object.rs:1202, or `EHKS_LOG_RM_IOCTL` shows an `AFTER` fd that differs from its `BEFORE` fd, or (RUST_LOG=debug) there is no "new mmap_address" line for the USERD handle.
- PASS (post-fix): the guest run completes, and every MAP/ALLOC_MEMORY/ALLOC_OS_EVENT/NV0005 line shows the same fd after the ioctl as before.

### 5.4 Bare-metal-vs-guest differential (submission fidelity: U2, U3, M-6, and the "no crossings" claim L-7)

Preconditions: same driver version everywhere (§5.0); the M-6 fix applied; no other GPU clients or VMs; envyhooks (§5.3) and nv_push_dump (§5.2) built.

Workloads, identical binaries, fixed frame counts, run once at 1x and once at 10x:
- **W1 (offscreen):** `nesprobe --device 0 --cost 400 --seconds 10 --warmup 0`.
- **W2 (Wayland present):** `vkcube --wsi wayland --c 300` (bare metal: a client of host Hyprland; guest: the same Hyprland through nvgpu-wl-guest).
- **W3 (display present):** `vkcube --wsi display --c 300` (bare metal: a bare VT; guest: a leased connector).

Capture, per side (SIDE=bare|guest):
```
export LD_PRELOAD=$EHKS EHKS_LOG_RM_IOCTL=1 RUST_LOG=warn __GL_SHADER_DISK_CACHE=0
export EHKS_PUSHBUF_DUMP_DIR=/tmp/ehks/$SIDE/$W; rm -rf $EHKS_PUSHBUF_DUMP_DIR
<cmd> 2>/tmp/ehks/$SIDE/$W.rm.log
```
Run a separate no-preload pass under `strace -f -e trace=ioctl,mmap,munmap,poll,ppoll,sendmsg,write -o $W.strace` (ptrace and envyhooks both use SIGTRAP; do not combine them). Save guest dmesg and the backend log.

**5.4a RM sequence diff.** Normalise each log to `AFTER IOCTL` lines only; rename each `h<Name>:` value to `H<n>` by first appearance; replace long hex with `PTR` and `fd: -?\d+` with `FD`; drop TIMER/CORRELATION values. Then `diff -u bare.seq guest.seq`.
- Expected differences (PASS): RM-assigned client handles, CPU pointers, pLinearAddress (window offset in the guest), UPDATE_DEVICE_MAPPING_INFO addresses (must be the caller's own once L-6 is fixed; today 0), SYSTEM_GET_BUILD_VERSION strings, time values.
- FAIL: any nonzero status on one side only; any class, control command or escape added/missing/reordered; different MAP_MEMORY length or caching flags; different MAP_MEMORY_DMA dmaOffset; guest-only envyhooks warnings ("Unknown memory with handle", "GPFIFO entries not found", "No CPU mapping for USERD").

**5.4b Pushbuffer diff.**
```
for s in bare guest; do (cd /tmp/ehks/$s/$W && sha256sum pushbuf_*.bin mme_*.bin \
  | cut -d' ' -f1 | sort -u) > $s.$W.h; done
comm -3 bare.$W.h guest.$W.h
```
Decode each unmatched file with nv_push_dump (§5.2) and pair by the (subch, mthd) signature.
- PASS: paired buffers differ only in address-like fields, and only where 5.4a showed VA divergence; semaphore payloads, query counters and timestamps may differ.
- FAIL: a signature on one side only, especially extra host-class (c56f) SEM_EXECUTE/NON_STALL_INTERRUPT/WFI in the guest, or different copy-engine (c6b5/c7b5) blit methods in W2/W3 (a modifier-negotiation difference; cross-check with WAYLAND_DEBUG=1).

**5.4c Crossings per frame (L-7).** RM ioctls per frame = (10x `BEFORE IOCTL` count − 1x count) / (9 × 1x frames). Take the backend message tally over the same delta for crossings per frame.
- PASS: W1 is ~0 on both sides and equal. W2/W3 give a small fixed per-present count; record it and publish it (do not report W1's ~0 as the per-present figure).

### 5.5 GPU-VA identity (U2)

From the 5.4a logs, compare the ordered list of `NV_ESC_RM_MAP_MEMORY_DMA` dmaOffset/length values between bare metal and guest.
- PASS: identical lists, and every address inside paired pushbuffers matches (5.4b).
- FAIL: any divergence; trace it back to the first differing earlier RM_ALLOC/RM_CONTROL (likely vCPU count, NUMA/sysparams, or a differing capability reply).

### 5.6 Layout / modifier / direct-scanout (Wayland proxy)

Host: Hyprland with `render:direct_scanout = 2` (auto) — in the Lua config, `hl.config({ render = { direct_scanout = 2 } })` — and `debug:enable_stdout_logs = true` (or run with `HYPRLAND_TRACE=1` for the TRACE lines).

Guest: run fullscreen through nvgpu-wl-guest with `WAYLAND_DEBUG=1`:
```
WAYLAND_DEBUG=1 vkcube --wsi wayland   # and eglgears_wayland
```
1. **Modifier check.** In the WAYLAND_DEBUG trace, `zwp_linux_buffer_params_v1.add` should show `modifier_hi/lo` with vendor 0x03, bit 4 set, k=generic_page_kind, g=2, s=1, c=0.
2. **vulkaninfo vs drm_info.** In the guest, `vulkaninfo` `VkDrmFormatModifierPropertiesEXT` for B8G8R8A8/XRGB8888 and `gbm_bo_get_modifier` must equal host `drm_info` IN_FORMATS for the primary plane.
3. **Direct scanout confirmation.** While the client is fullscreen, the host Hyprland log shows `Entered a direct scanout to <ptr>: "<title>"` (Monitor.cpp/OutputCommitCoordinator.cpp:187), and leaving fullscreen logs `Left a direct scanout.` Cross-check `/sys/kernel/debug/dri/<N>/state` shows the primary plane's fb carrying that modifier.
- PASS: the modifier matches host IN_FORMATS, and the host log reports direct scanout active with no `Cannot create FB from compressible surface` / `Invalid format modifier` lines in dmesg.
- FAIL: any of those mismatch, or the host falls back to composition (no "Entered a direct scanout" line while fullscreen).

### 5.7 Lease and VK_KHR_display modes (kmscube / modetest / drm_info)

Host: mark a monitor leasable in the Hyprland Lua config: `hl.monitor({ output = "<name>", leasable = true })` (MonitorRule leasable → wp_drm_lease_device_v1, MonitorRuleManager.cpp:147).

Guest:
1. `drm_info` on the guest card lists the leased connector; `modetest -c` shows it.
2. `kmscube -D /dev/dri/card<N>` or `vkcube --wsi display` renders to the leased output.
3. Explicit KMS-lease flip via `modetest -M nvidia -s <conn>@<crtc>:<mode>` should light the output.
- PASS: the leased connector appears and scans out; blocking commits complete within 3 s; nonblocking commits return -EBUSY only while a flip is pending.
- FAIL: connector missing, ADDFB2 -EINVAL where native succeeds, or a commit that never completes.

### 5.8 Hyprland leasable-monitor round trip

With a leasable monitor offered by host Hyprland, and a guest compositor (guest Hyprland in compositor-VM mode, `--kms-card`):
- The guest should enumerate the leased output through `wp_drm_lease_device_v1`, acquire it, and drive it.
- PASS: `wl-drm`/lease events reach the guest; the guest compositor lights the leased output; on guest lease close, the host reclaims it and re-enables its own use of the connector without a stuck/dark output (this also exercises M-9 — confirm no blank on the host after the restarted compositor takes the connector back).
- FAIL: the host output stays dark after the guest closes the lease, or the guest can still FLIP/SET_MODE the head after the lease ends (M-9 not fixed).

### 5.9 Explicit sync (semsurf, sync_fd)

1. **vkCreateDevice with semsurf.** In the guest, `vulkaninfo` then `vkcube --wsi wayland`.
   - PASS: device creation succeeds; a WAYLAND_DEBUG trace shows wp_linux_drm_syncobj timelines carried to the host (commit 1dab9c5).
   - On a `nvidia_drm.modeset=0` host (for M-4): GET_DEV_INFO should report supports_semsurf=0 after the fix, and vkCreateDevice should succeed by taking the non-semsurf path. Pre-fix it fails with VK_ERROR_INITIALIZATION_FAILED after 0x54 → EOPNOTSUPP.
2. **EGL_ANDROID_native_fence_sync.** Run an EGL client using `eglCreateSyncKHR(EGL_SYNC_NATIVE_FENCE_ANDROID)` and `eglDupNativeFenceFDANDROID`, then poll/merge the fd.
   - PASS: the exported sync_fd signals; `strace` shows SYNC_IOC_FILE_INFO and no unexpected CPU `poll()` waits where the host uses 0x56 (this also checks L-2 — a foreign-fence path should not block the submitting thread once fixed).
3. **Timeline import.** `vkWaitSemaphores` (timeline, 1 s) on a value the GPU signals ~100 ms later.
   - PASS: the wait returns on signal, not on the 5 s force-signal, and the guest proxy FILE_INFO status is 1 (not -110). Log EV_FENCE statuses at device/src/pump.rs:719-734.

### 5.10 Per-present crossings (publishes L-7 numbers)

For W2 and W3 (§5.4), snapshot the backend served-message count before and after a 30 s run (8 s warmup discarded), and divide by frames presented. Split by message type (WL SEND, SEMSURF/syncobj, ATOMIC/NVKMS FLIP, EV_DRM). Cross-check with `strace -f -c -e trace=ioctl,write,sendmsg` on `/dev/dri/*`, `/dev/nvidia-modeset` and `/dev/nvgpu-wl`.
- PASS: the per-present crossing count is stable and matches the strace-derived per-frame syscall count; results are added to BENCHMARKS.md and the README wording is corrected (L-7).

### 5.11 Caching / coherency (H-4, M-1) — Intel host required for the failure

On an Intel host with self-snoop, default VMM (quirk on):
1. Log the guest COHERENCY distribution the backend sees on NV_ESC_RM_ALLOC (NV01_MEMORY_SYSTEM), VID_HEAP_CONTROL and NVOS02, while running vkcube, a CUDA kernel and an NVKMS flip. Any value other than CACHED/WRITE_BACK confirms the exposure surface.
2. Read a GPU-written value through a HOST_VISIBLE|HOST_COHERENT, non-HOST_CACHED allocation (submit, wait, map, read).
   - FAIL (pre-fix): stale value or hang.
   - PASS (post-fix, or with KVM_CAP_DISABLE_QUIRKS2(IGNORE_GUEST_PAT)): correct value.
3. Doorbell latency (M-1): tight `vkQueueSubmit` empty CB + `vkWaitForFences`, 1e5 iterations, p50/p99/max, WC doorbell vs a build mapping the usermode page UC. A heavy tail only on WC confirms M-1. Confirm the effective type with `/sys/kernel/debug/x86/pat_memtype_list`.
On AMD, only the M-1 performance side shows (slow WB-intended reads through WC); the H-4 correctness failure does not occur.

### 5.12 Read-only mapping VM-kill (M-2)

In the guest, open `/dev/nvidiactl`, allocate NV00DE (RUSD), RM_MAP_MEMORY, `mmap(PROT_READ|PROT_WRITE)`, write one byte.
- FAIL (pre-fix): KVM_RUN returns -EFAULT and the VMM exits ("Bad address" in the VMM/host log).
- PASS (post-fix): the write faults inside the guest process (SIGSEGV) or `mprotect(PROT_WRITE)` returns EACCES; the VM keeps running.
