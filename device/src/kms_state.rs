// SPDX-License-Identifier: Apache-2.0
//! What the backend keeps of the host KMS files a VM holds, for the IOCTL2
//! interpreter (`xfer`) to check a call against: which framebuffers and
//! property blobs a file of the VM made, which blobs it has been shown,
//! which framebuffers a call in flight still names, and when each connector
//! was last probed. Each record is made and taken back by the KMS stages of
//! `xfer`'s execution; the backend retires a file's records as it lets go
//! of the file's handle.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// What the backend knows about one host KMS file.
///
/// Framebuffer ids are device-global and not lease-filtered, and a lessee
/// counts as current master whenever its lessor is (drm_auth.c:64-70), so the
/// host's own GETFB gate (drm_framebuffer.c:557) would hand a guest lease GEM
/// handles for the host compositor's framebuffers. We answer GETFB/GETFB2 with
/// handles only for framebuffers this very file created (RV:getfb).
///
/// The same global lookup is what every scanout source goes through:
/// ATOMIC's FB_ID (drm_atomic_uapi.c:547), SETCRTC (drm_crtc.c:771),
/// SETPLANE and PAGE_FLIP (drm_plane.c:1159, 1482), and a lease covers only
/// CRTCs, connectors and planes, never framebuffers
/// (drm_mode_object.c:126-155). So a lessee that counted 1..N through FB_ID
/// on its own plane would show the host desktop or another VM's lease on its
/// connector, and read it back through its CRTC's checksum (S-6). A
/// framebuffer id a guest names as a source must be one some KMS file of the
/// same VM made ([`VmKms`]); within the VM it is the guest kernel's business
/// who uses whose, as it is on bare metal.
pub struct KmsFileState {
    /// Which file this is in `vm`: a number never reused, so a call still
    /// running on a closed file cannot credit what it makes to the next file
    /// the guest's handle number is given to.
    serial: u64,
    vm: Arc<VmKms>,
    inner: Mutex<KmsInner>,
}

#[derive(Default)]
pub(crate) struct KmsInner {
    /// Property id -> (name, flags), from GETPROPERTY.
    pub(crate) prop_names: HashMap<u32, ([u8; 32], u32)>,
}

/// DRM_MODE_PROP_BLOB (drm_mode.h): the property's value is a blob id.
pub(crate) const DRM_MODE_PROP_BLOB: u32 = 1 << 4;

/// What the KMS files of one VM share: every framebuffer they made and
/// have not removed, by id, with the file ([`KmsFileState::serial`]) that
/// made it; and when the VM last had the host probe each connector
/// ([`VmKms::may_probe`]).
///
/// A record goes before the host could hand its id to anyone else: RMFB and
/// CLOSEFB take it out before the call and put it back only if the host
/// refused, and a file's records all go when the backend lets go of its
/// handle ([`KmsFileState::retire`]), which is before the host file can
/// close. A file that is retired never records anything again, so an ADDFB
/// finishing on it after the close cannot leave an id behind that outlives
/// the host framebuffer.
///
/// And an id stays the VM's framebuffer for as long as a call that checked
/// it may still hand it to the host: between a scanout call's check and the
/// end of its ioctl the id is in use ([`FbUses`]), an RMFB or CLOSEFB of it
/// waits for that before it runs, and the host file that made it closes
/// only after ([`VmKms::close_after`]). Without that, an id freed on
/// another executor in between could be the next framebuffer anyone on the
/// host made -- the kernel hands out the lowest free id -- and the call
/// would show it.
#[derive(Default)]
pub struct VmKms {
    inner: Mutex<VmKmsInner>,
    /// Signalled when a framebuffer stops being in use.
    idle: std::sync::Condvar,
}

#[derive(Default)]
struct VmKmsInner {
    owner: HashMap<u32, u64>,
    retired: HashSet<u64>,
    /// (card, connector id) -> the last forced probe.
    probed: HashMap<(u32, u32), std::time::Instant>,
    /// Framebuffer id -> scanout calls between their check of it and the
    /// end of their ioctl.
    in_use: HashMap<u32, usize>,
    /// Property blobs a file of this VM made (CREATEPROPBLOB) and has not
    /// destroyed, with the file.
    blobs: HashMap<u32, u64>,
    /// The blob each blob property of an object a file of this VM can see
    /// held when the host last said (OBJ_GETPROPERTIES, GETCONNECTOR): (file,
    /// object, property) -> blob.
    seen_blobs: HashMap<(u64, u32, u32), u32>,
    /// Host files whose framebuffers a call in flight still names, with
    /// those ids: closed once none is in use.
    parked: Vec<(Vec<u32>, Box<dyn Send>)>,
}

/// How long an RMFB or CLOSEFB waits for calls in flight that named its
/// framebuffer. They looked it up in the kernel at their very start, and
/// from then on hold it by reference, so this is only ever the time between
/// the check and the ioctl; past it the removal goes ahead, and says so.
const FB_IN_USE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The framebuffer ids a call named as scanout sources and was allowed,
/// in use until it is dropped (after the call's ioctl).
#[derive(Default)]
pub struct FbUses {
    vm: Option<Arc<VmKms>>,
    ids: Vec<u32>,
}

impl FbUses {
    pub(crate) fn release(&mut self) {
        if let Some(vm) = self.vm.take() {
            vm.release(&std::mem::take(&mut self.ids));
        }
    }
}

impl Drop for FbUses {
    fn drop(&mut self) {
        self.release();
    }
}

/// How many connectors' last probe the VM keeps (see
/// [`VmKms::may_probe`]): far more than a host has.
pub const PROBES_KEPT: usize = 256;

/// How often the VM may have the host probe one connector (GETCONNECTOR
/// with count_modes 0); see `Prepared::limit_forced_probe`.
pub const CONNECTOR_PROBE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

impl VmKms {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, VmKmsInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether some file of this VM made framebuffer `id` and has not
    /// removed it.
    pub fn made_here(&self, id: u32) -> bool {
        self.lock().owner.contains_key(&id)
    }

    /// Forget every record: the session is gone, and with it every file.
    /// What is in use stays counted, and parked files parked, until the
    /// calls still running from before let go. So do the retired marks:
    /// a call made before the reset may still be running on a retired
    /// file, and must record nothing when it finishes; each mark goes with
    /// its file's last reference.
    pub fn clear(&self) {
        let mut v = self.lock();
        v.owner.clear();
        v.probed.clear();
        v.blobs.clear();
        v.seen_blobs.clear();
    }

    /// Whether GETPROPBLOB may read blob `id`: one a file of this VM made,
    /// or the value of a blob property of an object a file of this VM can
    /// see, as the host last reported it. Blob ids are the device's and a
    /// lease does not cover blobs (drm_mode_object_lease_required): read by
    /// number, any other VM's MODE_ID and damage clips, and the host
    /// desktop's, would be the guest's to read.
    pub fn blob_readable(&self, id: u32) -> bool {
        let v = self.lock();
        v.blobs.contains_key(&id) || v.seen_blobs.values().any(|&b| b == id)
    }

    /// `id`, named as a scanout source by a call about to run: whether some
    /// file of this VM made it (0, none, always passes); if so it is in use
    /// in `uses` from now until `uses` is dropped.
    fn claim(self: &Arc<Self>, id: u32, uses: &mut FbUses) -> bool {
        if id == 0 {
            return true;
        }
        let mut v = self.lock();
        if !v.owner.contains_key(&id) {
            return false;
        }
        *v.in_use.entry(id).or_insert(0) += 1;
        drop(v);
        uses.vm.get_or_insert_with(|| self.clone());
        uses.ids.push(id);
        true
    }

    fn release(&self, ids: &[u32]) {
        let ready = {
            let mut v = self.lock();
            for id in ids {
                if let Some(n) = v.in_use.get_mut(id) {
                    *n -= 1;
                    if *n == 0 {
                        v.in_use.remove(id);
                    }
                }
            }
            let parked = std::mem::take(&mut v.parked);
            let (ready, still): (Vec<_>, Vec<_>) = parked
                .into_iter()
                .partition(|(ids, _)| ids.iter().all(|i| !v.in_use.contains_key(i)));
            v.parked = still;
            ready
        };
        self.idle.notify_all();
        for (_, item) in ready {
            crate::closer::close(item);
        }
    }

    /// Close `file` -- a host KMS file of this VM, whose framebuffers `fbs`
    /// the backend has just stopped counting as the VM's -- on the closer
    /// thread, once no call in flight names any of them.
    pub fn close_after(&self, fbs: Vec<u32>, file: Box<dyn Send>) {
        let mut v = self.lock();
        let busy: Vec<u32> = fbs
            .into_iter()
            .filter(|id| v.in_use.contains_key(id))
            .collect();
        if busy.is_empty() {
            drop(v);
            crate::closer::close(file);
            return;
        }
        log::debug!("a KMS file closes once no call in flight names framebuffers {busy:?}");
        v.parked.push((busy, file));
    }

    /// Wait, at most [`FB_IN_USE_WAIT`], until no call in flight names
    /// `id`.
    fn wait_unused(&self, id: u32) {
        let v = self.lock();
        let (v, t) = self
            .idle
            .wait_timeout_while(v, FB_IN_USE_WAIT, |v| v.in_use.contains_key(&id))
            .unwrap_or_else(|e| e.into_inner());
        drop(v);
        if t.timed_out() {
            log::warn!(
                "framebuffer {id}: a call that named it is still running after {:?}; removed \
                 anyway",
                FB_IN_USE_WAIT
            );
        }
    }

    /// Whether connector `connector` of card `card` may be probed at `now`
    /// (and if so, that it was): once per [`CONNECTOR_PROBE_EVERY`] across
    /// every file of the VM, since connector ids are the device's.
    ///
    /// The id is the guest's: the host's refusal takes the record back
    /// ([`VmKms::probe_refused`]), so only connectors the host serves stay
    /// recorded, and past [`PROBES_KEPT`] records the ones older than the
    /// window, which say nothing, go first.
    pub fn may_probe(&self, card: u32, connector: u32, now: std::time::Instant) -> bool {
        let mut v = self.lock();
        if v.probed.len() >= PROBES_KEPT && !v.probed.contains_key(&(card, connector)) {
            v.probed
                .retain(|_, &mut t| now.saturating_duration_since(t) < CONNECTOR_PROBE_EVERY);
            if v.probed.len() >= PROBES_KEPT {
                // As many connectors probed this very window as a host has
                // no need of: this one is reported, not probed.
                return false;
            }
        }
        let last = v.probed.entry((card, connector)).or_insert(now);
        if *last == now || now.duration_since(*last) >= CONNECTOR_PROBE_EVERY {
            *last = now;
            return true;
        }
        false
    }

    /// Connectors whose last probe is recorded.
    #[cfg(test)]
    pub(crate) fn probes_recorded(&self) -> usize {
        self.lock().probed.len()
    }

    /// The host refused the probe `may_probe` let through at `at`: no such
    /// connector, or not one this file may see. Its record goes, unless a
    /// later probe has made it since.
    pub fn probe_refused(&self, card: u32, connector: u32, at: std::time::Instant) {
        let mut v = self.lock();
        if v.probed.get(&(card, connector)) == Some(&at) {
            v.probed.remove(&(card, connector));
        }
    }
}

impl Default for KmsFileState {
    fn default() -> Self {
        Self::new()
    }
}

impl KmsFileState {
    /// A file with a VM of its own (tests, and a backend that has no other).
    pub fn new() -> Self {
        Self::in_vm(Arc::default())
    }

    /// A file of the VM whose framebuffers `vm` records.
    pub fn in_vm(vm: Arc<VmKms>) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self {
            serial: NEXT.fetch_add(1, Ordering::Relaxed),
            vm,
            inner: Mutex::default(),
        }
    }

    /// What the KMS files of this file's VM share.
    pub(crate) fn vm(&self) -> &VmKms {
        &self.vm
    }

    /// Whether this file created framebuffer `id` and has not removed it.
    pub fn owns_fb(&self, id: u32) -> bool {
        self.vm.lock().owner.get(&id) == Some(&self.serial)
    }

    /// Whether `id` may be named as a scanout source through this file: 0
    /// (none), or a framebuffer some file of this VM made.
    pub fn may_scan_out(&self, id: u32) -> bool {
        id == 0 || self.vm.made_here(id)
    }

    /// A CREATEPROPBLOB through this file made blob `id`.
    pub(crate) fn add_blob(&self, id: u32) {
        let mut v = self.vm.lock();
        if !v.retired.contains(&self.serial) {
            v.blobs.insert(id, self.serial);
        }
    }

    /// DESTROYPROPBLOB of `id`, about to run: its records go first, as
    /// for RMFB (only the file that made a blob may destroy it,
    /// drm_property.c:880). Whether it was this file's.
    pub(crate) fn take_blob(&self, id: u32) -> bool {
        let mut v = self.vm.lock();
        if v.blobs.get(&id) != Some(&self.serial) {
            return false;
        }
        v.blobs.remove(&id);
        v.seen_blobs.retain(|_, b| *b != id);
        true
    }

    /// What the host reported of object `obj`'s blob properties through
    /// this file: `(property, blob)`, 0 for none.
    pub(crate) fn saw_blobs(&self, obj: u32, props: &[(u32, u32)]) {
        let mut v = self.vm.lock();
        if v.retired.contains(&self.serial) {
            return;
        }
        for &(prop, blob) in props {
            if blob == 0 {
                v.seen_blobs.remove(&(self.serial, obj, prop));
            } else {
                v.seen_blobs.insert((self.serial, obj, prop), blob);
            }
        }
    }

    /// `may_scan_out`, for a call about to run: the id is then in use in
    /// `uses` (see [`VmKms`]).
    pub fn claim_scan_out(&self, id: u32, uses: &mut FbUses) -> bool {
        self.vm.claim(id, uses)
    }

    /// Record framebuffer `id` as this file's, unless the file is retired.
    pub fn add_fb(&self, id: u32) {
        let mut v = self.vm.lock();
        if !v.retired.contains(&self.serial) {
            v.owner.insert(id, self.serial);
        }
    }

    /// Take `id` out if it is this file's, and wait until no call in
    /// flight names it; whether it was.
    pub(crate) fn take_fb(&self, id: u32) -> bool {
        let mut v = self.vm.lock();
        if v.owner.get(&id) == Some(&self.serial) {
            v.owner.remove(&id);
            drop(v);
            self.vm.wait_unused(id);
            true
        } else {
            false
        }
    }

    /// The backend has let go of this file's handle: none of its
    /// framebuffers is the VM's any more, and nothing it makes from now on
    /// will be. Idempotent. Returns the ids it had, for the file's close to
    /// wait on ([`VmKms::close_after`]).
    pub fn retire(&self) -> Vec<u32> {
        let mut v = self.vm.lock();
        let me = self.serial;
        let mine: Vec<u32> = v
            .owner
            .iter()
            .filter(|&(_, s)| *s == me)
            .map(|(&id, _)| id)
            .collect();
        v.owner.retain(|_, s| *s != me);
        // The host destroys a file's blobs as it closes
        // (drm_property_destroy_user_blobs).
        v.blobs.retain(|_, s| *s != me);
        v.seen_blobs.retain(|&(s, _, _), _| s != me);
        v.retired.insert(me);
        mine
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, KmsInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Drop for KmsFileState {
    /// The last call on a retired file is done: its serial can never come
    /// back, so its "retired" mark has nothing left to stop.
    fn drop(&mut self) {
        let mut v = self.vm.lock();
        let me = self.serial;
        v.owner.retain(|_, s| *s != me);
        v.blobs.retain(|_, s| *s != me);
        v.seen_blobs.retain(|&(s, _, _), _| s != me);
        v.retired.remove(&me);
    }
}
