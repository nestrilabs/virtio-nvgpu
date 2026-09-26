// SPDX-License-Identifier: Apache-2.0
//! NVKMS policy: what a guest may do through `/dev/nvidia-modeset`, and
//! through nvidia-drm's GRANT/REVOKE_PERMISSIONS, to a display the host
//! compositor is driving.
//!
//! The schema (gen/schema/nvkms.py) makes every NVKMS call *well formed*:
//! pointers aimed at buffers of the right length, descriptors that are ours.
//! It cannot make one *harmless*, because NVKMS trusts its callers far more
//! than a VM boundary may. There are no process, credential or capability
//! checks anywhere in it (R:nvkms §2.3), and a handful of commands check no
//! permission at all even though they reach every display on the GPU:
//! SET_CURSOR_IMAGE, MOVE_CURSOR, SET_LAYER_POSITION, SET_DPY_ATTRIBUTE,
//! SET_DISP_ATTRIBUTE, SET_FRAMELOCK_ATTRIBUTE (nvkms.c:2230-2285, 3070-3241,
//! 3361-3376; R:nvkms §2.1), and QUERY_DPY_DYNAMIC_DATA, a "query" whose
//! force/override flags and EDID are stored in the dpy and outlive the call
//! (nvkms-dpy.c:3055-3160). So the backend decides, from state of its own:
//!
//! - **Grants.** A guest normally holds a display only through a lease: it
//!   asks nvidia-drm (0x52 GRANT_PERMISSIONS, on its lease or card handle) to
//!   grant a dpy to a fresh modeset file, and acquires that on its NVKMS file
//!   (ACQUIRE_PERMISSIONS). We record what nvidia-drm granted through which
//!   KMS handle, and what each acquire gave to which (modeset handle,
//!   device) -- the reply carries the file's whole permission set
//!   (nvkms.c:3475-3500) -- and gate the unchecked head and dpy commands on
//!   it. Records go when nvidia-drm's do: REVOKE through the same KMS handle,
//!   close of that handle (nvidia-drm-drv.c:1588-1600 revokes at postclose),
//!   FREE_DEVICE, close of the NVKMS file, and -- over-clearing, never
//!   under -- whenever an owner revokes wholesale. They also go when the
//!   lease under the KMS handle ends while the handle stays open, which
//!   nvidia-drm does *not* always follow (kms.rs, "lease ends"): from then
//!   on the host may still hold the grant, and only these records stand
//!   between it and the guest.
//! - **Head-level FLIP and SET_MODE.** NVKMS checks a FLIP only for the
//!   layers it dirties (nvCheckFlipPermissions, nvkms-flip.c:84-111), so an
//!   element that only moves the cursor or sets HDR metadata, colorimetry,
//!   the output TF or dithering passes on any head of the GPU, the host
//!   compositor's included (nvkms-flip.c:163-190). Every pFlipHead element
//!   must therefore name a head a grant covers. SET_MODE's own check
//!   (ValidateRequest, nvkms-modeset.c:3940-3966) is sound, but it asks
//!   the host's permission set, which a lease end does not always shrink; a
//!   committed SET_MODE is held to the records as well, by the same rule.
//! - **Refusals.** GRAB_OWNERSHIP, SET_DISP_ATTRIBUTE and
//!   SET_FRAMELOCK_ATTRIBUTE only in compositor-VM mode (`--kms-card`), where
//!   the guest owns the display anyway; the head and dpy gates are lifted
//!   there too. Kernel-client and device-global commands never (they are in
//!   no table either; named here as a second fence). Tegra syncpoints never
//!   (a dGPU host refuses `useSyncpt` itself, nvkms-hw-flip.c:716-719, but
//!   the descriptor behind it is one the schema does not translate) -- in a
//!   layer whose syncObjects.specified is set, the only place NVKMS reads
//!   it (nvkms-hw-flip.c:714, nvkms-modeset.c:246, 4013).
//! - **Rewrites.** QUERY_DPY_DYNAMIC_DATA's overrides are cleared, always.
//!   A FLIP or SET_MODE layer's `completionNotifier.awaken` is cleared
//!   outside `--kms-card`: it makes the flip's completion broadcast
//!   FLIP_OCCURRED to every open with flip permission on the head
//!   (nvkms-evo3.c:3686-3695, nvkms.c:6580-6625). A modeset grantee holds
//!   none, so the guest never gets the event itself; nvidia-drm's own open
//!   does, finds no flip of its own queued and WARNs
//!   (nv_drm_crtc_dequeue_flip, nvidia-drm-crtc.h:334-358) -- a host log
//!   flood, a panic under panic_on_warn. The notifier is still written.
//!   ALLOC_DEVICE's device-wide knobs (registry keys, console hotplugs, no3d)
//!   are cleared outside `--kms-card`: they apply when the call creates the
//!   device (nvkms-evo.c:9043, nvkms.c:1417). DECLARE_EVENT_INTEREST is cut
//!   to the events a display client needs: NVKMS's per-open event list has no
//!   bound (nvkms.c:6422-6435). ALLOC_DEVICE's reply is narrowed to coherent
//!   display memory where the device allows it (`coherent_display_only`).
//! - **Fresh files.** The first ioctl on an NVKMS file makes it an "ioctl"
//!   file forever, and such a file can never become a grant or unicast-event
//!   file (nvkms.c:1291-1342, 5171). A modeset handle becomes *typed* here
//!   the moment any NVKMS call on it is let through, and a typed handle is
//!   refused in every descriptor field -- nvidia-drm's GRANT included --
//!   as is a call's own target.
//!
//! Versions: every offset comes from the layout generated for the host's
//! table (`schema::NvkmsLayout`), and a command whose layout moved in the
//! next release measured (`policy::NVKMS_EXACT`) runs only on a host of
//! exactly its table's release. nvidia-drm's GRANT/REVOKE had no `type`
//! before 580 (8 and 4 bytes, always MODESET); those numbers pass only on a
//! host whose nvidia-drm has that layout.
//!
//! Shared by the queue thread (`before`/`after` under the backend mutex) and
//! the executors, which only ever read: a call's head and dpy gates are
//! decided in `before`, but the call runs later, behind whatever its
//! executor's FIFO holds, and NVKMS checks nothing itself for
//! SET_DPY_ATTRIBUTE, SET_LAYER_POSITION, MOVE_CURSOR or SET_CURSOR_IMAGE
//! (nvkms.c:3070-3085, 3361-3374, 2262-2285, 2230-2257). So every gated call
//! carries the revocation generation it was decided under, `at_run` refuses
//! it if a grant has been taken back since, and holds a read lock across the
//! host ioctl that every revocation takes for writing: a revocation the
//! guest starts (closing the granting lease, REVOKE_PERMISSIONS) is ordered
//! strictly after a gated call already running and before any later one
//! (S-14). A revocation only the host knows of (the lessor ending the
//! lease) is seen when the backend next asks (kms.rs, "lease ends"), as
//! before.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use abi::version::DriverVersion;

use crate::schema::{self, Kind, NvkmsLayout, NvkmsPermArr, NvkmsTarget, policy};
use crate::xfer::{Errno, Prepared};

/// Most `/dev/nvidia-modeset` files one VM may hold open. Each is a host
/// NVKMS open with its own unbounded event list and grant state; a guest
/// needs one per display client plus a few short-lived grant files.
pub const MAX_MODESET_OPENS: usize = 64;

/// And per guest process (quota.rs, B4): one process holding all 64 left
/// the compositor and every EGL and Vulkan window-system path without one.
/// A process holds at most 16, and the last 8 are kept for processes that
/// hold at most 2.
pub const MODESET_SHARE: crate::quota::Share = crate::quota::Share {
    per_owner: 16,
    reserve: 8,
    floor: 2,
};

// NvKmsPermissionsType (nvkms-api.h) and nvidia-drm's own
// (nv_drm_common_ioctl.h: MODESET 2, SUB_OWNER 3).
const PERM_FLIPPING: u32 = 1;
const PERM_MODESET: u32 = 2;
const PERM_SUB_OWNER: u32 = 3;
const NV_DRM_PERMISSIONS_TYPE_MODESET: u64 = 2;

/// NV_MAX_SUBDEVICES and NVKMS_MAX_HEADS_PER_DISP: the widest a permission
/// set has ever been (per (disp, head) through 580).
const MAX_DISPS: usize = 8;
const MAX_HEADS: usize = 4;

/// In no table; refused by name should one ever get there.
const REFUSED: &[&str] = &[
    "FRAMEBUFFER_CONSOLE_DISABLED",
    "REGISTER_VBLANK_INTR_CALLBACK",
    "UNREGISTER_VBLANK_INTR_CALLBACK",
    "EXPORT_VRR_SEMAPHORE_SURFACE",
    "VRR_SIGNAL_SEMAPHORE",
    "GET_3DVISION_DONGLE_PARAM_BYTES",
    "SET_3DVISION_AEGIS_PARAMS",
];

/// Commands a grant gates outside `--kms-card` (`State::check`,
/// `flip_heads`, `set_mode`): each carries a run gate (see the module
/// comment).
const GATED: &[&str] = &[
    "SET_CURSOR_IMAGE",
    "MOVE_CURSOR",
    "SET_LUT",
    "SET_DPY_ATTRIBUTE",
    "SET_LAYER_POSITION",
    "FLIP",
    "SET_MODE",
];

/// How often the VM may make the host probe one dpy (QUERY_DPY_DYNAMIC_DATA)
/// it may drive -- a grant covers it, or `--kms-card` -- and one it may not.
/// In between, the last reply is the answer (see `State::dpy_probe`).
const PROBE_EVERY_GRANTED: Duration = Duration::from_secs(1);
const PROBE_EVERY_OTHER: Duration = Duration::from_secs(30);
/// How many dpys' last probe the VM keeps: far more than a host has.
const DPY_PROBES_KEPT: usize = 256;

/// Only a guest that owns the display (`--kms-card`): device-wide state
/// with no permission check in NVKMS.
const KMS_CARD_ONLY: &[&str] = &[
    "GRAB_OWNERSHIP",
    "SET_DISP_ATTRIBUTE",
    "SET_FRAMELOCK_ATTRIBUTE",
];

/// Who put a grant on a fresh modeset file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// nvidia-drm GRANT_PERMISSIONS on KMS handle `kms`, for `dpy`.
    Drm { kms: u32, dpy: u32 },
    /// NVKMS GRANT_PERMISSIONS on modeset handle `modeset` (an owner or
    /// sub-owner: compositor-VM mode only).
    Nvkms { modeset: u32 },
}

/// What one (modeset handle, deviceHandle) holds, as the host's last
/// ACQUIRE_PERMISSIONS reply said, less what was revoked since. Per head
/// from 595 (index 0 is the device's only disp), per (disp, head) before.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Perms {
    flip: [[u8; MAX_HEADS]; MAX_DISPS],
    modeset: [[u32; MAX_HEADS]; MAX_DISPS],
    /// SUB_OWNER: every head (nvkms.c:3500-3510).
    full: bool,
    /// Some of it came from an NVKMS grant, which we cannot revoke head by
    /// head: dropped whole when its granter goes.
    from_nvkms: bool,
}

impl Perms {
    /// Flip or modeset permission on head `h` of disp `d` (modeset implies
    /// every layer, nvkms-flip.c:40-111).
    fn head(&self, d: usize, h: usize) -> bool {
        self.full
            || (d < MAX_DISPS && h < MAX_HEADS && (self.flip[d][h] != 0 || self.modeset[d][h] != 0))
    }

    /// Modeset permission for `dpy` (an NVDpyId: one bit) on disp `d`.
    fn dpy(&self, d: usize, dpy: u32) -> bool {
        self.full
            || (d < MAX_DISPS
                && dpy.count_ones() == 1
                && self.modeset[d].iter().any(|&l| l & dpy != 0))
    }
}

/// The last host probe of one dpy the VM made, and its reply half, if the
/// host has answered it yet.
struct DpyProbe {
    at: Instant,
    reply: Option<Vec<u8>>,
}

/// One call, as the policy sees it.
struct Call<'a> {
    /// The handle it runs on.
    target: u32,
    /// Handles standing in its descriptor fields.
    fds: &'a [u32],
    kms_card: bool,
}

#[derive(Default)]
struct State {
    version: Option<DriverVersion>,
    /// Modeset handles an NVKMS call has been let through on.
    typed: HashSet<u32>,
    /// Fresh modeset files something granted permissions to.
    grant_fds: HashMap<u32, Source>,
    /// dpyIds granted through each KMS handle (nvidia-drm 0x52).
    drm_grants: HashMap<u32, HashSet<u32>>,
    /// Modeset handles that granted permissions themselves.
    nvkms_granters: HashSet<u32>,
    /// ALLOC_DEVICE replies: (modeset handle, deviceHandle) -> dispHandles.
    disps: HashMap<(u32, u32), [u32; MAX_DISPS]>,
    /// ALLOC_DEVICE requests: (modeset handle, deviceHandle) -> the GPU's
    /// rmDeviceId, which is what makes a dpy the same dpy across files.
    gpus: HashMap<(u32, u32), u32>,
    /// QUERY_DPY_DYNAMIC_DATA, VM-wide, by (GPU, disp index, dpyId): the
    /// last probe the host made for this VM (S-8, `dpy_probe`).
    dpy_probes: HashMap<(u32, usize, u32), DpyProbe>,
    perms: HashMap<(u32, u32), Perms>,
    /// KMS handles whose lease ended with the handle still open, after
    /// something was granted through them: the host may still hold those
    /// grants, so the backend keeps asking (kms.rs, "lease ends") until the
    /// handle goes.
    ended: HashSet<u32>,
    /// A cleared `awaken` was logged (once per session; the rest at debug).
    awaken_logged: bool,
}

/// The NVKMS section's state, shared by the policy object and the backend
/// (which tells it the host version, the mode, and every handle it closes).
#[derive(Default)]
pub struct NvkmsPolicy {
    kms_card: AtomicBool,
    /// The backend leaves guest system memory as asked (rmmem.rs), so the
    /// ALLOC_DEVICE reply is left alone too.
    keep_display_coherency: AtomicBool,
    state: Mutex<State>,
    /// Bumped by every revocation, under `revoking`'s write lock.
    revocations: AtomicU64,
    /// Read-held by a gated call across its host ioctl (`at_run`),
    /// write-held by a revocation. Always taken before `state`.
    run_lock: RwLock<()>,
}

/// Narrow ALLOC_DEVICE's reply to coherent display memory, where the device
/// supports both kinds (H-4).
///
/// The reply's {iso,niso}IOCoherencyModes are how a client picks the memory
/// model for what display reads from system memory: NVKMS's own allocator
/// tries non-coherent first -- write-combined memory and a context DMA that
/// does not snoop -- and coherent only if that is unavailable
/// (nvkms-rm.c:2585-2633), and the userspace driver reads the same fields.
/// Non-coherent memory is what a guest cannot keep coherent on an Intel host,
/// where KVM maps guest RAM write-back whatever the guest's PAT says. On a
/// dGPU both kinds come from one bus capability (nvkms-rm.c:226-253), so
/// when `coherent` is set the non-coherent model is merely a preference and
/// clearing it costs nothing but a snoop. When `coherent` is clear it is left
/// alone: a device that cannot snoop keeps the only model it has.
fn coherent_display_only(lo: &NvkmsLayout, params: &mut [u8]) {
    let at = lo.alloc_reply_coherency as usize;
    for modes in [at, at + 2] {
        // NvKmsDispIOCoherencyModes: NvBool coherent, then noncoherent.
        if params.get(modes) == Some(&1) {
            if let Some(nc) = params.get_mut(modes + 1) {
                *nc = 0;
            }
        }
    }
}

fn rd(b: &[u8], off: usize, width: usize) -> Result<u64, Errno> {
    let s = b.get(off..off + width).ok_or(libc::EINVAL)?;
    let mut v = [0u8; 8];
    v[..width].copy_from_slice(s);
    Ok(u64::from_le_bytes(v))
}

fn rd32(b: &[u8], off: u32) -> Result<u32, Errno> {
    Ok(rd(b, off as usize, 4)? as u32)
}

fn wr32(b: &mut [u8], off: u32, v: u32) -> Result<(), Errno> {
    b.get_mut(off as usize..off as usize + 4)
        .ok_or(libc::EINVAL)?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

fn zero(b: &mut [u8], ranges: &[(u32, u32)]) -> Result<(), Errno> {
    for &(off, len) in ranges {
        b.get_mut(off as usize..(off + len) as usize)
            .ok_or(libc::EINVAL)?
            .fill(0);
    }
    Ok(())
}

/// Every (disp, head) value of one kind of permission in a params block.
fn each_perm(
    params: &[u8],
    pa: NvkmsPermArr,
    width: usize,
    mut f: impl FnMut(usize, usize, u64),
) -> Result<(), Errno> {
    let heads = (pa.head.count as usize).min(MAX_HEADS);
    match pa.disp {
        None => {
            for h in 0..heads {
                f(0, h, rd(params, pa.head.at(h as u32), width)?);
            }
        }
        Some(disp) => {
            for d in 0..(disp.count as usize).min(MAX_DISPS) {
                for h in 0..heads {
                    f(
                        d,
                        h,
                        rd(params, disp.at(d as u32) + pa.head.at(h as u32), width)?,
                    );
                }
            }
        }
    }
    Ok(())
}

/// Which disp row of a `Perms` covers subdevice `sd`: its own before 595,
/// the device's only one after (a permission set is per head there, and
/// NVKMS checks a head's whatever the subdevice, nvkms-flip.c:40-111).
fn perm_disp(lo: &NvkmsLayout, sd: usize) -> usize {
    if lo.acquire.modeset.disp.is_none() {
        0
    } else {
        sd
    }
}

/// One FLIP or SET_MODE layer at `at` in `b`. A Tegra syncpoint is refused
/// (the fence descriptor behind one is not in the schema, so it would reach
/// the host as the guest's own number) in a layer whose syncObjects are
/// specified, the only kind NVKMS reads useSyncpt in; and, when `scrub`,
/// `awaken` is cleared. Says whether it cleared one.
fn layer_ok(
    b: &mut [u8],
    at: usize,
    (specified, use_syncpt, awaken): (u32, u32, u32),
    scrub: bool,
    what: std::fmt::Arguments,
) -> Result<bool, Errno> {
    if rd(b, at + specified as usize, 1)? != 0 && rd(b, at + use_syncpt as usize, 1)? != 0 {
        return Err(refuse(format_args!("{what} asks for a Tegra syncpoint")));
    }
    let awaken = at + awaken as usize;
    if scrub && rd(b, awaken, 1)? != 0 {
        b[awaken] = 0;
        return Ok(true);
    }
    Ok(false)
}

fn refuse(why: std::fmt::Arguments) -> Errno {
    log::warn!("NVKMS: {why}; refused");
    libc::EPERM
}

impl NvkmsPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take a grant back: after every gated call already running has
    /// returned, and so that every one still queued is refused.
    ///
    /// Called on the queue thread under the backend mutex, which no executor
    /// waits for while holding the read side, so this waits at most for the
    /// calls in flight; and only when something is really taken back, so a
    /// CLOSE of any other handle never waits on a flip.
    fn revoking<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let _w = self.run_lock.write().unwrap_or_else(|e| e.into_inner());
        let r = f(&mut self.lock());
        self.revocations.fetch_add(1, Ordering::SeqCst);
        r
    }

    /// `Hooks::at_run`: a gated call whose grant may have been taken back
    /// since `before` let it through (see the module comment).
    pub fn at_run(&self, p: &Prepared) -> Result<Option<crate::xfer::RunGuard<'_>>, Errno> {
        let Some(seen) = p.run_gate() else {
            return Ok(None);
        };
        self.hold_unless_revoked(seen).map(Some).map_err(|_| {
            refuse(format_args!(
                "{} on handle {}: a grant it relied on was taken back while it waited to run",
                p.name(),
                p.target()
            ))
        })
    }

    /// The read side of the run lock, if nothing was revoked since the
    /// count was `seen`.
    fn hold_unless_revoked(&self, seen: u64) -> Result<crate::xfer::RunGuard<'_>, Errno> {
        let guard = self.run_lock.read().unwrap_or_else(|e| e.into_inner());
        if self.revocations.load(Ordering::SeqCst) != seen {
            return Err(libc::EPERM);
        }
        Ok(guard)
    }

    /// The host driver version, which picks the layout every offset below
    /// comes from.
    pub fn set_version(&self, v: DriverVersion) {
        self.lock().version = Some(v);
    }

    /// Compositor-VM mode: the guest owns the display.
    pub fn set_kms_card(&self, on: bool) {
        self.kms_card.store(on, Ordering::Relaxed);
    }

    /// Whether ALLOC_DEVICE's reply is narrowed to coherent display memory
    /// (the default; see `coherent_display_only`).
    pub fn set_coherent_display(&self, on: bool) {
        self.keep_display_coherency.store(!on, Ordering::Relaxed);
    }

    /// A handle closed: everything that was its, or granted through it,
    /// goes -- as nvidia-drm's postclose revokes what a DRM file granted
    /// (nvidia-drm-drv.c:1588-1600) and nvKmsClose frees a file's devices
    /// and permissions (nvkms.c:5254-5308).
    pub fn forget_handle(&self, h: u32) {
        // Grants *through* h: its own records are its file's, which a call
        // still queued on it keeps open on the host with the permissions it
        // had.
        let revokes = {
            let st = self.lock();
            st.granted_through(h) || st.nvkms_granters.contains(&h)
        };
        if revokes {
            self.revoking(|st| st.forget_handle(h));
        } else {
            self.lock().forget_handle(h);
        }
    }

    /// A lease or card handle stopped holding what it held (the lessor
    /// revoked the lease, closed, or dropped master): its grants go as if
    /// it had closed, though the host's may not have (kms.rs, "lease
    /// ends"), and a handle that had granted something stays on the list
    /// the backend keeps asking about. CLOSE forgets it anyway.
    pub fn lease_ended(&self, kms: u32) {
        if !self.lock().granted_through(kms) {
            return;
        }
        self.revoking(|st| {
            st.ended.insert(kms);
            st.revoke_all_through(kms);
        });
    }

    /// Whether nvidia-drm GRANT_PERMISSIONS ever succeeded through KMS
    /// handle `kms` (and the handle has not closed since): whether its file
    /// may be the one a host connector's grant belongs to, which its close
    /// then disables (nvidia-drm-drv.c:1497-1523).
    pub fn granted_through(&self, kms: u32) -> bool {
        self.lock().granted_through(kms)
    }

    /// KMS handles that granted something still recorded (nvidia-drm
    /// GRANT_PERMISSIONS), or whose lease ended after they had: whose leases
    /// the backend re-checks before an NVKMS call relies on them and on a
    /// timer (kms.rs, "lease ends").
    pub fn granting_handles(&self) -> Vec<u32> {
        let st = self.lock();
        let mut v: Vec<u32> = st.drm_grants.keys().copied().collect();
        v.extend(st.ended.iter().copied());
        for s in st.grant_fds.values() {
            if let Source::Drm { kms, .. } = *s {
                v.push(kms);
            }
        }
        v.sort_unstable();
        v.dedup();
        v
    }

    /// A session reset: every handle is gone.
    pub fn reset(&self) {
        self.revoking(|st| {
            let version = st.version;
            *st = State {
                version,
                ..State::default()
            };
        });
    }

    /// Whether `h` has had an NVKMS call (and so can never be a grant or
    /// unicast-event file).
    pub fn is_typed(&self, h: u32) -> bool {
        self.lock().typed.contains(&h)
    }

    fn layout(st: &State) -> Result<(&'static NvkmsLayout, bool), Errno> {
        let v = st.version.ok_or(libc::ENOTTY)?;
        let lo = schema::modeset_table(v)
            .and_then(|t| t.nvkms)
            .ok_or(libc::ENOTTY)?;
        Ok((lo, schema::modeset_table_exact(v)))
    }

    // ───────────────────────────── IOCTL2 ─────────────────────────────

    /// `Hooks::before` for NVKMS and nvidia-drm GRANT/REVOKE entries.
    pub fn before(&self, p: &mut Prepared) -> Result<(), Errno> {
        let fds: Vec<u32> = p.fd_in_handles().map(|(_, _, h)| h).collect();
        let call = Call {
            target: p.target(),
            fds: &fds,
            kms_card: self.kms_card.load(Ordering::Relaxed),
        };
        // Read before anything is decided: a revocation from here on is
        // one the decision below did not see.
        let seen = self.revocations.load(Ordering::SeqCst);
        let mut st = self.lock();
        let pol = p.policy();
        if pol & (policy::GRANT | policy::REVOKE) != 0 {
            let arg = p.buffer(0).unwrap_or(&[]);
            st.drm_check(p.name(), &call, arg)?;
            if pol & policy::REVOKE != 0 {
                // Taken back now, not when the reply comes: the host revokes
                // as the call runs, and a gated call on another executor
                // must not slip in between. Should the host refuse, the
                // records are stricter than it for a while, which only
                // costs the guest its own calls.
                drop(st);
                self.revoking(|st| st.drm_record(p.name(), call.target, arg, &[]));
            }
            return Ok(());
        }
        let name = p.name().strip_prefix("NVKMS_").unwrap_or(p.name());
        if !call.kms_card && GATED.contains(&name) {
            p.set_run_gate(seen);
        }
        let (lo, exact) = Self::layout(&st)?;
        if pol & policy::NVKMS_EXACT != 0 && !exact {
            return Err(refuse(format_args!(
                "{name}'s layout moved after this host's table was measured, and this host \
                 ({:?}) is not that release",
                st.version
            )));
        }
        if name == "FLIP" {
            let dev = rd32(p.buffer(1).ok_or(libc::EINVAL)?, lo.flip.device)?;
            // pFlipHead's array, if it got a buffer (a NULL one reaches the
            // host as NULL and fails there). It is IN only: what is cleared
            // here never reaches the guest's copy.
            if let Some(b) = p.pointee(1, lo.flip.ptr as usize) {
                let mut heads = p.buffer_mut(b).ok_or(libc::EINVAL)?;
                st.flip_heads(lo, &call, dev, &mut heads)?;
            }
        }
        let mut params = p.buffer_mut(1).ok_or(libc::EINVAL)?;
        st.check(lo, name, &call, &mut params)?;
        let local =
            name == "QUERY_DPY_DYNAMIC_DATA" && st.dpy_probe(lo, &call, &mut params, Instant::now());
        drop(params);
        if local {
            p.answer_locally(0);
        }
        if matches!(name, "REVOKE_PERMISSIONS" | "RELEASE_OWNERSHIP") {
            // As for nvidia-drm's REVOKE above; `record` forgets them again.
            drop(st);
            self.revoking(State::forget_all_grants);
        }
        Ok(())
    }

    /// `Hooks::after`: what the host granted, allocated or revoked, and the
    /// one reply rewritten (ALLOC_DEVICE's coherency modes).
    pub fn after(&self, p: &mut Prepared, ret: i32) {
        if ret != 0 {
            return;
        }
        let fds: Vec<u32> = p.fd_in_handles().map(|(_, _, h)| h).collect();
        let mut st = self.lock();
        // A call that finished after its file closed, or after a session
        // reset, records nothing: its grants were revoked with the file
        // (forget_handle), and a REVOKE's forgetting belongs to a session
        // that is gone. Its reply is still rewritten below.
        let records = p.records();
        if p.policy() & (policy::GRANT | policy::REVOKE) != 0 {
            if records {
                st.drm_record(p.name(), p.target(), p.buffer(0).unwrap_or(&[]), &fds);
            }
            return;
        }
        let name = p.name().strip_prefix("NVKMS_").unwrap_or(p.name());
        let Ok((lo, _)) = Self::layout(&st) else {
            return;
        };
        if name == "ALLOC_DEVICE" && !self.keep_display_coherency.load(Ordering::Relaxed) {
            if let Some(mut params) = p.buffer_mut(1) {
                coherent_display_only(lo, &mut params);
            }
        }
        let target = p.target();
        if let Some(params) = p.buffer(1).filter(|_| records) {
            st.record(lo, name, target, params, &fds);
        }
    }

    /// The reply to a v1 call `v1_before` let through: the 16-byte
    /// NvKmsIoctlParams and the params block, as `msg` was. Only
    /// ALLOC_DEVICE's reply is rewritten (see `after`); v1 records nothing.
    pub fn v1_after(&self, msg: &mut [u8]) {
        if self.keep_display_coherency.load(Ordering::Relaxed) {
            return;
        }
        let st = self.lock();
        let (Ok((lo, _)), Some(table)) = (
            Self::layout(&st),
            st.version.and_then(schema::modeset_table),
        ) else {
            return;
        };
        let Ok(cmd) = rd32(msg, 0) else { return };
        if table.lookup_nvkms(cmd).map(|e| e.name) == Some("NVKMS_ALLOC_DEVICE") {
            if let Some(params) = msg.get_mut(16..) {
                coherent_display_only(lo, params);
            }
        }
    }

    /// A v1 call `v1_before` let through, answered from the last probe
    /// if QUERY_DPY_DYNAMIC_DATA may not probe again yet: true if it was
    /// (the reply is in `msg`, and the host is not asked).
    pub fn v1_cached(&self, target: u32, msg: &mut [u8]) -> bool {
        let mut st = self.lock();
        let (Ok((lo, _)), Some(table)) = (
            Self::layout(&st),
            st.version.and_then(schema::modeset_table),
        ) else {
            return false;
        };
        let Ok(cmd) = rd32(msg, 0) else { return false };
        if table.lookup_nvkms(cmd).map(|e| e.name) != Some("NVKMS_QUERY_DPY_DYNAMIC_DATA") {
            return false;
        }
        let call = Call {
            target,
            fds: &[],
            kms_card: self.kms_card.load(Ordering::Relaxed),
        };
        match msg.get_mut(16..) {
            Some(params) => st.dpy_probe(lo, &call, params, Instant::now()),
            None => false,
        }
    }

    /// What a successful v1 call leaves for the probe limit: ALLOC_DEVICE's
    /// disps and GPU, QUERY_DPY_DYNAMIC_DATA's reply, the dpy events a
    /// GET_NEXT_EVENT returns (`State::record`). Grants cannot travel v1.
    pub fn v1_record(&self, target: u32, msg: &[u8]) {
        let mut st = self.lock();
        let (Ok((lo, _)), Some(table)) = (
            Self::layout(&st),
            st.version.and_then(schema::modeset_table),
        ) else {
            return;
        };
        let Ok(cmd) = rd32(msg, 0) else { return };
        let Some(name) = table.lookup_nvkms(cmd).map(|e| e.name) else {
            return;
        };
        let name = name.strip_prefix("NVKMS_").unwrap_or(name);
        if matches!(
            name,
            "ALLOC_DEVICE" | "QUERY_DPY_DYNAMIC_DATA" | "GET_NEXT_EVENT"
        ) {
            if let Some(params) = msg.get(16..) {
                st.record(lo, name, target, params, &[]);
            }
        }
    }

    // ───────────────────────────── v1 ─────────────────────────────

    /// A v1 IOCTL on a modeset handle: the 16-byte NvKmsIoctlParams and the
    /// params block behind it, back to back, as the old guest sends them.
    /// Only commands whose table entry has no pointer and no descriptor go
    /// this way -- v1 has no way to carry either -- with the same policy as
    /// IOCTL2, applied to `msg` in place. Nothing is recorded afterwards:
    /// what the record needs (ACQUIRE_PERMISSIONS) carries a descriptor, and
    /// what would clear a record is cleared here, before the call.
    pub fn v1_before(&self, target: u32, msg: &mut [u8]) -> Result<(), Errno> {
        let mut st = self.lock();
        let (lo, exact) = Self::layout(&st)
            .map_err(|_| refuse(format_args!("v1 call with no NVKMS table for this host")))?;
        let v = st.version.ok_or(libc::EPERM)?;
        let table = schema::modeset_table(v).ok_or(libc::EPERM)?;
        let cmd = rd32(msg, 0)?;
        let size = rd32(msg, 4)?;
        let entry = table
            .lookup_nvkms(cmd)
            .ok_or_else(|| refuse(format_args!("v1 command {cmd} is in no table")))?;
        let [root] = table.fields(entry.fields) else {
            return Err(libc::EPERM);
        };
        let Kind::Ptr { max, children, .. } = root.kind else {
            return Err(libc::EPERM);
        };
        let name = entry.name.strip_prefix("NVKMS_").unwrap_or(entry.name);
        if children.len != 0 {
            return Err(refuse(format_args!(
                "{name} carries pointers or descriptors, which v1 cannot; it needs IOCTL2"
            )));
        }
        if size != max || msg.len() != 16 + max as usize {
            return Err(libc::EINVAL);
        }
        if entry.policy & policy::NVKMS_EXACT != 0 && !exact {
            return Err(refuse(format_args!(
                "{name}'s layout is not known for this host"
            )));
        }
        let call = Call {
            target,
            fds: &[],
            kms_card: self.kms_card.load(Ordering::Relaxed),
        };
        st.check(lo, name, &call, &mut msg[16..])?;
        if matches!(name, "RELEASE_OWNERSHIP" | "REVOKE_PERMISSIONS") {
            drop(st);
            self.revoking(State::forget_all_grants);
        }
        Ok(())
    }
}

impl State {
    /// The policy for one NVKMS call on `call.target`, over its params block.
    /// (FLIP's heads, behind a pointer, are `flip_heads`'.)
    fn check(
        &mut self,
        lo: &NvkmsLayout,
        name: &str,
        call: &Call,
        params: &mut [u8],
    ) -> Result<(), Errno> {
        if REFUSED.contains(&name) {
            return Err(refuse(format_args!(
                "{name} is for kernel clients or device-global"
            )));
        }
        if KMS_CARD_ONLY.contains(&name) && !call.kms_card {
            return Err(refuse(format_args!(
                "{name} acts on the whole display and this guest does not own it (no --kms-card)"
            )));
        }
        self.check_fds(name, call)?;
        let gated = !call.kms_card;
        match name {
            "ALLOC_DEVICE" if gated => zero(params, lo.alloc_scrub)?,
            "FREE_DEVICE" => {
                // NvKmsFreeDeviceRequest.deviceHandle @0 in every release.
                // Forgotten whether or not the host agrees: a record that
                // outlives its device would gate the next one.
                let dev = rd32(params, 0)?;
                self.disps.remove(&(call.target, dev));
                self.gpus.remove(&(call.target, dev));
                self.perms.remove(&(call.target, dev));
            }
            "QUERY_DPY_DYNAMIC_DATA" => zero(params, lo.dpy_dynamic_scrub)?,
            "DECLARE_EVENT_INTEREST" if gated => {
                let mask = rd32(params, lo.event_interest)?;
                wr32(params, lo.event_interest, mask & lo.events_allowed)?;
            }
            "SET_CURSOR_IMAGE" if gated => {
                self.head_granted(name, call.target, params, lo.set_cursor_image)?
            }
            "MOVE_CURSOR" if gated => {
                self.head_granted(name, call.target, params, lo.move_cursor)?
            }
            "SET_LUT" if gated => self.head_granted(name, call.target, params, lo.set_lut)?,
            "SET_DPY_ATTRIBUTE" if gated => {
                self.dpy_granted(name, call.target, params, lo.set_dpy_attribute)?
            }
            "SET_LAYER_POSITION" if gated => self.layers_granted(lo, call.target, params)?,
            "SET_MODE" => self.set_mode(lo, call, params)?,
            "ACQUIRE_PERMISSIONS" => {
                // A grant we did not see made cannot be tracked, so it
                // cannot be revoked here when it is revoked on the host.
                if !call.fds.iter().all(|g| self.grant_fds.contains_key(g)) {
                    return Err(refuse(format_args!(
                        "ACQUIRE_PERMISSIONS names a file nothing we saw granted to"
                    )));
                }
            }
            _ => {}
        }
        self.typed.insert(call.target);
        Ok(())
    }

    /// FLIP's pFlipHead array on device `dev`. Outside `--kms-card` every
    /// element must name a head a grant to this file covers, whatever it
    /// changes there: NVKMS itself lets through any element that dirties
    /// no layer (cursor, HDR infoframe, colorimetry, TF, dithering,
    /// olutFpNormScale; nvkms-flip.c:84-111, 163-190). The (sd, head) pair
    /// is what nvFlipEvo acts on (nvkms-flip.c:542-560); the host checks
    /// both are in range (ValidateFlipHeads, nvkms.c:2639-2663), the
    /// records hold only what a grant gave. Then each layer (`layer_ok`).
    fn flip_heads(
        &mut self,
        lo: &NvkmsLayout,
        call: &Call,
        dev: u32,
        heads: &mut [u8],
    ) -> Result<(), Errno> {
        let f = lo.flip;
        let gated = !call.kms_card;
        let bytes = (f.sync_specified, f.use_syncpt, f.awaken);
        let mut cleared = 0;
        for (e, head) in heads.chunks_mut(f.head_size as usize).enumerate() {
            if gated {
                let (sd, h) = (rd32(head, f.sd)?, rd32(head, f.head)?);
                let d = perm_disp(lo, sd as usize);
                if !self
                    .perms(call.target, dev)
                    .is_some_and(|p| p.head(d, h as usize))
                {
                    return Err(refuse(format_args!(
                        "FLIP element {e} on sd {sd} head {h}, which no grant to handle {} covers",
                        call.target
                    )));
                }
            }
            for l in 0..f.layer.count {
                let what = format_args!("FLIP element {e} layer {l}");
                cleared += layer_ok(head, f.layer.at(l), bytes, gated, what)? as u32;
            }
        }
        self.awaken_cleared("FLIP", cleared);
        Ok(())
    }

    /// SET_MODE. Outside `--kms-card`, a committed request is held to the
    /// records the way ValidateRequest holds it to the host's permission
    /// set (nvkms-modeset.c:3940-3966): every requested head needs modeset
    /// permission, and the dpys put on it must be ones granted for it -- so
    /// a grant the backend ended (a lease that ended, kms.rs) ends here too,
    /// though the host may still have it. Requested disps and heads past
    /// what the host has room for it refuses itself (3897-3935), so they
    /// are skipped, and unrequested ones it never reads (1076-1094). Then
    /// each requested head's layers (`layer_ok`).
    fn set_mode(&mut self, lo: &NvkmsLayout, call: &Call, params: &mut [u8]) -> Result<(), Errno> {
        let m = lo.set_mode;
        let gated = !call.kms_card;
        let commit = rd(params, m.commit as usize, 1)? != 0;
        let dev = rd32(params, m.device)?;
        let disps = rd32(params, m.disps)?;
        let bytes = (m.sync_specified, m.use_syncpt, m.awaken);
        let mut cleared = 0;
        for d in (0..m.disp.count).filter(|d| disps & (1 << d) != 0) {
            let heads = rd32(params, (m.disp.at(d) + m.heads as usize) as u32)?;
            for h in (0..m.head.count).filter(|h| heads & (1 << h) != 0) {
                let at = m.disp.at(d) + m.head.at(h);
                if gated && commit {
                    let dpys = rd32(params, (at + m.dpys as usize) as u32)?;
                    let pd = perm_disp(lo, d as usize);
                    let ok = self.perms(call.target, dev).is_some_and(|p| {
                        p.full
                            || (pd < MAX_DISPS && (h as usize) < MAX_HEADS && {
                                let granted = p.modeset[pd][h as usize];
                                granted != 0 && dpys & !granted == 0
                            })
                    });
                    if !ok {
                        return Err(refuse(format_args!(
                            "SET_MODE on disp {d} head {h} (dpys {dpys:#x}), which no grant to \
                             handle {} covers",
                            call.target
                        )));
                    }
                }
                for l in 0..m.layer.count {
                    let what = format_args!("SET_MODE disp {d} head {h} layer {l}");
                    cleared += layer_ok(params, at + m.layer.at(l), bytes, gated, what)? as u32;
                }
            }
        }
        self.awaken_cleared("SET_MODE", cleared);
        Ok(())
    }

    fn awaken_cleared(&mut self, name: &str, n: u32) {
        if n == 0 {
            return;
        }
        if !self.awaken_logged {
            self.awaken_logged = true;
            log::warn!(
                "NVKMS: {name} asked for FLIP_OCCURRED on {n} layer(s), which would reach \
                 nvidia-drm's own open on the host; completionNotifier.awaken cleared \
                 (logged once per session)"
            );
        } else {
            log::debug!("NVKMS: {name}: completionNotifier.awaken cleared on {n} layer(s)");
        }
    }

    /// Every modeset file in a descriptor field must be one no NVKMS call
    /// has ever run on (and never the call's own file): grants and unicast
    /// events need a file of type Undefined, grant or unicast, never Ioctl
    /// (nvkms.c:1291-1342).
    fn check_fds(&self, name: &str, call: &Call) -> Result<(), Errno> {
        for &h in call.fds {
            if h == call.target || self.typed.contains(&h) {
                return Err(refuse(format_args!(
                    "{name} names handle {h} as a fresh or grant file, and an ioctl has already \
                     run on it"
                )));
            }
        }
        Ok(())
    }

    /// The disp index of `disp` on (target, dev), from the ALLOC_DEVICE reply.
    /// Which dpy a QUERY_DPY_DYNAMIC_DATA on `target` names, VM-wide:
    /// (GPU, disp index, dpyId). None for a device or disp the file's
    /// ALLOC_DEVICE did not answer with, which the host refuses cheaply.
    fn dpy_key(&self, lo: &NvkmsLayout, target: u32, params: &[u8]) -> Option<(u32, usize, u32)> {
        let t = lo.dpy_dynamic;
        let dev = rd32(params, t.device).ok()?;
        let disp = rd32(params, t.disp).ok()?;
        let dpy = rd32(params, t.what).ok()?;
        let d = self.disp_index(target, dev, disp)?;
        Some((*self.gpus.get(&(target, dev))?, d, dpy))
    }

    /// QUERY_DPY_DYNAMIC_DATA makes the host reconnect the dpy and read its
    /// EDID afresh over DDC or AUX (nvDpyGetDynamicData -> DpyConnectEvo ->
    /// ReadEdidFromResman with COPY_CACHE_NO, nvkms-dpy.c:109-133, 1319),
    /// all under the global nvkms_lock that the host compositor's flips wait
    /// on uninterruptibly (nvkms-kapi.c:3488-3491). A guest looping it over
    /// every host dpy from 16 files would drop the host desktop, and every
    /// VM whose flips go through NVKMS, to a few frames a second (S-8).
    ///
    /// So the VM probes each dpy at most once per `PROBE_EVERY_GRANTED` if
    /// it may drive the dpy, and once per `PROBE_EVERY_OTHER` otherwise --
    /// a host monitor is the guest's to see, not to probe -- and in between
    /// the last reply is the answer, which the host would give too: the
    /// overrides that could make it differ are always cleared (`check`),
    /// and the dpy events clear the whole record (`record`). A refusal would
    /// break every NVKMS client, which queries every dpy as it starts.
    ///
    /// True if `params`' reply half now holds that answer.
    fn dpy_probe(
        &mut self,
        lo: &NvkmsLayout,
        call: &Call,
        params: &mut [u8],
        now: Instant,
    ) -> bool {
        let Some(key) = self.dpy_key(lo, call.target, params) else {
            return false;
        };
        let (_, d, dpy) = key;
        let dev = rd32(params, lo.dpy_dynamic.device).unwrap_or(0);
        let may_drive =
            call.kms_card || self.perms(call.target, dev).is_some_and(|p| p.dpy(d, dpy));
        let every = if may_drive {
            PROBE_EVERY_GRANTED
        } else {
            PROBE_EVERY_OTHER
        };
        // The dpyId is the guest's: a record of every one it names, with
        // no reply because the host refused them, grew without bound. Past
        // DPY_PROBES_KEPT the ones never answered go (review 2026-09-26,
        // backend 6); a dpy the host has answered for keeps its limit.
        if self.dpy_probes.len() >= DPY_PROBES_KEPT && !self.dpy_probes.contains_key(&key) {
            self.dpy_probes.retain(|_, p| p.reply.is_some());
            if self.dpy_probes.len() >= DPY_PROBES_KEPT {
                return false;
            }
        }
        let probe = match self.dpy_probes.entry(key) {
            // The first: this one goes to the host.
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(DpyProbe {
                    at: now,
                    reply: None,
                });
                return false;
            }
            std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        };
        if now.duration_since(probe.at) >= every {
            // Due: this one goes to the host.
            probe.at = now;
            return false;
        }
        // Not due. With no reply yet the first probe is still running (or
        // failed): the host answers this one too.
        let Some(reply) = &probe.reply else {
            return false;
        };
        let (off, len) = lo.dpy_dynamic_reply;
        match params.get_mut(off as usize..(off + len) as usize) {
            Some(dst) if dst.len() == reply.len() => {
                dst.copy_from_slice(reply);
                true
            }
            _ => false,
        }
    }

    fn disp_index(&self, target: u32, dev: u32, disp: u32) -> Option<usize> {
        let d = self.disps.get(&(target, dev))?;
        d.iter().position(|&x| x != 0 && x == disp)
    }

    fn perms(&self, target: u32, dev: u32) -> Option<&Perms> {
        self.perms.get(&(target, dev))
    }

    fn head_granted(
        &self,
        name: &str,
        target: u32,
        params: &[u8],
        t: NvkmsTarget,
    ) -> Result<(), Errno> {
        let (dev, disp, head) = (
            rd32(params, t.device)?,
            rd32(params, t.disp)?,
            rd32(params, t.what)?,
        );
        let ok = self
            .disp_index(target, dev, disp)
            .zip(self.perms(target, dev))
            .is_some_and(|(d, p)| p.head(d, head as usize));
        if !ok {
            return Err(refuse(format_args!(
                "{name} on head {head} of disp handle {disp}, which no grant to handle {target} covers"
            )));
        }
        Ok(())
    }

    fn dpy_granted(
        &self,
        name: &str,
        target: u32,
        params: &[u8],
        t: NvkmsTarget,
    ) -> Result<(), Errno> {
        let (dev, disp, dpy) = (
            rd32(params, t.device)?,
            rd32(params, t.disp)?,
            rd32(params, t.what)?,
        );
        let ok = self
            .disp_index(target, dev, disp)
            .zip(self.perms(target, dev))
            .is_some_and(|(d, p)| p.dpy(d, dpy));
        if !ok {
            return Err(refuse(format_args!(
                "{name} on dpy {dpy:#x}, which no grant to handle {target} covers"
            )));
        }
        Ok(())
    }

    fn layers_granted(&self, lo: &NvkmsLayout, target: u32, params: &[u8]) -> Result<(), Errno> {
        let lp = lo.layer_position;
        let dev = rd32(params, lp.device)?;
        let disps = rd32(params, lp.disps)?;
        let p = self.perms(target, dev);
        for d in 0..32 {
            if disps & (1 << d) == 0 {
                continue;
            }
            if d >= lp.disp.count {
                return Err(refuse(format_args!(
                    "SET_LAYER_POSITION on disp {d}, which no device has"
                )));
            }
            let heads = rd32(params, lp.disp.at(d) as u32 + lp.heads)?;
            for h in 0..32 {
                if heads & (1 << h) != 0 && !p.is_some_and(|p| p.head(d as usize, h as usize)) {
                    return Err(refuse(format_args!(
                        "SET_LAYER_POSITION on disp {d} head {h}, which no grant to handle {target} covers"
                    )));
                }
            }
        }
        Ok(())
    }

    /// What a successful NVKMS call changed.
    fn record(&mut self, lo: &NvkmsLayout, name: &str, target: u32, params: &[u8], fds: &[u32]) {
        match name {
            "ALLOC_DEVICE" => {
                let Ok(dev) = rd32(params, lo.alloc_reply_device) else {
                    return;
                };
                let mut disps = [0u32; MAX_DISPS];
                for (i, d) in disps.iter_mut().enumerate() {
                    *d = rd32(params, lo.alloc_reply_disps + 4 * i as u32).unwrap_or(0);
                }
                self.disps.insert((target, dev), disps);
                if let Ok(gpu) = rd32(params, lo.alloc_device_id) {
                    self.gpus.insert((target, dev), gpu);
                }
                // A handle number the host reused starts with nothing.
                self.perms.remove(&(target, dev));
            }
            "QUERY_DPY_DYNAMIC_DATA" => {
                let (off, len) = lo.dpy_dynamic_reply;
                let Some(reply) = params.get(off as usize..(off + len) as usize) else {
                    return;
                };
                if let Some(key) = self.dpy_key(lo, target, params) {
                    let probe = self.dpy_probes.entry(key).or_insert(DpyProbe {
                        at: Instant::now(),
                        reply: None,
                    });
                    probe.reply = Some(reply.to_vec());
                }
            }
            // A dpy came, went or changed: the next probe of any goes to the
            // host, whatever the limit says.
            "GET_NEXT_EVENT" => {
                let valid = rd(params, lo.next_event_valid as usize, 1).unwrap_or(0) != 0;
                let ty = rd32(params, lo.next_event_type).unwrap_or(u32::MAX);
                if valid && ty < 32 && lo.dpy_events & (1 << ty) != 0 {
                    self.dpy_probes.clear();
                }
            }
            "ACQUIRE_PERMISSIONS" => {
                if let Err(e) = self.record_acquire(lo, target, params, fds) {
                    log::warn!(
                        "NVKMS: ACQUIRE_PERMISSIONS reply unreadable ({e}); nothing recorded"
                    );
                }
            }
            "GRANT_PERMISSIONS" => {
                for &g in fds {
                    self.grant_fds.insert(g, Source::Nvkms { modeset: target });
                }
                self.nvkms_granters.insert(target);
            }
            // Revocation by the owner resets every matching grant file and
            // strips every matching permission on the device
            // (nvkms.c:3536-3717); releasing ownership revokes them all
            // (1131-1152). We cannot tell which, so we forget them all.
            "REVOKE_PERMISSIONS" | "RELEASE_OWNERSHIP" => self.forget_all_grants(),
            _ => {}
        }
    }

    fn record_acquire(
        &mut self,
        lo: &NvkmsLayout,
        target: u32,
        params: &[u8],
        fds: &[u32],
    ) -> Result<(), Errno> {
        let a = lo.acquire;
        let dev = rd32(params, a.device)?;
        let ptype = rd32(params, a.ptype)?;
        let from_nvkms = fds
            .iter()
            .any(|g| matches!(self.grant_fds.get(g), Some(Source::Nvkms { .. })));
        let mut p = self.perms.get(&(target, dev)).cloned().unwrap_or_default();
        // The reply is the file's whole set of that type, grants of
        // earlier acquires included (nvkms.c:3475-3500): replace, not add.
        match ptype {
            PERM_FLIPPING => {
                p.flip = Default::default();
                each_perm(params, a.flip, 1, |d, h, v| p.flip[d][h] = v as u8)?;
            }
            PERM_MODESET => {
                p.modeset = Default::default();
                each_perm(params, a.modeset, 4, |d, h, v| p.modeset[d][h] = v as u32)?;
            }
            PERM_SUB_OWNER => p.full = true,
            _ => return Err(libc::EINVAL),
        }
        p.from_nvkms |= from_nvkms;
        self.perms.insert((target, dev), p);
        Ok(())
    }

    // ── nvidia-drm GRANT/REVOKE_PERMISSIONS (policy::GRANT, policy::REVOKE) ──

    /// MODESET only: SUB_OWNER blanks every head on the GPU and hands the
    /// whole device over, and nvidia-drm never looks at the lease to scope
    /// it (nvidia-drm-drv.c:1372-1417, 1533). A revocation only of a dpy
    /// granted through the same handle, which is what nvidia-drm's own
    /// check amounts to for a lessee's grants (RV:subowner). The layout
    /// before 580 has no type (always MODESET) and passes only on a host
    /// that has it.
    fn drm_check(&mut self, name: &str, call: &Call, arg: &[u8]) -> Result<(), Errno> {
        let typed_host = self
            .version
            .and_then(schema::modeset_table)
            .and_then(|t| t.nvkms)
            .is_none_or(|lo| lo.drm_grant_typed);
        let untyped_on_typed = || {
            log::warn!(
                "nvidia-drm {name}: a layout without a type, and this host's has one; refused"
            );
            libc::EINVAL
        };
        match name {
            "NV_GRANT_PERMISSIONS" => {
                if rd(arg, 8, 4)? != NV_DRM_PERMISSIONS_TYPE_MODESET {
                    return Err(refuse(format_args!(
                        "nvidia-drm GRANT_PERMISSIONS of a type other than MODESET"
                    )));
                }
            }
            "NV_GRANT_PERMISSIONS_UNTYPED" if typed_host => return Err(untyped_on_typed()),
            "NV_REVOKE_PERMISSIONS" | "NV_REVOKE_PERMISSIONS_UNTYPED" => {
                if name == "NV_REVOKE_PERMISSIONS" {
                    if rd(arg, 4, 4)? != NV_DRM_PERMISSIONS_TYPE_MODESET {
                        return Err(refuse(format_args!(
                            "nvidia-drm REVOKE_PERMISSIONS of a type other than MODESET"
                        )));
                    }
                } else if typed_host {
                    return Err(untyped_on_typed());
                }
                let dpy = rd32(arg, 0)?;
                if !self
                    .drm_grants
                    .get(&call.target)
                    .is_some_and(|s| s.contains(&dpy))
                {
                    return Err(refuse(format_args!(
                        "nvidia-drm REVOKE_PERMISSIONS of dpy {dpy:#x}, which handle {} never granted",
                        call.target
                    )));
                }
            }
            "NV_GRANT_PERMISSIONS_UNTYPED" => {}
            _ => return Err(libc::EPERM),
        }
        self.check_fds(name, call)
    }

    fn drm_record(&mut self, name: &str, kms: u32, arg: &[u8], fds: &[u32]) {
        let Ok(dpy) = rd32(arg, if name.starts_with("NV_GRANT") { 4 } else { 0 }) else {
            return;
        };
        if name.starts_with("NV_GRANT") {
            self.drm_grants.entry(kms).or_default().insert(dpy);
            for &g in fds {
                self.grant_fds.insert(g, Source::Drm { kms, dpy });
            }
        } else {
            self.revoke_dpy(kms, dpy);
        }
    }

    /// nvidia-drm revoked `dpy` it granted through `kms` from every file
    /// that acquired it (nvKms->revokePermissions, nvkms-kapi.c): gone from
    /// every head's list, and the grant file is Undefined again
    /// (RevokePermissionsSet, nvkms.c:3536-3580).
    fn revoke_dpy(&mut self, kms: u32, dpy: u32) {
        if let Some(s) = self.drm_grants.get_mut(&kms) {
            s.remove(&dpy);
        }
        for p in self.perms.values_mut() {
            for heads in p.modeset.iter_mut() {
                for list in heads.iter_mut() {
                    *list &= !dpy;
                }
            }
        }
        self.grant_fds.retain(|_, s| *s != Source::Drm { kms, dpy });
    }

    fn granted_through(&self, kms: u32) -> bool {
        self.drm_grants.contains_key(&kms)
            || self.ended.contains(&kms)
            || self
                .grant_fds
                .values()
                .any(|s| matches!(s, Source::Drm { kms: k, .. } if *k == kms))
    }

    fn revoke_all_through(&mut self, kms: u32) {
        if let Some(dpys) = self.drm_grants.remove(&kms) {
            for dpy in dpys {
                self.revoke_dpy(kms, dpy);
            }
        }
        self.grant_fds
            .retain(|_, s| !matches!(s, Source::Drm { kms: k, .. } if *k == kms));
    }

    /// `NvkmsPolicy::forget_handle`'s records.
    fn forget_handle(&mut self, h: u32) {
        self.typed.remove(&h);
        self.grant_fds.remove(&h);
        self.disps.retain(|&(m, _), _| m != h);
        self.gpus.retain(|&(m, _), _| m != h);
        self.perms.retain(|&(m, _), _| m != h);
        self.revoke_all_through(h);
        self.ended.remove(&h);
        if self.nvkms_granters.remove(&h) {
            self.forget_nvkms_grants();
        }
    }

    fn forget_nvkms_grants(&mut self) {
        self.perms.retain(|_, p| !p.from_nvkms);
        self.grant_fds
            .retain(|_, s| !matches!(s, Source::Nvkms { .. }));
    }

    fn forget_all_grants(&mut self) {
        self.perms.clear();
        self.grant_fds.clear();
    }
}

#[cfg(test)]
mod tests {
    //! The policy against params blocks laid out by the generated layouts:
    //! every refusal, every rewrite, and a grant's life from nvidia-drm's
    //! GRANT to its end. (A call's whole path through the interpreters is
    //! in i2_e2e.rs.)

    use super::*;

    const KMS: u32 = 10;
    const M: u32 = 20;
    const G: u32 = 30;
    const DEV: u32 = 1;
    const DISP: u32 = 0x100;
    const DPY: u32 = 1 << 3;

    fn v610() -> DriverVersion {
        DriverVersion::new(610, 57, 4)
    }

    fn lo(v: DriverVersion) -> &'static NvkmsLayout {
        schema::modeset_table(v).unwrap().nvkms.unwrap()
    }

    fn policy(v: DriverVersion) -> NvkmsPolicy {
        let p = NvkmsPolicy::new();
        p.set_version(v);
        p
    }

    fn size(v: DriverVersion, name: &str) -> usize {
        let t = schema::modeset_table(v).unwrap();
        let e = t.ioctls.iter().find(|e| e.name == name).unwrap();
        match t.fields(e.fields)[0].kind {
            Kind::Ptr { max, .. } => max as usize,
            _ => unreachable!(),
        }
    }

    fn put(b: &mut [u8], off: u32, v: u32) {
        wr32(b, off, v).unwrap();
    }

    impl NvkmsPolicy {
        fn check(
            &self,
            name: &str,
            target: u32,
            fds: &[u32],
            params: &mut [u8],
        ) -> Result<(), Errno> {
            let mut st = self.lock();
            let (lo, _) = Self::layout(&st).unwrap();
            let call = Call {
                target,
                fds,
                kms_card: self.kms_card.load(Ordering::Relaxed),
            };
            st.check(lo, name, &call, params)
        }

        fn record(&self, name: &str, target: u32, fds: &[u32], params: &[u8]) {
            let mut st = self.lock();
            let (lo, _) = Self::layout(&st).unwrap();
            st.record(lo, name, target, params, fds);
        }

        /// FLIP's pFlipHead array, as `before` hands it over.
        fn flip(&self, target: u32, heads: &mut [u8]) -> Result<(), Errno> {
            let mut st = self.lock();
            let (lo, _) = Self::layout(&st).unwrap();
            let call = Call {
                target,
                fds: &[],
                kms_card: self.kms_card.load(Ordering::Relaxed),
            };
            st.flip_heads(lo, &call, DEV, heads)
        }

        fn drm(&self, name: &str, kms: u32, fds: &[u32], arg: &[u8]) -> Result<(), Errno> {
            let call = Call {
                target: kms,
                fds,
                kms_card: false,
            };
            let r = self.lock().drm_check(name, &call, arg);
            if r.is_ok() {
                self.lock().drm_record(name, kms, arg, fds);
            }
            r
        }
    }

    /// nvidia-drm GRANT_PERMISSIONS {fd, dpyId, type}.
    fn drm_grant(dpy: u32, ty: u32) -> Vec<u8> {
        let mut a = vec![0u8; 12];
        put(&mut a, 4, dpy);
        put(&mut a, 8, ty);
        a
    }

    fn drm_revoke(dpy: u32, ty: u32) -> Vec<u8> {
        let mut a = vec![0u8; 8];
        put(&mut a, 0, dpy);
        put(&mut a, 4, ty);
        a
    }

    /// ALLOC_DEVICE's reply: deviceHandle DEV, disp 0's handle DISP.
    fn alloc_device(p: &NvkmsPolicy, v: DriverVersion, target: u32) {
        let l = lo(v);
        let mut a = vec![0u8; size(v, "NVKMS_ALLOC_DEVICE")];
        p.check("ALLOC_DEVICE", target, &[], &mut a).unwrap();
        put(&mut a, l.alloc_reply_device, DEV);
        put(&mut a, l.alloc_reply_disps, DISP);
        p.record("ALLOC_DEVICE", target, &[], &a);
    }

    /// ACQUIRE_PERMISSIONS on `target` with `g`, the host answering MODESET
    /// on head 1 of disp 0 for `dpy`.
    fn acquire(
        p: &NvkmsPolicy,
        v: DriverVersion,
        target: u32,
        g: u32,
        dpy: u32,
    ) -> Result<(), Errno> {
        let a = lo(v).acquire;
        let mut params = vec![0u8; size(v, "NVKMS_ACQUIRE_PERMISSIONS")];
        p.check("ACQUIRE_PERMISSIONS", target, &[g], &mut params)?;
        put(&mut params, a.device, DEV);
        put(&mut params, a.ptype, PERM_MODESET);
        let at = match a.modeset.disp {
            None => a.modeset.head.at(1),
            Some(d) => d.at(0) + a.modeset.head.at(1),
        };
        put(&mut params, at as u32, dpy);
        p.record("ACQUIRE_PERMISSIONS", target, &[g], &params);
        Ok(())
    }

    fn cursor(v: DriverVersion, head: u32) -> Vec<u8> {
        let t = lo(v).move_cursor;
        let mut a = vec![0u8; size(v, "NVKMS_MOVE_CURSOR")];
        put(&mut a, t.device, DEV);
        put(&mut a, t.disp, DISP);
        put(&mut a, t.what, head);
        a
    }

    fn dpy_attr(v: DriverVersion, dpy: u32) -> Vec<u8> {
        let t = lo(v).set_dpy_attribute;
        let mut a = vec![0u8; size(v, "NVKMS_SET_DPY_ATTRIBUTE")];
        put(&mut a, t.device, DEV);
        put(&mut a, t.disp, DISP);
        put(&mut a, t.what, dpy);
        a
    }

    /// The whole of it: nvidia-drm grants DPY through the lease to a fresh
    /// file, the NVKMS file acquires it, and exactly head 1 opens up.
    fn granted(v: DriverVersion) -> NvkmsPolicy {
        let p = policy(v);
        alloc_device(&p, v, M);
        p.drm("NV_GRANT_PERMISSIONS", KMS, &[G], &drm_grant(DPY, 2))
            .unwrap();
        acquire(&p, v, M, G, DPY).unwrap();
        p
    }

    const PROFILED: [(u32, u32, u32); 6] = [
        (535, 129, 3),
        (580, 178, 4),
        (595, 71, 5),
        (595, 99, 2),
        (610, 57, 4),
        (615, 71, 9),
    ];

    #[test]
    fn alloc_device_offers_only_coherent_display_memory_where_the_device_can_snoop() {
        for (a, b, c) in PROFILED {
            let v = DriverVersion::new(a, b, c);
            let l = lo(v);
            let at = l.alloc_reply_coherency as usize;
            let mut both = vec![0u8; size(v, "NVKMS_ALLOC_DEVICE")];
            // iso {coherent, noncoherent}, niso {coherent, noncoherent},
            // then displayIsGpuL2Coherent, which is not ours to touch.
            both[at..at + 5].copy_from_slice(&[1, 1, 1, 1, 1]);
            coherent_display_only(l, &mut both);
            assert_eq!(&both[at..at + 5], &[1, 0, 1, 0, 1], "{v:?}");

            // A device that cannot snoop keeps the only model it has.
            let mut only = vec![0u8; both.len()];
            only[at..at + 4].copy_from_slice(&[0, 1, 0, 1]);
            let before = only.clone();
            coherent_display_only(l, &mut only);
            assert_eq!(only, before, "{v:?}");
        }
    }

    #[test]
    fn the_coherency_modes_on_610_are_five_bytes_before_supports_syncpts() {
        // The layout derives the offset from supportsSyncpts; this pins what
        // that means against the offsets the probe measured for 610, where
        // supportsSyncpts is at 1419 and the reply starts at 624.
        assert_eq!(lo(v610()).alloc_reply_coherency, 1419 - 5);
    }

    #[test]
    fn a_v1_alloc_device_reply_is_narrowed_unless_guest_coherency_is_kept() {
        let v = v610();
        let l = lo(v);
        let cmd = schema::modeset_table(v)
            .unwrap()
            .ioctls
            .iter()
            .find(|e| e.name == "NVKMS_ALLOC_DEVICE")
            .unwrap()
            .nvkms_cmd;
        let at = 16 + l.alloc_reply_coherency as usize;
        let reply = || {
            let mut m = vec![0u8; 16 + size(v, "NVKMS_ALLOC_DEVICE")];
            put(&mut m, 0, cmd);
            m[at..at + 4].copy_from_slice(&[1, 1, 1, 1]);
            m
        };
        let p = policy(v);
        let mut m = reply();
        p.v1_after(&mut m);
        assert_eq!(&m[at..at + 4], &[1, 0, 1, 0]);

        p.set_coherent_display(false);
        let mut m = reply();
        p.v1_after(&mut m);
        assert_eq!(&m[at..at + 4], &[1, 1, 1, 1]);

        // Any other command's reply is left alone.
        p.set_coherent_display(true);
        let mut m = reply();
        put(&mut m, 0, cmd + 1);
        p.v1_after(&mut m);
        assert_eq!(&m[at..at + 4], &[1, 1, 1, 1]);
    }

    #[test]
    fn head_and_dpy_commands_pass_only_for_what_a_grant_covers() {
        for v in [
            v610(),
            DriverVersion::new(580, 178, 4),
            DriverVersion::new(535, 129, 3),
        ] {
            let p = policy(v);
            alloc_device(&p, v, M);
            assert_eq!(
                p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
                Err(libc::EPERM)
            );
            let p = granted(v);
            assert_eq!(
                p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
                Ok(()),
                "{v}"
            );
            assert_eq!(
                p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 0)),
                Err(libc::EPERM)
            );
            assert_eq!(
                p.check("SET_DPY_ATTRIBUTE", M, &[], &mut dpy_attr(v, DPY)),
                Ok(())
            );
            assert_eq!(
                p.check("SET_DPY_ATTRIBUTE", M, &[], &mut dpy_attr(v, DPY << 1)),
                Err(libc::EPERM),
                "another dpy"
            );
            assert_eq!(
                p.check(
                    "SET_DPY_ATTRIBUTE",
                    M,
                    &[],
                    &mut dpy_attr(v, DPY | DPY << 1)
                ),
                Err(libc::EPERM),
                "a dpyId is one bit"
            );
            // Another NVKMS file of the same guest holds nothing.
            alloc_device(&p, v, M + 5);
            assert_eq!(
                p.check("MOVE_CURSOR", M + 5, &[], &mut cursor(v, 1)),
                Err(libc::EPERM)
            );
        }
    }

    #[test]
    fn a_disp_handle_the_device_did_not_answer_with_is_refused() {
        let v = v610();
        let p = granted(v);
        let mut a = cursor(v, 1);
        put(&mut a, lo(v).move_cursor.disp, DISP + 1);
        assert_eq!(p.check("MOVE_CURSOR", M, &[], &mut a), Err(libc::EPERM));
    }

    #[test]
    fn set_layer_position_needs_every_named_head_granted() {
        let v = v610();
        let p = granted(v);
        let lp = lo(v).layer_position;
        let mut a = vec![0u8; size(v, "NVKMS_SET_LAYER_POSITION")];
        put(&mut a, lp.device, DEV);
        put(&mut a, lp.disps, 1);
        put(&mut a, lp.disp.at(0) as u32 + lp.heads, 1 << 1);
        assert_eq!(p.check("SET_LAYER_POSITION", M, &[], &mut a), Ok(()));
        put(&mut a, lp.disp.at(0) as u32 + lp.heads, 0b11);
        assert_eq!(
            p.check("SET_LAYER_POSITION", M, &[], &mut a),
            Err(libc::EPERM)
        );
        // The same head on a second disp: 610's grants are the device's
        // one disp's heads.
        put(&mut a, lp.disp.at(0) as u32 + lp.heads, 1 << 1);
        put(&mut a, lp.disp.at(1) as u32 + lp.heads, 1 << 1);
        put(&mut a, lp.disps, 0b11);
        assert_eq!(
            p.check("SET_LAYER_POSITION", M, &[], &mut a),
            Err(libc::EPERM)
        );
    }

    #[test]
    fn a_revoke_through_the_granting_handle_ends_the_grant() {
        let v = v610();
        let p = granted(v);
        assert_eq!(
            p.drm("NV_REVOKE_PERMISSIONS", KMS + 1, &[], &drm_revoke(DPY, 2)),
            Err(libc::EPERM),
            "only the handle that granted it may revoke it"
        );
        assert_eq!(
            p.drm("NV_REVOKE_PERMISSIONS", KMS, &[], &drm_revoke(DPY, 3)),
            Err(libc::EPERM)
        );
        assert_eq!(
            p.drm("NV_REVOKE_PERMISSIONS", KMS, &[], &drm_revoke(DPY, 2)),
            Ok(())
        );
        assert_eq!(
            p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
            Err(libc::EPERM)
        );
        assert_eq!(
            p.drm("NV_REVOKE_PERMISSIONS", KMS, &[], &drm_revoke(DPY, 2)),
            Err(libc::EPERM),
            "and it is not granted twice"
        );
    }

    // ── the dpy probe limit (S-8) ──

    /// QUERY_DPY_DYNAMIC_DATA of `dpy` on disp 0 of the file's device.
    fn dyn_query(v: DriverVersion, dpy: u32) -> Vec<u8> {
        let t = lo(v).dpy_dynamic;
        let mut q = vec![0u8; size(v, "NVKMS_QUERY_DPY_DYNAMIC_DATA")];
        put(&mut q, t.device, DEV);
        put(&mut q, t.disp, DISP);
        put(&mut q, t.what, dpy);
        q
    }

    impl NvkmsPolicy {
        /// `before`'s probe decision for a QUERY_DPY_DYNAMIC_DATA on
        /// `target` at `now`: true if answered from the last reply.
        fn probe(&self, target: u32, q: &mut [u8], now: Instant) -> bool {
            let mut st = self.lock();
            let (lo, _) = Self::layout(&st).unwrap();
            let call = Call {
                target,
                fds: &[],
                kms_card: self.kms_card.load(Ordering::Relaxed),
            };
            st.dpy_probe(lo, &call, q, now)
        }

        /// The host's answer to it: the reply half filled with `byte`.
        fn answer(&self, v: DriverVersion, target: u32, q: &mut [u8], byte: u8) {
            let (off, len) = lo(v).dpy_dynamic_reply;
            q[off as usize..(off + len) as usize].fill(byte);
            self.record("QUERY_DPY_DYNAMIC_DATA", target, &[], q);
        }
    }

    fn reply_byte(v: DriverVersion, q: &[u8]) -> u8 {
        let (off, len) = lo(v).dpy_dynamic_reply;
        let r = &q[off as usize..(off + len) as usize];
        assert!(r.iter().all(|&b| b == r[0]), "the whole reply, not part");
        r[0]
    }

    #[test]
    fn a_dpy_is_probed_once_per_window_and_answered_with_the_last_reply_between() {
        let v = v610();
        let p = granted(v);
        let other = 1 << 5;
        let t0 = Instant::now();
        for (dpy, every) in [(DPY, PROBE_EVERY_GRANTED), (other, PROBE_EVERY_OTHER)] {
            let mut q = dyn_query(v, dpy);
            assert!(!p.probe(M, &mut q, t0), "the first probe goes to the host");
            p.answer(v, M, &mut q, 0x5a);
            let mut again = dyn_query(v, dpy);
            assert!(p.probe(M, &mut again, t0 + every / 2));
            assert_eq!(reply_byte(v, &again), 0x5a);
            let mut due = dyn_query(v, dpy);
            assert!(!p.probe(M, &mut due, t0 + every), "{dpy:#x} is due again");
            assert_eq!(reply_byte(v, &due), 0, "and the host answers it");
        }
    }

    #[test]
    fn the_limit_is_the_vms_not_the_files() {
        // A second file of the same VM, its own device on the same GPU: the
        // dpy is the same dpy, and the guest opening more files buys it no
        // more probes.
        let v = v610();
        let p = policy(v);
        alloc_device(&p, v, M);
        alloc_device(&p, v, M + 1);
        let t0 = Instant::now();
        let mut q = dyn_query(v, DPY);
        assert!(!p.probe(M, &mut q, t0));
        p.answer(v, M, &mut q, 7);
        let mut q = dyn_query(v, DPY);
        assert!(p.probe(M + 1, &mut q, t0 + Duration::from_millis(1)));
        assert_eq!(reply_byte(v, &q), 7);
    }

    /// The dpyId is the guest's, and a record was kept of every one it
    /// named: bounded now, the ones the host never answered going first.
    #[test]
    fn probes_of_dpys_the_host_never_answers_are_not_kept_without_bound() {
        let v = v610();
        let p = granted(v);
        let t0 = Instant::now();
        let mut q = dyn_query(v, DPY);
        assert!(!p.probe(M, &mut q, t0));
        p.answer(v, M, &mut q, 0x5a);
        for i in 0..4 * DPY_PROBES_KEPT as u32 {
            // Made-up dpyIds, which the host refuses.
            let mut q = dyn_query(v, 0x1_0000 + i);
            p.probe(M, &mut q, t0);
        }
        assert!(p.lock().dpy_probes.len() <= DPY_PROBES_KEPT);
        let mut again = dyn_query(v, DPY);
        assert!(
            p.probe(M, &mut again, t0 + PROBE_EVERY_GRANTED / 2),
            "the answered dpy keeps its limit"
        );
    }

    #[test]
    fn a_probe_still_running_does_not_stop_the_next_reaching_the_host() {
        // Nothing to answer with yet: the host must.
        let v = v610();
        let p = granted(v);
        let t0 = Instant::now();
        assert!(!p.probe(M, &mut dyn_query(v, DPY), t0));
        assert!(!p.probe(M, &mut dyn_query(v, DPY), t0));
    }

    #[test]
    fn a_dpy_event_sends_the_next_probe_of_every_dpy_to_the_host() {
        let v = v610();
        let l = lo(v);
        let p = granted(v);
        let t0 = Instant::now();
        let mut q = dyn_query(v, DPY);
        assert!(!p.probe(M, &mut q, t0));
        p.answer(v, M, &mut q, 1);
        let mut ev = vec![0u8; size(v, "NVKMS_GET_NEXT_EVENT")];
        ev[l.next_event_valid as usize] = 1;
        // FLIP_OCCURRED (5) says nothing about any dpy.
        put(&mut ev, l.next_event_type, 5);
        p.record("GET_NEXT_EVENT", M, &[], &ev);
        assert!(p.probe(M, &mut dyn_query(v, DPY), t0));
        // DPY_CHANGED (0) does.
        put(&mut ev, l.next_event_type, 0);
        p.record("GET_NEXT_EVENT", M, &[], &ev);
        assert!(!p.probe(M, &mut dyn_query(v, DPY), t0));
    }

    #[test]
    fn a_dpy_of_a_device_the_file_never_allocated_goes_to_the_host_to_refuse() {
        let v = v610();
        let p = policy(v);
        let t0 = Instant::now();
        assert!(!p.probe(M, &mut dyn_query(v, DPY), t0));
        assert!(!p.probe(M, &mut dyn_query(v, DPY), t0));
    }

    #[test]
    fn a_revocation_waits_for_the_gated_call_running_and_refuses_those_queued() {
        use std::sync::{Arc, mpsc};
        use std::time::Duration;
        let p = Arc::new(granted(v610()));
        let seen = p.revocations.load(Ordering::SeqCst);
        let running = p.hold_unless_revoked(seen).unwrap();
        let (tx, rx) = mpsc::channel();
        let closer = {
            let p = p.clone();
            std::thread::spawn(move || {
                p.forget_handle(KMS);
                tx.send(()).unwrap();
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "the lease's close waits for the call already on the host"
        );
        drop(running);
        rx.recv().unwrap();
        closer.join().unwrap();
        assert_eq!(p.hold_unless_revoked(seen).err(), Some(libc::EPERM));
    }

    #[test]
    fn a_close_or_lease_end_that_takes_nothing_back_leaves_queued_calls_alone() {
        let p = granted(v610());
        let seen = p.revocations.load(Ordering::SeqCst);
        // M holds the grant, it did not make it; 99 is nobody.
        p.forget_handle(M);
        p.forget_handle(99);
        p.lease_ended(99);
        assert!(p.hold_unless_revoked(seen).is_ok());
        let p = granted(v610());
        let seen = p.revocations.load(Ordering::SeqCst);
        p.lease_ended(KMS);
        assert!(p.hold_unless_revoked(seen).is_err());
    }

    #[test]
    fn closing_the_granting_handle_or_ending_its_lease_ends_the_grant() {
        let v = v610();
        let p = granted(v);
        p.forget_handle(KMS);
        assert_eq!(
            p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
            Err(libc::EPERM)
        );
        let p = granted(v);
        p.lease_ended(KMS);
        assert_eq!(
            p.check("SET_DPY_ATTRIBUTE", M, &[], &mut dpy_attr(v, DPY)),
            Err(libc::EPERM)
        );
    }

    #[test]
    fn free_device_close_and_reset_forget_what_the_file_held() {
        let v = v610();
        let p = granted(v);
        let mut free = vec![0u8; 8];
        put(&mut free, 0, DEV);
        p.check("FREE_DEVICE", M, &[], &mut free).unwrap();
        alloc_device(&p, v, M);
        assert_eq!(
            p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
            Err(libc::EPERM),
            "a new device under the same handle holds nothing"
        );
        let p = granted(v);
        p.forget_handle(M);
        alloc_device(&p, v, M);
        assert_eq!(
            p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
            Err(libc::EPERM)
        );
        let p = granted(v);
        p.reset();
        assert!(!p.is_typed(M));
        assert_eq!(p.lock().version, Some(v), "the host is the same host");
    }

    #[test]
    fn an_owner_revoking_or_releasing_forgets_every_grant() {
        let v = v610();
        for name in ["REVOKE_PERMISSIONS", "RELEASE_OWNERSHIP"] {
            let p = granted(v);
            p.record(
                name,
                M + 1,
                &[],
                &vec![0u8; size(v, &format!("NVKMS_{name}"))],
            );
            assert_eq!(
                p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
                Err(libc::EPERM)
            );
        }
    }

    #[test]
    fn an_acquire_of_a_file_nobody_granted_to_is_refused() {
        let v = v610();
        let p = policy(v);
        alloc_device(&p, v, M);
        assert_eq!(acquire(&p, v, M, G, DPY), Err(libc::EPERM));
    }

    #[test]
    fn grants_from_an_nvkms_owner_go_when_the_owner_does() {
        let v = v610();
        let p = policy(v);
        p.set_kms_card(true);
        let owner = M + 7;
        let mut g = vec![0u8; size(v, "NVKMS_GRANT_PERMISSIONS")];
        p.check("GRANT_PERMISSIONS", owner, &[G], &mut g).unwrap();
        p.record("GRANT_PERMISSIONS", owner, &[G], &g);
        alloc_device(&p, v, M);
        acquire(&p, v, M, G, DPY).unwrap();
        p.set_kms_card(false);
        assert_eq!(p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)), Ok(()));
        p.forget_handle(owner);
        assert_eq!(
            p.check("MOVE_CURSOR", M, &[], &mut cursor(v, 1)),
            Err(libc::EPERM)
        );
    }

    #[test]
    fn a_file_an_ioctl_ran_on_is_never_a_grant_file() {
        let v = v610();
        let p = policy(v);
        alloc_device(&p, v, M);
        assert!(p.is_typed(M));
        // As nvidia-drm's grant fd, as NVKMS's, as a unicast event file.
        assert_eq!(
            p.drm("NV_GRANT_PERMISSIONS", KMS, &[M], &drm_grant(DPY, 2)),
            Err(libc::EPERM)
        );
        let mut notify = vec![0u8; size(v, "NVKMS_NOTIFY_VBLANK")];
        assert_eq!(
            p.check("NOTIFY_VBLANK", M + 1, &[M], &mut notify),
            Err(libc::EPERM)
        );
        // Nor the call's own file, typed or not.
        assert_eq!(
            p.check("NOTIFY_VBLANK", M + 2, &[M + 2], &mut notify),
            Err(libc::EPERM)
        );
        // A fresh one is fine, and stays fresh: it is not the call's target.
        assert_eq!(p.check("NOTIFY_VBLANK", M + 1, &[G], &mut notify), Ok(()));
        assert!(!p.is_typed(G));
        // A closed handle's number starts over.
        p.forget_handle(M);
        assert!(!p.is_typed(M));
    }

    #[test]
    fn nvidia_drm_grants_are_modeset_only_and_untyped_ones_only_where_the_host_has_them() {
        let v = v610();
        let p = policy(v);
        assert_eq!(
            p.drm("NV_GRANT_PERMISSIONS", KMS, &[G], &drm_grant(DPY, 3)),
            Err(libc::EPERM),
            "SUB_OWNER"
        );
        assert_eq!(
            p.drm("NV_GRANT_PERMISSIONS", KMS, &[G], &drm_grant(DPY, 0)),
            Err(libc::EPERM)
        );
        let untyped = drm_grant(DPY, 0)[..8].to_vec();
        assert_eq!(
            p.drm("NV_GRANT_PERMISSIONS_UNTYPED", KMS, &[G], &untyped),
            Err(libc::EINVAL)
        );
        let old = policy(DriverVersion::new(535, 129, 3));
        assert_eq!(
            old.drm("NV_GRANT_PERMISSIONS_UNTYPED", KMS, &[G], &untyped),
            Ok(())
        );
        assert_eq!(
            old.drm(
                "NV_REVOKE_PERMISSIONS_UNTYPED",
                KMS,
                &[],
                &DPY.to_le_bytes()
            ),
            Ok(())
        );
        assert!(old.lock().drm_grants[&KMS].is_empty());
        // A typed one on the old host still says MODESET, which is what the
        // old nvidia-drm always does with it.
        assert_eq!(
            old.drm("NV_GRANT_PERMISSIONS", KMS, &[G], &drm_grant(DPY, 2)),
            Ok(())
        );
    }

    #[test]
    fn owner_only_commands_need_kms_card() {
        let v = v610();
        let p = policy(v);
        for name in [
            "GRAB_OWNERSHIP",
            "SET_DISP_ATTRIBUTE",
            "SET_FRAMELOCK_ATTRIBUTE",
        ] {
            let mut a = vec![0u8; size(v, &format!("NVKMS_{name}"))];
            assert_eq!(p.check(name, M, &[], &mut a), Err(libc::EPERM), "{name}");
            p.set_kms_card(true);
            assert_eq!(p.check(name, M, &[], &mut a), Ok(()), "{name}");
            p.set_kms_card(false);
        }
    }

    #[test]
    fn kernel_client_and_device_global_commands_are_refused_by_name() {
        let p = policy(v610());
        p.set_kms_card(true);
        for name in REFUSED {
            assert_eq!(
                p.check(name, M, &[], &mut [0u8; 64]),
                Err(libc::EPERM),
                "{name}"
            );
        }
    }

    #[test]
    fn query_dpy_dynamic_data_loses_its_overrides_and_keeps_the_rest() {
        for v in [
            v610(),
            DriverVersion::new(615, 71, 9),
            DriverVersion::new(535, 129, 3),
        ] {
            let p = policy(v);
            p.set_kms_card(true);
            let mut a = vec![0xa5u8; size(v, "NVKMS_QUERY_DPY_DYNAMIC_DATA")];
            p.check("QUERY_DPY_DYNAMIC_DATA", M, &[], &mut a).unwrap();
            // deviceHandle, dispHandle, dpyId as sent; forceConnected (12)
            // through the whole EDID (22 + 2048) cleared.
            assert!(a[..12].iter().all(|&b| b == 0xa5), "{v}");
            assert!(a[12..2070].iter().all(|&b| b == 0), "{v}");
        }
    }

    #[test]
    fn alloc_device_loses_its_device_wide_knobs_outside_kms_card() {
        let v = v610();
        let p = policy(v);
        let mut a = vec![0xa5u8; size(v, "NVKMS_ALLOC_DEVICE")];
        p.check("ALLOC_DEVICE", M, &[], &mut a).unwrap();
        assert!(
            a[..40].iter().all(|&b| b == 0xa5),
            "versionString and deviceId as sent"
        );
        assert!(
            a[40..42].iter().all(|&b| b == 0),
            "no3d, enableConsoleHotplugHandling"
        );
        assert!(a[44..44 + 16 * 36].iter().all(|&b| b == 0), "registryKeys");
        p.set_kms_card(true);
        let mut a = vec![0xa5u8; size(v, "NVKMS_ALLOC_DEVICE")];
        p.check("ALLOC_DEVICE", M + 1, &[], &mut a).unwrap();
        assert!(a.iter().all(|&b| b == 0xa5));
    }

    #[test]
    fn event_interest_is_cut_to_what_a_display_client_needs() {
        let v = DriverVersion::new(615, 71, 9);
        let p = policy(v);
        let mut a = vec![0u8; 8];
        put(&mut a, 0, 0xff);
        p.check("DECLARE_EVENT_INTEREST", M, &[], &mut a).unwrap();
        // DPY_CHANGED, DYNAMIC_DPY_(DIS)CONNECTED, FLIP_OCCURRED; never the
        // attribute events, nor 615's CP_TOPOLOGY (7), which carries a
        // kernel pointer.
        assert_eq!(rd32(&a, 0), Ok(0b10_0111));
    }

    /// pFlipHead elements, each on (sd, head).
    fn flip_heads(v: DriverVersion, on: &[(u32, u32)]) -> Vec<u8> {
        let f = lo(v).flip;
        let mut heads = vec![0u8; on.len() * f.head_size as usize];
        for (e, &(sd, head)) in on.iter().enumerate() {
            let base = e as u32 * f.head_size;
            put(&mut heads, base + f.sd, sd);
            put(&mut heads, base + f.head, head);
        }
        heads
    }

    /// Byte `field` (relative to the layer) of layer `l` of element `e`.
    fn flip_byte(v: DriverVersion, e: usize, l: u32, field: u32) -> usize {
        let f = lo(v).flip;
        e * f.head_size as usize + f.layer.at(l) + field as usize
    }

    /// SET_MODE on device DEV requesting `heads` (disp, head, dpyIdList).
    fn set_mode(v: DriverVersion, commit: bool, heads: &[(u32, u32, u32)]) -> Vec<u8> {
        let m = lo(v).set_mode;
        let mut a = vec![0u8; size(v, "NVKMS_SET_MODE")];
        put(&mut a, m.device, DEV);
        a[m.commit as usize] = commit as u8;
        for &(d, h, dpys) in heads {
            let disps = rd32(&a, m.disps).unwrap();
            put(&mut a, m.disps, disps | 1 << d);
            let at = m.disp.at(d) as u32 + m.heads;
            let hs = rd32(&a, at).unwrap();
            put(&mut a, at, hs | 1 << h);
            put(&mut a, (m.disp.at(d) + m.head.at(h)) as u32 + m.dpys, dpys);
        }
        a
    }

    fn set_mode_byte(v: DriverVersion, d: u32, h: u32, l: u32, field: u32) -> usize {
        let m = lo(v).set_mode;
        m.disp.at(d) + m.head.at(h) + m.layer.at(l) + field as usize
    }

    /// NVKMS reads useSyncpt only where the layer's syncObjects are
    /// specified (nvkms-hw-flip.c:714-718, nvkms-modeset.c:246, 4013), and
    /// only in the heads a SET_MODE requests; a stray byte anywhere else is
    /// nothing the host would act on.
    #[test]
    fn a_tegra_syncpoint_is_refused_only_where_the_host_would_read_it() {
        let v = v610();
        let f = lo(v).flip;
        let m = lo(v).set_mode;
        let p = policy(v);
        p.set_kms_card(true);
        let mut heads = flip_heads(v, &[(0, 0), (0, 1)]);
        heads[flip_byte(v, 1, 7, f.use_syncpt)] = 1;
        assert_eq!(p.flip(M, &mut heads), Ok(()), "not specified");
        heads[flip_byte(v, 1, 7, f.sync_specified)] = 1;
        assert_eq!(p.flip(M, &mut heads), Err(libc::EPERM));
        heads[flip_byte(v, 1, 7, f.use_syncpt)] = 0;
        assert_eq!(p.flip(M, &mut heads), Ok(()), "specified semaphores");

        let mut sm = set_mode(v, true, &[(0, 1, DPY)]);
        assert_eq!(p.check("SET_MODE", M, &[], &mut sm), Ok(()));
        sm[set_mode_byte(v, 0, 1, 7, m.use_syncpt)] = 1;
        assert_eq!(
            p.check("SET_MODE", M, &[], &mut sm),
            Ok(()),
            "not specified"
        );
        sm[set_mode_byte(v, 0, 1, 7, m.sync_specified)] = 1;
        assert_eq!(p.check("SET_MODE", M, &[], &mut sm), Err(libc::EPERM));
        // The same layer of a head the request does not name.
        let mut sm = set_mode(v, true, &[(0, 1, DPY)]);
        sm[set_mode_byte(v, 7, 3, 7, m.use_syncpt)] = 1;
        sm[set_mode_byte(v, 7, 3, 7, m.sync_specified)] = 1;
        assert_eq!(p.check("SET_MODE", M, &[], &mut sm), Ok(()));
    }

    /// NVKMS checks a flip only for the layers it dirties: a cursor, HDR
    /// or colorimetry element names its head and nothing else. So the head
    /// itself must be granted, for every element, before 595 per (sd,
    /// head), from 595 per head whatever the sd.
    #[test]
    fn every_flip_element_must_name_a_head_a_grant_covers_whatever_it_changes() {
        for (v, per_head) in [
            (v610(), true),
            (DriverVersion::new(595, 71, 5), true),
            (DriverVersion::new(580, 178, 4), false),
            (DriverVersion::new(535, 129, 3), false),
        ] {
            let p = policy(v);
            alloc_device(&p, v, M);
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(0, 1)])),
                Err(libc::EPERM),
                "{v}: no grant yet"
            );
            let p = granted(v);
            assert_eq!(p.flip(M, &mut flip_heads(v, &[(0, 1)])), Ok(()), "{v}");
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(0, 0)])),
                Err(libc::EPERM),
                "{v}: the host compositor's head"
            );
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(0, 1), (0, 0)])),
                Err(libc::EPERM),
                "{v}: one element on another head refuses the lot"
            );
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(1, 1)])).is_ok(),
                per_head,
                "{v}: another subdevice"
            );
            assert_eq!(
                p.flip(M + 5, &mut flip_heads(v, &[(0, 1)])),
                Err(libc::EPERM),
                "{v}: another file"
            );
            p.set_kms_card(true);
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(0, 0)])),
                Ok(()),
                "{v}: the guest owns the display"
            );
            p.set_kms_card(false);
            p.lease_ended(KMS);
            assert_eq!(
                p.flip(M, &mut flip_heads(v, &[(0, 1)])),
                Err(libc::EPERM),
                "{v}: the lease ended, though the host may still have the grant"
            );
        }
    }

    /// FLIP_OCCURRED from a guest flip reaches only nvidia-drm's own open
    /// (a modeset grantee has no flip permission), which WARNs on it.
    #[test]
    fn awaken_is_cleared_in_every_flip_and_modeset_layer_outside_kms_card() {
        let v = v610();
        let f = lo(v).flip;
        let m = lo(v).set_mode;
        let p = granted(v);
        let mut heads = flip_heads(v, &[(0, 1), (0, 1)]);
        for (e, l) in [(0, 0), (1, 7)] {
            heads[flip_byte(v, e, l, f.awaken)] = 1;
        }
        let before = heads.clone();
        assert_eq!(p.flip(M, &mut heads), Ok(()));
        assert_eq!(heads[flip_byte(v, 0, 0, f.awaken)], 0);
        assert_eq!(heads[flip_byte(v, 1, 7, f.awaken)], 0);
        let changed = before.iter().zip(&heads).filter(|(a, b)| a != b).count();
        assert_eq!(changed, 2, "nothing else is touched");

        let mut sm = set_mode(v, true, &[(0, 1, DPY)]);
        sm[set_mode_byte(v, 0, 1, 3, m.awaken)] = 1;
        assert_eq!(p.check("SET_MODE", M, &[], &mut sm), Ok(()));
        assert_eq!(sm[set_mode_byte(v, 0, 1, 3, m.awaken)], 0);

        p.set_kms_card(true);
        let mut heads = flip_heads(v, &[(0, 1)]);
        heads[flip_byte(v, 0, 0, f.awaken)] = 1;
        assert_eq!(p.flip(M, &mut heads), Ok(()));
        assert_eq!(heads[flip_byte(v, 0, 0, f.awaken)], 1, "the guest owns it");
        let mut sm = set_mode(v, true, &[(0, 1, DPY)]);
        sm[set_mode_byte(v, 0, 1, 3, m.awaken)] = 1;
        assert_eq!(p.check("SET_MODE", M, &[], &mut sm), Ok(()));
        assert_eq!(sm[set_mode_byte(v, 0, 1, 3, m.awaken)], 1);
    }

    /// ValidateRequest's rule (nvkms-modeset.c:3940-3966), against the
    /// records rather than the host's set: a lease that ended ends it here.
    #[test]
    fn a_committed_set_mode_needs_every_requested_head_and_dpy_granted() {
        for v in [
            v610(),
            DriverVersion::new(580, 178, 4),
            DriverVersion::new(535, 129, 3),
        ] {
            let p = granted(v);
            let ok = |heads: &[(u32, u32, u32)], commit| {
                p.check("SET_MODE", M, &[], &mut set_mode(v, commit, heads))
            };
            assert_eq!(ok(&[(0, 1, DPY)], true), Ok(()), "{v}");
            assert_eq!(ok(&[(0, 1, 0)], true), Ok(()), "{v}: shutting it down");
            assert_eq!(ok(&[(0, 1, DPY << 1)], true), Err(libc::EPERM), "{v}");
            assert_eq!(ok(&[(0, 1, DPY | DPY << 1)], true), Err(libc::EPERM), "{v}");
            assert_eq!(ok(&[(0, 0, 0)], true), Err(libc::EPERM), "{v}");
            assert_eq!(
                ok(&[(0, 1, DPY), (0, 2, 0)], true),
                Err(libc::EPERM),
                "{v}: every requested head"
            );
            assert_eq!(
                ok(&[(0, 0, DPY << 1)], false),
                Ok(()),
                "{v}: validation only needs no permission"
            );
            p.lease_ended(KMS);
            assert_eq!(ok(&[(0, 1, DPY)], true), Err(libc::EPERM), "{v}");
            p.set_kms_card(true);
            assert_eq!(ok(&[(0, 0, DPY << 1)], true), Ok(()), "{v}");
        }
    }

    /// The host may still hold a grant whose lease ended (kms.rs, "lease
    /// ends"), so the handle it went through stays on the list the backend
    /// asks about, until it closes; one that granted nothing never joins.
    #[test]
    fn a_handle_whose_lease_ended_after_granting_is_asked_about_until_it_closes() {
        let v = v610();
        let p = granted(v);
        assert_eq!(p.granting_handles(), vec![KMS]);
        assert!(p.granted_through(KMS));
        p.lease_ended(KMS);
        assert_eq!(p.granting_handles(), vec![KMS]);
        assert!(p.granted_through(KMS));
        p.lease_ended(KMS);
        assert_eq!(p.granting_handles(), vec![KMS], "asked again, still there");
        p.forget_handle(KMS);
        assert!(p.granting_handles().is_empty());
        assert!(!p.granted_through(KMS));

        p.lease_ended(KMS + 1);
        assert!(p.granting_handles().is_empty());
        assert!(!p.granted_through(KMS + 1));
    }

    #[test]
    fn v1_takes_only_pointer_and_descriptor_free_commands_of_the_right_size() {
        let v = v610();
        let p = policy(v);
        let msg = |cmd: u32, size: usize| {
            let mut m = vec![0u8; 16 + size];
            put(&mut m, 0, cmd);
            put(&mut m, 4, size as u32);
            m
        };
        // QUERY_CONNECTOR_STATIC_DATA (3, 44 bytes): plain.
        assert_eq!(p.v1_before(M, &mut msg(3, 44)), Ok(()));
        assert!(p.is_typed(M));
        assert_eq!(p.v1_before(M, &mut msg(3, 40)), Err(libc::EINVAL));
        // FLIP (15) has pointers, REGISTER_SURFACE (17) descriptors.
        assert_eq!(p.v1_before(M, &mut msg(15, 3104)), Err(libc::EPERM));
        assert_eq!(p.v1_before(M, &mut msg(17, 152)), Err(libc::EPERM));
        // The same policy as IOCTL2: MOVE_CURSOR (11) with no grant.
        assert_eq!(p.v1_before(M, &mut msg(11, 20)), Err(libc::EPERM));
        // FRAMEBUFFER_CONSOLE_DISABLED (63): in no table.
        assert_eq!(p.v1_before(M, &mut msg(63, 8)), Err(libc::EPERM));
        // And QUERY_DPY_DYNAMIC_DATA is scrubbed in place.
        let mut q = msg(6, 37168);
        q[16 + 12] = 1;
        assert_eq!(p.v1_before(M, &mut q), Ok(()));
        assert_eq!(q[16 + 12], 0);
        // No table, no NVKMS.
        let none = NvkmsPolicy::new();
        assert_eq!(none.v1_before(M, &mut msg(3, 44)), Err(libc::EPERM));
    }

    #[test]
    fn v1_on_a_host_between_releases_runs_only_commands_that_did_not_move() {
        let p = policy(DriverVersion::new(610, 80, 0));
        let mut m = vec![0u8; 16 + 44];
        put(&mut m, 0, 3);
        put(&mut m, 4, 44);
        assert_eq!(
            p.v1_before(M, &mut m),
            Ok(()),
            "QUERY_CONNECTOR_STATIC_DATA did not move"
        );
        let mut m = vec![0u8; 16 + 48];
        put(&mut m, 0, 32);
        put(&mut m, 4, 48);
        assert_eq!(
            p.v1_before(M, &mut m),
            Err(libc::EPERM),
            "GET_NEXT_EVENT did, in 615"
        );
    }

    #[test]
    fn hello_offers_an_nvkms_table_for_every_host_a_table_covers() {
        use protocol::messages::BCAP_NVKMS_TABLE;
        for (host, offered) in [
            ("610.57.04", true),
            ("612.10.01", true),
            ("999.1.1", true),
            ("535.129.03", true),
            ("535.104.05", false),
            ("470.256.02", false),
        ] {
            let mut be = crate::nvidia::NvidiaBackend::for_test();
            be.set_host_driver_version(host);
            assert_eq!(
                be.config().caps() & BCAP_NVKMS_TABLE != 0,
                offered,
                "{host}"
            );
            // And the policy runs on the same version's layout.
            assert!(be.nvkms.lock().version.is_some());
        }
    }
}
