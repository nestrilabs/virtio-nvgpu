// SPDX-License-Identifier: Apache-2.0
//! A vhost-user backend serving `NvidiaBackend` to a guest.
//!
//! The guest driver (`driver/nvgpu_main.c`) binds virtio device ID 45 and
//! posts one descriptor chain per request on the control queue: readable
//! descriptors holding the request, then writable ones for the response. The
//! event queue carries buffers the guest posts for the host to fill.
//!
//! Attach it to QEMU with the generic vhost-user device:
//!
//! ```text
//! qemu-system-x86_64 \
//!   -chardev socket,id=nv,path=$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock \
//!   -device vhost-user-device-pci,virtio-id=45,num_vqs=2,chardev=nv \
//!   -object memory-backend-memfd,id=mem,size=2G,share=on -numa node,memdev=mem
//! ```
//!
//! Guest memory must be shared (`memory-backend-memfd,share=on`) or the backend
//! cannot read the request the guest wrote.
//!
//! What the transport is responsible for, beyond moving bytes:
//!
//! - **Whole chains.** A request may span any number of readable descriptors
//!   and a response any number of writable ones -- the guest builds large
//!   buffers from page chunks -- so requests are gathered from all of them and
//!   responses scattered over all of them. Sizes are summed from the
//!   descriptors *before* anything is allocated: a guest-chosen length must
//!   never size an allocation here unchecked.
//! - **Completions after a reset.** Executor jobs finish whenever the host call
//!   returns, possibly seconds after the guest reset the device and the rings
//!   were reconfigured. Every ring carries an epoch, bumped by anything that
//!   stops or moves it, and a completion from an older epoch is dropped
//!   without touching guest memory -- its used-ring write would land in pages
//!   the rebooted guest has reused, or in the new ring as a head it never
//!   posted.
//! - **Who waits.** The queue thread never waits on a host display call; those
//!   run on per-file executors (`device::exec`), which complete their own
//!   chains.

#![forbid(unsafe_code)]

use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use clap::Parser;
use device::exec::ExecPool;
use device::host;
use device::nvidia::NvidiaBackend;
use device::posture;
use device::privfd;
use device::pump::{EventQueue, Fill, Pump, PumpCmd, PumpHandle};
use device::session::{
    BackendConfig, MAX_XFER_DIRECT, MAX_XFER_INDIRECT, Outcome, PendingIoctl2, Reply,
};
use device::shm::{WindowPlacer, ZoneConfig};
use device::virtio::{EVENT_QUEUE, NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV, VirtioGpuNvConfig};
use device::vring::{gather, layout, scatter};
use device::wl::export::WlExport;
use device::wl::{LeaseThrottle, WlConfig, WlLimits};
use protocol::messages::{MsgHeader, MsgType, SHM_ID_UVM};
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserShMemConfig,
    VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, VhostUserFrontendReqHandler};
use vhost_user_backend::{
    VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringState, VringStateGuard,
    VringStateMutGuard, VringT,
};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::{
    VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC,
};
use virtio_queue::{Error as VirtQueError, QueueOwnedT, QueueT};
use vm_memory::{
    GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend, GuestMemoryMmap,
    GuestMemoryRegion,
};

/// The driver calls `virtio_find_vqs(vdev, 2, ...)` and fails probe on the
/// error from that call, so offering fewer is fatal before config is read.
const QUEUE_COUNT: usize = NUM_QUEUES;

const HDR: usize = size_of::<MsgHeader>();

#[derive(Parser, Debug)]
#[command(version, about = "vhost-user backend for virtio-nvgpu")]
struct Args {
    /// Allow the diagnostic flags: those that take away a protection to find
    /// out whether it is what broke something, and are never for running a
    /// guest. Each is refused without this (or NVGPU_DIAGNOSTIC=1), and each
    /// in effect is announced on stderr and in the log at every start:
    /// `--allow-root-unsafe`, `--proc-nvidia`, `--permissive-abi`,
    /// `--keep-guest-coherency`, `--rm-allowlist=log`,
    /// `--sandbox=best-effort`, `--sandbox=off`,
    /// `--allow-unmeasured-release` and `--allow-inject-self` (see each with
    /// `--diagnostic --help`).
    #[arg(long)]
    diagnostic: bool,

    /// Unix socket the VMM connects to [default:
    /// $XDG_RUNTIME_DIR/nvgpu/nvgpu.sock].
    ///
    /// Whoever listens here receives the guest's memory, so the default is
    /// in a directory only this user can enter, and a file already at the
    /// path is removed only if it is this user's socket
    /// (device::posture).
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,

    /// Serve the vhost-user socket already listening on descriptor N,
    /// bound by whoever started the backend (root, as the rig launcher does)
    /// instead of `--socket`.
    ///
    /// Under systemd's socket activation (`LISTEN_FDS`, a descriptor named
    /// `vhost-user`, and one named `inject` for `--inject-uid`) nothing needs
    /// saying: the units in contrib/systemd do that. Either way the backend
    /// needs no directory of its own for the socket, and its user can neither
    /// replace the socket nor intercept the VMM's connection.
    #[arg(long, value_name = "N", conflicts_with = "socket")]
    socket_fd: Option<i32>,

    /// Start even as root or with CAP_SYS_ADMIN.
    ///
    /// RM, DRM and NVKMS take a guest's privilege from the backend's
    /// credentials: as root every guest process is an RM administrator with
    /// all of BAR0 mappable, which is the host kernel. Capabilities are
    /// dropped at startup either way; this only lets the start happen.
    /// Diagnostic.
    #[arg(long, hide = true)]
    allow_root_unsafe: bool,

    /// Where the host driver publishes itself. Overridable for testing
    /// against a fixture tree rather than a live driver. Diagnostic.
    #[arg(long, default_value = host::PROC_NVIDIA, hide = true)]
    proc_nvidia: PathBuf,

    /// Forward ioctls the ABI profile does not describe instead of refusing
    /// them, and report what was forwarded at teardown.
    ///
    /// For finding out what a workload needs that the tables lack. It hands a
    /// guest the parts of the host driver's interface nobody has checked, so it
    /// is not a way to run one. Diagnostic.
    #[arg(long, hide = true)]
    permissive_abi: bool,

    /// Start on a host driver release the tables were not all measured at
    /// (device::release), with the nearest older RM allowlist, ABI profile
    /// and NVKMS schema, and no UVM table (compute refused).
    ///
    /// For keeping a host running across a driver upgrade until the new
    /// release is measured (gen/README.md). The older tables may let
    /// through a control whose parameters the new release reads
    /// differently; that is what measuring it rules out. Diagnostic.
    #[arg(long, hide = true)]
    allow_unmeasured_release: bool,

    /// What to do with an RM control or class the host release's allowlist
    /// lacks: `enforce` refuses it before the host's RM sees it, as RM
    /// answers a call it does not implement; `log` logs it with RM's name
    /// for it and forwards it anyway.
    ///
    /// `log` is for finding out what a new workload needs (the teardown
    /// report lists everything it would have refused); like
    /// `--permissive-abi`, it hands a guest the parts of RM nobody has
    /// vetted, so it is not a way to run one: `log` is diagnostic.
    /// `--permissive-abi` does not change this gate.
    #[arg(long, value_name = "MODE", default_value = "enforce")]
    rm_allowlist: device::rmallow::Mode,

    /// Serve CUDA and other compute: `/dev/nvidia-uvm` (with UVM's
    /// multi-process sharing mode and the UVM aperture, where the VMM maps
    /// semaphore pools at guest-chosen addresses in its own address space),
    /// and memory the guest registers by its pages (RM pins guest RAM for the
    /// GPU, released by a list of holders read from one release's sources).
    ///
    /// Off by default. Vulkan, OpenGL, EGL, Vulkan Video and the display
    /// paths need none of it; without it the guest sees a host whose
    /// nvidia-uvm is not loaded, and CUDA finds no device (SECURITY.md,
    /// "Compute").
    #[arg(long)]
    allow_compute: bool,

    /// The shared window, in MiB: the guest-visible region device memory
    /// is mapped into, which bounds how much GPU memory this VM's
    /// processes can have CPU-mapped at once. A multiple of 64, at least
    /// 256; with `--allow-compute`, at most 63 GiB (it shares crosvm's
    /// 64 GiB region cap with the 1 GiB UVM aperture), else 64 GiB. nesbox
    /// takes at most 32 GiB.
    ///
    /// Growth goes mostly to the write-combining zone, where video memory
    /// is mapped: the uncached zone stays at 32 MiB, and write-combining
    /// and write-back split the rest 24:7 as in the default (16384: UC 32,
    /// WC 12660, WB 3692; DEPLOY.md, "Sizing the window"). The VMM sizes
    /// its region from the backend (GET_SHMEM_CONFIG). What is CPU-mapped
    /// also takes BAR1, which the host's desktop and other VMs share.
    #[arg(long, value_name = "MIB", default_value_t = ZoneConfig::DEFAULT_MIB)]
    window_size: u64,

    /// The percent of each window zone one guest process may hold,
    /// 1 to 95. At 50 (the default) two processes are needed to fill a
    /// zone down to its reserve; from 88, one can (SECURITY.md, "The
    /// window's size and share"). The reserve and floor keep the last of a
    /// zone for processes that hold little of it either way.
    #[arg(long, value_name = "PERCENT", default_value_t = ZoneConfig::DEFAULT_OWNER_PERCENT)]
    window_owner_share: u8,

    /// Log the frame-pacing counters (device::pacing: message rates, the
    /// backend's service time per message type, how the guest's waits went,
    /// how long events took to reach the event queue) every SECS seconds
    /// while the guest is busy. They are always kept, and logged once at
    /// teardown; NVGPU_PACING_STATS=SECS does the same as this.
    #[arg(long, value_name = "SECS")]
    pacing_stats: Option<u64>,

    /// Microseconds the queue thread keeps looking at the control ring after
    /// draining it, before it asks the guest to kick again (0: not at all).
    /// A guest process presenting a frame sends a dozen requests one after
    /// another; one that arrives while the thread still looks costs the
    /// guest no kick (a VM exit) and this thread no wakeup. The price is
    /// that much CPU after each burst, never while the guest is idle: at
    /// most a core's worth under a guest that never stops sending. With the
    /// guest's own reply spin it cost less host CPU in all than the kicks
    /// and halt polling it replaces (DEPLOY.md, "Frame pacing"). At most
    /// 1000.
    #[arg(long, value_name = "US", default_value_t = 50)]
    queue_poll_us: u64,

    /// The EEVDF slice of every backend thread, in microseconds (100 to
    /// 100000; 0 keeps the slice the process started with: the host's
    /// default, about 3 ms, or the launcher's). A shorter slice
    /// gets the queue thread and the event pump back onto a busy CPU sooner
    /// after they wake, at the same share of the CPU: on a loaded host it
    /// is what keeps a guest's frames on their vblanks (DEPLOY.md, "Frame
    /// pacing"). Set at start, before any thread exists, so all inherit it;
    /// needs no privilege, and a kernel before 6.12 keeps its default. The
    /// policy stays as the launcher set it (SCHED_BATCH, SCHED_IDLE), and a
    /// real-time one is left alone; 0 leaves whatever slice the launcher
    /// gave (a launcher that sets one should pass it here too, as
    /// rig/run-guest.sh passes NVGPU_SLICE_US).
    #[arg(long, value_name = "US", default_value_t = 100)]
    sched_slice_us: u64,

    /// Allocate guest system memory with the coherency the guest asks for,
    /// instead of GPU-coherent (write-back, snooped).
    ///
    /// For ruling the rewrite out when chasing a problem. On an Intel host
    /// KVM maps guest RAM write-back whatever the guest asks, unless the VMM
    /// disables KVM_X86_QUIRK_IGNORE_GUEST_PAT, so with this set a guest there
    /// caches memory the GPU does not snoop. Diagnostic.
    #[arg(long, hide = true)]
    keep_guest_coherency: bool,

    /// A directory holding each GPU's whole PCI config space, one file per
    /// GPU named by its PCI address (`0000:01:00.0`), as root read it from
    /// `/sys/bus/pci/devices/<addr>/config`. The backend, unprivileged, gets
    /// only the first 64 bytes of that file, so the guest's device has no
    /// capability list without it (nvidia-smi cannot report the PCIe link).
    /// Read once at start, before the sandbox; a file of another device, or
    /// past 4 KiB, is ignored.
    #[arg(long, value_name = "DIR")]
    pci_config_dir: Option<PathBuf>,

    /// The process sandbox (device::sandbox): a network namespace of its
    /// own, Landlock confining it to the GPU's nodes and what it reads at
    /// run time, a seccomp syscall allowlist, RLIMIT_CORE 0. Installed before
    /// the first guest message; with `on` a layer the host kernel lacks, or
    /// has only in part, stops the start.
    ///
    /// `best-effort` runs with the layers the kernel has, each missing one
    /// reported as DEGRADED: for a host being brought up to the sandbox.
    /// `off` is for finding out whether the sandbox is what broke something,
    /// never for running a guest: a backend taken over is then everything
    /// its uid is. Both are diagnostic.
    #[arg(long, value_name = "on|best-effort|off", default_value_t = device::sandbox::Mode::On)]
    sandbox: device::sandbox::Mode,

    /// Compositor-VM mode: offer the host's card nodes to the guest, so a
    /// guest compositor can drive the display.
    ///
    /// Only for a host with no compositor of its own. A guest file that
    /// becomes guest DRM master makes its host card file host master, and a
    /// host compositor holding the card first would simply see its commits
    /// fail.
    #[arg(long)]
    kms_card: bool,

    /// The host compositor's Wayland socket, for the Wayland proxy
    /// (typically `$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY`).
    #[arg(long, value_name = "PATH")]
    wayland_socket: Option<PathBuf>,

    /// Offer the compositor's `wp_drm_lease_device_v1` to guest clients, so
    /// a guest can lease a host output and drive it through its own KMS.
    ///
    /// Only a lease device whose DRM file is one of this GPU's card nodes is
    /// ever shown, and only to a guest that can adopt DRM files.
    #[arg(long, requires = "wayland_socket")]
    wayland_lease: bool,

    /// Seconds between the lease requests one VM may submit, on average
    /// (three may go at once). A lease of a desktop monitor makes the
    /// compositor modeset it away and back; this keeps a guest from doing
    /// that in a loop. A request past the rate waits, it is not refused. 0
    /// lifts the limit.
    #[arg(long, value_name = "SECS", default_value_t = LeaseThrottle::DEFAULT_INTERVAL.as_secs())]
    wayland_lease_interval: u64,

    /// Accept host Wayland clients here and carry them to a guest compositor.
    #[arg(long, value_name = "PATH")]
    wayland_export: Option<PathBuf>,

    /// Accept host buffers to inject into the guest here, from a capture
    /// helper running as `--inject-uid` (SECURITY.md, "Capture injection").
    ///
    /// A SOCK_SEQPACKET socket, bound 0600 in a private directory and
    /// renamed into place; open it to the helper's group once it exists.
    /// The systemd units bind it instead (contrib/systemd,
    /// vhost-user-nvgpu-inject@.socket), open to that group from the start,
    /// and hand it over: then give `--inject-uid` alone. Each dma-buf the helper sends
    /// must be nvidia-drm memory of this GPU; a guest process that knows the
    /// id and token IMPORT answered opens it read-only for the CPU. Off by
    /// default: without it the guest has no /dev/nvgpu-capture.
    #[arg(long, value_name = "PATH", requires = "inject_uid")]
    inject_socket: Option<PathBuf>,

    /// The only uid whose connections to `--inject-socket` (or to the
    /// socket-activated `inject` socket) are served (SO_PEERCRED): the VM's
    /// capture helper, a user of its own.
    #[arg(long, value_name = "UID")]
    inject_uid: Option<u32>,

    /// Let `--inject-uid` be the backend's own uid: every process of the
    /// backend's user may then inject into this VM, which is how the rig
    /// runs (one user for everything), never a deployment. Diagnostic.
    #[arg(long, hide = true, requires = "inject_uid")]
    allow_inject_self: bool,

    /// Wayland channels one VM may have open at once (guest clients, or
    /// accepted host clients in export mode). Each is a client of the host
    /// compositor with a thread and a few descriptors here; past the limit
    /// the guest's CONNECT fails with EMFILE. One guest process may have a
    /// quarter of them, and the last eighth only while it has at most two.
    #[arg(long, value_name = "N", default_value_t = WlLimits::DEFAULT_MAX_CONNS)]
    wayland_max_conns: usize,

    /// MiB of wl_shm buffer memory one VM's clients may have the backend hold,
    /// over all its connections (each connection is also held to 512 MiB):
    /// what their live buffers cover, not how large their pools are. The
    /// pages are memfds the host OOM killer does not count as this process's;
    /// a buffer past the budget is a wl_display.error for its client. One
    /// guest process's connections may hold a quarter of it, with the last
    /// sixteenth kept for processes that hold little.
    #[arg(long, value_name = "MIB", default_value_t = WlLimits::DEFAULT_SHM_BYTES >> 20)]
    wayland_shm_budget: u64,

    /// MiB of compositor output one VM may leave unread, over all its
    /// connections (each is also held to 64 MiB), and one guest process's
    /// connections half of it; the connection that passes it is dropped.
    #[arg(long, value_name = "MIB", default_value_t = WlLimits::DEFAULT_QUEUE_BYTES >> 20)]
    wayland_queue_budget: usize,
}

/// Places device memory through the vhost-user backend request channel.
///
/// The VMM owns the window's address space and the memory slot that describes
/// it, so it is the only process whose `MAP_FIXED` the guest can see. This
/// hands the descriptor over and lets it do the placement.
struct VhostWindow(Backend);

impl WindowPlacer for VhostWindow {
    fn place(
        &self,
        shm_offset: u64,
        len: u64,
        fd: RawFd,
        fd_offset: u64,
        writable: bool,
    ) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset,
            shm_offset,
            len,
            flags: if writable {
                VhostUserMMapFlags::WRITABLE.bits()
            } else {
                0
            },
        };
        // The descriptor is owned by the handle table for the whole of this
        // call, and is only sent by number.
        self.0
            .shmem_map(&req, &fd)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw(&self, shm_offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset,
            len,
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    /// The same request on the UVM aperture (shm id 2), with the pool's base
    /// as the file offset: the VMM maps the file there, at that host address
    /// (UVM takes no other), and gives it a memory slot at `aperture_offset`.
    /// The VMM keeps no descriptor; its mapping holds the file.
    fn place_uvm(
        &self,
        aperture_offset: u64,
        len: u64,
        fd: RawFd,
        addr: u64,
    ) -> device::error::Result<()> {
        let req = uvm_mmap_msg(aperture_offset, len, addr);
        // As in `place`: the handle table owns the descriptor for the whole
        // of this call.
        self.0
            .shmem_map(&req, &fd)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    /// The VMM removes the memory slot, then its mapping, before it answers.
    fn withdraw_uvm(&self, aperture_offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: SHM_ID_UVM,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: aperture_offset,
            len,
            flags: 0,
        };
        self.0
            .shmem_unmap(&req)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }
}

/// SHMEM_MAP of a UVM pool: region 2, file offset = host address = `addr`.
fn uvm_mmap_msg(aperture_offset: u64, len: u64, addr: u64) -> VhostUserMMap {
    VhostUserMMap {
        shmid: SHM_ID_UVM,
        padding: [0; 7],
        fd_offset: addr,
        shm_offset: aperture_offset,
        len,
        flags: VhostUserMMapFlags::WRITABLE.bits(),
    }
}

/// The shared-memory id the guest driver looks the window up by, which must
/// match the capability the VMM publishes.
const NV_SHM_ID: u8 = 1;

/// The shared memory regions this device has, for the VMM (GET_SHMEM_CONFIG;
/// crosvm sizes its BAR by it, and nesbox from virtio-nvgpu-v4 its window):
/// the window, region 1, as large as the allocator's zones -- the same
/// `ZoneConfig` the allocator was made from -- and with `--allow-compute`
/// the UVM aperture, region 2, as large as the backend will place pools in.
/// Region 0 is the "undefined" id a guest discards, so it is never used.
fn shmem_config(window: &ZoneConfig, allow_compute: bool) -> VhostUserShMemConfig {
    let mut sizes = [0u64; 3];
    sizes[usize::from(NV_SHM_ID)] = window.total();
    let mut n = 1;
    if allow_compute {
        sizes[usize::from(SHM_ID_UVM)] = device::uvmmap::APERTURE_MAX;
        n += 1;
    }
    VhostUserShMemConfig::new(n, &sizes)
}

/// The Wayland proxy's configuration, made once at startup.
struct Wayland {
    /// `--wayland-socket`: one `WlConfig` cloned into every connection, so
    /// what the lease-device probe learns about the compositor's globals is
    /// learnt once and shared.
    cfg: Option<WlConfig>,
    /// `--wayland-export`: the listener and its readiness eventfd.
    export: Option<(Arc<WlExport>, OwnedFd)>,
    /// `--wayland-max-conns` and the budgets: one set for the VM.
    limits: WlLimits,
}

impl Wayland {
    fn from_args(args: &Args) -> anyhow::Result<Self> {
        let cfg = args.wayland_socket.as_ref().map(|p| {
            let mut c = WlConfig::new(p);
            c.allow_lease = args.wayland_lease;
            log::info!(
                "wayland: guest clients reach the compositor at {}{}",
                p.display(),
                if c.allow_lease {
                    ", leases offered"
                } else {
                    ""
                }
            );
            c
        });
        // Bound now rather than on the guest's first LISTEN: a path that cannot
        // be listened on is a configuration error the operator should see at
        // start, not a guest -ENODEV later.
        let export = match &args.wayland_export {
            Some(p) => {
                // Bound here, before the sandbox leaves no directory to make a
                // socket in; its thread starts after (`start_export`).
                let x = WlExport::bind_idle(p).map_err(|e| {
                    anyhow::anyhow!("--wayland-export {}: cannot listen: {e}", p.display())
                })?;
                log::info!("wayland export: host clients connect at {}", p.display());
                Some(x)
            }
            None => None,
        };
        let limits = WlLimits::new(
            args.wayland_max_conns,
            args.wayland_shm_budget.saturating_mul(1 << 20),
            args.wayland_queue_budget.saturating_mul(1 << 20),
        )
        .with_lease_rate(
            std::time::Duration::from_secs(args.wayland_lease_interval),
            LeaseThrottle::DEFAULT_BURST,
        );
        if cfg.is_some() || export.is_some() {
            log::info!(
                "wayland: at most {} channels, {} MiB of shm and {} MiB unread per VM",
                args.wayland_max_conns,
                args.wayland_shm_budget,
                args.wayland_queue_budget
            );
        }
        Ok(Self {
            cfg,
            export,
            limits,
        })
    }
}

/// `WlExport::shutdown` at exit.
struct ExportGuard(Option<Arc<WlExport>>);

impl Drop for ExportGuard {
    fn drop(&mut self) {
        if let Some(x) = self.0.take() {
            x.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// Rings with an epoch
// ---------------------------------------------------------------------------

/// A vring that counts the times it was stopped or moved.
///
/// vhost-user stops a ring with GET_VRING_BASE, which only clears `ready`
/// (vhost-user-backend 0.23.0, src/handler.rs `get_vring_base`, :446-465),
/// and virtio-queue's `add_used` does not look at `ready` at all
/// (virtio-queue 0.18.0, src/queue.rs, :441-477); after SET_VRING_ADDR the
/// used index is reloaded from the guest (`set_vring_addr`, handler.rs
/// :386-432). So a
/// completion arriving late has nothing to stop it writing a stale head into
/// whatever ring is there now. The epoch is that stop: every state change
/// bumps it under the ring's own write lock, and a completion compares it
/// under the same lock before writing anything.
#[derive(Clone)]
struct EpochVring<M: GuestAddressSpace = GuestMemoryAtomic<GuestMemoryMmap>> {
    inner: VringRwLock<M>,
    epoch: Arc<AtomicU64>,
    /// The kick, call and error eventfds the ring holds, by number, so each
    /// can be registered as the backend's own while it is open (`privfd`):
    /// they are in no handle table, and an IOCTL2 must never adopt one.
    eventfds: Arc<Mutex<[Option<RawFd>; 3]>>,
}

const KICK: usize = 0;
const CALL: usize = 1;
const ERR: usize = 2;

impl<M: GuestAddressSpace + 'static> EpochVring<M> {
    /// The current epoch. Read it while holding the ring's lock to tie it to
    /// the state the lock protects.
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Apply a state change and bump the epoch, both under the write lock.
    fn change(&self, f: impl FnOnce(&mut VringState<M>)) {
        let mut g = self.inner.get_mut();
        f(&mut *g);
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// Hand the ring a new eventfd for `slot`, keeping the private registry
    /// exact: the new number is registered before the ring holds it, and the
    /// old one unregistered only after the ring has dropped (closed) it -- a
    /// moment where a closed number is still registered costs at most one
    /// refused adoption, while the other order would leave an open one
    /// unregistered.
    fn swap_eventfd(&self, slot: usize, file: Option<File>, set: impl FnOnce(Option<File>)) {
        let new = file.as_ref().map(|f| f.as_raw_fd());
        if let Some(fd) = new {
            privfd::register(fd);
        }
        let mut fds = self.eventfds.lock().unwrap();
        set(file);
        if let Some(old) = std::mem::replace(&mut fds[slot], new)
            && Some(old) != new
        {
            privfd::unregister(old);
        }
    }
}

impl<'a, M: 'a + GuestAddressSpace> VringStateGuard<'a, M> for EpochVring<M> {
    type G = RwLockReadGuard<'a, VringState<M>>;
}

impl<'a, M: 'a + GuestAddressSpace> VringStateMutGuard<'a, M> for EpochVring<M> {
    type G = RwLockWriteGuard<'a, VringState<M>>;
}

impl<M: 'static + GuestAddressSpace> VringT<M> for EpochVring<M> {
    fn new(mem: M, max_queue_size: u16) -> Result<Self, VirtQueError> {
        Ok(Self {
            inner: VringRwLock::new(mem, max_queue_size)?,
            epoch: Arc::new(AtomicU64::new(0)),
            eventfds: Arc::default(),
        })
    }

    fn get_ref(&self) -> <Self as VringStateGuard<'_, M>>::G {
        self.inner.get_ref()
    }

    fn get_mut(&self) -> <Self as VringStateMutGuard<'_, M>>::G {
        self.inner.get_mut()
    }

    fn add_used(&self, desc_index: u16, len: u32) -> Result<(), VirtQueError> {
        self.inner.add_used(desc_index, len)
    }

    fn signal_used_queue(&self) -> std::io::Result<()> {
        self.inner.signal_used_queue()
    }

    fn enable_notification(&self) -> Result<bool, VirtQueError> {
        self.inner.enable_notification()
    }

    fn disable_notification(&self) -> Result<(), VirtQueError> {
        self.inner.disable_notification()
    }

    fn needs_notification(&self) -> Result<bool, VirtQueError> {
        self.inner.needs_notification()
    }

    fn set_enabled(&self, enabled: bool) {
        self.change(|s| s.set_enabled(enabled));
    }

    fn set_queue_info(
        &self,
        desc_table: u64,
        avail_ring: u64,
        used_ring: u64,
    ) -> Result<(), VirtQueError> {
        let mut res = Ok(());
        self.change(|s| res = s.set_queue_info(desc_table, avail_ring, used_ring));
        res
    }

    fn queue_next_avail(&self) -> u16 {
        self.inner.queue_next_avail()
    }

    fn set_queue_next_avail(&self, base: u16) {
        self.change(|s| s.get_queue_mut().set_next_avail(base));
    }

    fn set_queue_next_used(&self, idx: u16) {
        self.change(|s| s.get_queue_mut().set_next_used(idx));
    }

    fn queue_used_idx(&self) -> Result<u16, VirtQueError> {
        self.inner.queue_used_idx()
    }

    fn set_queue_size(&self, num: u16) {
        self.change(|s| s.get_queue_mut().set_size(num));
    }

    fn set_queue_event_idx(&self, enabled: bool) {
        self.inner.set_queue_event_idx(enabled);
    }

    fn set_queue_ready(&self, ready: bool) {
        self.change(|s| s.get_queue_mut().set_ready(ready));
    }

    fn set_kick(&self, file: Option<File>) {
        self.swap_eventfd(KICK, file, |f| self.inner.set_kick(f));
    }

    fn read_kick(&self) -> std::io::Result<bool> {
        self.inner.read_kick()
    }

    fn set_call(&self, file: Option<File>) {
        self.swap_eventfd(CALL, file, |f| self.inner.set_call(f));
    }

    fn set_err(&self, file: Option<File>) {
        self.swap_eventfd(ERR, file, |f| self.inner.set_err(f));
    }
}

type Vring = EpochVring<GuestMemoryAtomic<GuestMemoryMmap>>;

// ---------------------------------------------------------------------------
// Gather and scatter
// ---------------------------------------------------------------------------

/// A chain taken off the control queue, as much as completing it needs.
struct Taken {
    head: u16,
    epoch: u64,
    writable: Vec<(GuestAddress, u32)>,
    /// Total writable bytes.
    cap: usize,
}

/// A bare error header for a request the transport refuses on its own.
fn transport_error(errno: i32) -> Reply {
    Reply::error(MsgType::Ioctl, 0, errno)
}

// ---------------------------------------------------------------------------
// The shared state jobs complete against
// ---------------------------------------------------------------------------

/// What an executor job needs to finish a chain from its own thread.
struct Shared {
    nvidia: Mutex<NvidiaBackend>,
    mem: RwLock<Option<GuestMemoryAtomic<GuestMemoryMmap>>>,
    pool: ExecPool,
    pump: Mutex<PumpState>,
}

/// The event pump once running, and what was said to it before it was.
#[derive(Default)]
struct PumpState {
    handle: Option<PumpHandle>,
    queued: Vec<PumpCmd>,
}

impl Shared {
    /// Forward the backend's pump instructions, in order. Before the pump has
    /// started they wait: it cannot start until guest memory and the event
    /// queue are known, and a watch made by the first OPEN must not be lost.
    fn forward(&self, cmds: Vec<PumpCmd>) {
        if cmds.is_empty() {
            return;
        }
        let p = self.pump.lock().unwrap();
        Self::hand_over(p, cmds);
    }

    fn hand_over(mut p: MutexGuard<'_, PumpState>, cmds: Vec<PumpCmd>) {
        match p.handle.as_ref() {
            Some(h) => cmds.into_iter().for_each(|c| h.send(c)),
            None => p.queued.extend(cmds),
        }
    }

    /// Forward what the backend `be` has for the pump, and let go of it.
    /// The pump's lock is taken before the backend's is let go: whichever
    /// thread takes the backend next forwards what it makes after these,
    /// never before. Forwarded after the backend was let go, a CLOSE's
    /// Unwatch on one thread could reach the pump ahead of the Watch an
    /// earlier call on another made, and the pump then held its duplicate
    /// of the closed file -- a DRM master, a lease -- for as long as the
    /// session lasts.
    fn forward_from(&self, mut be: MutexGuard<'_, NvidiaBackend>) {
        let cmds = be.take_pump_cmds();
        if cmds.is_empty() {
            return;
        }
        let p = self.pump.lock().unwrap();
        drop(be);
        Self::hand_over(p, cmds);
    }

    /// Finish an executed IOCTL2 under the backend lock, and forward what
    /// finishing told the pump: a consumed handle it closed has a watch to
    /// end now, not whenever the next request happens to be served.
    fn finish(&self, p: PendingIoctl2) -> Reply {
        let mut be = self.nvidia.lock().unwrap();
        let reply = be.finish_ioctl2(p);
        self.forward_from(be);
        reply
    }

    /// Complete a chain, unless its ring has moved on since it was taken.
    fn complete(&self, vring: &Vring, t: &Taken, mut reply: Reply) {
        let Some(mem) = self.mem.read().unwrap().clone() else {
            return;
        };
        let mem = mem.memory();
        let mut ring = vring.get_mut();
        if vring.epoch() != t.epoch {
            drop(ring);
            log::info!(
                "a completion for chain {} arrived after its ring was reset; dropped",
                t.head
            );
            if !reply.created.is_empty() {
                let mut be = self.nvidia.lock().unwrap();
                be.close_handles(&reply.created);
                self.forward_from(be);
            }
            return;
        }
        // As late as it can be: the guest's clock sample is taken in its
        // virtqueue callback, so the closer this is to add_used the smaller
        // the asymmetry in the round trip it measures.
        reply.stamp();
        let written = scatter(&*mem, &t.writable, &reply.bytes);
        if let Err(e) = ring.add_used(t.head, written as u32) {
            log::warn!("add_used for chain {}: {e}", t.head);
        }
        let signal = wants_interrupt(ring.get_queue_mut(), &*mem);
        drop(ring);
        if signal && let Err(e) = vring.signal_used_queue() {
            log::warn!("signal used queue: {e}");
        }
    }
}

/// Whether the guest acked VIRTIO_F_NOTIFY_ON_EMPTY: then it is interrupted
/// whatever it asked, as the device did before it honoured suppression.
static NOTIFY_ON_EMPTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// VRING_AVAIL_F_NO_INTERRUPT (virtio 1.2 §2.7.7).
const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Whether the guest wants an interrupt for what was just added to `q`'s
/// used ring, asked under the ring's lock right after `add_used`: with
/// EVENT_IDX, once past its `used_event`; without, unless it set
/// VRING_AVAIL_F_NO_INTERRUPT, which virtio-queue does not read. The device
/// SHOULD honour both (virtio 1.2 §2.7.10). An error answers yes: a missed
/// interrupt strands a waiter, an extra one costs only time. Signalling
/// every completion would cost the guest's reply-poll path an interrupt per
/// reply however it asked.
fn wants_interrupt<M: GuestMemoryBackend>(q: &mut impl QueueT, mem: &M) -> bool {
    use vm_memory::Bytes;
    if NOTIFY_ON_EMPTY.load(Ordering::Relaxed) {
        return true;
    }
    match q.needs_notification(mem) {
        Err(_) | Ok(true) if q.event_idx_enabled() => true,
        Err(_) => true,
        Ok(false) => false,
        // `needs_notification` fenced after the used-ring writes.
        Ok(true) => mem
            .read_obj::<u16>(GuestAddress(q.avail_ring()))
            .map_or(true, |f| u16::from_le(f) & VRING_AVAIL_F_NO_INTERRUPT == 0),
    }
}

/// The event queue, as the pump fills it.
struct VringEventQueue {
    vring: Vring,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
}

impl EventQueue for VringEventQueue {
    fn fill(&mut self, build: &mut dyn FnMut(usize) -> Vec<u8>) -> Fill {
        let mem = self.mem.memory();
        let mut ring = self.vring.get_mut();
        let chain = {
            let Ok(mut avail) = ring.get_queue_mut().iter(mem.clone()) else {
                return Fill::NoBuffer;
            };
            let Some(chain) = avail.next() else {
                return Fill::NoBuffer;
            };
            chain
        };
        // Buffers in hand: the guest need not kick as it posts more. Without
        // EVENT_IDX it otherwise kicks on every buffer it hands back -- an
        // exit, and a wakeup of the worker and of this thread, per batch of
        // events; `want_kick` asks again when they run out. (With EVENT_IDX
        // this is a no-op: the guest kicks only past the avail event that
        // `want_kick` sets.)
        let _ = ring.get_queue_mut().disable_notification(&*mem);
        let head = chain.head_index();
        let writable: Vec<(GuestAddress, u32)> = chain
            .filter(|d| d.is_write_only())
            .map(|d| (d.addr(), d.len()))
            .collect();
        let cap = writable.iter().map(|&(_, l)| l as usize).sum();
        let bytes = build(cap);
        let written = scatter(&*mem, &writable, &bytes);
        if let Err(e) = ring.add_used(head, written as u32) {
            log::warn!("event queue add_used: {e}");
        }
        let signal = wants_interrupt(ring.get_queue_mut(), &*mem);
        drop(ring);
        if signal {
            let _ = self.vring.signal_used_queue();
        }
        if bytes.is_empty() {
            Fill::Empty
        } else {
            Fill::Filled
        }
    }

    fn want_kick(&mut self) -> bool {
        self.vring.enable_notification().unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// `--pci-config-dir`: each GPU's snapshot, by address, at most 4 KiB (a
/// longer file is no config space). A GPU with no file is left out, and
/// so is one that cannot be read, with a warning.
fn read_pci_configs(dir: &Path, gpus: &[device::virtio::GpuSlot]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for g in gpus {
        let addr = g.address();
        let path = dir.join(&addr);
        match std::fs::read(&path) {
            Ok(b) if b.len() <= 4096 => {
                log::info!(
                    "--pci-config-dir: {} bytes of GPU {addr}'s config space",
                    b.len()
                );
                out.push((addr, b));
            }
            Ok(b) => log::warn!(
                "--pci-config-dir: {} is {} bytes, no config space; not used",
                path.display(),
                b.len()
            ),
            Err(e) => log::warn!("--pci-config-dir: {}: {e}", path.display()),
        }
    }
    out
}

/// Have the inject server refuse the VMM's uid: the one other process this
/// backend's Unix sockets are connected to at the first SET_MEM_TABLE (the
/// vhost-user connection and the request channel). More than one uid there
/// and none is refused, with a warning; the `--inject-uid` rule still holds.
fn refuse_vmm_uid(s: &device::inject::InjectServer) {
    let mut uids: Vec<u32> = device::sys::fd::unix_peers()
        .iter()
        .map(|c| c.uid)
        .collect();
    uids.sort_unstable();
    uids.dedup();
    match uids[..] {
        [uid] => {
            log::info!("inject: the VMM is uid {uid}; no helper of that uid is accepted");
            s.refuse_uid(uid);
        }
        _ => log::warn!(
            "inject: the VMM's uid is not certain (peers {uids:?}); only --inject-uid applies"
        ),
    }
}

/// Something to start once the process's descriptors are registered.
type AfterScan = Box<dyn FnOnce() + Send + Sync>;

struct NvGpuBackend {
    shared: Arc<Shared>,
    event_idx: bool,
    config: VirtioGpuNvConfig,
    max_req: usize,
    max_resp: usize,
    /// Guest memory's backing descriptors, as registered with `privfd`.
    mem_fds: Vec<RawFd>,
    /// Whether the library's own descriptors have been registered yet.
    scanned_fds: bool,
    /// Threads that open descriptors of their own -- the Wayland export and
    /// inject accept threads -- started only once the scan is done: a peer
    /// or helper descriptor open at the scan was registered as the
    /// backend's own for good, and once closed its number, handed back by
    /// a later IOCTL2, would be refused adoption.
    after_scan: Vec<AfterScan>,
    /// `--allow-compute`, for the regions GET_SHMEM_CONFIG reports.
    allow_compute: bool,
    /// The window the allocator was made with, which GET_SHMEM_CONFIG
    /// reports.
    window: ZoneConfig,
    /// `--queue-poll-us`.
    queue_poll: std::time::Duration,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    fn new(
        proc_nvidia: &Path,
        abi_policy: device::nvidia::AbiPolicy,
        rm_allowlist: device::rmallow::Mode,
        config: BackendConfig,
        wayland: Wayland,
        allow_unmeasured: bool,
        window: ZoneConfig,
    ) -> anyhow::Result<Self> {
        let version = host::driver_version(proc_nvidia).ok_or_else(|| {
            anyhow::anyhow!(
                "no NVIDIA driver version at {} -- is the kernel module loaded?",
                proc_nvidia.display()
            )
        })?;
        let gpus = host::gpu_slots(proc_nvidia);
        anyhow::ensure!(
            !gpus.is_empty(),
            "driver {version} is loaded but owns no GPUs; the guest driver rejects an empty table"
        );
        log::info!("host driver {version}, {} GPU(s)", gpus.len());
        release_gate(&version, allow_unmeasured)?;
        // What the window lets this VM keep CPU-mapped of video memory goes
        // through BAR1, which the desktop and every other VM share: said at
        // start when one VM could take more than half of it (DEPLOY.md,
        // "Sizing the window").
        for g in &gpus {
            let addr = g.address();
            if let Some(bar1) = host::bar1_len(Path::new("/sys"), &addr)
                && window.wc_size > bar1 / 2
            {
                log::warn!(
                    "window: its write-combining zone ({} MiB) is more than half of GPU {addr}'s \
                     BAR1 ({} MiB), which the host's desktop and every other VM on it map video \
                     memory through too; one guest could leave them little",
                    window.wc_size >> 20,
                    bar1 >> 20
                );
            }
        }

        let allow_compute = config.allow_compute;
        let mut nvidia = NvidiaBackend::with_zone_config(window);
        nvidia.set_abi_policy(abi_policy);
        nvidia.set_rm_allowlist(rm_allowlist);
        nvidia.set_config(config);
        nvidia.set_host_driver_version(&version);
        if allow_unmeasured {
            nvidia.allow_unmeasured_release();
        }
        nvidia.set_wayland(wayland.cfg);
        nvidia.set_wayland_export(wayland.export);
        nvidia.set_wayland_limits(wayland.limits);

        Ok(Self {
            shared: Arc::new(Shared {
                nvidia: Mutex::new(nvidia),
                mem: RwLock::new(None),
                pool: ExecPool::default(),
                pump: Mutex::new(PumpState::default()),
            }),
            event_idx: false,
            // The GPUs the guest sees, and which ioctls carry a descriptor.
            config: VirtioGpuNvConfig::new(&version, &gpus),
            max_req: MAX_XFER_DIRECT as usize,
            max_resp: MAX_XFER_DIRECT as usize,
            mem_fds: Vec::new(),
            scanned_fds: false,
            after_scan: Vec::new(),
            allow_compute,
            window,
            queue_poll: std::time::Duration::ZERO,
        })
    }

    /// Look at the control ring for up to `--queue-poll-us` for a chain the
    /// guest posted after the last drain, notifications still off. True as
    /// soon as there is one.
    fn poll_for_more(&self, vring: &Vring) -> bool {
        if self.queue_poll.is_zero() {
            return false;
        }
        let Some(atomic) = self.shared.mem.read().unwrap().clone() else {
            return false;
        };
        let mem = atomic.memory();
        let until = std::time::Instant::now() + self.queue_poll;
        loop {
            {
                let ring = vring.get_ref();
                let q = ring.get_queue();
                if q.avail_idx(&*mem, Ordering::Acquire)
                    .is_ok_and(|a| a != std::num::Wrapping(q.next_avail()))
                {
                    device::pacing::PACING
                        .queue_polled
                        .fetch_add(1, Ordering::Relaxed);
                    return true;
                }
            }
            if std::time::Instant::now() >= until {
                return false;
            }
            std::hint::spin_loop();
        }
    }

    /// Start the event pump on first use: it needs guest memory and the event
    /// queue, neither of which exists before the guest drives the device.
    fn ensure_pump(&self, vrings: &[Vring]) {
        let mut p = self.shared.pump.lock().unwrap();
        if p.handle.is_some() {
            return;
        }
        let (Some(mem), Some(vring)) = (
            self.shared.mem.read().unwrap().clone(),
            vrings.get(EVENT_QUEUE).cloned(),
        ) else {
            return;
        };
        match Pump::spawn(VringEventQueue { vring, mem }) {
            Ok(h) => {
                for c in std::mem::take(&mut p.queued) {
                    h.send(c);
                }
                p.handle = Some(h);
            }
            Err(e) => log::error!("event pump would not start: {e}"),
        }
    }

    /// Serve under the backend lock, forwarding what it tells the pump and
    /// withdrawing queued executor jobs if it reset the session.
    fn serve(&self, req: &[u8], cap: usize) -> Outcome {
        let mut be = self.shared.nvidia.lock().unwrap();
        let before = be.generation();
        let outcome = be.serve(req, cap);
        let reset = be.generation() != before;
        self.shared.forward_from(be);
        if reset {
            self.shared.pool.cancel_pending();
        }
        outcome
    }

    /// Drain the control queue.
    fn process(&mut self, vring: &Vring) -> std::io::Result<bool> {
        let Some(atomic) = self.shared.mem.read().unwrap().clone() else {
            return Err(std::io::Error::other("guest memory not set"));
        };
        let mem = atomic.memory();
        let mut used = false;
        loop {
            let (chain, epoch) = {
                let mut ring = vring.get_mut();
                let Ok(mut avail) = ring.get_queue_mut().iter(mem.clone()) else {
                    break;
                };
                let Some(chain) = avail.next() else { break };
                (chain, vring.epoch())
            };
            used = true;
            // The backend's part of the guest's round trip starts here
            // (device::pacing).
            let t0 = std::time::Instant::now();
            let head = chain.head_index();
            let descs = chain.map(|d| (d.is_write_only(), d.addr(), d.len()));
            let (l, refused) = match layout(descs, self.max_req) {
                Ok(l) => (l, None),
                Err(l) => {
                    log::warn!(
                        "chain {head}: a request of more than {} bytes; refused",
                        self.max_req
                    );
                    (l, Some(libc::E2BIG))
                }
            };
            let taken = Taken {
                head,
                epoch,
                writable: l.writable,
                cap: l.cap,
            };
            if taken.cap < HDR {
                // Nowhere to put even a header. Hand the chain back empty so
                // the ring does not leak it.
                log::warn!("chain {head} has {} writable bytes; dropping", taken.cap);
                self.shared.complete(vring, &taken, Reply::default());
                continue;
            }
            let req = match refused {
                Some(e) => {
                    self.shared.complete(vring, &taken, transport_error(e));
                    continue;
                }
                None => match gather(&*mem, &l.readable, l.req_len) {
                    Some(r) => r,
                    None => {
                        self.shared
                            .complete(vring, &taken, transport_error(libc::EFAULT));
                        continue;
                    }
                },
            };

            let msg_type = req
                .get(..4)
                .map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
            let pacing = &device::pacing::PACING;
            match self.serve(&req, taken.cap.min(self.max_resp)) {
                Outcome::Reply(r) => {
                    self.shared.complete(vring, &taken, r);
                    pacing.served(msg_type, t0);
                }
                Outcome::Ioctl2(mut p) => match p.executor_key() {
                    Some(key) => {
                        let (shared, vring) = (self.shared.clone(), vring.clone());
                        pacing
                            .ioctl2_executor
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        self.shared.pool.submit(
                            key,
                            Box::new(move |cancelled| {
                                let reply = if cancelled {
                                    p.cancelled_reply()
                                } else {
                                    p.execute();
                                    shared.finish(p)
                                };
                                shared.complete(&vring, &taken, reply);
                                device::pacing::PACING.served(msg_type, t0);
                            }),
                        );
                    }
                    None => {
                        // Inline, but still without the backend lock: the
                        // host call is made by nobody else's schedule.
                        p.execute();
                        let reply = self.shared.finish(p);
                        self.shared.complete(vring, &taken, reply);
                        pacing.served(msg_type, t0);
                    }
                },
            }
        }
        Ok(used)
    }
}

impl VhostUserBackendMut for NvGpuBackend {
    type Bitmap = ();
    type Vring = Vring;

    fn num_queues(&self) -> usize {
        QUEUE_COUNT
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE as usize
    }

    /// An exit event for each ring worker, which `main` writes once the
    /// connection is gone. With none (the trait's default) a worker never
    /// leaves its epoll, and the daemon's handler joins it as it is dropped:
    /// a backend whose VMM hung up -- before the rings started, say -- said
    /// "backend exited" and then never did, and a socket-activated unit
    /// stayed up with nothing accepting.
    fn exit_event(
        &self,
        _thread_index: usize,
    ) -> Option<(
        vmm_sys_util::event::EventConsumer,
        vmm_sys_util::event::EventNotifier,
    )> {
        use vmm_sys_util::event::{EventFlag, new_event_consumer_and_notifier};
        match new_event_consumer_and_notifier(EventFlag::NONBLOCK | EventFlag::CLOEXEC) {
            Ok(pair) => Some(pair),
            Err(e) => {
                log::error!("no exit event for a ring worker ({e}); it will not stop at exit");
                None
            }
        }
    }

    fn features(&self) -> u64 {
        // INDIRECT_DESC lets one ring slot describe a whole table of
        // descriptors (virtio-queue 0.18.0, src/chain.rs
        // `switch_to_indirect_table`, :119-145, follows them), which is
        // what makes 4 MiB requests possible on a 256-entry ring. Without it
        // the guest keeps to 256 KiB.
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_F_NOTIFY_ON_EMPTY)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | (1 << VIRTIO_RING_F_INDIRECT_DESC)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn acked_features(&mut self, features: u64) {
        NOTIFY_ON_EMPTY.store(
            features & (1 << VIRTIO_F_NOTIFY_ON_EMPTY) != 0,
            Ordering::Relaxed,
        );
        let indirect = features & (1 << VIRTIO_RING_F_INDIRECT_DESC) != 0;
        let limit = if indirect {
            MAX_XFER_INDIRECT
        } else {
            MAX_XFER_DIRECT
        };
        log::info!(
            "features acked: indirect descriptors {}, requests and responses up to {limit} bytes",
            if indirect { "on" } else { "off" }
        );
        self.max_req = limit as usize;
        self.max_resp = limit as usize;
        self.shared
            .nvidia
            .lock()
            .unwrap()
            .set_transport_limits(limit, limit);
    }

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            // The channel mapping requests travel up on, and the feature that
            // gates the request itself. Device memory has to be placed by the
            // VMM: `MAP_FIXED` here would rewrite only this process's page
            // tables, and the memory slot the guest reads through describes
            // the VMM's address space, not ours.
            | VhostUserProtocolFeatures::BACKEND_REQ
            | VhostUserProtocolFeatures::SHMEM
    }

    /// The guest reset the device. The rings' epochs have already moved (the
    /// handler disables every ring first: vhost-user-backend 0.23.0,
    /// src/handler.rs `reset_device`, :279-290); this ends the
    /// session, so a rebooted guest does not find the old one's host files
    /// still open -- DRM master still held, leases still granted.
    fn reset_device(&mut self) {
        let mut be = self.shared.nvidia.lock().unwrap();
        be.session_reset("device reset");
        self.shared.forward_from(be);
        self.shared.pool.cancel_pending();
    }

    fn set_backend_req_fd(&mut self, backend: Backend) {
        log::info!("window: request channel open; device memory is now mappable");
        self.shared
            .nvidia
            .lock()
            .unwrap()
            .set_window(Box::new(VhostWindow(backend)));
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    fn get_shmem_config(&self) -> std::io::Result<VhostUserShMemConfig> {
        SHMEM_CONFIG_ASKED.store(true, Ordering::Relaxed);
        Ok(shmem_config(&self.window, self.allow_compute))
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    /// New guest memory. Its regions' backing files (the VMM's memfds) are
    /// the backend's own descriptors from here on, and registered as such;
    /// the previous table's go once it is replaced.
    ///
    /// The first time is also when everything else the vhost-user library
    /// holds is registered, by taking the whole descriptor table: the
    /// connection is up, the backend request channel has normally arrived,
    /// the library's worker threads have their epoll and exit eventfds, and
    /// no guest request can have been served (the rings are not started
    /// before the memory table exists), so every descriptor open is
    /// plumbing. None of it is ever shown to this crate one by one.
    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        let fds: Vec<RawFd> = mem
            .memory()
            .iter()
            .filter_map(|r| r.file_offset().map(|f| f.file().as_raw_fd()))
            .collect();
        fds.iter().for_each(|&fd| privfd::register(fd));
        // Where guest RAM is, for memory the guest registers with RM by its
        // pages (device::osdesc): each page it names must be in this table.
        self.shared.nvidia.lock().unwrap().set_guest_ram(Some(
            device::osdesc::GuestRam::from_vm_memory(mem.memory().into_inner()),
        ));
        *self.shared.mem.write().unwrap() = Some(mem);
        for old in std::mem::replace(&mut self.mem_fds, fds) {
            if !self.mem_fds.contains(&old) {
                privfd::unregister(old);
            }
        }
        if !self.scanned_fds {
            self.scanned_fds = true;
            // A VMM that never asked the window's size publishes its own
            // (nesbox before virtio-nvgpu-v4: 1 GiB), and every placement
            // past it then fails on its own as `SHM alloc failed`. Said once,
            // here, where the memory table shows the VMM is past setting up.
            if self.window.total() != ZoneConfig::default_1gib().total()
                && !SHMEM_CONFIG_ASKED.load(Ordering::Relaxed)
            {
                log::warn!(
                    "the VMM never asked the shared window's size (GET_SHMEM_CONFIG), and the \
                     window is {} MiB, not the 1024 a VMM assumes: placements past what it \
                     published will fail. Upgrade the VMM (nesbox virtio-nvgpu-v4 or later), \
                     or leave --window-size at 1024",
                    self.window.total() >> 20
                );
            }
            let be = self.shared.nvidia.lock().unwrap();
            let n = privfd::register_process_fds(&|fd| be.owns_fd(fd));
            drop(be);
            log::info!("{n} descriptor(s) of the transport registered as the backend's own");
            for start in self.after_scan.drain(..) {
                start();
            }
        }
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[Vring],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if device_event as usize >= QUEUE_COUNT {
            return Err(std::io::Error::other(format!(
                "event for unknown queue {device_event}"
            )));
        }
        self.ensure_pump(vrings);

        // The event queue carries buffers the guest posted for *us* to fill,
        // not requests. Serving them as requests is a loop with no bottom:
        // each one is dispatched, answered with "unknown message", handed back
        // filled, re-posted by the guest, and kicked again -- 7.4 million
        // times in five seconds, measured, the first time two guests ran at
        // once. The pump owns this queue; a kick on it means buffers arrived,
        // so it is told to flush what it holds now rather than at its next
        // sweep.
        if device_event as usize == EVENT_QUEUE {
            device::pacing::PACING
                .event_kicks
                .fetch_add(1, Ordering::Relaxed);
            if let Some(h) = self.shared.pump.lock().unwrap().handle.as_ref() {
                h.kick();
            }
            return Ok(());
        }

        let vring = &vrings[device_event as usize];
        // With --queue-poll-us the drain loop runs whatever was negotiated:
        // without EVENT_IDX, disabling sets VRING_USED_F_NO_NOTIFY, and the
        // guest does not kick while this thread still looks.
        if self.event_idx || !self.queue_poll.is_zero() {
            // With EVENT_IDX the guest suppresses notifications, so re-arm and
            // drain again rather than waiting for a kick that will not come.
            loop {
                vring.disable_notification().ok();
                self.process(vring)?;
                if self.poll_for_more(vring) {
                    continue;
                }
                if !vring.enable_notification().unwrap_or(false) {
                    break;
                }
            }
        } else {
            self.process(vring)?;
        }
        Ok(())
    }
}

/// Whether NVGPU_DIAGNOSTIC=1 is set.
fn diagnostic_env() -> bool {
    std::env::var_os("NVGPU_DIAGNOSTIC").is_some_and(|v| v == "1")
}

/// The arguments. The diagnostic flags are hidden from `--help` unless
/// `--diagnostic` (or NVGPU_DIAGNOSTIC=1) is given too.
fn parse_args() -> Args {
    use clap::{CommandFactory, FromArgMatches};
    let diagnostic = diagnostic_env() || std::env::args_os().any(|a| a == "--diagnostic");
    let mut cmd = Args::command();
    if diagnostic {
        cmd = cmd.mut_args(|a| a.hide(false));
    }
    let m = cmd.get_matches();
    Args::from_arg_matches(&m).unwrap_or_else(|e| e.exit())
}

/// The diagnostic flags in effect, each with what it takes away.
fn diagnostic_flags(args: &Args) -> Vec<(&'static str, &'static str)> {
    let mut v = Vec::new();
    if args.allow_root_unsafe {
        v.push((
            "--allow-root-unsafe",
            "the backend may start as root or with CAP_SYS_ADMIN, and every guest process is \
             then an RM administrator with BAR0 mappable",
        ));
    }
    if args.proc_nvidia != Path::new(host::PROC_NVIDIA) {
        v.push((
            "--proc-nvidia",
            "the host driver is read from a tree other than /proc/driver/nvidia, and the \
             sandbox opens that tree instead",
        ));
    }
    if args.allow_inject_self {
        v.push((
            "--allow-inject-self",
            "--inject-uid may be the backend's own uid, so every process of that user may \
             inject buffers into this VM",
        ));
    }
    if args.permissive_abi {
        v.push((
            "--permissive-abi",
            "ioctls the ABI profile does not describe are forwarded to the host driver \
             unchecked",
        ));
    }
    if args.keep_guest_coherency {
        v.push((
            "--keep-guest-coherency",
            "guest system memory is allocated with the coherency the guest asks for; on an \
             Intel host a guest can cache memory the GPU does not snoop",
        ));
    }
    if args.rm_allowlist == device::rmallow::Mode::Log {
        v.push((
            "--rm-allowlist=log",
            "RM controls and classes the allowlist lacks are forwarded to the host's RM",
        ));
    }
    match args.sandbox {
        device::sandbox::Mode::On => {}
        device::sandbox::Mode::BestEffort => v.push((
            "--sandbox=best-effort",
            "a sandbox layer this kernel lacks is done without",
        )),
        device::sandbox::Mode::Off => v.push((
            "--sandbox=off",
            "no network namespace, Landlock or seccomp: a backend taken over is everything \
             its uid is",
        )),
    }
    if args.allow_unmeasured_release {
        v.push((
            "--allow-unmeasured-release",
            "a host release the tables were not measured at runs on the nearest older ones",
        ));
    }
    v
}

/// The capture helper is a user of its own: not root, which is every user,
/// and not the backend's own uid, which is every process of the VM's backend
/// user (in the Wayland modes, the desktop's) -- unless the diagnostic
/// `--allow-inject-self` says this is a test rig of one user.
fn inject_uid_ok(uid: u32, own: u32, allow_self: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
        uid != 0,
        "refusing to start: --inject-uid 0; give the capture helper a user of its own"
    );
    anyhow::ensure!(
        uid != own || allow_self,
        "refusing to start: --inject-uid {uid} is the backend's own uid; give the capture \
         helper a user of its own (--allow-inject-self --diagnostic is for a test rig)"
    );
    Ok(())
}

/// Refuse the diagnostic flags without `--diagnostic`; announce them with it.
fn check_diagnostic(args: &Args, env: bool) -> anyhow::Result<Vec<String>> {
    let flags = diagnostic_flags(args);
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<&str> = flags.iter().map(|(n, _)| *n).collect();
    anyhow::ensure!(
        args.diagnostic || env,
        "refusing to start: {} {} diagnostic, for finding out what broke something and never \
         for running a guest; add --diagnostic (or NVGPU_DIAGNOSTIC=1) if that is what this is",
        names.join(", "),
        if names.len() == 1 { "is" } else { "are" }
    );
    Ok(flags
        .iter()
        .map(|(n, w)| format!("DIAGNOSTIC: {n}: {w}. Not for running a guest"))
        .collect())
}

/// Refuse a host release the tables were not measured at, unless
/// `--allow-unmeasured-release`; name the tables chosen either way.
fn release_gate(version: &str, allow_unmeasured: bool) -> anyhow::Result<()> {
    let v = abi::version::DriverVersion::parse(version).ok_or_else(|| {
        anyhow::anyhow!(
            "refusing to start: host driver version {version:?} does not parse, so no table \
             can be chosen for it"
        )
    })?;
    let cov = device::release::Coverage::of(v);
    // One line, at warning level, every start: what a guest is held to.
    log::warn!("{}", cov.summary());
    if cov.measured() {
        return Ok(());
    }
    let measured: Vec<String> = device::release::measured_releases()
        .iter()
        .map(|v| v.to_string())
        .collect();
    if !cov.runnable() {
        anyhow::bail!(
            "refusing to start: host driver {v} is older than every release measured ({}); \
             no table describes it",
            measured.join(", ")
        );
    }
    if !allow_unmeasured {
        anyhow::bail!(
            "refusing to start: host driver {v} is not a release the tables were measured at \
             ({} unmeasured). Measured: {}. Measure it (gen/README.md), or run a measured \
             release; --allow-unmeasured-release (with --diagnostic) runs it on the nearest \
             older tables",
            cov.unmeasured().join(", "),
            measured.join(", ")
        );
    }
    log::warn!(
        "--allow-unmeasured-release: host driver {v} runs on the nearest older tables ({} \
         unmeasured); a control or layout that changed in it is not caught",
        cov.unmeasured().join(", ")
    );
    Ok(())
}

/// Whether the VMM has asked GET_SHMEM_CONFIG on this connection (the
/// backend serves one).
static SHMEM_CONFIG_ASKED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// What an inherited listening socket is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handed {
    VhostUser,
    Inject,
}

/// Which descriptors systemd's socket activation hands this process, and
/// for what (sd_listen_fds(3)): none unless `LISTEN_PID` is this process;
/// then `LISTEN_FDS` of them from descriptor 3, named by `LISTEN_FDNAMES`
/// (`FileDescriptorName=` in the socket unit). A lone unnamed one is the
/// vhost-user socket; two or more must be named `vhost-user` and `inject`.
/// Anything else is refused rather than guessed at.
fn listen_fds(
    pid: i32,
    listen_pid: Option<&str>,
    listen_fds: Option<&str>,
    names: Option<&str>,
) -> anyhow::Result<Vec<(RawFd, Handed)>> {
    if listen_pid.and_then(|p| p.parse::<i32>().ok()) != Some(pid) {
        return Ok(Vec::new());
    }
    let n: usize = listen_fds
        .and_then(|n| n.parse().ok())
        .filter(|&n| n <= 2)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "LISTEN_FDS={}: socket activation hands the backend its vhost-user socket, and \
                 its inject socket at most",
                listen_fds.unwrap_or("")
            )
        })?;
    let names: Vec<&str> = names.map(|s| s.split(':').collect()).unwrap_or_default();
    let mut out = Vec::new();
    for i in 0..n {
        let fd = 3 + i as RawFd;
        let role = match names.get(i).copied() {
            Some("vhost-user") => Handed::VhostUser,
            Some("inject") => Handed::Inject,
            None | Some("") | Some("unknown") if n == 1 => Handed::VhostUser,
            other => anyhow::bail!(
                "socket activation: descriptor {fd} is named {:?}; name the sockets \
                 FileDescriptorName=vhost-user and FileDescriptorName=inject \
                 (contrib/systemd)",
                other.unwrap_or("")
            ),
        };
        if out.iter().any(|&(_, r)| r == role) {
            anyhow::bail!("socket activation: two descriptors named for {role:?}");
        }
        out.push((fd, role));
    }
    Ok(out)
}

/// The listening sockets this backend was handed rather than binding its
/// own, the vhost-user one with how it came, for the log.
struct Inherited {
    vhost_user: Option<(OwnedFd, String)>,
    inject: Option<OwnedFd>,
}

/// `--socket-fd N`, and systemd's socket activation (`listen_fds`): each
/// descriptor claimed and checked to be a listening `AF_UNIX` socket of the
/// type its protocol takes; the activation variables then removed from the
/// environment. Called while the process has one thread, before it opens
/// anything.
fn inherited_sockets(args: &Args) -> anyhow::Result<Inherited> {
    let var = |k: &str| std::env::var(k).ok();
    let handed = listen_fds(
        device::sys::proc::pid(),
        var("LISTEN_PID").as_deref(),
        var("LISTEN_FDS").as_deref(),
        var("LISTEN_FDNAMES").as_deref(),
    )?;
    // Unset whether or not they were ours: they describe this exec only.
    if !device::sys::inherit::unset_env(&["LISTEN_PID", "LISTEN_FDS", "LISTEN_FDNAMES"]) {
        log::debug!("socket activation: environment left as it was (more than one thread)");
    }
    let take = |fd: RawFd, ty: i32, what: &str| -> anyhow::Result<OwnedFd> {
        let owned = device::sys::inherit::claim(fd)
            .map_err(|e| anyhow::anyhow!("{what}: descriptor {fd}: {e}"))?;
        let k = device::sys::inherit::socket_kind(std::os::fd::AsFd::as_fd(&owned))
            .map_err(|e| anyhow::anyhow!("{what}: descriptor {fd}: {e}"))?;
        if k.domain != libc::AF_UNIX || k.ty != ty || !k.listening {
            anyhow::bail!(
                "{what}: descriptor {fd} is not a listening AF_UNIX {} socket",
                if ty == libc::SOCK_STREAM {
                    "SOCK_STREAM"
                } else {
                    "SOCK_SEQPACKET"
                }
            );
        }
        Ok(owned)
    };
    let mut out = Inherited {
        vhost_user: None,
        inject: None,
    };
    for (fd, role) in handed {
        match role {
            Handed::VhostUser => {
                out.vhost_user = Some((
                    take(fd, libc::SOCK_STREAM, "socket activation")?,
                    format!("descriptor {fd} (socket activation)"),
                ))
            }
            Handed::Inject => {
                out.inject = Some(take(fd, libc::SOCK_SEQPACKET, "socket activation")?)
            }
        }
    }
    if let Some(fd) = args.socket_fd {
        if out.vhost_user.is_some() {
            anyhow::bail!("--socket-fd {fd} and a socket-activated vhost-user socket: one of them");
        }
        out.vhost_user = Some((
            take(fd, libc::SOCK_STREAM, "--socket-fd")?,
            format!("descriptor {fd} (--socket-fd)"),
        ));
    }
    if out.vhost_user.is_some() && args.socket.is_some() {
        anyhow::bail!("--socket and a socket-activated vhost-user socket: one of them");
    }
    match (&out.inject, &args.inject_socket, args.inject_uid) {
        (Some(_), Some(_), _) => {
            anyhow::bail!("--inject-socket and a socket-activated inject socket: one of them")
        }
        (Some(_), None, None) => {
            anyhow::bail!("a socket-activated inject socket needs --inject-uid")
        }
        (None, None, Some(_)) => anyhow::bail!(
            "--inject-uid needs --inject-socket, or an inject socket from socket activation"
        ),
        _ => {}
    }
    Ok(out)
}

/// The window `--window-size` and `--window-owner-share` ask for, beside
/// the UVM aperture when compute is served, or why not.
fn window_config(args: &Args) -> anyhow::Result<ZoneConfig> {
    let aperture = if args.allow_compute {
        device::uvmmap::APERTURE_MAX
    } else {
        0
    };
    ZoneConfig::for_window(args.window_size, args.window_owner_share, aperture)
        .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))
}

fn main() -> anyhow::Result<()> {
    // Every call site metered (device::ratelimit): most of what is logged
    // here is something a guest did, and a guest can do it in a loop. Warnings
    // and up unless RUST_LOG says otherwise: the start-up lines worth keeping
    // (the tables, the sandbox, the diagnostic flags) are warnings.
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).build();
    let max_level = logger.filter();
    log::set_boxed_logger(Box::new(device::ratelimit::RateLimited::new(logger)))
        .expect("the logger is set once, first thing");
    log::set_max_level(max_level);
    let args = parse_args();
    // Before anything is opened: a diagnostic flag without --diagnostic
    // stops here, and one with it is said on stderr whatever the log level.
    for line in check_diagnostic(&args, diagnostic_env())? {
        eprintln!("vhost-user-nvgpu: {line}");
        log::warn!("{line}");
    }
    // Sockets handed over at exec: claimed while this is the only thread
    // and before anything is opened.
    let inherited = inherited_sockets(&args)?;
    // The window, from the flags: refused here, before anything is opened,
    // if it cannot be had. The allocator and GET_SHMEM_CONFIG both take it.
    let window = window_config(&args)?;
    // Kept at the default log level when it is not the default window.
    let level = if window == ZoneConfig::default_1gib() {
        log::Level::Info
    } else {
        log::Level::Warn
    };
    log::log!(
        level,
        "window: {} MiB (UC {} MiB, WC {} MiB, WB {} MiB), {}% of each zone per guest process",
        window.total() >> 20,
        window.uc_size >> 20,
        window.wc_size >> 20,
        window.wb_size >> 20,
        window.owner_percent
    );

    // Before any thread exists, so that every one inherits it.
    // 0 keeps what the launcher or the host set; so does a policy that is
    // not a fair one (device::sys::proc::set_sched_slice).
    if args.sched_slice_us != 0 {
        let us = args.sched_slice_us.clamp(100, 100_000);
        match device::sys::proc::set_sched_slice(us * 1000) {
            Ok(true) => log::info!("--sched-slice-us {us}: every backend thread's EEVDF slice"),
            Ok(false) => log::info!(
                "--sched-slice-us {us}: not a fair scheduling policy; the launcher's stays"
            ),
            Err(e) => log::warn!("--sched-slice-us {us}: {e}; the host's default slice stays"),
        }
    }

    // Before any thread exists: capabilities are per thread
    // (device::posture, S-5).
    let (uid, euid) = (device::sys::proc::uid(), device::sys::proc::euid());
    let caps = posture::Caps::current()?;
    log::info!("credentials: uid {uid}, euid {euid}, capabilities {caps}");
    if let Some(why) = posture::too_privileged(euid, &caps) {
        if !args.allow_root_unsafe {
            anyhow::bail!(
                "refusing to start: {why}. RM, DRM and NVKMS take a guest's privilege from \
                 the backend's, so every guest process would be an RM administrator with \
                 BAR0 mappable read-write. Run the backend as an unprivileged user in the \
                 video, render and kvm groups (rig/run-guest.sh does), or pass \
                 --allow-root-unsafe --diagnostic"
            );
        }
        log::warn!(
            "--allow-root-unsafe: starting although {why}; capabilities are dropped below, \
             but files owned by root stay open to this process"
        );
    }
    posture::drop_all_caps().map_err(|e| anyhow::anyhow!("dropping capabilities: {e}"))?;
    posture::set_undumpable().map_err(|e| anyhow::anyhow!("PR_SET_DUMPABLE: {e}"))?;
    // Nothing this process creates is for anyone else: the vhost-user
    // socket among others.
    device::sys::proc::umask(0o077);
    log::info!(
        "capabilities now {}, no_new_privs set",
        posture::Caps::current()?
    );

    // A socket handed over is served as it is: whoever bound it (root,
    // systemd) chose its path, owner and mode, and dropping it unlinks
    // nothing.
    let (mut listener, socket) = match inherited.vhost_user {
        Some((fd, what)) => (
            vhost::vhost_user::Listener::from(std::os::unix::net::UnixListener::from(fd)),
            what,
        ),
        None => {
            let socket = match &args.socket {
                Some(p) => p.clone(),
                None => {
                    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
                    let p = posture::default_socket(runtime.as_deref()).ok_or_else(|| {
                        anyhow::anyhow!("no --socket given and no XDG_RUNTIME_DIR to put one in")
                    })?;
                    let dir = p.parent().expect("the default has a directory");
                    posture::private_dir(dir, euid)
                        .map_err(|e| anyhow::anyhow!("socket directory: {e}"))?;
                    p
                }
            };
            posture::clear_socket_path(&socket, euid)
                .map_err(|e| anyhow::anyhow!("socket {}: {e}", socket.display()))?;
            // Bound now: the sandbox below leaves no directory writable. Not
            // unlinking first -- the path was just cleared, and a socket
            // someone else put there since makes the bind fail rather than
            // be shared.
            let l = vhost::vhost_user::Listener::new(&socket, false)
                .map_err(|e| anyhow::anyhow!("listen on {}: {e}", socket.display()))?;
            (l, socket.display().to_string())
        }
    };

    log::info!("virtio-nvgpu vhost-user backend: device id {VIRTIO_ID_GPU_NV}, socket {socket}");

    let abi_policy = if args.permissive_abi {
        device::nvidia::AbiPolicy::Permissive
    } else {
        device::nvidia::AbiPolicy::Enforce
    };
    let wayland = Wayland::from_args(&args)?;
    // Stops listening however main ends: the socket file would otherwise stay
    // behind, and a host client connecting to it would wait on a backend that
    // is gone.
    let _export = ExportGuard(wayland.export.as_ref().map(|(x, _)| x.clone()));
    // Bound now, like the export socket: the sandbox leaves no directory to
    // make a socket in. Its threads start after the sandbox.
    // Or handed over, bound by systemd (inherited_sockets checked the
    // combination).
    let inject = match (inherited.inject, &args.inject_socket, args.inject_uid) {
        (fd, p, Some(uid)) if fd.is_some() || p.is_some() => {
            inject_uid_ok(uid, device::sys::proc::uid(), args.allow_inject_self)?;
            let registry = Arc::new(device::inject::Registry::new(Arc::new(
                device::inject::SysInjectHost::for_this_host(),
            )));
            let s = match (fd, p) {
                (Some(fd), _) => device::inject::InjectServer::from_listener(
                    fd,
                    PathBuf::from("(socket activation)"),
                    uid,
                    registry,
                ),
                (None, Some(p)) => device::inject::InjectServer::bind_idle(p, uid, registry)
                    .map_err(|e| {
                        anyhow::anyhow!("--inject-socket {}: cannot listen: {e}", p.display())
                    })?,
                (None, None) => unreachable!("guarded above"),
            };
            log::info!(
                "inject: uid {uid} may inject buffers at {} (at most {} buffers, {} MiB)",
                s.path().display(),
                device::inject::MAX_BUFFERS,
                device::inject::MAX_BYTES >> 20
            );
            Some(Arc::new(s))
        }
        _ => None,
    };

    // What the sandbox would put out of reach is opened first: the uevent
    // socket (a network namespace of the backend's own hears none), and
    // the descriptor limit (the handle table is sized from it).
    let uevents = if args.kms_card || args.wayland_lease {
        match device::kms::uevent_socket() {
            Ok(s) => Some(s),
            Err(e) => {
                log::warn!("no host hotplug events: uevent socket: {e}");
                None
            }
        }
    } else {
        None
    };
    let nofile = posture::raise_nofile()
        .map_err(|e| log::warn!("RLIMIT_NOFILE: {e}; the handle table keeps its default size"))
        .ok();

    // Each GPU's whole PCI config space, as the launcher snapshotted it:
    // read before the sandbox, which leaves the directory out of reach.
    let pci_config: Vec<(String, Vec<u8>)> = match &args.pci_config_dir {
        None => Vec::new(),
        Some(dir) => read_pci_configs(dir, &host::gpu_slots(&args.proc_nvidia)),
    };

    // The sandbox, while this is the only thread (device::sandbox).
    match args.sandbox {
        mode @ (device::sandbox::Mode::On | device::sandbox::Mode::BestEffort) => {
            let gpus: Vec<String> = host::gpu_slots(&args.proc_nvidia)
                .iter()
                .map(|g| g.address())
                .collect();
            let plan = device::sandbox::Plan::backend(
                &device::sandbox::BackendPaths {
                    proc_nvidia: &args.proc_nvidia,
                    gpus,
                    compute: args.allow_compute,
                    kms_card: args.kms_card,
                    wayland_socket: args.wayland_socket.as_deref(),
                    export_socket: args.wayland_export.as_deref(),
                },
                Path::new("/dev"),
                Path::new("/sys"),
            );
            let report = device::sandbox::apply(&plan);
            report.log();
            if report.complete() {
                log::info!("sandbox: every layer in force");
            } else if mode == device::sandbox::Mode::On {
                anyhow::bail!(
                    "refusing to start: the sandbox is not complete on this host (the \
                     `sandbox: DEGRADED` lines above say which layer and why). Fix the host, \
                     or pass --sandbox=best-effort --diagnostic to run without what it lacks"
                );
            } else {
                log::warn!(
                    "--sandbox=best-effort: running without the layers reported DEGRADED above"
                );
            }
        }
        device::sandbox::Mode::Off => log::warn!(
            "sandbox: DEGRADED: --sandbox=off: no network namespace, Landlock or seccomp; \
             a backend taken over is everything uid {euid} is. For diagnosis only"
        ),
    }
    // The counters' clock starts now; their periodic report, if asked for,
    // on a thread of its own.
    device::pacing::start();
    let pacing_every = args.pacing_stats.or_else(|| {
        std::env::var("NVGPU_PACING_STATS")
            .ok()
            .and_then(|v| v.parse().ok())
    });
    if let Some(secs) = pacing_every.filter(|&s| s > 0) {
        device::pacing::spawn_reporter(std::time::Duration::from_secs(secs))
            .map_err(|e| anyhow::anyhow!("--pacing-stats: {e}"))?;
        log::warn!("pacing: counters logged every {secs} s while the guest is busy");
    }
    // The accept threads start once the transport's descriptors are
    // registered (`NvGpuBackend::after_scan`): until then, connections wait
    // in the listeners' backlog.
    let mut after_scan: Vec<AfterScan> = Vec::new();
    if let Some((x, _)) = &wayland.export {
        let x = x.clone();
        after_scan.push(Box::new(move || {
            if let Err(e) = x.start() {
                log::error!("--wayland-export: accept thread: {e}");
            }
        }));
    }
    if let Some(s) = &inject {
        let s = s.clone();
        let own_uid_ok = args.allow_inject_self;
        after_scan.push(Box::new(move || {
            // The VMM is connected by now: its uid is never a helper's.
            // A test rig runs all three as one user (--allow-inject-self).
            if !own_uid_ok {
                refuse_vmm_uid(&s);
            }
            if let Err(e) = s.start() {
                log::error!("--inject-socket: accept thread: {e}");
            }
        }));
    }
    let config = BackendConfig {
        kms_card: args.kms_card,
        wayland_socket: args.wayland_socket,
        wayland_export: args.wayland_export,
        // The fence and syncobj schemas are served (policy.rs FENCES,
        // fence.rs): waits are polls here and sleeps in the guest.
        fences: true,
        allow_compute: args.allow_compute,
        ..BackendConfig::default()
    };
    if config.allow_compute {
        log::info!(
            "--allow-compute: UVM, the UVM aperture and memory registered by its pages are served"
        );
    }
    if config.kms_card {
        log::warn!(
            "compositor-VM mode: the host card nodes are offered to the guest; \
             run no compositor on this host"
        );
    }
    let mut nvgpu = NvGpuBackend::new(
        &args.proc_nvidia,
        abi_policy,
        args.rm_allowlist,
        config,
        wayland,
        args.allow_unmeasured_release,
        window,
    )?;
    nvgpu.after_scan = after_scan;
    {
        let mut be = nvgpu.shared.nvidia.lock().expect("nvidia lock");
        for (addr, config) in pci_config {
            be.set_pci_config(&addr, config);
        }
        if let Some(s) = &inject {
            be.set_inject(Some(s.registry().clone()));
        }
        if args.keep_guest_coherency {
            be.set_guest_coherency(false);
        }
        // Every guest process's descriptors are this process's: the whole
        // of the hard limit, taken before the sandbox, sizes the handle
        // table (B1).
        if let Some(n) = nofile {
            be.set_nofile(n);
        }
    }
    // Past a millisecond it is a core spent for nothing a kick would not do.
    nvgpu.queue_poll = std::time::Duration::from_micros(args.queue_poll_us.min(1000));
    log::info!(
        "--queue-poll-us {}: the queue thread looks at the control ring that long after each \
         drain",
        nvgpu.queue_poll.as_micros()
    );
    let backend = Arc::new(RwLock::new(nvgpu));
    device::rmmem::warn_if_guest_pat_ignored(!args.keep_guest_coherency);
    // Host connector and lease changes of the host's cards, which arrive only
    // as uevents (device::kms). The guest hears of them only in
    // compositor-VM mode: it drives card nodes (BCAP_KMS_CARD) only then,
    // and otherwise drops EV_HOTPLUG. A lease change also makes the backend
    // re-check the leases it holds (NVKMS grants made through one that ended
    // must end too), which matters whenever the guest can hold a lease. The
    // same thread asks the leases NVKMS grants rest on once a second, and at
    // once when a connection to the host compositor hangs up: a lease can
    // end without any uevent, and its file must not outlive it
    // (device::kms, "lease ends"). What that closes, the pump must stop
    // watching now, not at the next guest request.
    let _hotplug = if args.kms_card || args.wayland_lease {
        let shared = backend.read().expect("backend lock").shared.clone();
        let cards = shared.nvidia.lock().expect("nvidia lock").kms_cards();
        let sink = shared.clone();
        let to_guest = args.kms_card;
        let ticker = shared.clone();
        let tick = device::kms::Tick {
            every: std::time::Duration::from_secs(1),
            run: Box::new(move || {
                let mut be = ticker.nvidia.lock().unwrap();
                be.recheck_granting_leases();
                ticker.forward_from(be);
            }),
        };
        let listener = uevents
            .ok_or_else(|| std::io::Error::other("no uevent socket"))
            .and_then(|sock| {
                device::kms::HotplugListener::spawn_ticking_on(
                    sock,
                    cards,
                    move |c| {
                        if let PumpCmd::Hotplug { card, flags } = c
                            && flags & protocol::messages::EV_HOTPLUG_F_LEASE != 0
                        {
                            let mut be = sink.nvidia.lock().unwrap();
                            be.check_leases(Some(card));
                            sink.forward_from(be);
                        }
                        if to_guest {
                            sink.forward(vec![c]);
                        }
                    },
                    Some(tick),
                )
            });
        if let Ok(l) = &listener {
            shared
                .nvidia
                .lock()
                .expect("nvidia lock")
                .set_lease_alarm(l.alarm());
        }
        match listener {
            Ok(l) => Some(l),
            Err(e) => {
                log::warn!("no host hotplug events: uevent socket: {e}");
                None
            }
        }
    } else {
        None
    };
    // What keeps one guest process, and one VM, from another's RM clients is
    // RM's strict client validation, which a host registry key can turn off
    // (device::semsurf::probe_strict_clients, R3). Asked before anything is
    // served; a host that serves a client from another file is refused.
    match device::semsurf::probe_strict_clients(&device::semsurf::HostRm) {
        Ok(true) => log::info!("host RM keeps each client to the file it was made on"),
        Ok(false) => anyhow::bail!(
            "refusing to start: the host's RM serves a client from a file other than the one \
             it was made on (RmValidateClientData turns strict client validation off). Every \
             guest process, and every VM run as this user, could then use another's RM \
             objects by handle. Remove the registry override (NVreg_RegistryDwords) and \
             reload the driver"
        ),
        Err(e) => anyhow::bail!(
            "refusing to start: could not ask the host's RM whether it keeps each client to \
             the file it was made on ({e}); without that, one guest process could use \
             another's RM objects by handle"
        ),
    }
    // vhost_user_backend::Error does not implement std::error::Error, so it
    // cannot ride `?` on its own.
    let mut daemon = VhostUserDaemon::new(
        "virtio-nvgpu".to_string(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("create daemon: {e:?}"))?;

    // `VhostUserDaemon::serve` on the listener bound before the sandbox: one
    // connection, served until the VMM hangs up, and the ring workers told
    // to stop whatever the outcome. A disconnect or a partial message is how
    // a guest ends, not an error.
    let served = daemon.start(&mut listener).and_then(|()| daemon.wait());
    for h in daemon.get_epoll_handlers() {
        h.send_exit_event();
    }
    match served {
        Ok(())
        | Err(vhost_user_backend::Error::HandleRequest(
            vhost::vhost_user::Error::Disconnected | vhost::vhost_user::Error::PartialMessage,
        )) => {}
        Err(e) => anyhow::bail!("serve {socket}: {e:?}"),
    }

    backend
        .read()
        .expect("backend lock")
        .shared
        .nvidia
        .lock()
        .expect("nvidia lock")
        .teardown();
    // What the log rate limit dropped and nothing reported since.
    log::logger().flush();
    log::info!("backend exited");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// GET_SHMEM_CONFIG: the window as region 1, and the UVM aperture as
    /// region 2 only when compute is served; region 0 never.
    #[test]
    fn shmem_config_names_the_window_and_the_aperture_only_with_compute() {
        let w = super::window_config(&args(&[])).unwrap();
        let g = super::shmem_config(&w, false);
        let (n, sizes) = (g.nregions, g.memory_sizes);
        assert_eq!(n, 1);
        assert_eq!(sizes[0], 0);
        assert_eq!(sizes[1], 1 << 30);
        assert!(sizes[2..].iter().all(|&s| s == 0));

        let w = super::window_config(&args(&["--allow-compute"])).unwrap();
        let c = super::shmem_config(&w, true);
        let (n, sizes) = (c.nregions, c.memory_sizes);
        assert_eq!(n, 2);
        assert_eq!(sizes[0], 0);
        assert_eq!(sizes[1], 1 << 30);
        assert_eq!(sizes[2], device::uvmmap::APERTURE_MAX);
        assert!(sizes[3..].iter().all(|&s| s == 0));
    }

    /// `--window-size` and `--window-owner-share` make the one ZoneConfig
    /// the allocator and GET_SHMEM_CONFIG are both given; the defaults are
    /// today's window; a window that cannot be had stops the start.
    #[test]
    fn shmem_config_follows_the_window_flags() {
        let d = super::window_config(&args(&[])).unwrap();
        assert_eq!(d, ZoneConfig::default_1gib());

        let a = args(&[
            "--window-size",
            "16384",
            "--window-owner-share",
            "90",
            "--allow-compute",
        ]);
        let w = super::window_config(&a).unwrap();
        assert_eq!((w.total(), w.owner_percent), (16 << 30, 90));
        let c = super::shmem_config(&w, true);
        assert_eq!(c.memory_sizes[1], 16 << 30);
        assert_eq!(c.memory_sizes[2], device::uvmmap::APERTURE_MAX);
        let be = NvidiaBackend::with_zone_config(w);
        assert_eq!(be.shm_total_size(), c.memory_sizes[1]);

        for bad in [
            &["--window-size", "100"][..],
            &["--window-size", "1000"],
            &["--window-owner-share", "0"],
            &["--window-owner-share", "96"],
            &["--window-size", "65536", "--allow-compute"],
        ] {
            let e = super::window_config(&args(bad)).unwrap_err().to_string();
            assert!(
                e.starts_with("refusing to start: --window-"),
                "{bad:?}: {e}"
            );
        }
        assert!(super::window_config(&args(&["--window-size", "65536"])).is_ok());
    }

    use super::*;

    fn args(a: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("vhost-user-nvgpu").chain(a.iter().copied())).unwrap()
    }

    /// sd_listen_fds(3): nothing unless LISTEN_PID is this process; a lone
    /// unnamed descriptor is the vhost-user socket; two must be named; no
    /// more than two, no name twice, no name it does not know.
    #[test]
    fn socket_activation_hands_over_only_what_it_names() {
        use Handed::*;
        assert!(listen_fds(7, None, Some("1"), None).unwrap().is_empty());
        assert!(
            listen_fds(7, Some("8"), Some("1"), None)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            listen_fds(7, Some("7"), Some("1"), None).unwrap(),
            [(3, VhostUser)]
        );
        assert_eq!(
            listen_fds(7, Some("7"), Some("1"), Some("unknown")).unwrap(),
            [(3, VhostUser)]
        );
        assert_eq!(
            listen_fds(7, Some("7"), Some("2"), Some("inject:vhost-user")).unwrap(),
            [(3, Inject), (4, VhostUser)]
        );
        assert!(listen_fds(7, Some("7"), Some("2"), None).is_err());
        assert!(listen_fds(7, Some("7"), Some("2"), Some("vhost-user:vhost-user")).is_err());
        assert!(listen_fds(7, Some("7"), Some("1"), Some("nvgpu.socket")).is_err());
        assert!(listen_fds(7, Some("7"), Some("3"), None).is_err());
        assert!(listen_fds(7, Some("7"), Some("x"), None).is_err());
        assert!(
            listen_fds(7, Some("7"), Some("0"), None)
                .unwrap()
                .is_empty()
        );
    }

    /// --socket-fd stands for --socket; --inject-uid alone parses (the
    /// inject socket may come from socket activation) and is checked at start.
    #[test]
    fn socket_fd_is_instead_of_socket() {
        assert_eq!(args(&["--socket-fd", "3"]).socket_fd, Some(3));
        assert!(
            Args::try_parse_from(["vhost-user-nvgpu", "--socket", "/x", "--socket-fd", "3"])
                .is_err()
        );
        let a = args(&["--inject-uid", "950"]);
        let e = inherited_sockets(&a)
            .err()
            .expect("no inject socket")
            .to_string();
        assert!(e.contains("--inject-uid needs"), "{e}");
    }

    /// Every diagnostic flag is refused alone and announced with
    /// --diagnostic (or the environment variable); the defaults are none.
    #[test]
    fn a_diagnostic_flag_needs_diagnostic() {
        assert!(check_diagnostic(&args(&[]), false).unwrap().is_empty());
        assert!(
            check_diagnostic(&args(&["--rm-allowlist=enforce", "--sandbox=on"]), false)
                .unwrap()
                .is_empty()
        );
        for flag in [
            &["--allow-root-unsafe"][..],
            &["--proc-nvidia", "/tmp/fixture"],
            &["--permissive-abi"],
            &["--keep-guest-coherency"],
            &["--rm-allowlist=log"],
            &["--sandbox=best-effort"],
            &["--sandbox=off"],
            &["--allow-unmeasured-release"],
            &[
                "--inject-socket",
                "/x",
                "--inject-uid",
                "5",
                "--allow-inject-self",
            ],
        ] {
            let e = check_diagnostic(&args(flag), false)
                .unwrap_err()
                .to_string();
            assert!(e.contains("--diagnostic"), "{flag:?}: {e}");
            let with: Vec<&str> = flag.iter().copied().chain(["--diagnostic"]).collect();
            let lines = check_diagnostic(&args(&with), false).unwrap();
            assert_eq!(lines.len(), 1, "{flag:?}");
            assert!(lines[0].starts_with("DIAGNOSTIC: "), "{}", lines[0]);
            assert_eq!(check_diagnostic(&args(flag), true).unwrap().len(), 1);
        }
        let e = check_diagnostic(&args(&["--permissive-abi", "--sandbox=off"]), false)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("--permissive-abi, --sandbox=off are diagnostic"),
            "{e}"
        );
    }

    /// The capture helper's uid is neither root nor the backend's own, but
    /// for a rig that says so.
    #[test]
    fn the_inject_uid_is_a_user_of_its_own() {
        assert!(inject_uid_ok(950, 1000, false).is_ok());
        assert!(inject_uid_ok(0, 1000, false).is_err());
        assert!(inject_uid_ok(0, 1000, true).is_err());
        assert!(inject_uid_ok(1000, 1000, false).is_err());
        assert!(inject_uid_ok(1000, 1000, true).is_ok());
    }

    /// `--help` shows none of them unless asked with --diagnostic.
    #[test]
    fn the_diagnostic_flags_are_hidden_from_help() {
        use clap::CommandFactory;
        let help = Args::command().render_long_help().to_string();
        for hidden in [
            "--allow-root-unsafe",
            "--proc-nvidia",
            "--permissive-abi",
            "--keep-guest-coherency",
            "--allow-unmeasured-release",
            "--allow-inject-self",
        ] {
            // Named in --diagnostic's own text, never as an option line.
            assert!(
                !help.lines().any(|l| l.trim_start().starts_with(hidden)),
                "{hidden}"
            );
        }
        assert!(
            help.lines()
                .any(|l| l.trim_start().starts_with("--diagnostic"))
        );
        let shown = Args::command()
            .mut_args(|a| a.hide(false))
            .render_long_help()
            .to_string();
        assert!(
            shown
                .lines()
                .any(|l| l.trim_start().starts_with("--permissive-abi"))
        );
    }

    /// What a thread hands the pump is handed over before the backend is let
    /// go, so the next thread to take the backend cannot get its own
    /// instructions to the pump first.
    #[test]
    fn pump_instructions_are_handed_over_before_the_backend_is_let_go() {
        let shared = Arc::new(Shared {
            nvidia: Mutex::new(NvidiaBackend::with_default_zones()),
            mem: RwLock::new(None),
            pool: ExecPool::default(),
            pump: Mutex::new(PumpState::default()),
        });
        // A fresh HELLO leaves the pump a Reset and two SetV2s.
        {
            use protocol::messages::{HELLO_F_FRESH, HelloReq, PROTO_V2};
            let hello = HelloReq {
                proto: PROTO_V2,
                flags: HELLO_F_FRESH,
                guest_caps: 0,
                uvm_aperture_mib: 0,
            };
            let hdr = MsgHeader {
                msg_type: MsgType::Hello as u32,
                handle: 0,
                status: 0,
                req_id: 0,
            };
            let mut req = device::sys::pod::bytes(&hdr).to_vec();
            req.extend_from_slice(device::sys::pod::bytes(&hello));
            let _ = shared.nvidia.lock().unwrap().serve(&req, 4096);
        }
        // The pump is busy: the forwarding thread must wait for it with the
        // backend still held.
        let pump = shared.pump.lock().unwrap();
        let s = shared.clone();
        let t = std::thread::spawn(move || {
            let be = s.nvidia.lock().unwrap();
            s.forward_from(be);
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(
            shared.nvidia.try_lock().is_err(),
            "the backend is held until the pump has what it was told"
        );
        drop(pump);
        t.join().unwrap();
        let queued = &shared.pump.lock().unwrap().queued;
        assert!(matches!(queued[0], PumpCmd::Reset), "in order");
        assert!(shared.nvidia.try_lock().is_ok());
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap()
    }

    /// A UVM placement reaches the VMM as SHMEM_MAP on region 2 with the
    /// pool's base as the file offset, carrying the descriptor and asking
    /// for a reply; the withdraw as SHMEM_UNMAP of the same aperture range.
    #[test]
    fn a_uvm_placement_is_a_shmem_map_on_the_aperture_with_its_descriptor() {
        use std::os::unix::net::UnixStream;
        use std::sync::Mutex as StdMutex;
        use vhost::vhost_user::message::VhostUserMMap as M;
        use vhost::vhost_user::{FrontendReqHandler, HandlerResult};

        /// What, shm id, fd offset, shm offset, length, flags, fd open.
        type Request = (&'static str, u8, u64, u64, u64, u64, bool);
        #[derive(Default)]
        struct Vmm(StdMutex<Vec<Request>>);
        impl VhostUserFrontendReqHandler for Vmm {
            fn shmem_map(&self, r: &M, fd: &dyn std::os::fd::AsRawFd) -> HandlerResult<u64> {
                let open = device::sys::fd::is_open(fd.as_raw_fd());
                let (id, fo, so, len, fl) = (r.shmid, r.fd_offset, r.shm_offset, r.len, r.flags);
                self.0
                    .lock()
                    .unwrap()
                    .push(("map", id, fo, so, len, fl, open));
                Ok(0)
            }
            fn shmem_unmap(&self, r: &M) -> HandlerResult<u64> {
                let (id, fo, so, len, fl) = (r.shmid, r.fd_offset, r.shm_offset, r.len, r.flags);
                self.0
                    .lock()
                    .unwrap()
                    .push(("unmap", id, fo, so, len, fl, false));
                Ok(0)
            }
        }

        let vmm = Arc::new(Vmm::default());
        let mut frontend = FrontendReqHandler::new(vmm.clone()).unwrap();
        frontend.set_reply_ack_flag(true);
        // A dup of the frontend's end, owned by the stream from here.
        let tx = UnixStream::from(device::sys::fd::dup_raw(frontend.get_tx_raw_fd()).unwrap());
        let backend = Backend::from_stream(tx);
        backend.set_reply_ack_flag(true);
        backend.set_shmem_flag(true);
        let window = VhostWindow(backend);

        let served = std::thread::spawn(move || {
            frontend.handle_request().unwrap();
            frontend.handle_request().unwrap();
        });
        let file = File::open("/dev/null").unwrap();
        let x = 0x2_06e0_0000;
        window
            .place_uvm(0x40_0000, 0x20_0000, file.as_raw_fd(), x)
            .unwrap();
        window.withdraw_uvm(0x40_0000, 0x20_0000).unwrap();
        served.join().unwrap();
        let seen = vmm.0.lock().unwrap().clone();
        let w = VhostUserMMapFlags::WRITABLE.bits();
        assert_eq!(
            seen,
            vec![
                ("map", 2, x, 0x40_0000, 0x20_0000, w, true),
                ("unmap", 2, 0, 0x40_0000, 0x20_0000, 0, false),
            ]
        );
    }

    /// The threads that open descriptors of their own start once the
    /// process's descriptors are registered, never before: what they open is
    /// then never taken for the backend's own.
    fn test_backend() -> NvGpuBackend {
        NvGpuBackend {
            shared: Arc::new(Shared {
                nvidia: Mutex::new(NvidiaBackend::with_zone_config(ZoneConfig::default_256mib())),
                mem: RwLock::new(None),
                pool: ExecPool::default(),
                pump: Mutex::new(PumpState::default()),
            }),
            event_idx: false,
            config: VirtioGpuNvConfig::new("595.99.02", &[]),
            max_req: MAX_XFER_DIRECT as usize,
            max_resp: MAX_XFER_DIRECT as usize,
            mem_fds: Vec::new(),
            scanned_fds: false,
            after_scan: Vec::new(),
            allow_compute: false,
            window: ZoneConfig::default_256mib(),
            queue_poll: std::time::Duration::ZERO,
        }
    }

    /// A VMM that connects and hangs up at once: the daemon returns, the
    /// ring workers take their exit events, and dropping it -- which joins
    /// them -- returns. It never did: the workers had no exit event.
    #[test]
    fn a_vmm_that_hangs_up_at_once_lets_the_backend_exit() {
        let path = std::env::temp_dir().join(format!("nvgpu-exit-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut listener = vhost::vhost_user::Listener::new(&path, true).unwrap();
        drop(std::os::unix::net::UnixStream::connect(&path).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let backend = Arc::new(RwLock::new(test_backend()));
            let mut daemon = VhostUserDaemon::new(
                "virtio-nvgpu-test".to_string(),
                backend,
                GuestMemoryAtomic::new(GuestMemoryMmap::new()),
            )
            .unwrap();
            let _ = daemon.start(&mut listener).and_then(|()| daemon.wait());
            for h in daemon.get_epoll_handlers() {
                h.send_exit_event();
            }
            drop(daemon);
            let _ = tx.send(());
        });
        let exited = rx.recv_timeout(std::time::Duration::from_secs(10));
        let _ = std::fs::remove_file(&path);
        assert!(exited.is_ok(), "the ring workers never stopped");
    }

    #[test]
    fn accept_threads_start_after_the_descriptor_scan() {
        use std::sync::Mutex as StdMutex;
        let mut be = test_backend();
        let seen = Arc::new(StdMutex::new(Vec::new()));
        let s2 = seen.clone();
        // Open before the scan, and so registered by it: the start sees
        // that done. (A descriptor the start opens itself is no test here:
        // the scan takes every other test thread's too, and one of those
        // numbers may come back.)
        let plumbing = device::sys::fd::eventfd(libc::EFD_CLOEXEC).unwrap();
        let raw = plumbing.as_raw_fd();
        be.after_scan.push(Box::new(move || {
            s2.lock().unwrap().push(privfd::is_private(raw));
        }));
        assert!(seen.lock().unwrap().is_empty(), "not before the scan");
        be.update_memory(GuestMemoryAtomic::new(memory())).unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![true], "after it");
        drop(plumbing);
        be.update_memory(GuestMemoryAtomic::new(memory())).unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1, "once");
    }

    /// The guest's interrupt suppression is honoured: without EVENT_IDX its
    /// VRING_AVAIL_F_NO_INTERRUPT, with it its used_event.
    #[test]
    fn a_completion_interrupts_only_a_guest_that_asked() {
        use virtio_queue::Queue;
        use vm_memory::Bytes;
        let mem = memory();
        let (desc, avail, used) = (0x1000u64, 0x2000u64, 0x3000u64);
        let mut q = Queue::new(16).unwrap();
        q.set_size(16);
        q.set_desc_table_address(Some(desc as u32), Some(0));
        q.set_avail_ring_address(Some(avail as u32), Some(0));
        q.set_used_ring_address(Some(used as u32), Some(0));
        q.set_ready(true);
        let flags = |f: u16| mem.write_obj(f.to_le(), GuestAddress(avail)).unwrap();

        flags(0);
        q.add_used(&mem, 0, 0).unwrap();
        assert!(wants_interrupt(&mut q, &mem));
        flags(VRING_AVAIL_F_NO_INTERRUPT);
        q.add_used(&mem, 1, 0).unwrap();
        assert!(!wants_interrupt(&mut q, &mem), "NO_INTERRUPT");

        // EVENT_IDX: used_event sits after the avail ring's 16 entries.
        q.set_event_idx(true);
        let used_event = |v: u16| {
            mem.write_obj(v.to_le(), GuestAddress(avail + 4 + 2 * 16))
                .unwrap()
        };
        used_event(10);
        q.add_used(&mem, 2, 0).unwrap();
        assert!(!wants_interrupt(&mut q, &mem), "not yet past used_event");
        used_event(3);
        q.add_used(&mem, 3, 0).unwrap();
        assert!(wants_interrupt(&mut q, &mem), "past it");
    }

    /// A vring's eventfds are in no handle table, so the private registry
    /// has to know them for exactly as long as the ring holds them.
    #[test]
    fn a_rings_eventfds_are_private_while_it_holds_them() {
        let eventfd = || File::from(device::sys::fd::eventfd(libc::EFD_CLOEXEC).unwrap());
        let v: Vring = VringT::new(GuestMemoryAtomic::new(memory()), 256).unwrap();
        let (kick, call) = (eventfd(), eventfd());
        let (k, c) = (kick.as_raw_fd(), call.as_raw_fd());
        v.set_kick(Some(kick));
        v.set_call(Some(call));
        assert!(privfd::is_private(k) && privfd::is_private(c));
        v.set_kick(None);
        assert!(!privfd::is_private(k), "dropped with the ring's file");
        assert!(privfd::is_private(c));
        v.set_call(None);
        assert!(!privfd::is_private(c));
    }

    /// Everything that stops or moves a ring must move its epoch, so a
    /// completion taken before cannot land after.
    #[test]
    fn every_ring_reconfiguration_bumps_the_epoch() {
        let v: Vring = VringT::new(GuestMemoryAtomic::new(memory()), 256).unwrap();
        let mut last = v.epoch();
        let mut bumped = |v: &Vring| {
            let e = v.epoch();
            assert!(e > last, "epoch did not move");
            last = e;
        };
        v.set_queue_ready(false);
        bumped(&v);
        v.set_queue_info(0x1000, 0x2000, 0x3000).unwrap();
        bumped(&v);
        v.set_queue_next_avail(0);
        bumped(&v);
        v.set_queue_next_used(0);
        bumped(&v);
        v.set_enabled(false);
        bumped(&v);
        v.set_queue_size(128);
        bumped(&v);
    }
}
