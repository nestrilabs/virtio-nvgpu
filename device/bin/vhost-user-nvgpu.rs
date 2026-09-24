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

use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use clap::Parser;
use device::host;
use device::nvidia::NvidiaBackend;
use protocol::messages::{MsgHeader, MsgType};
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

    /// Forward ioctls the ABI profile does not describe instead of refusing
    /// them, and report what was forwarded at teardown.
    ///
    /// For finding out what a workload needs that the tables lack. It hands a
    /// guest the parts of the host driver's interface nobody has checked, so it
    /// is not a way to run one.
    #[arg(long)]
    permissive_abi: bool,
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

/// What the event thread is told to start and stop watching.
enum Watch {
    Add(u32, OwnedFd),
    Remove(u32),
}

/// One `EventReady` message: a bare header naming the descriptor.
fn event_ready_bytes(handle: u32) -> Vec<u8> {
    let hdr = MsgHeader::ok(MsgType::EventReady, handle);
    // The wire form is the struct's bytes, which is what the driver reads.
    let p = &hdr as *const MsgHeader as *const u8;
    unsafe { std::slice::from_raw_parts(p, size_of::<MsgHeader>()) }.to_vec()
}

/// Put one message on the event queue, into a buffer the guest posted there.
///
/// Returns false when the guest has posted none, which is the normal state of
/// a guest whose driver predates this queue having a use -- and a reason to
/// drop the notification rather than to fail.
fn push_event(
    vring: &VringRwLock,
    mem: &GuestMemoryAtomic<GuestMemoryMmap>,
    handle: u32,
) -> bool {
    let guard = mem.memory();
    let mut vr = vring.get_mut();
    let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
        return false;
    };
    let Some(chain) = avail.next() else { return false };
    let head = chain.head_index();
    drop(vr);

    let bytes = event_ready_bytes(handle);
    let mut written = 0usize;
    for desc in chain {
        if desc.is_write_only() {
            let n = std::cmp::min(desc.len() as usize, bytes.len());
            if guard.write_slice(&bytes[..n], desc.addr()).is_ok() {
                written = n;
            }
            break;
        }
    }

    if vring.add_used(head, written as u32).is_err() {
        return false;
    }
    let _ = vring.signal_used_queue();
    written > 0
}

/// Watch the host's descriptors and tell the guest when one has something to
/// say.
///
/// This exists because the guest cannot find out any other way. NVIDIA's
/// user-mode driver waits for the GPU by polling the descriptor its RM event
/// is delivered on; the interrupt is the host's, and so is the descriptor that
/// becomes readable. Without this relay the guest's `poll` has nothing to
/// report and the driver spins -- measured at a whole core per guest at 100
/// frames a second.
///
/// A descriptor is dropped from the set after it is reported and put back a
/// millisecond later. Level-triggered polling would otherwise spin here
/// instead: the descriptor stays readable until the *guest* consumes the
/// event, which happens through an ioctl this thread never sees. Re-arming on
/// a timer costs a duplicate notification at worst, and the guest answers one
/// by waking, finding nothing, and waiting again.
fn event_pump(
    rx: Receiver<Watch>,
    vring: VringRwLock,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
) {
    // How often to re-check a descriptor that is still readable. See the
    // sweep below; this is a safety net, not the notification path.
    const SWEEP: Duration = Duration::from_millis(1);

    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        log::error!("event pump: epoll_create1: {}", std::io::Error::last_os_error());
        return;
    }
    let epfd = unsafe { OwnedFd::from_raw_fd(epfd) };

    let mut watched: HashMap<u64, OwnedFd> = HashMap::new();
    let mut last_sweep = Instant::now();

    // Edge-triggered. Level-triggered would report a descriptor as readable
    // until the *guest* consumes the event, which happens through an ioctl
    // this thread never sees -- so the pump would spin between notifying and
    // being believed. Parking the descriptor for a millisecond instead cost
    // 11% of the frames in an encode run, and parking it for 100 us cost more
    // than that, because then the pump spun on the host's CPU and took it from
    // the guest. An edge costs neither.
    let ctl = |op: i32, fd: i32, handle: u32| {
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLET) as u32,
            u64: handle as u64,
        };
        unsafe { libc::epoll_ctl(epfd.as_raw_fd(), op, fd, &mut ev) }
    };

    loop {
        // Drain the control channel first: a descriptor closed on the other
        // thread must leave the set before it can be reported again.
        loop {
            match rx.try_recv() {
                Ok(Watch::Add(handle, fd)) => {
                    if ctl(libc::EPOLL_CTL_ADD, fd.as_raw_fd(), handle) == 0 {
                        watched.insert(handle as u64, fd);
                    }
                }
                Ok(Watch::Remove(handle)) => {
                    if let Some(fd) = watched.remove(&(handle as u64)) {
                        ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd(), handle);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // The safety net: an edge can be missed if a descriptor was already
        // readable when it was added, or if a notification found no buffer
        // posted. Every 10 ms, ask the descriptors directly and re-notify the
        // ones that still have something to say. A lost wake costs a tenth of
        // a frame at 60 Hz rather than a hang.
        if last_sweep.elapsed() >= SWEEP {
            last_sweep = Instant::now();
            for (&handle, fd) in watched.iter() {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 && pfd.revents & libc::POLLIN != 0 {
                    push_event(&vring, &mem, handle as u32);
                }
            }
        }

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let n = unsafe {
            libc::epoll_wait(
                epfd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                SWEEP.as_millis() as i32,
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            log::error!("event pump: epoll_wait: {err}");
            return;
        }

        for ev in events.iter().take(n as usize) {
            // Copied out first: epoll_event is packed, so its field cannot be
            // borrowed.
            let handle = { ev.u64 } as u32;
            if !push_event(&vring, &mem, handle) {
                log::debug!("event pump: no buffer posted for handle {handle}; dropped");
            }
        }
    }
}

/// The queue the host posts events on. The guest posts empty buffers here and
/// the event pump fills them; nothing the guest sends on it is a request.
const EVENT_QUEUE: usize = 1;

/// The shared-memory id the guest driver looks the window up by, which must
/// match the capability the VMM publishes.
const NV_SHM_ID: u8 = 1;

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    config: VirtioGpuNvConfig,
    /// Started on the first message, because the event queue and guest memory
    /// are not known before then.
    watches: Option<Sender<Watch>>,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    fn new(proc_nvidia: &Path, abi_policy: device::nvidia::AbiPolicy) -> anyhow::Result<Self> {
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

        let mut nvidia = NvidiaBackend::with_default_zones();
        nvidia.set_abi_policy(abi_policy);

        Ok(Self {
            nvidia: Arc::new(Mutex::new(nvidia)),
            mem: None,
            event_idx: false,
            // Phase A forwards ioctls only. nvidia-smi needs no mapping at all
            // -- 100 ioctls and one mmap in the captured trace -- so a guest
            // can enumerate the GPU before the shared window exists.
            config: VirtioGpuNvConfig::new(&version, &gpus),
            watches: None,
        })
    }

    /// Keep the event thread's poll set in step with the descriptors the
    /// backend has open, starting the thread on first use.
    ///
    /// Each descriptor is duplicated before it is handed over. The handle table
    /// owns the original and may close it at any time; a watch holding the same
    /// number would then be watching whatever opened next.
    fn sync_watches(&mut self, vrings: &[VringRwLock]) {
        let (added, removed) = self
            .nvidia
            .lock()
            .expect("backend mutex")
            .take_watch_updates();
        if added.is_empty() && removed.is_empty() && self.watches.is_some() {
            return;
        }

        if self.watches.is_none() {
            let (Some(mem), Some(vring)) = (self.mem.clone(), vrings.get(1).cloned()) else {
                return;
            };
            let (tx, rx) = channel();
            std::thread::Builder::new()
                .name("nvgpu-events".into())
                .spawn(move || event_pump(rx, vring, mem))
                .map(|_| self.watches = Some(tx))
                .unwrap_or_else(|e| log::error!("event pump would not start: {e}"));
        }
        let Some(tx) = self.watches.as_ref() else {
            return;
        };

        for (handle, fd) in added {
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                log::warn!("watch on handle {handle}: dup: {}", std::io::Error::last_os_error());
                continue;
            }
            let _ = tx.send(Watch::Add(handle, unsafe { OwnedFd::from_raw_fd(dup) }));
        }
        for handle in removed {
            let _ = tx.send(Watch::Remove(handle));
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
        // The event queue carries buffers the guest posted for *us* to fill, not
        // requests. Serving them as requests is a loop with no bottom: each one
        // is dispatched, answered with "unknown message", handed back filled,
        // re-posted by the guest, and kicked again -- 7.4 million times in five
        // seconds, measured, the first time two guests ran at once. The pump
        // thread owns this queue; a kick on it needs no work here.
        if device_event as usize == EVENT_QUEUE {
            return Ok(());
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
        // After serving, not before: a message that opened a descriptor has to
        // have been served for the backend to know about it.
        self.sync_watches(vrings);
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

    let abi_policy = if args.permissive_abi {
        device::nvidia::AbiPolicy::Permissive
    } else {
        device::nvidia::AbiPolicy::Enforce
    };
    let backend = Arc::new(RwLock::new(NvGpuBackend::new(&args.proc_nvidia, abi_policy)?));
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
