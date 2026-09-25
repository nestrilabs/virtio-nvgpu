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
use std::thread::JoinHandle;

use protocol::messages::{EV_HOTPLUG_F_HOTPLUG, EV_HOTPLUG_F_LEASE};

use crate::hostfd::CardNode;
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
/// event pump. Stopped and joined when dropped.
pub struct HotplugListener {
    stop: PrivateFd,
    thread: Option<JoinHandle<()>>,
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
        let sock = uevent_socket()?;
        // SAFETY: plain syscall; the result is owned below.
        let stop = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if stop < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor eventfd() just returned.
        let stop = PrivateFd::new(unsafe { OwnedFd::from_raw_fd(stop) });
        let stop_rx = stop.try_clone()?;
        let thread = std::thread::Builder::new()
            .name("nvgpu-hotplug".into())
            .spawn(move || listen(sock, stop_rx, cards, sink))?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
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

fn listen(sock: PrivateFd, stop: PrivateFd, cards: Vec<CardNode>, sink: impl Fn(PumpCmd)) {
    log::info!(
        "hotplug: listening for uevents of {} card node(s)",
        cards.len()
    );
    let mut buf = vec![0u8; MSG_MAX];
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
        ];
        // SAFETY: polls two pollfds this function owns.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
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

impl NvidiaBackend {
    /// The host card nodes offered to the guest, in the order its
    /// `EV_HOTPLUG` cookie indexes them (GET_SYS_FILES section 3).
    pub fn kms_cards(&mut self) -> Vec<CardNode> {
        self.host_nodes().cards.clone()
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
        assert!(!crate::privfd::is_private(stop));
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
}
