//! KMS on the backend, beyond what the IOCTL2 interpreter (`xfer`) does for
//! every call: how a property is classified by its name, and the host card
//! hotplug listener.
//!
//! **Properties.** A property's value is an integer, an object id or a blob
//! id -- except for the few whose value is a descriptor the kernel resolves in
//! *our* table (`IN_FENCE_FD`, drm_atomic_uapi.c:558) or a pointer it writes
//! through in *our* address space (`OUT_FENCE_PTR`, :473;
//! `WRITEBACK_OUT_FENCE_PTR`, drm_writeback.c:134; nvidia-drm's
//! `NV_DRM_OUT_FENCE_PTR`, nvidia-drm-drv.c:546). Property ids are
//! device-global, so a guest can name those on any file, through ATOMIC or
//! the legacy setters (RV:setprop). `xfer` refuses every non-plain property on
//! the setters and requires the fence hook's consent and a translation record
//! on ATOMIC; [`prop_kind`] decides what "non-plain" is. Beyond the names we
//! know, a property a newer kernel or driver calls `*_PTR` or `*_FD` is taken
//! for a pointer or a descriptor: guessing wrong that way refuses a commit,
//! guessing wrong the other way lets a guest aim a host kernel write. The
//! guest classifies by the same rule (nvgpu_kms.c `nvgpu_kms_prop_kind_of`).
//!
//! **Hotplug.** A host connector change reaches userspace only as a uevent on
//! the card's device (`HOTPLUG=1`, drm_sysfs.c:444), and a lessee going away
//! as another (`LEASE=1`, drm_sysfs.c:423, drm_lease.c:293); nothing arrives
//! on the DRM file. A guest compositor driving our card needs both -- aquamarine
//! re-scans connectors on the first and leases on the second, and has no
//! polling fallback (R:hyprguest §9). So [`HotplugListener`] listens on the
//! kernel's uevent netlink group, picks out `change` events of our card nodes
//! by major:minor, and hands them to the pump as `EV_HOTPLUG`, which the guest
//! turns back into the same uevents on its own card (nvgpu_xfer.c).
//!
//! Joining the group needs no privilege: the uevent socket is created with
//! `NL_CFG_F_NONROOT_RECV` (lib/kobject_uevent.c:779), which is what
//! `netlink_bind` checks for group membership (net/netlink/af_netlink.c:990),
//! and the kernel broadcasts to group 1 of every uevent socket
//! (kobject_uevent.c:302, 330). Only the kernel may send to it
//! (no `NL_CFG_F_NONROOT_SEND`), and a message whose sender port is not 0 is
//! ignored anyway.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use protocol::messages::{EV_HOTPLUG_F_HOTPLUG, EV_HOTPLUG_F_LEASE};

use crate::hostfd::{CardNode, HandleKind};
use crate::nvidia::NvidiaBackend;
use crate::privfd::PrivateFd;
use crate::pump::PumpCmd;
use crate::xfer::{self, PropKind};

// ───────────────────────────── properties ─────────────────────────────

/// What a property's value is, by its (NUL-trimmed) name.
pub fn prop_kind(name: &[u8]) -> PropKind {
    match xfer::default_prop_kind(name) {
        PropKind::Plain if name.ends_with(b"_PTR") => PropKind::OutPtr,
        PropKind::Plain if name.ends_with(b"_FD") => PropKind::FenceFd,
        k => k,
    }
}

// ───────────────────────────── uevents ─────────────────────────────

/// The parts of a kernel uevent this module looks at.
///
/// The message is `ACTION@DEVPATH\0` followed by `KEY=VALUE\0` pairs
/// (kobject_uevent.c:284-297, the environment built at :560-660 and by
/// `dev_uevent`, drivers/base/core.c:2729-2740).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Uevent<'a> {
    pub action: &'a [u8],
    pub subsystem: &'a [u8],
    pub major: Option<u32>,
    pub minor: Option<u32>,
    pub hotplug: bool,
    pub lease: bool,
}

/// Parse one uevent datagram. `None` for anything that is not one (the udev
/// daemon's re-broadcasts go to group 2 in their own format and never reach
/// group 1, but a malformed message is not taken on trust either).
pub fn parse_uevent(buf: &[u8]) -> Option<Uevent<'_>> {
    let mut parts = buf.split(|&b| b == 0);
    let head = parts.next()?;
    let at = head.iter().position(|&b| b == b'@')?;
    let mut ev = Uevent {
        action: &head[..at],
        ..Uevent::default()
    };
    let num = |v: &[u8]| std::str::from_utf8(v).ok()?.parse::<u32>().ok();
    for kv in parts {
        let Some(eq) = kv.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (k, v) = (&kv[..eq], &kv[eq + 1..]);
        match k {
            b"ACTION" => ev.action = v,
            b"SUBSYSTEM" => ev.subsystem = v,
            b"MAJOR" => ev.major = num(v),
            b"MINOR" => ev.minor = num(v),
            b"HOTPLUG" => ev.hotplug = v == b"1",
            b"LEASE" => ev.lease = v == b"1",
            _ => {}
        }
    }
    Some(ev)
}

/// The card index and `EV_HOTPLUG_F_*` flags a uevent means for the guest,
/// if it is a hotplug or lease change of one of our card nodes. Matched by
/// device number, which is what identifies the node (the guest's aquamarine
/// compares `devnum` the same way, R:hyprguest §9), not by a name that a
/// renamed node would not keep.
pub fn card_event(ev: &Uevent<'_>, cards: &[CardNode]) -> Option<(u32, u32)> {
    if ev.action != b"change" || ev.subsystem != b"drm" {
        return None;
    }
    let (major, minor) = (ev.major?, ev.minor?);
    let card = cards
        .iter()
        .position(|c| c.major == major && c.minor == minor)?;
    let mut flags = 0;
    if ev.hotplug {
        flags |= EV_HOTPLUG_F_HOTPLUG;
    }
    if ev.lease {
        flags |= EV_HOTPLUG_F_LEASE;
    }
    (flags != 0).then_some((card as u32, flags))
}

// ───────────────────────────── the listener ─────────────────────────────

/// `NETLINK_KOBJECT_UEVENT`'s kernel multicast group.
const UEVENT_GROUP_KERNEL: u32 = 1;
/// Room for a burst of uevents (a GPU reset changes every connector at once);
/// past it the kernel drops and we see ENOBUFS.
const RCVBUF: libc::c_int = 1 << 20;
/// A uevent is at most UEVENT_BUFFER_SIZE (2048) of environment plus the
/// header (include/linux/kobject.h:32).
const MSG_MAX: usize = 8192;

/// A thread forwarding hotplug and lease uevents of our card nodes to the
/// event pump, and running a backend check on a timer (`Tick`). Stopped and
/// joined when dropped.
pub struct HotplugListener {
    stop: PrivateFd,
    alarm: Arc<LeaseAlarm>,
    thread: Option<JoinHandle<()>>,
}

/// What the listener runs every `every`, and at once when its alarm rings:
/// the re-check of the leases NVKMS grants rest on ("lease ends" below),
/// which must not wait for the guest's next NVKMS call.
pub struct Tick {
    pub every: Duration,
    pub run: Box<dyn Fn() + Send>,
}

impl HotplugListener {
    /// Start listening for `cards`, sending each change through `sink` (the
    /// transport's pump forwarding). Fails if the socket cannot be made; the
    /// backend then runs without hotplug, which a guest sees only as a
    /// compositor that misses monitor changes.
    pub fn spawn(
        cards: Vec<CardNode>,
        sink: impl Fn(PumpCmd) + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_ticking(cards, sink, None)
    }

    /// `spawn`, also running `tick`.
    pub fn spawn_ticking(
        cards: Vec<CardNode>,
        sink: impl Fn(PumpCmd) + Send + 'static,
        tick: Option<Tick>,
    ) -> io::Result<Self> {
        let sock = uevent_socket()?;
        // SAFETY: plain syscall; the result is owned below.
        let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor eventfd() just returned.
        let stop = PrivateFd::new(unsafe { OwnedFd::from_raw_fd(stop) });
        let stop_rx = stop.try_clone()?;
        let alarm = Arc::new(LeaseAlarm::new()?);
        let alarm_rx = alarm.clone();
        let thread = std::thread::Builder::new()
            .name("nvgpu-hotplug".into())
            .spawn(move || listen(sock, stop_rx, alarm_rx, tick, cards, sink))?;
        Ok(Self {
            stop,
            alarm,
            thread: Some(thread),
        })
    }

    /// What makes the tick run now (the Wayland proxy rings it when a
    /// connection to the host compositor hangs up).
    pub fn alarm(&self) -> Arc<LeaseAlarm> {
        self.alarm.clone()
    }
}

impl Drop for HotplugListener {
    fn drop(&mut self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a local to an eventfd this owns.
        unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A non-blocking socket on the kernel uevent group.
fn uevent_socket() -> io::Result<PrivateFd> {
    // SAFETY: plain syscall; the result is owned below.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor socket() just returned.
    let sock = PrivateFd::new(unsafe { OwnedFd::from_raw_fd(fd) });
    // Best effort: without it only a large burst is at risk, and ENOBUFS then
    // tells us so.
    // SAFETY: setsockopt with a local int of the size given.
    unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &RCVBUF as *const libc::c_int as *const libc::c_void,
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    // SAFETY: an all-zero sockaddr_nl is valid; the fields that matter are
    // set below.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    sa.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    sa.nl_groups = UEVENT_GROUP_KERNEL;
    // SAFETY: binds to a sockaddr_nl of the size given.
    let r = unsafe {
        libc::bind(
            sock.as_raw_fd(),
            &sa as *const libc::sockaddr_nl as *const libc::sockaddr,
            size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(sock)
}

/// What one receive produced.
#[derive(Debug, PartialEq, Eq)]
enum Recv {
    /// A datagram from the kernel, `n` bytes.
    Kernel(usize),
    /// From anyone else: ignored.
    Foreign,
    /// The kernel dropped uevents for want of buffer (ENOBUFS).
    Overrun,
    /// Nothing more to read now.
    Empty,
}

fn recv(fd: RawFd, buf: &mut [u8]) -> io::Result<Recv> {
    // SAFETY: an all-zero sockaddr_nl is valid for the kernel to fill.
    let mut sa: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    let mut sa_len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
    // SAFETY: receives into a buffer and a sockaddr this function owns, of
    // the sizes given.
    let n = unsafe {
        libc::recvfrom(
            fd,
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            &mut sa as *mut libc::sockaddr_nl as *mut libc::sockaddr,
            &mut sa_len,
        )
    };
    if n < 0 {
        let e = io::Error::last_os_error();
        return match e.raw_os_error() {
            Some(libc::EAGAIN) => Ok(Recv::Empty),
            Some(libc::EINTR) => recv(fd, buf),
            Some(libc::ENOBUFS) => Ok(Recv::Overrun),
            _ => Err(e),
        };
    }
    Ok(if sa.nl_pid == 0 {
        Recv::Kernel(n as usize)
    } else {
        Recv::Foreign
    })
}

fn listen(
    sock: PrivateFd,
    stop: PrivateFd,
    alarm: Arc<LeaseAlarm>,
    tick: Option<Tick>,
    cards: Vec<CardNode>,
    sink: impl Fn(PumpCmd),
) {
    log::info!(
        "hotplug: listening for uevents of {} card node(s)",
        cards.len()
    );
    let mut buf = vec![0u8; MSG_MAX];
    let mut last_tick = Instant::now();
    loop {
        let mut fds = [
            libc::pollfd {
                fd: sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: alarm.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Until the next tick is due (uevents arriving must not starve it).
        let timeout = tick.as_ref().map_or(-1, |t| {
            t.every.saturating_sub(last_tick.elapsed()).as_millis() as libc::c_int
        });
        // SAFETY: polls three pollfds this function owns.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 3, timeout) };
        if r < 0 {
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            log::error!("hotplug: poll: {}", io::Error::last_os_error());
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        let rung = fds[2].revents != 0;
        if rung {
            alarm.clear();
        }
        if let Some(t) = &tick
            && (rung || last_tick.elapsed() >= t.every)
        {
            (t.run)();
            last_tick = Instant::now();
        }
        loop {
            match recv(sock.as_raw_fd(), &mut buf) {
                Ok(Recv::Kernel(n)) => {
                    if let Some((card, flags)) =
                        parse_uevent(&buf[..n]).and_then(|ev| card_event(&ev, &cards))
                    {
                        log::debug!("hotplug: card {card} flags {flags:#x}");
                        sink(PumpCmd::Hotplug { card, flags });
                    }
                }
                Ok(Recv::Foreign) => {}
                Ok(Recv::Overrun) => {
                    // Something about our cards may have been among what was
                    // lost; a spurious HOTPLUG costs a guest connector rescan,
                    // a lost one a monitor it never sees.
                    log::warn!("hotplug: uevents were dropped; telling the guest to rescan");
                    for card in 0..cards.len() as u32 {
                        sink(PumpCmd::Hotplug {
                            card,
                            flags: EV_HOTPLUG_F_HOTPLUG | EV_HOTPLUG_F_LEASE,
                        });
                    }
                }
                Ok(Recv::Empty) => break,
                Err(e) => {
                    log::error!("hotplug: recv: {e}; no more hotplug events");
                    return;
                }
            }
        }
    }
}

// ───────────────────────────── lease ends ─────────────────────────────
//
// NVKMS permissions granted through a lease (nvidia-drm GRANT_PERMISSIONS on
// a `DrmLease` handle) are recorded by the NVKMS policy against that handle,
// and every head-level NVKMS call the guest makes is held to those records
// (nvkms.rs): the unchecked cursor, LUT and attribute commands, and FLIP and
// SET_MODE. Closing the handle clears them, and nvidia-drm's postclose takes
// the host's grants back with it (nvidia-drm-drv.c:1588-1600). The lease can
// also end while the handle stays open, which the host tells nobody about
// directly, and after which nvidia-drm does not always take its grants back:
//
// - the lessor revoking it (DRM_IOCTL_MODE_REVOKE_LEASE, drm_lease.c:725)
//   empties the lessee's object idr (`_drm_lease_revoke`, :300-333); that
//   ioctl passes through nvidia-drm's wrapper, which revokes the grants on
//   the revoked connectors (nvidia-drm-drv.c:1750-1774). GET_LEASE on the
//   lessee then counts zero objects (:636-684). No uevent: only a lessee's
//   destruction sends one (`LEASE=1`, :293);
// - the lessor closing its file (the host compositor exiting) revokes the
//   same way, from drm_master_release (drm_auth.c:351-357), which is *not*
//   an ioctl and bypasses that wrapper: the grants stay on the lessee's
//   file. nvidia-drm's master_drop, which runs too, revokes only the
//   dropping file's own grants (nvidia-drm-drv.c:1038-1043). And since the
//   lessor is no longer master, GET_LEASE (a DRM_MASTER ioctl,
//   drm_ioctl.c:746) on the lessee answers -EACCES;
// - the lessor dropping master (a VT switch) leaves the lease in place and
//   the lessee's grants with it; GET_LEASE answers -EACCES until master
//   comes back.
//
// So the host's NVKMS may go on honouring a grant whose lease is gone, and
// when the lessee's file finally closes, its postclose disables the
// granted connectors (nvidia-drm-drv.c:1497-1523) -- which by then a
// restarted host compositor may have taken back. The backend therefore asks
// (`lease_state`) and acts on the answer:
//
// - zero objects, from GET_LEASE, or from GETRESOURCES when GET_LEASE says
//   -EACCES (a lessee's GETRESOURCES lists only what its lease holds,
//   drm_mode_config.c:131-172 through drm_lease_held, and needs no master):
//   the lease is gone for good (a lease cannot be refilled). If a grant was
//   ever made through the handle, the backend closes its host file at once,
//   so postclose runs now rather than after the next compositor starts, and
//   leaves the handle behind as a stub that answers ENODEV until the guest
//   closes it (`HandleTable::bury`). Not in `--kms-card`: there the guest is
//   the lessor, the connectors are its own, and a revoked lessee keeps the
//   native behaviour of an open file with an empty lease. A host mapping
//   of the file (a dumb buffer mapped through it) holds the file too, and
//   postclose then waits for the guest to unmap it;
// - -EACCES with objects still listed: the lessor dropped master. The
//   records go (a gate, over-clearing: the guest must grant again), the
//   file stays, and the handle stays on the list asked about, in case the
//   lease ends for good later;
// - objects: nothing to do.
//
// It asks at four moments:
//
// 1. before every NVKMS call, for the handles that granted something still
//    recorded or whose lease ended after they had -- the only moment the
//    records matter to the guest;
// 2. after a REVOKE_LEASE that a guest ran through one of our card handles
//    (compositor-VM mode: the guest is the lessor);
// 3. on a `LEASE=1` uevent for a card (a lessee went away), and on the
//    hotplug listener's timer (`Tick`, once a second), so a lease that ended
//    is closed even while the guest makes no NVKMS call;
// 4. when a connection to the host compositor hangs up (`LeaseAlarm`, rung
//    by the Wayland proxy): the compositor is the usual lessor, and its exit
//    is the case nvidia-drm does not follow. The alarm can win the race with
//    the compositor's DRM file being released; the timer catches it then.
//
// What cannot be seen this way: a connector being unplugged (NVKMS drops the
// dpy's permissions, the lease keeps its objects) -- harmless, as NVKMS then
// refuses the head itself -- and a lease that ends and is re-granted between
// two checks (the records are then of the old grant, which the host has
// re-issued anyway).

/// `DRM_IOCTL_MODE_GET_LEASE`: `_IOWR('d', 0xC8, struct drm_mode_get_lease)`,
/// `{ u32 count_objects; u32 pad; u64 objects_ptr; }`.
pub const DRM_IOCTL_MODE_GET_LEASE: u32 = 0xc010_64c8;

/// `DRM_IOCTL_MODE_GETRESOURCES`: `_IOWR('d', 0xA0, struct drm_mode_card_res)`,
/// four u64 id-list pointers, then `count_fbs, count_crtcs,
/// count_connectors, count_encoders` and the four size limits (64 bytes).
pub const DRM_IOCTL_MODE_GETRESOURCES: u32 = 0xc040_64a0;

/// What a lessee's file says about its lease.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// It holds objects, and its lessor is master.
    Holds,
    /// It holds nothing: revoked, or its lessor closed. For good.
    Empty,
    /// Its lessor is not master (a VT switch), and it still holds objects.
    NotMaster,
}

/// Ask the lessee file `fd` about its lease: `Err` when the calls could not
/// say (a descriptor that is no DRM file, say). Both calls are counts only;
/// nothing is written through a pointer.
pub fn lease_state(sys: &dyn xfer::Sys, fd: RawFd) -> Result<LeaseState, xfer::Errno> {
    let mut arg = [0u8; 16];
    let r = sys.ioctl(fd, DRM_IOCTL_MODE_GET_LEASE, arg.as_mut_ptr());
    let count = |a: &[u8], off: usize| u32::from_le_bytes(a[off..off + 4].try_into().unwrap());
    match r {
        0.. if count(&arg, 0) != 0 => return Ok(LeaseState::Holds),
        0.. => return Ok(LeaseState::Empty),
        _ if -r == libc::EACCES => {}
        _ => return Err(-r),
    }
    let mut res = [0u8; 64];
    let r = sys.ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES, res.as_mut_ptr());
    if r < 0 {
        return Err(-r);
    }
    // count_crtcs and count_connectors: a lease holds at least one of each
    // (drm_lease.c validate_lease) until it is emptied.
    Ok(if count(&res, 36) == 0 && count(&res, 40) == 0 {
        LeaseState::Empty
    } else {
        LeaseState::NotMaster
    })
}

/// Rung from any thread for the hotplug listener to run its tick now rather
/// than at the next second.
pub struct LeaseAlarm(PrivateFd);

impl LeaseAlarm {
    fn new() -> io::Result<Self> {
        Ok(Self(PrivateFd::new(crate::hostfd::new_eventfd()?)))
    }

    pub fn ring(&self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a local to an eventfd this owns.
        unsafe {
            libc::write(
                self.0.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
    }

    fn clear(&self) {
        let mut v: u64 = 0;
        // SAFETY: reads 8 bytes into a local from a non-blocking eventfd.
        unsafe {
            libc::read(
                self.0.as_raw_fd(),
                &mut v as *mut u64 as *mut libc::c_void,
                8,
            )
        };
    }
}

// ─────────────────────────── scanout checksums ───────────────────────────
//
// nvidia-drm's GET_CRTC_CRC32 and _V2 hand back a checksum of what a CRTC
// scans out, each call holding nvkms_lock across two synchronous core
// updates (nvkms-evo.c:9301-9318). Which CRTCs a file may name is the DRM
// core's lease filter, and that filters only a lessee: for any file whose
// master has no lessor -- a non-master file of the card such as a lease
// device's `drm_fd`, or the lessor itself -- every CRTC is found
// (drm_lease.c:90-93, 109-121; drm_mode_object.c:151-155). So the schema
// offers them on card and lease files only (not on render nodes, where
// nvidia-drm also allows them), and here they pass only on a card the
// guest drives itself (a `DrmCard`, which exists only in compositor-VM
// mode) or on a file that is a lessee, whose CRTCs the host then filters
// natively.
//
// A lessee has nothing to show for it but its master's `lessor`, which no
// query reports: GET_LEASE answers a lessor with every object on the card
// (drm_lease.c:661-666). CREATE_LEASE looks at it first, though: a
// lessee's is refused -EINVAL as a sub-lease (:499-505) before the object
// list is read, which for anyone else is where a NULL list with a count
// of one faults (-EFAULT, :511-518); and a file that is not current master
// is refused -EACCES before the call runs at all (DRM_MASTER,
// drm_ioctl.c). Nothing is created on any of these paths.

/// `DRM_IOCTL_MODE_CREATE_LEASE`: `_IOWR('d', 0xC6, struct
/// drm_mode_create_lease)`, `{u64 object_ids; u32 object_count; u32 flags;
/// u32 lessee_id; u32 fd;}`.
pub const DRM_IOCTL_MODE_CREATE_LEASE: u32 = 0xc018_64c6;

/// `DRM_IOCTL_NVIDIA_GET_CRTC_CRC32` and `_V2` (nv_drm_common_ioctl.h).
pub const NV_GET_CRTC_CRC32: u32 = 0xc008_6440;
pub const NV_GET_CRTC_CRC32_V2: u32 = 0xc01c_644c;

/// Whether the DRM file `fd` is a lessee (see above).
pub fn is_lessee(sys: &dyn xfer::Sys, fd: RawFd) -> bool {
    let mut arg = [0u8; 24];
    arg[8..12].copy_from_slice(&1u32.to_le_bytes());
    let r = sys.ioctl(fd, DRM_IOCTL_MODE_CREATE_LEASE, arg.as_mut_ptr());
    if r >= 0 {
        // Cannot happen with a NULL object list; if a kernel ever made a
        // lease of it, the lessee file is ours and must not stay open.
        let lessee = i32::from_le_bytes(arg[20..24].try_into().unwrap());
        if lessee >= 0 {
            sys.close(lessee);
        }
        return false;
    }
    -r == libc::EINVAL
}

impl NvidiaBackend {
    /// GET_CRTC_CRC32(_V2) on KMS handle `target`: only on a card the guest
    /// drives or a lessee (see above). Every other call passes.
    pub(crate) fn crc_gate(&self, cmd: u32, target: u32, kind: HandleKind) -> Result<(), i32> {
        if cmd != NV_GET_CRTC_CRC32 && cmd != NV_GET_CRTC_CRC32_V2 {
            return Ok(());
        }
        let ok = match kind {
            HandleKind::DrmCard(_) => true,
            HandleKind::DrmLease(_) => self
                .handles
                .get_raw(target)
                .is_ok_and(|fd| is_lessee(&*self.xfer_sys, fd)),
            _ => false,
        };
        if !ok {
            log::warn!(
                "GET_CRTC_CRC32 on handle {target} ({kind:?}) refused: that file finds every \
                 CRTC of the host, not only the ones leased to it"
            );
            return Err(libc::EPERM);
        }
        Ok(())
    }

    /// The host card nodes offered to the guest, in the order its
    /// `EV_HOTPLUG` cookie indexes them (GET_SYS_FILES section 3).
    pub fn kms_cards(&mut self) -> Vec<CardNode> {
        self.host_nodes().cards.clone()
    }

    /// Ask every `DrmLease` handle (of card `card`, or of any) about its
    /// lease, and end the NVKMS grants of each that no longer holds one.
    /// Returns those handles.
    pub fn check_leases(&mut self, card: Option<u32>) -> Vec<u32> {
        let leases: Vec<u32> = self
            .handles
            .handles()
            .into_iter()
            .filter(|&h| match self.handles.kind(h) {
                Some(HandleKind::DrmLease(c)) => card.is_none_or(|want| want == c),
                _ => false,
            })
            .collect();
        self.end_dead_leases(&leases)
    }

    /// The leases that granted something the NVKMS policy still records, or
    /// whose lease ended after they had, re-checked: before an NVKMS call,
    /// so no per-head gate is opened by a grant the host has since taken
    /// back, and on the hotplug listener's tick, so a file whose lease is
    /// gone does not outlive it. Returns the handles whose lease ended.
    pub fn recheck_granting_leases(&mut self) -> Vec<u32> {
        let granting: Vec<u32> = self
            .nvkms
            .granting_handles()
            .into_iter()
            .filter(|&h| matches!(self.handles.kind(h), Some(HandleKind::DrmLease(_))))
            .collect();
        if granting.is_empty() {
            return granting;
        }
        self.end_dead_leases(&granting)
    }

    fn end_dead_leases(&mut self, leases: &[u32]) -> Vec<u32> {
        let mut ended = Vec::new();
        for &h in leases {
            let Ok(fd) = self.handles.get_raw(h) else {
                continue;
            };
            match lease_state(&*self.xfer_sys, fd) {
                Ok(LeaseState::Holds) => {}
                Ok(LeaseState::Empty) if !self.config.kms_card && self.nvkms.granted_through(h) => {
                    log::info!(
                        "lease handle {h} holds nothing any more (revoked, or its lessor closed); \
                         closing its host file so nvidia-drm takes back what was granted \
                         through it now, and ending those grants here"
                    );
                    self.bury_lease(h);
                    ended.push(h);
                }
                Ok(state) => {
                    log::info!(
                        "lease handle {h}: {state:?} (revoked, or its lessor closed or dropped \
                         master); ending the NVKMS grants made through it"
                    );
                    self.nvkms.lease_ended(h);
                    ended.push(h);
                }
                Err(e) => log::debug!("lease handle {h}: GET_LEASE failed ({e}); left as is"),
            }
        }
        ended
    }

    /// Close lease handle `h`'s host file, keeping the handle as a stub the
    /// guest can still close (and that answers ENODEV until it does). The
    /// pump's duplicate goes with the watch; an executor call still running
    /// on it holds the file until it returns.
    fn bury_lease(&mut self, h: u32) {
        self.nvkms.lease_ended(h);
        let stub = match crate::hostfd::new_eventfd() {
            Ok(fd) => fd,
            Err(e) => {
                log::warn!("lease handle {h}: no stub descriptor ({e}); its file stays open");
                return;
            }
        };
        // Its framebuffers stop being the VM's before the host file (and
        // with it every id that file made) can go (S-6).
        self.forget_kms_state(h);
        match self.handles.bury(h, stub) {
            // A lease file's last close is a master drop (closer.rs).
            Ok(old) => crate::closer::close(old),
            Err(e) => {
                log::warn!("lease handle {h}: {e}");
                return;
            }
        }
        self.nvkms.forget_handle(h);
        self.pump_cmds.push(PumpCmd::Unwatch { handle: h });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn card(name: &str, minor: u32) -> CardNode {
        CardNode {
            name: name.into(),
            major: 226,
            minor,
            render_index: 0,
        }
    }

    /// kobject_uevent_env's layout: "ACTION@DEVPATH", then the environment.
    fn uevent(action: &str, env: &[&str]) -> Vec<u8> {
        let mut v = format!("{action}@/devices/pci0000:00/0000:00:01.0/drm/card1\0").into_bytes();
        for e in env {
            v.extend_from_slice(e.as_bytes());
            v.push(0);
        }
        v
    }

    const HOTPLUG: &[&str] = &[
        "ACTION=change",
        "DEVPATH=/devices/pci0000:00/0000:00:01.0/drm/card1",
        "SUBSYSTEM=drm",
        "HOTPLUG=1",
        "MAJOR=226",
        "MINOR=1",
        "DEVNAME=dri/card1",
        "DEVTYPE=drm_minor",
        "SEQNUM=4711",
    ];

    #[test]
    fn a_card_hotplug_uevent_is_parsed_from_the_kernels_layout() {
        let msg = uevent("change", HOTPLUG);
        let ev = parse_uevent(&msg).unwrap();
        assert_eq!(ev.action, b"change");
        assert_eq!(ev.subsystem, b"drm");
        assert_eq!((ev.major, ev.minor), (Some(226), Some(1)));
        assert!(ev.hotplug && !ev.lease);
    }

    #[test]
    fn hotplug_and_lease_changes_of_our_cards_become_their_flags() {
        let cards = [card("card0", 0), card("card1", 1)];
        let msg = uevent("change", HOTPLUG);
        assert_eq!(
            card_event(&parse_uevent(&msg).unwrap(), &cards),
            Some((1, EV_HOTPLUG_F_HOTPLUG))
        );
        let msg = uevent(
            "change",
            &["SUBSYSTEM=drm", "LEASE=1", "MAJOR=226", "MINOR=0"],
        );
        assert_eq!(
            card_event(&parse_uevent(&msg).unwrap(), &cards),
            Some((0, EV_HOTPLUG_F_LEASE))
        );
    }

    #[test]
    fn other_devices_actions_and_plain_changes_are_not_hotplugs() {
        let cards = [card("card1", 1)];
        let ev = |action: &str, env: &[&str]| {
            let msg = uevent(action, env);
            card_event(&parse_uevent(&msg).unwrap(), &cards)
        };
        // Another card (the iGPU's), a render node, another subsystem.
        assert_eq!(
            ev(
                "change",
                &["SUBSYSTEM=drm", "HOTPLUG=1", "MAJOR=226", "MINOR=0"]
            ),
            None
        );
        assert_eq!(
            ev(
                "change",
                &["SUBSYSTEM=drm", "HOTPLUG=1", "MAJOR=226", "MINOR=128"]
            ),
            None
        );
        assert_eq!(
            ev(
                "change",
                &["SUBSYSTEM=usb", "HOTPLUG=1", "MAJOR=226", "MINOR=1"]
            ),
            None
        );
        // Our card being added or removed is not a connector change; nor is a
        // change that says neither HOTPLUG nor LEASE (a CONNECTOR property
        // event still carries HOTPLUG=1, drm_sysfs.c:463).
        assert_eq!(
            ev(
                "add",
                &["SUBSYSTEM=drm", "HOTPLUG=1", "MAJOR=226", "MINOR=1"]
            ),
            None
        );
        assert_eq!(
            ev("change", &["SUBSYSTEM=drm", "MAJOR=226", "MINOR=1"]),
            None
        );
        assert_eq!(
            ev(
                "change",
                &["SUBSYSTEM=drm", "HOTPLUG=0", "MAJOR=226", "MINOR=1"]
            ),
            None
        );
    }

    #[test]
    fn malformed_uevents_are_refused_rather_than_guessed_at() {
        assert_eq!(parse_uevent(b""), None);
        assert_eq!(parse_uevent(b"no header here\0HOTPLUG=1\0"), None);
        let msg = uevent(
            "change",
            &["SUBSYSTEM=drm", "HOTPLUG=1", "MAJOR=x", "MINOR=1"],
        );
        assert_eq!(
            card_event(&parse_uevent(&msg).unwrap(), &[card("card1", 1)]),
            None
        );
        // A value with an embedded '=' and a key with none are skipped, not
        // misread.
        let msg = uevent("change", &["JUNK", "SUBSYSTEM=drm=x", "MAJOR=226"]);
        let ev = parse_uevent(&msg).unwrap();
        assert_eq!(ev.subsystem, b"drm=x");
        assert_eq!(ev.minor, None);
    }

    #[test]
    fn fence_and_pointer_properties_are_known_by_name_and_by_suffix() {
        assert_eq!(prop_kind(b"IN_FENCE_FD"), PropKind::FenceFd);
        assert_eq!(prop_kind(b"OUT_FENCE_PTR"), PropKind::OutPtr);
        assert_eq!(prop_kind(b"WRITEBACK_OUT_FENCE_PTR"), PropKind::OutPtr);
        assert_eq!(prop_kind(b"NV_DRM_OUT_FENCE_PTR"), PropKind::OutPtr);
        // What a newer kernel might add, treated as what its name says.
        assert_eq!(prop_kind(b"SOME_NEW_FENCE_PTR"), PropKind::OutPtr);
        assert_eq!(prop_kind(b"SOME_NEW_SYNC_FD"), PropKind::FenceFd);
        // Everything nvidia-drm 610 and the core attach otherwise is plain
        // (R:nvdrm §3.8): ids, blobs, enums, ranges.
        for name in [
            &b"CRTC_ID"[..],
            b"FB_ID",
            b"MODE_ID",
            b"ACTIVE",
            b"GAMMA_LUT",
            b"NV_HDR_STATIC_METADATA",
            b"NV_CRTC_REGAMMA_LUT_SIZE",
            b"HDR_OUTPUT_METADATA",
            b"FD",
            b"PTR",
        ] {
            assert_eq!(
                prop_kind(name),
                PropKind::Plain,
                "{}",
                String::from_utf8_lossy(name)
            );
        }
    }

    /// The socket needs no privilege; where netlink is unavailable altogether
    /// (a restricted sandbox) the test says so and stops.
    #[test]
    fn the_listener_starts_unprivileged_and_stops_when_dropped() {
        let (tx, _rx) = mpsc::channel();
        let l = match HotplugListener::spawn(vec![card("card1", 1)], move |c| {
            let _ = tx.send(c);
        }) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: no uevent netlink socket here ({e})");
                return;
            }
        };
        let stop = l.stop.as_raw_fd();
        assert!(
            crate::privfd::is_private(stop),
            "its descriptors are the backend's own"
        );
        drop(l); // joins: a listener that ignored its stop would hang here
        // The registry is keyed by descriptor number and shared by every
        // test thread, so the number may already be another test's private
        // descriptor (semsurf's RM files, another listener) by the time this
        // looks. A registration this listener leaked is one whose number is
        // closed; a number that is open again belongs to whoever reopened it.
        // SAFETY: F_GETFD on a number, open or not, touches nothing.
        let open_again = unsafe { libc::fcntl(stop, libc::F_GETFD) } >= 0;
        assert!(
            !crate::privfd::is_private(stop) || open_again,
            "the listener closed its stop descriptor and left it registered"
        );
    }

    /// Only the kernel may speak on the uevent socket: an unprivileged sender
    /// is refused by netlink itself (no NL_CFG_F_NONROOT_SEND,
    /// af_netlink.c netlink_sendmsg), and a privileged one that gets through
    /// has a port that is not 0 and is ignored here.
    #[test]
    fn a_datagram_from_a_userspace_sender_is_never_taken_for_a_uevent() {
        let (rx, tx) = match (uevent_socket(), uevent_socket()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => {
                eprintln!("skipping: no uevent netlink socket here");
                return;
            }
        };
        // SAFETY: an all-zero sockaddr_nl is valid for the kernel to fill.
        let mut me: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        let mut len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        // SAFETY: getsockname into a local of the size given.
        let r = unsafe {
            libc::getsockname(
                rx.as_raw_fd(),
                &mut me as *mut libc::sockaddr_nl as *mut libc::sockaddr,
                &mut len,
            )
        };
        assert_eq!(r, 0);
        let mut to: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
        to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        to.nl_pid = me.nl_pid;
        let msg = uevent("change", HOTPLUG);
        // SAFETY: sends a buffer this test owns to a sockaddr of the size given.
        let sent = unsafe {
            libc::sendto(
                tx.as_raw_fd(),
                msg.as_ptr() as *const libc::c_void,
                msg.len(),
                0,
                &to as *const libc::sockaddr_nl as *const libc::sockaddr,
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        let mut buf = vec![0u8; MSG_MAX];
        if sent < 0 {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
            return;
        }
        let mut got = recv(rx.as_raw_fd(), &mut buf).unwrap();
        for _ in 0..100 {
            if got != Recv::Empty {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            got = recv(rx.as_raw_fd(), &mut buf).unwrap();
        }
        // Kernel uevents may interleave; ours must never read as the kernel's.
        while let Recv::Kernel(_) = got {
            got = recv(rx.as_raw_fd(), &mut buf).unwrap();
        }
        assert_eq!(got, Recv::Foreign);
    }

    /// A lessee's file as the kernel answers it: GET_LEASE a count or an
    /// errno, GETRESOURCES (which needs no master) the (crtcs, connectors)
    /// its lease still holds.
    struct LeaseSys(Result<u32, i32>, (u32, u32));

    impl xfer::Sys for LeaseSys {
        fn ioctl(&self, _: RawFd, cmd: u32, arg: *mut u8) -> i32 {
            match cmd {
                DRM_IOCTL_MODE_GET_LEASE => {
                    // SAFETY: lease_state passes its 16-byte argument.
                    let a = unsafe { std::slice::from_raw_parts_mut(arg, 16) };
                    assert!(a.iter().all(|&b| b == 0), "count only: no ids pointer");
                    match self.0 {
                        Ok(n) => {
                            a[..4].copy_from_slice(&n.to_le_bytes());
                            0
                        }
                        Err(e) => -e,
                    }
                }
                DRM_IOCTL_MODE_GETRESOURCES => {
                    // SAFETY: lease_state passes its 64-byte argument.
                    let a = unsafe { std::slice::from_raw_parts_mut(arg, 64) };
                    assert!(a.iter().all(|&b| b == 0), "counts only: no id lists");
                    a[36..40].copy_from_slice(&self.1.0.to_le_bytes());
                    a[40..44].copy_from_slice(&self.1.1.to_le_bytes());
                    a[44..48].copy_from_slice(&4u32.to_le_bytes()); // encoders: all
                    0
                }
                _ => panic!("unexpected ioctl {cmd:#x}"),
            }
        }
        fn close(&self, _: RawFd) {}
        fn size_of(&self, _: RawFd) -> i64 {
            0
        }
    }

    /// CREATE_LEASE with a NULL list of one object, as the kernel answers
    /// a lessee (-EINVAL), anyone else who is master (-EFAULT) and a file
    /// that is not (-EACCES).
    struct CreateLeaseSys(i32, std::sync::Mutex<Vec<RawFd>>);

    impl xfer::Sys for CreateLeaseSys {
        fn ioctl(&self, _: RawFd, cmd: u32, arg: *mut u8) -> i32 {
            assert_eq!(cmd, DRM_IOCTL_MODE_CREATE_LEASE);
            // SAFETY: is_lessee passes its 24-byte argument.
            let a = unsafe { std::slice::from_raw_parts_mut(arg, 24) };
            assert_eq!(&a[..8], &[0; 8], "no object list: nothing to lease");
            assert_eq!(&a[8..16], &[1, 0, 0, 0, 0, 0, 0, 0], "one object, no flags");
            if self.0 >= 0 {
                a[20..24].copy_from_slice(&self.0.to_le_bytes());
                return 0;
            }
            self.0
        }
        fn close(&self, fd: RawFd) {
            self.1.lock().unwrap().push(fd);
        }
        fn size_of(&self, _: RawFd) -> i64 {
            0
        }
    }

    fn create_lease(r: i32) -> CreateLeaseSys {
        CreateLeaseSys(r, Default::default())
    }

    #[test]
    fn only_a_lessee_is_taken_for_one() {
        assert!(is_lessee(&create_lease(-libc::EINVAL), 0));
        assert!(!is_lessee(&create_lease(-libc::EFAULT), 0), "a lessor");
        assert!(
            !is_lessee(&create_lease(-libc::EACCES), 0),
            "a lease device's drm_fd, or a lessee whose lessor lost master"
        );
        assert!(!is_lessee(&create_lease(-libc::ENOTTY), 0));
        // A lease somehow made is closed, not kept.
        let made = create_lease(42);
        assert!(!is_lessee(&made, 0));
        assert_eq!(*made.1.lock().unwrap(), vec![42]);
    }

    #[test]
    fn scanout_checksums_pass_on_our_card_or_a_lessee_only() {
        let mut be = NvidiaBackend::for_test();
        let devnull = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let card = be.adopt_for_test(devnull(), HandleKind::DrmCard(0));
        let lease = be.adopt_for_test(devnull(), HandleKind::DrmLease(0));
        for cmd in [NV_GET_CRTC_CRC32, NV_GET_CRTC_CRC32_V2] {
            assert_eq!(be.crc_gate(cmd, card, HandleKind::DrmCard(0)), Ok(()));
            be.xfer_sys = std::sync::Arc::new(create_lease(-libc::EINVAL));
            assert_eq!(be.crc_gate(cmd, lease, HandleKind::DrmLease(0)), Ok(()));
            for other in [-libc::EFAULT, -libc::EACCES] {
                be.xfer_sys = std::sync::Arc::new(create_lease(other));
                assert_eq!(
                    be.crc_gate(cmd, lease, HandleKind::DrmLease(0)),
                    Err(libc::EPERM)
                );
            }
        }
        // Anything else on a lease file is none of this gate's business.
        assert_eq!(
            be.crc_gate(DRM_IOCTL_MODE_GET_LEASE, lease, HandleKind::DrmLease(0)),
            Ok(())
        );
    }

    #[test]
    fn a_lease_is_empty_once_revoked_or_its_lessor_closed_and_only_gated_while_master_is_away() {
        let state = |lease, res| lease_state(&LeaseSys(lease, res), 0);
        assert_eq!(state(Ok(3), (0, 0)), Ok(LeaseState::Holds));
        // Revoked with the lessor still master.
        assert_eq!(state(Ok(0), (1, 1)), Ok(LeaseState::Empty));
        // The lessor closed: no master, and the lease emptied with it.
        assert_eq!(state(Err(libc::EACCES), (0, 0)), Ok(LeaseState::Empty));
        // The lessor dropped master: the lease still holds its objects.
        assert_eq!(state(Err(libc::EACCES), (1, 1)), Ok(LeaseState::NotMaster));
        // Not a DRM file, or not a lease: no answer, and no grant ended on it.
        assert_eq!(state(Err(libc::ENOTTY), (0, 0)), Err(libc::ENOTTY));
    }

    /// The tick runs on its timer and at once when the alarm rings, and the
    /// alarm's descriptor is the backend's own.
    #[test]
    fn the_listener_ticks_on_its_timer_and_when_its_alarm_rings() {
        let (tx, rx) = mpsc::channel();
        let tick = Tick {
            every: Duration::from_millis(50),
            run: Box::new(move || {
                let _ = tx.send(Instant::now());
            }),
        };
        let l = match HotplugListener::spawn_ticking(vec![card("card1", 1)], |_| {}, Some(tick)) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("skipping: no uevent netlink socket here ({e})");
                return;
            }
        };
        assert!(crate::privfd::is_private(l.alarm().0.as_raw_fd()));
        let timeout = Duration::from_secs(5);
        rx.recv_timeout(timeout).expect("a tick on the timer");
        rx.recv_timeout(timeout).expect("and another");
        // A slow timer, rung: the tick comes long before the timer would.
        drop(l);
        let (tx, rx) = mpsc::channel();
        let tick = Tick {
            every: Duration::from_secs(3600),
            run: Box::new(move || {
                let _ = tx.send(());
            }),
        };
        let l = HotplugListener::spawn_ticking(vec![card("card1", 1)], |_| {}, Some(tick)).unwrap();
        assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());
        l.alarm().ring();
        rx.recv_timeout(timeout).expect("a tick on the alarm");
    }
}
