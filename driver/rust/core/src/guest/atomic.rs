// SPDX-License-Identifier: GPL-2.0-only
//! An atomic commit's arrays, as the caller laid them out: which objects it
//! sets which properties on, and so which CRTCs the commit carries (for
//! their flip events) and which of its values are fences. A port of
//! `nvgpu_atomic_parse()` (`driver/nvgpu_atomic.c`); `nvgpu_kms.c` keeps
//! what the parse asks of it ([`Env`]).
//!
//! The arrays are the IOCTL2 call's kernel copies, read in the [`State`]
//! each time they are needed, never across a hook: a fence hook rewrites the
//! values it takes, and adds records to the state.

use super::i2::{State, Store};
use super::wire::{le32, le64};

/// `sizeof(struct drm_mode_atomic)`.
pub const SIZE: usize = 56;
const FLAGS: usize = 0;
const COUNT_OBJS: usize = 4;
const OBJS_PTR: usize = 8;
const COUNT_PROPS_PTR: usize = 16;
const PROPS_PTR: usize = 24;
const VALUES_PTR: usize = 32;
const USER_DATA: usize = 48;
/// `DRM_MODE_PAGE_FLIP_EVENT`.
pub const FLIP_EVENT: u32 = 0x01;
/// `DRM_MODE_ATOMIC_TEST_ONLY`.
pub const TEST_ONLY: u32 = 0x100;
/// Events reserved per commit (`NVGPU_ATOMIC_MAX_EVENTS`).
pub const MAX_EVENTS: usize = 32;
/// CRTC_ID assignments one commit may teach (`NVGPU_ATOMIC_MAX_LEARN`).
pub const MAX_LEARN: u32 = 64;

/// `NVGPU_KPROP_CRTC_ID`.
pub const KPROP_CRTC_ID: i32 = 2;
/// `NVGPU_KPROP_IN_FENCE`.
pub const KPROP_IN_FENCE: i32 = 3;
/// `NVGPU_KPROP_OUT_PTR`.
pub const KPROP_OUT_PTR: i32 = 4;
/// `NVGPU_KOBJ_CRTC`.
pub const KOBJ_CRTC: i32 = 1;

/// What the parse asks of the module (`struct nvgpu_atomic_ops`).
pub trait Env<S: Store> {
    /// Whether the commit is a real one (not TEST_ONLY), as soon as that is
    /// known and before any other hook runs: the fence hooks act on it (a
    /// TEST_ONLY commit's in-fences are only checked). The C parse writes
    /// `out->commit` there; here it is said.
    fn begin(&mut self, commit: bool);
    /// `NVGPU_KOBJ_*` and the CRTC a non-CRTC is on (0: not known), or a
    /// negative errno.
    fn obj_class(&mut self, obj: u32) -> (i32, u32);
    /// `NVGPU_KPROP_*`, or a negative error.
    fn prop_class(&mut self, id: u32) -> i32;
    /// IN_FENCE_FD with a value other than -1, at `off` of buffer `buf`.
    fn in_fence(&mut self, st: &mut State<S>, buf: u32, off: u32, fd: i64) -> i32;
    /// A `*_PTR` property with a nonzero value.
    fn out_fence(&mut self, st: &mut State<S>, buf: u32, off: u32, uptr: u64) -> i32;
    /// Object `obj` is put on CRTC `crtc`, if the commit goes.
    fn learn(&mut self, obj: u32, crtc: u32);
    /// A flip event for `crtc`.
    fn reserve(&mut self, crtc: u32, user_data: u64) -> i32;
}

/// `struct nvgpu_atomic_out`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Out {
    /// Not TEST_ONLY.
    pub commit: bool,
    /// prop_values' buffer, 0 for none.
    pub values_buf: u32,
}

/// Buffer `b` of the call, `None` for none or an empty one (as
/// `nvgpu_i2_buf()` answers NULL).
fn buf<S: Store>(st: &State<S>, b: u32) -> Option<&[u8]> {
    st.buf(b).filter(|k| !k.is_empty())
}

fn w32<S: Store>(st: &State<S>, b: u32, off: u64) -> u32 {
    let off = usize::try_from(off).unwrap_or(usize::MAX);
    buf(st, b).and_then(|k| le32(k, off)).unwrap_or(0)
}

fn w64<S: Store>(st: &State<S>, b: u32, off: u64) -> u64 {
    let off = usize::try_from(off).unwrap_or(usize::MAX);
    buf(st, b).and_then(|k| le64(k, off)).unwrap_or(0)
}

fn blen<S: Store>(st: &State<S>, b: u32) -> Option<u64> {
    buf(st, b).map(|k| k.len() as u64)
}

/// `nvgpu_atomic_add_crtc()`: past `MAX_EVENTS` a CRTC's event is reserved
/// when it arrives.
fn add_crtc(crtcs: &mut [u32; MAX_EVENTS], n: &mut usize, crtc: u32) {
    for i in 0..*n {
        if crtcs.get(i) == Some(&crtc) {
            return;
        }
    }
    if let Some(slot) = crtcs.get_mut(*n) {
        *slot = crtc;
        *n = n.saturating_add(1);
    }
}

/// `nvgpu_atomic_parse()`: walk the commit's arrays in the call's kernel
/// copies, asking `env` what its objects and properties are, reserving the
/// flip events and bridging the fences. 0 or a negative errno.
pub fn parse<S: Store, E: Env<S>>(st: &mut State<S>, env: &mut E, fences: bool, out: &mut Out) -> i32 {
    let Some(len0) = blen(st, 0) else { return 0 };
    if len0 < SIZE as u64 {
        return 0;
    }
    let flags = w32(st, 0, FLAGS as u64);
    let count = w32(st, 0, COUNT_OBJS as u64);
    out.commit = flags & TEST_ONLY == 0;
    env.begin(out.commit);
    let events = flags & FLIP_EVENT != 0 && out.commit;
    // Without the fence bridge a fence property is the backend's to refuse,
    // from its own copy; there is nothing to look up for it here.
    if count == 0 || (!events && !fences) {
        return 0;
    }

    let mut idx = 1u32;
    let mut next = || {
        let i = idx;
        idx = idx.saturating_add(1);
        i
    };
    let bobjs = if w64(st, 0, OBJS_PTR as u64) != 0 { next() } else { 0 };
    let bcp = if w64(st, 0, COUNT_PROPS_PTR as u64) != 0 { next() } else { 0 };
    if bobjs == 0 || bcp == 0 {
        return 0; // the host faults on it; nothing will be made
    }
    let want = u64::from(count).saturating_mul(4);
    if blen(st, bobjs) != Some(want) || blen(st, bcp) != Some(want) {
        return 0;
    }
    let mut sum: u64 = 0;
    for o in 0..u64::from(count) {
        sum = sum.saturating_add(u64::from(w32(st, bcp, o.saturating_mul(4))));
    }
    let bprops = if sum != 0 && w64(st, 0, PROPS_PTR as u64) != 0 { next() } else { 0 };
    let bvals = if sum != 0 && w64(st, 0, VALUES_PTR as u64) != 0 { next() } else { 0 };
    if sum != 0 {
        let lp = if bprops != 0 { blen(st, bprops) } else { None };
        let lv = if bvals != 0 { blen(st, bvals) } else { None };
        if lp != Some(sum.saturating_mul(4)) || lv != Some(sum.saturating_mul(8)) {
            return 0;
        }
    }
    out.values_buf = bvals;

    let mut crtcs = [0u32; MAX_EVENTS];
    let mut ncrtc = 0usize;
    let mut nlearn = 0u32;
    let mut k: u64 = 0;
    for o in 0..u64::from(count) {
        let obj = w32(st, bobjs, o.saturating_mul(4));
        let n = w32(st, bcp, o.saturating_mul(4));
        if n != 0 && events {
            let (ret, on) = env.obj_class(obj);
            if ret < 0 {
                return ret;
            }
            if ret == KOBJ_CRTC {
                add_crtc(&mut crtcs, &mut ncrtc, obj);
            } else if on != 0 {
                add_crtc(&mut crtcs, &mut ncrtc, on);
            }
        }
        for _ in 0..n {
            let id = w32(st, bprops, k.saturating_mul(4));
            // Read here each time: a fence hook rewrites the values it takes.
            let v = w64(st, bvals, k.saturating_mul(8));
            let off = k.saturating_mul(8) as u32;
            k = k.saturating_add(1);
            let ret = env.prop_class(id);
            if ret < 0 {
                return ret;
            }
            match ret {
                KPROP_CRTC_ID => {
                    if events && v != 0 {
                        add_crtc(&mut crtcs, &mut ncrtc, v as u32);
                    }
                    if nlearn < MAX_LEARN {
                        env.learn(obj, v as u32);
                        nlearn = nlearn.saturating_add(1);
                    }
                }
                KPROP_IN_FENCE if fences && v as i64 != -1 => {
                    let r = env.in_fence(st, bvals, off, v as i64);
                    if r != 0 {
                        return r;
                    }
                }
                KPROP_OUT_PTR if fences && v != 0 => {
                    let r = env.out_fence(st, bvals, off, v);
                    if r != 0 {
                        return r;
                    }
                }
                _ => {}
            }
        }
    }

    if events {
        for i in 0..ncrtc {
            let Some(&crtc) = crtcs.get(i) else { break };
            let r = env.reserve(crtc, w64(st, 0, USER_DATA as u64));
            if r != 0 {
                return r;
            }
        }
    }
    0
}
