// SPDX-License-Identifier: Apache-2.0
//! `NvidiaBackend`: the dispatcher every message a guest sends reaches, and
//! the state the backend keeps for a VM. Its parts:
//!
//! - `v1.rs`: the v1 IOCTL, from the request to the reply;
//! - `rm.rs`: RM escapes, their gates and their parameter blocks;
//! - `rmmap.rs`: RM mappings of device memory (MAP, UPDATE, UNMAP);
//! - `uvm.rs`: v1 UVM commands;
//! - `placement.rs`: MMAP and MUNMAP into the window and the UVM aperture;
//! - `hostnodes.rs`: the host's DRM nodes, and GET_PROC/SYS_FILES.
//!
//! Memory registered by its pages is `crate::osdesc`'s, the IOCTL2 and
//! HOST_OP messages `crate::session`'s, sharing `crate::rmshare`'s.

#![forbid(unsafe_code)]

mod hostnodes;
mod placement;
mod rm;
mod rmmap;
mod uvm;
mod v1;

use hostnodes::*;
#[cfg(fuzzing)]
pub(crate) use hostnodes::{DriDevice, NV_DEV_INFO_WORDS};
pub use hostnodes::{FileTree, host_render_names};
use placement::*;
use rm::*;
pub(crate) use rm::{FdAccept, FdField, FdIn, FdNone};
use v1::*;
pub(crate) use v1::{IoctlOut, V1};

use protocol::messages::*;
use std::ffi::CString;
#[cfg(any(test, fuzzing))]
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, BorrowedFd, RawFd};
use std::sync::Arc;

use crate::error::{DeviceError, Result};
use crate::handle_table::HandleTable;
use crate::hostfd::{self, CardNode, HandleKind};
use crate::kms_state::{KmsFileState, VmKms};
use crate::le;
use crate::nvkms::{self, NvkmsPolicy};
use crate::nvos::{self, *};
use crate::policy::BackendHooks;
use crate::privfd::PrivateFd;
use crate::pump::{PumpCmd, WatchMode};
use crate::semsurf::SemsurfPolicy;
use crate::session::{BackendConfig, MAX_XFER_DIRECT, Outcome, Reply, Session};
use crate::shm::{ShmAllocator, ZoneConfig};
use crate::sys::block::{Arena, BufId, Restore, SlotKind};
use crate::sys::pod;
use crate::xfer::{Hooks, Sys};

// ============================================================
// NvidiaBackend
// ============================================================

pub struct NvidiaBackend {
    /// The message being served, so a response can echo its type, and the
    /// handle it named, so handlers need not thread either through.
    pub(crate) current_msg: MsgType,
    pub(crate) current_handle: u32,
    /// The request's id, echoed in every response header. A v2 guest puts a
    /// unique one in each request; a v1 guest leaves it zero.
    pub(crate) current_req_id: u32,
    /// Top-level parameter length of the ioctl being served. The response has
    /// to split the bytes the same way the request did.
    current_data_len: u32,
    /// The guest process making the RM call being served, when the guest
    /// says (rmshare.rs).
    pub(crate) current_proc: Option<crate::rmshare::Caller>,
    /// The guest process what the message being served makes is charged to
    /// (quota.rs): the one an OPEN or HOST_OP names, else the owner of the
    /// handle the message acts on.
    pub(crate) current_owner: crate::quota::Owner,
    pub(crate) handles: HandleTable,
    shm: ShmAllocator,
    /// Active RM_MAP_MEMORY mappings, keyed by SHM offset.
    ///
    /// The SHM offset is written into pLinearAddress in the response to the
    /// guest, and userspace quotes it until UPDATE_DEVICE_MAPPING_INFO gives
    /// the mapping the virtual address it was mapped at, and that address
    /// after (in UPDATE and RM_UNMAP_MEMORY); an entry answers to both, for
    /// its own RM object and process only (`MmapContext::find`). No host
    /// address reaches the guest.
    ///
    /// Each entry owns its extent, and is the only record that does: the
    /// extent is released when RM_UNMAP_MEMORY succeeds, when the handle that
    /// carries the mapping closes, or when the session resets -- whichever
    /// comes first, and exactly once.
    pub(crate) active_maps: crate::mmap::MmapContext,
    /// Host driver version, as the transport read it from the driver
    /// (`set_host_driver_version`). Never learned from a guest: the string
    /// in CHECK_VERSION_STR's reply is the caller's in RM's relaxed mode.
    pub(crate) driver: Option<abi::version::DriverVersion>,
    /// ABI profile selected for `driver`, if one exists.
    abi: Option<&'static [abi::versions::IoctlEntry]>,
    /// Where device memory is placed so the guest can address it. `None` until
    /// the transport supplies one, and without it a mapping can be made on the
    /// host but never reached from the guest.
    window: Option<Box<dyn crate::shm::WindowPlacer>>,
    /// Window placements made for a DRM object, keyed by the node handle and
    /// the object's mmap offset on the host.
    ///
    /// Keyed by both because one open of a node holds many objects, and they
    /// are told apart only by that offset. Keyed at all because a buffer is
    /// mapped more than once -- the guest maps it, exports it, an importer maps
    /// it again -- and each placement costs a slice of a finite window.
    ///
    /// Only a lookup: the placement itself is owned by `live_maps`. An entry
    /// goes when its handle closes, so a later open that happens to reuse the
    /// offset cannot be handed a placement of someone else's object.
    dri_maps: std::collections::HashMap<(u32, u64), u32>,
    /// Every message this backend has served, by kind.
    ///
    /// Kept because "how often does the guest have to ask the host anything"
    /// is the question a benchmark of this design turns on, and counting log
    /// lines answers a different one -- what the log level happened to print.
    msg_counts: std::collections::BTreeMap<&'static str, u64>,
    /// Every live placement made without an RM record, by the id the guest
    /// quotes to take it back. Owns its extent; see [`LiveMap`].
    pub(crate) live_maps: std::collections::HashMap<u32, LiveMap>,
    /// Whether an ioctl the profile does not describe is refused or forwarded.
    abi_policy: AbiPolicy,
    /// Each GPU's whole PCI config space, by address, as the launcher
    /// snapshotted it (`--pci-config-dir`); see [`merge_pci_config`].
    pci_config: std::collections::HashMap<String, Vec<u8>>,
    /// Whether RM escapes pass with no host version set: only the unit
    /// tests' and fuzzers' fake RMs, which no release describes. Everything
    /// else refuses them (`unversioned_ok`).
    unversioned_for_test: bool,
    /// Every `RM_ALLOC` class and `RM_CONTROL` command a workload asked for,
    /// and how often.
    ///
    /// Narrowing these to what the pipeline uses is the step that actually
    /// reduces what a guest can reach in the host driver -- an unprivileged
    /// helper process contains a bug in *this* code, not one in NVIDIA's kernel
    /// module, and only fewer reachable commands helps with the second. A
    /// filter cannot be written from a guess, so this is the instrument that
    /// says what the set really is.
    ///
    /// Bounded (tally.rs): the keys are the guest's to choose.
    pub(crate) rm_classes: crate::tally::Tally,
    rm_controls: crate::tally::Tally,
    /// Which RM controls and classes reach the host at all (rmallow.rs).
    rmallow: crate::rmallow::RmAllow,
    /// Every ioctl forwarded, by namespace and number.
    ///
    /// There are three namespaces, not one, and that is the point of counting
    /// this way: NVIDIA's own escapes (`F`), the DRM node's (`d`) and
    /// modeset's (`m`). Only the first has an ABI table. A buffer-sharing run
    /// measured 1212 forwarded ioctls with *zero* RM allocations or controls
    /// among them -- so an allowlist written against RM alone would leave the
    /// path a compositor actually uses completely unfiltered.
    ioctls_by_ns: std::collections::BTreeMap<(char, u32), u64>,
    /// Escapes that failed the check, and how often, so a run can say what a
    /// workload actually needed. Reported at teardown.
    abi_refused: std::collections::BTreeMap<u32, u64>,
    /// Instructions for the event pump, in the order they were made, until
    /// the transport collects them.
    ///
    /// The backend cannot run the pump itself: it holds no queue to deliver
    /// on, and this crate names no VMM. It says what to watch and when to
    /// stop; the transport forwards that to whoever owns the event queue.
    pub(crate) pump_cmds: Vec<PumpCmd>,
    /// Handles the message being served created, for the reply to carry.
    created: Vec<u32>,
    next_mapping_id: u32,
    pub(crate) session: Session,
    pub(crate) config: BackendConfig,
    /// Transport limits: what one request and one response may carry. The
    /// transport sets them once it knows whether indirect descriptors were
    /// negotiated.
    pub(crate) max_req: u32,
    pub(crate) max_resp: u32,
    nodes: Option<Arc<HostNodes>>,
    /// An already-signalled sync_file, made once and duplicated for every
    /// SIGNALED_SYNC_FILE. The backend's own, not a guest's: registered as
    /// private so no IOCTL2 can ever adopt its number.
    pub(crate) signaled: Option<PrivateFd>,
    /// Per-file KMS state of each `DrmCard`/`DrmLease` handle an IOCTL2 has
    /// run on: the framebuffers that file created (the only ones GETFB may
    /// return handles for) and its property names. Made on the first KMS
    /// call and dropped with the handle, so a later handle that happens to
    /// get the same number starts with nothing.
    pub(crate) kms_states: std::collections::HashMap<u32, Arc<KmsFileState>>,
    /// Every framebuffer those files made, VM-wide: the only ids a guest
    /// may name as a scanout source (S-6, `kms_state::KmsFileState`).
    pub(crate) vm_kms: Arc<VmKms>,
    /// The policy every IOCTL2 is checked against (see `policy.rs`).
    pub(crate) hooks: Arc<dyn Hooks>,
    /// Shared syncobj wait registrations (HOST_OP SYNCOBJ_WATCH, fence.rs).
    pub(crate) syncobj_regs: crate::fence::Registrations,
    /// Its NVKMS section's state, which also gates v1 NVKMS calls and hears
    /// of the host version, the mode and every handle closed (nvkms.rs).
    pub(crate) nvkms: Arc<NvkmsPolicy>,
    /// What SEMSURF_FENCE_CTX_CREATE and the OS-event fields of RM calls may
    /// name: the host's semaphore layout, this VM's RM clients and OS
    /// events, its live fence contexts (semsurf.rs). Fed here, read by the
    /// policy hooks too.
    pub(crate) semsurf: Arc<SemsurfPolicy>,
    /// The system calls IOCTL2 makes: the host's, except in tests that run
    /// whole calls against a fake kernel.
    pub(crate) xfer_sys: Arc<dyn Sys>,
    /// The host driver. The real one, except in tests that need to see what
    /// the host driver would be handed.
    host_ioctl: Box<dyn crate::sys::block::Kernel>,
    /// UVM files whose VA space came up with pageable access, or could not
    /// be shown not to (`uvm_pageable_off`): nothing more goes to them, and no
    /// other file's call may name them.
    uvm_refused: std::collections::HashSet<u32>,
    /// UVM semaphore pools the guest may map, and those placed in the UVM
    /// aperture (uvmmap.rs).
    pub(crate) uvm_maps: crate::uvmmap::UvmMaps,
    /// Wayland channels (`DEV_WAYLAND` handles) and what configures them.
    pub(crate) wl: crate::wl::WlState,
    /// What the RM handles a mapping can name are (system memory and its
    /// coherency, doorbell registers), and the rewrite that makes guest
    /// system memory GPU-coherent (rmmem.rs).
    pub(crate) rmmem: crate::rmmem::RmMem,
    /// Guest RAM, as the vhost-user memory table gives it: what an OS
    /// descriptor's page list is checked against and mapped from
    /// (osdesc.rs). None where there is none (the socket harness, tests
    /// that do not set one), and then no guest is offered BCAP_OS_DESC.
    pub(crate) guest_ram: Option<crate::osdesc::GuestRam>,
    /// Memory the guest registered with RM by its pages (osdesc.rs).
    pub(crate) osdesc: crate::osdesc::OsDesc,
    /// Host buffers a capture helper injected (`--inject-socket`), and the
    /// handles INJECT_OPEN made of them (inject.rs).
    pub(crate) inject: crate::inject::BackendInject,
    /// The injected objects' dma-bufs, which no export may hand out
    /// (inject.rs, `Taint`); shared with the IOCTL2 hooks.
    pub(crate) inject_taint: crate::inject::SharedTaint,
}

/// A fake host driver for tests: what the forwarding paths hand the host,
/// as the host sees it (sys/block.rs), and its answer (>= 0, or -errno).
#[cfg(any(test, fuzzing))]
pub(crate) type HostIoctl = fn(RawFd, u64, &mut crate::sys::block::Arg<'_>) -> i32;

/// How many bytes an ioctl argument buffer must have.
///
/// Every host driver behind these paths copies `_IOC_SIZE(cmd)` bytes in and
/// back out of the argument, whatever the caller allocated: drm_ioctl sizes
/// its copies by the command (drm_ioctl.c, `drm_ioctl`: `in_size = out_size =
/// _IOC_SIZE(cmd)`), and nvidia.ko's frontend does the same with `arg_size`.
/// A buffer sized to the guest's `data_len` with a larger size encoded in the
/// command let the host write past the end of our heap allocation -- a guest
/// choosing both numbers chose how far. So the buffer is the larger of the
/// two, zero-filled, and only `data_len` bytes of it go back.
pub(crate) fn ioctl_arg_len(cmd: u64, data_len: usize) -> usize {
    hostfd::ioc_size(cmd as u32).max(data_len).max(1)
}

impl NvidiaBackend {
    /// The host's copy of a guest's top-level block `bytes`: a guarded block
    /// of [`ioctl_arg_len`] bytes of `a` holding them, zeros after, with the
    /// fields `plan` names declared (guestptr.rs). A handle the plan names
    /// is resolved in this backend's table.
    fn top_block(
        &self,
        a: &mut Arena,
        request: u64,
        bytes: &[u8],
        plan: &crate::guestptr::Plan<'_>,
    ) -> std::result::Result<BufId, i32> {
        let top = a.block(bytes, ioctl_arg_len(request, bytes.len()))?;
        plan.declare(a, top, &|h| self.handles.get(h).map(|(fd, _)| fd))?;
        Ok(top)
    }

    /// Hand block `top` of `a` to the host driver as the argument of
    /// `request` on `fd`: its result, or the errno it failed with.
    pub(crate) fn host_call(
        &self,
        a: &mut Arena,
        fd: RawFd,
        request: u64,
        top: BufId,
    ) -> std::result::Result<i32, i32> {
        let r = a.call(self.host_ioctl.as_ref(), fd, request, top);
        if r < 0 { Err(-r) } else { Ok(r) }
    }
}

/// What to do with an ioctl the ABI profile does not vouch for.
///
/// Refusing is the default, and the reason is the whole point of having tables:
/// an escape that is not in them is one whose parameter layout we have never
/// seen, and forwarding it means handing the host driver bytes that nobody has
/// checked. `nvproxy`, whose tables these are derived from, has always refused;
/// this crate logged and forwarded anyway until it was asked, in public, what
/// exactly a guest can reach.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AbiPolicy {
    /// Refuse anything the profile does not describe.
    #[default]
    Enforce,
    /// Forward it anyway and count it. For finding out what a workload needs
    /// that the tables lack -- never for running one.
    Permissive,
}

/// The result of checking one guest ioctl against the host's ABI profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AbiCheck {
    /// The escape is known and the guest's parameter size matches.
    Ok,
    /// No profile: no host version was set, or none was measured at it.
    NoProfile,
    /// The escape is not in this driver's table.
    UnknownEscape,
    /// The escape is variable length; there is no size to check.
    VariableLength,
    /// The guest disagrees with the host ABI about this struct's size.
    SizeMismatch { expected: u32, actual: u32 },
}

impl NvidiaBackend {
    /// Create a backend with a custom SHM zone config.
    pub fn new(cfg: ZoneConfig) -> Self {
        let nvkms = Arc::new(NvkmsPolicy::new());
        let semsurf = Arc::new(SemsurfPolicy::new());
        let inject_taint = crate::inject::SharedTaint::default();
        Self {
            window: None,
            dri_maps: std::collections::HashMap::new(),
            msg_counts: std::collections::BTreeMap::new(),
            live_maps: std::collections::HashMap::new(),
            abi_policy: AbiPolicy::default(),
            unversioned_for_test: cfg!(any(test, fuzzing)),
            pci_config: std::collections::HashMap::new(),
            abi_refused: std::collections::BTreeMap::new(),
            rm_classes: crate::tally::Tally::default(),
            rm_controls: crate::tally::Tally::default(),
            rmallow: crate::rmallow::RmAllow::default(),
            ioctls_by_ns: std::collections::BTreeMap::new(),
            pump_cmds: Vec::new(),
            created: Vec::new(),
            next_mapping_id: 1,
            current_msg: MsgType::Ioctl,
            current_handle: 0,
            current_req_id: 0,
            current_data_len: 0,
            current_proc: None,
            current_owner: crate::quota::Owner::Unknown,
            handles: HandleTable::new(),
            shm: ShmAllocator::new(cfg),
            active_maps: crate::mmap::MmapContext::new(),
            driver: None,
            abi: None,
            session: Session::default(),
            config: BackendConfig::default(),
            max_req: MAX_XFER_DIRECT,
            max_resp: MAX_XFER_DIRECT,
            nodes: None,
            signaled: None,
            kms_states: std::collections::HashMap::new(),
            vm_kms: Arc::default(),
            syncobj_regs: crate::fence::Registrations::default(),
            hooks: BackendHooks::with_state(nvkms.clone(), semsurf.clone())
                .with_inject_taint(inject_taint.clone()),
            inject_taint,
            nvkms,
            semsurf,
            xfer_sys: Arc::new(crate::xfer::HostSys),
            host_ioctl: Box::new(crate::sys::ioctl::Host),
            uvm_refused: std::collections::HashSet::new(),
            uvm_maps: crate::uvmmap::UvmMaps::default(),
            wl: crate::wl::WlState::default(),
            rmmem: crate::rmmem::RmMem::default(),
            guest_ram: None,
            osdesc: crate::osdesc::OsDesc::default(),
            inject: crate::inject::BackendInject::default(),
        }
    }

    /// A backend whose shared window is `cfg`: the one `ZoneConfig` the
    /// VMM's GET_SHMEM_CONFIG is answered from too (`--window-size`,
    /// `--window-owner-share`).
    pub fn with_zone_config(cfg: ZoneConfig) -> Self {
        Self::new(cfg)
    }

    /// A backend with the default window (1 GiB, half a zone per process).
    pub fn with_default_zones() -> Self {
        Self::with_zone_config(ZoneConfig::default_1gib())
    }

    /// Total SHM BAR size (for VMM config space).
    pub fn shm_total_size(&self) -> u64 {
        self.shm.total_size()
    }

    /// Forward ioctls the ABI profile does not describe, instead of refusing
    /// them. Diagnostic only: it exists to find out what a workload needs.
    pub fn set_abi_policy(&mut self, policy: AbiPolicy) {
        if policy == AbiPolicy::Permissive {
            log::warn!(
                "ABI enforcement off: ioctls this build cannot describe will be \
                 forwarded to the host driver unchecked"
            );
        }
        self.abi_policy = policy;
    }

    /// Refuse the RM controls and classes the host release's allowlist lacks
    /// (the default), or only log them (`--rm-allowlist=log`; rmallow.rs).
    pub fn set_rm_allowlist(&mut self, mode: crate::rmallow::Mode) {
        self.rmallow.set_mode(mode);
    }

    /// Whether guest system memory is allocated GPU-coherent (the default;
    /// see rmmem.rs). Off leaves every allocation as the guest asked, which on
    /// an Intel host under KVM's default IGNORE_GUEST_PAT quirk means the
    /// guest caches memory the GPU does not snoop.
    pub fn set_guest_coherency(&mut self, coherent: bool) {
        if !coherent {
            log::warn!(
                "guest system memory keeps the coherency it asks for: on an Intel host the \
                 guest may read stale GPU data unless the VMM honours guest PAT"
            );
        }
        self.rmmem.set_coherent(coherent);
        self.nvkms.set_coherent_display(coherent);
    }

    /// What the backend was started with: compositor-VM mode, the Wayland
    /// sockets, which schemas exist.
    pub fn set_config(&mut self, config: BackendConfig) {
        self.config = config;
    }

    pub fn config(&self) -> &BackendConfig {
        &self.config
    }

    /// Mutable access, for code that learns a capability after start-up (the
    /// fence schemas setting `fences`, say).
    pub fn config_mut(&mut self) -> &mut BackendConfig {
        &mut self.config
    }

    /// How much one request and one response may carry. HELLO reports these
    /// to the guest, and the transport enforces them.
    pub fn set_transport_limits(&mut self, max_req: u32, max_resp: u32) {
        self.max_req = max_req;
        self.max_resp = max_resp;
    }

    /// Instructions for the event pump made since this was last called.
    ///
    /// A transport calls it after serving messages and forwards them in
    /// order. Draining rather than reading, so two transports cannot both
    /// think they own a watch.
    pub fn take_pump_cmds(&mut self) -> Vec<PumpCmd> {
        std::mem::take(&mut self.pump_cmds)
    }

    /// Give the backend somewhere to place device memory.
    ///
    /// Until this is called every `RM_MAP_MEMORY` still succeeds on the host --
    /// the mapping is real -- but the `mmap` that follows is refused, because
    /// there is no address in the guest that names it.
    pub fn set_window(&mut self, placer: Box<dyn crate::shm::WindowPlacer>) {
        self.window = Some(placer);
    }

    /// Guest RAM, from the vhost-user memory table. Every OS descriptor's
    /// page list is checked against, and mapped from, the table current when
    /// it arrives (osdesc.rs); a registration made from an older one keeps
    /// what it mapped. With none, no guest is offered BCAP_OS_DESC.
    pub fn set_guest_ram(&mut self, ram: Option<crate::osdesc::GuestRam>) {
        self.guest_ram = ram;
    }

    /// Whether a placer is attached (the transport's request channel is up).
    pub(crate) fn has_window(&self) -> bool {
        self.window.is_some()
    }

    /// Size the handle table for a backend that may hold `nofile`
    /// descriptors (posture::raise_nofile).
    pub fn set_nofile(&mut self, nofile: u64) {
        let limit = crate::handle_table::limit_for_nofile(nofile);
        log::info!("RLIMIT_NOFILE {nofile}: at most {limit} guest handles");
        self.handles.set_limit(limit);
    }

    pub fn handle_count(&self) -> usize {
        self.handles.len()
    }

    /// Whether `fd` is a descriptor a guest handle stands for.
    pub fn owns_fd(&self, fd: RawFd) -> bool {
        self.handles.owns_fd(fd)
    }

    /// Free bytes per SHM zone, as `(uc, wc, wb)`. For tests that assert a
    /// mapping cycle gives back exactly what it took.
    pub fn shm_free_bytes(&self) -> (u64, u64, u64) {
        self.shm.free_bytes()
    }

    /// The window's local backing (sys::mem::Window): where a test that is
    /// its own VMM places device memory, and reads it back.
    pub fn shm_window(&self) -> Arc<crate::sys::mem::Window> {
        self.shm.window()
    }

    /// Create a minimal backend suitable for unit tests (8-page total BAR).
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self::new(ZoneConfig {
            uc_size: 4096 * 2,
            wc_size: 4096 * 4,
            wb_size: 4096 * 2,
            owner_percent: 50,
        })
    }

    /// Put a descriptor straight into the table, as if an OPEN or an fd out
    /// had produced it.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn adopt_for_test(&mut self, fd: OwnedFd, kind: HandleKind) -> u32 {
        self.handles.insert(fd, kind).expect("test table has room")
    }

    /// `adopt_for_test`, as opened by guest process `owner`.
    #[cfg(test)]
    pub(crate) fn adopt_for_test_as(
        &mut self,
        fd: OwnedFd,
        kind: HandleKind,
        owner: crate::quota::Owner,
    ) -> u32 {
        self.handles
            .insert_for(fd, kind, owner)
            .expect("test table has room")
    }

    /// Replace host node enumeration, which needs real hardware.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn set_host_nodes_for_test(&mut self, dri: Vec<DriDevice>, cards: Vec<CardNode>) {
        self.nodes = Some(Arc::new(HostNodes { dri, cards }));
    }

    /// Replace the host ioctl, to see what a forwarding path hands the driver.
    #[cfg(any(test, fuzzing))]
    pub(crate) fn set_host_ioctl_for_test(&mut self, f: HostIoctl) {
        self.host_ioctl = Box::new(crate::sys::block::FnKernel(f));
    }

    // ------------------------------------------------------------------
    // Teardown
    //
    // Called by the transport once the VMM has gone: a VM that shut down,
    // or one that exited without shutting its device down (SIGKILL, crash).
    //
    // Draining the handle table closes every host fd, which triggers the
    // host NVIDIA driver's fd-release path and frees all RM objects.
    // Analogous to nvproxy's Release() in nvproxy.go.
    // ------------------------------------------------------------------

    pub fn teardown(&mut self) {
        log::info!(
            "NvidiaBackend::teardown: draining {} handles, {} active maps",
            self.handles.len(),
            self.active_maps.len()
        );
        // How much of the window this VM used: what `--window-size` and
        // `--window-owner-share` are chosen by (DEPLOY.md), so it is kept
        // at the default log level.
        log::warn!("window use: {}", self.shm.usage_summary());
        // What crossed the boundary, how often and how fast (pacing.rs):
        // what frame pacing is judged by, at the same level.
        crate::pacing::log_summary();
        if !self.abi_refused.is_empty() {
            let verb = if self.abi_policy == AbiPolicy::Enforce {
                "refused"
            } else {
                "forwarded unchecked"
            };
            log::warn!(
                "NvidiaBackend::teardown: {verb} {} ioctl(s) the ABI profile does not describe: {}",
                self.abi_refused.values().sum::<u64>(),
                self.abi_refused
                    .iter()
                    .map(|(e, n)| format!("{e:#04x}={n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        // The whole forwarded surface, by namespace. A filter has to cover all
        // of these, and today only 'F' has a table to check against at all.
        if !self.ioctls_by_ns.is_empty() {
            let mut by_ns: std::collections::BTreeMap<char, Vec<String>> = Default::default();
            for ((ns, nr), n) in &self.ioctls_by_ns {
                by_ns.entry(*ns).or_default().push(format!("{nr:#04x}={n}"));
            }
            for (ns, entries) in by_ns {
                let what = match ns {
                    'F' => "nvidia escapes",
                    'd' => "DRM ioctls",
                    'm' => "modeset ioctls",
                    'u' => "UVM ioctls",
                    _ => "unknown namespace",
                };
                log::info!(
                    "NvidiaBackend::teardown: {} {what} ({}): {}",
                    entries.len(),
                    ns,
                    entries.join(" ")
                );
            }
        }

        // The two sets a filter would be written from. Printed whole rather
        // than summarised: the long tail is the interesting part, because that
        // is where something a pipeline needs exactly once hides.
        // A bounded number of entries to a line, and the calls past the
        // tally's cap as one number (S-17).
        let report = |what: &str, tally: &crate::tally::Tally, key: fn(u32) -> String| {
            for line in tally.lines(key) {
                log::info!("NvidiaBackend::teardown: {} {what}: {line}", tally.len());
            }
            if tally.overflow() > 0 {
                log::info!(
                    "NvidiaBackend::teardown: and {} more {what} call(s) past the tally's {} keys",
                    tally.overflow(),
                    crate::tally::MAX_KEYS
                );
            }
        };
        report("RM_ALLOC class(es)", &self.rm_classes, |c| {
            format!("{c:#06x}")
        });
        report("RM_CONTROL command(s)", &self.rm_controls, |c| {
            format!("{c:#010x}")
        });
        self.rmallow.report();
        let total: u64 = self.msg_counts.values().sum();
        log::info!(
            "NvidiaBackend::teardown: served {total} message(s): {}",
            self.msg_counts
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        self.release_all();
    }

    /// Give back every placement and close every handle.
    ///
    /// Placements go first, and each is withdrawn from the window as well as
    /// freed: a guest process that exits without unmapping is the normal
    /// case, not an error -- most of the mappings in a captured trace are
    /// still live when the process ends -- and the window must not keep host
    /// device memory mapped into a range the next session will be handed.
    pub(crate) fn release_all(&mut self) {
        // UVM pools first, while their files are still open here: the VMM's
        // mapping may hold a file's last reference, and its teardown belongs
        // to this process's close, not to the VMM's munmap.
        for w in self.uvm_maps.take_all() {
            self.withdraw_uvm(w, "session end");
        }
        let live: Vec<LiveMap> = self.live_maps.drain().map(|(_, m)| m).collect();
        for m in live {
            self.release_extent(&m.region, m.length, "session end");
        }
        self.dri_maps.clear();
        for e in self.active_maps.drain() {
            self.release_extent(&e.region, e.shm_length, "session end");
        }
        self.rmmem.clear();
        self.wl_forget_all();
        self.syncobj_regs.clear();
        self.uvm_refused.clear();
        self.nvkms.reset();
        self.inject.reset();
        // Every UVM mapping of a registration is taken down, and every
        // client holding one freed on the file it was made on, before any
        // file closes (osdesc.rs); then the records go.
        for h in self.osdesc.uvm_files() {
            if let Ok(fd) = self.handles.get_raw(h) {
                self.osdesc_uvm_close(fd, h, "session end");
            }
        }
        let holders: Vec<u32> = self.osdesc.clients().into_iter().collect();
        for c in holders {
            let fd = self
                .semsurf
                .issuer_of(c)
                .and_then(|h| self.handles.get_raw(h).ok());
            if let Some(fd) = fd {
                self.osdesc_end_clients(fd, &[c], "session end");
            }
        }
        self.osdesc.clear();
        self.semsurf.reset();
        // Then every file, as a CLOSE ends it: what is left of it here goes
        // (the VM-wide records above are empty by now), a display file
        // closes on the closer thread, not under the backend mutex a reset
        // holds (S-33), and after any call in flight that names its
        // framebuffers (S-6).
        let handles = self.handles.handles();
        if !handles.is_empty() {
            log::info!("release_all: closing {} host file(s)", handles.len());
        }
        for h in handles {
            let _ = self.retire_handle(h, "session end");
        }
        for (_, k) in self.kms_states.drain() {
            k.retire();
        }
        self.vm_kms.clear();
    }

    // ------------------------------------------------------------------
    // Top-level dispatch
    // ------------------------------------------------------------------

    /// Serve one request whose response buffer holds `cap` bytes.
    ///
    /// Most messages are answered here and now. An IOCTL2 comes back as
    /// [`Outcome::Ioctl2`]: validated and holding everything it needs, to be
    /// executed with no backend lock held and then handed to
    /// [`NvidiaBackend::finish_ioctl2`].
    pub fn serve(&mut self, req_buf: &[u8], cap: usize) -> Outcome {
        #[cfg(test)]
        crate::fuzz_seeds::served(req_buf, cap);
        self.created.clear();
        // The mode is configuration, which the transport may set at any
        // point before the first message; the NVKMS policy reads its copy.
        self.nvkms.set_kms_card(self.config.kms_card);
        let Some(hdr) = pod::read::<MsgHeader>(req_buf, 0) else {
            self.current_msg = MsgType::Ioctl;
            self.current_req_id = 0;
            let r = self.error_reply(libc::EPROTO);
            return Outcome::Reply(self.fit(r, cap));
        };
        self.current_req_id = hdr.req_id;

        let Some(msg_type) = MsgType::from_u32(hdr.msg_type) else {
            log::warn!("unknown msg_type {}", hdr.msg_type);
            self.current_msg = MsgType::Ioctl;
            let r = self.error_reply(libc::EPROTO);
            return Outcome::Reply(self.fit(r, cap));
        };
        self.current_msg = msg_type;
        *self
            .msg_counts
            .entry(match msg_type {
                MsgType::Open => "open",
                MsgType::Close => "close",
                MsgType::Ioctl => "ioctl",
                MsgType::Mmap => "mmap",
                MsgType::Munmap => "munmap",
                MsgType::GetProcFiles => "get_proc_files",
                MsgType::GetSysFiles => "get_sys_files",
                MsgType::EventReady => "event_ready",
                MsgType::Hello => "hello",
                MsgType::Ioctl2 => "ioctl2",
                MsgType::TimeSync => "time_sync",
                MsgType::EventData => "event_data",
                MsgType::Watch => "watch",
                MsgType::Unwatch => "unwatch",
                MsgType::HostOp => "host_op",
                MsgType::WlSend => "wl_send",
                MsgType::WlRecv => "wl_recv",
            })
            .or_insert(0) += 1;
        // The handle travels in the header, not the payload -- every message
        // after Open acts on one, and Open's response returns one the same way.
        self.current_handle = hdr.handle;

        let payload = &req_buf[size_of::<MsgHeader>()..];
        // Who what this message makes is charged to (quota.rs). OPEN and
        // HOST_OP make handles out of nothing and say the process after
        // their fixed part; everything else acts on a handle, whose owner
        // it is.
        self.current_owner = match msg_type {
            MsgType::Open => crate::quota::Owner::from_trailer(
                payload,
                size_of::<OpenReq>(),
                self.session.proc_ids,
            ),
            MsgType::HostOp => crate::quota::Owner::from_trailer(
                payload,
                size_of::<protocol::messages::HostOpReq>(),
                self.session.proc_ids,
            ),
            _ => self.handles.owner(hdr.handle),
        };
        let reply = match msg_type {
            MsgType::Hello
            | MsgType::Ioctl2
            | MsgType::TimeSync
            | MsgType::Watch
            | MsgType::Unwatch
            | MsgType::HostOp => match self.serve_v2(msg_type, payload, cap) {
                Outcome::Reply(r) => r,
                pending => return pending,
            },
            MsgType::WlSend | MsgType::WlRecv => self.serve_wl(msg_type, payload, cap),
            _ => {
                // The v1 handlers write into a buffer of the response's size.
                // Zeroed, so nothing of an earlier response can reach the
                // guest through a short write.
                let mut buf = vec![0u8; cap.min(self.max_resp as usize)];
                let n = self.dispatch_v1(msg_type, payload, &mut buf);
                buf.truncate(n);
                Reply {
                    bytes: buf,
                    stamp_at: None,
                    created: std::mem::take(&mut self.created),
                }
            }
        };
        Outcome::Reply(self.fit(reply, cap))
    }

    /// `reply`, or if it is larger than the `cap` bytes the guest posted, a
    /// header saying so -- or nothing, if even that does not fit: no reply
    /// is ever longer than the buffer it goes to.
    fn fit(&mut self, mut reply: Reply, cap: usize) -> Reply {
        if reply.bytes.len() > cap {
            // Nothing the guest could read: say why in a header, if even that
            // fits, and take back whatever the message created.
            let created = std::mem::take(&mut reply.created);
            self.close_handles(&created);
            reply = self.error_reply(libc::EMSGSIZE);
            if reply.bytes.len() > cap {
                reply.bytes.clear();
            }
        }
        reply
    }

    /// Serve one request into a caller-supplied buffer, all the way through:
    /// an IOCTL2 is executed inline, a TIME_SYNC stamped at once.
    ///
    /// For callers that have no executor and no ring -- tests, and the
    /// socket harness. Returns the bytes written.
    pub fn dispatch(&mut self, req_buf: &[u8], resp_buf: &mut [u8]) -> usize {
        let mut reply = match self.serve(req_buf, resp_buf.len()) {
            Outcome::Reply(r) => r,
            Outcome::Ioctl2(mut p) => {
                p.execute();
                self.finish_ioctl2(p)
            }
        };
        reply.stamp();
        let n = reply.bytes.len().min(resp_buf.len());
        resp_buf[..n].copy_from_slice(&reply.bytes[..n]);
        n
    }

    fn dispatch_v1(&mut self, msg_type: MsgType, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        match msg_type {
            MsgType::Open => self.handle_open(payload, resp_buf),
            MsgType::Close => self.handle_close(payload, resp_buf),
            MsgType::Ioctl => self.handle_ioctl(payload, resp_buf),
            MsgType::Mmap => self.handle_mmap(payload, resp_buf),
            MsgType::Munmap => self.handle_munmap(payload, resp_buf),
            MsgType::GetProcFiles => self.handle_get_files(FileTree::Proc, resp_buf),
            MsgType::GetSysFiles => self.handle_get_files(FileTree::Sys, resp_buf),
            // Host to guest only. A guest that sends one is confused about the
            // direction of the queue, and saying so beats serving it.
            MsgType::EventReady | MsgType::EventData => {
                log::warn!(
                    "{msg_type:?} arrived from the guest; that message only travels outward"
                );
                self.write_error(resp_buf, libc::EINVAL)
            }
            _ => self.write_error(resp_buf, libc::EPROTO),
        }
    }

    // ------------------------------------------------------------------
    // OPEN
    // ------------------------------------------------------------------

    fn handle_open(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let Some(req) = pod::read::<OpenReq>(payload, 0) else {
            return self.write_error(resp_buf, libc::ENODEV);
        };

        let kind = match DeviceKind::from_device_type(req.device_type) {
            // Cards are reached only through HOST_OP OPEN_KMS, which ties the
            // host card file to a guest file's render handle and exists only
            // in compositor-VM mode. An OPEN of one would hand a guest a
            // file that is DRM master of the host's display.
            Some(DeviceKind::DriCard(n)) => {
                log::warn!("OPEN of card {n} refused: cards are opened through HOST_OP OPEN_KMS");
                return self.write_error(resp_buf, libc::EPERM);
            }
            Some(DeviceKind::Dri(n)) => HandleKind::DriRender(n),
            // UVM is compute's alone (`--allow-compute`, session.rs): without
            // it neither device is opened on the host, so no UVM command,
            // sharing mode or aperture placement is reachable at all.
            Some(k @ (DeviceKind::Uvm | DeviceKind::UvmTools)) if !self.config.allow_compute => {
                log::warn!("OPEN of {k:?} refused: UVM is served only with --allow-compute");
                return self.write_error(resp_buf, libc::ENODEV);
            }
            // A channel to the host compositor: not a path (wl/serve.rs).
            Some(DeviceKind::Wayland) => {
                return match self.open_wayland(req.flags) {
                    Ok(h) => {
                        self.created.push(h);
                        self.write_hdr(resp_buf, h, 0)
                    }
                    Err(e) => self.write_error(resp_buf, e),
                };
            }
            Some(k) => HandleKind::Dev(k),
            None => {
                log::warn!("handle_open: invalid device type {}", req.device_type);
                return self.write_error(resp_buf, libc::ENODEV);
            }
        };

        if kind == HandleKind::Dev(DeviceKind::Modeset)
            && let Some(why) = self.modeset_open_refused(self.current_owner)
        {
            log::warn!("OPEN of /dev/nvidia-modeset refused: {why}");
            return self.write_error(resp_buf, libc::EMFILE);
        }
        let nodes = self.host_nodes();
        let path = match device_path_with(req.device_type, &nodes.dri) {
            Ok(p) => p,
            Err(e) => {
                log::warn!("handle_open: {}", e);
                return self.write_error(resp_buf, libc::ENODEV);
            }
        };

        // Never O_NONBLOCK, whatever the guest asked: nvidia-modeset skips
        // poll_wait for a non-blocking file (nvidia-modeset-linux.c:2023-
        // 2025), so the pump would never hear of its events. The guest keeps
        // its own O_NONBLOCK and applies it locally.
        let fd = match crate::sys::fd::open(&path, libc::O_RDWR | libc::O_CLOEXEC) {
            Ok(fd) => fd,
            Err(err) => {
                let errno = err.raw_os_error().unwrap_or(0);
                log::warn!("open({:?}) failed: {}", path, err);
                return self.write_error(resp_buf, errno);
            }
        };
        let raw_fd = fd.as_raw_fd();
        // The guest may wait on this descriptor, and only the host's copy ever
        // becomes readable. The pump gets a duplicate of its own, so the watch
        // cannot outlive this table's fd by watching a reused number.
        let watch = match kind {
            HandleKind::Dev(_) => fd.try_clone().ok(),
            _ => None,
        };
        let guest_handle = match self.handles.insert_for(fd, kind, self.current_owner) {
            Ok(h) => h,
            Err(full) => {
                log::warn!("open {:?}: handle table full", path);
                return self.write_error(resp_buf, full.errno());
            }
        };
        if let Some(fd) = watch {
            // Once per guest wait for a guest that arms RM readiness
            // (BCAP_ARMED_READY); every host event otherwise.
            let mode = if self.session.armed_ready && kind.readiness_is_armed() {
                WatchMode::LegacyArmed
            } else {
                WatchMode::Legacy
            };
            self.pump_cmds.push(PumpCmd::Watch {
                handle: guest_handle,
                fd,
                mode,
            });
        }
        self.created.push(guest_handle);
        log::debug!("open {:?} -> handle={guest_handle} (fd={raw_fd})", path);
        if let HandleKind::DriRender(dri) = kind {
            self.semsurf_render_opened(guest_handle, dri);
        }

        // The handle is returned in the header. The driver reads it from there
        // and there is no response payload at all.
        self.write_hdr(resp_buf, guest_handle, 0)
    }

    /// Why guest process `owner` may not open another `/dev/nvidia-modeset`,
    /// if it may not. Every one is a host NVKMS open with an event list NVKMS
    /// never bounds (nvkms.c:6422-6435) and permission state of its own: at
    /// most [`nvkms::MAX_MODESET_OPENS`] per VM, and a process's share of
    /// them ([`nvkms::MODESET_SHARE`], B4).
    pub(crate) fn modeset_open_refused(&self, owner: crate::quota::Owner) -> Option<String> {
        let kind = HandleKind::Dev(DeviceKind::Modeset);
        // Files still closing are still open on the host: with the closer
        // stalled on a modeset, one process looping open/close held far
        // more than the cap (review 2026-09-29 1.12).
        let (closing, closing_mine) = self.handles.closing_modesets(owner);
        let (mut open, mut mine) = (closing as usize, closing_mine as usize);
        for h in self.handles.handles() {
            if self.handles.kind(h) == Some(kind) {
                open += 1;
                if self.handles.owner(h) == owner {
                    mine += 1;
                }
            }
        }
        if open >= nvkms::MAX_MODESET_OPENS {
            return Some(format!("{open} already open, the most one VM may hold"));
        }
        crate::quota::admits(
            &nvkms::MODESET_SHARE,
            owner,
            mine as u64,
            1,
            open as u64,
            nvkms::MAX_MODESET_OPENS as u64,
        )
        .err()
        .map(|why| format!("guest process {owner:?} holds {mine} of the VM's {open} ({why:?})"))
    }

    fn current_kind(&self) -> Option<HandleKind> {
        self.handles.kind(self.current_handle)
    }

    // ------------------------------------------------------------------
    // CLOSE
    // ------------------------------------------------------------------

    fn handle_close(&mut self, payload: &[u8], resp_buf: &mut [u8]) -> usize {
        let _ = payload;
        match self.close_handle(self.current_handle) {
            Ok(()) => self.write_hdr(resp_buf, 0, 0),
            Err(_) => self.write_error(resp_buf, libc::EBADF),
        }
    }

    /// Close one handle, and everything that exists only because of it.
    ///
    /// RM mappings go with the file that carries them. For some clients that
    /// is the only release there is -- a CUDA run maps 29 times and never
    /// unmaps once -- so leaving them to teardown would cost the write-combine
    /// zone tens of megabytes for the life of the VM.
    ///
    /// Window placements of DRM objects do *not* go: a GEM proxy in another
    /// guest file may still have one mapped (see [`LiveMap`]). Only the lookup
    /// that would hand the placement to a new mmap on this handle goes.
    pub(crate) fn close_handle(&mut self, handle: u32) -> Result<()> {
        self.retire_handle(handle, "close")
    }

    /// Everything a handle's end does, whatever ends it (`why`): a CLOSE, or
    /// the session's (`release_all`), which does the VM-wide parts first.
    fn retire_handle(&mut self, handle: u32, why: &str) -> Result<()> {
        let owner = self.handles.owner(handle);
        let (fd, kind) = self.handles.remove(handle)?;
        // Its UVM pools leave the VMM while `fd` is still open here, so the
        // file's last reference, and UVM's teardown of it, stay ours.
        for w in self.uvm_maps.take_handle(handle) {
            self.withdraw_uvm(w, why);
        }
        self.uvm_refused.remove(&handle);
        // Registered memory its external mappings hold is taken down while
        // the file is ours, and only then may the guest unpin it (osdesc.rs).
        if kind == HandleKind::Dev(DeviceKind::Uvm) {
            self.osdesc_uvm_close(fd.as_raw_fd(), handle, why);
        }
        for entry in self.active_maps.take_for_fd(handle) {
            log::debug!(
                "{why} of handle {handle}: releasing mapping at SHM {:#x}+{:#x}",
                entry.region.offset,
                entry.region.length
            );
            self.end_rm_mapping(entry, why);
        }
        // The one client set (semsurf.rs) says which RM clients died with
        // this file; the memory records drop what those held.
        let gone_clients = self.semsurf.forget_handle(handle);
        // Clients holding memory the guest registered by its pages are freed
        // here, while the file is ours: RM lets go of the pages now, not
        // whenever the file's last reference goes, and only then is the guest
        // told it may unpin them (osdesc.rs).
        self.osdesc_end_clients(fd.as_raw_fd(), &gone_clients, why);
        self.rmmem.forget_fd(handle, &gone_clients);
        self.dri_maps.retain(|(h, _), _| *h != handle);
        let fbs = self.forget_kms_state(handle);
        // A render file's syncobj numbers die with it and a later file may
        // get the same handle number: its waits must never join these (S-13).
        if matches!(kind, HandleKind::DriRender(_)) {
            self.syncobj_regs.orphan_file(handle);
        }
        self.wl_forget(handle);
        self.nvkms.forget_handle(handle);
        self.inject.file_closed(handle);
        self.pump_cmds.push(PumpCmd::Unwatch { handle });
        log::debug!("{why} of handle {handle} ({kind:?})");
        // A display file's last close can wait on a modeset; not here, on
        // the queue thread under the backend mutex (closer.rs, S-33). One
        // whose framebuffers a call in flight names closes after it (S-6).
        // Counted against the table until it is closed (handle_table.rs).
        if !crate::closer::slow(kind) {
            drop(fd);
        } else if fbs.is_empty() {
            crate::closer::close(self.handles.closing(fd, owner, kind));
        } else {
            self.vm_kms
                .close_after(fbs, Box::new(self.handles.closing(fd, owner, kind)));
        }
        Ok(())
    }

    /// Drop KMS handle `handle`'s per-file state, and with it every
    /// framebuffer it made from the VM's scanout sources: before the host
    /// file closes and its ids can go to someone else (S-6). Returns those
    /// ids: the host file must close through `vm_kms.close_after`.
    pub(crate) fn forget_kms_state(&mut self, handle: u32) -> Vec<u32> {
        self.kms_states
            .remove(&handle)
            .map(|k| k.retire())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------------
    // Host driver version and ABI profile
    // ------------------------------------------------------------------

    /// GPU `addr`'s whole PCI config space, as a privileged launcher read it
    /// (`--pci-config-dir`). Checked against the device's own header at each
    /// GET_SYS_FILES ([`merge_pci_config`]).
    pub fn set_pci_config(&mut self, addr: &str, config: Vec<u8>) {
        self.pci_config.insert(addr.to_string(), config);
    }

    /// The host driver version, as the transport read it from the driver at
    /// start-up. Known before any guest asks, it tells HELLO whether an
    /// NVKMS schema exists for this host (`BCAP_NVKMS_TABLE`) -- which the
    /// guest uses to pick its own NVKMS table -- and lets a modeset IOCTL2
    /// find its table before the guest's first CHECK_VERSION_STR.
    ///
    /// The transport refuses to start on a version that does not parse, or
    /// one the tables were not measured at (`crate::release`), before this.
    pub fn set_host_driver_version(&mut self, text: &str) {
        match abi::version::DriverVersion::parse(text) {
            Some(v) => self.set_driver_version(v),
            None => log::error!(
                "host driver version {text:?} does not parse; no NVKMS schema, and no RM \
                 escape passes the ABI check"
            ),
        }
    }

    /// Whether an RM escape may pass with no host version set: in the unit
    /// tests and fuzzers only, whose fake RMs no release describes.
    fn unversioned_ok(&self) -> bool {
        cfg!(any(test, fuzzing)) && self.unversioned_for_test
    }

    fn set_driver_version(&mut self, v: abi::version::DriverVersion) {
        self.driver = Some(v);
        self.rmallow.set_driver(v);
        self.nvkms.set_version(v);
        self.config.nvkms_table = crate::schema::modeset_table(v).is_some();
        self.abi = abi::versions::table_for(v);
        match self.abi {
            Some(t) => log::debug!("host driver {v}: ABI profile selected, {} escapes", t.len()),
            // `allow_unmeasured_release` may still pick the nearest; the
            // transport refuses to start on this otherwise (release.rs).
            None => log::warn!("host driver {v} has no measured ABI profile"),
        }
    }

    /// `--allow-unmeasured-release`: a host newer than the ABI profiles were
    /// measured through is size-checked against the nearest older profile,
    /// rather than refused outright. After `set_host_driver_version`.
    pub fn allow_unmeasured_release(&mut self) {
        if let (Some(v), None) = (self.driver, self.abi) {
            self.abi = abi::versions::nearest_table_for(v);
            if self.abi.is_some() {
                log::warn!(
                    "host driver {v}: ABI profile of {} (nearest older, unmeasured at {v})",
                    abi::versions::nearest_profile_version(v).expect("a table has a version")
                );
            }
        }
    }

    /// Check one guest ioctl against the host's ABI profile.
    ///
    /// A size mismatch is the failure this is for: the guest and host disagree
    /// about a struct layout, so the host reads or writes the wrong number of
    /// bytes. Without a check it surfaces as corrupt GPU state rather than an
    /// error.
    pub fn check_abi(&self, escape: u32, param_size: u32) -> AbiCheck {
        let Some(table) = self.abi else {
            return AbiCheck::NoProfile;
        };
        let Some(entry) = abi::versions::lookup(table, escape) else {
            return AbiCheck::UnknownEscape;
        };
        match entry.param_size {
            None => AbiCheck::VariableLength,
            Some(expected) if expected == param_size => AbiCheck::Ok,
            Some(expected) => AbiCheck::SizeMismatch {
                expected,
                actual: param_size,
            },
        }
    }

    /// The one gate every export of a guest GEM object asks
    /// (exportgate.rs): HOST_OP PRIME_EXPORT and the Wayland dma-buf path
    /// here, the IOCTL2 re-home through its hooks.
    pub(crate) fn export_gate(&self) -> crate::exportgate::ExportGate<'_> {
        crate::exportgate::ExportGate {
            semsurf: &self.semsurf,
            injected: Some(&self.inject),
            taint: &self.inject_taint,
        }
    }

    // ------------------------------------------------------------------
    // Response helpers
    // ------------------------------------------------------------------

    /// Write a bare response header (nothing, if it does not fit).
    fn write_hdr(&self, resp_buf: &mut [u8], handle: u32, status: i32) -> usize {
        let h = crate::session::hdr(self.current_msg, handle, status, self.current_req_id);
        match resp_buf.get_mut(..h.len()) {
            Some(to) => {
                to.copy_from_slice(&h);
                h.len()
            }
            None => 0,
        }
    }

    /// Write a failure: the header alone, its status the negated `errno`
    /// (EIO if none is given).
    ///
    /// `status` is negative in the response because the driver tests
    /// `(s32)status < 0` and returns it straight out of the syscall. A positive
    /// value here reads as success and userspace proceeds on a failed call.
    fn write_error(&self, resp_buf: &mut [u8], errno: i32) -> usize {
        debug_assert!(errno != 0, "an error reply with no errno");
        let e = if errno != 0 {
            errno.saturating_abs()
        } else {
            libc::EIO
        };
        self.write_hdr(resp_buf, 0, -e)
    }
}

/// Close whatever the guest left open.
///
/// This was an inherent method named `drop` rather than a `Drop` impl, so it
/// never ran: a backend that went out of scope without `teardown()` leaked
/// every host fd it held. `cargo` reported it only as an unused-method warning.
impl Drop for NvidiaBackend {
    fn drop(&mut self) {
        #[cfg(test)]
        crate::fuzz_seeds::dropped(self);
        if !self.handles.is_empty() {
            log::warn!(
                "NvidiaBackend dropped with {} handles still open — \
                 call teardown() before dropping for clean shutdown",
                self.handles.len()
            );
            self.handles.drain_all();
        }
    }
}

// ============================================================
// Serialisation helpers
// ============================================================

#[cfg(test)]
fn read_struct<T: crate::sys::pod::Pod + Copy>(buf: &[u8], offset: usize) -> T {
    crate::sys::pod::read(buf, offset).expect("a buffer long enough for the struct")
}

fn write_struct<T: crate::sys::pod::Pod>(buf: &mut [u8], val: &T) -> usize {
    crate::sys::pod::write(buf, 0, val).expect("a buffer long enough for the struct")
}

#[cfg(test)]
mod abi_tests;

#[cfg(test)]
mod tests;

/// RM mappings as a guest sees them, against a fake RM that answers these
/// calls the way the real one does: the memory type each is placed and mapped
/// with (M-1), read-only placements (M-2), and when a window extent may go to
/// another mapping (M-3).
#[cfg(test)]
mod mapping_tests;

/// UVM semaphore pools in the UVM aperture (uvmmap.rs), end to end through
/// `dispatch`: a fake UVM that makes and frees pools, and a fake VMM that
/// records what it was asked to place and withdraw.
#[cfg(test)]
mod uvm_map_tests;

/// Descriptors RM resolves in the backend's process: every one is the
/// guest's handle of the caller's own file, turned into our descriptor of it,
/// or the call never reaches RM (R1, R2, R5).
#[cfg(test)]
mod descriptor_field_tests;

#[cfg(test)]
mod share_tests;

/// Where a deep block's address may go (review 2026-09-26, backend 7).
#[cfg(test)]
mod deep_tests;

/// Descriptors on their way to the closer (review 2026-09-26, backend 14).
#[cfg(test)]
mod closing_tests;
