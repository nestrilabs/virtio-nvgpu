// SPDX-License-Identifier: GPL-2.0-only
//! The IOCTL2 hooks, written once and run by both implementations: what
//! nvgpu_kms.c, nvgpu_fence.c and nvgpu_nvkms.c do, reduced to answers that
//! are a function of their arguments and a seed, including the ones that
//! reach into the kernel copies (the ATOMIC special, the phase hooks).

use crate::world::{Ev, Hook, Rng, World};

pub const EINVAL: i32 = 22;
pub const I2_FD_CONSUME: u32 = 1;

/// The kernel copies and records of a call, as a hook reaches them: through
/// `nvgpu_i2_buf()` and friends for the C, the `State` for the Rust.
pub trait CallBufs {
    fn buf(&mut self, b: u32) -> Option<&mut [u8]>;
    fn add_dyn(&mut self, kind: u32, buf: u32, off: u32, len: u32) -> i32;
    fn add_fd(&mut self, buf: u32, off: u32, handle: u32, flags: u32) -> i32;
    fn ret(&self) -> i32;
    fn set_ret(&mut self, r: i32);
    /// `nvgpu_atomic_parse()` of the implementation under test, its hooks
    /// the ones below: (result, commit, values_buf).
    fn atomic(&mut self, w: &mut World, fences: bool) -> (i32, bool, u32);
}

fn rng(w: &World, salt: &[u64]) -> Rng {
    let mut h = w.hooks.seed;
    for &s in salt {
        h = h.rotate_left(17) ^ s.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
    Rng::new(h)
}

pub fn has(w: &World, bit: u32) -> bool {
    w.hooks.mask & (1 << bit) != 0
}

pub fn fd_in(w: &mut World, buf: u32, off: u32, v: i64, kinds: u32) -> (i32, u32, u32) {
    w.events.push(Ev::Hook(Hook::FdIn { buf, off, v, kinds }));
    let mut r = rng(w, &[1, v as u64, u64::from(kinds)]);
    if v == 1000 {
        return (1, 0, 0);
    }
    match i32::try_from(v).ok().and_then(|fd| w.fds.get(&fd).copied()) {
        Some(h) if !r.chance(1, 16) => (0, h, if r.chance(1, 3) { I2_FD_CONSUME } else { 0 }),
        Some(_) => (-12, 0, 0),
        None => (-9, 0, 0),
    }
}

pub fn gem_in(w: &mut World, buf: u32, off: u32, guest: u32) -> (i32, u32, u32) {
    w.events.push(Ev::Hook(Hook::GemIn { buf, off, guest }));
    let mut r = rng(w, &[2, u64::from(guest)]);
    if r.chance(1, 10) {
        return (-2, 0, 0);
    }
    if r.chance(1, 12) {
        return (0, 7, 0);
    }
    (0, 5000 + guest % 3, guest.wrapping_mul(2).wrapping_add(1))
}

pub fn fd_out(w: &mut World, buf: u32, off: u32, handle: u32, kind: u32) -> (i32, i64) {
    w.events.push(Ev::Hook(Hook::FdOut { buf, off, handle, kind }));
    let mut r = rng(w, &[3, u64::from(handle), u64::from(kind)]);
    if r.chance(1, 10) {
        return (-24, -1);
    }
    (0, 300 + i64::from(handle))
}

pub fn gem_out(w: &mut World, buf: u32, off: u32, gem: u32, size: u64) -> (i32, u32) {
    w.events.push(Ev::Hook(Hook::GemOut { buf, off, gem, size }));
    let mut r = rng(w, &[4, u64::from(gem)]);
    if w.hooks.fail_gem == Some(gem) {
        return (-12, 0);
    }
    if w.hooks.fail_gem.is_none() && r.chance(1, 10) {
        return (-12, 0);
    }
    (0, 7000 + gem)
}

/// The ATOMIC special: in phase 0, a dyn record and an fd record, as
/// nvgpu_kms.c adds for OUT_FENCE_PTR and IN_FENCE_FD; in phase 1, nothing
/// to add but its say on the result.
pub fn special(w: &mut World, c: &mut dyn CallBufs, id: u32, phase: i32) -> i32 {
    w.events.push(Ev::Hook(Hook::Special { id, phase }));
    if let (Some(fences), 1, 0) = (w.atomic, id, phase) {
        let (r, commit, values_buf) = c.atomic(w, fences);
        w.events.push(Ev::Hook(Hook::AtomicOut { commit, values_buf }));
        return r;
    }
    let mut r = rng(w, &[5, u64::from(id), phase as u64]);
    if phase == 0 {
        let len = c.buf(0).map_or(0, |b| b.len() as u32);
        if r.chance(1, 2) {
            let off = (r.below(u64::from(len) + 8) as u32) & !3;
            let x = c.add_dyn(1, 0, off, 4);
            if x != 0 && r.chance(1, 2) {
                return x;
            }
        }
        if r.chance(1, 2) {
            let buf = r.below(3) as u32;
            let off = (r.below(u64::from(len) + 4) as u32) & !3;
            let x = c.add_fd(buf, off, 4242, if r.chance(1, 2) { I2_FD_CONSUME } else { 0 });
            if x != 0 && r.chance(1, 2) {
                return x;
            }
        }
    }
    if r.chance(1, 20) {
        return -EINVAL;
    }
    0
}

/// The phase hooks: a rewrite of what will be sent, a rewrite of what the
/// caller gets back, or a changed result.
pub fn phase(w: &mut World, c: &mut dyn CallBufs, phase: i32) -> i32 {
    w.events.push(Ev::Hook(Hook::Phase { phase }));
    let mut r = rng(w, &[6, phase as u64]);
    let b = r.below(2) as u32;
    if r.chance(1, 3) {
        if let Some(k) = c.buf(b) {
            if !k.is_empty() {
                let i = r.below(k.len() as u64) as usize;
                k[i] ^= 0x5a;
            }
        }
    }
    if phase == 1 && r.chance(1, 4) {
        let old = c.ret();
        c.set_ret(if old == 0 { -11 } else { 0 });
    }
    if r.chance(1, 25) {
        return -5;
    }
    0
}

// ── what nvgpu_kms.c answers the atomic parse, reduced ──

/// NVGPU_KOBJ_*: every third id a CRTC, 0xdead unknown to the host.
pub fn a_obj(w: &mut World, obj: u32) -> (i32, u32) {
    w.events.push(Ev::Hook(Hook::AObj { obj }));
    if obj == 0xdead {
        return (-5, 0);
    }
    if obj.is_multiple_of(3) {
        (1, 0)
    } else {
        (2, obj % 5)
    }
}

/// NVGPU_KPROP_* by id: plain, CRTC_ID, IN_FENCE_FD, OUT_FENCE_PTR; ids
/// that end in 10 (mod 11) are unknown to the host.
pub fn a_prop(w: &mut World, id: u32) -> i32 {
    w.events.push(Ev::Hook(Hook::AProp { id }));
    if id % 11 == 10 {
        return -22;
    }
    1 + (id % 4) as i32
}

/// nvgpu_kms_in_fence(): -1 in the copy for a fence that has signalled (4),
/// a record for one of ours, -EBADF otherwise. `commit` is what the parse
/// had said about the commit when the hook ran (nvgpu_atomic_parse()'s
/// `out->commit`, `atomic::Env::begin`), which the kernel's hook acts on.
pub fn a_in_fence(w: &mut World, c: &mut dyn CallBufs, buf: u32, off: u32, fd: i64, commit: bool) -> i32 {
    w.events.push(Ev::Hook(Hook::AInFence { buf, off, fd, commit }));
    if fd < 0 || fd > i64::from(i32::MAX) {
        return -EINVAL;
    }
    if fd == 4 {
        let Some(vals) = c.buf(buf) else { return -EINVAL };
        let off = off as usize;
        if off > vals.len() || vals.len() - off < 8 {
            return -EINVAL;
        }
        vals[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        return 0;
    }
    match w.fds.get(&(fd as i32)).copied() {
        Some(h) => c.add_fd(buf, off, h, if fd == 3 { I2_FD_CONSUME } else { 0 }),
        None => -9,
    }
}

/// nvgpu_kms_out_fence(): at most 32, -1 written through the pointer, a dyn
/// record.
pub fn a_out_fence(w: &mut World, c: &mut dyn CallBufs, buf: u32, off: u32, uptr: u64) -> i32 {
    w.events.push(Ev::Hook(Hook::AOutFence { buf, off, uptr }));
    if w.nfence >= 32 {
        return -7;
    }
    if !w.copy_to_user(uptr, &(-1i32).to_le_bytes()) {
        return -14;
    }
    let r = c.add_dyn(1, buf, off, 4);
    if r == 0 {
        w.nfence += 1;
    }
    r
}

pub fn a_learn(w: &mut World, obj: u32, crtc: u32) {
    w.events.push(Ev::Hook(Hook::ALearn { obj, crtc }));
}

/// nvgpu_kms_reserve(): CRTC 13 has no memory left.
pub fn a_reserve(w: &mut World, crtc: u32, user_data: u64) -> i32 {
    w.events.push(Ev::Hook(Hook::AReserve { crtc, user_data }));
    if crtc == 13 {
        -12
    } else {
        0
    }
}
