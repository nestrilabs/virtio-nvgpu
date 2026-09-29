// SPDX-License-Identifier: Apache-2.0
//! IOCTL2: the schema-driven vectored ioctl (protocol v2).
//!
//! The guest sends an ioctl's argument and every buffer a pointer in it
//! reaches. This module is the authority on what that request may be: it
//! walks the backend's own schema for (class, cmd) over the bytes received,
//! recomputes every pointer field, length, descriptor field and GEM field, and
//! refuses anything that disagrees. Guest-supplied layout is never trusted.
//!
//! Three phases, so the host ioctl can run without the backend mutex:
//!  - `prepare` (under the mutex): parse, validate, build host buffers, `dup`
//!    every descriptor the call needs, run the policy hooks.
//!  - `Prepared::execute` (no mutex, possibly on an executor thread): property
//!    checks, GEM re-homing and validation, the host ioctl, GEM-out re-homing,
//!    closing temporaries. Everything it touches it owns.
//!  - `Prepared::finish_with` (under the mutex): adopt fd outs, close consumed
//!    handles, build the response.
//!
//! What the host kernel is handed is built here and nowhere else: every
//! pointer field the schema names points at a buffer of exactly the length
//! the schema computes from the bytes the kernel will read (or is NULL), every
//! descriptor field holds a descriptor this call dup'd from a handle of an
//! allowed kind (or the "none" value), every GEM field names an object in the
//! target file this job put there, and every descriptor-out field starts at -1
//! so that a value the kernel did not write is never mistaken for one it did.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;

use protocol::messages::{I2_DYN_OUT_FENCE, I2_FD_CONSUME, I2_MAX_BUFS, I2_MAX_RECS};

use crate::hostfd::HandleKind;
use crate::kms_state::{CONNECTOR_PROBE_EVERY, DRM_MODE_PROP_BLOB, FbUses, KmsFileState};
use crate::schema::{
    self, Dir, Exec, Field, Ioctl, Kind, Len, SchemaClass, Span, Special, Table, policy,
};
use crate::sys::block::{Arena, Arg, BufId, DataMut, Kernel, Restore, SlotKind};

/// Why a request was refused before reaching the host. Carried back to the
/// guest as a negative errno in the response header.
pub type Errno = i32;

// ───────────────────────────── host calls ─────────────────────────────

/// The system calls the interpreter makes on host descriptors. A trait so the
/// whole of `execute` -- re-homing, validation, the ioctl, adoption -- can be
/// tested against a fake kernel. The ioctl itself is `Kernel`'s, and takes
/// only an argument an arena built (sys/block.rs).
pub trait Sys: Kernel {
    /// Give back a descriptor the host made for this module that nothing
    /// keeps.
    fn close(&self, fd: OwnedFd);
    /// `lseek(fd, 0, SEEK_END)`, which is a dma-buf's size; or -errno.
    fn size_of(&self, fd: RawFd) -> i64;
}

/// The real thing.
pub struct HostSys;

impl Kernel for HostSys {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut Arg<'_>) -> i32 {
        // A signal is not the call's answer; libdrm's drmIoctl() retries the
        // same way. Nothing here waits long enough to need interrupting.
        crate::sys::ioctl::HostRetry.ioctl(fd, request, arg)
    }
}

impl Sys for HostSys {
    fn close(&self, fd: OwnedFd) {
        drop(fd);
    }

    fn size_of(&self, fd: RawFd) -> i64 {
        match crate::sys::fd::size(fd) {
            Ok(n) => n as i64,
            Err(e) => -(e.raw_os_error().unwrap_or(libc::EIO) as i64),
        }
    }
}

// ───────────────────────────── backend state ─────────────────────────────

/// What `prepare` needs from the backend's state.
pub trait Env {
    /// The descriptor and kind behind a handle, duplicated so the call owns it
    /// for as long as it needs (a racing CLOSE cannot pull it away).
    fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)>;
    /// The kind of a handle without duplicating it.
    fn kind(&self, handle: u32) -> Option<HandleKind>;
    /// The NVKMS schema for the host driver version, if one exists.
    fn nvkms_version(&self) -> Option<abi::version::DriverVersion>;
    /// The per-file KMS state of a `DrmCard`/`DrmLease` handle, kept by the
    /// backend from the handle's creation to its CLOSE. Without it every
    /// framebuffer counts as someone else's (GETFB returns no handles) and
    /// property names are looked up on every call.
    fn kms_state(&self, target: u32) -> Option<Arc<KmsFileState>> {
        let _ = target;
        None
    }
    /// The policy hooks (FENCES, NVKMS, KMS fill these in).
    fn hooks(&self) -> Arc<dyn Hooks> {
        Arc::new(DefaultHooks)
    }
    /// The system calls to make.
    fn sys(&self) -> Arc<dyn Sys> {
        Arc::new(HostSys)
    }
}

// ───────────────────────────── policy hooks ─────────────────────────────

/// What a DRM property's value is, by the property's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropKind {
    /// An integer, an object id or a blob id: nothing to translate.
    Plain,
    /// A descriptor in the caller's table (IN_FENCE_FD); -1 is none.
    FenceFd,
    /// A pointer the kernel writes an s32 descriptor through; 0 is none.
    OutPtr,
}

/// Properties whose value is a descriptor or a pointer: the core's
/// (drm_mode_config.c:305, 311; drm_writeback.c:134) and nvidia-drm's
/// (nvidia-drm-drv.c:546, a raw user pointer, nvidia-drm-crtc.c:1898).
pub fn default_prop_kind(name: &[u8]) -> PropKind {
    match name {
        b"IN_FENCE_FD" => PropKind::FenceFd,
        b"OUT_FENCE_PTR" | b"WRITEBACK_OUT_FENCE_PTR" | b"NV_DRM_OUT_FENCE_PTR" => PropKind::OutPtr,
        _ => PropKind::Plain,
    }
}

/// The decisions other workstreams own, called at fixed points of a call.
/// Every method has a default that refuses what its owner has not yet
/// implemented; the backend supplies one object overriding what it knows.
pub trait Hooks: Send + Sync {
    /// Classify a property by its name (NUL-trimmed).
    fn prop_kind(&self, name: &[u8]) -> PropKind {
        default_prop_kind(name)
    }

    /// An ATOMIC commit sets property `name` of `kind` to something other
    /// than "none". Returning Ok lets the call proceed *if* the guest also
    /// sent the matching record (an Ioctl2FdIn on that prop_values slot for a
    /// FenceFd, an Ioctl2Dyn OUT_FENCE for an OutPtr): the translation itself
    /// is done here, never by the hook. Workstream FENCES.
    fn atomic_fence_prop(&self, p: &Prepared, kind: PropKind, name: &[u8]) -> Result<(), Errno> {
        let _ = (p, kind, name);
        Err(libc::EOPNOTSUPP)
    }

    /// Before the call runs, under the backend mutex, for every entry with a
    /// policy (`Prepared::policy()`). May inspect and rewrite the request
    /// through `buffer_mut`. Must not make host ioctls.
    fn before(&self, p: &mut Prepared) -> Result<(), Errno> {
        default_before(p)
    }

    /// After the call ran, under the backend mutex, before the response is
    /// built: e.g. NVKMS grant records from an ACQUIRE_PERMISSIONS reply.
    fn after(&self, p: &mut Prepared, ret: i32) {
        let _ = (p, ret);
    }

    /// On the thread that runs the call, immediately before the host ioctl,
    /// for a call `before` gave a run gate (`Prepared::set_run_gate`): a
    /// decision made at prepare time that must still hold when the call
    /// runs, which may be long after, behind other calls in its executor's
    /// FIFO. Err refuses the call there; a guard returned is held until the
    /// ioctl has returned, so whatever takes that decision back waits for
    /// the call instead of racing it (S-14).
    fn at_run<'a>(&'a self, p: &Prepared) -> Result<Option<RunGuard<'a>>, Errno> {
        let _ = p;
        Ok(None)
    }

    /// Whether a re-home may export `gem` of guest file `file` at all:
    /// asked before the export; Err refuses the call. On the thread that
    /// runs the call.
    fn may_export(&self, file: u32, gem: u32) -> Result<(), Errno> {
        let _ = (file, gem);
        Ok(())
    }

    /// Whether a dma-buf a re-home has just exported may be imported
    /// elsewhere; false refuses the call (EINVAL). On the thread that runs
    /// the call.
    fn exportable(&self, dmabuf: std::os::fd::BorrowedFd<'_>) -> bool {
        let _ = dmabuf;
        true
    }
}

/// What `Hooks::at_run` holds across a host ioctl.
pub type RunGuard<'a> = std::sync::RwLockReadGuard<'a, ()>;

/// The hooks with nothing overridden.
pub struct DefaultHooks;

impl Hooks for DefaultHooks {}

/// What `Hooks::before` does unless overridden:
/// - fence schemas are refused until FENCES decides how each may run (waits
///   must not park a host thread, ARCHITECTURE.md, "Fences");
/// - GRANT_PERMISSIONS and REVOKE_PERMISSIONS only of type MODESET (2): a
///   SUB_OWNER grant blanks every head on the GPU and hands the whole device
///   to the grantee, and nvidia-drm never looks at the lease to scope it
///   (nvidia-drm-drv.c:1372-1417, 1533). NVKMS narrows the revocation to dpys
///   the same handle was granted;
/// - NVKMS commands pass, because only commands with a table entry get here.
pub fn default_before(p: &Prepared) -> Result<(), Errno> {
    let pol = p.policy();
    if pol & policy::FENCE != 0 {
        return Err(libc::EOPNOTSUPP);
    }
    const NV_DRM_PERMISSIONS_TYPE_MODESET: u64 = 2;
    // drm_nvidia_grant_permissions_params.type @8, revoke's @4.
    let type_at = if pol & policy::GRANT != 0 {
        Some(8)
    } else if pol & policy::REVOKE != 0 {
        Some(4)
    } else {
        None
    };
    if let Some(off) = type_at
        && rd(p.bytes(0), off, 4) != NV_DRM_PERMISSIONS_TYPE_MODESET
    {
        return Err(libc::EPERM);
    }
    Ok(())
}

/// What `finish_with` needs from the backend's state.
pub trait Finisher {
    /// Whether `fd` is a descriptor the backend already holds (in the handle
    /// table or privately). The kernel only writes fresh descriptors where
    /// the schema says, but an fd number is not something to take on trust:
    /// adopting one the backend holds would close it from under its owner.
    fn is_backend_fd(&self, fd: RawFd) -> bool;
    /// Insert a descriptor the host produced into the handle table.
    fn adopt(&mut self, fd: OwnedFd) -> (u32, HandleKind);
    /// Close a handle the guest passed with `I2_FD_CONSUME`.
    fn close_handle(&mut self, handle: u32);
    /// Whether what the call did may still be recorded: false once the
    /// file it ran on has closed, or the session it ran in is gone -- a
    /// record made then would outlive what it describes, or land in a
    /// session that knows nothing of it.
    fn records(&self) -> bool {
        true
    }
}

// ───────────────────────────── prepared call ─────────────────────────────

/// One buffer of the request: its host copy, a block of the call's arena.
struct HostBuf {
    id: BufId,
    dir: Dir,
}

/// A schema position the walk reached, with the guest's value there.
struct Slot {
    field: &'static Field,
    buf: usize,
    off: usize,
    guest: u64,
    /// PTR: the buffer it points at, if it got one.
    target: Option<usize>,
}

struct FdInRec {
    slot: Option<usize>,
    buf: usize,
    off: usize,
    handle: u32,
    fd: OwnedFd,
}

struct GemInRec {
    slot: usize,
    owner: u32,
    gem: u32,
}

/// A validated request ready to run.
pub struct Prepared {
    entry: &'static Ioctl,
    class: SchemaClass,
    target: u32,
    target_kind: HandleKind,
    /// Every block the host is handed: the argument, the buffers its
    /// pointers reach, the out-fence words and the probe scratch
    /// (sys/block.rs). Every schema field that is a pointer, a descriptor or
    /// a GEM handle is declared in it, so none holds a guest byte.
    arena: Arena,
    bufs: Vec<HostBuf>,
    slots: Vec<Slot>,
    fd_ins: Vec<FdInRec>,
    consumed: Vec<u32>,
    gem_ins: Vec<GemInRec>,
    owners: HashMap<u32, OwnedFd>,
    render_fd: Option<OwnedFd>,
    /// ATOMIC: IN_FENCE_FD records (in `fd_ins`, slot None) and OUT_FENCE
    /// offsets in prop_values, with the s32 the kernel writes once executed.
    fence_outs: Vec<(usize, Option<BufId>)>,
    kms: Option<Arc<KmsFileState>>,
    /// The framebuffers this call names as scanout sources, in use from
    /// their check to the end of the ioctl ([`VmKms`]).
    fb_uses: FbUses,
    /// A forced connector probe this call makes: (card, connector, when).
    probing: Option<(u32, u32, std::time::Instant)>,
    /// What the call did may be recorded (`Finisher::records`), for the
    /// hooks.
    records: bool,
    /// What `Hooks::before` asked `Hooks::at_run` to check again.
    run_gate: Option<u64>,
    /// `Hooks::before` answered the call itself (`answer_locally`): the
    /// host is not asked, and this is its return value.
    local: Option<i32>,
    hooks: Arc<dyn Hooks>,
    sys: Arc<dyn Sys>,
    ret: i32,
    executed: bool,
    /// Descriptors the host produced at schema positions, not yet adopted.
    fd_outs: Vec<(usize, usize, OwnedFd)>,
    /// (buf, off, gem in the render file, size) for the response.
    gem_outs: Vec<(usize, usize, u32, u64)>,
}

/// A call is prepared on the queue thread and may run on an executor thread
/// (session.rs, `PendingIoctl2`), so it must be `Send`. Everything in it is
/// owned outright -- an arena of guarded buffers (sys/block.rs),
/// descriptors, `Arc`s of `Send + Sync` state -- and this keeps it that way.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Prepared>();
};

/// Parse and validate an IOCTL2 payload (the bytes after the MsgHeader) for a
/// call on `target` (a handle of `target_kind`). The request's `render` field
/// is used, and checked to be a render handle (the target itself for a render
/// node call), only by calls that can create GEM handles.
pub fn prepare(
    env: &dyn Env,
    class: SchemaClass,
    target: u32,
    target_kind: HandleKind,
    payload: &[u8],
) -> Result<Prepared, Errno> {
    let req = Req::parse(payload)?;
    if !class.runs_on(target_kind) {
        return Err(libc::EINVAL);
    }
    let (table, entry) = find_entry(env, class, &req)?;

    let mut w = Walk {
        table,
        req: &req,
        cursor: 0,
        out_only: 0,
        arena: Arena::new(),
        bufs: Vec::new(),
        slots: Vec::new(),
    };
    w.arg(entry)?;
    if w.bufs.len() != req.nbuf || w.cursor != req.data.len() {
        return Err(libc::EINVAL);
    }

    let mut p = Prepared {
        entry,
        class,
        target,
        target_kind,
        arena: w.arena,
        bufs: w.bufs,
        slots: w.slots,
        fd_ins: Vec::new(),
        consumed: Vec::new(),
        gem_ins: Vec::new(),
        owners: HashMap::new(),
        render_fd: None,
        fence_outs: Vec::new(),
        kms: None,
        fb_uses: FbUses::default(),
        probing: None,
        records: true,
        run_gate: None,
        local: None,
        hooks: env.hooks(),
        sys: env.sys(),
        ret: -libc::ECANCELED,
        executed: false,
        fd_outs: Vec::new(),
        gem_outs: Vec::new(),
    };
    p.take_fd_ins(env, &req)?;
    p.take_gem_ins(env, &req)?;
    p.take_dyns(&req)?;
    p.check_planes()?;
    if p.slots_of(|k| matches!(k, Kind::GemOut)).next().is_some() {
        p.take_render(env, req.render)?;
    }
    if class == SchemaClass::Kms {
        p.kms = env.kms_state(target);
    }
    p.aim_pointers()?;
    if entry.policy != 0 {
        let hooks = p.hooks.clone();
        hooks.before(&mut p)?;
    }
    Ok(p)
}

impl Prepared {
    /// Whether the schema asks for this call to run on the target file's
    /// serial executor rather than inline on the queue thread. Every call on
    /// a KMS or modeset file does: almost all of them take modeset locks or
    /// `nvkms_lock`, and the queue thread must never wait on either.
    pub fn wants_executor(&self) -> bool {
        self.entry.exec == Exec::Executor || self.class != SchemaClass::Render
    }

    /// Run the host side: GEM-in re-homing into the target file and NVKMS-type
    /// validation (for framebuffer-creating calls), the host ioctl on
    /// `target_fd`, GEM-out re-homing into the render file, closing every
    /// temporary. Returns the host ioctl's result (0 or -errno).
    pub fn execute(&mut self, target_fd: RawFd) -> i32 {
        if !self.executed {
            self.executed = true;
            self.ret = match self.run(target_fd) {
                Ok(r) => r,
                Err(e) => -e,
            };
            self.fb_uses.release();
        }
        self.ret
    }

    /// Adopt descriptors the host produced at schema positions into the
    /// backend's handle table (`f`), refusing any number the backend already
    /// holds and closing `I2_FD_CONSUME` handles, then build the response
    /// payload (`Ioctl2Resp` onwards, without MsgHeader).
    pub fn finish_with(mut self, f: &mut dyn Finisher) -> Vec<u8> {
        let mut fd_recs = Vec::new();
        for (buf, off, fd) in std::mem::take(&mut self.fd_outs) {
            if f.is_backend_fd(fd.as_raw_fd()) {
                log::error!(
                    "IOCTL2 {}: the host left fd {} at a descriptor-out field, \
                     and the backend already holds it; not adopted",
                    self.entry.name,
                    fd.as_raw_fd()
                );
                // Not ours to close either: the number is the backend's.
                let _ = fd.into_raw_fd();
                continue;
            }
            // The kernel installed it for us for this call (a schema FD_OUT
            // position that started at -1, claimed from the arena), nothing
            // else holds it (checked above and in `take_fd_outs`).
            let (handle, kind) = f.adopt(fd);
            fd_recs.push([buf as u32, off as u32, handle, kind.wire()]);
        }
        // The hook first, while every handle the call named is still open:
        // a record it makes of one (an NVKMS grant file) is then dropped by
        // that handle's close like any other, rather than outliving it.
        let (hooks, ret) = (self.hooks.clone(), self.ret);
        self.records = f.records();
        hooks.after(&mut self, ret);
        for h in std::mem::take(&mut self.consumed) {
            f.close_handle(h);
        }
        self.response(&fd_recs)
    }

    /// Whether a hook's `after` may record what the call did
    /// (`Finisher::records`): its reply may still be rewritten either way.
    pub fn records(&self) -> bool {
        self.records
    }

    /// The host copy of buffer `i` (0 = the ioctl argument), for policy hooks
    /// that inspect a request before it runs or a reply after.
    pub fn buffer(&self, i: usize) -> Option<&[u8]> {
        self.bufs.get(i).map(|b| self.arena.bytes(b.id))
    }

    /// Mutable access for policy hooks that sanitise a request (e.g. clearing
    /// NVKMS override flags) before `execute`: data only -- a pointer,
    /// descriptor or GEM field is put back as it was when the view ends.
    pub fn buffer_mut(&mut self, i: usize) -> Option<DataMut<'_>> {
        let id = self.bufs.get(i)?.id;
        self.arena.data_mut(id)
    }

    /// What the host writes into buffer `i` at `off`, for a test standing in
    /// for it after the call.
    #[cfg(test)]
    pub(crate) fn host_writes(&mut self, i: usize, off: usize, bytes: &[u8]) {
        let id = self.bufs[i].id;
        self.arena.host_writes(id, off, bytes);
    }

    /// Buffer `i`'s bytes (empty for one that does not exist).
    fn bytes(&self, i: usize) -> &[u8] {
        self.bufs.get(i).map_or(&[], |b| self.arena.bytes(b.id))
    }

    /// The ioctl number being run.
    pub fn cmd(&self) -> u32 {
        self.entry.cmd
    }

    /// What the host call returned (non-negative, or -errno), once it ran.
    pub fn result(&self) -> Option<i32> {
        self.executed.then_some(self.ret)
    }

    /// The schema entry's name, for logs.
    pub fn name(&self) -> &'static str {
        self.entry.name
    }

    /// Ask `Hooks::at_run` to check `revocations` again when the call runs (see
    /// there). For `Hooks::before`.
    pub fn set_run_gate(&mut self, revocations: u64) {
        self.run_gate = Some(revocations);
    }

    /// What `set_run_gate` recorded.
    pub fn run_gate(&self) -> Option<u64> {
        self.run_gate
    }

    /// For `Hooks::before`: the call is answered without the host, with
    /// `ret` and the buffers as the hook leaves them (it writes the reply
    /// into them). Only for calls that make nothing -- no descriptor, no
    /// GEM handle -- since nothing of that kind would be there to adopt.
    pub fn answer_locally(&mut self, ret: i32) {
        debug_assert!(self.fd_outs.is_empty() && self.gem_outs.is_empty());
        self.local = Some(ret);
    }

    /// What `answer_locally` set.
    pub fn answered_locally(&self) -> Option<i32> {
        self.local
    }

    /// The `schema::policy` bits of the entry.
    pub fn policy(&self) -> u32 {
        self.entry.policy
    }

    /// Modeset calls: the NVKMS command.
    pub fn nvkms_cmd(&self) -> Option<u32> {
        (self.entry.special == Special::NvkmsParams).then_some(self.entry.nvkms_cmd)
    }

    /// The handle the call runs on.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// What kind of host file that handle is (a card and a lease take the
    /// same KMS schema, but not the same policy).
    pub fn target_kind(&self) -> HandleKind {
        self.target_kind
    }

    /// Handles standing in descriptor fields: (buffer, offset, handle).
    pub fn fd_in_handles(&self) -> impl Iterator<Item = (usize, usize, u32)> + '_ {
        self.fd_ins.iter().map(|r| (r.buf, r.off, r.handle))
    }

    /// The buffer the pointer at (`buf`, `off`) was given, if it got one
    /// (it was non-NULL with a non-zero length). For policy hooks that look
    /// past the argument, such as NVKMS FLIP's per-head array.
    pub fn pointee(&self, buf: usize, off: usize) -> Option<usize> {
        self.slots
            .iter()
            .find(|s| s.buf == buf && s.off == off && matches!(s.field.kind, Kind::Ptr { .. }))
            .and_then(|s| s.target)
    }

    /// The most the response `finish_with` builds can take (exact unless the host
    /// left fewer descriptors or GEM handles than the schema has room for),
    /// so the transport can refuse -EMSGSIZE before executing.
    pub fn response_len(&self) -> usize {
        let data: usize = self
            .bufs
            .iter()
            .filter(|b| b.dir.has_out())
            .map(|b| pad8(self.arena.len(b.id)))
            .sum();
        let fds =
            self.slots_of(|k| matches!(k, Kind::FdOut { .. })).count() + self.fence_outs.len();
        let gems = self.slots_of(|k| matches!(k, Kind::GemOut)).count();
        RESP_HDR + data + fds * FD_OUT_REC + gems * GEM_OUT_REC
    }
}

// ───────────────────────────── parsing ─────────────────────────────

const REQ_HDR: usize = 32;
const RESP_HDR: usize = 32;
const REC: usize = 16;
const FD_OUT_REC: usize = 16;
const GEM_OUT_REC: usize = 24;

struct Req<'a> {
    cmd: u32,
    nbuf: usize,
    render: u32,
    buf_len: Vec<u32>,
    fds: Vec<[u32; 4]>,
    gems: Vec<[u32; 4]>,
    dyns: Vec<[u32; 4]>,
    data: &'a [u8],
}

impl<'a> Req<'a> {
    fn parse(p: &'a [u8]) -> Result<Self, Errno> {
        if p.len() < REQ_HDR {
            return Err(libc::EINVAL);
        }
        let h = |i: usize| rd(p, i * 4, 4) as u32;
        let (cmd, flags, nbuf, nfd, ngem, ndyn, data_len, render) =
            (h(0), h(1), h(2), h(3), h(4), h(5), h(6), h(7));
        if flags != 0 || nbuf == 0 || nbuf > I2_MAX_BUFS {
            return Err(libc::EINVAL);
        }
        if nfd > I2_MAX_RECS || ngem > I2_MAX_RECS || ndyn > I2_MAX_RECS {
            return Err(libc::E2BIG);
        }
        let (nbuf, nfd, ngem, ndyn) = (nbuf as usize, nfd as usize, ngem as usize, ndyn as usize);
        let recs = REQ_HDR + 4 * nbuf;
        let data_at = recs + REC * (nfd + ngem + ndyn);
        if p.len() != data_at + data_len as usize {
            return Err(libc::EINVAL);
        }
        let quad = |at: usize| -> [u32; 4] { std::array::from_fn(|i| rd(p, at + 4 * i, 4) as u32) };
        let run = |first: usize, n: usize| -> Vec<[u32; 4]> {
            (0..n).map(|i| quad(recs + REC * (first + i))).collect()
        };
        Ok(Req {
            cmd,
            nbuf,
            render,
            buf_len: (0..nbuf)
                .map(|i| rd(p, REQ_HDR + 4 * i, 4) as u32)
                .collect(),
            fds: run(0, nfd),
            gems: run(nfd, ngem),
            dyns: run(nfd + ngem, ndyn),
            data: &p[data_at..],
        })
    }
}

/// The entry for a request: by (class, type, nr) from the version-independent
/// table, or for NVKMS by the command inside the outer struct from the table
/// of the host's driver version. A number we do not know is -ENOTTY; a number
/// we know with the wrong size or direction is -EINVAL.
fn find_entry(
    env: &dyn Env,
    class: SchemaClass,
    req: &Req,
) -> Result<(&'static Table, &'static Ioctl), Errno> {
    if class == SchemaClass::Modeset {
        if req.cmd != schema::NVKMS_IOCTL_IOWR {
            return Err(libc::ENOTTY);
        }
        let table = env
            .nvkms_version()
            .and_then(schema::modeset_table)
            .ok_or(libc::ENOTTY)?;
        // The command is read from the outer struct's IN bytes, which are
        // buffer 0's and the first thing in `data`.
        if req.data.len() < 4 {
            return Err(libc::EINVAL);
        }
        let entry = table
            .lookup_nvkms(rd(req.data, 0, 4) as u32)
            .ok_or(libc::ENOTTY)?;
        return Ok((table, entry));
    }
    let table = schema::DRM_TABLE;
    let entry = table
        .lookup(class.table_class(), req.cmd)
        .ok_or(libc::ENOTTY)?;
    if entry.cmd != req.cmd {
        return Err(libc::EINVAL);
    }
    Ok((table, entry))
}

// ───────────────────────────── the walk ─────────────────────────────

/// The canonical traversal (gen/schema/lang.py): buffer 0 is the argument;
/// then, depth first in field order, each non-NULL pointer of non-zero length
/// gets the next buffer, filled from `data` if the caller sends it, and its
/// elements are walked before the next field of its parent.
struct Walk<'a> {
    table: &'static Table,
    req: &'a Req<'a>,
    cursor: usize,
    /// Bytes of OUT-only buffers so far: allocated here, sent by nobody.
    out_only: usize,
    arena: Arena,
    bufs: Vec<HostBuf>,
    slots: Vec<Slot>,
}

/// The largest response any session negotiates (ARCHITECTURE.md, "Protocol
/// v2", with indirect
/// descriptors). IN bytes are bounded by the request that carries them; OUT-
/// only buffers are allocated on the guest's word alone, so their total is
/// bounded here, before the transport can compare `response_len()` with its
/// own, possibly smaller, limit.
const MAX_OUT_BYTES: usize = 4 << 20;

/// Deeper than any schema (the generator reports the deepest), shallow
/// enough that a table bug cannot recurse far.
const MAX_DEPTH: usize = 8;

impl Walk<'_> {
    fn arg(&mut self, e: &'static Ioctl) -> Result<(), Errno> {
        let dir = e.arg_dir().unwrap_or(Dir::In);
        self.new_buf(e.size as u64, dir)?;
        self.list(0, 0, e.fields, 0)
    }

    fn new_buf(&mut self, len: u64, dir: Dir) -> Result<usize, Errno> {
        let i = self.bufs.len();
        if i >= self.req.nbuf || u64::from(self.req.buf_len[i]) != len {
            return Err(libc::EINVAL);
        }
        let len = len as usize;
        // Checked before anything is allocated: IN bytes must be in the
        // request, OUT-only bytes within what any response may carry.
        let end = self.cursor + pad8(len);
        if dir.has_in() && end > self.req.data.len() {
            return Err(libc::EINVAL);
        }
        if !dir.has_in() {
            self.out_only += len;
            if self.out_only > MAX_OUT_BYTES {
                return Err(libc::E2BIG);
            }
        }
        let init = if dir.has_in() {
            let b = &self.req.data[self.cursor..self.cursor + len];
            self.cursor = end;
            b
        } else {
            &[]
        };
        let id = self.arena.block(init, len)?;
        self.bufs.push(HostBuf { id, dir });
        Ok(i)
    }

    fn list(&mut self, buf: usize, base: usize, span: Span, depth: usize) -> Result<(), Errno> {
        if depth > MAX_DEPTH {
            return Err(libc::EINVAL);
        }
        let fields = self.table.fields(span);
        let mut created: Vec<Option<usize>> = vec![None; fields.len()];
        for (i, f) in fields.iter().enumerate() {
            if let Some(c) = f.cond {
                let v = self.read(buf, base + c.off as usize, 4)? as u32;
                if !c.holds(v) {
                    continue;
                }
            }
            let at = base + f.off as usize;
            // Inside the struct by construction (schema_gen.py checks it);
            // checked again because a table bug must not read out of bounds.
            if at + f.width() as usize > self.arena.len(self.bufs[buf].id) {
                return Err(libc::EINVAL);
            }
            match f.kind {
                Kind::Ptr {
                    dir,
                    len,
                    max,
                    stride,
                    children,
                    ..
                } => {
                    // Taken out of the host's copy as it is found: from here
                    // the field holds 0 until the backend points it at a
                    // buffer of the call (`aim_pointers`), and the caller's
                    // value is only the reply's.
                    let id = self.bufs[buf].id;
                    let guest = self.arena.slot(id, at, 8, SlotKind::Ptr, Restore::Yes)?;
                    let n = self.length(buf, base, len, max, &created, span)?;
                    let slot = self.slots.len();
                    self.slots.push(Slot {
                        field: f,
                        buf,
                        off: at,
                        guest,
                        target: None,
                    });
                    if guest == 0 || n == 0 {
                        continue;
                    }
                    if n > u64::from(max) {
                        return Err(libc::E2BIG);
                    }
                    if children.len > 0 && (stride == 0 || n % u64::from(stride) != 0) {
                        return Err(libc::EINVAL);
                    }
                    let nb = self.new_buf(n, dir)?;
                    self.slots[slot].target = Some(nb);
                    created[i] = Some(nb);
                    if children.len > 0 {
                        for e in 0..(n / u64::from(stride)) as usize {
                            self.list(nb, e * stride as usize, children, depth + 1)?;
                        }
                    }
                }
                Kind::Array {
                    count,
                    stride,
                    limit,
                    children,
                } => {
                    // Only the elements the kernel reads: a descriptor field
                    // in one it never looks at is the caller's garbage, not
                    // a descriptor (REGISTER_SURFACE's unused planes).
                    let value = match limit {
                        schema::Limit::All => 0,
                        schema::Limit::Count { off, width } => {
                            self.read(buf, base + off as usize, width as usize)?
                        }
                        schema::Limit::Planes { off } => self.read(buf, base + off as usize, 4)?,
                    };
                    let n = limit.elements(count, value, self.table.planes);
                    for e in 0..n as usize {
                        self.list(buf, at + e * stride as usize, children, depth + 1)?;
                    }
                }
                _ => {
                    let width = f.width() as usize;
                    let id = self.bufs[buf].id;
                    // Descriptors and GEM handles are the backend's to fill
                    // (the caller's go back in the reply, or -1 for a
                    // descriptor, which only the guest can fill); anything
                    // else is data, read where it is.
                    let guest = match f.kind {
                        Kind::FdIn { .. } => {
                            self.arena
                                .slot(id, at, width, SlotKind::Fd, Restore::To(u64::MAX))?
                        }
                        Kind::FdOut { .. } => self.arena.slot(
                            id,
                            at,
                            width,
                            SlotKind::FdOut,
                            Restore::To(u64::MAX),
                        )?,
                        Kind::GemIn { .. } => {
                            self.arena
                                .slot(id, at, width, SlotKind::Value, Restore::Yes)?
                        }
                        Kind::GemOut => {
                            self.arena
                                .slot(id, at, width, SlotKind::Value, Restore::No)?
                        }
                        _ => self.read(buf, at, width)?,
                    };
                    self.slots.push(Slot {
                        field: f,
                        buf,
                        off: at,
                        guest,
                        target: None,
                    });
                }
            }
        }
        Ok(())
    }

    /// A pointer's length in bytes, from the IN bytes of its struct.
    fn length(
        &self,
        buf: usize,
        base: usize,
        len: Len,
        max: u32,
        created: &[Option<usize>],
        span: Span,
    ) -> Result<u64, Errno> {
        Ok(match len {
            Len::Const(n) => u64::from(n),
            Len::Count { off, width, elem } => self
                .read(buf, base + off as usize, width as usize)?
                .checked_mul(u64::from(elem))
                .ok_or(libc::E2BIG)?,
            Len::Sum { field, elem } => {
                let local = (field - span.first) as usize;
                let total = match created.get(local).copied().flatten() {
                    Some(b) => self
                        .arena
                        .bytes(self.bufs[b].id)
                        .chunks_exact(4)
                        .map(|c| u64::from(u32::from_le_bytes(c.try_into().unwrap())))
                        .sum::<u64>(),
                    None => 0,
                };
                total.checked_mul(u64::from(elem)).ok_or(libc::E2BIG)?
            }
            Len::NvkmsParams => {
                // NvKmsIoctlParams.size must be the command's params size
                // exactly (nvkms.c:5183 refuses anything else anyway).
                let n = self.read(buf, base + 4, 4)?;
                if n != u64::from(max) {
                    return Err(libc::EINVAL);
                }
                n
            }
        })
    }

    fn read(&self, buf: usize, off: usize, width: usize) -> Result<u64, Errno> {
        let b = self.arena.bytes(self.bufs[buf].id);
        if off + width > b.len() {
            return Err(libc::EINVAL);
        }
        Ok(rd(b, off, width))
    }
}

// ───────────────────────────── records ─────────────────────────────

impl Prepared {
    fn slots_of<'a>(
        &'a self,
        pick: impl Fn(&Kind) -> bool + 'a,
    ) -> impl Iterator<Item = usize> + 'a {
        (0..self.slots.len()).filter(move |&i| pick(&self.slots[i].field.kind))
    }

    fn slot_at(&self, buf: u32, off: u32, pick: impl Fn(&Kind) -> bool) -> Option<usize> {
        self.slots_of(pick)
            .find(|&i| self.slots[i].buf == buf as usize && self.slots[i].off == off as usize)
    }

    /// The ATOMIC prop_values buffer, whose slots may carry fence records.
    fn prop_values_buf(&self) -> Option<usize> {
        if self.entry.special != Special::Atomic {
            return None;
        }
        self.slots
            .iter()
            .find(|s| s.buf == 0 && s.field.name == "prop_values_ptr")
            .and_then(|s| s.target)
    }

    /// An 8-byte prop_values slot a fence record may name.
    fn fence_slot(&self, buf: u32, off: u32) -> bool {
        self.prop_values_buf() == Some(buf as usize)
            && off.is_multiple_of(8)
            && (off as usize) + 8 <= self.bytes(buf as usize).len()
    }

    /// Every descriptor field has exactly one record naming a handle of an
    /// allowed kind, or holds the "none" value; no record is anywhere else.
    fn take_fd_ins(&mut self, env: &dyn Env, req: &Req) -> Result<(), Errno> {
        let mut seen = HashSet::new();
        for &[buf, off, handle, flags] in &req.fds {
            if !seen.insert((buf, off)) || flags & !I2_FD_CONSUME != 0 {
                return Err(libc::EINVAL);
            }
            let slot = self.slot_at(buf, off, |k| matches!(k, Kind::FdIn { .. }));
            let kinds = match slot.map(|s| self.slots[s].field.kind) {
                Some(Kind::FdIn { kinds, .. }) => kinds,
                // IN_FENCE_FD values: only sync_files, and only on ATOMIC.
                _ if self.fence_slot(buf, off) => HandleKind::SyncFile.mask_bit(),
                _ => return Err(libc::EINVAL),
            };
            let (fd, kind) = env.dup_handle(handle).ok_or(libc::EBADF)?;
            if !schema::kind_allowed(kinds, kind) {
                return Err(libc::EINVAL);
            }
            if flags & I2_FD_CONSUME != 0 {
                self.consumed.push(handle);
            }
            self.fd_ins.push(FdInRec {
                slot,
                buf: buf as usize,
                off: off as usize,
                handle,
                fd,
            });
        }
        for i in self
            .slots_of(|k| matches!(k, Kind::FdIn { .. }))
            .collect::<Vec<_>>()
        {
            let Kind::FdIn { width, none, .. } = self.slots[i].field.kind else {
                unreachable!()
            };
            let (b, o) = (self.bufs[self.slots[i].buf].id, self.slots[i].off);
            match self.fd_ins.iter().find(|r| r.slot == Some(i)) {
                Some(r) => self.arena.set_fd(b, o, r.fd.as_fd())?,
                None if sext(self.slots[i].guest, width) == i64::from(none) => {
                    // The "none" value, as the kernel is to read it.
                    if none < 0 {
                        self.arena.set_no_fd(b, o, i64::from(none))?;
                    }
                }
                // A raw number would be looked up in the backend's own table.
                None => return Err(libc::EINVAL),
            }
        }
        Ok(())
    }

    /// Every non-zero GEM field has exactly one record, every zero one none.
    fn take_gem_ins(&mut self, env: &dyn Env, req: &Req) -> Result<(), Errno> {
        let mut seen = HashSet::new();
        for &[buf, off, owner, gem] in &req.gems {
            let slot = self
                .slot_at(buf, off, |k| matches!(k, Kind::GemIn { .. }))
                .ok_or(libc::EINVAL)?;
            if !seen.insert(slot) || gem == 0 || self.slots[slot].guest == 0 {
                return Err(libc::EINVAL);
            }
            if owner == self.target {
                // Guest objects never live in a KMS or modeset file: the only
                // GEM handles there are this job's temporaries, which is what
                // makes IDENTIFY-then-ADDFB race-free (RV:TOCTOU).
                if self.class != SchemaClass::Render {
                    return Err(libc::EINVAL);
                }
            } else if self.class == SchemaClass::Render {
                // A render-node call names objects of its own file only. A
                // temporary re-home here would be closed after the call, and
                // PRIME import into a file that already holds the object
                // returns that file's existing handle (drm_prime.c:306-309)
                // -- so the close would take the handle from under the guest
                // proxy that owns it. The guest moves such an object into the
                // target file itself, keeps it there, and names it by that
                // handle (SEMSURF_FENCE_ATTACH, driver/nvgpu_semsurf.c).
                return Err(libc::EINVAL);
            } else if let std::collections::hash_map::Entry::Vacant(slot) = self.owners.entry(owner)
            {
                let (fd, kind) = env.dup_handle(owner).ok_or(libc::EBADF)?;
                if !matches!(kind, HandleKind::DriRender(_)) {
                    return Err(libc::EINVAL);
                }
                slot.insert(fd);
            }
            self.gem_ins.push(GemInRec { slot, owner, gem });
        }
        let present = self.slots_of(|k| matches!(k, Kind::GemIn { .. }));
        let unbacked = present.filter(|&i| self.slots[i].guest != 0).count();
        if unbacked != self.gem_ins.len() {
            return Err(libc::EINVAL);
        }
        Ok(())
    }

    /// Dyn records: only ATOMIC's OUT_FENCE, each on its own prop_values slot.
    fn take_dyns(&mut self, req: &Req) -> Result<(), Errno> {
        for &[kind, buf, off, len] in &req.dyns {
            let taken = self.fence_outs.iter().any(|&(o, _)| o == off as usize)
                || self
                    .fd_ins
                    .iter()
                    .any(|r| r.slot.is_none() && r.off == off as usize);
            if kind != I2_DYN_OUT_FENCE || len != 4 || !self.fence_slot(buf, off) || taken {
                return Err(libc::EINVAL);
            }
            self.fence_outs.push((off as usize, None));
        }
        Ok(())
    }

    /// ADDFB2: nvidia-drm looks up a handle for every plane of the format and
    /// dereferences what it finds (nvidia-drm-fb.c:112-167), so every handle
    /// it will look at is one this job validates, and every other must be 0.
    fn check_planes(&self) -> Result<(), Errno> {
        if self.entry.policy & policy::FB_PLANES == 0 {
            return Ok(());
        }
        // drm_mode_fb_cmd2: pixel_format @12, handles[4] @20 -- as the guest
        // sent them: a handle field is the backend's to fill in the host's
        // copy.
        let planes = schema::format_planes(self.sent(0, 12, 4) as u32) as usize;
        if (planes..4).any(|i| self.sent(0, 20 + 4 * i, 4) != 0) {
            return Err(libc::EINVAL);
        }
        Ok(())
    }

    /// The `width`-byte field at `off` of buffer `buf` as the guest sent it:
    /// its value taken out of a declared field, or the data there.
    fn sent(&self, buf: usize, off: usize, width: usize) -> u64 {
        let id = self.bufs[buf].id;
        self.arena
            .guest(id, off)
            .unwrap_or_else(|| rd(self.arena.bytes(id), off, width))
    }

    fn take_render(&mut self, env: &dyn Env, render: u32) -> Result<(), Errno> {
        if self.class == SchemaClass::Render {
            // Handles a render-node call creates are already in the file that
            // will own their proxies.
            return if render == self.target {
                Ok(())
            } else {
                Err(libc::EINVAL)
            };
        }
        let (fd, kind) = env.dup_handle(render).ok_or(libc::EBADF)?;
        if !matches!(kind, HandleKind::DriRender(_)) {
            return Err(libc::EINVAL);
        }
        self.render_fd = Some(fd);
        Ok(())
    }

    /// Every pointer the schema names now points at our buffer or is NULL
    /// (it was declared, and so NULL, from the moment the walk found it);
    /// every descriptor-out field starts at -1 and every GEM-out field at 0
    /// (their declarations), so what is there afterwards is what the kernel
    /// wrote.
    fn aim_pointers(&mut self) -> Result<(), Errno> {
        for i in 0..self.slots.len() {
            if let (Kind::Ptr { .. }, Some(t)) = (self.slots[i].field.kind, self.slots[i].target) {
                let (b, o) = (self.bufs[self.slots[i].buf].id, self.slots[i].off);
                self.arena.point(b, o, self.bufs[t].id)?;
            }
        }
        Ok(())
    }
}

// ───────────────────────────── execution ─────────────────────────────

const DRM_IOCTL_MODE_GETPROPERTY: u32 = 0xc040_64aa;
use crate::hostfd::{
    DRM_IOCTL_GEM_CLOSE, DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT, DRM_IOCTL_PRIME_FD_TO_HANDLE,
    DRM_IOCTL_PRIME_HANDLE_TO_FD,
};
const DRM_CLOEXEC: u32 = libc::O_CLOEXEC as u32;
const NV_GEM_OBJECT_NVKMS: u64 = crate::hostfd::NV_GEM_OBJECT_NVKMS as u64;

impl Prepared {
    fn run(&mut self, target_fd: RawFd) -> Result<i32, Errno> {
        if let Some(ret) = self.local {
            return Ok(ret);
        }
        self.check_fb_sources()?;
        self.check_props(target_fd)?;
        let destroying = self.check_blob()?;
        // In the call's arena, which outlives the ioctl.
        self.limit_forced_probe(std::time::Instant::now())?;
        let hooks = self.hooks.clone();
        let gate = match self.run_gate {
            Some(_) => hooks.at_run(self)?,
            None => None,
        };
        let temps = self.gems_in(target_fd)?;
        let removing = self.forget_removed_fb();
        let sys = self.sys.clone();
        let top = self.bufs[0].id;
        let ret = self
            .arena
            .call(&*sys, target_fd, u64::from(self.entry.cmd), top);
        // The host has the framebuffers it was named (or refused them).
        self.fb_uses.release();
        if let (Some((card, connector, at)), Some(k)) = (self.probing, &self.kms)
            && ret < 0
        {
            k.vm().probe_refused(card, connector, at);
        }
        drop(gate);
        for h in temps {
            self.gem_close(target_fd, h);
        }
        if ret < 0 {
            // The host kept it (not this file's after all, or a bad id):
            // still ours, still usable.
            if let (Some(fb), Some(k)) = (removing, &self.kms) {
                k.add_fb(fb);
            }
            if let (Some(blob), Some(k)) = (destroying, &self.kms) {
                k.add_blob(blob);
            }
            return Ok(ret);
        }
        self.track_fbs(target_fd);
        self.track_blobs(target_fd);
        self.take_fd_outs(target_fd);
        if let Err(e) = self.gems_out(target_fd) {
            for (_, _, fd) in self.fd_outs.drain(..) {
                self.sys.close(fd);
            }
            return Err(e);
        }
        Ok(ret)
    }

    /// The framebuffer id a legacy scanout call names, and where: SETCRTC's
    /// drm_mode_crtc.fb_id, SETPLANE's drm_mode_set_plane.fb_id, and
    /// PAGE_FLIP's drm_mode_crtc_page_flip(_target).fb_id (the same entry:
    /// PAGE_FLIP_TARGET is a flag, not an ioctl). CURSOR names a GEM handle,
    /// which the GEM_IN rules already cover.
    fn fb_source_at(name: &str) -> Option<usize> {
        match name {
            "SETCRTC" => Some(16),
            "SETPLANE" => Some(8),
            "PAGE_FLIP" => Some(4),
            _ => None,
        }
    }

    /// Whether `value` of a property named `name` names a framebuffer:
    /// FB_ID, the scanout source (drm_mode_config.c:229), and
    /// WRITEBACK_FB_ID, which the kernel would write *into*
    /// (drm_writeback.c:260).
    fn is_fb_prop(name: &[u8]) -> bool {
        matches!(name, b"FB_ID" | b"WRITEBACK_FB_ID")
    }

    /// Every framebuffer this call names as a scanout (or writeback) source
    /// must be none or one this VM made (S-6, see [`KmsFileState`]); EPERM
    /// otherwise, before the host sees the call. The kernel reads the id as
    /// a u32 whatever width it travels in (drm_framebuffer_lookup), so that
    /// is what is checked.
    /// Whether `value` may be set on a property with `flags`: anything for
    /// a property that is no blob; for a blob property (MODE_ID, GAMMA_LUT,
    /// CTM, damage clips) none, or a blob this VM made or sees. Blob ids
    /// are the device's, and a commit of another tenant's id would make
    /// OBJ_GETPROPERTIES report it and GETPROPBLOB read it -- and a
    /// TEST_ONLY commit alone says whether it exists and how large it is.
    fn blob_usable(&self, flags: u32, value: u64) -> bool {
        flags & DRM_MODE_PROP_BLOB == 0
            || value == 0
            || u32::try_from(value)
                .is_ok_and(|id| self.kms.as_ref().is_some_and(|k| k.vm().blob_readable(id)))
    }

    fn fb_usable(&mut self, id: u32) -> bool {
        match &self.kms {
            Some(k) => k.claim_scan_out(id, &mut self.fb_uses),
            None => id == 0,
        }
    }

    /// The legacy scanout calls' fb_id fields. SETCRTC's -1 keeps the CRTC's
    /// current framebuffer (drm_crtc.c:760), which is one the call's own
    /// CRTC already shows.
    fn check_fb_sources(&mut self) -> Result<(), Errno> {
        let Some(off) = Self::fb_source_at(self.entry.name) else {
            return Ok(());
        };
        let fb = rd(self.bytes(0), off, 4) as u32;
        if (self.entry.name == "SETCRTC" && fb == u32::MAX) || self.fb_usable(fb) {
            return Ok(());
        }
        log::warn!(
            "{} on handle {} names framebuffer {fb}, which no file of this VM made; refused",
            self.entry.name,
            self.target
        );
        Err(libc::EPERM)
    }

    /// GETCONNECTOR with count_modes 0 from a (lessee) master is a forced
    /// probe: under mode_config.mutex, fill_modes (drm_connector.c:3373-3377)
    /// -> nv_drm_connector_detect -> QUERY_DPY_DYNAMIC_DATA through the kapi
    /// (nvidia-drm-connector.c:165, nvkms-kapi.c:1527), a fresh EDID read
    /// under nvkms_lock, which the host compositor's flips wait on (S-8).
    /// Once per [`CONNECTOR_PROBE_EVERY`] per connector per VM it goes as
    /// sent; in between, count_modes becomes 1 with a one-mode buffer of our
    /// own behind modes_ptr: then the kernel only reports the modes of the
    /// last probe (3374: no fill_modes), copies them only if exactly one
    /// fits (into that buffer, which nobody reads), and writes the real
    /// count back, which is all a count_modes 0 caller gets anyway. The
    /// guest's own modes_ptr (NULL: it asked for no modes) goes back in
    /// place as for every pointer. The buffer is the call arena's, and
    /// outlives the call.
    fn limit_forced_probe(&mut self, now: std::time::Instant) -> Result<(), Errno> {
        const COUNT_MODES: usize = 32;
        const MODES_PTR: usize = 8;
        const CONNECTOR_ID: usize = 48;
        const MODEINFO: usize = 68;
        if self.entry.name != "GETCONNECTOR" {
            return Ok(());
        }
        let card = match self.target_kind {
            HandleKind::DrmCard(c) | HandleKind::DrmLease(c) => c,
            _ => return Ok(()),
        };
        let arg = self.bytes(0);
        if rd(arg, COUNT_MODES, 4) != 0 {
            return Ok(());
        }
        let connector = rd(arg, CONNECTOR_ID, 4) as u32;
        let Some(k) = &self.kms else {
            return Ok(());
        };
        if k.vm().may_probe(card, connector, now) {
            self.probing = Some((card, connector, now));
            return Ok(());
        }
        let scratch = self.arena.block(&[], MODEINFO)?;
        let top = self.bufs[0].id;
        self.arena.write(top, COUNT_MODES, &1u32.to_le_bytes())?;
        // modes_ptr is a schema pointer, declared by the walk.
        self.arena.point(top, MODES_PTR, scratch)?;
        log::debug!(
            "GETCONNECTOR {connector} on handle {}: probed less than {:?} ago; reported, not probed",
            self.target,
            CONNECTOR_PROBE_EVERY
        );
        Ok(())
    }

    /// RMFB/CLOSEFB: the record goes before the call, so no scanout call on
    /// another executor can find it between the host freeing the id and our
    /// forgetting it (and meanwhile be handed someone else's framebuffer
    /// under the same number). Returns the id if it was this file's.
    fn forget_removed_fb(&self) -> Option<u32> {
        if self.entry.policy & policy::FB_REMOVE == 0 {
            return None;
        }
        // fb_id @0 of RMFB's u32 and drm_mode_closefb.
        let fb = rd(self.bytes(0), 0, 4) as u32;
        self.kms.as_ref().filter(|k| k.take_fb(fb)).map(|_| fb)
    }

    /// Fence and pointer properties (RV:setprop). Their values are a
    /// descriptor the kernel resolves in *our* table (drm_atomic_uapi.c:558)
    /// or a pointer it writes through in *our* address space (:473), so on
    /// the legacy setters they are refused outright, and on ATOMIC each one
    /// that is not "none" needs the hook's consent and a record that says
    /// what to put there.
    fn check_props(&mut self, fd: RawFd) -> Result<(), Errno> {
        if self.entry.policy & policy::SETPROP != 0 {
            // drm_mode_connector_set_property / drm_mode_obj_set_property:
            // prop_id @8 in both.
            let id = rd(self.bytes(0), 8, 4) as u32;
            let (name, flags) = self.prop_info(fd, id)?;
            if self.hooks.prop_kind(trim(&name)) != PropKind::Plain {
                return Err(libc::EINVAL);
            }
            // value @0: the same framebuffer rule as ATOMIC's, or the
            // legacy setter is the way round it (S-6).
            if Self::is_fb_prop(trim(&name)) && !self.fb_usable(rd(self.bytes(0), 0, 4) as u32) {
                return Err(libc::EPERM);
            }
            if !self.blob_usable(flags, rd(self.bytes(0), 0, 8)) {
                return Err(libc::EPERM);
            }
        }
        let Some(values) = self.prop_values_buf() else {
            return Ok(());
        };
        let props = self
            .slots
            .iter()
            .find(|s| s.buf == 0 && s.field.name == "props_ptr")
            .and_then(|s| s.target)
            .ok_or(libc::EINVAL)?;
        let ids: Vec<u32> = self
            .bytes(props)
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        for (i, id) in ids.into_iter().enumerate() {
            let off = 8 * i;
            let value = rd(self.bytes(values), off, 8);
            let fd_rec = self
                .fd_ins
                .iter()
                .position(|r| r.slot.is_none() && r.off == off);
            let out_rec = self.fence_outs.iter().position(|&(o, _)| o == off);
            let (name, flags) = self.prop_info(fd, id)?;
            if !self.blob_usable(flags, value) {
                log::warn!(
                    "ATOMIC on handle {} sets {} to blob {value}, which no file of this VM \
                     made or sees; refused",
                    self.target,
                    String::from_utf8_lossy(trim(&name)),
                );
                return Err(libc::EPERM);
            }
            if Self::is_fb_prop(trim(&name)) && !self.fb_usable(value as u32) {
                log::warn!(
                    "ATOMIC on handle {} sets {} to framebuffer {}, which no file of this VM \
                     made; refused",
                    self.target,
                    String::from_utf8_lossy(trim(&name)),
                    value as u32
                );
                return Err(libc::EPERM);
            }
            let kind = self.hooks.prop_kind(trim(&name));
            let none = match kind {
                PropKind::Plain => true,
                PropKind::FenceFd => value as i64 == -1,
                PropKind::OutPtr => value == 0,
            };
            let has_rec = fd_rec.is_some() || out_rec.is_some();
            if none && !has_rec {
                continue;
            }
            if kind != PropKind::Plain {
                let hooks = self.hooks.clone();
                hooks.atomic_fence_prop(self, kind, trim(&name))?;
            }
            // The value becomes a field of the backend's: a descriptor of
            // its own, or the address of an s32 of the call's.
            let vid = self.bufs[values].id;
            match (kind, fd_rec, out_rec) {
                (PropKind::FenceFd, Some(r), None) => {
                    self.arena.fd(vid, off, 8)?;
                    self.arena.set_fd(vid, off, self.fd_ins[r].fd.as_fd())?;
                }
                (PropKind::OutPtr, None, Some(r)) => {
                    // The kernel writes -1 here when the property is set
                    // (drm_atomic_uapi.c:473) and the sync_file's number once
                    // it is installed (1404); ours until finish adopts it.
                    let s32 = self.arena.block(&[], 4)?;
                    self.arena.fd_out(s32, 0, 4)?;
                    self.arena.ptr(vid, off)?;
                    self.arena.point(vid, off, s32)?;
                    self.fence_outs[r].1 = Some(s32);
                }
                // A value with no record, or a record of the wrong kind.
                _ => return Err(libc::EINVAL),
            }
        }
        Ok(())
    }

    /// A property's name and flags, by GETPROPERTY on the target file with
    /// every count zero (drm_property.c:458: then neither pointer is
    /// written). Property ids are device-global and fixed for the device's
    /// life, so the answer is cached for the file. A property the host does
    /// not know is refused: the host would refuse the call anyway, and
    /// "unknown" is not "plain".
    fn prop_info(&self, fd: RawFd, id: u32) -> Result<([u8; 32], u32), Errno> {
        if let Some(n) = self
            .kms
            .as_ref()
            .and_then(|k| k.lock().prop_names.get(&id).copied())
        {
            return Ok(n);
        }
        let mut arg = [0u8; 64];
        wr(&mut arg, 16, 4, u64::from(id));
        // values_ptr and enum_blob_ptr: declared, and so NULL.
        let mut a = Arena::new();
        let top = a.small(&arg);
        a.ptr(top, 0)?;
        a.ptr(top, 8)?;
        let r = a.call(&*self.sys, fd, u64::from(DRM_IOCTL_MODE_GETPROPERTY), top);
        if r < 0 {
            return Err(-r);
        }
        let name: [u8; 32] = a.bytes(top)[24..56].try_into().unwrap();
        let flags = rd(a.bytes(top), 20, 4) as u32;
        if let Some(k) = &self.kms {
            k.lock().prop_names.insert(id, (name, flags));
        }
        Ok((name, flags))
    }

    /// GETPROPBLOB may read only a blob this VM made or can see
    /// ([`VmKms::blob_readable`]); ENOENT, the host's own answer for a blob
    /// that is not there, otherwise. DESTROYPROPBLOB takes the blob's
    /// records before it runs.
    fn check_blob(&self) -> Result<Option<u32>, Errno> {
        let id = || rd(self.bytes(0), 0, 4) as u32;
        match self.entry.name {
            "GETPROPBLOB" => {
                let id = id();
                if self.kms.as_ref().is_some_and(|k| k.vm().blob_readable(id)) {
                    return Ok(None);
                }
                log::warn!(
                    "GETPROPBLOB on handle {} of blob {id}, which no file of this VM made or \
                     can see; refused",
                    self.target
                );
                Err(libc::ENOENT)
            }
            "DESTROYPROPBLOB" => {
                let id = id();
                Ok(self.kms.as_ref().filter(|k| k.take_blob(id)).map(|_| id))
            }
            _ => Ok(None),
        }
    }

    /// After a successful call: the blob CREATEPROPBLOB made, and the blob
    /// properties OBJ_GETPROPERTIES or GETCONNECTOR reported.
    fn track_blobs(&mut self, target_fd: RawFd) {
        let Some(k) = self.kms.clone() else { return };
        // (count_props, object id, props_ptr, prop_values_ptr).
        let (count, obj, props, values) = match self.entry.name {
            "CREATEPROPBLOB" => {
                // drm_mode_create_blob.blob_id @12.
                k.add_blob(rd(self.bytes(0), 12, 4) as u32);
                return;
            }
            // drm_mode_obj_get_properties.
            "OBJ_GETPROPERTIES" => (16, 20, 0, 8),
            // drm_mode_get_connector.
            "GETCONNECTOR" => (36, 48, 16, 24),
            _ => return,
        };
        let arg = self.bytes(0);
        let (n, obj) = (rd(arg, count, 4) as usize, rd(arg, obj, 4) as u32);
        let (Some(pb), Some(vb)) = (self.pointee(0, props), self.pointee(0, values)) else {
            return;
        };
        let (ids, vals) = (self.bytes(pb).to_vec(), self.bytes(vb).to_vec());
        // As many as the host wrote: the count it returns, as far as both
        // arrays reach.
        let n = n.min(ids.len() / 4).min(vals.len() / 8);
        let mut blobs = Vec::new();
        for i in 0..n {
            let prop = rd(&ids, 4 * i, 4) as u32;
            match self.prop_info(target_fd, prop) {
                Ok((_, flags)) if flags & DRM_MODE_PROP_BLOB != 0 => {
                    blobs.push((prop, rd(&vals, 8 * i, 8) as u32));
                }
                _ => {}
            }
        }
        k.saw_blobs(obj, &blobs);
    }

    /// Put every GEM_IN object into the target file, validate it, and write
    /// its handle there. Returns the temporaries to close after the call.
    ///
    /// A guest proxy's object lives in its owner's render file; the call's
    /// file is another one. PRIME export from the owner and import into the
    /// target gives a handle to the same object (both nodes are one
    /// drm_device, drm_prime.c:292), which the call holds only for this job:
    /// the framebuffer or cursor it makes takes its own references
    /// (nvidia-drm-fb.c:113), so the handle is closed right after. Nothing is
    /// cached, so nothing can go stale (RV:rehome).
    fn gems_in(&mut self, target_fd: RawFd) -> Result<Vec<u32>, Errno> {
        let mut temps = Vec::new();
        match self.home_gems(target_fd, &mut temps) {
            Ok(()) => Ok(temps),
            Err(e) => {
                for h in temps {
                    self.gem_close(target_fd, h);
                }
                Err(e)
            }
        }
    }

    /// `gems_in`'s work: each handle it imports is in `temps` as soon as it
    /// exists, for the caller to close however this ends.
    fn home_gems(&mut self, target_fd: RawFd, temps: &mut Vec<u32>) -> Result<(), Errno> {
        let mut homed: HashMap<(u32, u32), u32> = HashMap::new();
        for i in 0..self.gem_ins.len() {
            let GemInRec { slot, owner, gem } = self.gem_ins[i];
            let h = if owner == self.target {
                gem
            } else if let Some(&h) = homed.get(&(owner, gem)) {
                h
            } else {
                let owner_fd = self.owners[&owner].as_raw_fd();
                let dmabuf = self.prime_export(owner, owner_fd, gem)?;
                let imported = self.prime_import(target_fd, dmabuf.as_raw_fd());
                self.sys.close(dmabuf);
                let h = imported?;
                temps.push(h);
                homed.insert((owner, gem), h);
                h
            };
            if let Kind::GemIn {
                validate_nvkms: true,
            } = self.slots[slot].field.kind
            {
                // In the same job as the ioctl, on a handle only this job
                // knows: nothing can swap the object in between.
                if self.identify(target_fd, h)? != NV_GEM_OBJECT_NVKMS {
                    return Err(libc::EINVAL);
                }
            }
            let (b, o) = (self.bufs[self.slots[slot].buf].id, self.slots[slot].off);
            self.arena.set_value(b, o, u64::from(h))?;
        }
        Ok(())
    }

    /// Framebuffer ownership, and GETFB's answer to a framebuffer that is not
    /// this file's: no handle, and the one the host made closed at once.
    fn track_fbs(&mut self, target_fd: RawFd) {
        let pol = self.entry.policy;
        if pol & (policy::FB_CREATE | policy::FB_REMOVE | policy::FB_READ) == 0 {
            return;
        }
        // fb_id @0 of drm_mode_fb_cmd, drm_mode_fb_cmd2, RMFB's u32, closefb.
        // A removal was recorded before the call (`forget_removed_fb`).
        let fb = rd(self.bytes(0), 0, 4) as u32;
        if let Some(k) = &self.kms
            && pol & policy::FB_CREATE != 0
        {
            k.add_fb(fb);
        }
        if pol & policy::FB_READ != 0 && !self.kms.as_ref().is_some_and(|k| k.owns_fb(fb)) {
            let mut closed = HashSet::new();
            for i in self
                .slots_of(|k| matches!(k, Kind::GemOut))
                .collect::<Vec<_>>()
            {
                let (b, o) = (self.bufs[self.slots[i].buf].id, self.slots[i].off);
                let h = rd(self.arena.bytes(b), o, 4) as u32;
                if h != 0 && closed.insert(h) {
                    self.gem_close(target_fd, h);
                }
                let _ = self.arena.set_value(b, o, 0);
            }
        }
    }

    /// Descriptors the host produced: only at schema positions (or the
    /// ATOMIC out-fence slots this call made), only values >= 0, and never a
    /// descriptor this call already holds.
    fn take_fd_outs(&mut self, target_fd: RawFd) {
        let mut found = Vec::new();
        for i in self
            .slots_of(|k| matches!(k, Kind::FdOut { .. }))
            .collect::<Vec<_>>()
        {
            let Kind::FdOut { width } = self.slots[i].field.kind else {
                unreachable!()
            };
            let (b, o) = (self.slots[i].buf, self.slots[i].off);
            let id = self.bufs[b].id;
            found.push((
                b,
                o,
                (id, o),
                sext(rd(self.arena.bytes(id), o, width as usize), width),
            ));
        }
        if let Some(values) = self.prop_values_buf() {
            for (off, s32) in &self.fence_outs {
                if let Some(s) = *s32 {
                    found.push((
                        values,
                        *off,
                        (s, 0),
                        i64::from(rd(self.arena.bytes(s), 0, 4) as u32 as i32),
                    ));
                }
            }
        }
        for (b, o, (at, at_off), v) in found {
            if v < 0 {
                continue;
            }
            let ours = v > i64::from(i32::MAX)
                || v == i64::from(target_fd)
                || self.held_fds().any(|fd| i64::from(fd) == v);
            if ours {
                log::error!(
                    "IOCTL2 {}: the host left {v} at a descriptor-out field, which is \
                     not a descriptor it made for us; not adopted",
                    self.entry.name
                );
                continue;
            }
            // Written by the host into a field that held -1: its own, and
            // now ours.
            if let Some(fd) = self.arena.claim_fd(at, at_off) {
                self.fd_outs.push((b, o, fd));
            }
        }
    }

    fn held_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.fd_ins
            .iter()
            .map(|r| r.fd.as_raw_fd())
            .chain(self.owners.values().map(|f| f.as_raw_fd()))
            .chain(self.render_fd.iter().map(|f| f.as_raw_fd()))
    }

    /// GEM handles the host created in the target file. On a render node they
    /// are already where the guest's proxies live. In a KMS file they must not
    /// stay (closing the file would no longer be the end of it), so each
    /// distinct one is moved into the calling file's render handle and closed
    /// here; the dma-buf's size is the object's (RV:getfb2, dedupe and size).
    fn gems_out(&mut self, target_fd: RawFd) -> Result<(), Errno> {
        let slots: Vec<usize> = self.slots_of(|k| matches!(k, Kind::GemOut)).collect();
        let mut homed: HashMap<u32, (u32, u64)> = HashMap::new();
        let mut dropped = HashSet::new();
        let mut failed = None;
        for &i in &slots {
            let (b, o) = (self.slots[i].buf, self.slots[i].off);
            let h = rd(self.bytes(b), o, 4) as u32;
            if h == 0 {
                continue;
            }
            if self.class == SchemaClass::Render {
                self.gem_outs.push((b, o, h, 0));
                continue;
            }
            let moved = match homed.get(&h) {
                Some(&m) => m,
                None if failed.is_some() => {
                    // Not moved, and it must not stay in the KMS file either.
                    if dropped.insert(h) {
                        self.gem_close(target_fd, h);
                    }
                    continue;
                }
                None => match self.rehome_out(target_fd, h) {
                    Ok(m) => {
                        homed.insert(h, m);
                        m
                    }
                    Err(e) => {
                        dropped.insert(h);
                        failed = Some(e);
                        continue;
                    }
                },
            };
            let id = self.bufs[b].id;
            let _ = self.arena.set_value(id, o, u64::from(moved.0));
            self.gem_outs.push((b, o, moved.0, moved.1));
        }
        if let Some(e) = failed {
            // All or nothing: the guest makes no proxies for a call that
            // failed, so nothing may stay behind in its render file.
            if let Some(render) = self.render_fd.as_ref().map(|f| f.as_raw_fd()) {
                for (rh, _) in homed.into_values() {
                    self.gem_close(render, rh);
                }
            }
            for &i in &slots {
                let (b, o) = (self.bufs[self.slots[i].buf].id, self.slots[i].off);
                let _ = self.arena.set_value(b, o, 0);
            }
            self.gem_outs.clear();
            return Err(e);
        }
        Ok(())
    }

    /// Move `h` from the target file into the render file; `h` is closed in
    /// the target file whatever happens.
    fn rehome_out(&self, target_fd: RawFd, h: u32) -> Result<(u32, u64), Errno> {
        let r = self.move_to_render(target_fd, h);
        self.gem_close(target_fd, h);
        r
    }

    fn move_to_render(&self, target_fd: RawFd, h: u32) -> Result<(u32, u64), Errno> {
        let render = self.render_fd.as_ref().ok_or(libc::EINVAL)?.as_raw_fd();
        let dmabuf = self.prime_export(self.target, target_fd, h)?;
        let size = self.sys.size_of(dmabuf.as_raw_fd());
        let imported = self.prime_import(render, dmabuf.as_raw_fd());
        self.sys.close(dmabuf);
        let rh = imported?;
        if size < 0 {
            self.gem_close(render, rh);
            return Err(-size as Errno);
        }
        Ok((rh, size as u64))
    }

    // DRM helpers. struct drm_prime_handle {u32 handle; u32 flags; s32 fd},
    // drm_gem_close {u32 handle; u32 pad},
    // drm_nvidia_gem_identify_object_params {u32 handle; u32 object_type}.

    /// A DRM call of the backend's own on `fd`: `a` (the whole argument,
    /// data only) sent and read back.
    fn flat(&self, fd: RawFd, cmd: u32, a: &mut [u8]) -> i32 {
        crate::sys::block::flat(&*self.sys, fd, u64::from(cmd), a)
    }

    /// `gem` of guest file `file` (host descriptor `fd`) as a dma-buf, if
    /// the export gate lets it leave (exportgate.rs, through the hooks).
    fn prime_export(&self, file: u32, fd: RawFd, gem: u32) -> Result<OwnedFd, Errno> {
        self.hooks.may_export(file, gem)?;
        let mut b = [0u8; 12];
        wr(&mut b, 0, 4, u64::from(gem));
        wr(&mut b, 4, 4, u64::from(DRM_CLOEXEC));
        let mut a = Arena::new();
        let top = a.small(&b);
        a.fd_out(top, 8, 4)?;
        let r = a.call(&*self.sys, fd, u64::from(DRM_IOCTL_PRIME_HANDLE_TO_FD), top);
        if r < 0 {
            return Err(-r);
        }
        let dmabuf = a.claim_fd(top, 8).ok_or(libc::EIO)?;
        if !self.hooks.exportable(dmabuf.as_fd()) {
            log::warn!("IOCTL2: an injected capture buffer would be re-homed; refused");
            self.sys.close(dmabuf);
            return Err(libc::EINVAL);
        }
        Ok(dmabuf)
    }

    fn prime_import(&self, fd: RawFd, dmabuf: RawFd) -> Result<u32, Errno> {
        let mut a = [0u8; 12];
        wr(&mut a, 8, 4, dmabuf as u32 as u64);
        let r = self.flat(fd, DRM_IOCTL_PRIME_FD_TO_HANDLE, &mut a);
        if r < 0 {
            return Err(-r);
        }
        Ok(rd(&a, 0, 4) as u32)
    }

    fn gem_close(&self, fd: RawFd, gem: u32) {
        let mut a = [0u8; 8];
        wr(&mut a, 0, 4, u64::from(gem));
        let r = self.flat(fd, DRM_IOCTL_GEM_CLOSE, &mut a);
        if r < 0 {
            log::warn!(
                "IOCTL2 {}: GEM_CLOSE of {gem} failed: {}",
                self.entry.name,
                -r
            );
        }
    }

    fn identify(&self, fd: RawFd, gem: u32) -> Result<u64, Errno> {
        let mut a = [0u8; 8];
        wr(&mut a, 0, 4, u64::from(gem));
        let r = self.flat(fd, DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT, &mut a);
        if r < 0 {
            return Err(-r);
        }
        Ok(rd(&a, 4, 4))
    }
}

// ───────────────────────────── response ─────────────────────────────

impl Prepared {
    /// Put the guest's own values back where ours were -- its pointers and
    /// GEM handles; -1 in every descriptor field, which only it can fill --
    /// and lay out `Ioctl2Resp`, the OUT bytes, the fd and GEM records.
    fn response(&mut self, fd_recs: &[[u32; 4]]) -> Vec<u8> {
        // Each buffer's reply copy (sys/block.rs) carries the guest's own
        // pointers and GEM handles, and -1 in every descriptor field.
        let mut data = Vec::new();
        for b in self.bufs.iter().filter(|b| b.dir.has_out()) {
            data.extend_from_slice(&self.arena.reply(b.id));
            data.resize(pad8(data.len()), 0);
        }
        let mut out = Vec::with_capacity(self.response_len());
        for v in [
            self.ret as u32,
            self.bufs.len() as u32,
            fd_recs.len() as u32,
            self.gem_outs.len() as u32,
            data.len() as u32,
            0,
            0,
            0,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&data);
        for r in fd_recs {
            for v in r {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        for &(b, o, gem, size) in &self.gem_outs {
            for v in [b as u32, o as u32, gem, 0] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&size.to_le_bytes());
        }
        out
    }
}

impl Drop for Prepared {
    /// A call dropped between execute and finish (a session reset) still
    /// owns what the host made for it.
    fn drop(&mut self) {
        for (_, _, fd) in self.fd_outs.drain(..) {
            self.sys.close(fd);
        }
    }
}

// ───────────────────────────── bytes ─────────────────────────────

fn pad8(n: usize) -> usize {
    (n + 7) & !7
}

/// Little-endian unsigned read of `width` (1..=8) bytes.
fn rd(b: &[u8], off: usize, width: usize) -> u64 {
    let mut v = [0u8; 8];
    v[..width].copy_from_slice(&b[off..off + width]);
    u64::from_le_bytes(v)
}

fn wr(b: &mut [u8], off: usize, width: usize, v: u64) {
    b[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
}

/// Sign-extend a `width`-byte value.
fn sext(v: u64, width: u8) -> i64 {
    if width == 4 {
        v as u32 as i32 as i64
    } else {
        v as i64
    }
}

fn trim(name: &[u8]) -> &[u8] {
    let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    &name[..end]
}

#[cfg(test)]
mod tests {
    //! Every rule above, against a fake kernel. The fake implements the DRM
    //! helper ioctls (PRIME both ways, GEM_CLOSE, IDENTIFY, GETPROPERTY) over
    //! a model of per-file GEM handle tables, and hands every other ioctl to
    //! the test, which sees exactly the argument the host would: host
    //! pointers, host descriptors, target-file GEM handles.

    use super::*;
    use crate::kms_state::{PROBES_KEPT, VmKms};
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::sync::Mutex;

    use abi::version::DriverVersion;
    use protocol::messages::DeviceKind;

    const KMS: u32 = 10;
    const OTHER_KMS: u32 = 11;
    const RENDER: u32 = 20;
    const OTHER_RENDER: u32 = 21;
    const CTL: u32 = 30;
    const MODESET: u32 = 31;
    const SYNC: u32 = 40;
    const SYNCOBJ: u32 = 41;

    fn kind_of(h: u32) -> HandleKind {
        match h {
            KMS | OTHER_KMS => HandleKind::DrmLease(0),
            RENDER | OTHER_RENDER => HandleKind::DriRender(0),
            CTL => HandleKind::Dev(DeviceKind::Ctl),
            MODESET => HandleKind::Dev(DeviceKind::Modeset),
            SYNC => HandleKind::SyncFile,
            SYNCOBJ => HandleKind::Syncobj,
            _ => HandleKind::Other,
        }
    }

    fn iowr(nr: u32, size: u32) -> u32 {
        0xc000_0000 | size << 16 | (b'd' as u32) << 8 | nr
    }

    /// The `width`-byte word `off` bytes past `p` in the call's memory, as
    /// the kernel reads it (`p`: the argument's address, `arg.addr()`, or a
    /// pointer read out of a block).
    fn peek(a: &Arg<'_>, p: u64, off: usize, width: usize) -> u64 {
        a.peek(p + off as u64, width)
    }

    fn poke(a: &mut Arg<'_>, p: u64, off: usize, width: usize, v: u64) {
        a.poke(p + off as u64, width, v)
    }

    type Main = Box<dyn FnMut(&mut Kernel, u32, u32, &mut Arg<'_>) -> i32 + Send>;

    #[derive(Default)]
    struct Kernel {
        /// Host descriptor → the backend handle whose file it is.
        files: HashMap<RawFd, u32>,
        /// File → GEM handle → object.
        gems: HashMap<u32, BTreeMap<u32, u64>>,
        /// Object → (IDENTIFY type, size).
        objects: HashMap<u64, (u64, u64)>,
        dmabufs: HashMap<RawFd, u64>,
        next_fd: RawFd,
        props: HashMap<u32, &'static str>,
        /// GETPROPERTY's flags, by property (0 if absent).
        prop_flags: HashMap<u32, u32>,
        log: Vec<String>,
    }

    impl Kernel {
        fn object(&mut self, file: u32, handle: u32, obj: u64, ty: u64, size: u64) {
            self.gems.entry(file).or_default().insert(handle, obj);
            self.objects.insert(obj, (ty, size));
        }

        fn handle_of(&self, file: u32, obj: u64) -> Option<u32> {
            self.gems
                .get(&file)?
                .iter()
                .find(|(_, o)| **o == obj)
                .map(|(h, _)| *h)
        }

        fn obj(&self, file: u32, handle: u32) -> Option<u64> {
            self.gems.get(&file)?.get(&handle).copied()
        }
    }

    type Pre = Arc<dyn Fn(u32) + Send + Sync>;

    struct FakeSys {
        k: Mutex<Kernel>,
        main: Mutex<Main>,
        /// Run first, holding nothing, on every ioctl: where a test holds a
        /// call inside the host while another runs.
        pre: Mutex<Option<Pre>>,
    }

    impl FakeSys {
        fn new() -> Arc<Self> {
            Arc::new(FakeSys {
                k: Mutex::new(Kernel {
                    next_fd: 50_000,
                    ..Kernel::default()
                }),
                main: Mutex::new(Box::new(|_, _, _, _| 0)),
                pre: Mutex::new(None),
            })
        }

        fn before_ioctl(&self, f: impl Fn(u32) + Send + Sync + 'static) {
            *self.pre.lock().unwrap() = Some(Arc::new(f));
        }

        fn on_ioctl(
            &self,
            f: impl FnMut(&mut Kernel, u32, u32, &mut Arg<'_>) -> i32 + Send + 'static,
        ) {
            *self.main.lock().unwrap() = Box::new(f);
        }

        fn k(&self) -> std::sync::MutexGuard<'_, Kernel> {
            self.k.lock().unwrap()
        }

        fn log(&self) -> Vec<String> {
            std::mem::take(&mut self.k().log)
        }
    }

    impl crate::sys::block::Kernel for FakeSys {
        fn ioctl(&self, fd: RawFd, cmd: u64, arg: &mut Arg<'_>) -> i32 {
            let cmd = cmd as u32;
            let pre = self.pre.lock().unwrap().clone();
            if let Some(pre) = pre {
                pre(cmd);
            }
            let mut k = self.k();
            let Some(&file) = k.files.get(&fd) else {
                return -libc::EBADF;
            };
            let top = arg.addr();
            {
                match cmd {
                    DRM_IOCTL_PRIME_HANDLE_TO_FD => {
                        let h = peek(arg, top, 0, 4) as u32;
                        let Some(obj) = k.obj(file, h) else {
                            return -libc::ENOENT;
                        };
                        let dmabuf = k.next_fd;
                        k.next_fd += 1;
                        k.dmabufs.insert(dmabuf, obj);
                        poke(arg, top, 8, 4, dmabuf as u64);
                        k.log.push(format!("export {file}:{h}"));
                        0
                    }
                    DRM_IOCTL_PRIME_FD_TO_HANDLE => {
                        let Some(&obj) = k.dmabufs.get(&(peek(arg, top, 8, 4) as RawFd)) else {
                            return -libc::EBADF;
                        };
                        // drm_prime.c:304: the handle the file already has.
                        let h = k.handle_of(file, obj).unwrap_or_else(|| {
                            let used = k.gems.get(&file).cloned().unwrap_or_default();
                            (1..).find(|h| !used.contains_key(h)).unwrap()
                        });
                        k.gems.entry(file).or_default().insert(h, obj);
                        poke(arg, top, 0, 4, h as u64);
                        k.log.push(format!("import {file}:{h}"));
                        0
                    }
                    DRM_IOCTL_GEM_CLOSE => {
                        let h = peek(arg, top, 0, 4) as u32;
                        k.log.push(format!("close {file}:{h}"));
                        match k.gems.get_mut(&file).and_then(|g| g.remove(&h)) {
                            Some(_) => 0,
                            None => -libc::EINVAL,
                        }
                    }
                    DRM_IOCTL_NVIDIA_GEM_IDENTIFY_OBJECT => {
                        let h = peek(arg, top, 0, 4) as u32;
                        k.log.push(format!("identify {file}:{h}"));
                        // An unknown handle is answered, not refused:
                        // nvidia-drm says OBJECT_UNKNOWN
                        // (nv_drm_gem_identify_object_ioctl).
                        let ty = k
                            .obj(file, h)
                            .map_or(u64::from(crate::hostfd::NV_GEM_OBJECT_UNKNOWN), |obj| {
                                k.objects[&obj].0
                            });
                        poke(arg, top, 4, 4, ty);
                        0
                    }
                    DRM_IOCTL_MODE_GETPROPERTY => {
                        let id = peek(arg, top, 16, 4) as u32;
                        k.log.push(format!("getprop {id}"));
                        let Some(name) = k.props.get(&id) else {
                            return -libc::ENOENT;
                        };
                        arg.bytes()[24..24 + name.len()].copy_from_slice(name.as_bytes());
                        let flags = k.prop_flags.get(&id).copied().unwrap_or(0);
                        arg.bytes()[20..24].copy_from_slice(&flags.to_le_bytes());
                        0
                    }
                    _ => {
                        k.log.push(format!("ioctl {file} {cmd:#x}"));
                        (self.main.lock().unwrap())(&mut k, file, cmd, arg)
                    }
                }
            }
        }
    }

    impl Sys for FakeSys {
        fn close(&self, fd: OwnedFd) {
            // A number of the fake kernel's: nothing to close.
            let fd = fd.into_raw_fd();
            let mut k = self.k();
            k.dmabufs.remove(&fd);
            k.log.push(format!("closefd {fd}"));
        }

        fn size_of(&self, fd: RawFd) -> i64 {
            let k = self.k();
            k.dmabufs
                .get(&fd)
                .map_or(-libc::EBADF as i64, |o| k.objects[o].1 as i64)
        }
    }

    /// Lets fence schemas and fence properties through, as FENCES will.
    struct AllowFences;

    impl Hooks for AllowFences {
        fn atomic_fence_prop(&self, _: &Prepared, _: PropKind, _: &[u8]) -> Result<(), Errno> {
            Ok(())
        }

        fn before(&self, _: &mut Prepared) -> Result<(), Errno> {
            Ok(())
        }
    }

    struct H {
        sys: Arc<FakeSys>,
        hooks: Arc<dyn Hooks>,
        kms: Option<Arc<KmsFileState>>,
        version: Option<DriverVersion>,
        /// The host descriptor of each target's file, which `execute` gets.
        target_fds: HashMap<u32, OwnedFd>,
    }

    impl Env for H {
        fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)> {
            if !(10..50).contains(&handle) {
                return None;
            }
            let fd: OwnedFd = File::open("/dev/null").unwrap().into();
            self.sys.k().files.insert(fd.as_raw_fd(), handle);
            Some((fd, kind_of(handle)))
        }

        fn kind(&self, handle: u32) -> Option<HandleKind> {
            Some(kind_of(handle))
        }

        fn nvkms_version(&self) -> Option<DriverVersion> {
            self.version
        }

        fn kms_state(&self, _: u32) -> Option<Arc<KmsFileState>> {
            self.kms.clone()
        }

        fn hooks(&self) -> Arc<dyn Hooks> {
            self.hooks.clone()
        }

        fn sys(&self) -> Arc<dyn Sys> {
            self.sys.clone()
        }
    }

    #[derive(Default)]
    struct Fin {
        backend_fds: HashSet<RawFd>,
        adopted: Vec<OwnedFd>,
        closed: Vec<u32>,
    }

    impl Finisher for Fin {
        fn is_backend_fd(&self, fd: RawFd) -> bool {
            self.backend_fds.contains(&fd)
        }

        fn adopt(&mut self, fd: OwnedFd) -> (u32, HandleKind) {
            self.adopted.push(fd);
            (1000 + self.adopted.len() as u32, HandleKind::SyncFile)
        }

        fn close_handle(&mut self, handle: u32) {
            self.closed.push(handle);
        }
    }

    fn h() -> H {
        let sys = FakeSys::new();
        let mut target_fds = HashMap::new();
        for t in [KMS, OTHER_KMS, RENDER, MODESET] {
            let fd: OwnedFd = File::open("/dev/null").unwrap().into();
            sys.k().files.insert(fd.as_raw_fd(), t);
            target_fds.insert(t, fd);
        }
        H {
            sys,
            hooks: Arc::new(DefaultHooks),
            kms: Some(Arc::new(KmsFileState::new())),
            version: None,
            target_fds,
        }
    }

    /// A request, built the way the guest builds one.
    #[derive(Default, Clone)]
    struct Rq {
        cmd: u32,
        render: u32,
        lens: Vec<u32>,
        data: Vec<u8>,
        fds: Vec<[u32; 4]>,
        gems: Vec<[u32; 4]>,
        dyns: Vec<[u32; 4]>,
    }

    impl Rq {
        fn new(cmd: u32) -> Self {
            Rq {
                cmd,
                render: RENDER,
                ..Rq::default()
            }
        }

        /// A buffer of `len` bytes; `bytes` are its IN bytes, if it has any.
        fn buf(mut self, len: u32, bytes: Option<&[u8]>) -> Self {
            self.lens.push(len);
            if let Some(b) = bytes {
                assert_eq!(b.len(), len as usize);
                self.data.extend_from_slice(b);
                self.data.resize(pad8(self.data.len()), 0);
            }
            self
        }

        fn fd(mut self, buf: u32, off: u32, handle: u32, flags: u32) -> Self {
            self.fds.push([buf, off, handle, flags]);
            self
        }

        fn gem(mut self, buf: u32, off: u32, owner: u32, gem: u32) -> Self {
            self.gems.push([buf, off, owner, gem]);
            self
        }

        fn dyn_(mut self, buf: u32, off: u32) -> Self {
            self.dyns.push([I2_DYN_OUT_FENCE, buf, off, 4]);
            self
        }

        fn bytes(&self) -> Vec<u8> {
            let mut p = Vec::new();
            for v in [
                self.cmd,
                0,
                self.lens.len() as u32,
                self.fds.len() as u32,
                self.gems.len() as u32,
                self.dyns.len() as u32,
                self.data.len() as u32,
                self.render,
            ] {
                p.extend_from_slice(&v.to_le_bytes());
            }
            for l in &self.lens {
                p.extend_from_slice(&l.to_le_bytes());
            }
            for r in self.fds.iter().chain(&self.gems).chain(&self.dyns) {
                for v in r {
                    p.extend_from_slice(&v.to_le_bytes());
                }
            }
            p.extend_from_slice(&self.data);
            p
        }
    }

    #[derive(Debug)]
    struct Resp {
        ret: i32,
        nbuf: u32,
        data: Vec<u8>,
        fds: Vec<[u32; 4]>,
        gems: Vec<(u32, u32, u32, u64)>,
    }

    fn parse(r: &[u8]) -> Resp {
        let w = |i: usize| rd(r, 4 * i, 4) as u32;
        let (nfd, ngem, dlen) = (w(2) as usize, w(3) as usize, w(4) as usize);
        assert_eq!(
            r.len(),
            RESP_HDR + dlen + nfd * FD_OUT_REC + ngem * GEM_OUT_REC
        );
        let fd_at = RESP_HDR + dlen;
        let gem_at = fd_at + nfd * FD_OUT_REC;
        Resp {
            ret: w(0) as i32,
            nbuf: w(1),
            data: r[RESP_HDR..fd_at].to_vec(),
            fds: (0..nfd)
                .map(|i| std::array::from_fn(|j| rd(r, fd_at + 16 * i + 4 * j, 4) as u32))
                .collect(),
            gems: (0..ngem)
                .map(|i| {
                    let a = gem_at + 24 * i;
                    let q = |j: usize| rd(r, a + 4 * j, 4) as u32;
                    (q(0), q(1), q(2), rd(r, a + 16, 8))
                })
                .collect(),
        }
    }

    impl H {
        fn prepare(&self, class: SchemaClass, target: u32, rq: &Rq) -> Result<Prepared, Errno> {
            prepare(self, class, target, kind_of(target), &rq.bytes())
        }

        fn run_with(
            &self,
            class: SchemaClass,
            target: u32,
            rq: &Rq,
            fin: &mut Fin,
        ) -> Result<Resp, Errno> {
            let mut p = self.prepare(class, target, rq)?;
            p.execute(self.target_fds[&target].as_raw_fd());
            let len = p.response_len();
            let out = p.finish_with(fin);
            assert!(out.len() <= len, "response_len is an upper bound");
            Ok(parse(&out))
        }

        fn run(&self, class: SchemaClass, target: u32, rq: &Rq) -> Result<Resp, Errno> {
            self.run_with(class, target, rq, &mut Fin::default())
        }

        fn kms(&self, rq: &Rq) -> Result<Resp, Errno> {
            self.run(SchemaClass::Kms, KMS, rq)
        }

        fn owns(&self, fb: u32) -> bool {
            self.kms.as_ref().unwrap().owns_fb(fb)
        }
    }

    fn arg(size: usize, fields: &[(usize, usize, u64)]) -> Vec<u8> {
        let mut a = vec![0u8; size];
        for &(off, width, v) in fields {
            wr(&mut a, off, width, v);
        }
        a
    }

    fn fresh_fd() -> RawFd {
        use std::os::fd::IntoRawFd;
        File::open("/dev/null").unwrap().into_raw_fd()
    }

    // ── lookup and framing ──

    #[test]
    fn an_ioctl_with_no_schema_is_refused_as_unknown() {
        let h = h();
        // GEM_IMPORT_USERSPACE_MEMORY: pins memory at an address in *our* mm.
        let rq = Rq::new(iowr(0x42, 24)).buf(24, Some(&[0; 24]));
        assert_eq!(
            h.run(SchemaClass::Render, RENDER, &rq).err(),
            Some(libc::ENOTTY)
        );
        // GEM_OPEN on a lease: any flink name on the device.
        let rq = Rq::new(iowr(0x0b, 16)).buf(16, Some(&[0; 16]));
        assert_eq!(h.kms(&rq).err(), Some(libc::ENOTTY));
    }

    #[test]
    fn a_known_ioctl_with_another_size_or_direction_is_invalid() {
        let h = h();
        let rq = Rq::new(iowr(0xa0, 60)).buf(60, Some(&[0; 60]));
        assert_eq!(h.kms(&rq).err(), Some(libc::EINVAL));
        let rq = Rq::new(iowr(0xa0, 64) & !0x8000_0000).buf(64, Some(&[0; 64]));
        assert_eq!(h.kms(&rq).err(), Some(libc::EINVAL));
    }

    #[test]
    fn a_schema_class_only_runs_on_its_own_kind_of_file() {
        let h = h();
        let rq = Rq::new(iowr(0xa6, 20)).buf(20, Some(&[0; 20]));
        assert_eq!(
            h.run(SchemaClass::Kms, RENDER, &rq).err(),
            Some(libc::EINVAL)
        );
        assert!(h.run(SchemaClass::Kms, KMS, &rq).is_ok());
    }

    #[test]
    fn a_payload_that_disagrees_with_its_own_counts_is_refused() {
        let h = h();
        let rq = Rq::new(iowr(0xa6, 20)).buf(20, Some(&[0; 20]));
        let kind = kind_of(KMS);
        let mut p = rq.bytes();
        p.push(0);
        assert_eq!(
            prepare(&h, SchemaClass::Kms, KMS, kind, &p).err(),
            Some(libc::EINVAL)
        );
        p.truncate(p.len() - 9);
        assert_eq!(
            prepare(&h, SchemaClass::Kms, KMS, kind, &p).err(),
            Some(libc::EINVAL)
        );
        let mut p = rq.bytes();
        wr(&mut p, 4, 4, 1); // flags
        assert_eq!(
            prepare(&h, SchemaClass::Kms, KMS, kind, &p).err(),
            Some(libc::EINVAL)
        );
    }

    // ── the walk ──

    /// GETRESOURCES with the given counts and guest pointers.
    fn getresources(counts: [u64; 4], ptrs: [u64; 4]) -> Vec<u8> {
        let mut f = Vec::new();
        for i in 0..4 {
            f.push((8 * i, 8, ptrs[i]));
            f.push((32 + 4 * i, 4, counts[i]));
        }
        arg(64, &f)
    }

    #[test]
    fn null_and_empty_pointers_get_no_buffer_and_reach_the_host_as_null() {
        let h = h();
        let a = getresources([3, 0, 2, 1], [0, 0x1000, 0x2000, 0x3000]);
        let rq = Rq::new(iowr(0xa0, 64))
            .buf(64, Some(&a))
            .buf(8, None)
            .buf(4, None);
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            assert_eq!(peek(arg, top, 0, 8), 0, "NULL stays NULL");
            assert_eq!(
                peek(arg, top, 8, 8),
                0,
                "an empty list is handed over as NULL"
            );
            let conn = peek(arg, top, 16, 8);
            let enc = peek(arg, top, 24, 8);
            assert!(conn != 0 && enc != 0);
            poke(arg, conn, 0, 4, 71);
            poke(arg, conn, 4, 4, 72);
            poke(arg, enc, 0, 4, 81);
            for (i, n) in [0u64, 0, 2, 1].iter().enumerate() {
                poke(arg, top, 32 + 4 * i, 4, *n);
            }
            0
        });
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, 0);
        assert_eq!(r.nbuf, 3);
        assert_eq!(r.data.len(), 64 + 8 + 8);
        // The guest's own pointers come back, not ours.
        for (i, p) in [0u64, 0x1000, 0x2000, 0x3000].iter().enumerate() {
            assert_eq!(rd(&r.data, 8 * i, 8), *p);
        }
        assert_eq!(rd(&r.data, 64, 4), 71);
        assert_eq!(rd(&r.data, 68, 4), 72);
        assert_eq!(rd(&r.data, 72, 4), 81);
    }

    #[test]
    fn buffer_lengths_must_be_exactly_the_ones_the_counts_give() {
        let h = h();
        let a = getresources([0, 0, 2, 0], [0, 0, 0x2000, 0]);
        let short = Rq::new(iowr(0xa0, 64)).buf(64, Some(&a)).buf(4, None);
        assert_eq!(h.kms(&short).err(), Some(libc::EINVAL));
        let extra = Rq::new(iowr(0xa0, 64))
            .buf(64, Some(&a))
            .buf(8, None)
            .buf(8, None);
        assert_eq!(h.kms(&extra).err(), Some(libc::EINVAL));
        let missing = Rq::new(iowr(0xa0, 64)).buf(64, Some(&a));
        assert_eq!(h.kms(&missing).err(), Some(libc::EINVAL));
        let right = Rq::new(iowr(0xa0, 64)).buf(64, Some(&a)).buf(8, None);
        assert!(h.kms(&right).is_ok());
    }

    #[test]
    fn a_count_above_the_cap_is_refused_before_anything_is_allocated() {
        let h = h();
        let a = getresources([0, 0, 1 << 20, 0], [0, 0, 0x2000, 0]);
        let rq = Rq::new(iowr(0xa0, 64)).buf(64, Some(&a)).buf(4 << 20, None);
        assert_eq!(h.kms(&rq).err(), Some(libc::E2BIG));
    }

    #[test]
    fn a_pointer_the_kernel_ignores_is_still_never_a_guest_address() {
        let h = h();
        let a = arg(104, &[(0, 8, 0xdead_0000), (8, 4, 4)]);
        let rq = Rq::new(iowr(0xa1, 104)).buf(104, Some(&a));
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            assert_eq!(peek(arg, top, 0, 8), 0);
            0
        });
        let r = h.kms(&rq).unwrap();
        assert_eq!(rd(&r.data, 0, 8), 0xdead_0000);
    }

    #[test]
    fn out_only_buffers_are_bounded_in_total_before_they_are_allocated() {
        // No DRM entry reaches the bound; an NVKMS table could, so a table
        // of five 1 MiB OUT arrays stands in for one.
        const OUT_MIB: Field = Field {
            name: "p",
            off: 0,
            cond: None,
            kind: Kind::Ptr {
                dir: Dir::Out,
                len: Len::Count {
                    off: 40,
                    width: 4,
                    elem: 1,
                },
                copyback: schema::CopyBack::Full,
                max: 1 << 20,
                stride: 0,
                children: Span { first: 0, len: 0 },
            },
        };
        static FIELDS: [Field; 5] = [
            OUT_MIB,
            Field { off: 8, ..OUT_MIB },
            Field { off: 16, ..OUT_MIB },
            Field { off: 24, ..OUT_MIB },
            Field { off: 32, ..OUT_MIB },
        ];
        static IOCTLS: [Ioctl; 1] = [Ioctl {
            name: "BIG",
            class: schema::Class::Kms,
            cmd: 0xc030_64f0,
            nvkms_cmd: 0,
            size: 48,
            exec: Exec::Executor,
            special: Special::None,
            policy: 0,
            arg_in_only: false,
            fields: Span { first: 0, len: 5 },
        }];
        static TABLE: Table = Table {
            name: "big",
            versions: None,
            ioctls: &IOCTLS,
            fields: &FIELDS,
            planes: &[],
            nvkms: None,
        };
        let mut a = vec![1u8; 48];
        wr(&mut a, 40, 4, 1 << 20);
        let mut rq = Rq::new(IOCTLS[0].cmd).buf(48, Some(&a));
        for _ in 0..5 {
            rq = rq.buf(1 << 20, None);
        }
        let payload = rq.bytes();
        let req = Req::parse(&payload).unwrap();
        let mut w = Walk {
            table: &TABLE,
            req: &req,
            cursor: 0,
            out_only: 0,
            arena: Arena::new(),
            bufs: Vec::new(),
            slots: Vec::new(),
        };
        assert_eq!(w.arg(&IOCTLS[0]), Err(libc::E2BIG));
        assert_eq!(w.bufs.len(), 5, "the fifth MiB was never allocated");
    }

    /// GEM_IMPORT_NVKMS_MEMORY: a pointer whose pointee holds a descriptor.
    fn import_nvkms(size: u64) -> Vec<u8> {
        arg(
            32,
            &[(0, 8, 4096), (8, 8, 0x5000), (16, 8, size), (24, 4, 0x77)],
        )
    }

    #[test]
    fn a_descriptor_inside_a_pointee_is_translated_and_a_new_render_handle_stays_put() {
        let h = h();
        let params = arg(28, &[(0, 4, 9)]);
        let rq = Rq::new(0xc020_6441)
            .buf(32, Some(&import_nvkms(28)))
            .buf(28, Some(&params))
            .fd(1, 0, CTL, 0);
        h.sys.on_ioctl(|k, _, _, arg| {
            let top = arg.addr();
            let p = peek(arg, top, 8, 8);
            let memfd = peek(arg, p, 0, 4) as RawFd;
            assert_eq!(
                k.files.get(&memfd),
                Some(&CTL),
                "a dup of the nvidiactl handle"
            );
            assert_eq!(peek(arg, top, 24, 4), 0, "a GEM-out field starts at 0");
            poke(arg, top, 24, 4, 5);
            0
        });
        let r = h.run(SchemaClass::Render, RENDER, &rq).unwrap();
        assert_eq!(r.ret, 0);
        assert_eq!(r.gems, vec![(0, 24, 5, 0)]);
        assert_eq!(rd(&r.data, 8, 8), 0x5000, "pointer restored");
        assert_eq!(r.data.len(), 32, "the pointee is IN only");
    }

    #[test]
    fn a_pointee_with_fields_must_hold_whole_elements() {
        let h = h();
        let rq = Rq::new(0xc020_6441)
            .buf(32, Some(&import_nvkms(20)))
            .buf(20, Some(&[0; 20]));
        assert_eq!(
            h.run(SchemaClass::Render, RENDER, &rq).err(),
            Some(libc::EINVAL)
        );
    }

    // ── descriptors in ──

    fn grant(fd: i32, ty: u32) -> Vec<u8> {
        arg(
            12,
            &[(0, 4, fd as u32 as u64), (4, 4, 0x100), (8, 4, ty as u64)],
        )
    }

    const GRANT: u32 = 0xc00c_6452;

    #[test]
    fn a_descriptor_field_is_a_dup_of_the_named_handle_and_comes_back_as_minus_one() {
        let h = h();
        let rq = Rq::new(GRANT)
            .buf(12, Some(&grant(7, 2)))
            .fd(0, 0, MODESET, 0);
        h.sys.on_ioctl(|k, _, _, arg| {
            let top = arg.addr();
            let fd = peek(arg, top, 0, 4) as RawFd;
            assert_eq!(k.files.get(&fd), Some(&MODESET));
            0
        });
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, 0);
        assert_eq!(rd(&r.data, 0, 4) as u32 as i32, -1);
    }

    #[test]
    fn a_descriptor_field_without_a_record_must_hold_none() {
        let h = h();
        let raw = Rq::new(GRANT).buf(12, Some(&grant(7, 2)));
        assert_eq!(h.kms(&raw).err(), Some(libc::EINVAL));
        let none = Rq::new(GRANT).buf(12, Some(&grant(-1, 2)));
        assert!(h.kms(&none).is_ok());
    }

    #[test]
    fn descriptor_records_must_sit_exactly_on_schema_positions_once() {
        let h = h();
        let off = Rq::new(GRANT)
            .buf(12, Some(&grant(-1, 2)))
            .fd(0, 4, MODESET, 0);
        assert_eq!(h.kms(&off).err(), Some(libc::EINVAL));
        let dup = Rq::new(GRANT)
            .buf(12, Some(&grant(7, 2)))
            .fd(0, 0, MODESET, 0)
            .fd(0, 0, MODESET, 0);
        assert_eq!(h.kms(&dup).err(), Some(libc::EINVAL));
        let flags = Rq::new(GRANT)
            .buf(12, Some(&grant(7, 2)))
            .fd(0, 0, MODESET, 2);
        assert_eq!(h.kms(&flags).err(), Some(libc::EINVAL));
    }

    #[test]
    fn a_descriptor_of_another_kind_or_no_handle_at_all_is_refused() {
        let h = h();
        let ctl = Rq::new(GRANT).buf(12, Some(&grant(7, 2))).fd(0, 0, CTL, 0);
        assert_eq!(h.kms(&ctl).err(), Some(libc::EINVAL));
        let gone = Rq::new(GRANT).buf(12, Some(&grant(7, 2))).fd(0, 0, 999, 0);
        assert_eq!(h.kms(&gone).err(), Some(libc::EBADF));
    }

    #[test]
    fn only_modeset_grants_and_revocations_pass() {
        let h = h();
        let sub_owner = Rq::new(GRANT)
            .buf(12, Some(&grant(7, 3)))
            .fd(0, 0, MODESET, 0);
        assert_eq!(h.kms(&sub_owner).err(), Some(libc::EPERM));
        let revoke = |ty| Rq::new(0xc008_6453).buf(8, Some(&arg(8, &[(0, 4, 1), (4, 4, ty)])));
        assert_eq!(h.kms(&revoke(3)).err(), Some(libc::EPERM));
        assert!(h.kms(&revoke(2)).is_ok());
    }

    #[test]
    fn fence_schemas_wait_for_the_fence_hook() {
        let mut h = h();
        let rq = Rq::new(iowr(0xbf, 8)).buf(8, Some(&[0; 8]));
        assert_eq!(
            h.run(SchemaClass::Render, RENDER, &rq).err(),
            Some(libc::EOPNOTSUPP)
        );
        h.hooks = Arc::new(AllowFences);
        assert!(h.run(SchemaClass::Render, RENDER, &rq).is_ok());
    }

    #[test]
    fn a_render_call_names_only_objects_of_its_own_file() {
        // SEMSURF_FENCE_ATTACH: buffer @0 and fence context @4, both GEM_IN.
        // Another render file's object would be re-homed as a temporary and
        // closed after the call -- and if the target already held it, the
        // import would have returned the target's own handle, which the
        // close would take from its proxy. The guest re-homes it for keeps.
        let mut h = h();
        h.hooks = Arc::new(AllowFences);
        let attach = |owner| {
            Rq::new(0x4000_0000 | 24 << 16 | (b'd' as u32) << 8 | 0x57)
                .buf(24, Some(&arg(24, &[(0, 4, 3), (4, 4, 4)])))
                .gem(0, 0, owner, 3)
                .gem(0, 4, RENDER, 4)
        };
        assert_eq!(
            h.prepare(SchemaClass::Render, RENDER, &attach(OTHER_RENDER))
                .err(),
            Some(libc::EINVAL)
        );
        assert!(
            h.prepare(SchemaClass::Render, RENDER, &attach(RENDER))
                .is_ok()
        );
    }

    #[test]
    fn a_conditional_descriptor_takes_the_kind_its_condition_selects() {
        let mut h = h();
        h.hooks = Arc::new(AllowFences);
        let fd_to_handle = |flags: u64, handle| {
            let a = arg(24, &[(4, 4, flags), (8, 4, 5)]);
            Rq::new(iowr(0xc2, 24))
                .buf(24, Some(&a))
                .fd(0, 8, handle, 0)
        };
        let r = |rq: Rq| h.run(SchemaClass::Render, RENDER, &rq).err();
        assert_eq!(r(fd_to_handle(1, SYNC)), None);
        assert_eq!(r(fd_to_handle(1, SYNCOBJ)), Some(libc::EINVAL));
        assert_eq!(r(fd_to_handle(0, SYNCOBJ)), None);
        assert_eq!(r(fd_to_handle(0, SYNC)), Some(libc::EINVAL));
    }

    #[test]
    fn consumed_handles_are_closed_by_finish() {
        let h = h();
        let rq = Rq::new(GRANT)
            .buf(12, Some(&grant(7, 2)))
            .fd(0, 0, MODESET, I2_FD_CONSUME);
        let mut fin = Fin::default();
        h.run_with(SchemaClass::Kms, KMS, &rq, &mut fin).unwrap();
        assert_eq!(fin.closed, vec![MODESET]);
    }

    // ── descriptors out ──

    fn create_lease() -> Rq {
        let a = arg(24, &[(0, 8, 0x9000), (8, 4, 1)]);
        Rq::new(iowr(0xc6, 24))
            .buf(24, Some(&a))
            .buf(4, Some(&[1, 0, 0, 0]))
    }

    #[test]
    fn a_descriptor_the_host_makes_at_a_schema_position_is_adopted() {
        let h = h();
        let fd = fresh_fd();
        h.sys.on_ioctl(move |_, _, _, arg| {
            let top = arg.addr();
            assert_eq!(peek(arg, top, 20, 4) as u32 as i32, -1, "starts at -1");
            poke(arg, top, 20, 4, fd as u64);
            0
        });
        let mut fin = Fin::default();
        let r = h
            .run_with(SchemaClass::Kms, KMS, &create_lease(), &mut fin)
            .unwrap();
        assert_eq!(fin.adopted.len(), 1);
        assert_eq!(fin.adopted[0].as_raw_fd(), fd);
        assert_eq!(r.fds, vec![[0, 20, 1001, HandleKind::SyncFile.wire()]]);
        assert_eq!(
            rd(&r.data, 20, 4) as u32 as i32,
            -1,
            "only the guest can fill it"
        );
    }

    #[test]
    fn nothing_is_adopted_that_the_host_did_not_make_for_this_call() {
        let h = h();
        // Left alone: still -1.
        h.sys.on_ioctl(|_, _, _, _| 0);
        assert!(h.kms(&create_lease()).unwrap().fds.is_empty());
        // A failed call adopts nothing, whatever is there.
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            poke(arg, top, 20, 4, 3);
            -libc::EINVAL
        });
        let r = h.kms(&create_lease()).unwrap();
        assert_eq!(r.ret, -libc::EINVAL);
        assert!(r.fds.is_empty());
        // The call's own target.
        let target = h.target_fds[&KMS].as_raw_fd();
        h.sys.on_ioctl(move |_, _, _, arg| {
            let top = arg.addr();
            poke(arg, top, 20, 4, target as u64);
            0
        });
        assert!(h.kms(&create_lease()).unwrap().fds.is_empty());
        // One the backend already holds.
        let held = File::open("/dev/null").unwrap();
        let fd = held.as_raw_fd();
        h.sys.on_ioctl(move |_, _, _, arg| {
            let top = arg.addr();
            poke(arg, top, 20, 4, fd as u64);
            0
        });
        let mut fin = Fin::default();
        fin.backend_fds.insert(fd);
        let r = h
            .run_with(SchemaClass::Kms, KMS, &create_lease(), &mut fin)
            .unwrap();
        assert!(r.fds.is_empty() && fin.adopted.is_empty());
        // Still open: not adopted, and not closed either.
        assert!(crate::sys::fd::is_open(fd));
        drop(held);
    }

    // ── GEM in ──

    fn addfb2(format: u32, handles: [u32; 4]) -> Vec<u8> {
        let mut f = vec![(4, 4, 64), (8, 4, 64), (12, 4, format as u64)];
        for (i, h) in handles.iter().enumerate() {
            f.push((20 + 4 * i, 4, *h as u64));
        }
        arg(104, &f)
    }

    const NV12: u32 = 0x3231_564e;
    const XRGB8888: u32 = 0x3432_5258;
    const ADDFB2: u32 = 0xc068_64b8;

    #[test]
    fn addfb2_rehomes_validates_runs_and_closes_every_plane_in_one_job() {
        let h = h();
        h.sys
            .k()
            .object(RENDER, 3, 100, NV_GEM_OBJECT_NVKMS, 1 << 20);
        let rq = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(NV12, [9, 9, 0, 0])))
            .gem(0, 20, RENDER, 3)
            .gem(0, 24, RENDER, 3);
        h.sys.on_ioctl(|k, file, _, arg| {
            let top = arg.addr();
            let (a, b) = (peek(arg, top, 20, 4) as u32, peek(arg, top, 24, 4) as u32);
            assert_eq!(a, b, "one object, one temporary");
            assert_eq!(k.obj(file, a), Some(100));
            poke(arg, top, 0, 4, 42);
            0
        });
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, 0);
        let log: Vec<String> = h
            .sys
            .log()
            .into_iter()
            .filter(|l| !l.starts_with("closefd"))
            .collect();
        assert_eq!(
            log,
            vec![
                format!("export {RENDER}:3"),
                format!("import {KMS}:1"),
                format!("identify {KMS}:1"),
                format!("identify {KMS}:1"),
                format!("ioctl {KMS} {ADDFB2:#x}"),
                format!("close {KMS}:1"),
            ]
        );
        assert!(
            h.sys.k().gems[&KMS].is_empty(),
            "nothing stays in the KMS file"
        );
        assert_eq!(rd(&r.data, 20, 4), 9, "the guest's handles come back");
        assert!(h.owns(42));
    }

    /// A re-home whose export the hooks refuse (an injected capture buffer,
    /// inject/) runs nothing and leaves nothing in the KMS file.
    #[test]
    fn a_rehome_the_hooks_refuse_never_reaches_addfb() {
        struct NoExport;
        impl Hooks for NoExport {
            fn before(&self, _: &mut Prepared) -> Result<(), Errno> {
                Ok(())
            }
            fn exportable(&self, _: std::os::fd::BorrowedFd<'_>) -> bool {
                false
            }
        }
        let mut h = h();
        h.hooks = Arc::new(NoExport);
        h.sys
            .k()
            .object(RENDER, 3, 100, NV_GEM_OBJECT_NVKMS, 1 << 20);
        let rq = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])))
            .gem(0, 20, RENDER, 3);
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, -libc::EINVAL);
        let log = h.sys.log();
        assert!(!log.iter().any(|l| l.starts_with("ioctl")), "{log:?}");
        assert!(!log.iter().any(|l| l.starts_with("import")), "{log:?}");
        assert!(h.sys.k().gems.get(&KMS).is_none_or(|g| g.is_empty()));
    }

    /// A fence context is never re-homed: the backend's hooks ask the one
    /// export gate before the export (exportgate.rs).
    #[test]
    fn a_fence_context_is_never_rehomed() {
        let semsurf = Arc::new(crate::semsurf::SemsurfPolicy::default());
        semsurf.ctx_made_for_test(RENDER, 3);
        let mut h = h();
        h.hooks = crate::policy::BackendHooks::with_state(Default::default(), semsurf)
            .with_inject_taint(Default::default());
        h.sys
            .k()
            .object(RENDER, 3, 100, NV_GEM_OBJECT_NVKMS, 1 << 20);
        let rq = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])))
            .gem(0, 20, RENDER, 3);
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, -libc::EINVAL);
        let log = h.sys.log();
        assert!(!log.iter().any(|l| l.starts_with("export")), "{log:?}");
        assert!(!log.iter().any(|l| l.starts_with("ioctl")), "{log:?}");
    }

    #[test]
    fn an_object_that_is_not_nvkms_memory_never_reaches_addfb() {
        let h = h();
        // USERMEMORY: pMemory is NULL, which fb_create dereferences.
        h.sys.k().object(RENDER, 3, 100, 2, 4096);
        let rq = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])))
            .gem(0, 20, RENDER, 3);
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, -libc::EINVAL);
        let log = h.sys.log();
        assert!(!log.iter().any(|l| l.starts_with("ioctl")), "{log:?}");
        assert!(log.contains(&format!("close {KMS}:1")));
        assert!(!h.owns(0));
    }

    #[test]
    fn addfb2_handles_past_the_formats_planes_must_be_zero() {
        let h = h();
        h.sys.k().object(RENDER, 3, 100, NV_GEM_OBJECT_NVKMS, 4096);
        let rq = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [9, 9, 0, 0])))
            .gem(0, 20, RENDER, 3)
            .gem(0, 24, RENDER, 3);
        assert_eq!(h.kms(&rq).err(), Some(libc::EINVAL));
    }

    #[test]
    fn a_gem_field_holding_a_handle_needs_one_record_and_a_zero_one_none() {
        let h = h();
        let unbacked = Rq::new(ADDFB2).buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])));
        assert_eq!(h.kms(&unbacked).err(), Some(libc::EINVAL));
        let on_zero = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [0, 0, 0, 0])))
            .gem(0, 20, RENDER, 3);
        assert_eq!(h.kms(&on_zero).err(), Some(libc::EINVAL));
        let twice = Rq::new(ADDFB2)
            .buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])))
            .gem(0, 20, RENDER, 3)
            .gem(0, 20, RENDER, 3);
        assert_eq!(h.kms(&twice).err(), Some(libc::EINVAL));
    }

    #[test]
    fn guest_objects_are_named_through_a_render_owner_never_a_kms_file() {
        let h = h();
        let own = |owner| {
            Rq::new(ADDFB2)
                .buf(104, Some(&addfb2(XRGB8888, [9, 0, 0, 0])))
                .gem(0, 20, owner, 3)
        };
        assert_eq!(h.kms(&own(KMS)).err(), Some(libc::EINVAL));
        assert_eq!(h.kms(&own(OTHER_KMS)).err(), Some(libc::EINVAL));
        assert_eq!(h.kms(&own(CTL)).err(), Some(libc::EINVAL));
    }

    #[test]
    fn a_cursor_handle_is_a_gem_field_only_with_the_bo_flag() {
        let h = h();
        let cursor = |flags| {
            let a = arg(28, &[(0, 4, flags), (24, 4, 9)]);
            Rq::new(iowr(0xa3, 28)).buf(28, Some(&a))
        };
        // DRM_MODE_CURSOR_MOVE: the handle is not read.
        assert!(h.kms(&cursor(2)).is_ok());
        assert_eq!(h.kms(&cursor(1)).err(), Some(libc::EINVAL));
        h.sys.k().object(RENDER, 3, 100, NV_GEM_OBJECT_NVKMS, 4096);
        let r = h.kms(&cursor(1).gem(0, 24, RENDER, 3)).unwrap();
        assert_eq!(r.ret, 0);
    }

    // ── GEM out ──

    fn getfb(fb: u32) -> Rq {
        Rq::new(iowr(0xad, 28)).buf(28, Some(&arg(28, &[(0, 4, fb as u64)])))
    }

    #[test]
    fn getfb_answers_with_a_render_handle_only_for_the_files_own_framebuffer() {
        let h = h();
        h.kms.as_ref().unwrap().add_fb(42);
        h.sys.on_ioctl(|k, file, _, arg| {
            let top = arg.addr();
            k.object(file, 7, 200, NV_GEM_OBJECT_NVKMS, 8192);
            poke(arg, top, 24, 4, 7);
            0
        });
        let r = h.kms(&getfb(42)).unwrap();
        assert_eq!(r.ret, 0);
        let rh = h
            .sys
            .k()
            .handle_of(RENDER, 200)
            .expect("moved to the render file");
        assert_eq!(r.gems, vec![(0, 24, rh, 8192)]);
        assert_eq!(rd(&r.data, 24, 4) as u32, rh);
        assert!(h.sys.k().gems[&KMS].is_empty());

        // Someone else's framebuffer: the handle is closed and zeroed.
        h.sys.log();
        let r = h.kms(&getfb(43)).unwrap();
        assert!(r.gems.is_empty());
        assert_eq!(rd(&r.data, 24, 4), 0);
        assert!(h.sys.log().contains(&format!("close {KMS}:7")));
    }

    #[test]
    fn a_removed_framebuffer_is_no_longer_the_files_own() {
        let h = h();
        h.kms.as_ref().unwrap().add_fb(42);
        let rmfb = Rq::new(iowr(0xaf, 4)).buf(4, Some(&[42, 0, 0, 0]));
        h.kms(&rmfb).unwrap();
        assert!(!h.owns(42));
    }

    #[test]
    fn getfb2_moves_an_object_shared_by_planes_once() {
        let h = h();
        h.kms.as_ref().unwrap().add_fb(42);
        h.sys.on_ioctl(|k, file, _, arg| {
            let top = arg.addr();
            k.object(file, 5, 300, NV_GEM_OBJECT_NVKMS, 4096);
            poke(arg, top, 20, 4, 5);
            poke(arg, top, 24, 4, 5);
            0
        });
        let rq = Rq::new(iowr(0xce, 104)).buf(104, Some(&arg(104, &[(0, 4, 42)])));
        let r = h.kms(&rq).unwrap();
        let exports = h
            .sys
            .log()
            .iter()
            .filter(|l| l.starts_with("export"))
            .count();
        assert_eq!(exports, 1);
        let rh = r.gems[0].2;
        assert_eq!(r.gems, vec![(0, 20, rh, 4096), (0, 24, rh, 4096)]);
    }

    #[test]
    fn a_failed_move_fails_the_call_and_leaves_nothing_behind() {
        let h = h();
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            // A handle the export cannot find.
            poke(arg, top, 16, 4, 5);
            0
        });
        let a = arg(32, &[(0, 4, 64), (4, 4, 64), (8, 4, 32)]);
        let rq = Rq::new(iowr(0xb2, 32)).buf(32, Some(&a));
        let r = h.kms(&rq).unwrap();
        assert_eq!(r.ret, -libc::ENOENT);
        assert!(r.gems.is_empty());
        assert_eq!(rd(&r.data, 16, 4), 0);
        assert!(h.sys.log().contains(&format!("close {KMS}:5")));
    }

    // ── ATOMIC ──

    const ATOMIC: u32 = 0xc038_64bc;

    fn atomic(props: &[(u32, u64)], split: &[u32]) -> Rq {
        let n: u32 = split.iter().sum();
        assert_eq!(n as usize, props.len());
        let objs = split.len() as u32;
        let a = arg(
            56,
            &[
                (4, 4, objs as u64),
                (8, 8, 0x100),
                (16, 8, 0x200),
                (24, 8, 0x300),
                (32, 8, 0x400),
            ],
        );
        let ids_of_objs: Vec<u8> = (0..objs).flat_map(|i| (i + 1).to_le_bytes()).collect();
        let counts: Vec<u8> = split.iter().flat_map(|c| c.to_le_bytes()).collect();
        let ids: Vec<u8> = props.iter().flat_map(|p| p.0.to_le_bytes()).collect();
        let values: Vec<u8> = props.iter().flat_map(|p| p.1.to_le_bytes()).collect();
        Rq::new(ATOMIC)
            .buf(56, Some(&a))
            .buf(4 * objs, Some(&ids_of_objs))
            .buf(4 * objs, Some(&counts))
            .buf(4 * n, Some(&ids))
            .buf(8 * n, Some(&values))
    }

    fn with_props(h: &H) {
        let mut k = h.sys.k();
        k.props.insert(1, "FB_ID");
        k.props.insert(2, "IN_FENCE_FD");
        k.props.insert(3, "OUT_FENCE_PTR");
        k.props.insert(4, "CRTC_ID");
    }

    #[test]
    fn atomic_property_arrays_are_as_long_as_the_counts_add_up_to() {
        let h = h();
        with_props(&h);
        h.kms.as_ref().unwrap().add_fb(9);
        h.kms.as_ref().unwrap().add_fb(8);
        let rq = atomic(&[(1, 9), (4, 3), (1, 8), (4, 2), (4, 1)], &[2, 3]);
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            let ids = peek(arg, top, 24, 8);
            assert_eq!(
                peek(arg, ids, 16, 4),
                4,
                "the fifth id is where it should be"
            );
            0
        });
        assert_eq!(h.kms(&rq).unwrap().ret, 0);
        let mut wrong = rq.clone();
        wrong.lens[3] = 16;
        assert_eq!(h.kms(&wrong).err(), Some(libc::EINVAL));
    }

    #[test]
    fn atomic_fence_properties_are_refused_until_the_fence_hook_allows_them() {
        let h = h();
        with_props(&h);
        h.kms.as_ref().unwrap().add_fb(5);
        // "None" values pass: compositors set IN_FENCE_FD = -1 routinely.
        let none = atomic(&[(2, u64::MAX), (3, 0), (1, 5)], &[3]);
        assert_eq!(h.kms(&none).unwrap().ret, 0);
        h.sys.log();
        let fence = atomic(&[(2, 5)], &[1]);
        assert_eq!(h.kms(&fence).unwrap().ret, -libc::EOPNOTSUPP);
        let out = atomic(&[(3, 0x7777_0000)], &[1]);
        assert_eq!(h.kms(&out).unwrap().ret, -libc::EOPNOTSUPP);
        assert!(!h.sys.log().iter().any(|l| l.starts_with("ioctl")));
    }

    #[test]
    fn an_allowed_fence_property_still_needs_its_record() {
        let mut h = h();
        h.hooks = Arc::new(AllowFences);
        with_props(&h);
        h.kms.as_ref().unwrap().add_fb(5);
        let raw = atomic(&[(2, 5)], &[1]);
        assert_eq!(h.kms(&raw).unwrap().ret, -libc::EINVAL);
        let on_plain = atomic(&[(1, 5)], &[1]).fd(4, 0, SYNC, 0);
        assert_eq!(h.kms(&on_plain).unwrap().ret, -libc::EINVAL);
        let wrong_kind = atomic(&[(2, 5)], &[1]).fd(4, 0, SYNCOBJ, 0);
        assert_eq!(h.kms(&wrong_kind).err(), Some(libc::EINVAL));
        let misaligned = atomic(&[(2, 5)], &[1]).fd(4, 4, SYNC, 0);
        assert_eq!(h.kms(&misaligned).err(), Some(libc::EINVAL));
    }

    #[test]
    fn an_in_fence_becomes_a_host_descriptor_and_an_out_fence_a_host_pointer() {
        let mut h = h();
        h.hooks = Arc::new(AllowFences);
        with_props(&h);
        let out_fd = fresh_fd();
        let rq = atomic(&[(2, 5), (3, 0x7777_0000)], &[2])
            .fd(4, 0, SYNC, 0)
            .dyn_(4, 8);
        h.sys.on_ioctl(move |k, _, _, arg| {
            let top = arg.addr();
            let values = peek(arg, top, 32, 8);
            let in_fd = peek(arg, values, 0, 8) as RawFd;
            assert_eq!(k.files.get(&in_fd), Some(&SYNC));
            let s32 = peek(arg, values, 8, 8);
            assert_ne!(s32, 0x7777_0000, "never the guest's pointer");
            assert_eq!(peek(arg, s32, 0, 4) as u32 as i32, -1);
            poke(arg, s32, 0, 4, out_fd as u64);
            0
        });
        let mut fin = Fin::default();
        let r = h.run_with(SchemaClass::Kms, KMS, &rq, &mut fin).unwrap();
        assert_eq!(r.ret, 0);
        assert_eq!(r.fds, vec![[4, 8, 1001, HandleKind::SyncFile.wire()]]);
        assert_eq!(fin.adopted[0].as_raw_fd(), out_fd);
    }

    #[test]
    fn dyn_records_are_refused_on_anything_but_atomic() {
        let h = h();
        let rq = create_lease().dyn_(1, 0);
        assert_eq!(h.kms(&rq).err(), Some(libc::EINVAL));
    }

    #[test]
    fn legacy_property_setters_refuse_fence_properties_outright() {
        let mut h = h();
        h.hooks = Arc::new(AllowFences);
        with_props(&h);
        let set = |prop| {
            let a = arg(24, &[(0, 8, u64::MAX), (8, 4, prop)]);
            Rq::new(iowr(0xba, 24)).buf(24, Some(&a))
        };
        assert_eq!(h.kms(&set(2)).unwrap().ret, -libc::EINVAL);
        assert_eq!(h.kms(&set(3)).unwrap().ret, -libc::EINVAL);
        assert_eq!(h.kms(&set(4)).unwrap().ret, 0);
        // A property the host does not know is not "plain".
        assert_eq!(h.kms(&set(99)).unwrap().ret, -libc::ENOENT);
    }

    #[test]
    fn property_names_are_looked_up_once_per_file() {
        let h = h();
        with_props(&h);
        h.kms.as_ref().unwrap().add_fb(9);
        let rq = atomic(&[(1, 9), (4, 3)], &[2]);
        h.kms(&rq).unwrap();
        h.kms(&rq).unwrap();
        let lookups = h
            .sys
            .log()
            .iter()
            .filter(|l| l.starts_with("getprop"))
            .count();
        assert_eq!(lookups, 2);
    }

    // ── scanout sources (S-6) ──

    fn setcrtc(fb: u32) -> Rq {
        Rq::new(iowr(0xa2, 104)).buf(104, Some(&arg(104, &[(16, 4, fb as u64)])))
    }

    fn setplane(fb: u32) -> Rq {
        Rq::new(iowr(0xb7, 48)).buf(48, Some(&arg(48, &[(8, 4, fb as u64)])))
    }

    fn page_flip(fb: u32) -> Rq {
        Rq::new(iowr(0xb0, 24)).buf(24, Some(&arg(24, &[(4, 4, fb as u64)])))
    }

    fn obj_setprop(prop: u32, value: u64) -> Rq {
        let a = arg(24, &[(0, 8, value), (8, 4, prop as u64)]);
        Rq::new(iowr(0xba, 24)).buf(24, Some(&a))
    }

    fn reached_host(h: &H) -> bool {
        h.sys.log().iter().any(|l| l.starts_with("ioctl"))
    }

    #[test]
    fn every_legacy_scanout_call_refuses_a_framebuffer_this_vm_never_made() {
        let h = h();
        with_props(&h);
        h.sys.log();
        for rq in [setcrtc(43), setplane(43), page_flip(43), obj_setprop(1, 43)] {
            assert_eq!(h.kms(&rq).unwrap().ret, -libc::EPERM, "{:#x}", rq.cmd);
            assert!(!reached_host(&h), "{:#x} never reaches the host", rq.cmd);
        }
        h.kms.as_ref().unwrap().add_fb(42);
        for rq in [setcrtc(42), setplane(42), page_flip(42), obj_setprop(1, 42)] {
            assert_eq!(h.kms(&rq).unwrap().ret, 0, "{:#x}", rq.cmd);
            assert!(reached_host(&h));
        }
    }

    #[test]
    fn no_framebuffer_and_setcrtcs_keep_the_current_one_always_pass() {
        let h = h();
        with_props(&h);
        for rq in [
            setcrtc(0),
            setcrtc(u32::MAX),
            setplane(0),
            obj_setprop(1, 0),
        ] {
            assert_eq!(h.kms(&rq).unwrap().ret, 0, "{:#x}", rq.cmd);
        }
        // -1 means "keep" only to SETCRTC; elsewhere it is just an id.
        assert_eq!(h.kms(&setplane(u32::MAX)).unwrap().ret, -libc::EPERM);
    }

    #[test]
    fn an_atomic_fb_id_must_be_a_framebuffer_this_vm_made() {
        let h = h();
        with_props(&h);
        h.kms.as_ref().unwrap().add_fb(42);
        h.sys.k().props.insert(5, "WRITEBACK_FB_ID");
        h.sys.log();
        for props in [&[(1, 43)][..], &[(4, 3), (1, 42), (1, 43)], &[(5, 43)]] {
            let rq = atomic(props, &[props.len() as u32]);
            assert_eq!(h.kms(&rq).unwrap().ret, -libc::EPERM, "{props:?}");
            assert!(!reached_host(&h));
        }
        // The kernel reads the id as a u32 (drm_framebuffer_lookup), so a
        // foreign id cannot hide in the high half either.
        let wide = atomic(&[(1, (1 << 32) | 43)], &[1]);
        assert_eq!(h.kms(&wide).unwrap().ret, -libc::EPERM);
        let ok = atomic(&[(1, 42), (1, 0), (5, 42)], &[3]);
        assert_eq!(h.kms(&ok).unwrap().ret, 0);
    }

    #[test]
    fn a_framebuffer_any_file_of_the_vm_made_is_a_source_until_that_file_retires() {
        let mut h = h();
        let vm = Arc::new(VmKms::new());
        let other = KmsFileState::in_vm(vm.clone());
        other.add_fb(42);
        h.kms = Some(Arc::new(KmsFileState::in_vm(vm.clone())));
        assert_eq!(h.kms(&page_flip(42)).unwrap().ret, 0);
        assert!(!h.owns(42), "a source, but not this file's to GETFB");
        other.retire();
        assert!(!vm.made_here(42));
        assert_eq!(h.kms(&page_flip(42)).unwrap().ret, -libc::EPERM);
        // A call still finishing on the retired file records nothing.
        other.add_fb(44);
        assert!(!vm.made_here(44));
    }

    /// A session reset while a KMS call is still running: the call
    /// finishes on a retired file and records nothing, even though the
    /// reset forgot every record; and whatever a file did record goes with
    /// its last reference.
    #[test]
    fn a_call_finishing_after_a_reset_records_no_blob_or_framebuffer() {
        let vm = Arc::new(VmKms::new());
        let f = KmsFileState::in_vm(vm.clone());
        f.retire();
        vm.clear();
        f.add_blob(5);
        f.saw_blobs(31, &[(7, 6)]);
        f.add_fb(44);
        assert!(!vm.blob_readable(5) && !vm.blob_readable(6));
        assert!(!vm.made_here(44));

        let g = KmsFileState::in_vm(vm.clone());
        g.add_blob(8);
        g.saw_blobs(31, &[(7, 9)]);
        assert!(vm.blob_readable(8) && vm.blob_readable(9));
        drop(g);
        assert!(!vm.blob_readable(8) && !vm.blob_readable(9));
    }

    #[test]
    fn a_framebuffer_leaves_the_vms_sources_before_rmfb_runs_and_returns_if_it_fails() {
        let h = h();
        let k = h.kms.as_ref().unwrap().clone();
        k.add_fb(42);
        let rmfb = Rq::new(iowr(0xaf, 4)).buf(4, Some(&[42, 0, 0, 0]));
        let seen = Arc::new(Mutex::new(None));
        let (k2, seen2) = (k.clone(), seen.clone());
        h.sys.on_ioctl(move |_, _, _, _| {
            *seen2.lock().unwrap() = Some(k2.may_scan_out(42));
            -libc::ENOENT
        });
        assert_eq!(h.kms(&rmfb).unwrap().ret, -libc::ENOENT);
        assert_eq!(
            *seen.lock().unwrap(),
            Some(false),
            "gone while the host ran it"
        );
        assert!(h.owns(42), "and back, since the host kept it");
        h.sys.on_ioctl(|_, _, _, _| 0);
        assert_eq!(h.kms(&rmfb).unwrap().ret, 0);
        assert!(!k.may_scan_out(42));
    }

    /// A PAGE_FLIP of the owner's framebuffer, on another file, held in the
    /// host after its check; the file whose framebuffer it is, and a guard
    /// that lets it go on.
    struct InFlight {
        owner: Arc<KmsFileState>,
        vm: Arc<VmKms>,
        go: std::sync::mpsc::Sender<()>,
        flip: std::thread::JoinHandle<i32>,
    }

    fn flip_in_flight(h: &mut H, fb: u32) -> InFlight {
        let vm = Arc::new(VmKms::new());
        let owner = Arc::new(KmsFileState::in_vm(vm.clone()));
        owner.add_fb(fb);
        h.kms = Some(Arc::new(KmsFileState::in_vm(vm.clone())));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (go, go_rx) = std::sync::mpsc::channel::<()>();
        let go_rx = Mutex::new(go_rx);
        let flip = iowr(0xb0, 24);
        h.sys.before_ioctl(move |cmd| {
            if cmd == flip {
                entered_tx.send(()).unwrap();
                let _ = go_rx.lock().unwrap().recv();
            }
        });
        let mut p = h.prepare(SchemaClass::Kms, KMS, &page_flip(fb)).unwrap();
        let fd = h.target_fds[&KMS].as_raw_fd();
        let flip = std::thread::spawn(move || p.execute(fd));
        entered_rx.recv().unwrap();
        InFlight {
            owner,
            vm,
            go,
            flip,
        }
    }

    /// S-6's check and the host's lookup are not one step: an RMFB of the
    /// same id on another executor in between freed it, and the kernel
    /// hands the lowest free id to the next framebuffer anyone on the host
    /// makes -- which the flip would then show. The RMFB waits for the
    /// flip.
    #[test]
    fn a_framebuffer_a_call_in_flight_named_is_not_removed_under_it() {
        let mut h = h();
        let f = flip_in_flight(&mut h, 42);
        let owner = H {
            sys: h.sys.clone(),
            hooks: h.hooks.clone(),
            kms: Some(f.owner.clone()),
            version: None,
            target_fds: HashMap::new(),
        };
        let rmfb = Rq::new(iowr(0xaf, 4)).buf(4, Some(&[42, 0, 0, 0]));
        let mut p = owner.prepare(SchemaClass::Kms, OTHER_KMS, &rmfb).unwrap();
        let fd = h.target_fds[&OTHER_KMS].as_raw_fd();
        let rm = std::thread::spawn(move || p.execute(fd));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let removed = format!("ioctl {OTHER_KMS} {:#x}", iowr(0xaf, 4));
        assert!(
            !h.sys.log().contains(&removed),
            "the RMFB waits while the flip that named 42 runs"
        );
        assert!(!f.vm.made_here(42), "and nothing new may name it meanwhile");
        f.go.send(()).unwrap();
        assert_eq!(f.flip.join().unwrap(), 0);
        assert_eq!(rm.join().unwrap(), 0);
        assert!(h.sys.log().contains(&removed));
    }

    /// The same for the close of the file that made it: the host frees a
    /// file's framebuffers when it closes, so the file closes only once the
    /// flip is done.
    #[test]
    fn the_file_that_made_a_framebuffer_a_call_in_flight_named_closes_after_it() {
        struct Closed(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Closed {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let mut h = h();
        let f = flip_in_flight(&mut h, 42);
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fbs = f.owner.retire();
        assert_eq!(fbs, vec![42]);
        f.vm.close_after(fbs, Box::new(Closed(closed.clone())));
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert!(!closed.load(std::sync::atomic::Ordering::SeqCst), "parked");
        f.go.send(()).unwrap();
        f.flip.join().unwrap();
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
        // A file whose framebuffers nothing names closes at once.
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        f.vm.close_after(vec![42], Box::new(Closed(closed.clone())));
        assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
        assert!(closed.load(std::sync::atomic::Ordering::SeqCst));
    }

    // ── property blobs ──

    fn getpropblob(id: u32) -> Rq {
        Rq::new(iowr(0xac, 16)).buf(16, Some(&arg(16, &[(0, 4, id as u64)])))
    }

    fn obj_getproperties(obj: u32, count: u32) -> Rq {
        let a = arg(
            32,
            &[
                (0, 8, 0x100),
                (8, 8, 0x200),
                (16, 4, count as u64),
                (20, 4, obj as u64),
            ],
        );
        Rq::new(iowr(0xb9, 32))
            .buf(32, Some(&a))
            .buf(4 * count, None)
            .buf(8 * count, None)
    }

    /// Blob ids are the device's and a lease does not cover them: any other
    /// VM's MODE_ID or damage clips, or the host desktop's, read by number.
    /// A blob is readable when this VM made it, or the host reported it as
    /// the value of a blob property of an object this VM sees -- what
    /// drm_info and modetest read, MODE_ID of their own CRTC.
    #[test]
    fn getpropblob_reads_only_blobs_this_vm_made_or_sees() {
        let h = h();
        {
            let mut k = h.sys.k();
            k.props.insert(20, "MODE_ID");
            k.prop_flags.insert(20, DRM_MODE_PROP_BLOB);
            k.props.insert(21, "ACTIVE");
        }
        let blob_read = |h: &H, id| {
            h.sys.log();
            let r = h.kms(&getpropblob(id)).unwrap().ret;
            (r, reached_host(h))
        };
        assert_eq!(
            blob_read(&h, 66),
            (-libc::ENOENT, false),
            "another tenant's"
        );

        // OBJ_GETPROPERTIES of our CRTC 5: MODE_ID is blob 66, ACTIVE is 67.
        h.sys.on_ioctl(|_, _, cmd, arg| {
            let top = arg.addr();
            match cmd & 0xff {
                0xb9 => {
                    let (ids, vals) = (peek(arg, top, 0, 8), peek(arg, top, 8, 8));
                    for (i, (p, v)) in [(20u64, 66u64), (21, 67)].into_iter().enumerate() {
                        arg.poke(ids + 4 * i as u64, 4, p);
                        arg.poke(vals + 8 * i as u64, 8, v);
                    }
                    poke(arg, top, 16, 4, 2);
                    0
                }
                // CREATEPROPBLOB: blob 70.
                0xbd => {
                    poke(arg, top, 12, 4, 70);
                    0
                }
                _ => 0,
            }
        });
        assert_eq!(h.kms(&obj_getproperties(5, 2)).unwrap().ret, 0);
        assert_eq!(blob_read(&h, 66), (0, true), "MODE_ID of our own CRTC");
        assert_eq!(
            blob_read(&h, 67),
            (-libc::ENOENT, false),
            "a value of a property that is no blob"
        );

        // One of our own, until it is destroyed.
        let data = [7u8; 8];
        let create = Rq::new(iowr(0xbd, 16))
            .buf(16, Some(&arg(16, &[(0, 8, 0x100), (8, 4, 8)])))
            .buf(8, Some(&data));
        assert_eq!(h.kms(&create).unwrap().ret, 0);
        assert_eq!(blob_read(&h, 70), (0, true));
        let destroy = Rq::new(iowr(0xbe, 4)).buf(4, Some(&70u32.to_le_bytes()));
        assert_eq!(h.kms(&destroy).unwrap().ret, 0);
        assert_eq!(blob_read(&h, 70), (-libc::ENOENT, false));
        // And what a file made or saw goes with it.
        h.kms.as_ref().unwrap().retire();
        assert!(!h.kms.as_ref().unwrap().vm().blob_readable(66));
    }

    /// A blob property may be set only to none or a blob this VM made or
    /// sees: another tenant's MODE_ID committed to our own CRTC would be
    /// reported back by OBJ_GETPROPERTIES and read by GETPROPBLOB, and a
    /// TEST_ONLY commit alone tells whether an id exists.
    #[test]
    fn a_blob_property_takes_only_a_blob_this_vm_made_or_sees() {
        let h = h();
        {
            let mut k = h.sys.k();
            k.props.insert(20, "MODE_ID");
            k.prop_flags.insert(20, DRM_MODE_PROP_BLOB);
            k.props.insert(21, "ACTIVE");
        }
        let commit = |v: u64| h.kms(&atomic(&[(20, v), (21, 66)], &[2])).unwrap().ret;
        assert_eq!(commit(66), -libc::EPERM, "another tenant's blob");
        assert_eq!(commit(1 << 32 | 70), -libc::EPERM);
        assert_eq!(commit(0), 0, "none");
        h.kms.as_ref().unwrap().add_blob(66);
        assert_eq!(commit(66), 0, "one of ours");
        // The legacy setter, value @0 and prop_id @8.
        let setprop = |v: u64| {
            let a = arg(
                24,
                &[(0, 8, v), (8, 4, 20), (12, 4, 5), (16, 4, 0xcccc_cccc)],
            );
            h.kms(&Rq::new(iowr(0xba, 24)).buf(24, Some(&a)))
                .unwrap()
                .ret
        };
        assert_eq!(setprop(67), -libc::EPERM);
        assert_eq!(setprop(66), 0);
    }

    // ── forced connector probes (S-8) ──

    fn getconnector(connector: u32) -> Rq {
        Rq::new(iowr(0xa7, 80)).buf(80, Some(&arg(80, &[(48, 4, connector as u64)])))
    }

    #[test]
    fn a_connector_probed_a_moment_ago_is_reported_without_a_probe() {
        let h = h();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        h.sys.on_ioctl(move |_, _, _, arg| {
            let top = arg.addr();
            let (count, ptr) = (peek(arg, top, 32, 4), peek(arg, top, 8, 8));
            log.lock().unwrap().push((count, ptr != 0));
            if count >= 1 && ptr != 0 {
                // One mode, and room for one: the kernel copies it.
                poke(arg, ptr, 0, 4, 0xdead);
            }
            poke(arg, top, 32, 4, 1);
            0
        });
        for _ in 0..2 {
            let r = h.kms(&getconnector(7)).unwrap();
            assert_eq!(r.ret, 0);
            assert_eq!(rd(&r.data, 32, 4), 1, "the real count, either way");
            assert_eq!(rd(&r.data, 8, 8), 0, "and the guest's own pointer");
        }
        // Another connector is its own.
        h.kms(&getconnector(8)).unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![(0, false), (1, true), (0, false)],
            "the second call asked for one mode into a buffer of ours: no probe"
        );
    }

    #[test]
    fn a_connector_may_be_probed_once_a_window_across_the_vm() {
        let vm = VmKms::new();
        let t0 = std::time::Instant::now();
        assert!(vm.may_probe(0, 7, t0));
        assert!(!vm.may_probe(0, 7, t0 + CONNECTOR_PROBE_EVERY / 2));
        assert!(
            vm.may_probe(1, 7, t0),
            "another card's connector 7 is another"
        );
        assert!(vm.may_probe(0, 7, t0 + CONNECTOR_PROBE_EVERY));
    }

    /// Connector ids are the guest's: every GETCONNECTOR of a made-up one
    /// used to leave a record behind, and the map grew without bound. A
    /// probe the host refuses keeps none, and the records are bounded.
    #[test]
    fn a_probe_of_a_connector_the_host_refuses_leaves_no_record() {
        let h = h();
        h.sys.on_ioctl(|_, _, _, arg| {
            let top = arg.addr();
            if peek(arg, top, 48, 4) == 7 {
                0
            } else {
                -libc::ENOENT
            }
        });
        for c in 1000..1100 {
            assert_eq!(h.kms(&getconnector(c)).unwrap().ret, -libc::ENOENT);
        }
        assert_eq!(h.kms(&getconnector(7)).unwrap().ret, 0);
        let vm = h.kms.as_ref().unwrap().vm();
        assert_eq!(
            vm.probes_recorded(),
            1,
            "only the connector the host served"
        );

        let vm = VmKms::new();
        let t0 = std::time::Instant::now();
        for c in 0..PROBES_KEPT as u32 {
            assert!(vm.may_probe(0, c, t0));
        }
        assert!(!vm.may_probe(0, 9999, t0), "full of this window's probes");
        let later = t0 + CONNECTOR_PROBE_EVERY;
        assert!(vm.may_probe(0, 9999, later), "the stale ones make room");
        assert_eq!(vm.probes_recorded(), 1);
    }

    // ── NVKMS ──

    fn nvkms(cmd: u32, size: u32) -> Rq {
        let outer = arg(
            16,
            &[(0, 4, cmd as u64), (4, 4, size as u64), (8, 8, 0xa000)],
        );
        let mut rq = Rq::new(schema::NVKMS_IOCTL_IOWR);
        rq.render = MODESET;
        rq.buf(16, Some(&outer))
    }

    #[test]
    fn an_nvkms_call_is_keyed_by_the_command_inside_and_sized_by_it() {
        let mut h = h();
        let params = arg(44, &[(0, 4, 1), (4, 4, 2), (8, 4, 3)]);
        let rq = nvkms(3, 44).buf(44, Some(&params));
        let modeset = |h: &H, rq: &Rq| h.run(SchemaClass::Modeset, MODESET, rq);
        assert_eq!(
            modeset(&h, &rq).err(),
            Some(libc::ENOTTY),
            "no table for this host"
        );
        h.version = Some(DriverVersion::new(610, 57, 4));
        h.sys.on_ioctl(|_, _, cmd, arg| {
            let top = arg.addr();
            assert_eq!(cmd, schema::NVKMS_IOCTL_IOWR);
            assert_eq!(peek(arg, top, 4, 4), 44);
            let p = peek(arg, top, 8, 8);
            assert_eq!(peek(arg, p, 8, 4), 3, "the request half is the guest's");
            poke(arg, p, 12, 4, 0x5555);
            0
        });
        let r = modeset(&h, &rq).unwrap();
        assert_eq!(r.ret, 0);
        assert_eq!(r.data.len(), 48, "the outer struct is never returned");
        assert_eq!(rd(&r.data, 12, 4), 0x5555);
        let wrong_size = nvkms(3, 40).buf(40, Some(&[0; 40]));
        assert_eq!(modeset(&h, &wrong_size).err(), Some(libc::EINVAL));
        // 35 has no dispatch entry in any release, so no table has it.
        let unknown = nvkms(35, 44).buf(44, Some(&params));
        assert_eq!(modeset(&h, &unknown).err(), Some(libc::ENOTTY));
        let mut not_nvkms = rq.clone();
        not_nvkms.cmd = iowr(0, 16);
        assert_eq!(modeset(&h, &not_nvkms).err(), Some(libc::ENOTTY));
    }

    // ── odds and ends ──

    /// The walk declares every pointer, descriptor and GEM field in the
    /// call's arena, and the arena refuses a field over another: so no two
    /// such fields of one struct (arrays spelled out) may overlap unless
    /// their conditions cannot both hold -- in every table, every release.
    #[test]
    fn no_two_declared_fields_of_any_schema_overlap() {
        fn declared(k: &Kind) -> bool {
            matches!(
                k,
                Kind::Ptr { .. }
                    | Kind::FdIn { .. }
                    | Kind::FdOut { .. }
                    | Kind::GemIn { .. }
                    | Kind::GemOut
            )
        }
        fn exclusive(a: Option<schema::Cond>, b: Option<schema::Cond>) -> bool {
            match (a, b) {
                (Some(a), Some(b)) if a.off == b.off && a.mask == b.mask => {
                    (!a.ne && !b.ne && a.value != b.value) || (a.value == b.value && a.ne != b.ne)
                }
                _ => false,
            }
        }
        /// (offset, width, condition) of each declared field of `span` at
        /// `base`, arrays walked element by element.
        fn fields(
            t: &Table,
            span: Span,
            base: u32,
            out: &mut Vec<(u32, u32, Option<schema::Cond>, &'static str)>,
        ) {
            for f in t.fields(span) {
                if let Kind::Array {
                    count,
                    stride,
                    children,
                    ..
                } = f.kind
                {
                    for e in 0..count {
                        fields(t, children, base + f.off + e * stride, out);
                    }
                } else if declared(&f.kind) {
                    out.push((base + f.off, f.width(), f.cond, f.name));
                }
            }
        }
        fn check(t: &Table, span: Span, what: &str) {
            let mut fs = Vec::new();
            fields(t, span, 0, &mut fs);
            for (i, a) in fs.iter().enumerate() {
                for b in &fs[i + 1..] {
                    let overlap = a.0 < b.0 + b.1 && b.0 < a.0 + a.1;
                    assert!(
                        !overlap || exclusive(a.2, b.2),
                        "{}: {what}: {} and {} overlap",
                        t.name,
                        a.3,
                        b.3
                    );
                }
            }
            for f in t.fields(span) {
                if let Kind::Ptr { children, .. } = f.kind
                    && children.len > 0
                {
                    check(t, children, f.name);
                }
            }
        }
        let mut tables = vec![schema::DRM_TABLE];
        tables.extend(abi::schema::MODESET_TABLES.iter().copied());
        for t in tables {
            for e in t.ioctls {
                check(t, e.fields, e.name);
            }
        }
    }

    #[test]
    fn a_call_without_an_argument_hands_the_host_null() {
        let h = h();
        h.sys.on_ioctl(|_, _, _, arg| {
            assert_eq!(arg.addr(), 0);
            0
        });
        let rq = Rq::new(0x0000_6444).buf(0, None);
        let r = h.run(SchemaClass::Render, RENDER, &rq).unwrap();
        assert_eq!((r.ret, r.data.len()), (0, 0));
    }

    #[test]
    fn render_calls_run_inline_and_kms_calls_on_the_executor() {
        let h = h();
        let dev_info = Rq::new(0xc024_6443).buf(36, Some(&[0; 36]));
        let p = h.prepare(SchemaClass::Render, RENDER, &dev_info).unwrap();
        assert!(!p.wants_executor());
        // GET_DPY_ID_FOR_CONNECTOR_ID walks the connector list under
        // mode_config locks, render node or not.
        let dpy = Rq::new(0xc008_6450).buf(8, Some(&[0; 8]));
        let p = h.prepare(SchemaClass::Render, RENDER, &dpy).unwrap();
        assert!(p.wants_executor());
        // Scanout checksums are no render-node call here at all: from a
        // render node they would find every host CRTC (M-7).
        let crc = Rq::new(0xc008_6440).buf(8, Some(&[0; 8]));
        assert_eq!(
            h.prepare(SchemaClass::Render, RENDER, &crc).err(),
            Some(libc::ENOTTY)
        );
        assert!(
            h.prepare(SchemaClass::Kms, KMS, &crc)
                .unwrap()
                .wants_executor()
        );
        let enc = Rq::new(iowr(0xa6, 20)).buf(20, Some(&[0; 20]));
        assert!(
            h.prepare(SchemaClass::Kms, KMS, &enc)
                .unwrap()
                .wants_executor()
        );
    }

    #[test]
    fn a_render_call_creating_handles_must_name_its_own_file_as_render() {
        let h = h();
        let mut rq = Rq::new(0xc018_644b).buf(24, Some(&[0; 24]));
        rq.render = OTHER_KMS;
        assert_eq!(
            h.run(SchemaClass::Render, RENDER, &rq).err(),
            Some(libc::EINVAL)
        );
    }
}
