// SPDX-License-Identifier: GPL-2.0-only
//! The schema-driven IOCTL2 interpreter: gathers a caller's buffers per the
//! schema, builds the request, and reads the reply back into the caller's
//! memory by the kernel's own copy-back rules. A line-for-line port of
//! `driver/nvgpu_i2.c`, which stays the reference (and is what the
//! differential test runs this against).
//!
//! What differs is where the caller's bytes live and who may touch them. The
//! argument and everything its pointers reach are copied in once each, by
//! [`Store::fetch`], into buffers this code owns; every length, pointer,
//! count and descriptor position is read from those copies and never again
//! from the caller. The caller's memory is written only by
//! [`Store::copy_out`], with the range [`copy_extent`] computes. The module's
//! hooks (descriptor and GEM translation, the ATOMIC special, the phase
//! hooks) and the transport are an [`Env`]; the hooks may read and rewrite
//! the kernel copies through the [`State`] they are handed, exactly as the C
//! hooks do through `nvgpu_i2_buf()`.

use super::schema::{
    SField, SIoctl, SchemaSet, Table, Which, MAX_DEPTH, MAX_LIST, NVKMS_IOCTL_IOWR,
    SCB_ALL_OR_NOTHING, SCB_EXACT, SCB_FULL, SCB_PARTIAL, SCB_RANGE, SCB_WRITTEN, SCLASS_KMS,
    SCLASS_MODESET, SCLASS_RENDER, SDIR_IN, SDIR_INOUT, SDIR_OUT, SFF_COND, SFF_COND_NE, SF_ARRAY,
    SF_FD_IN, SF_FD_OUT, SF_GEM_IN, SF_GEM_OUT, SF_PTR, SIO_EXECUTOR, SLEN_CONST, SLEN_COUNT,
    SLEN_NVKMS_PARAMS, SLEN_PLANES, SLEN_SUM, SSPECIAL_ATOMIC,
};
use super::wire::{
    align8, copy, le32, le64, le_n, put_le, Errno, E2BIG, EFAULT, EINTR, EINVAL, EMFILE, ENOMEM,
    ENOTTY, EPROTO, ETIMEDOUT, HDR_LEN, I2_FD_CONSUME, I2_GEM_OUT_LEN, I2_MAX_BUFS, I2_MAX_RECS,
    I2_REC_LEN, I2_REQ_LEN, I2_RESP_LEN, MAX_ERRNO, MSG_IOCTL2,
};

/// Schema positions one call may reach (`NVGPU_I2_MAX_SLOTS`): far above any
/// real call, so a count the caller made up sizes nothing but a refusal.
pub const MAX_SLOTS: usize = 512;

/// `NVGPU_XF_EXECUTOR`: the call runs on a host executor.
pub const XF_EXECUTOR: u32 = 1;

/// The buffers of one call: the kernel's copies of the caller's memory.
///
/// Buffer `i` exists once [`Store::alloc`] made it; every accessor answers
/// an empty slice for one that does not.
pub trait Store {
    /// Make buffer `i` `len` zeroed bytes; `-ENOMEM` if it cannot.
    fn alloc(&mut self, i: usize, len: usize) -> Result<(), Errno>;
    /// Fill buffer `i`, whole, from the caller's memory at `uptr`: the one
    /// read of those bytes. `-EFAULT` if the caller's memory is not there.
    fn fetch(&mut self, i: usize, uptr: u64) -> Result<(), Errno>;
    /// Buffer `i`.
    fn buf(&self, i: usize) -> &[u8];
    /// Buffer `i`, to rewrite.
    fn buf_mut(&mut self, i: usize) -> &mut [u8];
    /// Copy bytes `[start, end)` of buffer `i` to the caller's `uptr + start`.
    fn copy_out(&mut self, i: usize, uptr: u64, start: usize, end: usize) -> Result<(), Errno>;
}

/// How the transport ended a call (`nvgpu_xfer()`).
pub enum Xfer<T> {
    /// Answered: `used` bytes of `resp` are the device's.
    Done {
        /// The request, the caller's again.
        req: T,
        /// The reply.
        resp: T,
        /// Bytes the device wrote.
        used: u32,
    },
    /// Failed before an answer, buffers the caller's (-ENODEV, ...).
    Failed {
        /// The request.
        req: T,
        /// The reply buffer.
        resp: T,
        /// Why.
        err: Errno,
    },
    /// Abandoned (-ETIMEDOUT, -EINTR): both buffers are the transport's now,
    /// and so is closing whatever the late reply creates and every handle
    /// the request was to consume.
    Abandoned {
        /// Why.
        err: Errno,
    },
}

/// What went wrong that is worth a (rate-limited) line in the log.
#[derive(Clone, Copy, Debug)]
pub enum Warn<'t> {
    /// A known ioctl with another size or direction than the schema's.
    CmdSize {
        /// The schema's entry.
        entry: &'t SIoctl,
        /// The caller's number.
        cmd: u32,
        /// Its size.
        size: u32,
    },
    /// A reply whose counts or lengths are not what was sent.
    Malformed {
        /// The entry.
        entry: &'t SIoctl,
        /// Bytes the device wrote.
        used: u32,
        /// Descriptors it names.
        nfd: u32,
        /// GEM handles it names.
        ngem: u32,
    },
    /// A reply that names a descriptor or GEM handle where the schema has
    /// none.
    Unnamed {
        /// The entry.
        entry: &'t SIoctl,
    },
}

/// The module around the interpreter: its hooks (`struct nvgpu_i2_ops`),
/// the transport, and the log. Every hook gets the [`State`], which is how
/// it reaches the kernel copies; each returns the C hook's `int`.
pub trait Env<S: Store> {
    /// A transport buffer (`struct nvgpu_tbuf`); dropping one frees it.
    type TBuf;

    /// `ops->fd_in`: `(ret, handle, flags)`. -EINVAL without the hook.
    fn fd_in(
        &mut self,
        st: &mut State<S>,
        buf: u32,
        off: u32,
        value: i64,
        kinds: u32,
    ) -> (i32, u32, u32);
    /// `ops->gem_in`: `(ret, owner, gem)`. -EINVAL without the hook.
    fn gem_in(&mut self, st: &mut State<S>, buf: u32, off: u32, guest: u32) -> (i32, u32, u32);
    /// `ops->fd_out`: `(ret, the caller's new descriptor)`. -EINVAL without
    /// the hook.
    fn fd_out(
        &mut self,
        st: &mut State<S>,
        buf: u32,
        off: u32,
        handle: u32,
        kind: u32,
    ) -> (i32, i64);
    /// `ops->gem_out`: `(ret, the caller's new GEM handle)`. -EINVAL without
    /// the hook.
    fn gem_out(&mut self, st: &mut State<S>, buf: u32, off: u32, gem: u32, size: u64)
        -> (i32, u32);
    /// `ops->special`; 0 without the hook.
    fn special(&mut self, st: &mut State<S>, id: u32, phase: i32) -> i32;
    /// `ops->phase`; 0 without the hook.
    fn phase(&mut self, st: &mut State<S>, phase: i32) -> i32;
    /// `nvgpu_close_handle_async()`.
    fn close_handle(&mut self, handle: u32);
    /// `nvgpu_gem_close_async()` on the call's render handle.
    fn gem_close(&mut self, gem: u32);
    /// `nvgpu_tbuf_alloc(len, GFP_KERNEL)`.
    fn tbuf_alloc(&mut self, len: usize) -> Option<Self::TBuf>;
    /// `nvgpu_tbuf_write()`.
    fn tbuf_write(&mut self, tb: &mut Self::TBuf, off: usize, src: &[u8]) -> Result<(), Errno>;
    /// `nvgpu_tbuf_read()`.
    fn tbuf_read(&mut self, tb: &Self::TBuf, off: usize, dst: &mut [u8]) -> Result<(), Errno>;
    /// Whatever the hooks asked to be held (`nvgpu_i2_hold()`) is released
    /// with `req` from now on (`nvgpu_tbuf_on_free()`), not with the state.
    fn hand_over_held(&mut self, st: &mut State<S>, req: &mut Self::TBuf);
    /// `nvgpu_xfer()`.
    fn xfer(&mut self, req: Self::TBuf, resp: Self::TBuf, flags: u32) -> Xfer<Self::TBuf>;
    /// `dev_warn_ratelimited()`.
    fn warn(&mut self, w: Warn<'_>);
}

/// One buffer of the request.
#[derive(Clone, Copy, Debug, Default)]
struct KBuf {
    len: u32,
    dir: u8,
    /// Reached through a PTR field (`f`), not the argument.
    has_f: bool,
    f: u32,
    /// The buffer that pointer sits in, and where its struct starts there:
    /// the copy-back count is that struct's, read once as sent and once as
    /// the host left it.
    parent: u32,
    pbase: u32,
    sent: u64,
    uptr: u64,
}

/// A schema position the walk reached, with the caller's value there.
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    f: u32,
    kind: u8,
    width: u8,
    buf: u32,
    off: u32,
    orig: u64,
}

/// One 16-byte request record (`nvgpu_i2_fd_in`, `_gem_in`, `_dyn`), as its
/// four little-endian words.
#[derive(Clone, Copy, Debug, Default)]
struct Rec([u32; 4]);

/// A descriptor or GEM handle the reply carries.
#[derive(Clone, Copy, Debug, Default)]
struct Out {
    buf: u32,
    off: u32,
    /// Backend handle (descriptor) or host GEM handle in the render file.
    handle: u32,
    /// Descriptor: `NVGPU_HK_*`; GEM: the guest handle, once made.
    kind: u32,
    size: u64,
}

/// What one call is: the interpreter's state (`struct nvgpu_i2_state`).
///
/// All-zero bytes are a valid empty state for a `Store` whose all-zero
/// bytes are valid: every field is an integer or a `bool`, and [`Which`]'s
/// zero is `Drm`. The kernel allocates it that way (it is some 70 KiB, too
/// large for a kernel stack) and [`run`] resets it before use.
pub struct State<S> {
    /// The buffers.
    pub store: S,
    which: Which,
    entry: usize,
    max_req: u64,
    max_resp: u64,
    nbuf: u32,
    nslot: u32,
    nfd: u32,
    ngem: u32,
    ndyn: u32,
    nfdo: u32,
    ngemo: u32,
    in_bytes: u64,
    out_bytes: u64,
    /// The host's ioctl result once a reply is parsed; the phase hook may
    /// change it, and it is what the caller gets back (`call->ret`).
    pub ret: i32,
    buf: [KBuf; I2_MAX_BUFS],
    slot: [Slot; MAX_SLOTS],
    fd: [Rec; I2_MAX_RECS],
    gem: [Rec; I2_MAX_RECS],
    dyns: [Rec; I2_MAX_RECS],
    fdo: [Out; I2_MAX_RECS],
    gemo: [Out; I2_MAX_RECS],
}

/// What the call is, from `struct nvgpu_i2_call` and the device.
#[derive(Clone, Copy, Debug, Default)]
pub struct Args {
    /// `NVGPU_SCLASS_*`.
    pub sclass: u32,
    /// The caller's ioctl number.
    pub cmd: u32,
    /// The caller's argument.
    pub uarg: u64,
    /// Target backend handle.
    pub handle: u32,
    /// Render handle of the calling guest file.
    pub render: u32,
    /// `NVGPU_XF_*` from the caller.
    pub xflags: u32,
    /// `in_compat_syscall()`.
    pub compat: bool,
    /// The largest request the backend takes (`dev->max_req`).
    pub max_req: u64,
    /// The largest reply it builds (`dev->max_resp`).
    pub max_resp: u64,
}

fn ioc_size(cmd: u32) -> u32 {
    (cmd >> 16) & 0x3fff
}

fn ioc_dir(cmd: u32) -> u32 {
    cmd >> 30
}

fn ioc_nr(cmd: u32) -> u32 {
    cmd & 0xff
}

/// `nvgpu_i2_sext()`: a 4-byte descriptor field's value, sign-extended.
fn sext(v: u64, width: u8) -> i64 {
    if width == 4 {
        i64::from(v as u32 as i32)
    } else {
        v as i64
    }
}

fn idx(v: u32) -> usize {
    v as usize
}

impl<S: Store> State<S> {
    /// An empty state around `store` (the host's way; the kernel's is a
    /// zeroed allocation).
    pub fn new(store: S) -> State<S> {
        State {
            store,
            which: Which::Drm,
            entry: 0,
            max_req: 0,
            max_resp: 0,
            nbuf: 0,
            nslot: 0,
            nfd: 0,
            ngem: 0,
            ndyn: 0,
            nfdo: 0,
            ngemo: 0,
            in_bytes: 0,
            out_bytes: 0,
            ret: 0,
            buf: [KBuf::default(); I2_MAX_BUFS],
            slot: [Slot::default(); MAX_SLOTS],
            fd: [Rec::default(); I2_MAX_RECS],
            gem: [Rec::default(); I2_MAX_RECS],
            dyns: [Rec::default(); I2_MAX_RECS],
            fdo: [Out::default(); I2_MAX_RECS],
            gemo: [Out::default(); I2_MAX_RECS],
        }
    }

    fn reset(&mut self, a: &Args) {
        self.which = Which::Drm;
        self.entry = 0;
        self.max_req = a.max_req;
        self.max_resp = a.max_resp;
        self.nbuf = 0;
        self.nslot = 0;
        self.nfd = 0;
        self.ngem = 0;
        self.ndyn = 0;
        self.nfdo = 0;
        self.ngemo = 0;
        self.in_bytes = 0;
        self.out_bytes = 0;
        self.ret = 0;
    }

    /// Buffers gathered so far.
    pub fn nbuf(&self) -> u32 {
        self.nbuf
    }

    /// Every buffer gathered so far, as `(length, NVGPU_SDIR_*)`.
    pub fn buffers(&self) -> impl Iterator<Item = (u32, u8)> + '_ {
        self.buf.iter().take(idx(self.nbuf)).map(|k| (k.len, k.dir))
    }

    /// Where the walk found fields of kind `kind` (`NVGPU_SF_*`), as
    /// `(buffer, offset)`.
    pub fn positions(&self, kind: u8) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.slots()
            .iter()
            .filter(move |s| s.kind == kind)
            .map(|s| (s.buf, s.off))
    }

    /// Where the dyn records added so far point, as `(buffer, offset)`.
    pub fn dyn_positions(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.dyns
            .iter()
            .take(idx(self.ndyn))
            .map(|d| (d.0[1], d.0[2]))
    }

    /// `nvgpu_i2_buf()`: the kernel copy of buffer `buf`, `None` for one the
    /// call does not have.
    pub fn buf_mut(&mut self, buf: u32) -> Option<&mut [u8]> {
        if buf >= self.nbuf {
            return None;
        }
        Some(self.store.buf_mut(idx(buf)))
    }

    /// `nvgpu_i2_buf()`, to read.
    pub fn buf(&self, buf: u32) -> Option<&[u8]> {
        if buf >= self.nbuf {
            return None;
        }
        Some(self.store.buf(idx(buf)))
    }

    fn buf_len(&self, b: u32) -> u32 {
        self.buf.get(idx(b)).map_or(0, |k| k.len)
    }

    /// `nvgpu_i2_add_dyn()`: a dyn record naming an 8-byte value slot
    /// (ATOMIC's prop_values). 0 or -EINVAL.
    pub fn add_dyn(&mut self, kind: u32, buf: u32, off: u32, len: u32) -> i32 {
        let blen = self.buf_len(buf);
        if idx(self.ndyn) >= I2_MAX_RECS
            || buf >= self.nbuf
            || blen < 8
            || off > blen.wrapping_sub(8)
        {
            return -EINVAL;
        }
        match self.dyns.get_mut(idx(self.ndyn)) {
            Some(d) => *d = Rec([kind, buf, off, len]),
            None => return -EINVAL,
        }
        self.ndyn = self.ndyn.saturating_add(1);
        0
    }

    /// `nvgpu_i2_add_fd()`: an fd record. 0 or -EINVAL.
    pub fn add_fd(&mut self, buf: u32, off: u32, handle: u32, flags: u32) -> i32 {
        let blen = self.buf_len(buf);
        if idx(self.nfd) >= I2_MAX_RECS
            || buf >= self.nbuf
            || blen < 4
            || off > blen.wrapping_sub(4)
        {
            return -EINVAL;
        }
        match self.fd.get_mut(idx(self.nfd)) {
            Some(r) => *r = Rec([buf, off, handle, flags]),
            None => return -EINVAL,
        }
        self.nfd = self.nfd.saturating_add(1);
        0
    }

    /// `nvgpu_i2_rd()`: a little-endian unsigned field of 1, 2, 4 or 8 bytes
    /// of buffer `b`.
    fn rd(&self, b: u32, off: u32, width: u32) -> Result<u64, Errno> {
        le_n(self.store.buf(idx(b)), idx(off), idx(width)).ok_or(-EINVAL)
    }

    /// `nvgpu_i2_wr()`: only ever at positions the walk reached, which it
    /// bounds-checked, and exactly `width` bytes of them (1, 2, 4 or 8).
    fn wr(&mut self, b: u32, off: u32, width: u8, v: u64) {
        if matches!(width, 1 | 2 | 4 | 8) {
            put_le(self.store.buf_mut(idx(b)), idx(off), usize::from(width), v);
        }
    }

    /// `nvgpu_i2_new_buf()`: a new buffer of `len` bytes, the caller's
    /// `uptr`, its bytes copied in if they travel IN. The request and the
    /// reply are bounded as the walk goes, so a made-up count is refused
    /// before it is allocated.
    fn new_buf(&mut self, len: u64, dir: u8, uptr: u64) -> Result<u32, Errno> {
        if idx(self.nbuf) >= I2_MAX_BUFS {
            return Err(-E2BIG);
        }
        if dir & SDIR_IN != 0 {
            self.in_bytes = self.in_bytes.saturating_add(align8(len));
        }
        if dir & SDIR_OUT != 0 {
            self.out_bytes = self.out_bytes.saturating_add(align8(len));
        }
        if self.in_bytes > self.max_req || self.out_bytes > self.max_resp {
            return Err(-E2BIG);
        }
        let n = self.nbuf;
        let len32 = u32::try_from(len).map_err(|_| -E2BIG)?;
        if let Some(kb) = self.buf.get_mut(idx(n)) {
            *kb = KBuf {
                len: len32,
                dir,
                uptr,
                ..KBuf::default()
            };
        }
        if len32 != 0 {
            self.store.alloc(idx(n), idx(len32))?;
        }
        self.nbuf = self.nbuf.saturating_add(1);
        if dir & SDIR_IN == 0 || len32 == 0 {
            return Ok(n);
        }
        self.store.fetch(idx(n), uptr)?;
        Ok(n)
    }

    fn add_slot(&mut self, f: u32, fd: &SField, b: u32, off: u32, orig: u64) -> Result<(), Errno> {
        let s = self.slot.get_mut(idx(self.nslot)).ok_or(-E2BIG)?;
        *s = Slot {
            f,
            kind: fd.kind,
            width: fd.width,
            buf: b,
            off,
            orig,
        };
        self.nslot = self.nslot.saturating_add(1);
        Ok(())
    }

    fn slots(&self) -> &[Slot] {
        self.slot.get(..idx(self.nslot)).unwrap_or(&[])
    }

    /// `nvgpu_i2_len()`: a pointer's length in bytes, from the kernel copy of
    /// its struct. Mirrors `Walk::length` in device/src/xfer.rs.
    fn len(
        &self,
        f: &SField,
        b: u32,
        base: u32,
        first: u16,
        created: &[i32; MAX_LIST],
    ) -> Result<u64, Errno> {
        match f.len_kind {
            SLEN_CONST => Ok(u64::from(f.len_a)),
            SLEN_COUNT => {
                let n = self.rd(b, base.wrapping_add(f.len_a), u32::from(f.len_width))?;
                n.checked_mul(u64::from(f.len_elem)).ok_or(-E2BIG)
            }
            SLEN_SUM => {
                // An earlier sibling, by the generator's check; bounded
                // regardless.
                let first = u32::from(first);
                let src = if f.len_a >= first {
                    created
                        .get(idx(f.len_a.wrapping_sub(first)))
                        .copied()
                        .unwrap_or(-1)
                } else {
                    -1
                };
                let mut sum: u64 = 0;
                if let Ok(src) = u32::try_from(src) {
                    for w in self.store.buf(idx(src)).chunks_exact(4) {
                        sum = sum.wrapping_add(u64::from(le32(w, 0).unwrap_or(0)));
                    }
                }
                sum.checked_mul(u64::from(f.len_elem)).ok_or(-E2BIG)
            }
            SLEN_NVKMS_PARAMS => {
                // NvKmsIoctlParams.size, which NVKMS requires to be the
                // command's params size exactly (nvkms.c:5183), as do we.
                let n = self.rd(b, base.wrapping_add(4), 4)?;
                if n != u64::from(f.max) {
                    return Err(-EINVAL);
                }
                Ok(n)
            }
            _ => Err(-EINVAL),
        }
    }

    /// `nvgpu_i2_walk()`: the canonical traversal over one field list, `n`
    /// fields from `first`, of the struct at `base` in buffer `b`. Pointers
    /// get buffers depth first, in field order, exactly as the backend's
    /// `Walk::list` assigns them.
    fn walk(
        &mut self,
        t: &Table<'_>,
        b: u32,
        base: u32,
        first: u16,
        n: u16,
        depth: u32,
    ) -> Result<(), Errno> {
        if depth > MAX_DEPTH || usize::from(n) > MAX_LIST {
            return Err(-EINVAL);
        }
        let mut created = [-1i32; MAX_LIST];

        for i in 0..usize::from(n) {
            let fi = usize::from(first).saturating_add(i);
            let f = *t.fields.get(fi).ok_or(-EINVAL)?;
            let fi = u32::try_from(fi).map_err(|_| -EINVAL)?;
            let at = base.wrapping_add(f.off);

            if f.flags & SFF_COND != 0 {
                let v = self.rd(b, base.wrapping_add(f.cond_off), 4)?;
                // NE: an NVKMS NvBool, which the kernel tests for non-zero,
                // so a caller's 2 is as true as its 1 (Cond::holds).
                let mut holds = (v as u32 & f.cond_mask) == f.cond_value;
                if f.flags & SFF_COND_NE != 0 {
                    holds = !holds;
                }
                if !holds {
                    continue;
                }
            }

            match f.kind {
                SF_PTR => {
                    let v = self.rd(b, at, 8)?;
                    let len = self.len(&f, b, base, first, &created)?;
                    self.add_slot(fi, &f, b, at, v)?;
                    // NULL or empty: no buffer, and the host is handed NULL,
                    // which is what it would do with an empty list anyway.
                    if v == 0 || len == 0 {
                        continue;
                    }
                    if len > u64::from(f.max) {
                        return Err(-E2BIG);
                    }
                    let stride = u64::from(f.stride);
                    if f.nchild != 0 && len.checked_rem(stride) != Some(0) {
                        return Err(-EINVAL);
                    }
                    let nb = self.new_buf(len, f.dir, v)?;
                    if let Some(c) = created.get_mut(i) {
                        *c = nb as i32;
                    }
                    if let Some(kb) = self.buf.get_mut(idx(nb)) {
                        kb.has_f = true;
                        kb.f = fi;
                        kb.parent = b;
                        kb.pbase = base;
                    }
                    if matches!(f.cb_kind, SCB_PARTIAL | SCB_ALL_OR_NOTHING | SCB_EXACT) {
                        let sent =
                            self.rd(b, base.wrapping_add(f.cb_off), u32::from(f.cb_width))?;
                        if let Some(kb) = self.buf.get_mut(idx(nb)) {
                            kb.sent = sent;
                        }
                    }
                    if f.nchild != 0 {
                        // stride is non-zero here, and len fits in u32.
                        let elems = len.checked_div(stride).unwrap_or(0);
                        for e in 0..elems {
                            let eb = (e as u32).wrapping_mul(f.stride);
                            self.walk(t, nb, eb, f.child, f.nchild, depth.saturating_add(1))?;
                        }
                    }
                }
                SF_ARRAY => {
                    // Only the elements the kernel reads (Limit::elements): a
                    // descriptor field in one it never looks at is whatever
                    // the caller left there, and translating it would fail a
                    // call the kernel accepts.
                    let mut ne = f.count;
                    if f.len_kind == SLEN_COUNT {
                        let v = self.rd(b, base.wrapping_add(f.len_a), u32::from(f.len_width))?;
                        ne = v.min(u64::from(f.count)) as u32;
                    } else if f.len_kind == SLEN_PLANES {
                        let v = self.rd(b, base.wrapping_add(f.len_a), 4)?;
                        ne = match usize::try_from(v).ok().and_then(|v| t.planes.get(v)) {
                            Some(&p) => u32::from(p).min(f.count),
                            None => 0,
                        };
                    }
                    for e in 0..ne {
                        let eb = at.wrapping_add(e.wrapping_mul(f.stride));
                        self.walk(t, b, eb, f.child, f.nchild, depth.saturating_add(1))?;
                    }
                }
                _ => {
                    // A descriptor is 4 or 8 bytes and a GEM handle 4, as the
                    // generator checks; a table that disagreed would have its
                    // caller's values put back at another width.
                    let bad = match f.kind {
                        SF_FD_IN | SF_FD_OUT => !matches!(f.width, 4 | 8),
                        SF_GEM_IN | SF_GEM_OUT => f.width != 4,
                        _ => false,
                    };
                    if bad {
                        return Err(-EINVAL);
                    }
                    let v = self.rd(b, at, u32::from(f.width))?;
                    self.add_slot(fi, &f, b, at, v)?;
                }
            }
        }
        Ok(())
    }

    fn count(&self, kind: u8) -> u32 {
        let mut n = 0u32;
        for s in self.slots() {
            if s.kind == kind {
                n = n.saturating_add(1);
            }
        }
        n
    }

    /// `nvgpu_i2_gather()`: find the entry, copy the argument, walk it.
    fn gather<E: Env<S>>(
        &mut self,
        env: &mut E,
        set: &SchemaSet<'_>,
        a: &Args,
    ) -> Result<(), Errno> {
        let size = ioc_size(a.cmd);
        let mut dir = 0u8;
        if ioc_dir(a.cmd) & 1 != 0 {
            dir |= SDIR_IN;
        }
        if ioc_dir(a.cmd) & 2 != 0 {
            dir |= SDIR_OUT;
        }

        if a.sclass != SCLASS_MODESET {
            let (w, e) = set.lookup(a.sclass, a.cmd, &[]).ok_or(-ENOTTY)?;
            self.which = w;
            self.entry = e;
            let entry = set.table(w).ioctls.get(e).ok_or(-ENOTTY)?;
            if entry.cmd != a.cmd {
                env.warn(Warn::CmdSize {
                    entry,
                    cmd: a.cmd,
                    size,
                });
                return Err(-EINVAL);
            }
        } else if a.cmd != NVKMS_IOCTL_IOWR {
            return Err(-ENOTTY);
        }

        // 32-bit callers: every KMS struct is layout-identical except these
        // two, whose compat forms the core converts (drm_ioc32.c:334-346).
        // We do not convert; refusing is better than handing the host a
        // misparsed struct.
        if a.sclass == SCLASS_KMS && a.compat && (ioc_nr(a.cmd) == 0x3a || ioc_nr(a.cmd) == 0xb8) {
            return Err(-EINVAL);
        }

        // NVKMS never writes the outer struct (nvidia-modeset-linux.c:1963).
        if a.sclass == SCLASS_MODESET {
            dir = SDIR_IN;
        }
        self.new_buf(u64::from(size), dir, a.uarg)?;
        if a.sclass == SCLASS_MODESET {
            let (w, e) = set
                .lookup(a.sclass, a.cmd, self.store.buf(0))
                .ok_or(-ENOTTY)?;
            self.which = w;
            self.entry = e;
        }

        let t = set.table(self.which);
        let entry = *t.ioctls.get(self.entry).ok_or(-ENOTTY)?;
        self.walk(&t, 0, 0, entry.field, entry.nfield, 0)?;
        if idx(self.count(SF_FD_OUT)) > I2_MAX_RECS || idx(self.count(SF_GEM_OUT)) > I2_MAX_RECS {
            return Err(-E2BIG);
        }
        Ok(())
    }

    /// `nvgpu_i2_build()`: the request, into `tb`.
    fn build<E: Env<S>>(&self, env: &mut E, tb: &mut E::TBuf, a: &Args) -> Result<(), Errno> {
        let mut h = [0u8; HDR_LEN + I2_REQ_LEN];
        let words = [
            MSG_IOCTL2,
            a.handle,
            0,
            0,
            a.cmd,
            0,
            self.nbuf,
            self.nfd,
            self.ngem,
            self.ndyn,
            self.in_bytes as u32,
            a.render,
        ];
        for (c, w) in h.chunks_exact_mut(4).zip(words) {
            copy(c, &w.to_le_bytes());
        }
        let mut off = 0usize;
        env.tbuf_write(tb, off, &h)?;
        off = off.saturating_add(h.len());
        for kb in self.buf.iter().take(idx(self.nbuf)) {
            env.tbuf_write(tb, off, &kb.len.to_le_bytes())?;
            off = off.saturating_add(4);
        }
        for (recs, n) in [
            (&self.fd, self.nfd),
            (&self.gem, self.ngem),
            (&self.dyns, self.ndyn),
        ] {
            for r in recs.iter().take(idx(n)) {
                let mut b = [0u8; I2_REC_LEN];
                for (c, w) in b.chunks_exact_mut(4).zip(r.0) {
                    copy(c, &w.to_le_bytes());
                }
                env.tbuf_write(tb, off, &b)?;
                off = off.saturating_add(I2_REC_LEN);
            }
        }
        for i in 0..idx(self.nbuf) {
            let Some(kb) = self.buf.get(i) else { break };
            if kb.dir & SDIR_IN == 0 {
                continue;
            }
            let len = idx(kb.len);
            let pad = idx(align8(u64::from(kb.len)) as u32).saturating_sub(len);
            env.tbuf_write(tb, off, self.store.buf(i))?;
            env.tbuf_write(
                tb,
                off.saturating_add(len),
                [0u8; 8].get(..pad).unwrap_or(&[]),
            )?;
            off = off.saturating_add(len).saturating_add(pad);
        }
        Ok(())
    }

    fn at_slot(&self, kind: u8, b: u32, off: u32) -> bool {
        self.slots()
            .iter()
            .any(|s| s.kind == kind && s.buf == b && s.off == off)
    }

    fn at_dyn(&self, b: u32, off: u32) -> bool {
        self.dyns
            .iter()
            .take(idx(self.ndyn))
            .any(|d| d.0[1] == b && d.0[2] == off)
    }

    /// `nvgpu_i2_drop_outs()`: every handle a reply created and nobody will
    /// own now.
    fn drop_outs<E: Env<S>>(&mut self, env: &mut E, fd_from: u32, gem_from: u32) {
        for o in self.fdo.get(idx(fd_from)..idx(self.nfdo)).unwrap_or(&[]) {
            env.close_handle(o.handle);
        }
        // A handle an earlier record named is closed already, or owned now
        // by the proxy made for it (below `gem_from`): never closed here.
        let gemo = self.gemo.get(..idx(self.ngemo)).unwrap_or(&[]);
        for i in idx(gem_from)..gemo.len() {
            let Some(o) = gemo.get(i) else { break };
            if !gemo
                .get(..i)
                .unwrap_or(&[])
                .iter()
                .any(|p| p.handle == o.handle)
            {
                env.gem_close(o.handle);
            }
        }
        self.nfdo = fd_from;
        self.ngemo = gem_from;
    }

    /// `nvgpu_i2_drop_consumed()`: descriptors the hooks handed us to be
    /// consumed by the call. The backend closes them once the call has run;
    /// if it never runs, that is ours to do.
    fn drop_consumed<E: Env<S>>(&self, env: &mut E) {
        for r in self.fd.iter().take(idx(self.nfd)) {
            if r.0[3] & I2_FD_CONSUME != 0 {
                env.close_handle(r.0[2]);
            }
        }
    }

    /// `nvgpu_i2_translate()`: the caller's descriptors and GEM handles,
    /// through the hooks.
    fn translate<E: Env<S>>(&mut self, env: &mut E, t: &Table<'_>) -> Result<(), Errno> {
        let mut i = 0usize;
        while i < idx(self.nslot) {
            let Some(&s) = self.slot.get(i) else { break };
            i = i.saturating_add(1);
            let f = *t.fields.get(idx(s.f)).ok_or(-EINVAL)?;
            if s.kind == SF_FD_IN {
                let v = sext(s.orig, s.width);
                if v == i64::from(f.none_value) {
                    continue;
                }
                let (ret, handle, flags) = env.fd_in(self, s.buf, s.off, v, f.kinds);
                if ret < 0 {
                    return Err(ret);
                }
                // Our number means nothing to the host; the backend writes
                // its own descriptor here, and the caller's comes back from
                // the slot.
                self.wr(s.buf, s.off, s.width, i64::from(f.none_value) as u64);
                if ret == 1 {
                    continue;
                }
                let r = self.add_fd(s.buf, s.off, handle, flags);
                if r != 0 {
                    if flags & I2_FD_CONSUME != 0 {
                        env.close_handle(handle);
                    }
                    return Err(r);
                }
            } else if s.kind == SF_GEM_IN {
                if s.orig == 0 {
                    continue;
                }
                let (ret, owner, gem) = env.gem_in(self, s.buf, s.off, s.orig as u32);
                if ret != 0 {
                    return Err(ret);
                }
                // The backend holds a zero field to have no record and any
                // other to have exactly one.
                if gem == 0 || idx(self.ngem) >= I2_MAX_RECS {
                    return Err(-EINVAL);
                }
                self.wr(s.buf, s.off, 4, u64::from(gem));
                if let Some(r) = self.gem.get_mut(idx(self.ngem)) {
                    *r = Rec([s.buf, s.off, owner, gem]);
                }
                self.ngem = self.ngem.saturating_add(1);
            }
        }
        Ok(())
    }

    /// `nvgpu_i2_parse()`: read the reply into the kernel copies. Everything
    /// is checked against what was sent and what the device says it wrote:
    /// buffer count, data length, record counts no larger than the schema
    /// allows, and every record at a position the schema (or a dyn record
    /// sent) names, at most once.
    fn parse<E: Env<S>>(
        &mut self,
        env: &mut E,
        entry: &SIoctl,
        tb: &E::TBuf,
        used: u32,
        max_fdo: u32,
        max_gemo: u32,
    ) -> Result<(), Errno> {
        // The status first, from the header alone: a refusal is a bare
        // nvgpu_msg_hdr, with no nvgpu_i2_resp behind it.
        let mut h = [0u8; HDR_LEN + I2_RESP_LEN];
        let usedz = idx(used);
        if usedz < HDR_LEN {
            return Err(-EPROTO);
        }
        env.tbuf_read(tb, 0, h.get_mut(..HDR_LEN).unwrap_or(&mut []))
            .map_err(|_| -EPROTO)?;
        let status = le32(&h, 8).unwrap_or(0) as i32;
        if status != 0 {
            // Refused before the call ran: nothing was consumed or created.
            self.drop_consumed(env);
            return Err(if (-MAX_ERRNO..0).contains(&status) {
                status
            } else {
                -EPROTO
            });
        }
        if usedz < h.len() {
            return Err(-EPROTO);
        }
        env.tbuf_read(tb, 0, &mut h).map_err(|_| -EPROTO)?;
        let w = |i: usize| le32(&h, HDR_LEN.saturating_add(i.saturating_mul(4))).unwrap_or(0);
        let ret = w(0) as i32;
        let (nbuf, nfd, ngem, data_len) = (w(1), w(2), w(3), w(4));
        let need = (h.len() as u64)
            .saturating_add(self.out_bytes)
            .saturating_add(u64::from(nfd).saturating_mul(I2_REC_LEN as u64))
            .saturating_add(u64::from(ngem).saturating_mul(I2_GEM_OUT_LEN as u64));
        if nbuf != self.nbuf
            || u64::from(data_len) != self.out_bytes
            || nfd > max_fdo
            || ngem > max_gemo
            || u64::from(used) < need
            || ret < -MAX_ERRNO
        {
            env.warn(Warn::Malformed {
                entry,
                used,
                nfd,
                ngem,
            });
            return Err(-EPROTO);
        }

        let mut off = h.len();
        for i in 0..idx(self.nbuf) {
            let kb = self.buf.get(i).copied().unwrap_or_default();
            if kb.dir & SDIR_OUT == 0 {
                continue;
            }
            env.tbuf_read(tb, off, self.store.buf_mut(i))
                .map_err(|_| -EPROTO)?;
            off = off.saturating_add(align8(u64::from(kb.len)) as usize);
        }
        for i in 0..nfd {
            let mut r = [0u8; I2_REC_LEN];
            if env.tbuf_read(tb, off, &mut r).is_err() {
                self.nfdo = i;
                self.drop_outs(env, 0, 0);
                return Err(-EPROTO);
            }
            let w = |j: usize| le32(&r, j.saturating_mul(4)).unwrap_or(0);
            if let Some(o) = self.fdo.get_mut(idx(i)) {
                *o = Out {
                    buf: w(0),
                    off: w(1),
                    handle: w(2),
                    kind: w(3),
                    size: 0,
                };
            }
            off = off.saturating_add(I2_REC_LEN);
        }
        self.nfdo = nfd;
        for i in 0..ngem {
            let mut r = [0u8; I2_GEM_OUT_LEN];
            if env.tbuf_read(tb, off, &mut r).is_err() {
                self.ngemo = i;
                self.drop_outs(env, 0, 0);
                return Err(-EPROTO);
            }
            let w = |j: usize| le32(&r, j.saturating_mul(4)).unwrap_or(0);
            if let Some(o) = self.gemo.get_mut(idx(i)) {
                *o = Out {
                    buf: w(0),
                    off: w(1),
                    handle: w(2),
                    kind: 0,
                    size: le64(&r, 16).unwrap_or(0),
                };
            }
            off = off.saturating_add(I2_GEM_OUT_LEN);
        }
        self.ngemo = ngem;

        let mut ok = true;
        for i in 0..idx(self.nfdo) {
            let o = self.fdo.get(i).copied().unwrap_or_default();
            ok = self.at_slot(SF_FD_OUT, o.buf, o.off) || self.at_dyn(o.buf, o.off);
            ok = ok
                && !self
                    .fdo
                    .iter()
                    .take(i)
                    .any(|p| p.buf == o.buf && p.off == o.off);
            if !ok {
                break;
            }
        }
        for i in 0..idx(self.ngemo) {
            if !ok {
                break;
            }
            let o = self.gemo.get(i).copied().unwrap_or_default();
            ok = o.handle != 0 && self.at_slot(SF_GEM_OUT, o.buf, o.off);
            ok = ok
                && !self
                    .gemo
                    .iter()
                    .take(i)
                    .any(|p| p.buf == o.buf && p.off == o.off);
        }
        if !ok {
            env.warn(Warn::Unnamed { entry });
            self.drop_outs(env, 0, 0);
            return Err(-EPROTO);
        }
        self.ret = ret;
        Ok(())
    }

    /// `nvgpu_i2_restore()`: the caller's own values back where ours or the
    /// backend's were: its pointers, its descriptor numbers, its GEM
    /// handles. Descriptor-out and GEM-out fields start empty and are filled
    /// by the hooks.
    fn restore(&mut self) {
        for i in 0..idx(self.nslot) {
            let Some(&s) = self.slot.get(i) else { break };
            match s.kind {
                SF_PTR => self.wr(s.buf, s.off, 8, s.orig),
                SF_FD_IN => self.wr(s.buf, s.off, s.width, s.orig),
                SF_GEM_IN => self.wr(s.buf, s.off, 4, s.orig),
                SF_FD_OUT => self.wr(s.buf, s.off, s.width, u64::MAX),
                SF_GEM_OUT => self.wr(s.buf, s.off, 4, 0),
                _ => {}
            }
        }
    }

    /// `nvgpu_i2_outputs()`: what the host made, made ours: one proxy per
    /// distinct host GEM handle (GETFB2 names one object once per plane),
    /// and a guest fd per descriptor. A hook that fails leaves the rest to
    /// be closed here.
    fn outputs<E: Env<S>>(&mut self, env: &mut E) -> Result<(), Errno> {
        for i in 0..idx(self.ngemo) {
            let o = self.gemo.get(i).copied().unwrap_or_default();
            let kind = match self.gemo.iter().take(i).find(|p| p.handle == o.handle) {
                Some(p) => p.kind,
                None => {
                    let (ret, kind) = env.gem_out(self, o.buf, o.off, o.handle, o.size);
                    if ret != 0 {
                        self.drop_outs(env, 0, i as u32);
                        return Err(ret);
                    }
                    kind
                }
            };
            if let Some(g) = self.gemo.get_mut(i) {
                g.kind = kind;
            }
            self.wr(o.buf, o.off, 4, u64::from(kind));
        }

        for i in 0..idx(self.nfdo) {
            let o = self.fdo.get(i).copied().unwrap_or_default();
            // Handle 0: the host made the descriptor but the backend could
            // not keep it (its handle table was full) and closed it again.
            // What the call made is gone, so the caller hears it failed, the
            // way a host process out of descriptors would.
            let (ret, v) = if o.handle == 0 {
                (-EMFILE, -1)
            } else {
                env.fd_out(self, o.buf, o.off, o.handle, o.kind)
            };
            if ret != 0 {
                self.drop_outs(env, i as u32, self.ngemo);
                return Err(ret);
            }
            // A dyn position (ATOMIC's out-fence) has no slot and is in no
            // buffer we copy back; the special writes the caller's pointer
            // itself.
            for j in 0..idx(self.nslot) {
                let Some(&s) = self.slot.get(j) else { break };
                if s.kind == SF_FD_OUT && s.buf == o.buf && s.off == o.off {
                    self.wr(o.buf, o.off, s.width, v as u64);
                }
            }
        }
        Ok(())
    }

    /// `nvgpu_i2_copy_back()`: every OUT buffer, as much of it as the
    /// kernel would have written. The argument goes back whole whatever
    /// happened, as drm_ioctl copies out_size bytes back after the handler
    /// whatever it returned.
    fn copy_back(&mut self, t: &Table<'_>) -> Result<(), Errno> {
        let mut fault = Ok(());
        for i in 0..idx(self.nbuf) {
            let kb = self.buf.get(i).copied().unwrap_or_default();
            let len = u64::from(kb.len);
            let (mut start, mut end, mut left) = (0u64, len, 0u64);
            if kb.dir & SDIR_OUT == 0 || kb.len == 0 {
                continue;
            }
            if kb.has_f {
                let Some(f) = t.fields.get(idx(kb.f)) else {
                    continue;
                };
                if matches!(
                    f.cb_kind,
                    SCB_PARTIAL | SCB_ALL_OR_NOTHING | SCB_EXACT | SCB_WRITTEN
                ) {
                    match self.rd(
                        kb.parent,
                        kb.pbase.wrapping_add(f.cb_off),
                        u32::from(f.cb_width),
                    ) {
                        Ok(v) => left = v,
                        Err(_) => continue,
                    }
                }
                (start, end) = copy_extent(f, kb.dir, len, self.ret, kb.sent, left);
            }
            if end <= start {
                continue;
            }
            if self
                .store
                .copy_out(i, kb.uptr, idx(start as u32), idx(end as u32))
                .is_err()
            {
                fault = Err(-EFAULT);
            }
        }
        fault
    }
}

/// `nvgpu_i2_copy_extent()`: which bytes `[start, end)` of an OUT buffer
/// reach the caller. A line-for-line mirror of `CopyBack::extent`
/// (gen/src/schema/mod.rs), whose tests are the kernel's fill rules stated
/// as cases.
pub fn copy_extent(f: &SField, dir: u8, len: u64, ret: i32, sent: u64, left: u64) -> (u64, u64) {
    let ok = ret == 0;
    let arg = u64::from(f.cb_arg);
    let n = match f.cb_kind {
        SCB_FULL if ok || dir == SDIR_INOUT => len,
        SCB_PARTIAL if ok => sent.min(left).saturating_mul(arg),
        SCB_ALL_OR_NOTHING if ok && left <= sent => left.saturating_mul(arg),
        SCB_EXACT if ok && left == sent => len,
        SCB_RANGE => {
            let off = u64::from(f.cb_off);
            return (off.min(len), off.saturating_add(arg).min(len));
        }
        SCB_WRITTEN => left,
        _ => 0,
    };
    (0, n.min(len))
}

/// `nvgpu_i2_has_schema()`: is there a schema for this call?
pub fn has_schema(set: &SchemaSet<'_>, sclass: u32, cmd: u32, prefix: &[u8]) -> bool {
    set.lookup(sclass, cmd, prefix).is_some()
}

/// `nvgpu_i2_native_cmd()`: the command of the DRM-class entry numbered as
/// `cmd` (by type and number; the one of exactly `cmd` if there is one, as
/// [`run`] looks it up), which a caller's argument of another size or
/// direction is normalised to before the call, as `drm_ioctl()` does; 0 if
/// there is none. NVKMS's entries share one command, so never for those.
pub fn native_cmd(set: &SchemaSet<'_>, sclass: u32, cmd: u32) -> u32 {
    if sclass == SCLASS_MODESET {
        return 0;
    }
    match set.lookup(sclass, cmd, &[]) {
        Some((w, e)) => set.table(w).ioctls.get(e).map_or(0, |e| e.cmd),
        None => 0,
    }
}

/// `nvgpu_i2_ioctl()` from the gather on: run an ioctl through IOCTL2 --
/// gather the caller's buffers, translate descriptor and GEM fields through
/// the hooks, send, copy every OUT buffer back per the schema's copy-back
/// rule (also when the host call failed), and materialise descriptor and
/// GEM outputs. Returns the host ioctl's result (0 or -errno) or a
/// transport or validation -errno. (Whether the device speaks v2 at all is
/// the caller's to check first.)
pub fn run<S: Store, E: Env<S>>(
    env: &mut E,
    st: &mut State<S>,
    set: &SchemaSet<'_>,
    a: &Args,
) -> i32 {
    st.reset(a);
    if let Err(e) = st.gather(env, set, a) {
        return e;
    }
    let t = set.table(st.which);
    let Some(entry) = t.ioctls.get(st.entry).copied() else {
        return -ENOTTY;
    };

    let mut ret = match st.translate(env, &t) {
        Ok(()) => 0,
        Err(e) => e,
    };
    if ret == 0 && entry.special == SSPECIAL_ATOMIC {
        ret = env.special(st, u32::from(SSPECIAL_ATOMIC), 0);
    }
    // The caller's own look at exactly what will be sent (event
    // reservations, rewriting a blocking wait into an event, nvgpu_kms.c).
    if ret == 0 {
        ret = env.phase(st, 0);
    }
    if ret != 0 {
        st.drop_consumed(env);
        return ret;
    }

    let max_fdo = st.count(SF_FD_OUT).saturating_add(st.ndyn);
    let max_gemo = st.count(SF_GEM_OUT);
    // A reply may name one descriptor per slot and dyn record, each kept in
    // `fdo`: a call that could be answered with more than it holds is not
    // sent (one past it was skipped, and its handle never closed).
    if idx(max_fdo) > I2_MAX_RECS {
        st.drop_consumed(env);
        return -E2BIG;
    }
    let rec = I2_REC_LEN as u64;
    let req_len = ((HDR_LEN + I2_REQ_LEN) as u64)
        .saturating_add(4u64.saturating_mul(u64::from(st.nbuf)))
        .saturating_add(rec.saturating_mul(u64::from(st.nfd)))
        .saturating_add(rec.saturating_mul(u64::from(st.ngem)))
        .saturating_add(rec.saturating_mul(u64::from(st.ndyn)))
        .saturating_add(st.in_bytes);
    let resp_len = ((HDR_LEN + I2_RESP_LEN) as u64)
        .saturating_add(st.out_bytes)
        .saturating_add(rec.saturating_mul(u64::from(max_fdo)))
        .saturating_add((I2_GEM_OUT_LEN as u64).saturating_mul(u64::from(max_gemo)));
    if req_len > a.max_req || resp_len > a.max_resp {
        st.drop_consumed(env);
        return -E2BIG;
    }
    let req = env.tbuf_alloc(req_len as usize);
    let resp = env.tbuf_alloc(resp_len as usize);
    let (Some(mut req), Some(resp)) = (req, resp) else {
        st.drop_consumed(env);
        return -ENOMEM;
    };
    // What the request names is held as long as the request is: past a
    // timeout or a signal too, until the transport knows it is done.
    env.hand_over_held(st, &mut req);
    if let Err(e) = st.build(env, &mut req, a) {
        st.drop_consumed(env);
        drop(req);
        drop(resp);
        return e;
    }

    // Every call on a KMS or modeset file waits behind that file's executor
    // on the host; a render-node call only if its entry says so.
    let mut xflags = a.xflags;
    if a.sclass != SCLASS_RENDER || entry.flags & SIO_EXECUTOR != 0 {
        xflags |= XF_EXECUTOR;
    }
    let (req, resp, used) = match env.xfer(req, resp, xflags) {
        // The transport owns both buffers now, closes whatever the late
        // reply creates, and releases the consumed handles if the call never
        // runs -- so neither is freed nor dropped here.
        Xfer::Abandoned { err } => return err,
        Xfer::Failed { req, resp, err } => {
            st.drop_consumed(env);
            drop(req);
            drop(resp);
            return err;
        }
        Xfer::Done { req, resp, used } => (req, resp, used),
    };

    let ret = (|| {
        st.parse(env, &entry, &resp, used, max_fdo, max_gemo)?;
        st.restore();
        // The host has answered: whatever the caller does with that happens
        // before anything is materialised, so a refusal here still drops
        // every output.
        let r = env.phase(st, 1);
        if r != 0 {
            st.drop_outs(env, 0, 0);
            return Err(r);
        }
        st.outputs(env)?;
        if entry.special == SSPECIAL_ATOMIC {
            let r = env.special(st, u32::from(SSPECIAL_ATOMIC), 1);
            if r != 0 {
                return Err(r);
            }
        }
        st.copy_back(&t)?;
        Ok(st.ret)
    })();
    drop(req);
    drop(resp);
    match ret {
        Ok(r) | Err(r) => r,
    }
}

/// The errors [`Xfer::Abandoned`] carries: the transport owns the buffers.
pub fn abandons(err: Errno) -> bool {
    err == -ETIMEDOUT || err == -EINTR
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used,
    clippy::panic
)]
mod tests {
    use super::*;
    use crate::guest::schema::{SIoctl, SCB_PARTIAL, SCLASS_KMS, SDIR_OUT};
    use std::boxed::Box;
    use std::collections::BTreeMap;
    use std::vec;
    use std::vec::Vec;

    const EBADF_: i32 = 9;
    const EPERM_: i32 = 1;
    const ARG: u64 = 0x1000;
    const LIST: u64 = 0x9000;

    /// `_IOWR('d', nr, size)`.
    fn iowr(nr: u32, size: u32) -> u32 {
        (3 << 30) | (size << 16) | (0x64 << 8) | nr
    }

    /// A struct of 32 bytes: a pointer at 0 to `count` (at 8) u32s the host
    /// fills (PARTIAL), a descriptor at 12 (-1: none), a GEM handle at 16,
    /// and a descriptor the host writes at 20.
    fn table() -> (Vec<SIoctl>, Vec<SField>) {
        let ioctls = vec![SIoctl {
            name: core::ptr::null(),
            cmd: iowr(0xa0, 32),
            nvkms_cmd: 0,
            size: 32,
            sclass: SCLASS_KMS as u8,
            special: 0,
            flags: 0,
            policy: 0,
            field: 0,
            nfield: 4,
        }];
        let fields = vec![
            SField {
                off: 0,
                kind: SF_PTR,
                width: 8,
                dir: SDIR_OUT,
                len_kind: SLEN_COUNT,
                len_a: 8,
                len_width: 4,
                len_elem: 4,
                max: 64,
                cb_kind: SCB_PARTIAL,
                cb_off: 8,
                cb_width: 4,
                cb_arg: 4,
                ..SField::default()
            },
            SField {
                off: 12,
                kind: SF_FD_IN,
                width: 4,
                none_value: -1,
                kinds: 7,
                ..SField::default()
            },
            SField {
                off: 16,
                kind: SF_GEM_IN,
                width: 4,
                ..SField::default()
            },
            SField {
                off: 20,
                kind: SF_FD_OUT,
                width: 4,
                ..SField::default()
            },
        ];
        (ioctls, fields)
    }

    /// The caller's memory and the kernel copies.
    #[derive(Default)]
    struct Mem {
        user: BTreeMap<u64, Vec<u8>>,
        bufs: BTreeMap<usize, Vec<u8>>,
        fetches: Vec<(u64, usize)>,
    }

    impl Mem {
        fn region(&mut self, a: u64, n: usize) -> Option<&mut [u8]> {
            let (&base, r) = self.user.range_mut(..=a).next_back()?;
            let off = (a - base) as usize;
            r.get_mut(off..off + n)
        }
    }

    impl Store for Mem {
        fn alloc(&mut self, i: usize, len: usize) -> Result<(), Errno> {
            self.bufs.insert(i, vec![0; len]);
            Ok(())
        }
        fn fetch(&mut self, i: usize, uptr: u64) -> Result<(), Errno> {
            let n = self.bufs[&i].len();
            self.fetches.push((uptr, n));
            let src = self.region(uptr, n).ok_or(-EFAULT)?.to_vec();
            self.bufs.get_mut(&i).unwrap().copy_from_slice(&src);
            Ok(())
        }
        fn buf(&self, i: usize) -> &[u8] {
            self.bufs.get(&i).map_or(&[], |b| b.as_slice())
        }
        fn buf_mut(&mut self, i: usize) -> &mut [u8] {
            self.bufs.get_mut(&i).map_or(&mut [], |b| b.as_mut_slice())
        }
        fn copy_out(&mut self, i: usize, uptr: u64, start: usize, end: usize) -> Result<(), Errno> {
            let src = self.bufs[&i][start..end].to_vec();
            self.region(uptr + start as u64, end - start)
                .ok_or(-EFAULT)?
                .copy_from_slice(&src);
            Ok(())
        }
    }

    /// Hooks and a backend that answers with `reply(request)`.
    struct Fake {
        reply: fn(&[u8]) -> Vec<u8>,
        sent: Vec<Vec<u8>>,
        closed: Vec<u32>,
    }

    impl Env<Mem> for Fake {
        type TBuf = Vec<u8>;
        fn fd_in(&mut self, _: &mut State<Mem>, _: u32, _: u32, v: i64, _: u32) -> (i32, u32, u32) {
            if v == 3 {
                (0, 103, I2_FD_CONSUME)
            } else {
                (-EBADF_, 0, 0)
            }
        }
        fn gem_in(&mut self, _: &mut State<Mem>, _: u32, _: u32, g: u32) -> (i32, u32, u32) {
            (0, 9, g + 1000)
        }
        fn fd_out(&mut self, _: &mut State<Mem>, _: u32, _: u32, h: u32, _: u32) -> (i32, i64) {
            (0, i64::from(h) + 50)
        }
        fn gem_out(&mut self, _: &mut State<Mem>, _: u32, _: u32, g: u32, _: u64) -> (i32, u32) {
            (0, g)
        }
        fn special(&mut self, _: &mut State<Mem>, _: u32, _: i32) -> i32 {
            0
        }
        fn phase(&mut self, _: &mut State<Mem>, _: i32) -> i32 {
            0
        }
        fn close_handle(&mut self, h: u32) {
            self.closed.push(h);
        }
        fn gem_close(&mut self, _: u32) {}
        fn tbuf_alloc(&mut self, len: usize) -> Option<Vec<u8>> {
            Some(vec![0; len])
        }
        fn tbuf_write(&mut self, tb: &mut Vec<u8>, off: usize, src: &[u8]) -> Result<(), Errno> {
            tb.get_mut(off..off + src.len())
                .ok_or(-EINVAL)?
                .copy_from_slice(src);
            Ok(())
        }
        fn tbuf_read(&mut self, tb: &Vec<u8>, off: usize, dst: &mut [u8]) -> Result<(), Errno> {
            dst.copy_from_slice(tb.get(off..off + dst.len()).ok_or(-EINVAL)?);
            Ok(())
        }
        fn hand_over_held(&mut self, _: &mut State<Mem>, _: &mut Vec<u8>) {}
        fn xfer(&mut self, req: Vec<u8>, mut resp: Vec<u8>, _: u32) -> Xfer<Vec<u8>> {
            self.sent.push(req.clone());
            let r = (self.reply)(&req);
            let n = r.len().min(resp.len());
            resp[..n].copy_from_slice(&r[..n]);
            Xfer::Done {
                req,
                resp,
                used: n as u32,
            }
        }
        fn warn(&mut self, _: Warn<'_>) {}
    }

    fn words(ws: &[u32]) -> Vec<u8> {
        ws.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    fn arg(ptr: u64, count: u32, fd: i32, gem: u32) -> Vec<u8> {
        let mut a = vec![0u8; 32];
        a[0..8].copy_from_slice(&ptr.to_le_bytes());
        a[8..12].copy_from_slice(&count.to_le_bytes());
        a[12..16].copy_from_slice(&fd.to_le_bytes());
        a[16..20].copy_from_slice(&gem.to_le_bytes());
        a
    }

    fn run_with(a: Vec<u8>, reply: fn(&[u8]) -> Vec<u8>) -> (i32, Box<State<Mem>>, Fake) {
        let (ioctls, fields) = table();
        let set = SchemaSet {
            drm: Table {
                ioctls: &ioctls,
                fields: &fields,
                planes: &[],
            },
            modeset: None,
        };
        let mut mem = Mem::default();
        mem.user.insert(ARG, a);
        mem.user.insert(LIST, vec![0xee; 16]);
        let mut st = Box::new(State::new(mem));
        let mut env = Fake {
            reply,
            sent: Vec::new(),
            closed: Vec::new(),
        };
        let args = Args {
            sclass: SCLASS_KMS,
            cmd: iowr(0xa0, 32),
            uarg: ARG,
            handle: 7,
            render: 8,
            max_req: 1 << 16,
            max_resp: 1 << 16,
            ..Args::default()
        };
        let r = run(&mut env, &mut st, &set, &args);
        (r, st, env)
    }

    /// A good reply: the argument with count 2 and the list's four words,
    /// and a new descriptor at buffer 0, offset 20.
    fn good(_: &[u8]) -> Vec<u8> {
        let mut r = words(&[MSG_IOCTL2, 7, 0, 0, 0, 2, 1, 0, 48, 0, 0, 0]);
        r.extend_from_slice(&arg(0, 2, -1, 0));
        r.extend_from_slice(&words(&[11, 22, 33, 44]));
        r.extend_from_slice(&words(&[0, 20, 40, 1]));
        r
    }

    #[test]
    fn gathers_translates_and_builds_the_request() {
        let (r, st, env) = run_with(arg(LIST, 4, 3, 5), good);
        assert_eq!(r, 0);
        // Each buffer copied in once: the argument; the list is OUT only.
        assert_eq!(st.store.fetches, [(ARG, 32)]);
        let q = &env.sent[0];
        // Header; cmd, flags, nbuf 2, nfd 1, ngem 1, ndyn 0, IN bytes 32,
        // render 8; the buffer lengths; the records; the argument.
        assert_eq!(&q[..16], &words(&[MSG_IOCTL2, 7, 0, 0])[..]);
        assert_eq!(
            &q[16..48],
            &words(&[iowr(0xa0, 32), 0, 2, 1, 1, 0, 32, 8])[..]
        );
        assert_eq!(&q[48..56], &words(&[32, 16])[..]);
        assert_eq!(&q[56..72], &words(&[0, 12, 103, I2_FD_CONSUME])[..]);
        assert_eq!(&q[72..88], &words(&[0, 16, 9, 1005])[..]);
        // The pointer as the caller wrote it, the descriptor as none, the GEM
        // handle as the host's.
        let sent = &q[88..120];
        assert_eq!(&sent[0..8], &LIST.to_le_bytes());
        assert_eq!(&sent[12..16], &(-1i32).to_le_bytes());
        assert_eq!(&sent[16..20], &1005u32.to_le_bytes());
    }

    #[test]
    fn copies_back_what_the_kernel_would() {
        let (r, st, _) = run_with(arg(LIST, 4, 3, 5), good);
        assert_eq!(r, 0);
        let a = &st.store.user[&ARG];
        // The caller's pointer, descriptor and GEM handle back; the count as
        // the host left it; the new descriptor, materialised.
        assert_eq!(&a[0..8], &LIST.to_le_bytes());
        assert_eq!(&a[8..12], &2u32.to_le_bytes());
        assert_eq!(&a[12..16], &3i32.to_le_bytes());
        assert_eq!(&a[16..20], &5u32.to_le_bytes());
        assert_eq!(&a[20..24], &90u32.to_le_bytes());
        // PARTIAL: min(sent 4, now 2) entries.
        assert_eq!(
            &st.store.user[&LIST][..],
            &[11, 0, 0, 0, 22, 0, 0, 0, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee, 0xee]
        );
    }

    #[test]
    fn refuses_before_allocating_or_sending() {
        // A count past the field's maximum.
        let (r, st, env) = run_with(arg(LIST, 17, -1, 0), good);
        assert_eq!(r, -E2BIG);
        assert!(env.sent.is_empty() && st.nbuf() == 1);
        // A descriptor that is not ours.
        let (r, _, env) = run_with(arg(LIST, 1, 4, 0), good);
        assert_eq!(r, -EBADF_);
        assert!(env.sent.is_empty());
        // An argument that is not there; another size of a known number;
        // an unknown number.
        let (ioctls, fields) = table();
        let set = SchemaSet {
            drm: Table {
                ioctls: &ioctls,
                fields: &fields,
                planes: &[],
            },
            modeset: None,
        };
        let mut st = Box::new(State::new(Mem::default()));
        let mut env = Fake {
            reply: good,
            sent: Vec::new(),
            closed: Vec::new(),
        };
        let args = Args {
            sclass: SCLASS_KMS,
            cmd: iowr(0xa0, 32),
            uarg: 0xdead_0000,
            max_req: 4096,
            max_resp: 4096,
            ..Args::default()
        };
        assert_eq!(run(&mut env, &mut st, &set, &args), -EFAULT);
        let args = Args {
            cmd: iowr(0xa0, 24),
            ..args
        };
        assert_eq!(run(&mut env, &mut st, &set, &args), -EINVAL);
        let args = Args {
            cmd: iowr(0xa1, 32),
            ..args
        };
        assert_eq!(run(&mut env, &mut st, &set, &args), -ENOTTY);
    }

    #[test]
    fn a_callers_size_or_direction_finds_the_native_command() {
        let (ioctls, fields) = table();
        let set = SchemaSet {
            drm: Table {
                ioctls: &ioctls,
                fields: &fields,
                planes: &[],
            },
            modeset: None,
        };
        // Shorter, longer, another direction: the entry's own command, which
        // is what the DRM node normalises the argument to.
        for cmd in [
            iowr(0xa0, 32),
            iowr(0xa0, 24),
            iowr(0xa0, 40),
            iowr(0xa0, 0),
            iowr(0xa0, 32) & !(1 << 31),
        ] {
            assert_eq!(
                native_cmd(&set, SCLASS_KMS, cmd),
                iowr(0xa0, 32),
                "{cmd:#x}"
            );
        }
        // Another number, class or type; NVKMS never.
        assert_eq!(native_cmd(&set, SCLASS_KMS, iowr(0xa1, 32)), 0);
        assert_eq!(native_cmd(&set, SCLASS_RENDER, iowr(0xa0, 32)), 0);
        assert_eq!(native_cmd(&set, SCLASS_KMS, iowr(0xa0, 32) ^ (1 << 8)), 0);
        assert_eq!(native_cmd(&set, SCLASS_MODESET, NVKMS_IOCTL_IOWR), 0);
    }

    #[test]
    fn a_null_pointer_or_an_empty_list_gets_no_buffer() {
        for (p, n) in [(0u64, 4u32), (LIST, 0)] {
            let (_, _, env) = run_with(arg(p, n, -1, 0), good);
            assert_eq!(&env.sent[0][24..28], &1u32.to_le_bytes(), "{p:#x} {n}");
        }
    }

    #[test]
    fn a_reply_naming_a_descriptor_elsewhere_is_refused_and_closed() {
        fn elsewhere(req: &[u8]) -> Vec<u8> {
            let mut r = good(req);
            let n = r.len();
            r[n - 12..n - 8].copy_from_slice(&24u32.to_le_bytes());
            r
        }
        let (r, _, env) = run_with(arg(LIST, 4, -1, 0), elsewhere);
        assert_eq!(r, -EPROTO);
        assert_eq!(env.closed, [40]);
        // A refusal before the call ran closes what it was to consume.
        fn refused(_: &[u8]) -> Vec<u8> {
            words(&[MSG_IOCTL2, 7, (-EPERM_) as u32, 0])
        }
        let (r, _, env) = run_with(arg(LIST, 4, 3, 0), refused);
        assert_eq!(r, -EPERM_);
        assert_eq!(env.closed, [103]);
    }

    #[test]
    fn copy_extents_follow_the_kernels_fill_rules() {
        let f = |cb_kind, cb_off, cb_arg| SField {
            cb_kind,
            cb_off,
            cb_arg,
            ..SField::default()
        };
        assert_eq!(
            copy_extent(&f(SCB_FULL, 0, 0), SDIR_OUT, 64, 0, 0, 0),
            (0, 64)
        );
        assert_eq!(
            copy_extent(&f(SCB_FULL, 0, 0), SDIR_OUT, 64, -1, 0, 0),
            (0, 0)
        );
        assert_eq!(
            copy_extent(&f(SCB_FULL, 0, 0), SDIR_INOUT, 64, -1, 0, 0),
            (0, 64)
        );
        assert_eq!(
            copy_extent(&f(SCB_PARTIAL, 0, 8), SDIR_OUT, 64, 0, 3, 5),
            (0, 24)
        );
        assert_eq!(
            copy_extent(&f(SCB_PARTIAL, 0, 8), SDIR_OUT, 64, 0, u64::MAX, u64::MAX),
            (0, 64)
        );
        assert_eq!(
            copy_extent(&f(SCB_ALL_OR_NOTHING, 0, 8), SDIR_OUT, 64, 0, 3, 5),
            (0, 0)
        );
        assert_eq!(
            copy_extent(&f(SCB_ALL_OR_NOTHING, 0, 8), SDIR_OUT, 64, 0, 5, 3),
            (0, 24)
        );
        assert_eq!(
            copy_extent(&f(SCB_EXACT, 0, 0), SDIR_OUT, 64, 0, 5, 5),
            (0, 64)
        );
        assert_eq!(
            copy_extent(&f(SCB_EXACT, 0, 0), SDIR_OUT, 64, 0, 5, 4),
            (0, 0)
        );
        assert_eq!(
            copy_extent(&f(SCB_RANGE, 16, 32), SDIR_OUT, 40, -1, 0, 0),
            (16, 40)
        );
        assert_eq!(
            copy_extent(&f(SCB_WRITTEN, 0, 0), SDIR_OUT, 64, -1, 0, 10),
            (0, 10)
        );
    }
}
