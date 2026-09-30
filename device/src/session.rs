// SPDX-License-Identifier: Apache-2.0
//! Protocol v2 on the backend: the session, and the messages only it serves.
//!
//! A *session* is one guest driver instance's view of this backend: its
//! handles, its watches, whether it said HELLO. It begins with the device and
//! is reset -- every handle closed, every watch dropped, every executor job
//! still queued withdrawn -- when the device is reset or when a new driver
//! instance says HELLO with `HELLO_F_FRESH`. Without the reset, a guest that
//! reboots leaves its host files open: a card that is DRM master stays master
//! and the rebooted compositor cannot take it back, a lease stays granted, the
//! host output stays dark.
//!
//! Only a successful HELLO switches a session to v2. Until then every v2
//! message is answered the way an old backend answers an unknown one
//! (-EPROTO), the event pump speaks only 16-byte EVENT_READY, and nothing is
//! deferred -- a v1 guest has one completion shared by every waiter, and a
//! response arriving out of order would wake the wrong one.
//!
//! IOCTL2 is split in three so a call that may wait runs without the backend
//! mutex: [`NvidiaBackend::serve`] validates and prepares under it and hands
//! back a [`PendingIoctl2`]; the transport runs [`PendingIoctl2::execute`] on
//! the target file's executor with no lock held; [`NvidiaBackend::finish_ioctl2`]
//! adopts what the host produced and builds the response, under the mutex
//! again. A call the schema says never waits -- every render-node call but a
//! few -- runs all three under the one hold of the mutex, on the queue
//! thread, and on the handle table's own descriptor: nothing can close it
//! while the mutex is held, so it needs no duplicate of its own
//! (`serve_ioctl2`).

#![forbid(unsafe_code)]

use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

use protocol::messages::*;

use crate::hostfd::{self, HandleKind, HostOp};
use crate::nvidia::NvidiaBackend;
use crate::privfd::{self, PrivateFd};
use crate::pump::PumpCmd;
use crate::schema::SchemaClass;
use crate::xfer;

/// Request and response limits when the guest negotiated indirect
/// descriptors: one descriptor then describes a table of up to 65535 entries
/// (virtio-queue 0.18.0, src/chain.rs, :119-145), so a 4 MiB buffer built
/// from page chunks fits whatever the ring size.
pub const MAX_XFER_INDIRECT: u32 = 4 << 20;
/// Without them every chunk costs a ring slot, and a 256-entry ring shared by
/// every request in flight bounds what one request may take.
pub const MAX_XFER_DIRECT: u32 = 256 << 10;

const HDR: usize = size_of::<MsgHeader>();

/// What this backend was started with.
#[derive(Clone, Debug, Default)]
pub struct BackendConfig {
    /// Compositor-VM mode: offer the host card nodes to the guest.
    pub kms_card: bool,
    /// The host compositor's socket, for the Wayland proxy.
    pub wayland_socket: Option<PathBuf>,
    /// Where host clients reach a guest compositor, in export mode.
    pub wayland_export: Option<PathBuf>,
    /// Whether fence and syncobj schemas are available. Set by the code that
    /// provides them; until then the guest is told there are none.
    pub fences: bool,
    /// Whether an NVKMS schema exists for the host driver version.
    pub nvkms_table: bool,
    /// Serve the paths only CUDA and other compute needs (`--allow-compute`):
    /// `/dev/nvidia-uvm` (every open of it or its tools device is refused
    /// without), and with it UVM's sharing mode and the UVM aperture
    /// (BCAP_UVM_MAP), and memory registered by its guest pages
    /// (BCAP_OS_DESC). Off by default: graphics, video and display need none
    /// of them (SECURITY.md, "Compute").
    pub allow_compute: bool,
}

impl BackendConfig {
    /// `HelloResp::backend_caps`.
    pub fn caps(&self) -> u32 {
        // Segmented deep blocks are this code's, whatever it was started
        // with (deepseg.rs).
        let mut caps = BCAP_DEEP_SEGS;
        if self.kms_card {
            caps |= BCAP_KMS_CARD;
        }
        if self.wayland_socket.is_some() {
            caps |= BCAP_WAYLAND;
        }
        if self.fences {
            caps |= BCAP_FENCES;
        }
        if self.nvkms_table {
            caps |= BCAP_NVKMS_TABLE;
        }
        if self.wayland_export.is_some() {
            caps |= BCAP_WL_EXPORT;
        }
        if self.allow_compute {
            caps |= BCAP_COMPUTE;
        }
        caps
    }
}

/// Per-session state.
#[derive(Debug, Default)]
pub struct Session {
    /// Set by a successful HELLO, cleared by a reset.
    pub v2: bool,
    /// Bumped by every reset. Work prepared in one session and finished in
    /// another is discarded: its handles would land in a table the new guest
    /// knows nothing about, and never be closed.
    pub generation: u64,
    /// The guest says which of its processes makes each RM_ALLOC and
    /// RM_DUP_OBJECT (HELLO's GCAP_PROC_ID, rmshare.rs).
    pub proc_ids: bool,
    /// With them, the caller's euid, and on every RM_CONTROL too (HELLO's
    /// GCAP_PROC_EUID, BCAP_PROC_EUID).
    pub proc_euid: bool,
    /// A guest that does not has been told of in the log, once a session.
    pub proc_ids_noted: bool,
    /// The guest arms legacy readiness (HELLO's GCAP_ARMS_READY,
    /// BCAP_ARMED_READY): one report per W_ARM.
    pub armed_ready: bool,
}

/// A response, ready to be written into the chain it answers.
#[derive(Debug, Default)]
pub struct Reply {
    pub bytes: Vec<u8>,
    /// Where to write `CLOCK_MONOTONIC` in nanoseconds just before the chain
    /// is handed back (TIME_SYNC), so the stamp is as close as it can be to
    /// the moment the guest's callback sees it.
    pub stamp_at: Option<usize>,
    /// Handles this response creates. If it is never delivered (the ring was
    /// reset under it) the transport closes them: nobody else knows they
    /// exist.
    pub created: Vec<u32>,
}

impl Reply {
    /// A bare header refusing message `t` (request `req_id`) with `errno`,
    /// given positive and stored negated.
    pub fn error(t: MsgType, req_id: u32, errno: i32) -> Self {
        debug_assert!(errno > 0, "an error reply with errno {errno}");
        Reply {
            bytes: hdr(t, 0, -errno.saturating_abs(), req_id),
            ..Reply::default()
        }
    }

    /// A success of message `t`: the header carrying `handle`, then `body`.
    pub fn ok(t: MsgType, handle: u32, req_id: u32, body: &[u8]) -> Self {
        let mut bytes = hdr(t, handle, 0, req_id);
        bytes.extend_from_slice(body);
        Reply {
            bytes,
            ..Reply::default()
        }
    }

    /// Write the monotonic clock at `stamp_at`, if this reply wants one, and
    /// `CLOCK_REALTIME` and `CLOCK_MONOTONIC_RAW` after it when the reply has
    /// room for them (TimeSyncResp2). Read back to back, so the guest can take
    /// each one's distance from the monotonic clock as of one instant.
    pub fn stamp(&mut self) {
        if let Some(off) = self.stamp_at {
            let clocks = [
                monotonic_ns(),
                clock_ns(libc::CLOCK_REALTIME),
                clock_ns(libc::CLOCK_MONOTONIC_RAW),
            ];
            for (i, ns) in clocks.into_iter().enumerate() {
                let at = off + 8 * i;
                if let Some(dst) = self.bytes.get_mut(at..at + 8) {
                    dst.copy_from_slice(&ns.to_le_bytes());
                }
            }
        }
    }
}

/// `CLOCK_MONOTONIC`, in nanoseconds: the clock DRM vblank and flip
/// timestamps are in, which is what the guest translates.
pub fn monotonic_ns() -> u64 {
    clock_ns(libc::CLOCK_MONOTONIC)
}

fn clock_ns(clock: libc::clockid_t) -> u64 {
    crate::sys::proc::clock_ns(clock)
}

/// What serving one request produced.
// One per request, moved once and never held in bulk: boxing the large arm
// would only add an allocation to every call.
#[allow(clippy::large_enum_variant)]
pub enum Outcome {
    /// The response, now.
    Reply(Reply),
    /// An IOCTL2 that has been validated and must now be executed, with no
    /// backend lock held, then finished.
    Ioctl2(PendingIoctl2),
}

/// Work the queue thread must not wait for, between `prepare` and `finish`:
/// an IOCTL2, or one of the two HOST_OPs that can take the display's locks
/// (see [`KmsCall`]).
pub struct PendingIoctl2(Pending);

// As `Outcome`: one per call in flight.
#[allow(clippy::large_enum_variant)]
enum Pending {
    Ioctl2(Ioctl2Call),
    Kms(KmsCall),
}

impl PendingIoctl2 {
    /// The executor queue this call belongs on, or `None` to run it inline on
    /// the queue thread.
    pub fn executor_key(&self) -> Option<u64> {
        match &self.0 {
            Pending::Ioctl2(c) => c.executor_key(),
            Pending::Kms(k) => Some(k.executor_key()),
        }
    }

    /// The answer for a call that a session reset withdrew before it ran.
    /// Consumes the call, closing every descriptor it held.
    pub fn cancelled_reply(self) -> Reply {
        match self.0 {
            Pending::Ioctl2(c) => c.cancelled_reply(),
            Pending::Kms(k) => k.cancelled_reply(),
        }
    }

    /// Run the host side. Takes no lock; may block for as long as the host
    /// call does.
    pub fn execute(&mut self) {
        match &mut self.0 {
            Pending::Ioctl2(c) => c.execute(),
            Pending::Kms(k) => k.execute(),
        }
    }
}

/// Executor keys for work that has no host file of its own yet: one serial
/// queue per card, above every handle's (handles are u32).
const CARD_KEY: u64 = 1 << 32;

/// OPEN_KMS and DROP_IF_MASTER (S-34). Neither is the quick call it looks
/// like. Opening a card node while the host has no master makes the new file
/// master (drm_master_open), and nvidia-drm's master_set then grabs NVKMS
/// ownership under nvkms_lock (nvidia-drm-drv.c:954-966), behind any other
/// client's SET_MODE or FLIP. Dropping master runs nv_drm_master_drop:
/// mode_config.mutex, every modeset lock, a disable-all commit and
/// releaseOwnership (nvidia-drm-drv.c:1038-1068). On the queue thread either
/// stalled every RM call of the VM behind the display. So they run on an
/// executor like a card IOCTL2: DROP_IF_MASTER in the file's own queue, after
/// whatever it already has queued; OPEN_KMS, which has no file yet, in a
/// queue per card. The new file is adopted under the backend lock only when
/// the job finishes, so a session reset in between closes it instead.
struct KmsCall {
    op: KmsOp,
    generation: u64,
    req_id: u32,
    /// The guest process that asked: what OPEN_KMS opens is charged to it
    /// (quota.rs), whoever the backend served in the meantime.
    owner: crate::quota::Owner,
}

enum KmsOp {
    Open {
        card: u32,
        path: String,
        opened: Option<Result<OwnedFd, i32>>,
    },
    DropIfMaster {
        handle: u32,
        /// A duplicate, so a CLOSE racing the job cannot pull the file away.
        fd: OwnedFd,
        dropped: bool,
    },
}

impl KmsCall {
    fn executor_key(&self) -> u64 {
        match &self.op {
            KmsOp::Open { card, .. } => CARD_KEY | u64::from(*card),
            KmsOp::DropIfMaster { handle, .. } => u64::from(*handle),
        }
    }

    fn cancelled_reply(self) -> Reply {
        match self.op {
            KmsOp::Open {
                opened: Some(Ok(fd)),
                ..
            } => crate::closer::close(fd),
            KmsOp::DropIfMaster { fd, .. } => crate::closer::close(fd),
            KmsOp::Open { .. } => {}
        }
        Reply::error(MsgType::HostOp, self.req_id, libc::ECANCELED)
    }

    fn execute(&mut self) {
        match &mut self.op {
            KmsOp::Open { path, opened, .. } => {
                // A card file the guest just closed may still be closing
                // (closer.rs); if it was master, the file opened now must
                // find master free, as it would natively once close()
                // returned.
                if !crate::closer::wait_idle(std::time::Duration::from_secs(3)) {
                    log::warn!("OPEN_KMS: earlier closes still running after 3 s; opening anyway");
                }
                *opened = Some(open_card(path));
            }
            KmsOp::DropIfMaster { fd, dropped, .. } => {
                *dropped = hostfd::drop_master(fd.as_raw_fd());
            }
        }
    }
}

/// Open a card node for KMS: O_NONBLOCK, as every DRM-class file the pump
/// reads is (hostfd::set_nonblock).
fn open_card(path: &str) -> Result<OwnedFd, i32> {
    // Fuzzing (device/src/fuzzing): no real device is ever opened.
    #[cfg(fuzzing)]
    let path = {
        let _ = path;
        "/dev/null"
    };
    crate::sys::fd::open_path(path, libc::O_RDWR | libc::O_CLOEXEC | libc::O_NONBLOCK).map_err(
        |e| {
            log::warn!("OPEN_KMS: {path}: {e}");
            errno_of(&e)
        },
    )
}

/// An IOCTL2 between `prepare` and `finish`.
struct Ioctl2Call {
    prepared: xfer::Prepared,
    /// For a call that runs on an executor, a duplicate of the target's
    /// descriptor: the call owns it for as long as it runs, so a CLOSE racing
    /// it cannot pull the file away. None for one that runs inline, under
    /// the backend mutex, on `table_fd` (`serve_ioctl2`).
    target_fd: Option<OwnedFd>,
    target: u32,
    /// Who what the call makes is charged to (quota.rs), taken when it was
    /// served: the target may be closed before it finishes.
    owner: crate::quota::Owner,
    executor: bool,
    generation: u64,
    req_id: u32,
    cap: usize,
    /// The descriptor number the handle table held for `target` when the
    /// call was prepared (`target_fd` is a duplicate of it).
    table_fd: RawFd,
    /// When serving began, and how long preparing took (pacing report).
    t0: std::time::Instant,
    prep_ns: u64,
}

impl Ioctl2Call {
    /// The executor queue this call belongs on, or `None` to run it inline on
    /// the queue thread. Card, lease and modeset calls always go to an
    /// executor, whatever the schema says: the queue thread must never wait
    /// on a modeset lock or `nvkms_lock`.
    fn executor_key(&self) -> Option<u64> {
        self.executor.then_some(self.target as u64)
    }

    /// The answer for a call that a session reset withdrew before it ran.
    /// Consumes the call, closing every descriptor it held.
    fn cancelled_reply(self) -> Reply {
        Reply::error(MsgType::Ioctl2, self.req_id, libc::ECANCELED)
    }

    /// Run it on its own duplicate. A call without one is an inline call,
    /// which only `serve_ioctl2` runs, under the mutex: here it is left
    /// unrun, and answered as cancelled.
    fn execute(&mut self) {
        let Some(fd) = &self.target_fd else {
            log::error!(
                "IOCTL2 {} on handle {}: an inline call reached an executor; not run",
                self.prepared.name(),
                self.target
            );
            return;
        };
        let ret = self.prepared.execute(fd.as_raw_fd());
        log::debug!(
            "IOCTL2 {:#x} on handle {}: host returned {ret}",
            self.prepared.cmd(),
            self.target
        );
    }
}

/// `xfer::Env` over the backend's table.
struct BackendEnv<'a> {
    backend: &'a NvidiaBackend,
}

impl xfer::Env for BackendEnv<'_> {
    fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)> {
        self.backend.handles.dup(handle)
    }

    fn kind(&self, handle: u32) -> Option<HandleKind> {
        self.backend.handles.kind(handle)
    }

    fn nvkms_version(&self) -> Option<abi::version::DriverVersion> {
        self.backend.driver
    }

    /// Made by `prepare_ioctl2` before this Env exists (it needs the table
    /// mutably), so here it is only looked up.
    fn kms_state(&self, target: u32) -> Option<Arc<crate::kms_state::KmsFileState>> {
        self.backend.kms_states.get(&target).cloned()
    }

    fn hooks(&self) -> Arc<dyn xfer::Hooks> {
        self.backend.hooks.clone()
    }

    fn sys(&self) -> Arc<dyn xfer::Sys> {
        self.backend.xfer_sys.clone()
    }
}

/// `xfer::Finisher` over the backend: what a finished IOCTL2 may adopt, and
/// where.
struct BackendFinisher<'a> {
    backend: &'a mut NvidiaBackend,
    cards: &'a [hostfd::CardNode],
    /// The session was reset while the call ran: nothing it made may land
    /// in a table the new session knows nothing about.
    stale: bool,
    /// Handles adopted, for the reply to carry (and close, if it is never
    /// delivered).
    created: Vec<u32>,
    /// The guest process they are charged to: the owner of the file the
    /// call ran on (quota.rs).
    owner: crate::quota::Owner,
    /// The file the call ran on is still the guest's, unburied.
    target_live: bool,
}

impl xfer::Finisher for BackendFinisher<'_> {
    fn records(&self) -> bool {
        !self.stale && self.target_live
    }

    /// Every descriptor the backend holds: the guest's, in the handle table,
    /// and its own -- the vhost-user socket, guest memory, the pump, the
    /// vrings' eventfds, the window memfd, the cached signalled sync_file --
    /// in the private registry (`privfd`).
    fn is_backend_fd(&self, fd: RawFd) -> bool {
        self.backend.handles.owns_fd(fd) || privfd::is_private(fd)
    }

    /// Classified by what the kernel says the file is, never by where the
    /// schema found it; made non-blocking if it is a DRM file, because the
    /// pump reads those and one blocking read stops every event for the VM.
    /// Refused (the descriptor closes here, and the record carries handle 0)
    /// when the session is gone or the table is full.
    fn adopt(&mut self, fd: OwnedFd) -> (u32, HandleKind) {
        if self.stale {
            return (0, HandleKind::Other);
        }
        let tc = std::time::Instant::now();
        let kind = hostfd::classify(fd.as_fd(), self.cards);
        crate::pacing::PACING.ioctl2_timed(
            "(adopt: classify)",
            u64::try_from(tc.elapsed().as_nanos()).unwrap_or(u64::MAX),
            0,
            0,
        );
        if kind.is_kms()
            && let Err(e) = hostfd::set_nonblock(fd.as_raw_fd())
        {
            log::warn!("IOCTL2: cannot make an adopted DRM file non-blocking: {e}");
        }
        match self.backend.handles.insert_for(fd, kind, self.owner) {
            Ok(h) => {
                self.created.push(h);
                (h, kind)
            }
            Err(_) => {
                log::warn!("IOCTL2: handle table full; closing an adopted {kind:?}");
                (0, HandleKind::Other)
            }
        }
    }

    /// An `I2_FD_CONSUME` handle, used up by a call that ran. Closed the way
    /// a CLOSE closes it, so its watches and window state go too. Not in a
    /// stale session: the reset closed it already, and the number may
    /// belong to nobody or (after a wrap) to someone else.
    fn close_handle(&mut self, handle: u32) {
        if !self.stale {
            let _ = self.backend.close_handle(handle);
        }
    }
}

/// A bare header, echoing the request's type, handle-field choice and id.
pub(crate) fn hdr(t: MsgType, handle: u32, status: i32, req_id: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(HDR);
    b.extend_from_slice(&(t as u32).to_le_bytes());
    b.extend_from_slice(&handle.to_le_bytes());
    b.extend_from_slice(&wire_status(status).to_le_bytes());
    b.extend_from_slice(&req_id.to_le_bytes());
    b
}

/// A reply's status as it may go on the wire: 0, or an errno the kernel
/// knows (-1 to -MAX_ERRNO); anything else -- a positive value, which the
/// guest's v1 paths hand straight to userspace as an ioctl's result -- is
/// EPROTO, as the guest's IOCTL2 path clamps it.
pub(crate) fn wire_status(status: i32) -> i32 {
    const MAX_ERRNO: i32 = 4095;
    if status == 0 || (-MAX_ERRNO..0).contains(&status) {
        status
    } else {
        -libc::EPROTO
    }
}

fn read<T: crate::sys::pod::Pod + Copy>(payload: &[u8]) -> Option<T> {
    crate::sys::pod::read(payload, 0)
}

fn bytes_of<T: crate::sys::pod::Pod>(v: &T) -> &[u8] {
    crate::sys::pod::bytes(v)
}

fn errno_of(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

/// A HOST_OP reply's body: `res`, at most [`OP_MAX_RES`] results, then
/// `tail`, what an op says after its fixed reply.
fn host_op_body(res: &[u64], tail: &[u8]) -> Vec<u8> {
    debug_assert!(res.len() <= OP_MAX_RES, "{} HOST_OP results", res.len());
    let n = res.len().min(OP_MAX_RES);
    let mut resp = HostOpResp {
        nres: n as u32,
        pad: 0,
        res: [0; OP_MAX_RES],
    };
    resp.res[..n].copy_from_slice(&res[..n]);
    let mut body = bytes_of(&resp).to_vec();
    body.extend_from_slice(tail);
    body
}

impl NvidiaBackend {
    /// A failure response to the message being served.
    pub(crate) fn error_reply(&self, errno: i32) -> Reply {
        Reply::error(self.current_msg, self.current_req_id, errno)
    }

    fn ok_reply(&self, handle: u32, body: &[u8]) -> Reply {
        Reply::ok(self.current_msg, handle, self.current_req_id, body)
    }

    /// Serve one of the messages only a v2 session may send.
    pub(crate) fn serve_v2(&mut self, t: MsgType, payload: &[u8], cap: usize) -> Outcome {
        if t != MsgType::Hello && !self.session.v2 {
            // Exactly what an old backend answers, so a guest that skipped
            // HELLO learns nothing it would not have learned from one.
            return Outcome::Reply(self.error_reply(libc::EPROTO));
        }
        let r = match t {
            MsgType::Hello => self.serve_hello(payload),
            // The long form (TimeSyncResp2) only where the guest posted room
            // for it: an older guest's 8-byte buffer gets the 8-byte reply.
            MsgType::TimeSync => Ok(Reply {
                stamp_at: Some(HDR),
                ..self.ok_reply(
                    0,
                    if cap >= HDR + size_of::<TimeSyncResp2>() {
                        &[0u8; size_of::<TimeSyncResp2>()][..]
                    } else {
                        &[0u8; size_of::<TimeSyncResp>()][..]
                    },
                )
            }),
            MsgType::Watch => self.serve_watch(payload),
            MsgType::Unwatch => self.serve_unwatch(payload),
            MsgType::HostOp => match self.serve_host_op(payload) {
                Ok(o) => return o,
                Err(e) => Err(e),
            },
            MsgType::Ioctl2 => return self.serve_ioctl2(payload, cap),
            _ => Err(libc::EPROTO),
        };
        Outcome::Reply(r.unwrap_or_else(|e| self.error_reply(e)))
    }

    fn serve_hello(&mut self, payload: &[u8]) -> Result<Reply, i32> {
        let req = read::<HelloReq>(payload).ok_or(libc::EINVAL)?;
        if req.proto != PROTO_V2 {
            log::warn!(
                "HELLO for protocol {}; this backend speaks {PROTO_V2}",
                req.proto
            );
            return Err(libc::EPROTO);
        }
        if req.flags & HELLO_F_FRESH != 0 {
            self.session_reset("a new guest driver instance said HELLO");
        }
        self.session.v2 = true;
        self.session.armed_ready = req.guest_caps & GCAP_ARMS_READY != 0;
        self.pump_cmds.push(PumpCmd::SetV2(true));
        let num_cards = if self.config.kms_card {
            self.host_nodes().cards.len() as u32
        } else {
            0
        };
        let mut backend_caps = self.config.caps();
        if self.hello_uvm_aperture(&req) {
            backend_caps |= BCAP_UVM_MAP;
        }
        // Memory the guest already has is registered by its pages, which
        // takes knowing where guest RAM is (osdesc.rs), and is served only
        // for compute.
        if self.guest_ram.is_some() && self.config.allow_compute {
            backend_caps |= BCAP_OS_DESC;
        }
        // RM objects kept to the guest process that made their client
        // (rmshare.rs), for a guest that can say which that is; and a second
        // client a call names held to RM's rule, for one that also says the
        // caller's euid.
        self.session.proc_ids = req.guest_caps & GCAP_PROC_ID != 0;
        self.session.proc_euid = self.session.proc_ids && req.guest_caps & GCAP_PROC_EUID != 0;
        if self.session.proc_ids {
            backend_caps |= BCAP_PROC_ID;
        }
        if self.session.proc_euid {
            backend_caps |= BCAP_PROC_EUID;
        }
        if self.inject.registry().is_some() {
            backend_caps |= BCAP_INJECT;
        }
        // Legacy readiness once per arm, for a guest that arms it: the host
        // events nobody in the guest waits on -- most of them, in a render
        // loop -- stop costing a record and an interrupt each.
        if self.session.armed_ready {
            backend_caps |= BCAP_ARMED_READY;
        }
        let resp = HelloResp {
            proto: PROTO_V2,
            backend_caps,
            max_req: self.max_req,
            max_resp: self.max_resp,
            num_cards,
            reserved: [0; 3],
        };
        log::info!(
            "HELLO: protocol v2, caps {:#x}, limits {}/{} bytes, {num_cards} card(s)",
            resp.backend_caps,
            resp.max_req,
            resp.max_resp
        );
        Ok(self.ok_reply(0, bytes_of(&resp)))
    }

    /// Whether this session maps UVM pools into the aperture the guest says
    /// it has (uvmmap.rs): only when it has one, the VMM's request channel is
    /// up to place into it, and the host's UVM takes multi-process sharing
    /// mode, without which the VMM cannot map a UVM file at all. Otherwise
    /// every MMAP of a UVM file is EINVAL, as it always was, and the VMM is
    /// never asked.
    fn hello_uvm_aperture(&mut self, req: &HelloReq) -> bool {
        let sharing = self
            .driver
            .and_then(abi::schema::uvm_table)
            .is_some_and(|t| {
                t.init_flags_mask & crate::guestptr::UVM_INIT_FLAGS_MULTI_PROCESS_SHARING_MODE != 0
            });
        let len = if req.guest_caps & GCAP_UVM_APERTURE != 0
            && self.config.allow_compute
            && self.has_window()
            && sharing
        {
            u64::from(req.uvm_aperture_mib) << 20
        } else {
            0
        };
        // Placements outlive only a HELLO without FRESH, which keeps them
        // where they are.
        self.uvm_maps.set_aperture(len);
        let have = self.uvm_maps.aperture_len();
        if have > 0 {
            log::info!(
                "HELLO: UVM pools map into the guest's {} MiB aperture (using {} MiB)",
                req.uvm_aperture_mib,
                have >> 20
            );
        }
        have >= crate::uvmmap::CHUNK
    }

    fn serve_watch(&mut self, payload: &[u8]) -> Result<Reply, i32> {
        let req = read::<WatchReq>(payload).ok_or(libc::EINVAL)?;
        let kind = self.handles.kind(req.handle).ok_or(libc::EBADF)?;
        if req.flags == W_ARM {
            // An RM device handle's armed legacy watch, which OPEN made.
            if !self.session.armed_ready || !kind.readiness_is_armed() {
                return Err(libc::EINVAL);
            }
            self.pump_cmds.push(PumpCmd::Arm { handle: req.handle });
            return Ok(self.ok_reply(0, &[]));
        }
        let mode = hostfd::watch_mode(kind, req.flags, req.cookie).inspect_err(|_| {
            log::warn!(
                "WATCH handle {} ({kind:?}) with flags {:#x}: not a watch this kind supports",
                req.handle,
                req.flags
            );
        })?;
        let (fd, _) = self.handles.dup(req.handle).ok_or(libc::EBADF)?;
        self.pump_cmds.push(PumpCmd::Watch {
            handle: req.handle,
            fd,
            mode,
        });
        Ok(self.ok_reply(0, &[]))
    }

    fn serve_unwatch(&mut self, payload: &[u8]) -> Result<Reply, i32> {
        let req = read::<UnwatchReq>(payload).ok_or(libc::EINVAL)?;
        self.handles.kind(req.handle).ok_or(libc::EBADF)?;
        self.pump_cmds.push(PumpCmd::Unwatch { handle: req.handle });
        Ok(self.ok_reply(0, &[]))
    }

    fn serve_host_op(&mut self, payload: &[u8]) -> Result<Outcome, i32> {
        let req = read::<HostOpReq>(payload).ok_or(libc::EINVAL)?;
        crate::pacing::PACING.host_op(req.op);
        let nodes = self.host_nodes();
        let cards = self.config.kms_card.then_some(nodes.cards.as_slice());
        let handles = &self.handles;
        let op = hostfd::check_host_op(&req, &|h| handles.kind(h), cards).inspect_err(|e| {
            log::warn!("HOST_OP {} refused before running: errno {e}", req.op);
        })?;
        // Releases of memory registered by its pages: a list, after the
        // fixed reply (osdesc.rs).
        if let HostOp::OsdescReap { ack } = op {
            let (last, ids) = self.osdesc_reap(ack);
            let tail: Vec<u8> = ids.iter().flat_map(|id| id.to_le_bytes()).collect();
            let body = host_op_body(&[last, ids.len() as u64], &tail);
            return Ok(Outcome::Reply(self.ok_reply(0, &body)));
        }
        // A buffer the capture helper injected: its description goes after
        // the fixed reply (inject/backend.rs).
        if let HostOp::InjectOpenSyncobj { file, id, token } = op {
            let h = self.inject_open_syncobj(file, id, &token)?;
            return Ok(Outcome::Reply(self.ok_reply(0, &host_op_body(&[h], &[]))));
        }
        if let HostOp::InjectOpen { file, id, token } = op {
            let (res, info) = self.inject_open(file, id, &token)?;
            let body = host_op_body(&res, &info.to_bytes());
            return Ok(Outcome::Reply(self.ok_reply(0, &body)));
        }
        // The two that can wait on the display go to an executor (KmsCall).
        let kms = match op {
            HostOp::OpenKms { card, .. } => Some(KmsOp::Open {
                card,
                path: nodes.cards[card as usize].path(),
                opened: None,
            }),
            HostOp::DropIfMaster { card } => Some(KmsOp::DropIfMaster {
                handle: card,
                fd: self.handles.dup(card).ok_or(libc::EBADF)?.0,
                dropped: false,
            }),
            _ => None,
        };
        if let Some(op) = kms {
            return Ok(Outcome::Ioctl2(PendingIoctl2(Pending::Kms(KmsCall {
                op,
                generation: self.session.generation,
                req_id: self.current_req_id,
                owner: self.current_owner,
            }))));
        }
        let (res, created) = self.run_host_op(op).inspect_err(|e| {
            log::warn!("HOST_OP {} failed on the host: errno {e}", req.op);
        })?;
        Ok(Outcome::Reply(Reply {
            created,
            ..self.ok_reply(0, &host_op_body(&res, &[]))
        }))
    }

    fn insert(&mut self, fd: OwnedFd, kind: HandleKind) -> Result<u32, i32> {
        self.handles
            .insert_for(fd, kind, self.current_owner)
            .map_err(|e| {
                log::warn!("handle table full; refusing a new {kind:?}");
                e.errno()
            })
    }

    fn raw(&self, h: u32) -> Result<i32, i32> {
        self.handles.get_raw(h).map_err(|_| libc::EBADF)
    }

    /// Run a checked HOST_OP. Returns the result words and the handles made.
    ///
    /// Every op here is non-blocking on the host: PRIME export and import,
    /// sync_file merges and eventfds take short kernel locks and no display
    /// waits, so they run on the queue thread. OPEN_KMS and DROP_IF_MASTER
    /// are not: they go to an executor (KmsCall) and never get here.
    fn run_host_op(&mut self, op: HostOp) -> Result<(Vec<u64>, Vec<u32>), i32> {
        let io = |e: std::io::Error| errno_of(&e);
        match op {
            HostOp::PrimeExport { file, gem } => {
                // Not a fence context, not an injected capture buffer: the
                // one gate every export path asks (exportgate.rs).
                let gate = self.export_gate();
                gate.may_export(file, gem)?;
                let dmabuf = hostfd::prime_export(self.raw(file)?, gem).map_err(io)?;
                self.export_gate().may_leave(dmabuf.as_fd())?;
                let size = hostfd::dmabuf_size(dmabuf.as_raw_fd()).unwrap_or(0);
                let h = self.insert(dmabuf, HandleKind::Dmabuf)?;
                Ok((vec![h as u64, size], vec![h]))
            }
            HostOp::DmabufImport { file, dmabuf } => {
                let (file, dmabuf) = (self.raw(file)?, self.raw(dmabuf)?);
                let gem = hostfd::prime_import(file, dmabuf).map_err(io)?;
                // The third word is what the object is, for the guest proxy to
                // answer IDENTIFY with (hostfd::import_type); a guest that
                // predates it reads two words, and a backend that predates it
                // sends two, which the guest reads as NVKMS -- the old answer.
                let described = hostfd::dmabuf_size(dmabuf)
                    .map_err(|e| errno_of(&e))
                    .and_then(|size| {
                        let ty = hostfd::import_type(hostfd::gem_identify(file, gem))?;
                        Ok((size, ty))
                    });
                match described {
                    Ok((size, ty)) => Ok((vec![gem as u64, size, u64::from(ty)], vec![])),
                    Err(e) => {
                        // A GEM handle the guest will never hear about is a
                        // leak in the host file; take it back.
                        let _ = hostfd::gem_close(file, gem);
                        Err(e)
                    }
                }
            }
            HostOp::SyncMerge { fences } => {
                let first = self.raw(fences[0])?;
                let mut acc = hostfd::sync_merge(first, first).map_err(io)?;
                for &f in &fences[1..] {
                    acc = hostfd::sync_merge(acc.as_raw_fd(), self.raw(f)?).map_err(io)?;
                }
                let h = self.insert(acc, HandleKind::SyncFile)?;
                Ok((vec![h as u64], vec![h]))
            }
            HostOp::NewEventfd => {
                let h = self.insert(hostfd::new_eventfd().map_err(io)?, HandleKind::Eventfd)?;
                Ok((vec![h as u64], vec![h]))
            }
            HostOp::FdKind { handle } => {
                let kind = self.handles.kind(handle).ok_or(libc::EBADF)?;
                Ok((
                    vec![kind.wire() as u64, kind.index().unwrap_or(0) as u64],
                    vec![],
                ))
            }
            HostOp::SignaledSyncFile => {
                if self.signaled.is_none() {
                    let paths: Vec<String> = self
                        .host_nodes()
                        .dri
                        .iter()
                        .map(|d| format!("/dev/dri/{}", d.name))
                        .collect();
                    self.signaled = Some(PrivateFd::new(
                        hostfd::signaled_sync_file(&paths).map_err(io)?,
                    ));
                }
                // The duplicate is the guest's: a plain descriptor, not a
                // private one.
                let fd = self
                    .signaled
                    .as_ref()
                    .expect("filled above")
                    .as_fd()
                    .try_clone_to_owned()
                    .map_err(io)?;
                let h = self.insert(fd, HandleKind::SyncFile)?;
                Ok((vec![h as u64], vec![h]))
            }
            HostOp::OpenKms { .. } | HostOp::DropIfMaster { .. } => {
                unreachable!("serve_host_op sends these to an executor")
            }
            HostOp::InjectOpen { .. }
            | HostOp::InjectOpenSyncobj { .. }
            | HostOp::OsdescReap { .. } => {
                unreachable!("serve_host_op answers it itself")
            }
            HostOp::SyncobjWatch { key, cookie } => Ok((self.syncobj_watch(key, cookie)?, vec![])),
            HostOp::CloseMany { handles } => {
                let closed = handles
                    .iter()
                    .filter(|&&h| self.close_handle(h).is_ok())
                    .count();
                Ok((vec![closed as u64], vec![]))
            }
        }
    }

    /// A call that never waits runs here and now, with `&mut self` -- the
    /// backend mutex -- held from its preparing to its finishing: the handle
    /// table cannot change in between, so the descriptor the table holds for
    /// the target is the one the call was prepared against, and it is used
    /// as it is. The duplicate an executor call takes, and closes after, is
    /// two system calls, a tenth of a render call's backend time; and the
    /// queue thread, which would have waited for the call anyway, takes the
    /// mutex once rather than twice. What waits for this is only the other
    /// holders of the mutex, for the few microseconds such a call takes on
    /// the host.
    fn serve_ioctl2(&mut self, payload: &[u8], cap: usize) -> Outcome {
        match self.prepare_ioctl2(payload, cap) {
            Ok(PendingIoctl2(Pending::Ioctl2(mut call))) if !call.executor => {
                call.prepared.execute(call.table_fd);
                Outcome::Reply(self.finish_ioctl2(PendingIoctl2(Pending::Ioctl2(call))))
            }
            Ok(p) => Outcome::Ioctl2(p),
            Err(e) => Outcome::Reply(self.error_reply(e)),
        }
    }

    fn prepare_ioctl2(&mut self, payload: &[u8], cap: usize) -> Result<PendingIoctl2, i32> {
        let t0 = std::time::Instant::now();
        let req = read::<Ioctl2Req>(payload).ok_or(libc::EINVAL)?;
        // The response's fixed part must fit before anything runs; the host
        // call cannot be taken back once it has.
        if cap < HDR + size_of::<Ioctl2Resp>() {
            return Err(libc::EMSGSIZE);
        }
        // The schema never lists these; refused here too, so no schema table
        // can let one through by mistake.
        if hostfd::refused_everywhere(req.cmd) {
            return Err(libc::EPERM);
        }
        let target = self.current_handle;
        let kind = self.handles.kind(target).ok_or(libc::EBADF)?;
        // A lease file the backend closed when its lease ended (kms.rs,
        // "lease ends"): the handle lives on only to be closed.
        if self.handles.is_buried(target) {
            return Err(libc::ENODEV);
        }
        // An NVKMS call makes no GEM handle, so it has no render file to
        // put one in, and a guest modeset file has none to name: 0 there.
        let nvkms_without_render = kind == HandleKind::Dev(DeviceKind::Modeset) && req.render == 0;
        if !nvkms_without_render
            && !matches!(
                self.handles.kind(req.render),
                Some(HandleKind::DriRender(_))
            )
        {
            log::warn!(
                "IOCTL2 {:#x}: render field {} is not a render handle",
                req.cmd,
                req.render
            );
            return Err(libc::EBADF);
        }
        let class = match kind {
            HandleKind::DriRender(_) => {
                // A render-class call runs in the render file itself; any
                // other render handle here would re-home its results into a
                // file the call never touched.
                if req.render != target {
                    return Err(libc::EINVAL);
                }
                SchemaClass::Render
            }
            HandleKind::DrmCard(_) | HandleKind::DrmLease(_) => SchemaClass::Kms,
            HandleKind::Dev(DeviceKind::Modeset) => SchemaClass::Modeset,
            _ => {
                log::warn!(
                    "IOCTL2 {:#x} on handle {target} ({kind:?}): no schema class",
                    req.cmd
                );
                return Err(libc::EPERM);
            }
        };
        if class == SchemaClass::Kms {
            let vm = self.vm_kms.clone();
            self.kms_states
                .entry(target)
                .or_insert_with(|| Arc::new(crate::kms_state::KmsFileState::in_vm(vm)));
            // Scanout checksums: our card or a lessee only (kms.rs).
            self.crc_gate(req.cmd, target, kind)?;
        }
        if class == SchemaClass::Modeset {
            // A per-head gate may rest on a lease's grant; the host takes
            // those back without telling anyone (kms.rs, "lease ends").
            self.recheck_granting_leases();
        }
        let prepared = xfer::prepare(&BackendEnv { backend: self }, class, target, kind, payload)?;
        // A fence context over memory registered by its pages (osdesc.rs)
        // would be a holder of it the registration does not know: NVKMS
        // duplicates the surface and kernel-maps its memory, and would go
        // on writing into the pages after the guest unpinned them for
        // another process. Refused, as the cheap fail-closed answer.
        if let Some((client, surface)) = crate::semsurf::SemsurfPolicy::ctx_surface(&prepared)
            && self.osdesc.holds(client, surface)
        {
            log::warn!(
                "SEMSURF_FENCE_CTX_CREATE on handle {target}: surface {client:#x}/{surface:#x} \
                 holds memory registered by its pages; refused"
            );
            return Err(libc::EPERM);
        }
        // The whole reply must fit what the guest posted, and that is known
        // now: response_len() is exact up to descriptors and GEM handles the
        // host might not produce. A call that ran and then could not answer
        // would have changed host state the guest never hears of -- a lease
        // made, a framebuffer added -- so it is refused before it runs, and
        // the guest (which sized its buffer from the same schema) never sees
        // this unless it lied about its own capacity.
        let need = HDR + prepared.response_len();
        if need > cap {
            log::warn!(
                "IOCTL2 {} on handle {target}: a reply of up to {need} bytes does not fit \
                 the {cap} bytes posted; refused before running",
                prepared.name()
            );
            return Err(libc::EMSGSIZE);
        }
        let executor = prepared.wants_executor() || class != SchemaClass::Render;
        // The table's own descriptor for the target, which an executor
        // call's duplicate shares a file with (see finish_ioctl2), and on
        // which an inline call runs (see serve_ioctl2).
        let table_fd = self.handles.get_raw(target).map_err(|_| libc::EBADF)?;
        let target_fd = if executor {
            Some(self.handles.dup(target).ok_or(libc::EBADF)?.0)
        } else {
            None
        };
        // SYNCOBJ_DESTROY frees the number the moment it runs, and the next
        // import in this file gets it back: no watch may join a wait on the
        // old syncobj from here on (fence.rs, `Registrations::orphan`; S-13).
        // And an export or import of a syncobj file is where a syncobj gets
        // a holder other than its file (fence.rs, `before_ioctl2`).
        self.syncobj_regs
            .before_ioctl2(target, prepared.name(), prepared.buffer(0));
        Ok(PendingIoctl2(Pending::Ioctl2(Ioctl2Call {
            prepared,
            target_fd,
            target,
            owner: self.handles.owner(target),
            executor,
            generation: self.session.generation,
            req_id: self.current_req_id,
            cap,
            table_fd,
            t0,
            prep_ns: u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
        })))
    }

    /// Adopt what an executed IOCTL2 produced, close what it consumed, and
    /// build its response (see [`BackendFinisher`] for what is adopted how).
    /// If the session was reset while the call ran, nothing is adopted: the
    /// descriptors are closed and the guest is told the call was cancelled.
    pub fn finish_ioctl2(&mut self, p: PendingIoctl2) -> Reply {
        let call = match p.0 {
            Pending::Ioctl2(c) => c,
            Pending::Kms(k) => return self.finish_kms(k),
        };
        let Ioctl2Call {
            prepared,
            target_fd,
            target,
            owner,
            generation,
            req_id,
            cap,
            table_fd,
            t0,
            prep_ns,
            ..
        } = call;
        let (name, host_ns) = (prepared.name(), prepared.host_ns());
        let stale = generation != self.session.generation;
        // Done with the host file. The handle table still has its own --
        // unless a CLOSE raced the call, and then this is the last reference,
        // whose release can wait on a modeset (closer.rs, S-33): that one
        // goes to the closer thread. Otherwise it is closed here, which only
        // drops a reference: the table is not changing (the backend mutex is
        // held), the session is the one the call was prepared in, and the
        // handle still names the same unburied descriptor -- handles are not
        // issued twice in a session (handle_table.rs), so that is the file
        // the duplicate shares. The closer's queue and wakeup cost every
        // call on a render node a few microseconds of the queue thread.
        // An inline call has none: it ran on the table's own descriptor.
        if let Some(target_fd) = target_fd {
            if !stale
                && !self.handles.is_buried(target)
                && self.handles.get_raw(target).ok() == Some(table_fd)
            {
                drop(target_fd);
            } else {
                crate::closer::close(target_fd);
            }
        }
        crate::pacing::PACING.ioctl2(prepared.name(), prepared.result());
        // A lease a guest lessor revoked through us: whatever the lessee's
        // handle granted is gone on the host (kms.rs, "lease ends").
        let revoked_lease = prepared.name() == "REVOKE_LEASE" && prepared.result() == Some(0);
        // A destroyed syncobj only its file could reach: its wait
        // registrations go with it (fence.rs, `after_ioctl2`).
        if !stale && prepared.name() == "SYNCOBJ_DESTROY" {
            self.syncobj_regs.after_ioctl2(
                target,
                prepared.name(),
                prepared.buffer(0),
                prepared.result(),
            );
        }
        let nodes = self.host_nodes();
        let target_live = self.handles.kind(target).is_some() && !self.handles.is_buried(target);
        let mut fin = BackendFinisher {
            backend: self,
            cards: &nodes.cards,
            stale,
            created: Vec::new(),
            owner,
            target_live,
        };
        let body = prepared.finish_with(&mut fin);
        let created = fin.created;
        crate::pacing::PACING.ioctl2_timed(
            name,
            u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX),
            prep_ns,
            host_ns,
        );
        self.current_msg = MsgType::Ioctl2;
        self.current_req_id = req_id;
        if stale {
            log::info!("IOCTL2 on handle {target} finished after a session reset; discarded");
            return self.error_reply(libc::ECANCELED);
        }
        if HDR + body.len() > cap {
            // prepare_ioctl2 refused every call whose reply could exceed the
            // posted buffer, and response_len() bounds what finish builds, so
            // this is a bug in one of them. The call ran and consumed what it
            // was given; only what it made can still be taken back.
            log::error!(
                "IOCTL2 on handle {target}: a {}-byte reply does not fit the {cap}-byte \
                 buffer posted, though it was checked before running",
                HDR + body.len(),
            );
            self.close_handles(&created);
            return self.error_reply(libc::EMSGSIZE);
        }
        if revoked_lease {
            self.check_leases(None);
        }
        Reply {
            created,
            ..Reply::ok(MsgType::Ioctl2, target, req_id, &body)
        }
    }

    /// Adopt what an OPEN_KMS job opened, or answer a DROP_IF_MASTER, under
    /// the backend lock; after a session reset, close instead.
    fn finish_kms(&mut self, k: KmsCall) -> Reply {
        self.current_msg = MsgType::HostOp;
        self.current_req_id = k.req_id;
        let stale = k.generation != self.session.generation;
        let (res, created) = match k.op {
            KmsOp::DropIfMaster { fd, dropped, .. } => {
                crate::closer::close(fd);
                (dropped as u64, Vec::new())
            }
            KmsOp::Open {
                card, path, opened, ..
            } => match opened {
                Some(Ok(fd)) if stale => {
                    crate::closer::close(fd);
                    (0, Vec::new())
                }
                // Charged to the process that asked, not to whichever one
                // the queue thread served last.
                Some(Ok(fd)) => match self
                    .handles
                    .insert_for(fd, HandleKind::DrmCard(card), k.owner)
                    .map_err(|e| e.errno())
                {
                    Ok(h) => {
                        log::debug!("OPEN_KMS: {path} -> handle {h}");
                        (h as u64, vec![h])
                    }
                    Err(e) => return self.error_reply(e),
                },
                Some(Err(e)) => return self.error_reply(e),
                None => return self.error_reply(libc::EIO),
            },
        };
        if stale {
            log::info!("HOST_OP finished after a session reset; discarded");
            return self.error_reply(libc::ECANCELED);
        }
        Reply {
            created,
            ..self.ok_reply(0, &host_op_body(&[res], &[]))
        }
    }

    /// Close handles a reply created but could not deliver.
    pub fn close_handles(&mut self, handles: &[u32]) {
        for &h in handles {
            let _ = self.close_handle(h);
        }
    }

    /// Reset the session: close every handle, release every placement, drop
    /// every watch. The transport withdraws queued executor jobs when it sees
    /// the generation change.
    pub fn session_reset(&mut self, why: &str) {
        log::info!(
            "session reset ({why}): closing {} handle(s), {} placement(s)",
            self.handles.len(),
            self.live_maps.len() + self.active_maps.len()
        );
        self.session.generation += 1;
        self.session.v2 = false;
        self.session.proc_ids = false;
        self.session.proc_euid = false;
        self.session.proc_ids_noted = false;
        self.pump_cmds.push(PumpCmd::Reset);
        self.pump_cmds.push(PumpCmd::SetV2(false));
        self.release_all();
    }

    /// The current session generation.
    pub fn generation(&self) -> u64 {
        self.session.generation
    }

    /// Whether the session has said HELLO.
    pub fn is_v2(&self) -> bool {
        self.session.v2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfd::CardNode;
    use crate::pump::WatchMode;

    fn msg(t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
        let mut v = hdr(t, handle, 0, 0x55);
        v.extend_from_slice(body);
        v
    }

    fn status(resp: &[u8]) -> i32 {
        i32::from_le_bytes(resp[8..12].try_into().unwrap())
    }

    #[test]
    fn a_host_op_reply_is_its_results_then_its_tail() {
        let b = host_op_body(&[7, 9], &[1, 2, 3]);
        let r: HostOpResp = read(&b).unwrap();
        assert_eq!((r.nres, r.res), (2, [7, 9, 0, 0]));
        assert_eq!(&b[size_of::<HostOpResp>()..], &[1, 2, 3]);
        let e = Reply::error(MsgType::HostOp, 5, libc::EPERM);
        assert_eq!(e.bytes, hdr(MsgType::HostOp, 0, -libc::EPERM, 5));
        assert_eq!(status(&e.bytes), -libc::EPERM);
        let ok = Reply::ok(MsgType::HostOp, 3, 5, &b);
        assert_eq!(&ok.bytes[..HDR], &hdr(MsgType::HostOp, 3, 0, 5)[..]);
        assert_eq!(&ok.bytes[HDR..], &b[..]);
    }

    fn call(be: &mut NvidiaBackend, t: MsgType, handle: u32, body: &[u8]) -> Vec<u8> {
        let mut resp = vec![0u8; 4096];
        let n = be.dispatch(&msg(t, handle, body), &mut resp);
        resp.truncate(n);
        resp
    }

    fn hello(be: &mut NvidiaBackend, flags: u32) -> Vec<u8> {
        let req = HelloReq {
            proto: PROTO_V2,
            flags,
            guest_caps: 0,
            uvm_aperture_mib: 0,
        };
        call(be, MsgType::Hello, 0, bytes_of(&req))
    }

    fn devnull() -> OwnedFd {
        crate::sys::fd::open(c"/dev/null", libc::O_RDONLY | libc::O_CLOEXEC).unwrap()
    }

    fn backend() -> NvidiaBackend {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(
            Vec::new(),
            vec![CardNode {
                name: "card1".into(),
                major: 226,
                minor: 1,
                render_index: 0,
            }],
        );
        be
    }

    #[test]
    fn v2_messages_are_refused_until_hello_as_an_old_backend_would() {
        // (And a status is never one the guest could take for a result.)
        assert_eq!(wire_status(0), 0);
        assert_eq!(wire_status(-libc::EINVAL), -libc::EINVAL);
        for bad in [1, 4096 * 2, -4096, i32::MIN, -i32::MAX] {
            assert_eq!(wire_status(bad), -libc::EPROTO, "{bad}");
        }
        let mut be = backend();
        let r = call(&mut be, MsgType::TimeSync, 0, &[]);
        assert_eq!(status(&r), -libc::EPROTO);
        assert_eq!(status(&hello(&mut be, 0)), 0);
        let r = call(&mut be, MsgType::TimeSync, 0, &[]);
        assert_eq!(status(&r), 0);
        assert_ne!(
            u64::from_le_bytes(r[16..24].try_into().unwrap()),
            0,
            "stamped"
        );
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri's clocks start at zero")]
    fn time_sync_carries_realtime_and_raw_when_the_guest_has_room() {
        let mut be = backend();
        assert_eq!(status(&hello(&mut be, 0)), 0);
        let r = call(&mut be, MsgType::TimeSync, 0, &[]);
        assert_eq!(r.len(), HDR + size_of::<TimeSyncResp2>());
        let w = |i: usize| u64::from_le_bytes(r[HDR + 8 * i..HDR + 8 * i + 8].try_into().unwrap());
        let (mono, real, raw) = (w(0), w(1), w(2));
        assert!(mono != 0 && raw != 0);
        assert!(real > mono, "the epoch is further back than boot");
        assert_eq!(w(3), 0, "reserved");
    }

    #[test]
    fn time_sync_answers_an_old_guests_8_byte_buffer_in_8_bytes() {
        let mut be = backend();
        assert_eq!(status(&hello(&mut be, 0)), 0);
        let mut resp = vec![0u8; HDR + size_of::<TimeSyncResp>()];
        let n = be.dispatch(&msg(MsgType::TimeSync, 0, &[]), &mut resp);
        assert_eq!(n, HDR + size_of::<TimeSyncResp>());
        assert_eq!(status(&resp), 0);
        assert_ne!(
            u64::from_le_bytes(resp[HDR..HDR + 8].try_into().unwrap()),
            0
        );
    }

    #[test]
    fn hello_reports_the_configured_capabilities_and_limits() {
        let mut be = backend();
        be.config.kms_card = true;
        be.config.wayland_socket = Some("/run/user/1000/wayland-1".into());
        let r = hello(&mut be, HELLO_F_FRESH);
        assert_eq!(status(&r), 0);
        assert_eq!(
            u32::from_le_bytes(r[12..16].try_into().unwrap()),
            0x55,
            "req_id echoed"
        );
        let resp = read::<HelloResp>(&r[HDR..]).unwrap();
        assert_eq!(resp.proto, PROTO_V2);
        assert_eq!(
            resp.backend_caps,
            BCAP_KMS_CARD | BCAP_WAYLAND | BCAP_DEEP_SEGS
        );
        assert_eq!(resp.max_req, MAX_XFER_DIRECT);
        assert_eq!(resp.num_cards, 1);
        let bad = HelloReq {
            proto: 3,
            ..Default::default()
        };
        assert_eq!(
            status(&call(&mut be, MsgType::Hello, 0, bytes_of(&bad))),
            -libc::EPROTO
        );
    }

    struct NoVmm;
    impl crate::shm::WindowPlacer for NoVmm {
        fn place(&self, _: u64, _: u64, _: RawFd, _: u64, _: bool) -> crate::error::Result<()> {
            Ok(())
        }
        fn withdraw(&self, _: u64, _: u64) -> crate::error::Result<()> {
            Ok(())
        }
    }

    /// The aperture is offered only when the guest has one, the VMM's
    /// request channel is up, and the host's UVM takes sharing mode; and it
    /// is what the guest said, capped.
    #[test]
    fn the_uvm_aperture_is_offered_only_with_all_three_halves() {
        let caps = |be: &mut NvidiaBackend, gcap: u32, mib: u32| {
            let req = HelloReq {
                proto: PROTO_V2,
                flags: HELLO_F_FRESH,
                guest_caps: gcap,
                uvm_aperture_mib: mib,
            };
            let r = call(be, MsgType::Hello, 0, bytes_of(&req));
            assert_eq!(status(&r), 0);
            read::<HelloResp>(&r[HDR..]).unwrap().backend_caps & BCAP_UVM_MAP
        };
        let mut be = backend();
        be.set_host_driver_version("595.99.02");
        be.set_window(Box::new(NoVmm));
        assert_eq!(
            caps(&mut be, GCAP_UVM_APERTURE, 1024),
            0,
            "no --allow-compute"
        );
        assert_eq!(be.uvm_maps.aperture_len(), 0);
        let mut be = backend();
        be.set_host_driver_version("595.99.02");
        be.config_mut().allow_compute = true;
        assert_eq!(
            caps(&mut be, GCAP_UVM_APERTURE, 1024),
            0,
            "no request channel"
        );
        be.set_window(Box::new(NoVmm));
        assert_eq!(caps(&mut be, 0, 1024), 0, "the guest has none");
        assert_eq!(be.uvm_maps.aperture_len(), 0);
        assert_eq!(
            caps(&mut be, GCAP_UVM_APERTURE, 1),
            0,
            "smaller than a chunk"
        );
        assert_ne!(caps(&mut be, GCAP_UVM_APERTURE, 1024), 0);
        assert_eq!(be.uvm_maps.aperture_len(), 1 << 30);
        assert_ne!(caps(&mut be, GCAP_UVM_APERTURE, 64 << 10), 0);
        assert_eq!(be.uvm_maps.aperture_len(), crate::uvmmap::APERTURE_MAX);
        assert_ne!(caps(&mut be, GCAP_UVM_APERTURE, 7), 0);
        assert_eq!(be.uvm_maps.aperture_len(), 6 << 20);
        // A host whose UVM this backend has no table for.
        let mut be = backend();
        be.config_mut().allow_compute = true;
        be.set_window(Box::new(NoVmm));
        assert_eq!(
            caps(&mut be, GCAP_UVM_APERTURE, 1024),
            0,
            "no sharing mode known"
        );
    }

    /// `--allow-compute` off (the default): no UVM device opens on the
    /// host, and HELLO offers none of what only compute uses.
    #[test]
    fn compute_is_served_only_when_the_operator_allows_it() {
        let caps = |be: &mut NvidiaBackend| {
            let req = HelloReq {
                proto: PROTO_V2,
                flags: HELLO_F_FRESH,
                guest_caps: GCAP_UVM_APERTURE | GCAP_PROC_ID | GCAP_PROC_EUID,
                uvm_aperture_mib: 1024,
            };
            let r = call(be, MsgType::Hello, 0, bytes_of(&req));
            assert_eq!(status(&r), 0);
            read::<HelloResp>(&r[HDR..]).unwrap().backend_caps
        };
        let open = |be: &mut NvidiaBackend, device_type: u32| {
            let req = OpenReq {
                device_type,
                flags: 0,
            };
            call(be, MsgType::Open, 0, bytes_of(&req))
        };
        assert!(!BackendConfig::default().allow_compute, "off by default");
        let mut be = backend();
        be.set_host_driver_version("595.99.02");
        be.set_window(Box::new(NoVmm));
        let c = caps(&mut be);
        assert_eq!(c & (BCAP_COMPUTE | BCAP_UVM_MAP | BCAP_OS_DESC), 0);
        // What the RM isolation needs is not compute's.
        assert_eq!(
            c & (BCAP_PROC_ID | BCAP_PROC_EUID),
            BCAP_PROC_ID | BCAP_PROC_EUID
        );
        for dev in [DEV_UVM, DEV_UVM_TOOLS] {
            let r = open(&mut be, dev);
            assert_eq!(status(&r), -libc::ENODEV, "device {dev:#x}");
        }
        assert_eq!(be.handle_count(), 0, "nothing opened on the host");
        // Allowed: offered, and the open goes to the host (which in this
        // sandbox may or may not have a UVM device; either way the gate is
        // not what answers).
        be.config_mut().allow_compute = true;
        let c = caps(&mut be);
        assert_ne!(c & BCAP_COMPUTE, 0);
        assert_ne!(c & BCAP_UVM_MAP, 0);
        let r = open(&mut be, DEV_UVM_TOOLS);
        if status(&r) == 0 {
            let h = read::<MsgHeader>(&r).unwrap().handle;
            be.close_handle(h).unwrap();
        } else if !std::path::Path::new("/dev/nvidia-uvm-tools").exists() {
            assert_eq!(status(&r), -libc::ENOENT, "the host's own answer");
        }
    }

    #[test]
    fn a_fresh_hello_closes_every_handle_and_resets_the_pump() {
        let mut be = backend();
        let a = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let b = be.adopt_for_test(devnull(), HandleKind::SyncFile);
        hello(&mut be, 0);
        assert_eq!(be.handle_count(), 2, "a plain HELLO is not a reset");
        let gen0 = be.generation();
        be.take_pump_cmds();
        hello(&mut be, HELLO_F_FRESH);
        assert_eq!(be.handle_count(), 0);
        assert!(be.handles.kind(a).is_none() && be.handles.kind(b).is_none());
        assert_eq!(be.generation(), gen0 + 1);
        assert!(be.is_v2());
        let cmds = be.take_pump_cmds();
        assert!(matches!(cmds[0], PumpCmd::Reset));
        assert!(matches!(cmds.last(), Some(PumpCmd::SetV2(true))));
    }

    /// Armed readiness is offered only to a guest that arms, and an arm is
    /// taken only then and only on a device handle (the kind with a legacy
    /// watch); it reaches the pump as an Arm.
    #[test]
    fn an_arm_is_taken_only_from_a_guest_that_arms_and_only_on_a_device_handle() {
        let caps = |be: &mut NvidiaBackend, gcap: u32| {
            let req = HelloReq {
                proto: PROTO_V2,
                flags: HELLO_F_FRESH,
                guest_caps: gcap,
                uvm_aperture_mib: 0,
            };
            let r = call(be, MsgType::Hello, 0, bytes_of(&req));
            assert_eq!(status(&r), 0);
            read::<HelloResp>(&r[HDR..]).unwrap().backend_caps
        };
        let arm = |h| WatchReq {
            handle: h,
            flags: W_ARM,
            cookie: 0,
        };
        let mut be = backend();
        assert_eq!(caps(&mut be, 0) & BCAP_ARMED_READY, 0);
        let dev = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        assert_eq!(
            status(&call(&mut be, MsgType::Watch, 0, bytes_of(&arm(dev)))),
            -libc::EINVAL,
            "an arm from a guest that did not say it arms"
        );

        assert_ne!(caps(&mut be, GCAP_ARMS_READY) & BCAP_ARMED_READY, 0);
        be.take_pump_cmds();
        let dev = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let ev = be.adopt_for_test(hostfd::new_eventfd().unwrap(), HandleKind::Eventfd);
        // The modeset device's readiness is NVKMS's event queue, consumed
        // its own way (nvgpu_nvkms.c): never armed.
        let modeset = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Modeset));
        for h in [ev, modeset] {
            assert_eq!(
                status(&call(&mut be, MsgType::Watch, 0, bytes_of(&arm(h)))),
                -libc::EINVAL
            );
        }
        assert_eq!(
            status(&call(&mut be, MsgType::Watch, 0, bytes_of(&arm(999)))),
            -libc::EBADF
        );
        assert!(be.take_pump_cmds().is_empty());
        assert_eq!(
            status(&call(&mut be, MsgType::Watch, 0, bytes_of(&arm(dev)))),
            0
        );
        assert!(matches!(
            be.take_pump_cmds()[..],
            [PumpCmd::Arm { handle }] if handle == dev
        ));
    }

    #[test]
    fn watch_is_validated_against_the_handle_kind() {
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let ev = be.adopt_for_test(hostfd::new_eventfd().unwrap(), HandleKind::Eventfd);
        let other = be.adopt_for_test(devnull(), HandleKind::Other);
        let w = |h, flags| WatchReq {
            handle: h,
            flags,
            cookie: 0xabc,
        };
        be.take_pump_cmds();

        assert_eq!(
            status(&call(&mut be, MsgType::Watch, 0, bytes_of(&w(ev, W_DRM)))),
            -libc::EINVAL
        );
        assert_eq!(
            status(&call(
                &mut be,
                MsgType::Watch,
                0,
                bytes_of(&w(other, W_READY))
            )),
            -libc::EINVAL
        );
        assert_eq!(
            status(&call(
                &mut be,
                MsgType::Watch,
                0,
                bytes_of(&w(999, W_READY))
            )),
            -libc::EBADF
        );
        assert!(
            be.take_pump_cmds().is_empty(),
            "a refused watch reaches no pump"
        );

        assert_eq!(
            status(&call(
                &mut be,
                MsgType::Watch,
                0,
                bytes_of(&w(ev, W_READY | W_ONESHOT))
            )),
            0
        );
        let cmds = be.take_pump_cmds();
        assert!(matches!(
            cmds[..],
            [PumpCmd::Watch { handle, mode: WatchMode::Ready { cookie: 0xabc, oneshot: true, consume: true }, .. }]
                if handle == ev
        ));
        let u = UnwatchReq { handle: ev, pad: 0 };
        assert_eq!(status(&call(&mut be, MsgType::Unwatch, 0, bytes_of(&u))), 0);
        assert!(matches!(be.take_pump_cmds()[..], [PumpCmd::Unwatch { .. }]));
    }

    fn host_op(be: &mut NvidiaBackend, op: u32, args: &[u64]) -> (i32, HostOpResp) {
        let mut req = HostOpReq {
            op,
            nargs: args.len() as u32,
            args: [0; OP_MAX_ARGS],
        };
        req.args[..args.len()].copy_from_slice(args);
        let r = call(be, MsgType::HostOp, 0, bytes_of(&req));
        (
            status(&r),
            read::<HostOpResp>(&r[HDR..]).unwrap_or_default(),
        )
    }

    /// HOST_OP with the caller after it (quota.rs): one guest process
    /// runs out of handles at its share, another does not (B1).
    #[test]
    fn one_guest_process_cannot_take_every_handle_through_host_ops() {
        let mut be = backend();
        let req = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID,
            uvm_aperture_mib: 0,
        };
        call(&mut be, MsgType::Hello, 0, bytes_of(&req));
        be.handles.set_limit(256);
        let op = |be: &mut NvidiaBackend, tgid: u32| {
            let mut body = bytes_of(&HostOpReq {
                op: OP_NEW_EVENTFD,
                nargs: 0,
                args: [0; OP_MAX_ARGS],
            })
            .to_vec();
            body.extend_from_slice(bytes_of(&ProcId {
                start_ns: 1,
                tgid,
                euid: 0,
            }));
            let r = call(be, MsgType::HostOp, 0, &body);
            (
                status(&r),
                read::<HostOpResp>(&r[HDR..]).unwrap_or_default(),
            )
        };
        let mut made = 0;
        while op(&mut be, 10).0 == 0 {
            made += 1;
        }
        assert_eq!(made, 64, "a quarter of the table");
        assert_eq!(op(&mut be, 10).0, -libc::EMFILE);
        let (st, r) = op(&mut be, 11);
        assert_eq!(st, 0, "another process still gets one");
        let h = r.res[0] as u32;
        let b = crate::quota::Owner::Proc {
            tgid: 11,
            start_ns: 1,
        };
        assert_eq!(be.handles.owner(h), b);
        assert_eq!(be.handles.held_by(b), 1);
        // Closing gives the share back.
        call(&mut be, MsgType::Close, h, &[]);
        assert_eq!(be.handles.held_by(b), 0);
    }

    #[test]
    fn host_ops_make_eventfds_report_kinds_and_close_many() {
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let (st, r) = host_op(&mut be, OP_NEW_EVENTFD, &[]);
        assert_eq!(st, 0);
        let ev = r.res[0] as u32;
        assert_eq!(be.handles.kind(ev), Some(HandleKind::Eventfd));
        let (_, r) = host_op(&mut be, OP_FD_KIND, &[ev as u64]);
        assert_eq!(r.res[0], HK_EVENTFD as u64);
        // Wrong kinds never reach the host.
        assert_eq!(
            host_op(&mut be, OP_SYNC_MERGE, &[1, ev as u64]).0,
            -libc::EBADF
        );
        assert_eq!(
            host_op(&mut be, OP_PRIME_EXPORT, &[ev as u64, 1]).0,
            -libc::EBADF
        );
        assert_eq!(
            host_op(&mut be, OP_OPEN_KMS, &[ev as u64, 0]).0,
            -libc::EOPNOTSUPP
        );
        let (st, r) = host_op(&mut be, OP_CLOSE_MANY, &[2, ev as u64, 12345]);
        assert_eq!((st, r.res[0]), (0, 1));
        assert!(be.handles.kind(ev).is_none());
    }

    /// A dma-buf of a fence context would keep it alive -- a host kthread,
    /// a timer, an NVKMS duplicate -- after its GEM_CLOSE gave its slot back
    /// to the caps (semsurf.rs): the export is refused before the host is
    /// asked, and any other object of the file still goes.
    #[test]
    fn a_fence_context_is_never_exported() {
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        be.semsurf.render_opened(render, 0);
        be.semsurf.ctx_made_for_test(render, 7);
        assert_eq!(
            host_op(&mut be, OP_PRIME_EXPORT, &[render as u64, 7]).0,
            -libc::EINVAL
        );
        assert_eq!(be.semsurf.ctx_counts(render), (1, 1));
        // Another GEM of the file reaches the host (/dev/null: no ioctls).
        assert_eq!(
            host_op(&mut be, OP_PRIME_EXPORT, &[render as u64, 8]).0,
            -libc::ENOTTY
        );
    }

    /// A fence context over a semaphore surface that holds memory
    /// registered by its pages never reaches the host: NVKMS would be a
    /// holder of the pages the registration does not know. Another surface of the same client does.
    #[test]
    fn a_fence_context_over_registered_memory_is_refused() {
        const CLIENT: u32 = 0xc1d0_0001;
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        be.semsurf.render_opened(render, 0);
        be.semsurf.set_layout(
            0,
            crate::semsurf::Layout {
                stride: 32,
                max_submitted: 24,
            },
        );
        be.semsurf.client_allocated(render, CLIENT);
        be.osdesc.held_for_test(CLIENT, 0x5e5);
        let ctx = |surface: u32| {
            let mut block = [0u8; 16];
            block[0..4].copy_from_slice(&CLIENT.to_le_bytes());
            block[4..8].copy_from_slice(&surface.to_le_bytes());
            block[8..16].copy_from_slice(&4096u64.to_le_bytes());
            let mut arg = [0u8; 32];
            arg[8..16].copy_from_slice(&0x7000u64.to_le_bytes());
            arg[16..24].copy_from_slice(&16u64.to_le_bytes());
            let mut p = Vec::new();
            for v in [
                crate::semsurf::SEMSURF_FENCE_CTX_CREATE,
                0,
                2,
                0,
                0,
                0,
                48,
                render,
            ] {
                p.extend_from_slice(&v.to_le_bytes());
            }
            for n in [32u32, 16] {
                p.extend_from_slice(&n.to_le_bytes());
            }
            p.extend_from_slice(&arg);
            p.extend_from_slice(&block);
            p
        };
        be.current_handle = render;
        assert_eq!(
            be.prepare_ioctl2(&ctx(0x5e5), 4096).err(),
            Some(libc::EPERM)
        );
        assert!(be.prepare_ioctl2(&ctx(0x5e6), 4096).is_ok());
    }

    #[test]
    fn ioctl2_refuses_targets_without_a_schema_class_and_non_render_render_fields() {
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let sync = be.adopt_for_test(devnull(), HandleKind::SyncFile);
        let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
        let req = |render| Ioctl2Req {
            cmd: 0xC0406400,
            render,
            ..Default::default()
        };
        let r = call(&mut be, MsgType::Ioctl2, sync, bytes_of(&req(render)));
        assert_eq!(status(&r), -libc::EPERM);
        let r = call(&mut be, MsgType::Ioctl2, ctl, bytes_of(&req(render)));
        assert_eq!(status(&r), -libc::EPERM);
        let r = call(&mut be, MsgType::Ioctl2, render, bytes_of(&req(sync)));
        assert_eq!(status(&r), -libc::EBADF);
        let other_render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let r = call(
            &mut be,
            MsgType::Ioctl2,
            render,
            bytes_of(&req(other_render)),
        );
        assert_eq!(status(&r), -libc::EINVAL);
    }

    #[test]
    fn a_reply_that_would_not_be_delivered_closes_the_handles_it_made() {
        let mut be = backend();
        hello(&mut be, HELLO_F_FRESH);
        let mut req = HostOpReq {
            op: OP_NEW_EVENTFD,
            ..Default::default()
        };
        req.nargs = 0;
        let Outcome::Reply(r) = be.serve(&msg(MsgType::HostOp, 0, bytes_of(&req)), 4096) else {
            panic!("HOST_OP is answered inline")
        };
        assert_eq!(r.created.len(), 1);
        be.close_handles(&r.created);
        assert_eq!(be.handle_count(), 0);
    }

    fn kms_backend() -> NvidiaBackend {
        let mut be = backend();
        be.set_config(BackendConfig {
            kms_card: true,
            ..BackendConfig::default()
        });
        hello(&mut be, HELLO_F_FRESH);
        be
    }

    fn host_op_req(op: u32, args: &[u64]) -> Vec<u8> {
        let mut req = HostOpReq {
            op,
            nargs: args.len() as u32,
            args: [0; OP_MAX_ARGS],
        };
        req.args[..args.len()].copy_from_slice(args);
        msg(MsgType::HostOp, 0, bytes_of(&req))
    }

    /// S-34: both can wait on nvkms_lock or every modeset lock, so neither
    /// runs on the queue thread. DROP_IF_MASTER queues behind whatever its
    /// file already has; OPEN_KMS, with no file yet, in its card's queue.
    #[test]
    fn open_kms_and_drop_if_master_run_on_an_executor() {
        let mut be = kms_backend();
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let Outcome::Ioctl2(p) = be.serve(&host_op_req(OP_OPEN_KMS, &[render as u64, 0]), 4096)
        else {
            panic!("OPEN_KMS answered on the queue thread")
        };
        assert_eq!(p.executor_key(), Some(CARD_KEY));
        let r = p.cancelled_reply();
        assert_eq!(status(&r.bytes), -libc::ECANCELED);

        let card = be.adopt_for_test(devnull(), HandleKind::DrmCard(0));
        let Outcome::Ioctl2(mut p) =
            be.serve(&host_op_req(OP_DROP_IF_MASTER, &[card as u64]), 4096)
        else {
            panic!("DROP_IF_MASTER answered on the queue thread")
        };
        assert_eq!(p.executor_key(), Some(card as u64));
        p.execute();
        let r = be.finish_ioctl2(p);
        assert_eq!(status(&r.bytes), 0);
        let resp = read::<HostOpResp>(&r.bytes[HDR..]).unwrap();
        assert_eq!(
            (resp.nres, resp.res[0]),
            (1, 0),
            "/dev/null was never master"
        );
    }

    fn pipe_ends() -> (OwnedFd, OwnedFd) {
        crate::sys::fd::pipe2(libc::O_CLOEXEC).unwrap()
    }

    fn opened(be: &NvidiaBackend, fd: OwnedFd) -> PendingIoctl2 {
        PendingIoctl2(Pending::Kms(KmsCall {
            op: KmsOp::Open {
                card: 0,
                path: "/dev/dri/card1".into(),
                opened: Some(Ok(fd)),
            },
            generation: be.generation(),
            req_id: 7,
            owner: crate::quota::Owner::Unknown,
        }))
    }

    /// OPEN_KMS finishes on the queue thread after the executor opened the
    /// card, and other processes' messages are served meanwhile: the card
    /// file is the asking process's, against its share of the handle table.
    #[test]
    fn an_open_kms_is_charged_to_the_process_that_asked() {
        let mut be = backend();
        be.set_config(BackendConfig {
            kms_card: true,
            ..BackendConfig::default()
        });
        let req = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: GCAP_PROC_ID,
            uvm_aperture_mib: 0,
        };
        call(&mut be, MsgType::Hello, 0, bytes_of(&req));
        let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
        let proc = |tgid| crate::quota::Owner::Proc { tgid, start_ns: 1 };
        let from = |op: u32, args: &[u64], tgid: u32| {
            let mut r = HostOpReq {
                op,
                nargs: args.len() as u32,
                args: [0; OP_MAX_ARGS],
            };
            r.args[..args.len()].copy_from_slice(args);
            let mut body = bytes_of(&r).to_vec();
            body.extend_from_slice(bytes_of(&ProcId {
                start_ns: 1,
                tgid,
                euid: 0,
            }));
            msg(MsgType::HostOp, 0, &body)
        };
        let Outcome::Ioctl2(mut p) = be.serve(&from(OP_OPEN_KMS, &[render as u64, 0], 10), 4096)
        else {
            panic!("OPEN_KMS answered on the queue thread")
        };
        // The executor opened the card (a pipe here).
        let (r, _w) = pipe_ends();
        let Pending::Kms(k) = &mut p.0 else {
            panic!("an OPEN_KMS job")
        };
        let KmsOp::Open { opened, .. } = &mut k.op else {
            panic!("an OPEN_KMS job")
        };
        *opened = Some(Ok(r));
        // Another process is served meanwhile.
        let Outcome::Reply(_) = be.serve(&from(OP_NEW_EVENTFD, &[], 11), 4096) else {
            panic!("NEW_EVENTFD is answered inline")
        };
        let reply = be.finish_ioctl2(p);
        assert_eq!(status(&reply.bytes), 0);
        let h = read::<HostOpResp>(&reply.bytes[HDR..]).unwrap().res[0] as u32;
        assert_eq!(be.handles.kind(h), Some(HandleKind::DrmCard(0)));
        assert_eq!(be.handles.owner(h), proc(10));
        assert_eq!(be.handles.held_by(proc(11)), 1, "only its eventfd");
    }

    /// What OPEN_KMS opened is adopted only when the job finishes, and only
    /// into the session it was asked in: after a reset it is closed, and the
    /// guest hears the call was cancelled.
    #[test]
    fn an_open_kms_finishing_after_a_session_reset_is_closed_not_adopted() {
        let mut be = kms_backend();
        let (r, w) = pipe_ends();
        let p = opened(&be, r);
        be.session_reset("test");
        let reply = be.finish_ioctl2(p);
        assert_eq!(status(&reply.bytes), -libc::ECANCELED);
        assert!(reply.created.is_empty());
        assert_eq!(be.handle_count(), 0);
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        let n = crate::sys::fd::write(&w, b"x");
        assert!(
            n.is_err() || crate::testfd::only_end_here(w.as_fd()),
            "closed, not adopted"
        );

        let (r, _w) = pipe_ends();
        let p = opened(&be, r);
        let reply = be.finish_ioctl2(p);
        assert_eq!(status(&reply.bytes), 0);
        let h = read::<HostOpResp>(&reply.bytes[HDR..]).unwrap().res[0] as u32;
        assert_eq!(reply.created, vec![h]);
        assert_eq!(be.handles.kind(h), Some(HandleKind::DrmCard(0)));
    }
}
