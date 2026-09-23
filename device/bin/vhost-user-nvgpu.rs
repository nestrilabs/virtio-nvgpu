//! A vhost-user backend serving `NvidiaBackend` to a guest.
//!
//! The guest driver (`driver/virtio_gpu_nv.c`) binds virtio device ID 45 and
//! posts one descriptor chain per request: a readable descriptor holding the
//! request, and a writable one for the response. That is the whole transport.
//!
//! Attach it to QEMU with the generic vhost-user device:
//!
//! ```text
//! qemu-system-x86_64 \
//!   -chardev socket,id=nv,path=/tmp/nvgpu.sock \
//!   -device vhost-user-device-pci,virtio-id=45,num_vqs=1,chardev=nv \
//!   -object memory-backend-memfd,id=mem,size=2G,share=on -numa node,memdev=mem
//! ```
//!
//! Guest memory must be shared (`memory-backend-memfd,share=on`) or the backend
//! cannot read the request the guest wrote.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use device::host;
use device::nvidia::NvidiaBackend;
use device::virtio::{VirtioGpuNvConfig, NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV};
use device::shm::WindowPlacer;
use std::os::fd::{BorrowedFd, RawFd};
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, VhostUserFrontendReqHandler};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::QueueOwnedT;
use vm_memory::{Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard, GuestMemoryMmap};

/// The driver calls `virtio_find_vqs(vdev, 2, ...)` and fails probe on the
/// error from that call, so offering fewer is fatal before config is read.
const QUEUE_COUNT: usize = NUM_QUEUES;
/// Largest response we will build for one request.
const RESP_MAX: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(version, about = "vhost-user backend for virtio-nvgpu")]
struct Args {
    /// Unix socket QEMU connects to.
    #[arg(long, default_value = "/tmp/nvgpu.sock")]
    socket: String,

    /// Where the host driver publishes itself. Overridable for testing
    /// against a fixture tree rather than a live driver.
    #[arg(long, default_value = host::PROC_NVIDIA)]
    proc_nvidia: PathBuf,
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
        // SAFETY: the descriptor is owned by the handle table for the whole of
        // this call, and is only borrowed to be sent.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
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
}

/// The shared-memory id the guest driver looks the window up by, which must
/// match the capability the VMM publishes.
const NV_SHM_ID: u8 = 1;

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    config: VirtioGpuNvConfig,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    fn new(proc_nvidia: &Path) -> anyhow::Result<Self> {
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

        Ok(Self {
            nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
            mem: None,
            event_idx: false,
            // Phase A forwards ioctls only. nvidia-smi needs no mapping at all
            // -- 100 ioctls and one mmap in the captured trace -- so a guest
            // can enumerate the GPU before the shared window exists.
            config: VirtioGpuNvConfig::new(&version, &gpus),
        })
    }

    /// Drain one virtqueue, dispatching every chain.
    fn process(
        &mut self,
        vring: &VringRwLock,
        mem: &GuestMemoryLoadGuard<GuestMemoryMmap>,
    ) -> std::io::Result<bool> {
        let mut used = false;
        loop {
            let mut guard = vring.get_mut();
            let Ok(mut avail) = guard.get_queue_mut().iter(mem.clone()) else {
                break;
            };
            let Some(chain) = avail.next() else { break };
            drop(guard);

            let head = chain.head_index();
            let mut req = Vec::new();
            let mut resp_desc = None;

            for desc in chain.clone() {
                if desc.is_write_only() {
                    resp_desc = Some(desc);
                } else {
                    let mut buf = vec![0u8; desc.len() as usize];
                    mem.read_slice(&mut buf, desc.addr()).map_err(|e| {
                        std::io::Error::other(format!("read request descriptor: {e}"))
                    })?;
                    req.extend_from_slice(&buf);
                }
            }

            let written = match resp_desc {
                Some(d) => {
                    let cap = std::cmp::min(d.len() as usize, RESP_MAX);
                    let mut resp = vec![0u8; cap];
                    let n = self
                        .nvidia
                        .lock()
                        .expect("backend mutex")
                        .dispatch(&req, &mut resp);
                    if n > 0 {
                        mem.write_slice(&resp[..n], d.addr()).map_err(|e| {
                            std::io::Error::other(format!("write response descriptor: {e}"))
                        })?;
                    }
                    n
                }
                None => {
                    log::warn!("chain {head} has no writable descriptor; dropping");
                    0
                }
            };

            vring
                .add_used(head, written as u32)
                .map_err(|e| std::io::Error::other(format!("add_used: {e}")))?;
            used = true;
        }
        Ok(used)
    }
}

impl VhostUserBackendMut for NvGpuBackend {
    type Bitmap = ();
    type Vring = VringRwLock;

    fn num_queues(&self) -> usize {
        QUEUE_COUNT
    }

    fn max_queue_size(&self) -> usize {
        QUEUE_SIZE as usize
    }

    fn features(&self) -> u64 {
        (1 << VIRTIO_F_VERSION_1)
            | (1 << VIRTIO_F_NOTIFY_ON_EMPTY)
            | (1 << VIRTIO_RING_F_EVENT_IDX)
            | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
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

    fn set_backend_req_fd(&mut self, backend: Backend) {
        log::info!("window: request channel open; device memory is now mappable");
        self.nvidia.lock().unwrap().set_window(Box::new(VhostWindow(backend)));
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        self.mem = Some(mem);
        Ok(())
    }

    fn handle_event(
        &mut self,
        device_event: u16,
        _evset: vmm_sys_util::epoll::EventSet,
        vrings: &[VringRwLock],
        _thread_id: usize,
    ) -> std::io::Result<()> {
        if device_event as usize >= QUEUE_COUNT {
            return Err(std::io::Error::other(format!(
                "event for unknown queue {device_event}"
            )));
        }
        let mem = self
            .mem
            .as_ref()
            .ok_or_else(|| std::io::Error::other("guest memory not set"))?
            .memory();

        let vring = &vrings[device_event as usize];
        if self.event_idx {
            // With EVENT_IDX the guest suppresses notifications, so re-arm and
            // drain again rather than waiting for a kick that will not come.
            loop {
                vring.disable_notification().ok();
                self.process(vring, &mem)?;
                if !vring.enable_notification().unwrap_or(false) {
                    break;
                }
            }
        } else {
            self.process(vring, &mem)?;
        }
        vring
            .signal_used_queue()
            .map_err(|e| std::io::Error::other(format!("signal used queue: {e}")))?;
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    log::info!(
        "virtio-nvgpu vhost-user backend: device id {VIRTIO_ID_GPU_NV}, socket {}",
        args.socket
    );

    let backend = Arc::new(RwLock::new(NvGpuBackend::new(&args.proc_nvidia)?));
    // vhost_user_backend::Error does not implement std::error::Error, so it
    // cannot ride `?` on its own.
    let mut daemon = VhostUserDaemon::new(
        "virtio-nvgpu".to_string(),
        backend.clone(),
        GuestMemoryAtomic::new(GuestMemoryMmap::new()),
    )
    .map_err(|e| anyhow::anyhow!("create daemon: {e:?}"))?;

    let _ = std::fs::remove_file(&args.socket);
    daemon
        .serve(&args.socket)
        .map_err(|e| anyhow::anyhow!("serve {}: {e:?}", args.socket))?;

    backend
        .write()
        .expect("backend lock")
        .nvidia
        .lock()
        .expect("nvidia lock")
        .teardown();
    log::info!("backend exited");
    Ok(())
}
