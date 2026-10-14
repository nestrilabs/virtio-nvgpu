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

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use vmm_sys_util::eventfd::{EFD_NONBLOCK, EventFd};

use clap::Parser;
use device::caps::Caps;
use device::host;
use device::nvidia::NvidiaBackend;
use device::shm::WindowPlacer;
use device::virtio::{NUM_QUEUES, QUEUE_SIZE, VIRTIO_ID_GPU_NV, VirtioGpuNvConfig};
use protocol::messages::{MsgHeader, MsgType};
use std::os::fd::{BorrowedFd, RawFd};
use vhost::vhost_user::message::{
    VhostUserMMap, VhostUserMMapFlags, VhostUserProtocolFeatures, VhostUserVirtioFeatures,
};
use vhost::vhost_user::{Backend, VhostUserFrontendReqHandler};
use vhost_user_backend::{VhostUserBackendMut, VhostUserDaemon, VringRwLock, VringT};
use virtio_bindings::bindings::virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_VERSION_1};
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::QueueOwnedT;
use vm_memory::{
    Address, Bytes, GuestAddress, GuestAddressSpace, GuestMemoryAtomic, GuestMemoryBackend,
    GuestMemoryLoadGuard, GuestMemoryMmap, GuestMemoryRegion,
};

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

    /// What the guest is served: a comma list of graphics, compute, video and
    /// utility. Compute (CUDA, through nvidia-uvm) is off unless named.
    #[arg(long, default_value_t = Caps::DEFAULT, value_parser = Caps::parse)]
    caps: Caps,

    /// Video memory the guest may hold, in MiB. Omitted, it may take the
    /// whole card. The limit is announced in device config, so a VMM given
    /// one refuses to start a guest on a backend that is not enforcing it.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    vram_limit_mib: Option<u64>,
}

/// Places device memory through the vhost-user backend request channel.
///
/// The VMM owns the window's address space and the memory slot that describes
/// it, so it is the only process whose `MAP_FIXED` the guest can see. This
/// hands the descriptor over and lets it do the placement.
/// Guest RAM, as the backend can map from it.
///
/// A vhost-user frontend sends each region as a file descriptor, and
/// `vhost-user-backend` builds every region with `MmapRegion::from_file`, so
/// the fd and the offset within it survive into here. That is what makes a
/// host address aliasing a guest's own pages possible at all; without the fd
/// there would be only this process's mapping, which cannot be re-mapped
/// somewhere else.
///
/// Holds the atomic handle rather than a snapshot, so a memory table replaced
/// after this was built is seen rather than silently stale.
struct VhostGuestRam(GuestMemoryAtomic<GuestMemoryMmap>);

impl device::guestmem::GuestRam for VhostGuestRam {
    fn backing(&self, gpa: u64) -> Option<device::guestmem::Backing> {
        let mem = self.0.memory();
        let region = mem.find_region(GuestAddress(gpa))?;
        // A region the frontend sent without a descriptor cannot be mapped
        // from, only read through. None has been seen, and a registration is
        // refused rather than served from a mapping we cannot alias.
        let file = region.file_offset()?;
        let within = gpa.checked_sub(region.start_addr().raw_value())?;
        Some(device::guestmem::Backing {
            fd: file.file().try_clone().ok()?.into(),
            offset: file.start().checked_add(within)?,
            len: region.len().checked_sub(within)?,
        })
    }
}

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

    fn place_pool(&self, offset: u64, len: u64, fd: RawFd, addr: u64) -> device::error::Result<()> {
        // On the aperture, fd_offset is the pool's host address as well as
        // its offset in the file: UVM takes the mapping nowhere else.
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: addr,
            shm_offset: offset,
            len,
            flags: VhostUserMMapFlags::WRITABLE.bits(),
        };
        // SAFETY: as in `place`.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        self.0
            .shmem_map(&req, &borrowed)
            .map(|_| ())
            .map_err(device::error::DeviceError::Io)
    }

    fn withdraw_pool(&self, offset: u64, len: u64) -> device::error::Result<()> {
        let req = VhostUserMMap {
            shmid: NV_SHM_ID_APERTURE,
            padding: [0; 7],
            fd_offset: 0,
            shm_offset: offset,
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

struct EventControl {
    tx: Option<Sender<Watch>>,
    wake: Arc<EventFd>,
}

impl EventControl {
    fn send(&self, update: Watch) {
        if self.tx.as_ref().unwrap().send(update).is_ok() {
            self.notify();
        }
    }

    fn notify(&self) {
        if let Err(e) = self.wake.write(1) {
            log::error!("event pump: wake: {e}");
        }
    }
}

impl Drop for EventControl {
    fn drop(&mut self) {
        // Disconnect before waking, so the pump cannot miss shutdown.
        self.tx.take();
        self.notify();
    }
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
/// retain the notification for a later queue kick rather than to fail.
fn push_event(vring: &VringRwLock, mem: &GuestMemoryAtomic<GuestMemoryMmap>, handle: u32) -> bool {
    let guard = mem.memory();
    let mut vr = vring.get_mut();
    let chain = loop {
        let Ok(mut avail) = vr.get_queue_mut().iter(guard.clone()) else {
            return false;
        };
        if let Some(chain) = avail.next() {
            break chain;
        }
        drop(avail);
        // Arm EVENT_IDX and recheck a buffer posted while arming.
        if !vr.enable_notification().unwrap_or(false) {
            return false;
        }
    };
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
/// Edge-triggered polling avoids spinning on a descriptor that stays readable
/// until the guest consumes its event. The guest consumes readiness on poll,
/// so still-readable descriptors need paced retries. Quiet open descriptors
/// sleep until an edge or a control/queue wake instead of polling every millisecond.
fn event_pump(
    rx: Receiver<Watch>,
    wake: Arc<EventFd>,
    vring: VringRwLock,
    mem: GuestMemoryAtomic<GuestMemoryMmap>,
) {
    let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if epfd < 0 {
        log::error!(
            "event pump: epoll_create1: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let epfd = unsafe { OwnedFd::from_raw_fd(epfd) };

    let mut watched: HashMap<u64, OwnedFd> = HashMap::new();
    let mut ready = HashSet::new();
    let mut last_retry = Instant::now();
    const RETRY: Duration = Duration::from_millis(1);
    const CONTROL: u64 = u64::MAX;
    let mut control_event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: CONTROL,
    };
    if unsafe {
        libc::epoll_ctl(
            epfd.as_raw_fd(),
            libc::EPOLL_CTL_ADD,
            wake.as_raw_fd(),
            &mut control_event,
        )
    } < 0
    {
        log::error!(
            "event pump: control epoll_ctl: {}",
            std::io::Error::last_os_error()
        );
        return;
    }

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
                    ready.remove(&(handle as u64));
                    if let Some(fd) = watched.remove(&(handle as u64)) {
                        ctl(libc::EPOLL_CTL_DEL, fd.as_raw_fd(), handle);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return,
            }
        }

        // A successful delivery is consumed by the guest's poll(), not by
        // the host fd. Keep its paced retry until host readiness clears.
        if last_retry.elapsed() >= RETRY {
            last_retry = Instant::now();
            ready.retain(|&handle| {
                if let Some(fd) = watched.get(&handle) {
                    let mut pfd = libc::pollfd {
                        fd: fd.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let n = unsafe { libc::poll(&mut pfd, 1, 0) };
                    // An interrupted probe must not lose the only readiness edge.
                    if n < 0
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                    {
                        return true;
                    }
                    if n > 0 && pfd.revents & libc::POLLIN != 0 {
                        push_event(&vring, &mem, handle as u32);
                        return true;
                    }
                }
                false
            });
        }

        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let n = unsafe {
            libc::epoll_wait(
                epfd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as i32,
                if ready.is_empty() {
                    -1
                } else {
                    RETRY.as_millis() as i32
                },
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
            if { ev.u64 } == CONTROL {
                let _ = wake.read();
                continue;
            }
            // Copied out first: epoll_event is packed, so its field cannot be
            // borrowed.
            let handle = { ev.u64 } as u32;
            if watched.contains_key(&(handle as u64)) {
                push_event(&vring, &mem, handle);
                ready.insert(handle as u64);
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
/// The UVM aperture, where semaphore pools are placed. See `device/src/nvidia/aperture.rs`.
const NV_SHM_ID_APERTURE: u8 = 2;

struct NvGpuBackend {
    nvidia: Arc<Mutex<NvidiaBackend>>,
    mem: Option<GuestMemoryAtomic<GuestMemoryMmap>>,
    event_idx: bool,
    config: VirtioGpuNvConfig,
    /// Started on the first message, because the event queue and guest memory
    /// are not known before then.
    watches: Option<EventControl>,
    /// The request and response of the chain being served, kept across chains.
    /// A fresh 64 KiB response zeroed per request cost more than the host's
    /// whole RM call, and only the bytes dispatch writes are sent back.
    req: Vec<u8>,
    resp: Vec<u8>,
}

impl NvGpuBackend {
    /// Build a backend describing the GPUs this host actually has.
    ///
    /// The guest driver rejects `num_gpus == 0`, so a host with no NVIDIA
    /// module loaded is refused here, where the reason can be stated, rather
    /// than in a guest as a bare -EINVAL from probe.
    fn new(proc_nvidia: &Path, caps: Caps, vram_limit_mib: Option<u64>) -> anyhow::Result<Self> {
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
        let release = abi::version::DriverVersion::parse(&version)
            .ok_or_else(|| anyhow::anyhow!("host driver version {version:?} does not parse"))?;
        nvidia
            .set_host_driver_version(release)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;
        nvidia.set_caps(caps);
        nvidia
            .set_vram_limit_mib(vram_limit_mib)
            .map_err(|e| anyhow::anyhow!("refusing to start: {e}"))?;

        let nvidia_vram_mib = nvidia.vram_limit_mib();
        Ok(Self {
            nvidia: Arc::new(Mutex::new(nvidia)),
            mem: None,
            event_idx: false,
            // Phase A forwards ioctls only. nvidia-smi needs no mapping at all
            // -- 100 ioctls and one mmap in the captured trace -- so a guest
            // can enumerate the GPU before the shared window exists.
            config: VirtioGpuNvConfig::new(&version, &gpus, caps, nvidia_vram_mib),
            watches: None,
            req: Vec::new(),
            resp: vec![0u8; RESP_MAX],
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
            let wake = match EventFd::new(EFD_NONBLOCK) {
                Ok(wake) => Arc::new(wake),
                Err(e) => {
                    log::error!("event pump: eventfd: {e}");
                    return;
                }
            };
            let pump_wake = wake.clone();
            std::thread::Builder::new()
                .name("nvgpu-events".into())
                .spawn(move || event_pump(rx, pump_wake, vring, mem))
                .map(|_| self.watches = Some(EventControl { tx: Some(tx), wake }))
                .unwrap_or_else(|e| log::error!("event pump would not start: {e}"));
        }
        let Some(tx) = self.watches.as_ref() else {
            return;
        };

        for (handle, fd) in added {
            let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
            if dup < 0 {
                log::warn!(
                    "watch on handle {handle}: dup: {}",
                    std::io::Error::last_os_error()
                );
                continue;
            }
            tx.send(Watch::Add(handle, unsafe { OwnedFd::from_raw_fd(dup) }));
        }
        for handle in removed {
            tx.send(Watch::Remove(handle));
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
            let mut resp_desc = None;
            self.req.clear();

            for desc in chain.clone() {
                if desc.is_write_only() {
                    resp_desc = Some(desc);
                } else {
                    let at = self.req.len();
                    self.req.resize(at + desc.len() as usize, 0);
                    mem.read_slice(&mut self.req[at..], desc.addr())
                        .map_err(|e| {
                            std::io::Error::other(format!("read request descriptor: {e}"))
                        })?;
                }
            }

            let written = match resp_desc {
                Some(d) => {
                    let cap = std::cmp::min(d.len() as usize, RESP_MAX);
                    let resp = &mut self.resp[..cap];
                    let n = self
                        .nvidia
                        .lock()
                        .expect("backend mutex")
                        .dispatch(&self.req, resp);
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
        self.nvidia
            .lock()
            .unwrap()
            .set_window(Box::new(VhostWindow(backend)));
    }

    fn get_config(&self, offset: u32, size: u32) -> Vec<u8> {
        self.config.read(offset, size)
    }

    fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx = enabled;
    }

    fn update_memory(&mut self, mem: GuestMemoryAtomic<GuestMemoryMmap>) -> std::io::Result<()> {
        // Also the backend's, so memory a guest registers by CPU address can
        // be found. The handle is atomic, so a later table replaces what this
        // one sees rather than leaving the backend on a stale set of regions.
        self.nvidia
            .lock()
            .unwrap()
            .set_guest_ram(Box::new(VhostGuestRam(mem.clone())));
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
        // thread owns this queue; a kick wakes its paced retry if a buffer is available.
        if device_event as usize == EVENT_QUEUE {
            if let Some(control) = &self.watches {
                control.notify();
            }
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
    // Before any device is opened: the host driver judges every guest call by
    // this process's credentials (device::posture).
    device::posture::enforce()?;

    // The sandbox, before anything else: Landlock and seccomp cover this
    // thread and every thread started after them, and `VhostUserDaemon::new`
    // starts one. Everything the backend needs afterwards -- the GPU nodes,
    // the driver's own trees, and the directory its socket is bound in -- is
    // named here, because a ruleset cannot be added to once it is in force.
    let socket_dir = Path::new(&args.socket)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let report = device::sandbox::enter(&device::sandbox::Paths {
        devices: device::sandbox::gpu_nodes(),
        read_only: vec![
            args.proc_nvidia.clone(),
            PathBuf::from("/sys/class/drm"),
            PathBuf::from("/sys/bus/pci/devices"),
            PathBuf::from("/sys/devices"),
        ],
        sockets: vec![socket_dir],
    })?;
    log::info!(
        "sandbox: landlock ABI {}, {} seccomp instructions; no path outside the GPU nodes, the \
         driver's own trees and the socket's directory, no executable mapping, no process, no \
         socket but AF_UNIX",
        report.landlock_abi,
        report.seccomp_rules
    );

    log::info!(
        "virtio-nvgpu vhost-user backend: device id {VIRTIO_ID_GPU_NV}, socket {}, caps {}, {}",
        args.socket,
        args.caps,
        match args.vram_limit_mib {
            Some(m) => format!("video memory limited to {m} MiB"),
            None => "no video memory limit".to_string(),
        }
    );

    let backend = Arc::new(RwLock::new(NvGpuBackend::new(
        &args.proc_nvidia,
        args.caps,
        args.vram_limit_mib,
    )?));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::IntoRawFd;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn queue() -> (GuestMemoryAtomic<GuestMemoryMmap>, VringRwLock) {
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap(),
        );
        let vring = VringRwLock::new(mem.clone(), 8).unwrap();
        vring.set_queue_info(0x1000, 0x2000, 0x3000).unwrap();
        vring.set_queue_size(8);
        vring.set_queue_ready(true);
        vring.set_queue_event_idx(true);
        (mem, vring)
    }

    fn descriptor(mem: &GuestMemoryMmap, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
        let mut bytes = Vec::new();
        bytes.extend(addr.to_le_bytes());
        bytes.extend(len.to_le_bytes());
        bytes.extend(flags.to_le_bytes());
        bytes.extend(next.to_le_bytes());
        mem.write_slice(&bytes, GuestAddress(0x1000 + u64::from(index) * 16))
            .unwrap();
    }

    fn post(mem: &GuestMemoryMmap, index: u16, head: u16) {
        mem.write_obj(
            head.to_le(),
            GuestAddress(0x2004 + u64::from(index % 8) * 2),
        )
        .unwrap();
        mem.store((index + 1).to_le(), GuestAddress(0x2002), Ordering::Release)
            .unwrap();
    }

    fn used(mem: &GuestMemoryMmap) -> u16 {
        u16::from_le(mem.load(GuestAddress(0x3002), Ordering::Acquire).unwrap())
    }

    fn wait_used(mem: &GuestMemoryMmap, count: u16) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while used(mem) != count {
            assert!(
                Instant::now() < deadline,
                "event delivery timed out at {} of {count}",
                used(mem)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn event_pump_sleeps_on_quiet_descriptors_and_preserves_readiness_retries() {
        let (mem, vring) = queue();
        let wake = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let gpu = EventFd::new(EFD_NONBLOCK).unwrap();
        let (tx, rx) = channel();
        let control = EventControl {
            tx: Some(tx),
            wake: wake.clone(),
        };
        let (started, tid) = channel();
        let pump_mem = mem.clone();
        let pump_vring = vring.clone();
        let pump = std::thread::spawn(move || {
            started
                .send(unsafe { libc::syscall(libc::SYS_gettid) })
                .unwrap();
            event_pump(rx, wake, pump_vring, pump_mem);
        });
        let path = format!("/proc/self/task/{}/status", tid.recv().unwrap());
        let switches = || -> u64 {
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        };
        control.send(Watch::Add(7, unsafe {
            OwnedFd::from_raw_fd(gpu.try_clone().unwrap().into_raw_fd())
        }));
        std::thread::sleep(Duration::from_millis(30));
        let before = switches();
        std::thread::sleep(Duration::from_millis(100));
        let wakes = switches() - before;
        assert!(
            wakes <= 1,
            "quiet open descriptor woke {wakes} times in 100 ms"
        );
        gpu.write(1).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        let guard = mem.memory();
        assert_eq!(used(&guard), 0);
        assert_eq!(
            guard
                .read_obj::<u16>(GuestAddress(0x3000 + 4 + 8 * 8))
                .unwrap(),
            0,
            "empty event queue must request a guest kick"
        );
        descriptor(&guard, 0, 0x4000, 16, 2, 0);
        post(&guard, 0, 0);
        control.notify();
        wait_used(&guard, 1);
        let mut bytes = [0; 16];
        guard.read_slice(&mut bytes, GuestAddress(0x4000)).unwrap();
        assert_eq!(bytes.as_slice(), event_ready_bytes(7));

        // A guest poll consumes its flag, so host readiness must be repeated.
        descriptor(&guard, 1, 0x4100, 16, 2, 0);
        post(&guard, 1, 1);
        control.notify();
        wait_used(&guard, 2);
        gpu.read().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        // Exercise wraparound and both queue-kick/event arrival orders.
        for index in 2..66u16 {
            let head = index % 8;
            let lock = vring.get_mut();
            descriptor(&guard, head, 0x4000 + u64::from(head) * 256, 16, 2, 0);
            if index % 2 == 0 {
                gpu.write(1).unwrap();
            }
            post(&guard, index, head);
            drop(lock);
            control.notify();
            if index % 2 != 0 {
                gpu.write(1).unwrap();
            }
            wait_used(&guard, index + 1);
            gpu.read().unwrap();
            std::thread::sleep(Duration::from_millis(3));
        }
        // Remove an event that could not be delivered, then post a buffer.
        gpu.write(1).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        control.send(Watch::Remove(7));
        std::thread::sleep(Duration::from_millis(30));
        descriptor(&guard, 2, 0x4200, 16, 2, 0);
        post(&guard, 66, 2);
        control.notify();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(used(&guard), 66);
        // Reusing the guest handle must not revive the old fd's pending event.
        let replacement = EventFd::new(EFD_NONBLOCK).unwrap();
        control.send(Watch::Add(7, unsafe {
            OwnedFd::from_raw_fd(replacement.try_clone().unwrap().into_raw_fd())
        }));
        control.notify();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(used(&guard), 66);
        replacement.write(1).unwrap();
        wait_used(&guard, 67);
        replacement.read().unwrap();
        std::thread::sleep(Duration::from_millis(10));
        // More ready handles than queue slots must all survive backpressure.
        let many: Vec<_> = (100..110)
            .map(|handle| {
                let fd = EventFd::new(EFD_NONBLOCK).unwrap();
                fd.write(1).unwrap();
                control.send(Watch::Add(handle, unsafe {
                    OwnedFd::from_raw_fd(fd.try_clone().unwrap().into_raw_fd())
                }));
                fd
            })
            .collect();
        std::thread::sleep(Duration::from_millis(30));
        let mut delivered = HashSet::new();
        for (start, count) in [(67u16, 8u16), (75, 2)] {
            let lock = vring.get_mut();
            for index in start..start + count {
                let head = index % 8;
                descriptor(&guard, head, 0x4000 + u64::from(head) * 256, 16, 2, 0);
                post(&guard, index, head);
            }
            drop(lock);
            control.notify();
            wait_used(&guard, start + count);
            for index in start..start + count {
                let handle = guard
                    .read_obj::<u32>(GuestAddress(0x4004 + u64::from(index % 8) * 256))
                    .unwrap();
                assert!(delivered.insert(handle), "duplicate event for {handle}");
                many[(handle - 100) as usize].read().unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(delivered, (100..110).collect());
        drop(many);
        drop(control);
        pump.join().unwrap();
    }

    #[test]
    fn quiet_event_pump_stops_when_control_is_dropped() {
        let (mem, vring) = queue();
        let wake = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let (tx, rx) = channel();
        let control = EventControl {
            tx: Some(tx),
            wake: wake.clone(),
        };
        let (done, stopped) = channel();
        let pump = std::thread::spawn(move || {
            event_pump(rx, wake, vring, mem);
            done.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(10));
        drop(control);
        stopped
            .recv_timeout(Duration::from_secs(2))
            .expect("quiet pump did not stop");
        pump.join().unwrap();
    }

    #[test]
    #[ignore = "real NVIDIA quiet-wait benchmark; serialize GPU benchmarks"]
    fn quiet_nvidia_descriptor_benchmark() {
        let (mem, vring) = queue();
        let wake = Arc::new(EventFd::new(EFD_NONBLOCK).unwrap());
        let gpu: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/nvidiactl")
            .unwrap()
            .into();
        let mut pfd = libc::pollfd {
            fd: gpu.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut pfd, 1, 0) },
            0,
            "descriptor is not quiet"
        );
        let (tx, rx) = channel();
        let control = EventControl {
            tx: Some(tx),
            wake: wake.clone(),
        };
        let (started, tid) = channel();
        let pump = std::thread::spawn(move || {
            started
                .send(unsafe { libc::syscall(libc::SYS_gettid) })
                .unwrap();
            event_pump(rx, wake, vring, mem);
        });
        let dir = format!("/proc/self/task/{}", tid.recv().unwrap());
        let sample = || -> (u64, u64) {
            let status = std::fs::read_to_string(format!("{dir}/status")).unwrap();
            let switches = status
                .lines()
                .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let stat = std::fs::read_to_string(format!("{dir}/stat")).unwrap();
            let fields: Vec<_> = stat[stat.rfind(')').unwrap() + 2..]
                .split_whitespace()
                .collect();
            let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
            (switches, ticks)
        };
        control.send(Watch::Add(7, gpu));
        std::thread::sleep(Duration::from_millis(100));
        for run in 0..5 {
            let before = sample();
            let start = Instant::now();
            std::thread::sleep(Duration::from_secs(2));
            let after = sample();
            println!(
                "QUIET_NVIDIA run={run} ms={} voluntary_wakes={} cpu_ticks={}",
                start.elapsed().as_millis(),
                after.0 - before.0,
                after.1 - before.1
            );
        }
        drop(control);
        pump.join().unwrap();
    }
}
