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
//!   under -- whenever an owner revokes wholesale.
//! - **Refusals.** GRAB_OWNERSHIP, SET_DISP_ATTRIBUTE and
//!   SET_FRAMELOCK_ATTRIBUTE only in compositor-VM mode (`--kms-card`), where
//!   the guest owns the display anyway; the head and dpy gates are lifted
//!   there too. Kernel-client and device-global commands never (they are in
//!   no table either; named here as a second fence). Tegra syncpoints never
//!   (a dGPU host refuses `useSyncpt` itself, nvkms-hw-flip.c:716-719, but
//!   the descriptor behind it is one the schema does not translate).
//! - **Rewrites.** QUERY_DPY_DYNAMIC_DATA's overrides are cleared, always.
//!   ALLOC_DEVICE's device-wide knobs (registry keys, console hotplugs, no3d)
//!   are cleared outside `--kms-card`: they apply when the call creates the
//!   device (nvkms-evo.c:9043, nvkms.c:1417). DECLARE_EVENT_INTEREST is cut
//!   to the events a display client needs: NVKMS's per-open event list has no
//!   bound (nvkms.c:6422-6435).
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
//! nothing else; the lock is only for the `Arc` it lives in.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use abi::version::DriverVersion;

use crate::schema::{self, Kind, NvkmsLayout, NvkmsPermArr, NvkmsTarget, policy};
use crate::xfer::{Errno, Prepared};

/// Most `/dev/nvidia-modeset` files one VM may hold open. Each is a host
/// NVKMS open with its own unbounded event list and grant state; a guest
/// needs one per display client plus a few short-lived grant files.
pub const MAX_MODESET_OPENS: usize = 64;

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
    perms: HashMap<(u32, u32), Perms>,
}

/// The NVKMS section's state, shared by the policy object and the backend
/// (which tells it the host version, the mode, and every handle it closes).
#[derive(Default)]
pub struct NvkmsPolicy {
    kms_card: AtomicBool,
    state: Mutex<State>,
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

/// No layer of any FLIP head (pFlipHead's array) asks for a Tegra
/// syncpoint: the fence descriptor behind one is not in the schema, so it
/// would reach the host as the guest's own number.
fn flip_heads_ok(lo: &NvkmsLayout, heads: &[u8]) -> Result<(), Errno> {
    let f = lo.flip;
    for (e, head) in heads.chunks(f.head_size as usize).enumerate() {
        for l in 0..f.layer.count {
            if rd(head, f.layer.at(l) + f.use_syncpt as usize, 1)? != 0 {
                return Err(refuse(format_args!(
                    "FLIP head {e} layer {l} asks for a Tegra syncpoint"
                )));
            }
        }
    }
    Ok(())
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

    /// The host driver version, which picks the layout every offset below
    /// comes from.
    pub fn set_version(&self, v: DriverVersion) {
        self.lock().version = Some(v);
    }

    /// Compositor-VM mode: the guest owns the display.
    pub fn set_kms_card(&self, on: bool) {
        self.kms_card.store(on, Ordering::Relaxed);
    }

    /// A handle closed: everything that was its, or granted through it,
    /// goes -- as nvidia-drm's postclose revokes what a DRM file granted
    /// (nvidia-drm-drv.c:1588-1600) and nvKmsClose frees a file's devices
    /// and permissions (nvkms.c:5254-5308).
    pub fn forget_handle(&self, h: u32) {
        let mut st = self.lock();
        st.typed.remove(&h);
        st.grant_fds.remove(&h);
        st.disps.retain(|&(m, _), _| m != h);
        st.perms.retain(|&(m, _), _| m != h);
        st.revoke_all_through(h);
        if st.nvkms_granters.remove(&h) {
            st.forget_nvkms_grants();
        }
    }

    /// A lease or card handle stopped holding what it held (the lessor
    /// revoked the lease, the master dropped): its grants go as if it had
    /// closed. The backend calls it when GET_LEASE on the handle says so
    /// (kms.rs, "lease ends"); CLOSE does it anyway.
    pub fn lease_ended(&self, kms: u32) {
        self.lock().revoke_all_through(kms);
    }

    /// KMS handles that granted something still recorded (nvidia-drm
    /// GRANT_PERMISSIONS): whose leases the backend re-checks before an
    /// NVKMS call relies on them (kms.rs, "lease ends").
    pub fn granting_handles(&self) -> Vec<u32> {
        let st = self.lock();
        let mut v: Vec<u32> = st.drm_grants.keys().copied().collect();
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
        let mut st = self.lock();
        let version = st.version;
        *st = State {
            version,
            ..State::default()
        };
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
        let mut st = self.lock();
        let pol = p.policy();
        if pol & (policy::GRANT | policy::REVOKE) != 0 {
            let arg = p.buffer(0).unwrap_or(&[]);
            return st.drm_check(p.name(), &call, arg);
        }
        let name = p.name().strip_prefix("NVKMS_").unwrap_or(p.name());
        let (lo, exact) = Self::layout(&st)?;
        if pol & policy::NVKMS_EXACT != 0 && !exact {
            return Err(refuse(format_args!(
                "{name}'s layout moved after this host's table was measured, and this host \
                 ({:?}) is not that release",
                st.version
            )));
        }
        if name == "FLIP" {
            // pFlipHead's array, if it got a buffer (a NULL one reaches the
            // host as NULL and fails there).
            if let Some(heads) = p.pointee(1, lo.flip.ptr as usize).and_then(|b| p.buffer(b)) {
                flip_heads_ok(lo, heads)?;
            }
        }
        let params = p.buffer_mut(1).ok_or(libc::EINVAL)?;
        st.check(lo, name, &call, params)
    }

    /// `Hooks::after`: what the host granted, allocated or revoked.
    pub fn after(&self, p: &Prepared, ret: i32) {
        if ret != 0 {
            return;
        }
        let fds: Vec<u32> = p.fd_in_handles().map(|(_, _, h)| h).collect();
        let mut st = self.lock();
        if p.policy() & (policy::GRANT | policy::REVOKE) != 0 {
            st.drm_record(p.name(), p.target(), p.buffer(0).unwrap_or(&[]), &fds);
            return;
        }
        let name = p.name().strip_prefix("NVKMS_").unwrap_or(p.name());
        let Ok((lo, _)) = Self::layout(&st) else {
            return;
        };
        if let Some(params) = p.buffer(1) {
            st.record(lo, name, p.target(), params, &fds);
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
            st.forget_all_grants();
        }
        Ok(())
    }
}

impl State {
    /// The policy for one NVKMS call on `call.target`, over its params block.
    /// (FLIP's heads, behind a pointer, are `flip_heads_ok`'s.)
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
            "SET_MODE" => {
                let m = lo.set_mode;
                for d in 0..m.disp.count {
                    for h in 0..m.head.count {
                        for l in 0..m.layer.count {
                            let at =
                                m.disp.at(d) + m.head.at(h) + m.layer.at(l) + m.use_syncpt as usize;
                            if rd(params, at, 1)? != 0 {
                                return Err(refuse(format_args!(
                                    "SET_MODE disp {d} head {h} layer {l} asks for a Tegra syncpoint"
                                )));
                            }
                        }
                    }
                }
            }
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
                // A handle number the host reused starts with nothing.
                self.perms.remove(&(target, dev));
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

    fn revoke_all_through(&mut self, kms: u32) {
        if let Some(dpys) = self.drm_grants.remove(&kms) {
            for dpy in dpys {
                self.revoke_dpy(kms, dpy);
            }
        }
        self.grant_fds
            .retain(|_, s| !matches!(s, Source::Drm { kms: k, .. } if *k == kms));
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

    #[test]
    fn a_tegra_syncpoint_anywhere_in_a_flip_or_modeset_is_refused() {
        let v = v610();
        let p = policy(v);
        let f = lo(v).flip;
        let mut heads = vec![0u8; 2 * f.head_size as usize];
        let mut st = p.lock();
        let (l, _) = NvkmsPolicy::layout(&st).unwrap();
        let call = Call {
            target: M,
            fds: &[],
            kms_card: true,
        };
        assert_eq!(flip_heads_ok(l, &heads), Ok(()));
        heads[f.head_size as usize + f.layer.at(7) + f.use_syncpt as usize] = 1;
        assert_eq!(flip_heads_ok(l, &heads), Err(libc::EPERM));
        let m = l.set_mode;
        let mut sm = vec![0u8; size(v, "NVKMS_SET_MODE")];
        assert_eq!(st.check(l, "SET_MODE", &call, &mut sm), Ok(()));
        sm[m.disp.at(7) + m.head.at(3) + m.layer.at(7) + m.use_syncpt as usize] = 1;
        assert_eq!(st.check(l, "SET_MODE", &call, &mut sm), Err(libc::EPERM));
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
