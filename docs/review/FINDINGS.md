# Verified findings from security + adversarial review (34 real, 2 rejected)

> **A review record, not current documentation.** This is the security and
> adversarial review of branch `display-passthrough` at `fbfa3fb`
> (2026-09-25), kept because [`SECURITY.md`](../../SECURITY.md) §8 and
> [`TESTING.md`](../../TESTING.md) cite its finding ids. File and line
> references are to that commit, and "INVENTORY" is the attack-surface
> inventory the review worked from, which is not shipped (SECURITY.md §3-§7
> is its current form). The status of each finding is in SECURITY.md §8, not
> here.

## S-1. [critical] (sec-gpuside) RM_CONTROL embedded pointers the guest driver's table does not list reach host RM with their raw guest values, and RM uses them against the backend's own address space
- Area: RM escapes (NV_ESC_RM_CONTROL), device/src/nvidia.rs dispatch_nested + driver/nvgpu_main.c nvgpu_ioctl_rm_control
- Evidence: Guest side: driver/nvgpu_main.c:501-531 carries a second-level pointer only for commands in gen/nvgpu_v1v2_rewrites.h (20 entries) or nvgpu_deep_only_table (1 entry, nvgpu_main.c:411-424). For every other command, and whenever count*8 > NVGPU_DEEP_MAX (nvgpu_main.c:84, 527-531), the caller's NvP64 value is forwarded unchanged. The backend replaces only the top-level pointer and at most one deep pointer at a guest-declared offset (device/src/nvidia.rs:2296-2326). Every other NvP64 in the nested block goes to the host as is. The deep buffer is a heap Vec sized max(guest bytes, 64 KiB) (nvidia.rs:2313-2314, DEEP_BUF_FLOOR at :33), and its length comes from a guest field. Host side: RM resolves embedded pointers of user-level controls by copying from and to the calling process, which is the backend (src/nvidia/src/kernel/rmapi/embedded_param_copy.c, e.g. :292-306 FIFO_GET_CHANNELLIST with two pointers, :712-722). RM allows up to 1 MiB per copy (inc/kernel/rmapi/param_copy.h:63, param_copy.c:85). Such commands include non-privileged ones: NV0080_CTRL_CMD_FIFO_GET_CHANNELLIST has flags 0x109, which includes RMCTRL_FLAGS_NON_PRIVILEGED (generated/g_device_nvoc.c:977-980; control.h:205). NVK_VERIFICATION does not cover this, and INVENTORY §1.1 records RM_CONTROL only as 'counted, not allowlisted'.
- Scenario: An unprivileged process in the guest opens /dev/nvidiactl (0666) and issues an RM_CONTROL whose parameter block holds a pointer the guest driver does not list, or a listed one with a large count. The guest kernel forwards the pointer value unchanged. Host RM then reads or writes at that address inside the backend process. The damage is not limited to that one call: the backend is corrupted or disclosed, and the backend runs unsandboxed with the desktop user's privileges (INVENTORY Part 2). The listed commands have a related defect: the backend's deep buffer size is decided by a guest field, while RM's copy length is decided by RM, so the two can disagree. The same applies to RM_ALLOC nested params, which travel the same path (nvidia.rs:2020).
- Fix: Make the backend the only authority for every NvP64 in RM escapes, and refuse anything it does not understand before calling the host ioctl.

(1) Generate, per supported driver version, a backend table from embedded_param_copy.c (RM_CONTROL) and from the RM_ALLOC class list, the way gen/nvkms is generated. Key it by cmd or class and give each pointer offset, its count-field offset and width, the element size, and the in/out/zero flags. For each pointer, the backend allocates a GuardedBuf at RM's own count*elem, rejecting anything over RMAPI_PARAM_COPY_MAX_PARAMS_SIZE. It copies in the guest bytes, which must be exactly that length or the call gets EINVAL, and returns only that length. Delete the heap Vec deep_buf and DEEP_BUF_FLOOR (nvidia.rs:2296-2326).

(2) Refuse with EINVAL, before host_ioctl: any table command whose pointers were not all replaced, any command not in the table whose paramsSize exceeds the known inline struct size for that cmd, and, at minimum, all table-listed pointer commands the backend has no entry for.

(3) Refuse outright NV_ESC_RM_VID_HEAP_CONTROL with function == NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR, and NV_ESC_RM_ALLOC_MEMORY / NV_ESC_RM_ALLOC with hClass 0x71 (NV01_MEMORY_SYSTEM_OS_DESCRIPTOR). If guest userptr import is needed later, implement it by translating guest pages into a backend-owned mapping of guest RAM, never by passing a guest VA through.

(4) Keep the guest table (gen/nvgpu_v1v2_rewrites.h, nvgpu_deep_only_table) only as a hint for how many bytes to marshal. The backend must recompute and check every length itself.

(5) Tests: a GET_CHANNELLIST with an undeclared pointer, a deep_len/count mismatch, and ALLOC_OS_DESCRIPTOR must each return EINVAL without host_ioctl being called (assert with the mock host_ioctl).

## S-2. [high] (sec-parsers) Wayland shm memory has only a per-connection cap (8 GiB) and the number of connections is unlimited, so a guest can commit host RAM without bound
- Area: wlwire/src/shm.rs + device/src/wl/serve.rs (host-side Wayland proxy)
- Evidence: wlwire/src/shm.rs:68 MAX_POOL_BYTES = 8 GiB, described as 'Pool bytes one connection may have'. may_grow (shm.rs:126-140) counts only this Shm instance's pools. engine.rs, the from_channel SHM_POOL branch, creates a memfd sized by the guest after may_grow passes. The host engine applies SHM_SYNC records from the guest with Shm::sync (shm.rs:269), which pwrites into that memfd, and no compositor round-trip is needed. open_wayland (device/src/wl/serve.rs:227) has no limit on how many channels exist. The only other bound is the 65536-entry handle table. The inventory lists 'shm 8GiB' as a cap, but it applies per connection.
- Scenario: A malicious guest kernel opens several DEV_WAYLAND CONNECT channels. On each one it creates wl_shm pools up to the per-connection limit, then sends SHM_SYNC records that the backend writes into host memfds (shmem pages). The WL_SEND backpressure check (conn.rs send(): local_out backlog) does not count SHM_SYNC bytes. Each channel can therefore pin 8 GiB of host shmem, and N channels pin N x 8 GiB. The host desktop's memory runs out, and the OOM killer, or shmem pressure, hits unrelated processes such as the compositor or other VMs.
- Fix: 1. Add one shm budget per backend (per VM) to WlConfig, for example `shm_budget: Arc<ShmBudget { used: AtomicU64, max: u64 }>`, and give it to every Engine or Shm. Reserve from it with a compare_exchange loop before each memfd creation (engine.rs:841) and before each server-side grow in resize (shm.rs:156). Give reservations back in `impl Drop for Pool`, subtracting the pool's final size. That way a pool that buffers still reference stays charged until its last Arc is dropped, and nothing is released twice through forget(). Keep the per-connection check as a second, smaller limit.

2. Lower the defaults. For example, set the per-backend budget to 1 GiB, add a --wayland-shm-budget override, and cut the per-connection MAX_POOL_BYTES to about 512 MiB. Four 4K RGBA swapchains of 3 buffers each take about 400 MiB.

3. Cap the number of concurrent DEV_WAYLAND channels per session in open_wayland (serve.rs:227). Count the Chan::Conn entries in self.wl.chans and return EMFILE above a limit such as 64.

4. As defence in depth, document that the backend should run in a systemd scope or cgroup with MemoryMax, since memfd shmem is charged to the memcg of the process that writes it. Also consider making /dev/nvgpu-wl 0660 with a group (nvgpu_wl.c:866) rather than 0666.

## S-3. [high] (sec-desktop) No per-VM cap on concurrent Wayland channels: each opens a host compositor connection, a reader thread, and up to ~8 GiB of host memory, bounded only by the 65536 handle table
- Area: Wayland proxy (backend) — device/src/wl/serve.rs, conn.rs, wlwire/src/shm.rs
- Evidence: open_wayland (device/src/wl/serve.rs:227-308) inserts one Wayland handle per OPEN(DEV_WAYLAND, WL_OPEN_CONNECT) with no limit other than the handle table (device/src/handle_table.rs:53 MAX_HANDLES=65536). Each successful CONNECT calls WlConn::open→start, which UnixStream::connect()s the host compositor (device/src/wl/conn.rs:259) and spawns a dedicated reader thread (device/src/wl/conn.rs:334-336). Per-connection host memory is capped only *within one connection*: MAX_POOL_BYTES=8<<30 is enforced by Shm::may_grow over a single Engine's own Shm (wlwire/src/shm.rs:68,126-140; the Shm is a field of Engine, engine.rs:229, one Engine per WlConn, conn.rs:301), the guest-direction queue is capped per connection at max_queue=64<<20 (conn.rs:123,138,470-476), and blob pending at MAX_PENDING=64 MiB per connection (wlwire/src/blob.rs:305-307). None of these are summed across connections, and there is no cap on the number of connections (grep for chans.len / MAX_CONN is empty). The backend runs unsandboxed with the desktop user's privileges (README.md:103,131,263; INVENTORY Part 2).
- Scenario: A guest driver opens thousands of DEV_WAYLAND channels (HELLO, then repeated OPEN with WL_OPEN_CONNECT), each of which (a) spawns a host thread and consumes a host fd + a Hyprland client slot, and (b) after create_pool + a committed shm buffer, backs up to 8 GiB of memfd pages per channel via SHM_SYNC (shm.rs:269-283 pwrite into the server memfd). A few dozen channels each committing large buffers pins tens of GiB of host RAM; scaling toward the 65536 handle cap exhausts host threads/fds first. Result: the host desktop session (compositor and the user's apps) is OOM-killed or wedged — a host-impacting denial of service reachable by an untrusted guest with only BCAP_WAYLAND.
- Fix: (1) In open_wayland, before WlConn::open and WlConn::from_export, add a cap per session: count Chan::Conn entries in self.wl.chans against a small WlConfig field (for example max_conns = 16, settable with --wayland-max-conns). Past the cap, return EMFILE.

(2) Add one memory budget per session, shared by all of its connections, with a default of a few GiB and a flag to change it:
- Add a budget: Arc<AtomicU64> (limit plus used) to WlConfig, and pass it into EngineConfig.
- In Shm::add_pool and resize (engine.rs:852,916), charge the delta with compare-exchange against the shared limit. Replace the per-Engine may_grow check with this, or keep may_grow as a second, per-connection check.
- Credit the bytes back in Shm::forget and when the Engine drops, and fail with ERR_NO_MEMORY when over budget.
- Also lower MAX_POOL_BYTES to something like 1 GiB.

(3) Charge to_guest_bytes and blob pending against the same session budget, or a sibling one, so N×64 MiB queues cannot add up.

(4) As defense in depth, document or enforce that the backend runs in its own cgroup with MemoryMax. The shmem is charged to the process that pwrites it, so a memcg OOM then kills only that VM's backend and not the host desktop.

(5) Optionally, count open channels per struct file owner in driver/nvgpu_wl.c. This is not a security boundary, because the guest kernel is untrusted, but it keeps one unprivileged guest process from taking the whole session's channel allowance.

## S-4. [high] (sec-dos) Wayland shm pools are backed by host memfds, capped at 8 GiB per connection, with no per-VM cap and no cap on connections; host-wide OOM that the OOM killer cannot attribute
- Area: wlwire shm / device/src/wl (Wayland proxy, --wayland-socket)
- Evidence: OURS: wlwire/src/shm.rs:65-68 `MAX_POOL_BYTES = 8 << 30` is per Shm, and there is one Shm per Engine, so one per connection. may_grow (shm.rs:124-141) counts only this connection's pools. engine.rs:831-852: every guest wl_shm.create_pool makes `sys::memfd("nvgpu-wl-shm", size)` (sys.rs:26-35, ftruncate, sparse), up to i32::MAX (about 2 GiB) per pool. engine.rs:507-514 plus shm.rs:283-298: a REC_SHM_SYNC record from the channel is pwrite()n into that memfd as soon as the buffer id is known. No commit is needed and the compositor does not have to agree first. open_wayland (device/src/wl/serve.rs:226-296) never counts self.wl.chans, and nothing else in device/src or wlwire caps channels (grep for chans.len/MAX_CONN finds nothing). After a hangup the engine, and with it every pool memfd, lives until the guest closes the handle: hangup() (conn.rs:454-465) only shuts the socket, and the WlConn is dropped only in wl_forget/wl_forget_all (serve.rs:299-322). GUEST: /dev/nvgpu-wl is 0666 (driver/nvgpu_wl.c:866). Each open file is one channel (nvgpu_wl.c:230-283) with no per-device count. SEND passes guest records through untouched; only descriptors are checked, and SHM_POOL goes by value (nvgpu_wl.c:396-425). HOST: Hyprland maps each pool it receives. The memfd pages are charged to the writer (the backend) and appear in no process's RSS until someone maps and faults them.
- Scenario: An unprivileged guest process opens /dev/nvgpu-wl 4 times and CONNECTs each. On each connection it sends get_registry, binds wl_shm, and then 4 x create_pool(size=0x7fffffff) with create_buffer(stride=65536,height=32767) on each pool. In the same frames it sends about 2048 REC_SHM_SYNC records of 64 KiB per pool (4 MiB frames). The backend pwrite()s about 8 GiB of shmem per connection, 32 GiB in total, in seconds. It keeps the handles open and never commits. Host RAM runs out. The OOM killer's badness (rss+swap+pgtables) does not see unmapped memfd pages, so it kills the largest innocent process: another VM's QEMU (its memfd guest RAM is mapped and counted) or the host compositor. The pages stay pinned because the offending backend still holds the fds. If the guest commits one buffer, Hyprland faults the pages in, becomes the largest RSS, and is killed, which takes down the host desktop and every VM's display. With the default NOFILE of 1024 and about 9 fds per such connection, about 110 connections are possible, so the reachable quota is far above any host's RAM.
- Fix: 1. Add a per-backend (per-VM) shm budget that every connection shares.
   - `WlConfig` gets a `shm_budget: Arc<AtomicU64>`, with its limit set by a `--wayland-shm-budget` flag, default about 512 MiB to 1 GiB.
   - `Shm::may_grow` (shm.rs:126) is replaced by a `try_charge(more)` that does a compare-and-swap on the shared counter. Call it from engine.rs:841 (create_pool) and shm.rs:156 (resize).
   - Give `Pool` a `Drop` that returns its size to the budget. Buffers keep pools alive through `Arc`, so a pool is released when its last reference drops.
   - Lower the per-connection `MAX_POOL_BYTES` to about 256 MiB. That is still more than enough for several 4K double-buffered swapchains.

2. Cap concurrent Wayland channels per VM in `open_wayland` (serve.rs:227), for example 64, returning EMFILE past that. Also cap pending export ACCEPTs.

3. When a connection hangs up (conn.rs:454), drop the engine's `Shm` pools and blob assemblies right away, so pages from a dead connection are freed without waiting for the guest to close the handle.

4. Optionally reject SHM_SYNC for a buffer the client has not yet attached and committed. This stops a guest from writing pool pages before the compositor has taken the buffer. It is hardening only; the budget above is what bounds the memory.

5. Document and ship a launcher that runs each backend in its own memory cgroup, for example `systemd-run --scope -p MemoryMax=... -p MemorySwapMax=...`. The backend's memfd writes are charged to the writer's memcg, so an OOM stays inside that VM. Also set `oom_score_adj` so the backend, not other VMs, is the preferred victim.

6. A per-device channel cap in driver/nvgpu_wl.c is optional. It does not stop a malicious guest kernel, so it is not a security fix.

## S-5. [high] (sec-posture) When the backend runs as root (as the repository's own launcher does), every guest process is an RM administrator: all of BAR0 mappable read-write, register allowlist bypassed, and DRM capable() paths enabled. Nothing guards against it.
- Area: backend process privileges
- Evidence: scripts/run-guest.sh:14,48 runs `/root/vhost-user-nvgpu` from a root tree, and the same script exists on dev. device/bin/vhost-user-nvgpu.rs checks neither euid nor capabilities (a grep of device/, wlwire/ and nvgpu-wl-guest/ finds no geteuid, prctl, capset, seccomp or landlock; the only getuid is export.rs:126). On the host, NVIDIA derives privilege from the calling process: escape.c:381 `secInfo.privLevel = osIsAdministrator() ? RS_PRIV_LEVEL_USER_ROOT : RS_PRIV_LEVEL_USER`, with osIsAdministrator being capable(CAP_SYS_ADMIN) (os-interface.c:389-391, nv-linux.h:490). osapi.c:2209-2213 has RmValidateMmapRequest return NV_PROTECT_READ_WRITE for any BAR range to an admin, and it is used for every BAR mapping at osapi.c:2536. gpu_access.c:1230 skips the register allowlist for admins, affecting regops through the unfiltered RM_CONTROL (nvidia.rs:2011). DRM files the backend opens as root are authenticated (linux drm_file.c:149), pass drm_master_check_perm for any file (drm_auth.c:239), and GETFB returns handles (drm_framebuffer.c:557,635; the backend's own FB tracking in xfer.rs:1269-1297 still blocks that one). Nothing on the display branch needs root: card DRM master comes from being the first opener, the uevent group is NL_CFG_F_NONROOT_RECV, /proc/self/fd readlink is unprivileged, and /dev/udmabuf only needs the kvm group.
- Scenario: The operator follows scripts/run-guest.sh. A guest user with no privileges opens /dev/nvidiactl and /dev/nvidia0, allocates a client, device and subdevice, and issues RM_MAP_MEMORY on the subdevice for the whole BAR0 register range, then mmaps it. The backend places the host mapping in the guest window. Because the host RM client is USER_ROOT, the validation step grants read-write on all of BAR0. The guest then programs the GPU's MMU and copy engines to DMA into host physical memory (limited only by the IOMMU mode), which compromises the host kernel. Without root, the same request is limited to the user-accessible register ranges.
- Fix: 1. At backend startup (device/bin/vhost-user-nvgpu.rs main), refuse to start if geteuid()==0 or if capget shows CAP_SYS_ADMIN in the effective or permitted set, unless the operator passes an explicit --allow-root flag, which prints a loud warning.
2. After opening the host device nodes and adopting any lease fds, drop all capabilities: capset everything to empty, clear the bounding set with PR_CAPBSET_DROP, and clear the ambient set. Then set PR_SET_NO_NEW_PRIVS=1 and PR_SET_DUMPABLE=0.
3. As defence in depth, and because it holds even when someone forces root: in the RM_ALLOC dispatch (nvidia.rs:2023), refuse hClass NV01_MEMORY_LOCAL_PRIVILEGED (0x3f), and rewrite or refuse root-client classes other than NV01_ROOT_NON_PRIV (0x1000? verify the class value in cl0000.h) so that bIsRootNonPriv is set on every guest client. rmclientIsAdmin() is then false regardless of the backend's credentials (client.c:88,419).
4. Change scripts/run-guest.sh to run the backend as a dedicated user in the video/render (and kvm, for udmabuf) groups, for example via setpriv --reuid/--regid/--init-groups --inh-caps=-all --no-new-privs.
5. Add a line to the README security section: RM, DRM and NVKMS all take guest privilege from the backend's credentials, so the backend must never run with CAP_SYS_ADMIN.

## S-6. [medium] (sec-gpuside) Guest KMS commits can scan out any host framebuffer by id (FB_ID / fb_id not ownership-checked), which also defeats the planned fix for M-7
- Area: DRM core KMS on lease/card files (ATOMIC FB_ID, SETCRTC, SETPLANE, PAGE_FLIP)
- Evidence: Ours: GETFB/GETFB2 handle return is restricted to the file's own framebuffers (xfer.rs:1269-1297; KmsFileState.fbs at xfer.rs:137-155). The same ownership set is never consulted for framebuffers used as scanout sources. kms.rs:617 classifies FB_ID as PropKind::Plain, so ATOMIC passes it unchecked (xfer.rs check_props :1117-1170). SETCRTC, SETPLANE and PAGE_FLIP have no fb field rule (gen/schema/drm_kms.py SETCRTC/SETPLANE/PAGE_FLIP entries). Linux: framebuffers are looked up device-globally. drm_mode_object_lease_required covers only CRTC, connector and plane (drm_mode_object.c:126-155). The lookups are drm_atomic_uapi.c:547, drm_crtc.c:771, drm_plane.c:1159 and :1482.
- Scenario: A guest holding a lease (--wayland-lease with the Hyprland/aquamarine patches), or a card in --kms-card mode, tries fb ids 1..N. For each one it sets FB_ID on its leased plane, or passes it as fb_id to PAGE_FLIP or SETCRTC. Any id belonging to the host compositor or to another VM's lease gets scanned out on the guest's connector, which shows another tenant's screen contents. Combined with GET_CRTC_CRC32 (M-7), a guest can read another tenant's pixels through CRCs of its own CRTC. The M-7 fix limits CRC to CRTCs in the guest's lease, but that CRTC can now be showing a foreign framebuffer, so the fix does not close the leak. The guest also holds a reference that keeps foreign framebuffers alive.
- Fix: 1. Add POL_FB_USE in gen/schema/lang.py and enforce it in xfer.rs (Prepared::run, before the ioctl, next to check_props). For each fb id listed below, require id==0 or `self.kms.owns_fb(id)`, and return EPERM otherwise:
   - SETCRTC: drm_mode_crtc.fb_id at offset 16. Skip the check when it is 0 or -1 (-1 means keep the old fb, drm_crtc.c:760).
   - SETPLANE: drm_mode_set_plane.fb_id at offset 8.
   - PAGE_FLIP: drm_mode_crtc_page_flip.fb_id at offset 4. This also covers PAGE_FLIP_TARGET, which has the same layout.
   - ATOMIC: in check_props, add a PropKind::FbId, or check the name directly: when trim(name)=="FB_ID", validate `value`.
   - OBJ_SETPROPERTY (POL_SETPROP): when the property name is FB_ID, validate the value at offset 0 of drm_mode_obj_set_property. This path is missing from the claim and must be covered, or the other four checks can be bypassed.
2. Keep ownership per host KMS file, the same as for GETFB. For any cross-file sharing a guest compositor may need (e.g. dup'd lease fds), make sure all guest fds that share one host file share one KmsFileState.
3. Add tests with a foreign id and with an owned id on each of the five paths. Add one test showing the host ioctl is not issued on refusal. Update kms.rs:617's Plain list/test if FB_ID gets its own kind.
4. In NVK_VERIFICATION, note that the M-7 fix (CRC only on leased CRTCs) is sound only once this lands, and update DESIGN.md's KMS policy section.

## S-7. [medium] (sec-dos) No per-VM cap on Wayland channels: each one is a full host-compositor client (up to 131072 objects) plus a 64 MiB backend queue that is kept after hangup and a reader thread
- Area: device/src/wl/serve.rs, conn.rs, wlwire objects; host compositor
- Evidence: OURS: open_wayland (serve.rs:226-296) has no count or rate limit. WlConfig::new (conn.rs:88-97): max_queue 64 MiB and max_backlog 4 MiB, both per connection. collect() (conn.rs:414-428) hangs up when the queue is over 64 MiB but keeps st.to_guest, which is freed only when the guest drains it or closes the handle. Each connection spawns a reader thread (conn.rs:294-297), and every close spawns another thread (serve.rs:98-110). wlwire/src/objects.rs:46 caps objects at 1<<17 per connection only. That is about 5 backend fds per connection (socket, ready, wake, table dup, pump dup: conn.rs:258-300, serve.rs:276-288), so the only effective cap is the backend's inherited RLIMIT_NOFILE, which the backend never sets (grep for RLIMIT/setrlimit in device finds nothing). The handle table's 65536 is not reached first. HOST: libwayland keeps every wl_registry until disconnect (there is no destroy request) and walks all of them on every wl_global_create (wayland/src/wayland-server.c:1496-1498). Hyprland has no client limit and raises its own NOFILE (hyprland/src/Compositor.cpp:146-160).
- Scenario: A guest process opens about 190 channels (the NOFILE 1024 default). On each it creates about 130k objects (wl_surface or wl_registry at 12 bytes per request) and reads/discards the registry replies so it is never dropped. The host compositor now holds about 25M resources, which is GBs in Hyprland for surfaces. Each host output hotplug or new global publishes to about 25M registries on the compositor main thread, which freezes the host desktop for seconds and overflows other clients' 1 MiB buffers (Compositor.cpp:306), disconnecting them, including other VMs' proxied clients. Separately, a guest that stops reading pins 64 MiB per channel in the backend (about 12 GiB at 190 channels). With a launcher that raises NOFILE (systemd/libvirt), the channel count reaches thousands and the thread count scales with it, counted against the desktop user's NPROC/TasksMax.
- Fix: 1. **Per-VM channel cap.** Add a per-VM limit on concurrent channels in `open_wayland` (serve.rs:227): count `self.wl.chans` entries of kind `Chan::Conn` and return -EMFILE past `--wayland-max-clients` (default about 64-128, enough for one channel per guest client). Also rate-limit `WL_OPEN_CONNECT` with a token bucket (for example 20/s, burst 32; -EAGAIN when empty) so connect/close churn cannot hammer the compositor. Put the check before `WlConn::open`, so no socket or thread is created for a refused open.
2. **Per-interface object caps in wlwire.** Add them to the engine's new_id path (objects.rs:103 / engine.rs:680), for example `wl_registry` ≤ 16 and `wl_surface`/`wl_subsurface` ≤ 4096 per connection. Lower `MAX_OBJECTS` to about 32k. Hitting a cap is a fatal "too many objects" error for that channel, as it is today.
3. **Per-VM queue budget.** Make `max_queue` a per-VM budget, for example an `Arc<AtomicUsize>` shared through `WlConfig` and charged and credited in `collect`/`recv`. On hangup, drop the queued units except the final ERROR/HANGUP records, releasing any fds they hold.
4. **Explicit backend NOFILE.** At backend startup, set RLIMIT_NOFILE to a fixed value (for example min(hard, 8192)) and log it, so the per-VM fd bound is deliberate and matches the caps above.
5. **Guest driver (defense in depth).** Optionally cap bound channels per device in `nvgpu_wl_connect` (nvgpu_wl.c:230), for example 256, so one guest user cannot use up the VM's whole channel budget. The backend cap is the one that matters.

## S-8. [medium] (sec-dos) Guest can keep the global nvkms_lock busy with forced EDID/DDC reads (NVKMS QUERY_DPY_DYNAMIC_DATA on any host dpy, and lease GETCONNECTOR count_modes=0), with up to 16 parallel executors; this stalls host compositor flips and other VMs
- Area: device/src/nvkms.rs policy, device/src/exec.rs executor keying
- Evidence: OURS: nvkms.rs:480 only scrubs QUERY_DPY_DYNAMIC_DATA's override fields. It is not checked against grants (compare SET_DPY_ATTRIBUTE, nvkms.rs:492-494), has no rate limit, and is also v1-reachable (flat params, generated.rs:262 children len 0). The schema marks it Exec::Executor (generated.rs:415). executor_key = target file (session.rs:162-164), so each modeset open is its own lane. MAX_THREADS=16 (exec.rs:33), MAX_MODESET_OPENS=64 (nvkms.rs:70), and the guest's /dev/nvidia-modeset is 0666. HOST: QueryDpyDynamicData does no permission check for any dpy of an ALLOC_DEVICE'd device (nvkms.c:1857-1872) -> nvDpyGetDynamicData calls nvRmGetConnectedDpys (nvkms-dpy.c:3145) and DpyConnectEvo (nvkms-dpy.c:109-133) -> ReadAndApplyEdidEvo (635-697) -> ReadEdidFromResman with NV0073_CTRL_SPECIFIC_GET_EDID_FLAGS_COPY_CACHE_NO (nvkms-dpy.c:1319-1326), a fresh DDC/AUX read. All of this runs under the global semaphore nvkms_lock (nvidia-modeset-linux.c:497, 1511-1536). The host compositor's nvidia-drm flips and modesets take that same lock uninterruptibly (nvkms-kapi.c:3488-3491, 3703-3705 -> nvkms_ioctl_from_kapi_try_pmlock -> nvkms_ioctl_common(..., NV_FALSE)). Same path through DRM: drm_mode_getconnector with count_modes==0 and a (lessee) master holds dev->mode_config.mutex and calls fill_modes (linux drm_connector.c:3371-3380) -> nv_drm_connector_detect -> getDynamicDisplayInfo (nvidia-drm-connector.c:165) -> QUERY_DPY_DYNAMIC_DATA (nvkms-kapi.c:1527).
- Scenario: An unprivileged guest process opens 16+ /dev/nvidia-modeset files, ALLOC_DEVICEs each, and loops QUERY_DPY_DYNAMIC_DATA over every host dpy, including the host's active desktop monitors. Each call does an RM connect-detect plus an uncached EDID read (tens of ms over 100 kHz DDC, a few ms over DP AUX) while holding nvkms_lock. With 16 guest waiters queued FIFO on the semaphore, every host compositor commit waits behind up to 16 of these. The host desktop and every VM whose display or flips go through NVKMS drop to a few frames per second for as long as the guest keeps going. DpyConnectEvo also re-pushes infoframes to the active head on every call (nvkms-dpy.c:130). In lease mode, a lessee looping GETCONNECTOR(count_modes=0) on its leased connector does the same while also holding mode_config.mutex. Timings are estimates; the lock and the uncached read are certain.
- Fix: 1. Outside --kms-card, add a policy arm in State::check (nvkms.rs:480) for QUERY_DPY_DYNAMIC_DATA that combines the scrub with a grant test. If dpy_granted(name, call.target, params, lo.<dpy_dynamic target>) passes, forward the call as today. Otherwise, do not refuse with EPERM, because guest NVKMS clients enumerate every dpy at start-up and would break. Answer from a per-(device, disp, dpy) reply cache in the backend instead:
   - Fill the cache on the first forwarded query.
   - Invalidate it on NVKMS DPY_CHANGED/DYNAMIC_DPY_CONNECTED events and on the hotplug listener's EV_HOTPLUG.
   - Apply the cache across the whole VM, not per file.
   This takes host EDID reads for ungranted dpys off the guest's control.
   To do this, export the device/disp/dpyId offsets for QUERY_DPY_DYNAMIC_DATA from gen/schema/nvkms.py, as is already done for set_dpy_attribute.

2. Even for granted dpys, and in --kms-card mode, put forced probes behind a per-VM token bucket, for example 2 per second per dpy. When the bucket is empty, serve the cached reply.

3. For GETCONNECTOR on DrmCard/DrmLease handles, allow count_modes==0 through at most once per connector per second. Otherwise rewrite count_modes to the last nonzero count the backend saw for that connector, so drm_mode_getconnector skips fill_modes (drm_connector.c:3374). Keep the guest's modes_ptr and the AllOrNothing bound the same.

4. Change PendingIoctl2::executor_key (session.rs:162-164) so that every Modeset-class and every Kms-class job of one VM shares a single lane per class. For example, return a constant per class for Modeset and Kms, instead of the target file. A VM then never has more than one waiter queued on nvkms_lock or mode_config.mutex, however many files it opens. Keep per-file keys only for render-class calls. This also limits M-7 and every other nvkms_lock-heavy command.

5. Also cap the number of modeset opens that may hold an ALLOC_DEVICE at once far below 64 (4 is enough for a desktop guest).

## S-9. [medium (the operator has to opt in with --wayland-lease plus a `leasable` desktop monitor; after that any unprivileged guest process can stall the whole host compositor and other VMs' proxied clients)] (sec-dos) Lease request/withdraw loops on a 'leasable' desktop monitor force repeated blocking modesets and workspace migration in the host compositor; nothing rate-limits them
- Area: --wayland-lease + patches/hyprland lease flow; wlwire lease policy
- Evidence: PATCH: patches/hyprland/0001-lease-desktop-outputs.patch:15-33 and :490-515. Every lease first runs CMonitor::releaseForLease: workspaces and focus move away, then 'a blocking modeset through CDRMOutput::commit()' disables the output on the compositor thread. When the lease ends, 'the monitor is reconnected ... a full modeset as on hotplug' and the connector is advertised again at once (patch:40-58). patches/README.md:28-46 documents leasable desktop monitors as the primary configuration. OURS: the wlwire lease policy only decides visibility by global name (wlwire/src/policy.rs:55-66). The engine only tracks release/released (engine.rs:940-941). Nothing counts or throttles wp_drm_lease_request_v1.submit or wp_drm_lease_v1.destroy. /dev/nvgpu-wl is 0666 in the guest (nvgpu_wl.c:866). KERNEL: each lessee close emits a LEASE uevent to every netlink listener (linux drm_lease.c:291-294, drm_sysfs.c:423).
- Scenario: The operator enables --wayland-lease and marks DP-2 leasable while using it as a desktop monitor. Any guest user process loops: create_lease_request(DP-2) -> submit -> receive the fd -> destroy the lease. Each iteration makes Hyprland do a blocking disable commit (nvidia-drm waits up to 3 s per wait), move the user's workspaces off DP-2 and back, do a full modeset to re-enable the monitor, and handle a LEASE uevent. The compositor main loop spends most of its time in blocking commits, so the whole host desktop and every other VM's proxied Wayland clients stall. The physical monitor keeps blanking. host udevd and all netlink listeners (every other VM's hotplug thread) wake on each cycle.
- Fix: 1. Primary fix, on the host backend (the trusted side). Add a lease throttle to the wlwire engine for wp_drm_lease_request_v1.submit, keyed per VM connection, or per lease-device global plus connector.
   - Track when the last lease was granted (EVT_LEASE_FD seen) and when it ended (wp_drm_lease_v1.destroy, or EVT_FINISHED).
   - If a submit arrives within a cool-down (for example 5 s, doubling up to about 60 s whenever the previous lease lived for less than the cool-down), do not forward it at once. Queue it and release it when the timer expires.
   - Queuing is simpler and safer than inventing a local `finished`, because the compositor still creates and owns the wp_drm_lease_v1 object.
   - Log once when the throttle engages, with a rate limit on that log.
   - Put the knob next to --wayland-lease (for example --wayland-lease-cooldown).
2. Defence in depth, in patches/hyprland. In CDRMLeaseResource, refuse (reject({}) -> finished) a new lease on a monitor whose previous lease ended less than N seconds ago. Record the time in CMonitor::onLeaseEnded, and re-offer from a timer rather than calling readvertise() at once. This also protects against native host clients.
3. Optional. Document in patches/README.md that the most robust setup for an untrusted guest is a monitor that is `disabled = true, leasable = true`. releaseForLease() then does no onDisconnect or blocking modeset, and the reconnect only disables it again.

## S-10. [medium] (sec-posture) /dev/nvgpu-wl is mode 0666: any guest user can become a host-compositor client and, in export mode, ACCEPT host clients and act as their compositor
- Area: guest-internal isolation (/dev/nvgpu-wl), export mode
- Evidence: driver/nvgpu_wl.c:864-866 sets mode 0666 with the comment "Separating guest users from each other is the guest's own business". CONNECT accepts any mode up to NVGPU_WL_ACCEPT from any opener (nvgpu_wl.c:241, 249), and the backend honours it: serve.rs:248-254 allows LISTEN, and serve.rs:255-270 serves ACCEPT with `export.accept_pending()` (export.rs:88-95). Who ACCEPTs first gets the connection; nothing binds ACCEPT to the daemon's LISTEN file. The host client is then faced by WlConn::from_export with Policy::default (conn.rs:238-244). export.rs:7-9 states the intent: "only this user's own programs are meant to be its clients", meaning host desktop-user programs. The intended consumer is the daemon, which LISTENs and then ACCEPTs (nvgpu-wl-guest/src/daemon.rs:297, 559). In CONNECT mode the same 0666 node lets any guest uid (including service accounts that have no seat and no XDG_RUNTIME_DIR) open host compositor connections within the 42-global allowlist, including data-device and primary-selection. On a real Linux desktop the equivalent is a 0700 $XDG_RUNTIME_DIR socket. All LISTEN handles also share the export's single readiness eventfd (serve.rs:249-253), so another user's LISTEN handle can take the daemon's wake-ups.
- Scenario: Precondition: export mode (--wayland-export). The host user runs `WAYLAND_DISPLAY=<export socket> foot`, a host terminal running as the desktop user. In the guest, an unprivileged process (for example a compromised web service) loops open("/dev/nvgpu-wl") + CONNECT{mode=ACCEPT} until it gets EAGAIN-free success, beating the daemon. It then acts as the Wayland server for the host terminal: it advertises wl_seat and a keyboard, sends a keymap blob, then enter and key events. That types `curl …|sh⏎` into the host terminal, which is code execution as the host desktop user. The host already treats the guest compositor as untrusted, but here the bar drops from 'guest root or the compositor account' to 'any guest process'. In CONNECT mode the same user can show windows on the host desktop, for phishing, and read the clipboard when focused.
- Fix: **1. Default the node to non-world access**
- Change nvgpu_wl.c:866 to `misc.mode = 0600`, or to 0660 with the group set by udev.
- Ship a udev rule, for example `KERNEL=="nvgpu-wl*", GROUP="nvgpu-wl", MODE="0660"`, or `TAG+="uaccess"` for a seat user.
- Guest clients only talk to the daemon's socket, so only the daemon's account needs the device.
- Update the comment at lines 864-865 to match.

**2. Bind ACCEPT to the single LISTEN holder in the kernel**
- Add `struct file *listener` (under a mutex) to nvgpu_wl_dev.
- LISTEN fails with -EBUSY while a listener is live. nvgpu_wl_release clears it at nvgpu_wl.c:813.
- ACCEPT requires the caller to name the listener. Add a `listen_fd` field to struct nvgpu_wl_connect, which is currently flags-only, and check `fget(listen_fd)->private_data == wl->listener_wf` for the same device. Alternatively, make ACCEPT an ioctl on the LISTEN fd that returns a new channel fd through anon_inode_getfd.
- Either way, the daemon changes only at daemon.rs:559, where it passes its export channel's fd.
- A weaker interim fix: at CONNECT time, refuse ACCEPT unless `uid_eq(current_euid(), listener->f_cred->euid)`.

**3. Backend defence in depth**
- In serve.rs:248, refuse a second LISTEN while a `Chan::Listener` exists, with EBUSY.
- Then the shared eventfd dup cannot be split across guest users.

## S-11. [medium] (cor-guest) Host GEM handle gets two guest owners: gem_index entry is erased before the dying proxy's GEM_CLOSE, and a PRIME import that returns the same handle in that window gets a second proxy, which then goes stale and later points at a reused handle
- Area: driver/nvgpu_drm.c GEM proxies / gem_index; export-mode Wayland import; KMS gem_out
- Evidence: Our side:
- nvgpu_gem_free erases its own gem_index entry first (driver/nvgpu_drm.c:517-518). It then calls nvgpu_fence_gem_free, which takes the global nvgpu_rehome_lock mutex (driver/nvgpu_fence.c:1637). That mutex is held across two synchronous HOST_OPs in nvgpu_fence_rehome (fence.c:1576-1602), so the wait can be long. Only after that does free send GEM_CLOSE(owner, host_handle) (drm.c:523-524).
- The comment at drm.c:512-516 reasons that the number cannot be handed out again before the close. That holds for new objects only. A PRIME import of the *same* object returns the existing handle: drm_gem_prime_fd_to_handle → drm_prime_lookup_buf_handle (linux drivers/gpu/drm/drm_prime.c:306-310), used by the backend's DMABUF_IMPORT (device/src/hostfd.rs:518-525).
- nvgpu_gem_proxy_find skips a proxy whose refcount is already zero (drm.c:1109-1113). After the erase, nvgpu_gem_proxy_new's xa_insert succeeds (drm.c:911).
- Two callers make a proxy for a host handle that may be an existing one: nvgpu_dmabuf_from_host (drm.c:1026-1035), reached from nvgpu_wl_import after HOST_OP DMABUF_IMPORT (driver/nvgpu_wl.c:566-571), and nvgpu_kms_gem_out for GETFB/GETFB2, whose results are re-homed with a PRIME import (driver/nvgpu_kms.c:1325-1343).
- Host GEM handles are reused lowest-free (linux drivers/gpu/drm/drm_gem.c:499).
- The daemon imports every export-mode dma-buf, from all channels, into one render file (nvgpu-wl-guest/src/daemon.rs:167, 292, 685-690).

Other paths that break the same one-owner-per-host-handle rule:
- The fence re-home table counts sharing only among its own entries, not against gem_index (fence.c:1644-1649).
- The abandoned-reply reaper GEM_CLOSEs every DMABUF_IMPORT or IOCTL2 GEM result as if it were new (xfer.c:987, 1015-1018), which is wrong when the import returned an existing handle.
- Scenario: Export mode.
1. A host client (another VM) sends dma-buf D. The daemon imports it into render file R: host handle H, proxy P, handed to the guest compositor.
2. The compositor destroys that wl_buffer and drops its import. The daemon's descriptor for it is closed too, so P's refcount reaches 0. nvgpu_gem_free erases P from R's gem_index, then blocks on nvgpu_rehome_lock (or just waits its turn on the ring).
3. Meanwhile the client creates a new wl_buffer from the same D. This is common for clients that recreate wl_buffers from a pool. WL_RECV → DMABUF_IMPORT on R returns the existing H.
4. nvgpu_gem_proxy_find(R, H) returns NULL (P is gone from the index), so a new proxy Q(H) is made and exported to the compositor.
5. P's GEM_CLOSE(R, H) now runs and the host frees H. Q is stale.
6. The next import of any buffer into R (another client's, possibly from another VM) is given H by the host. nvgpu_gem_proxy_find(R, H) finds Q and hands out Q's dma-buf for it. The compositor gets the old client's object for the new client's buffer, or the new client's memory once Q is mapped: two clients' buffers are mixed up.
7. When Q dies it GEM_CLOSEs H, which is now the new client's handle. That proxy goes stale in turn, and the corruption cascades.

The KMS path has the same window: a GETFB2 whose re-home returns H while the compositor's proxy for H is being freed.
- Fix: Keep a host handle indexed until it is really closed, and make importers wait for a pending close and then import again.
1. nvgpu_gem_free: leave the gem_index entry in place and do nvgpu_fence_gem_free, GEM_CLOSE and MUNMAP first. Only then run xa_cmpxchg(&owner->gem_index, h, ng, NULL) and wake a per-nvgpu_fd waitqueue (gem_wq). While it waits, the entry is a tombstone with refcount 0, and nvgpu_gem_proxy_find already refuses it.
2. nvgpu_gem_proxy_new: when xa_insert returns -EBUSY and xa_load finds an entry whose refcount is 0, return a distinct -EAGAIN, not -EEXIST. Set ng->host_handle = 0 so the new proxy's free does not close H.
3. nvgpu_wl_import: on -EAGAIN from nvgpu_dmabuf_from_host, wait_event on gem_wq until xa_load(gem_index, h) no longer holds that tombstone, then run HOST_OP DMABUF_IMPORT again. The backend dma-buf handle is still held, because it is closed only at out:. The re-import gives a fresh handle, which may even be H again, but now truly new. Do the same in nvgpu_kms_gem_out: fail the GETFB/GETFB2 call with -EAGAIN, or re-home again, rather than adopting a number that is about to be closed.
4. nvgpu_fence_rehome: after DMABUF_IMPORT, check gem_index of the target file first. If a live proxy owns res[0], record the entry as borrowed and never GEM_CLOSE it; if a tombstone owns it, wait and retry. In nvgpu_fence_gem_free, skip the close for borrowed entries.
5. Reaper (nvgpu_xfer.c:987, 1015-1018): do not GEM_CLOSE a DMABUF_IMPORT or IOCTL2 gem-out result that the file's gem_index holds, live or tombstone. That handle already existed and is not the reply's to close.
A per-fd mutex held from import through proxy creation, and in free around close-then-erase, also works. It must not be held across drm_gem_object_put, or free re-enters and deadlocks. The tombstone-and-waitqueue approach avoids that lock-ordering hazard.

## S-12. [medium] (cor-guest) A dead fence's buried consumer can signal a new, unrelated fence proxy that reused its xarray id
- Area: driver/nvgpu_fence.c host fence proxies
- Evidence: - nvgpu_host_fence_release (fence.c:173-184) erases the proxy's id from nvgpu_host_fences and only *buries* its consumer. The consumer stays registered in the event registry, under the old WATCH cookie, until the next nvgpu_fence_reap(). The host sync_file is closed asynchronously on the ordered workqueue (xfer.c:863-895), which can sit behind reaper work that makes synchronous calls (xfer.c:1093-1098). Until that CLOSE runs, the backend's one-shot EV_FENCE for the old cookie can still arrive.
- nvgpu_host_fence_deliver (fence.c:197-216) looks up `xa_load(&nvgpu_host_fences, e->id)` and signals whatever fence it finds. It never checks that the fence found is the one this consumer belongs to (f->ev == e).
- The xarray is XA_FLAGS_ALLOC1 (fence.c:149-150), so xa_alloc hands out the lowest free index (linux lib/xarray.c:2001-2009). A freed id is reused at once.
- nvgpu_host_fence_fd reaps at fence.c:245 and allocates at :254, with two GFP_KERNEL kzallocs (which may sleep) in between. A release that lands in that gap is not reaped before its id is reused.
- Scenario: 1. Process A drops its last reference to unsignalled host-fence proxy F1 (id 5, cookie C1). For example it closes an out-fence or SEMSURF sync_file fd it never waited on. The release erases id 5, buries e1 (still registered on C1) and queues CLOSE(H1).
2. Concurrently, thread B is in nvgpu_host_fence_fd for a new host sync_file H2. It has already run nvgpu_fence_reap(), so e1 is not unregistered. xa_alloc_irq returns id 5 and F2 is initialised there.
3. The host GPU signals H1 before the queued CLOSE(H1) reaches the backend. The backend sends EV_FENCE(C1).
4. e1's deliver loads id 5, gets F2, and calls dma_fence_signal(F2).
5. F2 now reads as signalled while H2 has not. A guest consumer (poll or SYNC_FILE_INFO), or nvgpu_fence_unwrap (which then sends 'no fence' to the host for IN_FENCE_FD or SEMSURF_WAIT), proceeds before the GPU work behind H2 is done. The result is scanout or sampling of a half-rendered buffer. There is no error, and nothing corrects it later.
- Fix: Make an id-to-fence lookup succeed only for the consumer that owns the fence, and publish the fence in the xarray only after it is fully initialised.

(a) In nvgpu_host_fence_deliver (driver/nvgpu_fence.c:210-214), after dma_fence_get_rcu succeeds, add:

    if (READ_ONCE(f->ev) != e) { dma_fence_put(&f->base); return; }

(b) In nvgpu_host_fence_fd, reserve the id without publishing the fence:

    xa_alloc_irq(&nvgpu_host_fences, &e->id, NULL, xa_limit_32b, GFP_KERNEL)

This reserves the slot, and xa_load returns NULL for it. Then set f->dev, f->handle and f->ev = e, and call dma_fence_init64. Only after that publish with xa_store_irq(&nvgpu_host_fences, e->id, f, GFP_KERNEL) (or xa_cmpxchg). That store is an rcu_assign_pointer, so any reader that finds f also sees f->ev and the initialised refcount. The failure path before the store must xa_erase the reservation.

Optional hardening: allocate with xa_alloc_cyclic_irq, so an id is not reused while a stale consumer may still hold it. Alternatively, store the WATCH cookie in f and compare it with the delivered cookie. The f->ev check plus publish-after-init already closes the whole class.

## S-13. [medium] (cor-backend) Syncobj wait registrations are keyed by the syncobj handle number; after DESTROY and reuse, a new waiter joins a dead registration and is never woken
- Area: device/src/fence.rs Registrations::watch; device/src/nvidia.rs close_handle; finish_ioctl2
- Evidence: RegKey is {render handle, syncobj handle number, point, flags} (hostfd.rs:489-494). Registrations::watch returns Watched::Joined(r.cookie) for any existing key whose eventfd has not fired (fence.rs:331-333). Each registration keeps the old syncobj alive through its own syncobj file (`_syncobj`, fence.rs:263, 359), and by design ends only when it fires (fence.rs:30-35). Nothing removes a registration when the syncobj handle is destroyed: SYNCOBJ_DESTROY is forwarded as a plain render IOCTL2 (driver/nvgpu_fence.c:1369, 1411-1414), fence::before does not look at it (fence.rs:112-160), finish_ioctl2 special-cases only REVOKE_LEASE (session.rs:664), and close_handle does not touch syncobj_regs (nvidia.rs:1714-1731). The host reuses handle numbers at once: syncobj handles come from xa_alloc, lowest free index (drm_syncobj.c:606), and DESTROY is xa_erase (:635). Guest side: nvgpu_sowait_get (driver/nvgpu_fence.c:760-784) subscribes to the joined cookie. nvgpu_fence_eventfd (:1212-1262) polls once and then relies only on that cookie's EV_READY. Hyprland imports each client timeline with drmSyncobjFDToHandle and destroys it with the timeline (SyncTimeline.cpp:35, 47). It waits for every acquire point through drmSyncobjEventfd with flags 0 (SyncTimeline.cpp:87; DRMSyncobj.cpp:32; Compositor.cpp:616).
- Scenario: Guest Hyprland (compositor-VM or export mode) has render handle R. Client A's timeline is imported as syncobj handle 3. A commits a frame with acquire point 120, which is not yet materialized. Hyprland's SYNCOBJ_EVENTFD becomes HOST_OP SYNCOBJ_WATCH(R,3,120,0), a new registration. A is killed before point 120 is ever signalled; for example, its driver thread that would materialize the point dies with it. Hyprland destroys the timeline, so the host erases handle 3. The registration survives and keeps the orphaned syncobj alive, never to fire. Client B's timeline is imported next and gets handle 3 again (lowest free). On B's 120th frame the point is not ready at commit time. SYNCOBJ_WATCH(R,3,120,0) finds A's unfired registration and returns Joined(A's cookie). The guest polls once (not ready yet) and then waits on a cookie that can never fire. B's GPU work completes, but Hyprland's eventfd is never signalled, so B's surface freezes at frame 119 for good, with every later commit gated behind it. Separately, each such dead registration holds one of the REGISTRATION_CAP (1024) slots forever.
- Fix: Make a destroyed handle's registrations impossible to join, and clear them before the DESTROY reply reaches the guest.

1. **fence.rs:** add `Registrations::orphan(render: u32, syncobj: u32)`. It moves every `regs` entry with that (render, syncobj) into a new `orphans: Vec<Reg>`. Also add `orphan_file(render)`, which does the same for every entry of a render handle.
   - `len()` and the cap check count `regs.len() + orphans.len()`, which keeps the rule at fence.rs:30-35 that an entry counts until it fires.
   - `sweep()` also retires orphans whose eventfd has fired, pushing each handle onto `retired`.
   - `clear()` empties `orphans`.
   - `watch()` only ever looks up `regs`, so after DESTROY the same (render, handle, point, flags) gets a fresh registration on the new syncobj.
2. **session.rs `finish_ioctl2`:** next to the REVOKE_LEASE case at :664, when `prepared.name()` is SYNCOBJ_DESTROY (0xc0) and `prepared.result() == Some(0)`, read the u32 handle at offset 0 of the argument and call `self.syncobj_regs.orphan(target, handle)`. It must run before the reply is built. Hyprland imports the next timeline only after DESTROY returns, so no watch can then land on the recycled number.
3. **nvidia.rs `close_handle`:** when the kind is a DRI render handle, call `self.syncobj_regs.orphan_file(handle)`.
4. **Optional guest hardening:** in `nvgpu_fence_eventfd` (driver/nvgpu_fence.c:1212-1262), when the cookie came back as joined, keep the (handle, point, flags) on the sub. In `nvgpu_sowait_deliver`, re-poll that point before signalling, and re-arm if it is not ready. This bounds any future key-aliasing bug to a spurious re-poll instead of an early signal.
5. **Tests:**
   - watch (R, h, P); orphan (R, h); watch (R, h, P) again returns `New`, not `Joined`.
   - An orphan still holds a cap slot until its eventfd is written, and after that `sweep` frees the slot.

## S-14. [medium (low-medium today, because H-3 already leaves FLIP ungated; it becomes the main gap once H-3 and M-9 are fixed)] (cor-backend) NVKMS per-head and per-dpy gates are checked at prepare time, but the call runs later from the executor FIFO; a grant revoked in between is not seen
- Area: device/src/nvkms.rs State::check via Hooks::before; session.rs prepare_ioctl2; xfer.rs Prepared::run
- Evidence: hooks.before runs inside xfer::prepare (xfer.rs:411-415), under the backend mutex on the queue thread. The lease re-check that is supposed to catch lease ends also runs only there (session.rs:610-614, recheck_granting_leases, kms.rs:433-444). Modeset-class calls always go to an executor (session.rs:633; vhost-user-nvgpu.rs:812-826). The executor is a per-handle FIFO (exec.rs:81-88), so a gated call can wait behind earlier calls on the same modeset handle for as long as those take. Prepared::run (xfer.rs:1084-1088) issues the ioctl with no second policy check. Revocations that land in the window: close of the granting lease or card handle (nvidia.rs:1727 forget_handle → revoke_all_through), a lease end found later (kms.rs:458 lease_ended), and the LEASE uevent path (vhost-user-nvgpu.rs:1058-1063). NVKMS itself does not check permissions for SET_DPY_ATTRIBUTE (nvkms.c:3070-3085), SET_LAYER_POSITION (:3361-3374), MOVE_CURSOR (:2262-2285) or SET_CURSOR_IMAGE (:2230-2257), so the backend gates at nvkms.rs:484-495 are the only protection. This also affects the fixes for M-9 (the re-check still runs before the wait) and H-3 (a FLIP head-field gate added to `before` would have the same gap).
- Scenario: A guest process holds lease K for connector C/dpy D. It does nvidia-drm GRANT_PERMISSIONS(K→G) and NVKMS ACQUIRE_PERMISSIONS on modeset handle M, so perms[M] now include D. It then sends an IOCTL2 on M that takes long, such as VALIDATE_MODE or QUERY_DPY_DYNAMIC_DATA probing DDC or DP AUX, or any call queued on nvkms_lock behind the host compositor's SET_MODE. Right after, it sends SET_DPY_ATTRIBUTE(D, …) on M. prepare checks it: the lease still holds objects and D is granted, so the call is queued behind the slow one. The process then CLOSEs K. The lease ends, nvidia-drm revokes the grant, the backend's records are cleared, and Hyprland takes connector C back. The slow call finishes and the queued SET_DPY_ATTRIBUTE runs. The guest changes a display attribute (dithering, colour range, vibrance and so on) on the monitor now showing the host desktop. The whole sequence is under guest control, and the host lessor revoking the lease opens the same window.
- Fix: Re-check the gate at execution time, and hold it across the host ioctl so that a revocation the guest starts is strictly ordered after it.

1. **Revocation generation plus a run guard.** In `NvkmsPolicy`, add `gen: AtomicU64` and `run_guard: RwLock<()>`.
   - Every revocation path takes `run_guard.write()`, bumps `gen`, and clears the records. These paths are `forget_handle`, `revoke_all_through`, `forget_all_grants`, `lease_ended` and `revoke_dpy`.
   - `State::check` records `gen` into `Prepared` for gated entries: SET_CURSOR_IMAGE, MOVE_CURSOR, SET_LUT, SET_DPY_ATTRIBUTE, SET_LAYER_POSITION, and the FLIP/SET_MODE head fields once H-3 and M-9 land.

2. **New hook `Hooks::at_run(&self, p: &Prepared) -> Result<RunGuard, Errno>`.** Call it in `Prepared::run` (device/src/xfer.rs:1084) immediately before `sys.ioctl`. For gated NVKMS entries it does three things:
   - takes `run_guard.read()`;
   - returns EPERM if `gen` has moved;
   - holds the guard until after the ioctl returns.

3. **Order the close path.** In `close_handle` (device/src/nvidia.rs:1714-1731), call `nvkms.forget_handle` (write lock plus bump) before `drop(fd)`, as it already does. That way the host-side lease end is ordered after any in-flight gated call and before any later one. The executor never takes the backend mutex, so waiting on the write lock under that mutex cannot deadlock. The gated calls are short.

4. **Shrink the window for host-initiated revocation.** When the grant came from a DrmLease, `before` dup's the granting lease fd into `Prepared`. `at_run` then calls `kms::lease_holds_objects` on it (the GET_LEASE check, kms.rs:447-466) before the ioctl. This cuts the lessor-revocation window to microseconds; it cannot be closed fully because the host revokes asynchronously.

Tests: queue a gated call behind a blocking job on the same modeset handle, close the granting lease, then release the blocker. The gated call must return EPERM without reaching `sys.ioctl`.

## S-15. [medium] (cor-walk-wl) NV_ESC_EXPORT_TO_DMABUF_FD is forwarded raw: the host dma-buf fd leaks in the backend and its number is returned to the guest
- Area: RM frontend escapes (buffer export path)
- Evidence: Ours:
- 0xD9 is tagged `FdCarrying` in the ABI tables (gen/src/versions/v595_71_05.rs:69; also v580 and v535).
- It is missing from FD_CARRYING_IOCTLS (device/src/virtio.rs:121-127), so the guest sends it untranslated (driver/nvgpu_main.c:929-970, falling through to nvgpu_ioctl_simple).
- The backend's escape match has no arm for it and passes it to dispatch_simple (device/src/nvidia.rs:1983-2049).

Host:
- nv.c:2806-2813 calls nv_dma_buf_export.
- With fd == -1, the new dma-buf is installed in the caller's (the backend's) fd table and its number is written to `params->fd` (kernel-open/nvidia/nv-dmabuf.c:1683-1697).
- With fd >= 0, `dma_buf_get(params->fd)` resolves the number in the backend's table (nv-dmabuf.c:1738-1750).

This is the RM export path NVIDIA's own GBM uses (nvrm_gbm: NV2080_CTRL_CMD_DMABUF_EXPORT_OBJECTS_TO_FD, per research/hyprguest.md:220; the params mirror ctrl2080dmabuf.h:35-101). Not covered by M-6, which lists other escapes.
- Scenario: A guest process calls 0xD9 with fd = -1: NVIDIA GBM's RM path, CUDA `cuMemGetHandleForAddressRange(DMA_BUF)`, or a guest that took the RM path after the M-4 fix left supports_alloc = 0.
1. The backend gains a dma-buf fd that pins that guest's vidmem and is never closed. The memory outlives the guest process.
2. The guest gets a backend fd number. Used as a dma-buf (for example in `zwp_linux_buffer_params_v1.add`), it is EBADF, or it aliases an unrelated guest fd.
3. Repeated calls exhaust the backend's RLIMIT_NOFILE. After that, every OPEN, WlConn::open, PRIME export and adoption for the whole VM fails with EMFILE.
4. With fd >= 0, a guest can name any nv dma-buf fd the backend holds for another guest process and append objects to it.
- Fix: Immediate fix (backend, device/src/nvidia.rs, the escape match around lines 1983-2049):
- Add an arm `NV_ESC_EXPORT_TO_DMABUF_FD => return self.write_error_resp(resp_buf, Status::IoctlFailed, cookie, libc::EINVAL)`. It must apply regardless of AbiPolicy.
- Better, add a generic guard before the match: any escape whose table kind is IoctlKind::FdCarrying but has no entry in FD_CARRYING_IOCTLS and no dedicated arm is refused. A future FdCarrying escape then cannot fall into dispatch_simple.
- Add a unit test that sends 0xD9 and expects EINVAL.

Do not just add (0xD9, 0) to FD_CARRYING_IOCTLS. With fd == -1 the guest's fget-based translation would fail, and the output fd would still come back untranslated.

Proper translation, once there is a real consumer:
- Input: if fd >= 0, the guest resolves its dma-buf fd to a host handle of HandleKind::Dmabuf (hostfd.rs:37). The backend checks the handle's kind before substituting the host fd. fd == -1 passes through unchanged.
- Output: when status is NV_OK and fd == -1 was sent, the backend adopts the new params->fd into the handle table as Dmabuf. On handle-table EMFILE it closes the fd. It then returns the handle, never the raw number.
- Guest: turn the handle into a guest dma-buf the same way export mode does (DMABUF_IMPORT into the caller's render file, then nvgpu_dmabuf_from_host). Write that guest fd into params->fd before copy_to_user, and close the host handle if any step fails.

Hardening, separately: cap or account the dma-buf handles each guest file may hold.

## S-16. [low] (sec-parsers) Incoming Wayland blobs hold a host memfd each with no count limit (zero-length chunks bypass the byte cap)
- Area: wlwire/src/blob.rs (host side of the Wayland channel)
- Evidence: blob.rs:85-107 Blobs::chunk creates a memfd for every new id whose first chunk has off==0. The only limits are bytes: MAX_BLOB and MAX_PENDING (blob.rs:23, 101). A zero-length payload adds nothing to pending_bytes, and frame.rs Records accepts len==0 records. Entries stay in `incoming` until take() or the end of the connection. The same pattern exists for shm pools: one backend memfd per pool, where only the byte total is bounded (up to MAX_OBJECTS=1<<17 per connection, objects.rs:46).
- Scenario: The guest sends WL_SEND frames made of many BLOB records, each with a fresh guest-range id and an empty payload. Every record leaves an open memfd in the backend process. The backend's RLIMIT_NOFILE (default soft limit 1024; the backend never raises it) is used up. After that, PRIME export, host opens, adopting fds from the compositor, and export-socket accept() all fail with EMFILE, which disables the device for this guest and for export-mode peers that connect to this backend.
- Fix: 1. wlwire/src/blob.rs chunk():
- Reject empty payloads outright. The sender never emits them: send() uses data.chunks(), which yields nothing for an empty blob, and take() handles len==0 without chunks (blob.rs:116-120).
- Before calling sys::memfd, refuse to open a new id once incoming.len() reaches a small constant, e.g. MAX_INCOMING = 16, and return BlobError::TooBig.
- Optionally, drop unfinished incoming blobs whenever a WAYLAND record is processed with none of its BLOB descriptors pointing at them.

2. wlwire/src/shm.rs:
- Cap the number of pools per connection, e.g. 1024 pools, and check it next to may_grow at engine.rs:841.
- Keep a running byte total instead of the O(n^2) scan in may_grow.

3. The real fix for the whole class (this finding plus handle_table.rs:53 and nvidia.rs:1351), done once:
- At backend startup, raise the RLIMIT_NOFILE soft limit to the hard limit.
- Then keep one per-session fd budget, well below that limit, shared by the handle table, Wayland blobs, shm pools, streams and adopted compositor fds. Every fd-minting path answers -EMFILE (a Wayland ERR_NO_MEMORY on the channel) once the budget is used up.
- Set MAX_HANDLES from that budget, not the fixed 65536.

## S-17. [low] (sec-parsers) Every v1 RM_CONTROL/RM_ALLOC adds a counter-map entry keyed by the guest's value, so these maps grow without bound
- Area: device/src/nvidia.rs v1 RM escape path
- Evidence: nvidia.rs:2012 `*self.rm_controls.entry(cmd).or_insert(0) += 1` with cmd = the guest's NVOS54.cmd (any u32), and nvidia.rs:2026 `rm_classes.entry(class)` with any u32 hClass. Both run after check_abi, which only validates the escape and its 32/48-byte size, and before the host rejects the command. Nothing removes or limits entries. The same code exists on dev (baseline).
- Scenario: On an open nvidiactl handle, a guest issues well-sized RM_CONTROL escapes, each with a different cmd value. The host RM rejects each one cheaply, but the backend keeps one HashMap entry per distinct value. Over a long run, backend memory grows with no limit and eventually causes host memory pressure or OOM.
- Fix: At nvidia.rs:2009-2013 and 2023-2027, stop counting before the forward. Count after dispatch_nested returns, and only when the host RM status in the reply is NV_OK. The maps exist to show which commands the pipeline really uses (see the comment at nvidia.rs:386-394), so commands the host rejects are noise anyway. Also cap each map: if the key is not already present and map.len() >= 4096, add to a separate `rm_controls_overflow` / `rm_classes_overflow` u64 counter instead of inserting. In teardown (nvidia.rs:1051-1072), log the overflow counters, and print at most a bounded number of entries per line (or iterate and write in chunks) instead of building one Vec<String> and joining it.

## S-18. [low] (sec-parsers) Wayland channels (reader thread plus about 5 fds each) have no per-session limit
- Area: device/src/wl/serve.rs open_wayland, device/src/wl/conn.rs
- Evidence: serve.rs:227 open_wayland inserts a channel on every CONNECT or ACCEPT. conn.rs start() spawns a thread for each connection and holds sock, ready, wake and dup fds. Each connection can queue up to max_queue = 64 MiB (conn.rs:93) of undelivered records before it is hung up, and that queue stays allocated until the guest closes the handle. Closing spawns another thread (drop_detached).
- Scenario: A guest opens hundreds of channels and never calls WL_RECV. Each channel's compositor replies are queued up to 64 MiB of counted bytes, and per-Unit allocation overhead makes the real footprint larger. Backend memory and threads grow with the number of channels, bounded only by the fd limit. This compounds the shm finding above.
- Fix: 1. Add a per-session channel cap in WlState, for example const MAX_WL_CHANNELS: usize = 64, since a busy guest compositor proxies a few dozen clients. Check it at the top of open_wayland, before connect or accept_pending, with `if self.wl.chans.len() >= MAX_WL_CHANNELS { return Err(libc::EMFILE) }`, and log it once.

2. Add a session-wide queue budget. Put an Arc<AtomicUsize> in WlConfig (it is shared, like lease_cache), for example a 256 MiB budget per session:
   - collect() and fail() add u.bytes() to the counter.
   - recv() subtracts what frame::pack removed. Compute the before and after totals rather than re-summing.
   - Drop for WlConn subtracts whatever is still queued.
   - When the session total passes the budget, hang up the connection that is over its share, using the existing hangup(s, st, ENOBUFS) path.
   - Keep the per-connection 64 MiB limit, or lower it to 16 MiB, as a second guard.

3. Optionally, cap how much the reader thread drains beyond the compositor's own 1 MiB client buffer before it stops polling the socket for POLLIN. That returns backpressure to the compositor instead of piling data up in the backend.

The per-Unit accounting change is not needed: records are already batched (engine.rs:356-375).

## S-19. [low] (sec-desktop) A malicious guest can seize a leasable desktop monitor indefinitely, and churn leases to force repeated host modesets
- Area: Host compositor patch — patches/hyprland/0001-lease-desktop-outputs.patch, patches/aquamarine/0001-keep-leased-crtcs.patch
- Evidence: The patch offers a normal (desktop) monitor through wp_drm_lease_device_v1 whenever the admin sets leasable=true (CMonitor::setLeaseOffered, Monitor.cpp added by patch; MonitorRule m_leasable). On a lease request Hyprland fully releases the monitor to the lessee: CMonitor::releaseForLease() calls onDisconnect() (moves workspaces/focus away, disables the output) and CMonitorState::commit()/test() then refuse every commit while m_isBeingLeased (patch, Monitor.cpp CMonitorState::commit/test). The lease ends only when the client destroys the lease object, closes every copy of the lease fd, or the monitor is unplugged (patches/README.md 'What happens during a lease' step 5) — there is no timeout and no host-side reclaim. A guest that adopts the lease fd (device/src/wl/serve.rs TableRecv::adopt at :162-186, held as HandleKind::DrmLease) and never releases it holds the physical monitor away from the desktop for as long as the VM runs.
- Scenario: (a) Indefinite hold: guest leases the leasable monitor and keeps the lease fd open; the host user loses that monitor until the VM is killed. (b) Lease churn: guest repeatedly requests then drops the lease; each release runs a full onDisconnect(), and each onLeaseEnded() reconnects with a blocking full modeset (Monitor.cpp onLeaseEnded doLater→onConnect), so rapid lease/unlease cycles drive a modeset storm on the host desktop — visible flicker, workspace reshuffling, and stalls. Impact is bounded to monitors the admin explicitly marked leasable, which is why this is medium rather than high.
- Fix: 1) In our own proxy, limit how often a guest can take a lease. In the wlwire policy / device/src/wl/conn.rs, count wp_drm_lease_request_v1.submit per connection (and per backend). Refuse or delay a submit that comes within N seconds (for example 2-5 s) of the previous lease on that connection ending. A refusal can be answered with a synthesized wp_drm_lease_v1.finished, so a well-behaved guest compositor just retries later. This is the only fix that is under this project's control.
2) In the Hyprland patch, debounce the takeback. In CMonitor::onLeaseEnded (Monitor.cpp:2395), replace the immediate doLater(onConnect) with a short timer, and cancel it if a new lease on the same monitor arrives first. Also add a per-monitor cooldown in the CDRMLeaseResource constructor (DRMLease.cpp, before the releaseForLease loop at :69): reject with reject({}) when the monitor was taken back less than X ms ago. That caps the rate at one full modeset cycle per cooldown period.
3) Give the host a way to revoke, instead of adding a lease time limit. For a desktop (non-nonDesktop) monitor, have CDRMLeaseProtocol::withdraw (DRMLease.cpp:463-480), when reached via setLeaseOffered(false), also terminate() any active lease on that monitor. Alternatively add a hyprctl dispatcher such as `revokelease <output>`. Then setting `leasable = false` at runtime (or running the dispatcher) returns the monitor to the host. Document this in patches/README.md. Do not add a maximum lease duration, because it would break the intended use of giving a monitor to the VM.

## S-20. [low] (sec-dos) Guest-triggerable log flooding: no rate limiting anywhere in the backend, default level info, and the shipped launcher writes to an unrotated file on the host
- Area: logging (device/bin/vhost-user-nvgpu.rs, session.rs, nvkms.rs, wl/serve.rs, nvidia.rs); scripts/run-guest.sh
- Evidence: OURS: env_logger default_filter_or("info") (vhost-user-nvgpu.rs:1009). No ratelimit or once guard exists in device/ (grep finds nothing). Examples of one warn per request with no state and no host call: HELLO with proto != 2 (session.rs:350-356, new); a chain with fewer than 16 writable bytes (vhost-user-nvgpu.rs:791); an oversized chain (:776); each NVKMS refusal (nvkms.rs:250-252 plus the v1 wrapper at nvidia.rs:1911-1919); HOST_OP refusals (session.rs:416, 419); WL_RECV sizing errors (wl/serve.rs:395-408). scripts/run-guest.sh:48-49 redirects backend stderr to /root/logs/<tag>.backend.log with no size cap. The teardown also prints the guest-keyed RM_CONTROL/RM_ALLOC BTreeMaps (nvidia.rs:395-396, 2006-2027, 1051-1072) in one log line. They grow by one entry per distinct guest-chosen u32, with no cap.
- Scenario: A guest kernel posts 256-entry batches of HELLO{proto=7} (or chains with a 4-byte writable descriptor) in a loop. Each costs the backend a few microseconds and emits one ~100-byte warn line, on the order of 10 MB/s. Under run-guest.sh this fills the host root filesystem in hours, affecting the whole host and every VM. Under journald the per-unit rate limit drops lines, but journald still parses every one (host CPU). Separately, sending RM_CONTROL with 10^8 distinct cmd values grows rm_controls by GBs and makes teardown build and write a multi-GB single log line.
- Fix: (1) Add a small rate-limited logging macro in device/src, for example warn_ratelimited!(key, ...). It would keep a per-call-site token bucket (say 10 lines/s, burst 50) and emit "N similar messages suppressed" when the bucket refills. Use it at every refusal that depends on guest input: nvkms.rs:250 refuse(), nvidia.rs:1911, session.rs:352/416/419, wl/serve.rs:395/402, vhost-user-nvgpu.rs:776/791, and the dev-era per-request sites (nvidia.rs open failure, EXPORT_TO_FD/IMPORT_FROM_FD bad handle, unknown msg_type). Alternatively, demote them to debug and keep per-errno counters that are printed at teardown.
(2) Cap rm_classes/rm_controls (and msg_counts) at a fixed number of distinct keys, for example 4096. After that, add counts to an overflow counter. At teardown, print the top N entries by count plus "and K more (M calls)" instead of joining the whole map into one line.
(3) Optionally, for run-guest.sh (test-only), pipe through `head -c 256M` or systemd-cat. The more useful change is to document that production runs the backend under systemd/journald with RateLimitBurst and SystemMaxUse set.

## S-21. [low (pre-existing, not worsened; closer to informational)] (sec-dos) 1 ms level sweep polls every Legacy/Ready watch; its cost scales with guest-opened handles and can pin a host core per VM
- Area: device/src/pump.rs
- Evidence: pump.rs:36-39 and :72: SWEEP = 1 ms. step() uses a 1 ms epoll timeout whenever any swept watch exists (pump.rs:584-596). sweep() runs poll(fd,0) on every Legacy and non-oneshot Ready watch (pump.rs:789-800, readable() at :823-831). Every OPEN'd device handle gets a Legacy watch (pump.rs:78-81), and every Wayland channel adds another (serve.rs:289-293). The baseline had the same 1 ms sweep (dev:device/src/pump.rs:199); the display work adds more swept kinds. The only bound is the backend's NOFILE (and the 65536-entry handle table if NOFILE is raised).
- Scenario: A guest opens about 1000 /dev/nvidiactl or /dev/nvgpu-wl handles (default NOFILE) and leaves them idle. The pump makes about 10^6 poll() syscalls a second, roughly one host core burned continuously per VM. With a launcher that raises NOFILE toward 65536, the sweep never finishes within its period, the pump spins at 100%, and event latency for that VM degrades. It is confined to one core per VM, but it is host CPU the guest does not pay for.
- Fix: Stop sweeping every swept watch on every tick.

1. **Keep a dirty set.** In pump.rs, maintain a small `maybe_stale: HashSet<u32>`:
   - Add a handle in on_ready when a notification was queued but not delivered (flush got NoBuffer or Empty).
   - Add a handle in apply_commands when a new watch is armed.
   - sweep() polls only this set and removes a handle once poll reports it not readable.
   - Idle handles then cost nothing.
2. **Back off if a full sweep is kept.** Double the interval from 1 ms up to about 16 ms after each sweep that finds nothing. Reset it to 1 ms on any epoll event or queue kick.
3. **Optionally skip dead watches.** Do not arm Legacy watches for kinds whose host fd never becomes POLLIN in practice, such as render nodes and card files that no one watches with W_DRM. Check this per HandleKind before nvidia.rs:1358.
4. **Correct the claim.** Record the finding as pre-existing and cite the baseline sweep at dev:device/bin/vhost-user-nvgpu.rs:199,251-262.

## S-22. [low] (sec-posture) Any guest user can exhaust host memory through the Wayland proxy: 8 GiB of shm per channel, with no cap on the number of channels
- Area: backend / Wayland proxy (whole-host DoS), reachable by an unprivileged guest user
- Evidence: The guest node is world-writable: driver/nvgpu_wl.c:866 sets `wl->misc.mode = 0666`, and open/CONNECT (nvgpu_wl.c:735-751, 231-285) check no credentials and keep no per-device channel count. Each CONNECT becomes OPEN(DEV_WAYLAND). device/src/wl/serve.rs:227-247 then calls WlConn::open, which makes a fresh `UnixStream::connect(&cfg.socket)` to the host compositor (wl/conn.rs:213) and spawns a reader thread (conn.rs:290). The only bound is the handle table: MAX_HANDLES=65536 (handle_table.rs:53, insert at :98-101). The shm cap is per connection: wlwire/src/shm.rs:65-68 `MAX_POOL_BYTES = 8 << 30` ("Pool bytes one connection may have"), enforced by Shm::may_grow over that one connection's pools (shm.rs:124-140; engine.rs:841-846). The backend creates the pool as a host memfd of the guest-chosen size (engine.rs:847, sys.rs:28-32). Every SHM_SYNC record is pwrite()n into it at once, with no commit needed (shm.rs:268-283), so the pages become host shmem that belongs to no process's RSS. The per-connection to_guest queue is also 64 MiB (conn.rs:93). On the compositor side, wayland-server accepts clients without any limit (libwayland: src/wayland-server.c:1835-1848).
- Scenario: Precondition: the backend runs with --wayland-socket. A guest user with no privileges (for example `nobody` or a compromised guest service) runs 4 processes. Each opens /dev/nvgpu-wl, issues CONNECT, and speaks the frame protocol directly (the guest daemon is not needed). Each binds wl_shm, creates a pool of 8 GiB and eight 16384x16384 ARGB buffers in it, then streams SHM_SYNC records covering every byte. At virtqueue speed (GB/s) this takes a few seconds. The result is 32 GiB of host shmem held by memfds that both the backend and the host compositor reference. The host OOM killer runs and can kill the host compositor or unrelated desktop processes rather than the backend, since unmapped shmem is not charged to its RSS. Separately, about 200 channels (5 fds each: socket, ready and wake eventfds, the table dup and the pump dup) exhaust a default 1024 RLIMIT_NOFILE. After that every later host open fails (GPU opens, PRIME exports, sync_files) for every user in the VM.
- Fix: 1. Make the pool-byte budget VM-wide. Keep an Arc<AtomicU64> of pool bytes in WlState and pass it to every WlConn's Engine/Shm, including export-mode ACCEPT channels. Replace the total computed in Shm::may_grow (shm.rs:126-140) with a compare-and-add against that shared counter. Subtract from it in Shm::forget and on pool shrink or close. Default the budget to 1-2 GiB and make it configurable (--wayland-shm-max).

2. Cap channels per VM in open_wayland (serve.rs:227). Count the Chan::Conn/accepted entries in self.wl.chans the same way the modeset cap counts them (default 64-128), and return EMFILE past the cap. Optionally mirror the count with an atomic on struct nvgpu_wl_dev in nvgpu_wl_connect.

3. Fix the shared root cause of fd exhaustion. At backend start, raise RLIMIT_NOFILE to its hard limit. Then set the session HandleTable limit (handle_table.rs:53/85) to that value minus a reserve for the backend's own opens (GPU nodes, PRIME, sync_file), so a guest runs out of handles before the host process runs out of fds.

4. Document and ship a systemd unit or cgroup for the backend with MemoryMax/TasksMax. This bounds Wayland shmem, which is charged to the backend's memcg on pwrite, and the reader threads. It does not bound nvidia.ko GFP_KERNEL sysmem, which needs its own per-VM byte budget on RM system-memory allocations. Track that budget as a separate pre-existing issue.

## S-23. [low] (sec-posture) The default vhost-user socket /tmp/nvgpu.sock can be squatted by another local host user; the backend ignores a failed unlink
- Area: host control plane (whole-host posture)
- Evidence: device/bin/vhost-user-nvgpu.rs:86-87 defaults --socket to /tmp/nvgpu.sock. :1088 `let _ = std::fs::remove_file(&args.socket);` discards the error, then :1089-1091 serve() on the path. /tmp is sticky, so the backend cannot unlink another user's socket. scripts/run-guest.sh:22,48,57 points the VMM at the same fixed path.
- Scenario: On a shared host, user B binds /tmp/nvgpu.sock before the backend starts. The backend's unlink fails with EPERM (ignored) and bind fails with EADDRINUSE, so the backend exits. The VMM, started by run-guest.sh, then connects to B's socket and sends vhost-user SET_MEM_TABLE with the guest RAM fds. B can then read and write all guest memory, including the guest kernel.
- Fix: 1. Drop the /tmp default. Make --socket required, or default it to a private directory: $XDG_RUNTIME_DIR/nvgpu/<name>.sock for a user, or /run/nvgpu/ with mode 0700 for root. Create that directory with mode 0700 and refuse to start if it already exists and is owned by another uid or is group- or world-writable. Update the example at vhost-user-nvgpu.rs:12 to match.

2. Replace `let _ = remove_file` at :1088 with the export.rs:58 pattern: lstat the path, unlink it only if it is a socket owned by our euid, treat ENOENT as fine, and fail on anything else. Then call Listener::new(path, false) and daemon.start(listener) instead of serve(), so the crate does not do its own blind unlink. After bind, chmod the socket to 0600.

3. In run-guest.sh, create the socket inside `mktemp -d` (mode 0700) rather than using /tmp/nvgpu.sock. Before starting nesbox, check that $BACKEND is still alive (`kill -0 $BACKEND`) and that `stat -c %u "$SOCK"` equals `id -u`.

4. Best option: skip the filesystem path entirely. Have the launcher create a socketpair or an already-bound listener and hand it to the backend as an inherited fd (for example a --socket-fd flag built on Listener::from(UnixListener::from_raw_fd)).

## S-24. [low] (sec-posture) Any guest user can list the host PIDs of every GPU client on the host (other VMs' backends, the host compositor, host apps) through the unfiltered RM_CONTROL
- Area: information disclosure (RM_CONTROL)
- Evidence: RM_CONTROL is counted but not allowlisted (nvidia.rs:2007-2015). NV2080_CTRL_CMD_GPU_GET_PIDS (subdevice_ctrl_gpu_kernel.c:2300-2375) calls gpuGetProcWithObject, which walks every RM client and skips only kernel/internal admin clients (gpu_rmapi.c:855-873). It returns each client's ProcID, a host tgid. NV2080_CTRL_CMD_GPU_GET_PID_INFO then returns per-PID video memory usage. With MIG off there is no instance filter (subdevice_ctrl_gpu_kernel.c:2354-2365).
- Scenario: A guest user allocates a subdevice and calls GET_PIDS with idType CLASS and id NV01_DEVICE_0. It receives the host PIDs of Hyprland, every other VM's vhost-user-nvgpu, and host CUDA jobs, then polls GET_PID_INFO to watch their VRAM usage over time. That is a cross-VM activity side channel, and it reveals host PIDs useful for later attacks.
- Fix: 1. Primary fix, which covers every PID-reporting control at once: run each vhost-user-nvgpu backend in its own PID namespace. Either call unshare(CLONE_NEWPID) and fork before opening /dev/nvidiactl, or run it under systemd with PrivatePIDs=yes or bwrap --unshare-pid. The host driver then drops by itself every client outside that namespace (os_find_ns_pid returns 0, and gpu_rmapi.c:980-988 skips the client). GET_PIDS and GET_PID_INFO would then return only the backend's own PID. The kern_perf_ctrl.c per-PID samples would be filtered the same way.
2. Defence in depth, in device/src/nvidia.rs NV_ESC_RM_CONTROL (about line 2007): until a deny-by-default allowlist exists, return NV_ERR_INSUFFICIENT_PERMISSIONS for 0x2080018d (GPU_GET_PIDS), 0x2080018e (GPU_GET_PID_INFO) and the per-PID perf/utilization sample controls. Real workloads do not need them; only nvidia-smi-style monitoring uses them.
3. Longer term, build the RM_CONTROL allowlist from the rm_controls counts the backend already collects.

## S-25. [low] (cor-guest) GEM-in translations drop the proxy reference before the host call, so a concurrent last close can make the call act on a reused host GEM handle
- Area: driver/nvgpu_kms.c gem_in, driver/nvgpu_nvkms.c dma-buf fd_in, driver/nvgpu_drm.c nested GEM ioctls
- Evidence: Paths that drop the reference early:
- nvgpu_gem_to_host looks the proxy up, copies (host_handle, owner_handle), and drops the reference before returning (driver/nvgpu_drm.c:1067-1086).
- The KMS gem_in hook is exactly that call (driver/nvgpu_kms.c:1308-1313). nvgpu_i2_translate writes the numbers into the request (nvgpu_i2.c:549-561), and the request is only sent later (nvgpu_i2.c:1082).
- nvgpu_nvkms_dmabuf calls dma_buf_put(buf) (driver/nvgpu_nvkms.c:120) *before* HOST_OP PRIME_EXPORT(owner, gem) (nvkms.c:123-125).
- The nested GEM ioctl path (drm.c:1237-1245) has the same shape and already existed in the baseline (dev:driver/virtio_gpu_nv.c:2999).

Paths that hold the reference, as they should:
- Wayland SEND keeps the dma-buf until the host has answered (driver/nvgpu_wl.c:289-317, 474-479).
- SEMSURF_ATTACH keeps the proxy referenced across its call (fence.c:1677-1698).

Why a stale number is dangerous: nvgpu_gem_free sends GEM_CLOSE synchronously (drm.c:523-524), and host GEM handles are reused lowest-free (linux drm_gem.c:499).
- Scenario: 1. A process holds a dma-buf (or GEM handle) of a proxy P owned by another process V's render file: host handle H in V's file.
2. Thread 1 issues NVKMS REGISTER_SURFACE with that dma-buf. nvgpu_nvkms_dmabuf resolves it to (V, H) and drops its reference.
3. Thread 2 closes the process's last reference to P. nvgpu_gem_free runs GEM_CLOSE(V, H), and the call completes.
4. V allocates a new private buffer, and the host gives it H.
5. Thread 1's PRIME_EXPORT(V, H) now exports V's new buffer. The caller gets an NVKMS surface (which it can flip or scan out) on memory it was never given.

The same interleaving applies to ADDFB2 or SETCURSOR through the KMS gem_in hook, and to the nested GEM ioctls.
- Fix: Hold the proxy, or the dma-buf, until the host has finished with the numbers taken from it:
1. Add struct nvgpu_gem_object **held to the gem_in hook (or return the object). nvgpu_kms_gem_in uses nvgpu_gem_lookup() (nvgpu_drm.c:1088) instead of nvgpu_gem_to_host(), and stores the reference in the i2 state next to st->gem[]. nvgpu_i2_ioctl puts every held object after nvgpu_xfer returns, on the success path and in 'drop'.
2. On -ETIMEDOUT or -EINTR the request can still run on the host later. Hand the held references to the transport's orphan record, the same place that already owns req/resp and the consumed handles, and put them when the late reply arrives or the request is known dead.
3. In nvgpu_nvkms_dmabuf, keep buf in struct nvgpu_nvkms_call (like nc->event) and dma_buf_put it only after nvgpu_host_op(PRIME_EXPORT) has returned. The host dma-buf then pins the object, so the put can happen as soon as the op returns, even before the IOCTL2.
4. In nvgpu_ioctl_drm_gem_nested, replace nvgpu_gem_to_host with nvgpu_gem_lookup, keep ng until nvgpu_xfer completes (orphan it on timeout/EINTR), then drm_gem_object_put.
5. Optionally, add a single helper, nvgpu_gem_to_host_ref(), that returns the referenced object, so nothing new calls a variant that drops the reference early. Also document on nvgpu_gem_to_host that its output is valid only while the caller holds another reference.

## S-26. [low] (cor-guest) Device removal leaves open Wayland, DRM, GEM and fence objects pointing at the freed nvgpu_device
- Area: driver/nvgpu_main.c remove, nvgpu_wl.c, nvgpu_drm.c, nvgpu_fence.c
- Evidence: - The nvgpu_device is devm_kzalloc'd on vdev->dev (nvgpu_main.c:2565), so devres frees it right after nvgpu_remove returns (nvgpu_main.c:2818-2869).
- nvgpu_xfer_destroy frees dev->xfer and dev->events and sets them to NULL (xfer.c:1855-1872).
- nvgpu_wl.c:57-63 claims files opened before remove stay valid because the nvgpu_wl_dev is refcounted. But nvgpu_wl_release dereferences wf->wl->dev (nvgpu_wl.c:753-761: nvgpu_ev_unregister, nvgpu_fd_unregister, nvgpu_close_handle), and ioctl and poll use it too.
- DRM files survive drm_dev_unregister (drm.c:1787-1803), which has no drm_dev_unplug or drm_dev_enter guards. Their release, postclose, GEM free and ioctls use nfd->dev and ng->dev (drm.c:1474-1508, 508-543).
- Host fence proxies keep a module reference (fence.c:264) but no device reference. Their release, possibly long after removal, calls nvgpu_close_handle_async(f->dev) (fence.c:181), which reads dev->xfer from freed memory.
- Chardev nfds had the same problem in the baseline. The Wayland, fence and hostfile objects are new, and so is the explicit safety claim.
- Scenario: 1. Unbind the driver, through sysfs unbind or a virtio-pci hot-unplug, while nvgpu-wl-guest has /dev/nvgpu-wl open and a compositor holds a render file, GEM proxies and sync_files.
2. remove() completes and devres frees the nvgpu_device.
3. The daemon exits: nvgpu_wl_release reads wf->wl->dev->events and dev->fds_lock from freed memory. Separately, a sync_file close runs nvgpu_host_fence_release → nvgpu_queue_close(dev) → dev->xfer from freed memory.
4. The result is a guest-kernel use-after-free: an oops or memory corruption.
- Fix: 1. **Allocate the device with a reference count.** Replace `devm_kzalloc` (`nvgpu_main.c:2565`) with `kzalloc` plus a `kref` in `struct nvgpu_device`. Add `nvgpu_dev_get()` and `nvgpu_dev_put()`; the final put frees the struct.

2. **Take a reference in every object that stores `dev`:**
   - `nvgpu_fd` when it is created (chardev, DRM and Wayland), put in `nvgpu_fd_put` after the CLOSE;
   - `nvgpu_wl_file` in `nvgpu_wl_open`, put in `nvgpu_wl_release` after the close;
   - `nvgpu_host_fence` at `fence.c:260`.
   The host fence's release can run in IRQ context, so its put must not sleep: either make the final free RCU- or kfree-only, or hand the put to the close work item that `nvgpu_queue_close` already queues (it already holds a module reference). Also take a reference in the hostfile and each `nvgpu_close_work` item.

3. **Order the end of `nvgpu_remove`.** Set a `dev->dead` flag (or rely on `xf->dead`, which quiesce already sets) before `nvgpu_xfer_destroy`, and finish with `nvgpu_dev_put(dev)` instead of leaving the free to devres.

4. **Switch DRM teardown to the unplug model.** In `nvgpu_dri_cleanup`, call `drm_dev_unplug()` instead of `drm_dev_unregister()`. Wrap the forwarded DRM ioctl, mmap and fault paths in `drm_dev_enter()`/`drm_dev_exit()`. Leave postclose and GEM free unguarded; they only need the device reference, because they end in `nvgpu_close_handle`, which returns `-ENODEV` once `xfer` is NULL.

5. **Handle the unguarded `events` read.** Make `nvgpu_ev_new_cookie` and any other `dev->events` dereference check for NULL, or return `-ENODEV` at the Wayland and chardev ioctl entry when `dev->dead` is set.

6. **Fix the comment.** Correct `nvgpu_wl.c:57-63` so it states that the file pins the `nvgpu_device` too.

## S-27. [low] (cor-backend) Executor pool counts already-woken workers as idle, so a second file's job waits behind another file's blocking job
- Area: device/src/exec.rs (ExecPool::submit / worker)
- Evidence: exec.rs:89 spawns a thread only when `st.idle == 0 && st.threads < max_threads`. exec.rs:141-144: a worker leaves `idle` only after it wakes from `wake.wait` and takes the mutex back (`st.idle -= 1`). exec.rs:101 is `notify_one` after unlock. So between one submit's notify and the woken worker getting the lock, `idle` still counts that worker. A second submit in that gap spawns nothing, and its notify_one finds no waiter. The woken worker pops only one file (exec.rs:141-153) and runs that job to completion before it looks at `ready` again. The queue thread submits back to back: the process loop (vhost-user-nvgpu.rs:760-837) drains every available chain and calls pool.submit per IOCTL2 (:815) microseconds apart, which makes the gap wide. The test a_blocked_file_does_not_hold_up_another (exec.rs:203-211) starts from zero threads (idle==0, so each submit spawns) and never covers this case. The module's stated guarantee (exec.rs:13-19, 200-201) is 'one file stuck in a three-second commit does not hold up another file's work'. nvidia-drm blocking commits wait up to 3 s twice (nvidia-drm-modeset.c:765-778, 896-930, cited at exec.rs:6-8).
- Scenario: Start with the pool at 1 thread, idle. Compositor-VM guest: the compositor sends a blocking ATOMIC modeset on card handle K, and in the same ring batch a lease client sends a page-flip ATOMIC on lease handle L. submit(K): idle==1, so no spawn, and the worker is notified. submit(L): idle is still 1, so no spawn, and the notify is lost. The worker pops K and blocks for up to about 6 s (link training plus two 3 s waits). L sits in `ready` with 1 thread running and 15 allowed. It runs only when K's commit returns or some later IOCTL2 happens to reach submit with idle==0. The lease client (a VR headset, for example) misses every vblank in that window. The same happens with N idle workers and N+1 submits in one burst.
- Fix: In ExecPool::submit (exec.rs:89), decide whether to spawn by comparing waiting work with workers that can still take it, instead of looking at the raw idle count:

    if st.ready.len() > st.idle && st.threads < self.shared.max_threads { spawn }

Why this is correct:
- Every counted idle worker (waiting, or woken but not yet holding the lock) pops at most one `ready` file before it starts running a job.
- So whenever the ready files outnumber those workers, some file would otherwise be left unclaimed until a busy worker finishes, and a thread must be spawned.
- A spurious wakeup is harmless: the worker decrements `idle`, finds `ready` empty, and increments it again.
- The only cost is an occasional extra spawn when a busy worker would have picked the file up soon anyway. The pool is still capped at MAX_THREADS.

Do not use the other option in the finding (decrement `idle` in submit and skip the decrement after wait). A spurious wakeup would then increment `idle` a second time without anything decrementing it, so `idle` would over-count and the same stall would come back.

Add a regression test:
1. Create ExecPool::new(4), submit a trivial job on file 99, wait for it, and sleep briefly so exactly one worker is idle.
2. Submit a job on file 1 that blocks on a channel, then a job on file 2 that signals done, back to back.
3. Require file 2's done signal within a short timeout while file 1 is still blocked.
4. Loop steps 1-3 about 100 times with fresh pools, because the bug is a race.

## S-28. [low] (cor-backend) Lease-device probe: one failed probe hides the lease device for the life of the backend, and a probe in progress stalls the whole VM's backend
- Area: device/src/wl/probe.rs LeaseCache::is_ours; wl/conn.rs reader/send/recv
- Evidence: probe.rs:42-56 holds the LeaseCache `known` mutex across probe(). probe() does a blocking connect, a send retry of up to 1 s, and two until_done waits of 1 s each (probe.rs:32, 71-79, 91-131). On any error it permanently caches `false` for that name (`*known.entry(name).or_insert(false)`, probe.rs:55). The probe runs from Policy::offer (wlwire/src/policy.rs LeaseGate::Check), called by engine.from_local inside the reader thread while it holds the connection's State lock (conn.rs:525-557). WL_SEND and WL_RECV take the same State lock (conn.rs:309, 352) from serve_wl, which runs under the backend mutex (serve.rs:338-419; vhost-user-nvgpu.rs:740). serve.rs:90-97 acknowledges that the reader can sit in a probe for 'up to a couple of seconds' holding that lock, but only moves Drop off the mutex. Every other connection's reader that reaches a lease-device global also blocks on `known` while holding its own State lock.
- Scenario: (a) The host Hyprland is briefly busy (for example a modeset or a stalled output) when the guest's first Wayland client connects, so the probe times out after about 1 s. That global name is cached as 'not ours'. For the rest of the backend's life no guest client sees wp_drm_lease_device_v1, and leasing needs a backend restart. (b) During the same probe, nvgpu-wl-guest does WL_RECV on that channel, since the eventfd was signalled by the hello units. The queue thread takes the backend mutex and blocks on the connection lock for up to about 2-3 s. Every guest RM, UVM, CUDA and KMS request waits behind it.
- Fix: 1. In LeaseCache::is_ours (probe.rs:47-55), stop caching failures permanently. On Err, insert nothing, or insert a negative entry with an expiry, e.g. `HashMap<u32, (bool, Option<Instant>)>` where a failed probe expires after about 5-10 s. Return false for that call only. In probe(), put a name in the result only when its drm_fd event actually arrived. A device that sent no drm_fd (Hyprland DRMLease.cpp:331-334) should get a short-lived negative entry, not a permanent `false`.

2. Do not hold `known` across the socket I/O. Lock, look up the name, and if it is absent mark it in progress (Condvar or a per-name flag). Unlock, run probe(), then re-lock and insert, so concurrent readers wait on the flag and not on a mutex held during I/O.

3. For the documented stall (optional, already known): serve.rs wl_send and wl_recv could use `state.try_lock()` and return EAGAIN (the guest already handles EAGAIN from the backlog check at conn.rs:314-316). That way a probing reader never blocks the queue thread while it holds the backend mutex. The cleaner alternative is for the engine to defer only the lease-device global event: queue it, run the probe outside the State lock, then release it to the guest.

## S-29. [low] (cor-walk-wl) Guest daemon busy-loops and floods WL_SEND when a client hangs up while frames are queued for a busy host
- Area: nvgpu-wl-guest event loop
- Evidence: Ours:
- While `tx` is non-empty, sync() arms the client socket for EPOLLRDHUP only (nvgpu-wl-guest/src/daemon.rs:738-741). The epoll registration is level-triggered.
- turn() calls read_local on EPOLLHUP/EPOLLERR (daemon.rs:404-409).
- read_local returns at once while `!c.tx.is_empty()` (daemon.rs:608-610), so the hangup is never consumed and `closing` is never set.
- Every turn() also retries the queued frames (daemon.rs:433-442).
- The 5 ms back-off (daemon.rs:368-373) is bypassed because epoll_wait returns immediately.

Kernel: when a unix stream peer closes, it sets `sk_shutdown = SHUTDOWN_MASK` on our socket (linux net/unix/af_unix.c:704-707). unix_poll then reports EPOLLHUP unconditionally, and EPOLLHUP cannot be masked (af_unix.c:3373-3376).

Backend: EAGAIN comes back only while more than max_backlog (4 MiB) is unread by the compositor (device/src/wl/conn.rs:359-362).
- Scenario: 1. Host Hyprland stops reading one client for a while (stalled in a GPU wait, stopped in a debugger, and so on). The backlog passes 4 MiB and WL_SEND returns EAGAIN, so `tx` holds frames.
2. The guest client exits or crashes.
3. Every epoll_wait now returns EPOLLHUP at once. The daemon spins at 100% CPU.
4. On every iteration it re-issues SEND. Each SEND is a virtqueue round trip, repeats the dma-buf and syncobj resolution in the kernel (nvgpu_wl.c:410-442), and is served under the backend mutex. All other guest traffic for the VM is slowed until the compositor drains.
- Fix: In nvgpu-wl-guest/src/daemon.rs, add a `peer_gone: bool` to Client.

1. In turn()'s SUB_SOCK arm: if `events & (EPOLLHUP|EPOLLERR|EPOLLRDHUP) != 0` and `!c.tx.is_empty()`, set `peer_gone = true`. Then call `epoll_ctl(EPOLL_CTL_DEL, sock)` and set `sock_events = 0`. The DEL is required because EPOLLHUP/EPOLLERR cannot be masked through EPOLL_CTL_MOD. Leave the retry to the existing 5 ms timeout and the pump_tx loop at daemon.rs:433-442.

2. In sync(): if `peer_gone`, skip the socket re-arm. Once `tx` is empty, either call read_local directly to drain the remaining requests and reach the 0 byte EOF, which sets `closing`, or simply set `closing = true`.

A simpler alternative: on HUP/ERR with a non-empty `tx`, set `closing = true` at once and drop `tx`. The client can no longer read replies, and closing the channel hangs up the host-side client either way. Frames queued from a dead client are not worth keeping.

In either case, stop arming EPOLLRDHUP while `tx` is non-empty, or handle it the same way as HUP. This stops a half-closed but live client from spinning the loop. Add a test: fill the backlog with a stub that returns Busy, close the client socket, run turn() N times, and assert epoll_wait was not woken more than about N/5ms-worth of SEND attempts, or that the slot closes.

## S-30. [low] (cor-walk-wl) Lease-device probe runs under the connection lock and the global LeaseCache lock, and caches a timeout as 'not ours' forever
- Area: Wayland lease gating (--wayland-lease)
- Evidence: - The reader thread holds the connection's State lock while running the engine (device/src/wl/conn.rs:571-603). The engine's registry filter then calls LeaseGate::Check, which calls LeaseCache::is_ours (conn.rs:260-265).
- is_ours holds the process-wide `known` mutex across a synchronous probe (device/src/wl/probe.rs:42-56). The probe does a blocking connect and two round trips, each with a 1 s timeout (probe.rs:32, 68-82, 86-152, 157-218).
- On Err (timeout), `*known.entry(name).or_insert(false)` caches the name as not ours for the rest of the backend's life (probe.rs:53-55).
- WL_SEND and WL_RECV take the same connection lock (conn.rs:355, 398) from the dispatcher, which holds the backend mutex. serve.rs:90-97 acknowledges this for Drop only.
- Hyprland creates the lease global once per DRM device (hyprland/src/protocols/DRMLease.cpp:400), so its name never changes.
- Scenario: The guest starts while the host compositor is slow, for example at login or during a modeset. The first connection's probe times out after 1 s.
1. The lease-device global is hidden from every guest client until the backend restarts. VK_KHR_display and lease flows silently disappear.
2. While any probe runs, other connections' reader threads block on `known` while holding their own State lock. A WL_SEND or WL_RECV on those channels then stalls the backend mutex, and with it all of the VM's RM, UVM and KMS traffic, for up to about 2-3 s.
- Fix: 1. **Cache only definite answers.** In `LeaseCache::is_ours` (device/src/wl/probe.rs:42-56), insert only the entries returned by `Ok(found)`. On `Err`, return `false` for this call and leave `name` out of `known`. Also record a `retry_after = Instant::now() + backoff` (for example 5 s, doubling up to 60 s) so a hung compositor is not probed on every global.

2. **Never hold `known` across I/O.**
   - Check the map, drop the lock, then run the probe.
   - Use a separate "probe in flight" slot (a `Mutex<Option<Arc<(Mutex<Option<Result>>, Condvar)>>>`, or a `OnceLock` per generation) so concurrent connections share one probe instead of queueing on the map lock.
   - Re-take `known` only to insert the results.

3. **Take the probe off the connection lock.** Resolve lease names before the engine runs under `State`. In the reader loop (conn.rs:525-555), peek the incoming compositor bytes for `wl_registry.global` events for `wp_drm_lease_device_v1` whose names are not in `known`. Release the State lock, resolve them (share the in-flight probe; the timeout applies here, not under the backend mutex), then re-lock and call `from_local`. If the whole reader must not block, hold back just that `global` event and replay it after an asynchronous probe finishes.

4. **Clear the cache when the compositor changes.** Clear `known` whenever a connection sees the compositor go away, or key the cache by the compositor socket's inode plus the compositor's PID from SO_PEERCRED.

5. **Add a test** that points a probe at a listener that accepts but never answers. Assert that:
   - a later connection re-probes and sees the global;
   - a concurrent WL_SEND on another channel returns within a few milliseconds.

## S-31. [low] (cor-walk-wl) Every WL_RECV posts a max_frame (4 MiB with indirect descriptors) buffer: a fresh userspace allocation plus about 65 order-4 page allocations per call on the per-present path
- Area: Wayland channel transport performance
- Evidence: - With indirect descriptors the backend offers 4 MiB (device/bin/vhost-user-nvgpu.rs:865-883; device/src/session.rs:42). max_frame = min(max_req, max_resp) - 16 (driver/nvgpu_wl.c:95-100).
- The daemon calls recv(max_frame) and allocates `vec![0u8; max]` on every call (nvgpu-wl-guest/src/daemon.rs:683-690; nvgpu-wl-guest/src/channel.rs:151-166).
- The kernel allocates a `H + cap` tbuf per call (nvgpu_wl.c:596, 607), built from 64 KiB order-4 pieces (driver/nvgpu_xfer.c:62, 228-252). It is freed after copying only `flen` bytes (nvgpu_wl.c:705).
- `pending` is cleared before each RECV, so a wake usually costs two RECVs (nvgpu_wl.c:621-626).
- Scenario: A 144-240 Hz fullscreen client gets frame done, presentation feedback and buffer release every frame, which is at least 2 RECVs per frame.
- About 300-500 RECVs/s means roughly 1.2-2 GB/s of calloc/memset (glibc raises the mmap threshold after the first free) or mmap/munmap churn (musl) in the daemon.
- The same rate of order-4 alloc/free happens in the guest kernel. Under fragmentation this falls back to order-3 with direct reclaim, which puts latency spikes into the frame-callback path.
- This adds to the per-present costs tracked in L-7.
- Fix: 1. **Use a smaller receive size in the daemon.** In nvgpu-wl-guest/src/daemon.rs read_channel, pass something like `min(info.max_frame, max(frame::MIN_FRAME, 256 KiB))` instead of info.max_frame. MIN_FRAME already covers the largest record plus 32 descriptors (wlwire/src/frame.rs:72; driver/uapi/nvgpu_wl.h:130).
   - The backend already packs only what fits and sets F_MORE when more is waiting.
   - When F_MORE is set the kernel sets pending=1 again (nvgpu_wl.c:714), so poll wakes the daemon again even after the 64-iteration cap in read_channel.
   - This one change also shrinks the kernel's resp tbuf, because the driver sizes it as H + min(x.len, max_frame).
2. **Stop allocating a new buffer on every call.** In nvgpu-wl-guest/src/channel.rs, keep one receive buffer per Chan and reuse it; it does not need zeroing because the kernel writes x.len bytes. Hand the frame back as a slice, or copy only x.len bytes out, so the 4 MiB allocation and memset disappear.
3. **Optional, in the driver.** Cache one response tbuf per nvgpu_wl_file and reuse it while wf->lock is held. Drop it on release, and also on -ETIMEDOUT/-EINTR, where ownership passes to the late reply. This removes the per-call order-4 alloc/free churn entirely.

## S-32. [low (close to informational: the comment is wrong, but the client it describes cannot present anyway)] (cor-walk-wl) An unresolvable syncobj becomes a placeholder, and Hyprland answers it with a fatal INVALID_TIMELINE error, not a refused timeline
- Area: Wayland explicit sync
- Evidence: Ours:
- A syncobj fd that is not a host-handle file of this device is sent as DESC_F_INVALID (driver/nvgpu_wl.c:334-360). The comment at nvgpu_wl.c:331-332 and driver/uapi/nvgpu_wl.h:19-24 says the compositor "refuses that timeline, not the connection".
- The backend substitutes an empty memfd (wlwire/src/engine.rs:881-891).

Host: Hyprland's CSyncTimeline::create fails on that fd and raises `WP_LINUX_DRM_SYNCOBJ_MANAGER_V1_ERROR_INVALID_TIMELINE` (hyprland/src/protocols/DRMSyncobj.cpp:128-131). That is a wl_display.error, which disconnects the client.

The global is offered to every guest client whenever fences are on (wlwire/src/policy_table.rs:156; daemon.rs:320-322).
- Scenario: A guest client on another guest DRM device imports a timeline, for example a Mesa Vulkan driver on a second virtio-gpu, or a syncobj from any non-nvgpu node. Natively this works, because syncobj files are not tied to a device. Here the client's whole Wayland connection is killed on its first swapchain creation.
- Fix: 1. Fix the comments in driver/nvgpu_wl.c:326-332 and driver/uapi/nvgpu_wl.h:19-24 (and the DESIGN Wayland section). An unresolvable syncobj becomes a placeholder that Hyprland rejects with INVALID_TIMELINE, a protocol error that disconnects the client. That is unlike DMABUF, where `failed` is sent and the connection survives.
2. Cheap improvement: make it fail in the guest with a clear message, instead of a silent host-side disconnect. In the daemon's syncobj_out path, when the kernel reports the descriptor as F_INVALID, fail the import_timeline locally. Either have SEND return per-descriptor flags (the kernel already sets d->flags in nvgpu_wl_resolve_syncobj and could copy the table back), or add a pre-check ioctl. The daemon then posts a wl_display.error to the client itself, naming the cause ("timeline syncobj is not from the nvgpu device"), and never forwards the placeholder to the host.
3. Optional: log the rate-limited warning in nvgpu_wl.c:344-346 together with the client pid, so the cause shows in dmesg.
No change to the protocol or the global advertisement is needed. Hiding the global for foreign-device clients is not worth it, because those clients cannot present via dma-buf anyway.

## S-33. [low] (cor-walk-kms) Final close of host DRM/lease/card/modeset files happens on the event-pump or queue thread; the kernel teardown then runs blocking modesets there and stalls the whole VM
- Area: backend close path (device/src/nvidia.rs close_handle, device/src/pump.rs Unwatch)
- Evidence: Our side:
- device/src/nvidia.rs:1714-1730: close_handle pushes PumpCmd::Unwatch (1728) and then drops the table's fd on the queue thread (1730).
- The pump holds its own duplicate of every watched handle: KMS handles through WATCH (device/src/session.rs:394), and every Dev handle, /dev/nvidia-modeset included, from OPEN (device/src/nvidia.rs:1348).
- The pump drops that duplicate only when it later processes Unwatch (device/src/pump.rs:668-671). So the pump thread usually does the final close of lease, card and modeset files, and the task_work __fput runs in its context.
- Render handles are never watched (hostfd.rs:327-357), so their final close runs on the queue thread.
- DESIGN.md §2.5 says the queue thread never waits on a modeset lock or on nvkms_lock.

Host side, what the last fput runs:
- drm_file_free calls drm_fb_release (drm_file.c:253). A framebuffer still on a plane is removed by drm_mode_rmfb_work_fn/drm_framebuffer_remove under flush_work (drm_framebuffer.c:807-823, 383-396), which is a blocking atomic disable.
- drm_master_release (drm_file.c:264, drm_auth.c:351) calls nv_drm_master_drop, which does drm_modeset_lock_all, nv_drm_atomic_helper_disable_all and releaseOwnership (nvidia-drm-drv.c:1038-1068).
- nv_drm_postclose (nvidia-drm-drv.c:1588-1600) runs for every file, render files included. It calls nv_drm_revoke_modeset_permission, which takes DRM_MODESET_LOCK_ALL (1478) and commits a connector disable for grants made through the file (1502-1520).
- Closing a modeset file runs nvKmsClose under nvkms_lock.
- Scenario: B: a guest app exits or closes its lease fd while its framebuffer is on screen, which is the normal exit. The pump thread runs the host rmfb disable commit (at least one vblank, more if nvidia-drm's flip wait times out). While it does, EV_DRM, EV_FENCE and EV_READY stop for every file in the VM.

C: the lease that granted the head is closed. nv_drm_postclose commits the connector disable on the pump thread.

D: guest Hyprland exits. The final close of the DrmCard lands on the pump thread and runs master_drop's disable-all, which can take hundreds of ms with DP link teardown.

All three: any guest GL, Vulkan or GBM client closing a render node while a blocking ALLOW_MODESET commit holds the modeset locks blocks the queue thread in nv_drm_postclose's DRM_MODESET_LOCK_ALL. That commit can be B's first lessee modeset, or D's initial or VT-return restoreAfterVT modeset. Every RM ioctl, HOST_OP and IOCTL2 prepare of the VM then waits for the modeset to finish.
- Fix: 1. Add a dedicated closer thread, "nvgpu-closer", that receives descriptors over an mpsc channel and drops them there. Dropping a PrivateFd unregisters it on any thread, so that stays correct.
2. close_handle (nvidia.rs:1730): send the table's OwnedFd to the closer instead of calling drop(fd), for DriRender, DrmCard, DrmLease and Dev(Modeset). Descriptors that are cheap to close (eventfd, sync_file, RM Dev) can still close inline. For KMS-class handles with executor jobs pending, a better option is to submit the fd as a trailing job on that host file's ExecPool FIFO. That keeps native per-file ordering: the close lands after any in-flight commit on that file.
3. Pump: in the Unwatch and Reset arms (pump.rs:668-677), and wherever Watch replaces an old watch (pump.rs:647), send Watched.fd to the same closer instead of letting it drop inline. PumpHandle can carry a clone of the closer's Sender.
4. release_all and session_reset: route the table fds through the closer the same way, so a reset never closes under the backend mutex.
5. Add a test with a fake Sys/closer channel. For each HandleKind, assert that the last close never happens on the queue or pump thread and never while the backend mutex is held.
6. Update DESIGN.md §2.5 to state the rule: the last close of display-class files happens off the queue and pump threads.

## S-34. [low] (cor-walk-kms) HOST_OP OPEN_KMS and DROP_IF_MASTER run on the queue thread but can take nvkms_lock and blank every head
- Area: device/src/session.rs run_host_op
- Evidence: Our side:
- device/src/session.rs:446-449 states that every HOST_OP is non-blocking and runs on the queue thread.
- OpenKms calls open() on the host card (515-534). DropIfMaster calls hostfd::drop_master (536-537, hostfd.rs:584-586).
- The guest issues these ops from nvgpu_kms_open_card (driver/nvgpu_kms.c:389-425) on the first KMS ioctl of any primary file (nvgpu_kms_get_handle, 450-484).

Host side:
- Opening a card node while the host has no master becomes drm_master_open, then drm_new_set_master, then nv master_set, which calls nvKms->grabOwnership under nvkms_lock (nvidia-drm-drv.c:954-966).
- Dropping that master becomes nv_drm_master_drop, which runs drm_modeset_lock_all, a disable-all atomic commit and releaseOwnership (nvidia-drm-drv.c:1038-1068).
- Scenario: D: guest seatd has dropped master for a VT switch, and the hook has dropped host master too.

A guest process then issues its first KMS ioctl on a non-master card file. Examples: Hyprland's lease-device getNonMasterFD file, drm_info, or a Vulkan probe that does GET_CAP.

OPEN_KMS makes the new host file master, which grabs ownership under nvkms_lock. Another NVKMS client's SET_MODE or FLIP on an executor may be holding that lock. DROP_IF_MASTER then runs the lock-all and disable-all commit.

All of this runs on the queue thread, so every RM ioctl of the VM stalls behind it.
- Fix: Run OPEN_KMS and DROP_IF_MASTER on an executor instead of in run_host_op.
- Make them return an Outcome that queues a job, like the IOCTL2 path. Key the job on a per-card executor, for example a synthetic key derived from the card index, because the new handle does not exist yet.
- In the job, do open() and then the drop. Adopt the handle under the backend lock only when the job finishes, so a session reset can still cancel it.
- Better: add a single OPEN_KMS flag, "open non-master", so the open and the drop run in one executor job. The guest then never holds, even briefly, a host file that is master, and it saves a round trip.
- Correct the comment at session.rs:444-448. DROP_MASTER is not a short-lock op: nv_drm_master_drop takes mode_config.mutex, nvkms_lock (revokeSubOwnership) and plane or all modeset locks.
- Optionally, skip the drop when the host already has a master. drm_master_open only makes the file master when dev->master is NULL.


# Rejected
- (sec-posture) No sandbox exists, and the planned per-guest-process isolate cannot hold the new display paths, which assume one process with one VM-wide handle table
- (sec-posture) New host details disclosed to every guest user: host CLOCK_MONOTONIC (uptime) through /dev/nvgpu-wl HELLO, and host render/card dev_t numbers in every mode