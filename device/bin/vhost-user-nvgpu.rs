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

use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use device::nvidia::NvidiaBackend;
use device::virtio::{NvGpuConfig, QUEUE_SIZE, VIRTIO_ID_GPU_NV};
use vhost::vhost_user::message::{VhostUserProtocolFeatures, VhostUserVirtioFeatures};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::QueueOwnedT;
use vm_memory::{Bytes, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryLoadGuard, GuestMemoryMmap};

const QUEUE_COUNT: usize = 1;
/// Largest response we will build for one request.
const RESP_MAX: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(version, about = "vhost-user backend for virtio-nvgpu")]
struct Args {
    /// Unix socket QEMU connects to.
    #[arg(long, default_value = "/tmp/nvgpu.sock")]
    socket: String,

    /// Number of GPUs to advertise in the config space.
    #[arg(long, default_value_t = 1)]
    num_gpus: u32,
}

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    config: NvGpuConfig,
}

impl NvGpuBackend {
    fn new(num_gpus: u32) -> Self {
        Self {
            nvidia: Arc::new(Mutex::new(NvidiaBackend::with_default_zones())),
            mem: None,
            event_idx: false,
            config: NvGpuConfig {
                num_gpus,
                // Phase A forwards ioctls only. nvidia-smi needs no mapping at
                // all -- 87 ioctls and zero mmaps in the captured trace -- so a
                // guest can enumerate the GPU before the shared window exists.
                shm_bar_gpa: 0,
                shm_bar_size: 0,
            },
        }
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
        VhostUserProtocolFeatures::MQ | VhostUserProtocolFeatures::CONFIG
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &self.config as *const NvGpuConfig as *const u8,
                std::mem::size_of::<NvGpuConfig>(),
            )
        };
        let start = std::cmp::min(offset as usize, bytes.len());
        let end = std::cmp::min(start + size as usize, bytes.len());
        bytes[start..end].to_vec()
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

    let backend = Arc::new(RwLock::new(NvGpuBackend::new(args.num_gpus)));
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
