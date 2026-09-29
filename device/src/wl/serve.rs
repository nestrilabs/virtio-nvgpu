// SPDX-License-Identifier: Apache-2.0
//! The dispatcher's half of the Wayland channel: `OPEN(DEV_WAYLAND)`,
//! `WL_SEND`, `WL_RECV` and the end of a channel, over the session's handle
//! table (ARCHITECTURE.md §14).
//!
//! A channel is a backend handle of kind `Wayland`. What the table holds under
//! it is not the compositor socket -- that belongs to the connection's reader
//! thread, and a descriptor the guest could name in a WATCH or FD_KIND must
//! never be one whose bytes somebody else is parsing -- but a duplicate of the
//! connection's *readiness eventfd*, readable while the connection has
//! something for the guest. The pump watches it from the OPEN onwards, the
//! legacy way (EVENT_READY on the handle to a v1 dialect, EV_READY with cookie
//! = handle to a v2 one), which is what `nvgpu_wl.c` registers for: it never
//! sends a WATCH of its own. The connection object itself lives beside the
//! table, in [`WlState`], keyed by the same handle, and goes when the handle
//! does -- CLOSE, CLOSE_MANY, a session reset, teardown.
//!
//! Everything the guest names is checked against the table here, never taken
//! from the frame: a DMABUF descriptor's owner must be a render handle of this
//! session before its GEM is PRIME-exported on it, and a descriptor the
//! compositor sends is classified by what the host kernel says it is (a DRM
//! file must be a card-node file of our own GPU, `hostfd::classify`) before it
//! is put in the table for the guest to adopt.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::sync::Arc;

use protocol::messages::*;

use crate::handle_table::HandleTable;
use crate::hostfd::{self, CardNode, HandleKind};
use crate::kms::LeaseAlarm;
use crate::nvidia::NvidiaBackend;
use crate::pump::{PumpCmd, WatchMode};
use crate::session::{Reply, hdr};
use crate::wl::conn::{HostFds, RecvOps, SendOps, WlConfig, WlConn, WlLimits};
use crate::wl::export::WlExport;
use wlwire::frame;

const HDR: usize = size_of::<MsgHeader>();

/// One Wayland handle.
enum Chan {
    /// A connection to the compositor (CONNECT), or an export-mode host
    /// client the guest accepted (ACCEPT).
    Conn(WlConn),
    /// Export mode's listener (LISTEN): readable while host clients wait to
    /// be accepted; nothing to send or receive.
    Listener,
}

/// The Wayland side of the backend: configuration, and one entry per open
/// channel handle.
#[derive(Default)]
pub struct WlState {
    /// `--wayland-socket` (and `--wayland-lease`): one configuration shared by
    /// every connection, so the lease-device probe cache is too.
    cfg: Option<WlConfig>,
    /// `--wayland-export`: the listener, bound once at startup, and a
    /// duplicate of its readiness eventfd for LISTEN handles to share.
    export: Option<(Arc<WlExport>, OwnedFd)>,
    chans: HashMap<u32, Chan>,
    /// How descriptors are classified. The host's own classification over the
    /// card list, made on first use; tests put a fake here.
    host: Option<Arc<dyn HostFds>>,
    /// Rung when a compositor connection hangs up (`HostFds`).
    lease_alarm: Option<Arc<LeaseAlarm>>,
    /// What all of this VM's channels may hold together; put into every
    /// connection's configuration, whichever one it was made from.
    limits: WlLimits,
    /// A refused OPEN was logged; cleared once a channel fits again, so a
    /// guest retrying in a loop costs one line, not one per try.
    cap_logged: bool,
    /// Each guest process's shm budget, which all its connections share
    /// (quota.rs): a quarter of the VM's bytes and pools. An entry no
    /// connection holds any more goes at the next OPEN.
    owner_shm: HashMap<crate::quota::Owner, Arc<wlwire::shm::ShmBudget>>,
}

/// A guest process's share of the VM's channels (quota.rs, W1): a quarter,
/// with the last eighth kept for processes holding at most two. Its
/// connections' shm is a quarter of the VM's, and their unread output half
/// of the VM's queue budget (`QueueBudget`). The guest daemon charges each
/// connection to the client it is for, not to itself (NVGPU_WL_IOC_CONNECT_FOR).
fn chan_share(max_conns: usize) -> crate::quota::Share {
    let m = max_conns as u64;
    crate::quota::Share {
        per_owner: (m / 4).max(1),
        reserve: m / 8,
        floor: 2,
    }
}

impl WlState {
    /// Channels that hold a connection: what `WlLimits::max_conns` counts.
    fn conns(&self) -> usize {
        self.chans
            .values()
            .filter(|c| matches!(c, Chan::Conn(_)))
            .count()
    }

    /// Take a channel's connection out, for [`drop_detached`] to end.
    fn take(&mut self, handle: u32) -> Option<WlConn> {
        match self.chans.remove(&handle)? {
            Chan::Conn(c) => Some(c),
            Chan::Listener => None,
        }
    }
}

/// `hostfd::classify` over the host's card list: what the connection's reader
/// thread asks of every DRM file and dma-buf the compositor sends.
struct CardClassifier {
    cards: Vec<CardNode>,
    alarm: Option<Arc<LeaseAlarm>>,
}

impl HostFds for CardClassifier {
    fn classify(&self, fd: BorrowedFd<'_>) -> HandleKind {
        hostfd::classify(fd, &self.cards)
    }

    fn compositor_hung_up(&self) {
        if let Some(a) = &self.alarm {
            a.ring();
        }
    }
}

/// End connections without holding anyone up.
///
/// Dropping a `WlConn` joins its reader thread, and that thread can be inside
/// a lease-device probe, or waiting for another connection's (up to a couple
/// of seconds on a compositor that does not answer, `probe.rs`). It holds no
/// lock of ours while it does, but it cannot notice the stop until it is out.
/// The callers hold the backend mutex, and every guest request waits behind
/// it, so the join happens on a thread of its own. The compositor still
/// learns at once: `Drop` shuts the socket down before it joins.
fn drop_detached(conns: Vec<WlConn>) {
    if conns.is_empty() {
        return;
    }
    // If the thread cannot be made the closure is dropped right here, which
    // is the synchronous close: slower, never lost.
    if let Err(e) = std::thread::Builder::new()
        .name("nvgpu-wl-close".into())
        .spawn(move || drop(conns))
    {
        log::warn!("wayland: closing connections inline, no thread for it: {e}");
    }
}

/// PRIME export for a WL_SEND, on the owner the guest named, if and only if
/// that owner is a render file of this session.
pub(super) struct TableSend<'a> {
    pub(super) handles: &'a HandleTable,
    /// What may leave the backend: no fence context, no injected capture
    /// buffer (exportgate.rs), the same gate HOST_OP PRIME_EXPORT asks.
    pub(super) gate: crate::exportgate::ExportGate<'a>,
}

impl SendOps for TableSend<'_> {
    fn prime_export(&mut self, owner: u32, gem: u32) -> io::Result<OwnedFd> {
        match self.handles.get(owner) {
            // Only a render file: a guest GEM proxy lives nowhere else (a KMS
            // file holds only an executor's temporaries, ARCHITECTURE.md §10), and a
            // PRIME export on a lease or card file would be one of the host
            // compositor's framebuffer objects by number.
            Some((fd, HandleKind::DriRender(_))) => {
                self.gate
                    .may_export(owner, gem)
                    .map_err(io::Error::from_raw_os_error)?;
                let dmabuf = hostfd::prime_export(fd.as_raw_fd(), gem)?;
                self.gate
                    .may_leave(dmabuf.as_fd())
                    .map_err(io::Error::from_raw_os_error)?;
                Ok(dmabuf)
            }
            Some((_, kind)) => {
                log::warn!(
                    "wayland: a dma-buf names owner {owner}, a {kind:?}, not a render file; refused"
                );
                Err(io::Error::from_raw_os_error(libc::EBADF))
            }
            None => {
                log::warn!("wayland: a dma-buf names owner {owner}, which does not exist");
                Err(io::Error::from_raw_os_error(libc::EBADF))
            }
        }
    }

    /// A duplicate of a syncobj handle of this session, and of nothing else:
    /// the compositor imports it as a timeline and signals and waits on it,
    /// so a number naming any other kind of file would hand it that file.
    fn syncobj(&mut self, handle: u32) -> io::Result<OwnedFd> {
        match self.handles.get(handle) {
            Some((fd, HandleKind::Syncobj)) => fd.try_clone_to_owned(),
            _ => {
                log::warn!("wayland: a syncobj names handle {handle}, which is not a syncobj");
                Err(io::Error::from_raw_os_error(libc::EBADF))
            }
        }
    }
}

/// Adoption for a WL_RECV: a descriptor from the compositor becomes a handle,
/// if it is what its frame descriptor says it is.
pub(super) struct TableRecv<'a> {
    pub(super) handles: &'a mut HandleTable,
    pub(super) host: &'a dyn HostFds,
    pub(super) created: Vec<u32>,
    /// Who what the compositor sends is charged to: the channel's opener
    /// (quota.rs).
    pub(super) owner: crate::quota::Owner,
}

impl RecvOps for TableRecv<'_> {
    fn adopt(&mut self, fd: OwnedFd, desc_kind: u16) -> io::Result<(u32, u32)> {
        let kind = self.host.classify(fd.as_fd());
        match (desc_kind, kind) {
            (frame::DESC_DRM_FILE, HandleKind::DrmLease(_)) => {
                // Aquamarine makes its leases without O_NONBLOCK and Hyprland
                // closes its copy after sending (DRMLease.cpp:89-91), so this
                // file description is ours alone, and the pump reads DRM
                // events from it once the guest watches it: one blocking
                // read would park every event for the VM (hostfd::
                // set_nonblock). DRM ioctls ignore the flag.
                hostfd::set_nonblock(fd.as_raw_fd())?;
            }
            (frame::DESC_DMABUF, HandleKind::Dmabuf) => {}
            (d, k) => {
                log::warn!("wayland: a descriptor sent as kind {d} is a {k:?}; not adopted");
                return Err(io::Error::from_raw_os_error(libc::EBADF));
            }
        }
        let h = self
            .handles
            .insert_for(fd, kind, self.owner)
            .map_err(|full| {
                log::warn!("wayland: handle table full; a {kind:?} from the compositor is dropped");
                io::Error::from_raw_os_error(full.errno())
            })?;
        self.created.push(h);
        Ok((h, kind.wire()))
    }
}

impl NvidiaBackend {
    /// What `--wayland-socket` (and `--wayland-lease`) configured: one
    /// `WlConfig` for every connection. Without it CONNECT is refused.
    pub fn set_wayland(&mut self, cfg: Option<WlConfig>) {
        self.wl.cfg = cfg;
    }

    /// The export listener bound at startup (`--wayland-export`), and the
    /// readiness eventfd `WlExport::bind` returned. Without it LISTEN and
    /// ACCEPT are refused.
    pub fn set_wayland_export(&mut self, export: Option<(Arc<WlExport>, OwnedFd)>) {
        self.wl.export = export;
    }

    /// The per-VM limits (`--wayland-max-conns`, `--wayland-shm-budget`,
    /// `--wayland-queue-budget`); `WlLimits::default()` otherwise.
    pub fn set_wayland_limits(&mut self, limits: WlLimits) {
        self.wl.limits = limits;
    }

    /// Room for one more channel with a connection behind it, or EMFILE.
    /// Asked before anything is connected or accepted, so a refused OPEN
    /// costs the host no socket, thread or compositor client.
    fn wl_room(&mut self) -> Result<(), i32> {
        let n = self.wl.conns();
        let owner = self.current_owner;
        let mine = self
            .wl
            .chans
            .iter()
            .filter(|(h, c)| matches!(c, Chan::Conn(_)) && self.handles.owner(**h) == owner)
            .count();
        let max = self.wl.limits.max_conns;
        if n < max {
            if let Err(why) = crate::quota::admits(
                &chan_share(max),
                owner,
                mine as u64,
                1,
                n as u64,
                max as u64,
            ) {
                log::warn!(
                    "OPEN(DEV_WAYLAND): guest process {owner:?} holds {mine} of the VM's {n} \
                     channels ({why:?}); refused"
                );
                return Err(libc::EMFILE);
            }
            self.wl.cap_logged = false;
            return Ok(());
        }
        if !self.wl.cap_logged {
            self.wl.cap_logged = true;
            log::warn!(
                "OPEN(DEV_WAYLAND): the VM has {n} channels open, its limit \
                 (--wayland-max-conns); refusing more until one closes"
            );
        }
        Err(libc::EMFILE)
    }

    /// What the connection being opened is charged to: the guest process
    /// the OPEN names, and that process's shm budget.
    fn wl_owner_budgets(&mut self, cfg: &mut WlConfig) {
        let owner = self.current_owner;
        cfg.owner = owner;
        self.wl.owner_shm.retain(|_, b| Arc::strong_count(b) > 1);
        if owner == crate::quota::Owner::Unknown {
            cfg.owner_shm = None;
            return;
        }
        let (bytes, pools) = self.wl.limits.shm.limits();
        let b = self
            .wl
            .owner_shm
            .entry(owner)
            .or_insert_with(|| {
                Arc::new(wlwire::shm::ShmBudget::new(
                    (bytes / 4).max(1),
                    (pools / 4).max(1),
                ))
            })
            .clone();
        cfg.owner_shm = Some(b);
    }

    /// Guest processes with a shm budget of their own, and the most bytes
    /// each may cover.
    #[cfg(test)]
    pub(crate) fn wl_owner_shm(&self) -> (usize, u64) {
        let max = self
            .wl
            .owner_shm
            .values()
            .map(|b| b.limits().0)
            .max()
            .unwrap_or(0);
        (self.wl.owner_shm.len(), max)
    }

    /// Replace descriptor classification, which needs real DRM nodes.
    #[cfg(test)]
    pub(crate) fn set_wl_host_for_test(&mut self, host: Arc<dyn HostFds>) {
        self.wl.host = Some(host);
    }

    /// Whether handle `h` has a live compositor connection behind it.
    #[cfg(test)]
    pub(crate) fn wl_is_open(&self, h: u32) -> bool {
        matches!(self.wl.chans.get(&h), Some(Chan::Conn(c)) if !c.is_closed())
    }

    /// What a compositor connection that hangs up rings: the hotplug
    /// listener's, so the leases the compositor granted are asked about at
    /// once (kms.rs, "lease ends"). Set before the first connection.
    pub fn set_lease_alarm(&mut self, alarm: Arc<LeaseAlarm>) {
        self.wl.lease_alarm = Some(alarm);
    }

    fn wl_host(&mut self) -> Arc<dyn HostFds> {
        if let Some(h) = &self.wl.host {
            return h.clone();
        }
        let cards = self.host_nodes().cards.clone();
        let alarm = self.wl.lease_alarm.clone();
        let h: Arc<dyn HostFds> = Arc::new(CardClassifier { cards, alarm });
        self.wl.host = Some(h.clone());
        h
    }

    /// OPEN(DEV_WAYLAND): `mode` is the OPEN flags word, `WL_OPEN_*`.
    /// Returns the new handle or an errno.
    pub(crate) fn open_wayland(&mut self, mode: u32) -> Result<u32, i32> {
        // A channel is a v2 feature: its readiness arrives as EV_READY and its
        // frames in WL_SEND/WL_RECV, none of which a v1 guest speaks.
        if !self.session.v2 {
            return Err(libc::ENODEV);
        }
        let (chan, ready) = match mode {
            frame::WL_OPEN_CONNECT => {
                let mut cfg = self.wl.cfg.clone().ok_or(libc::ENODEV)?;
                self.wl_room()?;
                // Explicit sync rides on the fences this backend serves.
                cfg.fences = self.config.fences;
                cfg.limits = self.wl.limits.clone();
                self.wl_owner_budgets(&mut cfg);
                let host = self.wl_host();
                let (conn, ready) = WlConn::open(&cfg, host).map_err(|e| {
                    log::warn!(
                        "wayland: connecting to the compositor at {}: {e}",
                        cfg.socket.display()
                    );
                    e.raw_os_error().unwrap_or(libc::ECONNREFUSED)
                })?;
                (Chan::Conn(conn), ready)
            }
            frame::WL_OPEN_LISTEN => {
                let (_, ready) = self.wl.export.as_ref().ok_or(libc::ENODEV)?;
                // One listener: every LISTEN handle would share the export's
                // one readiness eventfd, and whoever reads it first takes the
                // others' wake-ups. The daemon opens exactly one.
                if self.wl.chans.values().any(|c| matches!(c, Chan::Listener)) {
                    log::warn!("OPEN(DEV_WAYLAND, LISTEN): the export already has a listener");
                    return Err(libc::EBUSY);
                }
                let ready = ready
                    .try_clone()
                    .map_err(|e| e.raw_os_error().unwrap_or(libc::EMFILE))?;
                (Chan::Listener, ready)
            }
            frame::WL_OPEN_ACCEPT => {
                let (export, _) = self.wl.export.as_ref().ok_or(libc::ENODEV)?;
                let export = export.clone();
                // Before the accept: a refused ACCEPT leaves the host client
                // waiting in the export's queue, not accepted and dropped.
                self.wl_room()?;
                let stream = export.accept_pending().ok_or(libc::EAGAIN)?;
                // Export connections face a host client, not the compositor
                // socket; the configuration only lends its limits, so an
                // export-only backend makes one of its own.
                let mut cfg = self
                    .wl
                    .cfg
                    .clone()
                    .unwrap_or_else(|| WlConfig::new(export.path()));
                cfg.limits = self.wl.limits.clone();
                self.wl_owner_budgets(&mut cfg);
                let host = self.wl_host();
                let (conn, ready) = WlConn::from_export(stream, &cfg, host).map_err(|e| {
                    log::warn!("wayland export: taking a host client: {e}");
                    e.raw_os_error().unwrap_or(libc::EIO)
                })?;
                (Chan::Conn(conn), ready)
            }
            m => {
                log::warn!("OPEN(DEV_WAYLAND) with unknown mode {m}");
                return Err(libc::EINVAL);
            }
        };
        // The pump's own duplicate, so the table closing its copy can never
        // leave epoll watching a number reused for something else.
        let watch = match ready.try_clone() {
            Ok(w) => w,
            Err(e) => {
                if let Chan::Conn(c) = chan {
                    drop_detached(vec![c]);
                }
                return Err(e.raw_os_error().unwrap_or(libc::EMFILE));
            }
        };
        let handle = match self
            .handles
            .insert_for(ready, HandleKind::Wayland, self.current_owner)
        {
            Ok(h) => h,
            Err(full) => {
                log::warn!("OPEN(DEV_WAYLAND): handle table full");
                if let Chan::Conn(c) = chan {
                    drop_detached(vec![c]);
                }
                return Err(full.errno());
            }
        };
        self.wl.chans.insert(handle, chan);
        self.pump_cmds.push(PumpCmd::Watch {
            handle,
            fd: watch,
            mode: WatchMode::Legacy,
        });
        log::info!("OPEN(DEV_WAYLAND, mode {mode}) -> handle {handle}");
        Ok(handle)
    }

    /// End channel `handle`, if it is one. Called for every handle that
    /// closes; the connection's thread is joined elsewhere.
    pub(crate) fn wl_forget(&mut self, handle: u32) {
        if let Some(c) = self.wl.take(handle) {
            log::debug!("wayland: channel {handle} closed");
            drop_detached(vec![c]);
        }
    }

    /// End every channel (session reset, teardown).
    pub(crate) fn wl_forget_all(&mut self) {
        let conns: Vec<WlConn> = self
            .wl
            .chans
            .drain()
            .filter_map(|(_, c)| match c {
                Chan::Conn(c) => Some(c),
                Chan::Listener => None,
            })
            .collect();
        if !conns.is_empty() {
            log::info!("wayland: closing {} connection(s)", conns.len());
        }
        drop_detached(conns);
    }

    /// WL_SEND and WL_RECV, on the channel the header names. `cap` is what
    /// the guest posted for the response.
    pub(crate) fn serve_wl(&mut self, t: MsgType, payload: &[u8], cap: usize) -> Reply {
        if !self.session.v2 {
            return self.error_reply(libc::EPROTO);
        }
        let handle = self.current_handle;
        let r = match t {
            MsgType::WlSend => self.wl_send(handle, payload),
            MsgType::WlRecv => self.wl_recv(handle, payload, cap),
            _ => Err(libc::EPROTO),
        };
        r.unwrap_or_else(|e| self.error_reply(e))
    }

    fn wl_conn(&self, handle: u32) -> Result<&WlConn, i32> {
        match self.wl.chans.get(&handle) {
            Some(Chan::Conn(c)) => Ok(c),
            // The listener only says when to ACCEPT.
            Some(Chan::Listener) => Err(libc::ENOTCONN),
            None => Err(libc::EBADF),
        }
    }

    fn wl_send(&mut self, handle: u32, frame_bytes: &[u8]) -> Result<Reply, i32> {
        let conn = self.wl_conn(handle)?;
        let mut ops = TableSend {
            handles: &self.handles,
            gate: self.export_gate(),
        };
        let resp = conn.send(frame_bytes, &mut ops)?;
        let mut bytes = hdr(MsgType::WlSend, handle, 0, self.current_req_id);
        bytes.extend_from_slice(crate::sys::pod::bytes(&resp));
        Ok(Reply {
            bytes,
            ..Reply::default()
        })
    }

    fn wl_recv(&mut self, handle: u32, payload: &[u8], cap: usize) -> Result<Reply, i32> {
        let req: WlRecvReq = crate::sys::pod::read(payload, 0).ok_or(libc::EINVAL)?;
        self.wl_conn(handle)?;
        // What comes out of the queue is gone from it: a frame that then did
        // not fit the guest's buffer would be records and descriptors lost
        // for good, and the channel desynchronised. So the whole of what
        // `max_bytes` allows must fit what was posted, before anything is
        // taken -- and at least one of the largest record the engine makes
        // (MIN_FRAME), or a big record could never be delivered at all.
        let max_bytes = req.max_bytes as usize;
        if max_bytes < frame::MIN_FRAME {
            log::warn!(
                "WL_RECV on {handle}: room for {max_bytes} bytes, under the {} a record may need",
                frame::MIN_FRAME
            );
            return Err(libc::EINVAL);
        }
        if HDR + max_bytes > cap {
            log::warn!(
                "WL_RECV on {handle}: asks for up to {max_bytes} bytes but posted {cap}; refused \
                 before taking anything"
            );
            return Err(libc::EMSGSIZE);
        }
        let host = self.wl_host();
        let conn = match self.wl.chans.get(&handle) {
            Some(Chan::Conn(c)) => c,
            _ => return Err(libc::EBADF),
        };
        let owner = self.current_owner;
        let mut ops = TableRecv {
            handles: &mut self.handles,
            host: &*host,
            created: Vec::new(),
            owner,
        };
        let f = conn.recv(req.max_bytes, req.max_desc, &mut ops)?;
        let created = ops.created;
        let mut bytes = hdr(MsgType::WlRecv, handle, 0, self.current_req_id);
        bytes.extend_from_slice(&f);
        Ok(Reply {
            bytes,
            stamp_at: None,
            // If this reply never reaches the guest (its ring was reset under
            // it) nobody else knows these handles: the transport closes them.
            created,
        })
    }
}
