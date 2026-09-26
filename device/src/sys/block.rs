// SPDX-License-Identifier: Apache-2.0
//! What the host kernel is handed: parameter blocks built, never patched.
//!
//! Every ioctl the backend makes goes through an [`Arena`]. An arena owns
//! the blocks of one call -- the argument and every buffer a pointer in it
//! addresses -- and is the only thing that writes an address into one:
//!
//! - A block starts as bytes: the guest's (copied once, from the request the
//!   backend already holds) or the backend's own.
//! - Every field the host treats as more than data is *declared* a slot
//!   before the call: a pointer the host follows ([`Arena::ptr`]), a
//!   descriptor it resolves ([`Arena::fd`]), one it creates
//!   ([`Arena::fd_out`]), or a value the backend decides ([`Arena::value`]).
//!   Declaring takes the caller's value out -- it is kept for the reply --
//!   and leaves 0 (-1 for a descriptor out) in the block.
//! - A pointer slot then holds 0 or, through [`Arena::point`] and
//!   [`Arena::point_span`], the address of another block of the same arena
//!   or of memory a [`HostSpan`] keeps mapped. Nothing else puts an address
//!   into a slot: no code outside `sys` takes a pointer's address
//!   (`scripts/check-unsafe.sh` refuses the casts there), data writes
//!   ([`Arena::write`], [`DataMut`]) cannot touch a slot, and a block is
//!   never handed out mutably while it is part of a call.
//! - The reply is a copy ([`Arena::reply`]): the host's bytes, with the
//!   caller's own values back in the slots declared to restore them. The
//!   host's addresses never leave.
//!
//! What this cannot know is which fields of an undeclared layout the host
//! follows: that is the ABI tables' (guestptr.rs, deepseg.rs, schema.rs),
//! measured from the drivers' sources. A pointer field the tables miss
//! reaches the host as the caller's bytes, exactly as it would have before;
//! the arena guarantees that every field they *do* name holds nothing the
//! caller wrote.

use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd, RawFd};

use super::guarded::GuardedBuf;
use super::mem::HostSpan;

/// A refusal: the errno the caller's ioctl returns.
pub type Errno = i32;

/// One block of an arena.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufId(usize);

/// What a declared field is to the host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    /// An address the host dereferences.
    Ptr,
    /// A descriptor the host resolves in this process.
    Fd,
    /// A descriptor the host creates and writes there.
    FdOut,
    /// A value the backend decides: a key, a forced flag, a handle.
    Value,
}

/// Whether the reply gets the caller's value of a slot back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restore {
    /// Always.
    Yes,
    /// Only if the caller's value was not zero: a zero the host overwrote
    /// is the host's answer.
    IfSet,
    /// Never: the host's value (or what the backend writes over it) is the
    /// answer.
    No,
    /// This value, whatever either side had there.
    To(u64),
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    off: usize,
    width: usize,
    kind: SlotKind,
    guest: u64,
    restore: Restore,
    claimed: bool,
    /// Holds an address the arena wrote: the reply never carries it.
    pointed: bool,
}

impl Slot {
    fn overlaps(&self, off: usize, width: usize) -> bool {
        off < self.off + self.width && self.off < off + width
    }
}

enum Mem {
    /// A block of no bytes: its address is 0.
    Empty,
    Guarded(GuardedBuf),
    /// A block the backend wrote itself, of the size the host copies.
    Heap(Box<[u8]>),
}

struct Block {
    mem: Mem,
    len: usize,
    slots: Vec<Slot>,
}

impl Block {
    fn bytes(&self) -> &[u8] {
        match &self.mem {
            Mem::Empty => &[],
            Mem::Guarded(g) => g.as_slice(),
            Mem::Heap(h) => h,
        }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        match &mut self.mem {
            Mem::Empty => &mut [],
            Mem::Guarded(g) => g.as_mut_slice(),
            Mem::Heap(h) => h,
        }
    }

    /// The address the host is given, and how many bytes from it are ours.
    fn region(&mut self) -> (*mut u8, usize) {
        match &mut self.mem {
            Mem::Empty => (std::ptr::null_mut(), 0),
            Mem::Guarded(g) => {
                let reach = g.reach();
                (g.as_mut_ptr(), reach)
            }
            Mem::Heap(h) => (h.as_mut_ptr(), h.len()),
        }
    }

    fn slot(&self, off: usize) -> Option<&Slot> {
        self.slots.iter().find(|s| s.off == off)
    }

    fn put(&mut self, off: usize, width: usize, v: u64) {
        self.bytes_mut()[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }

    fn get(&self, off: usize, width: usize) -> u64 {
        let mut b = [0u8; 8];
        b[..width].copy_from_slice(&self.bytes()[off..off + width]);
        u64::from_le_bytes(b)
    }
}

/// The blocks of one host call.
#[derive(Default)]
pub struct Arena {
    blocks: Vec<Block>,
    /// Memory outside the arena a pointer slot addresses, kept mapped until
    /// the arena goes.
    spans: Vec<HostSpan>,
    /// The last call's result, for claiming descriptors it made.
    last: Option<i32>,
}

// Send by what it holds -- guarded buffers (Send, guarded.rs), boxed
// slices, spans of `Send + Sync` keep-alives -- so an IOCTL2 prepared on the
// queue thread can run on an executor's (xfer.rs).
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Arena>();
};

impl Arena {
    pub fn new() -> Self {
        Self::default()
    }

    fn block_ref(&self, b: BufId) -> Result<&Block, Errno> {
        self.blocks.get(b.0).ok_or(libc::EINVAL)
    }

    fn block_mut(&mut self, b: BufId) -> Result<&mut Block, Errno> {
        self.blocks.get_mut(b.0).ok_or(libc::EINVAL)
    }

    /// A guarded block of `len` bytes holding `init` and zeros after it:
    /// what a guest's bytes become. `len` 0 is a block with no memory,
    /// whose address is 0.
    pub fn block(&mut self, init: &[u8], len: usize) -> Result<BufId, Errno> {
        if init.len() > len {
            return Err(libc::EINVAL);
        }
        let mem = if len == 0 {
            Mem::Empty
        } else {
            let mut g = GuardedBuf::new(len).ok_or(libc::ENOMEM)?;
            g.as_mut_slice()[..init.len()].copy_from_slice(init);
            Mem::Guarded(g)
        };
        self.blocks.push(Block {
            mem,
            len,
            slots: Vec::new(),
        });
        Ok(BufId(self.blocks.len() - 1))
    }

    /// A block of exactly `init`, from the heap: for a struct the backend
    /// writes itself, of the size the host copies, where a guard page per
    /// call would be three system calls for nothing.
    pub fn small(&mut self, init: &[u8]) -> BufId {
        let mem = if init.is_empty() {
            Mem::Empty
        } else {
            Mem::Heap(init.to_vec().into_boxed_slice())
        };
        self.blocks.push(Block {
            mem,
            len: init.len(),
            slots: Vec::new(),
        });
        BufId(self.blocks.len() - 1)
    }

    /// Declare `width` bytes at `off` of `b` a field of kind `kind`: the
    /// caller's value there is taken out and returned, and the block holds
    /// 0 there (-1 for [`SlotKind::FdOut`]) until the backend sets it. A
    /// field outside the block, of another width, or over a field already
    /// declared is refused.
    pub fn slot(
        &mut self,
        b: BufId,
        off: usize,
        width: usize,
        kind: SlotKind,
        restore: Restore,
    ) -> Result<u64, Errno> {
        if !matches!(width, 1 | 2 | 4 | 8) {
            return Err(libc::EINVAL);
        }
        if kind == SlotKind::Ptr && width != 8 {
            return Err(libc::EINVAL);
        }
        let blk = self.block_mut(b)?;
        let end = off.checked_add(width).ok_or(libc::EINVAL)?;
        if end > blk.len || blk.slots.iter().any(|s| s.overlaps(off, width)) {
            return Err(libc::EINVAL);
        }
        let guest = blk.get(off, width);
        let init = if kind == SlotKind::FdOut { u64::MAX } else { 0 };
        blk.put(off, width, init);
        blk.slots.push(Slot {
            off,
            width,
            kind,
            guest,
            restore,
            claimed: false,
            pointed: false,
        });
        Ok(guest)
    }

    /// A pointer the host follows; the reply gets the caller's back.
    pub fn ptr(&mut self, b: BufId, off: usize) -> Result<u64, Errno> {
        self.slot(b, off, 8, SlotKind::Ptr, Restore::IfSet)
    }

    /// A pointer the host only writes (an OUT address): zero on the way in,
    /// and the host's answer, not the caller's value, in the reply.
    pub fn ptr_out(&mut self, b: BufId, off: usize) -> Result<u64, Errno> {
        self.slot(b, off, 8, SlotKind::Ptr, Restore::No)
    }

    /// A descriptor the host resolves; the reply gets the caller's back.
    pub fn fd(&mut self, b: BufId, off: usize, width: usize) -> Result<u64, Errno> {
        self.slot(b, off, width, SlotKind::Fd, Restore::Yes)
    }

    /// A descriptor the host creates: -1 on the way in.
    pub fn fd_out(&mut self, b: BufId, off: usize, width: usize) -> Result<u64, Errno> {
        self.slot(b, off, width, SlotKind::FdOut, Restore::No)
    }

    /// A value the backend decides.
    pub fn value(
        &mut self,
        b: BufId,
        off: usize,
        width: usize,
        restore: Restore,
    ) -> Result<u64, Errno> {
        self.slot(b, off, width, SlotKind::Value, restore)
    }

    fn slot_of(&self, b: BufId, off: usize, kind: SlotKind) -> Result<Slot, Errno> {
        match self.block_ref(b)?.slot(off) {
            Some(s) if s.kind == kind => Ok(*s),
            _ => Err(libc::EINVAL),
        }
    }

    /// Write address `addr` into the pointer slot at `off` of `b`.
    fn aim(&mut self, b: BufId, off: usize, addr: u64) -> Result<(), Errno> {
        let blk = self.block_mut(b)?;
        blk.put(off, 8, addr);
        if let Some(s) = blk.slots.iter_mut().find(|s| s.off == off) {
            s.pointed = addr != 0;
        }
        Ok(())
    }

    /// Point the pointer slot at `off` of `b` at block `to` of this arena
    /// (another block: never itself). A block of no bytes is address 0.
    pub fn point(&mut self, b: BufId, off: usize, to: BufId) -> Result<(), Errno> {
        self.slot_of(b, off, SlotKind::Ptr)?;
        if to == b {
            return Err(libc::EINVAL);
        }
        let addr = self.block_mut(to)?.region().0 as u64;
        self.aim(b, off, addr)
    }

    /// Point the pointer slot at `off` of `b` `at` bytes into `span`, which
    /// the arena keeps mapped until it goes.
    pub fn point_span(
        &mut self,
        b: BufId,
        off: usize,
        span: &HostSpan,
        at: u64,
    ) -> Result<(), Errno> {
        self.slot_of(b, off, SlotKind::Ptr)?;
        let addr = span.addr_at(at).ok_or(libc::EINVAL)?;
        self.spans.push(span.clone());
        self.aim(b, off, addr)
    }

    /// Put descriptor `fd` in the descriptor slot at `off` of `b`. The
    /// caller keeps `fd` open until the call returns.
    pub fn set_fd(&mut self, b: BufId, off: usize, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        let s = self.slot_of(b, off, SlotKind::Fd)?;
        let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
        self.block_mut(b)?.put(off, s.width, raw as i64 as u64);
        Ok(())
    }

    /// Put the "no descriptor" value `none` (negative) in the descriptor
    /// slot at `off` of `b`.
    pub fn set_no_fd(&mut self, b: BufId, off: usize, none: i64) -> Result<(), Errno> {
        let s = self.slot_of(b, off, SlotKind::Fd)?;
        if none >= 0 {
            return Err(libc::EINVAL);
        }
        self.block_mut(b)?.put(off, s.width, none as u64);
        Ok(())
    }

    /// Put `v` in the value slot at `off` of `b`.
    pub fn set_value(&mut self, b: BufId, off: usize, v: u64) -> Result<(), Errno> {
        let s = self.slot_of(b, off, SlotKind::Value)?;
        self.block_mut(b)?.put(off, s.width, v);
        Ok(())
    }

    /// Write data into `b`: refused where it would touch a declared field.
    pub fn write(&mut self, b: BufId, off: usize, bytes: &[u8]) -> Result<(), Errno> {
        let blk = self.block_mut(b)?;
        let end = off.checked_add(bytes.len()).ok_or(libc::EINVAL)?;
        if end > blk.len || blk.slots.iter().any(|s| s.overlaps(off, bytes.len())) {
            return Err(libc::EINVAL);
        }
        blk.bytes_mut()[off..end].copy_from_slice(bytes);
        Ok(())
    }

    /// Write `bytes` at `off` of `b` whatever is declared there: what a
    /// test's host writes into a call's block after the call.
    #[cfg(test)]
    pub fn host_writes(&mut self, b: BufId, off: usize, bytes: &[u8]) {
        let blk = self.block_mut(b).expect("a block");
        blk.bytes_mut()[off..off + bytes.len()].copy_from_slice(bytes);
    }

    /// `b`'s bytes for policy code that rewrites data in place (NVKMS's
    /// and the fence hooks'): every declared field is put back as it was
    /// when the view ends, whatever was written over it.
    pub fn data_mut(&mut self, b: BufId) -> Option<DataMut<'_>> {
        let blk = self.blocks.get_mut(b.0)?;
        let saved = blk
            .slots
            .iter()
            .map(|s| (s.off, s.width, blk.get(s.off, s.width)))
            .collect();
        Some(DataMut { blk, saved })
    }

    /// The bytes of `b` as they stand: what the host will read, or what it
    /// left.
    pub fn bytes(&self, b: BufId) -> &[u8] {
        self.block_ref(b).map_or(&[], |blk| blk.bytes())
    }

    /// The length of `b`.
    pub fn len(&self, b: BufId) -> usize {
        self.block_ref(b).map_or(0, |blk| blk.len)
    }

    /// Whether the arena holds no blocks.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The caller's value of the slot at `off` of `b`.
    pub fn guest(&self, b: BufId, off: usize) -> Option<u64> {
        self.block_ref(b).ok()?.slot(off).map(|s| s.guest)
    }

    /// Whether the slot at `off` of `b` holds an address, not 0.
    pub fn is_pointed(&self, b: BufId, off: usize) -> bool {
        self.slot_of(b, off, SlotKind::Ptr)
            .ok()
            .is_some_and(|s| self.block_ref(b).map_or(0, |blk| blk.get(off, s.width)) != 0)
    }

    /// The reply's copy of `b`: the bytes as the host left them, with the
    /// caller's values back in the slots declared to restore them -- and in
    /// every slot the arena aimed at memory of its own, whatever the slot
    /// was declared to do: no address of this process leaves in a reply.
    pub fn reply(&self, b: BufId) -> Vec<u8> {
        let Ok(blk) = self.block_ref(b) else {
            return Vec::new();
        };
        let mut out = blk.bytes().to_vec();
        for s in &blk.slots {
            let back = match s.restore {
                _ if s.pointed => Some(s.guest),
                Restore::Yes => Some(s.guest),
                Restore::IfSet => (s.guest != 0).then_some(s.guest),
                Restore::No => None,
                Restore::To(v) => Some(v),
            };
            if let Some(v) = back {
                out[s.off..s.off + s.width].copy_from_slice(&v.to_le_bytes()[..s.width]);
            }
        }
        out
    }

    /// The descriptor the host made in the slot at `off` of `b`, once, after
    /// a call that succeeded: the slot held -1 going in, so anything else is
    /// what the host wrote.
    pub fn claim_fd(&mut self, b: BufId, off: usize) -> Option<OwnedFd> {
        if !self.last.is_some_and(|r| r >= 0) {
            return None;
        }
        let blk = self.block_mut(b).ok()?;
        let i = blk
            .slots
            .iter()
            .position(|s| s.off == off && s.kind == SlotKind::FdOut && !s.claimed)?;
        let width = blk.slots[i].width;
        let raw = match width {
            4 => blk.get(off, 4) as u32 as i32,
            8 => blk.get(off, 8) as i64 as i32,
            _ => return None,
        };
        if raw < 0 {
            return None;
        }
        blk.slots[i].claimed = true;
        // SAFETY: the slot held -1 when the call began, the call succeeded,
        // and the host wrote a descriptor number there: one it just installed
        // in this process's table for this caller and that nothing else in
        // this process knows of. `claimed` makes this the only owner.
        Some(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    /// Hand block `top` to `kernel` as the argument of ioctl `request` on
    /// `fd`: the kernel's result, never negative except as -errno. Refused
    /// (-EINVAL, the kernel not asked) when the block holds fewer bytes
    /// than the host copies for the request (`_IOC_SIZE`).
    pub fn call(&mut self, kernel: &dyn Kernel, fd: RawFd, request: u64, top: BufId) -> i32 {
        let size = crate::hostfd::ioc_size(request as u32);
        let regions: Vec<(u64, usize)> = self
            .blocks
            .iter_mut()
            .map(|b| {
                let (p, n) = b.region();
                (p as u64, n)
            })
            .chain(self.spans.iter().map(|s| (s.addr(), s.len())))
            .filter(|&(p, _)| p != 0)
            .collect();
        let Ok(blk) = self.block_mut(top) else {
            return -libc::EINVAL;
        };
        let len = blk.len.max(size);
        let (ptr, reach) = blk.region();
        if size > reach || (ptr.is_null() && size > 0) {
            return -libc::EINVAL;
        }
        let mut arg = Arg {
            ptr,
            len: len.min(reach),
            regions: &regions,
        };
        let r = kernel.ioctl(fd, request, &mut arg);
        self.last = Some(r);
        r
    }
}

/// A block's bytes, mutable, for data only (`Arena::data_mut`).
pub struct DataMut<'a> {
    blk: &'a mut Block,
    saved: Vec<(usize, usize, u64)>,
}

impl std::ops::Deref for DataMut<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.blk.bytes()
    }
}

impl std::ops::DerefMut for DataMut<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.blk.bytes_mut()
    }
}

impl Drop for DataMut<'_> {
    fn drop(&mut self) {
        for &(off, width, v) in &self.saved {
            if self.blk.get(off, width) != v {
                log::error!(
                    "a policy rewrite touched the declared field at {off}; put back as it was"
                );
                self.blk.put(off, width, v);
            }
        }
    }
}

/// The argument of one ioctl, as the kernel sees it: made only by
/// [`Arena::call`], so every declared pointer in it addresses the arena.
pub struct Arg<'a> {
    ptr: *mut u8,
    len: usize,
    /// Every range of memory the call's pointers may address.
    #[cfg_attr(not(any(test, fuzzing)), allow(dead_code))]
    regions: &'a [(u64, usize)],
}

impl Arg<'_> {
    /// The raw argument, for the real kernel.
    pub(super) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    /// The argument's bytes: at least `_IOC_SIZE` of the request (the whole
    /// block for a request with no size, such as UVM's).
    pub fn bytes(&mut self) -> &mut [u8] {
        if self.ptr.is_null() {
            return &mut [];
        }
        // SAFETY: `ptr` is the start of an arena block with at least `len`
        // bytes mapped read-write (`Arena::call` checks), owned by the arena
        // that is exclusively borrowed for the call; `&mut self` makes this
        // the only borrow of them.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

/// For the fake kernels of tests and fuzzing: follow a pointer as the host
/// would, into the memory the call's arena owns -- and nowhere else.
#[cfg(any(test, fuzzing))]
impl Arg<'_> {
    /// The argument's address, as the kernel is given it.
    pub fn addr(&self) -> u64 {
        self.ptr as u64
    }

    /// Whether `len` bytes from `p` all lie in one region the call may reach
    /// (a block with its slack, or a span): what the host could copy. A
    /// start outside every region is `None` -- no memory the backend built
    /// for this call is there.
    pub fn reach(&self, p: u64, len: u64) -> Option<bool> {
        let &(base, n) = self
            .regions
            .iter()
            .find(|&&(b, n)| p >= b && p < b + n as u64)?;
        Some(p.checked_add(len).is_some_and(|e| e <= base + n as u64))
    }

    /// `len` bytes from `p`, copied; `None` unless they lie in one region.
    pub fn read(&self, p: u64, len: usize) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        if self.reach(p, len as u64) != Some(true) {
            return None;
        }
        let mut out = vec![0u8; len];
        // SAFETY: `reach` found the range inside one region of this call's
        // arena, mapped for the call's length, and no Rust reference to it
        // is live (the arena is exclusively borrowed by the call, and the
        // copy goes through raw pointers).
        unsafe { std::ptr::copy_nonoverlapping(p as *const u8, out.as_mut_ptr(), len) };
        Some(out)
    }

    /// Write `bytes` at `p`; `false` (nothing written) unless they lie in
    /// one region.
    pub fn write(&mut self, p: u64, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.reach(p, bytes.len() as u64) != Some(true) {
            return false;
        }
        // SAFETY: as `read`, and `&mut self` excludes a live `bytes()`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        true
    }

    /// The little-endian word of `width` bytes at `p` (panics outside the
    /// call's memory: a test's fake following a pointer the backend did not
    /// build is the finding).
    pub fn peek(&self, p: u64, width: usize) -> u64 {
        let b = self
            .read(p, width)
            .unwrap_or_else(|| panic!("the host was handed {p:#x}, which is no block of the call"));
        let mut w = [0u8; 8];
        w[..width].copy_from_slice(&b);
        u64::from_le_bytes(w)
    }

    /// Write the little-endian word `v` of `width` bytes at `p` (panics as
    /// `peek`).
    pub fn poke(&mut self, p: u64, width: usize, v: u64) {
        assert!(
            self.write(p, &v.to_le_bytes()[..width]),
            "the host was handed {p:#x}, which is no block of the call"
        );
    }

    /// The argument's bytes, and every other region of the call to follow
    /// pointers into at the same time.
    pub fn split(&mut self) -> (&mut [u8], Others<'_>) {
        let top = self.ptr as u64;
        let others = Others {
            regions: self
                .regions
                .iter()
                .copied()
                .filter(|&(b, _)| b != top)
                .collect(),
            _call: std::marker::PhantomData,
        };
        (self.bytes(), others)
    }
}

/// The regions of a call other than its argument (`Arg::split`).
#[cfg(any(test, fuzzing))]
pub struct Others<'a> {
    regions: Vec<(u64, usize)>,
    _call: std::marker::PhantomData<&'a mut ()>,
}

#[cfg(any(test, fuzzing))]
impl Others<'_> {
    /// As [`Arg::reach`], outside the argument.
    pub fn reach(&self, p: u64, len: u64) -> Option<bool> {
        let &(base, n) = self
            .regions
            .iter()
            .find(|&&(b, n)| p >= b && p < b + n as u64)?;
        Some(p.checked_add(len).is_some_and(|e| e <= base + n as u64))
    }

    /// As [`Arg::read`], outside the argument.
    pub fn read(&self, p: u64, len: usize) -> Option<Vec<u8>> {
        if len == 0 {
            return Some(Vec::new());
        }
        if self.reach(p, len as u64) != Some(true) {
            return None;
        }
        let mut out = vec![0u8; len];
        // SAFETY: inside one region of the call other than the argument
        // (whose slice the caller may hold), mapped for the call; a raw copy.
        unsafe { std::ptr::copy_nonoverlapping(p as *const u8, out.as_mut_ptr(), len) };
        Some(out)
    }

    /// As [`Arg::write`], outside the argument.
    pub fn write(&mut self, p: u64, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.reach(p, bytes.len() as u64) != Some(true) {
            return false;
        }
        // SAFETY: as `read`.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len()) };
        true
    }

    /// As [`Arg::peek`], outside the argument.
    pub fn peek(&self, p: u64, width: usize) -> u64 {
        let b = self
            .read(p, width)
            .unwrap_or_else(|| panic!("the host was handed {p:#x}, which is no block of the call"));
        let mut w = [0u8; 8];
        w[..width].copy_from_slice(&b);
        u64::from_le_bytes(w)
    }

    /// As [`Arg::poke`], outside the argument.
    pub fn poke(&mut self, p: u64, width: usize, v: u64) {
        assert!(
            self.write(p, &v.to_le_bytes()[..width]),
            "the host was handed {p:#x}, which is no block of the call"
        );
    }
}

/// A host kernel: the real one ([`super::ioctl::Host`]) or a test's fake.
pub trait Kernel: Send + Sync {
    /// `ioctl(fd, request, arg)`: its non-negative result, or -errno.
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut Arg<'_>) -> i32;
}

/// A fake kernel as a plain function, for tests.
pub struct FnKernel(pub fn(RawFd, u64, &mut Arg<'_>) -> i32);

impl Kernel for FnKernel {
    fn ioctl(&self, fd: RawFd, request: u64, arg: &mut Arg<'_>) -> i32 {
        (self.0)(fd, request, arg)
    }
}

/// One flat call: `bytes` as the whole argument, no pointer in it, copied
/// back after. For the small structs the backend writes itself.
pub fn flat(kernel: &dyn Kernel, fd: RawFd, request: u64, bytes: &mut [u8]) -> i32 {
    let mut a = Arena::new();
    let top = a.small(bytes);
    let r = a.call(kernel, fd, request, top);
    bytes.copy_from_slice(a.bytes(top));
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostfd::{IOC_RW, ioc};

    const REQ: u64 = ioc(IOC_RW, b'F', 0x2a, 32) as u64;

    fn guest_block() -> Vec<u8> {
        let mut b = vec![0x11u8; 32];
        b[16..24].copy_from_slice(&0x7fff_dead_b000u64.to_le_bytes());
        b
    }

    #[test]
    fn a_declared_pointer_never_holds_the_callers_value() {
        let mut a = Arena::new();
        let top = a.block(&guest_block(), 32).unwrap();
        assert_eq!(a.ptr(top, 16).unwrap(), 0x7fff_dead_b000);
        assert_eq!(&a.bytes(top)[16..24], &[0; 8]);
        let nested = a.block(&[1, 2, 3, 4], 4).unwrap();
        a.point(top, 16, nested).unwrap();
        let p = u64::from_le_bytes(a.bytes(top)[16..24].try_into().unwrap());
        assert_ne!(p, 0x7fff_dead_b000);
        assert_ne!(p, 0);
        // The reply carries the caller's value, never ours.
        assert_eq!(a.reply(top), guest_block());
    }

    #[test]
    fn data_writes_cannot_touch_a_declared_field() {
        let mut a = Arena::new();
        let top = a.block(&guest_block(), 32).unwrap();
        a.ptr(top, 16).unwrap();
        assert_eq!(a.write(top, 12, &[0; 8]), Err(libc::EINVAL));
        assert_eq!(a.write(top, 23, &[0]), Err(libc::EINVAL));
        assert_eq!(a.write(top, 24, &[0; 8]), Ok(()));
        assert_eq!(
            a.write(top, 28, &[0; 8]),
            Err(libc::EINVAL),
            "past the block"
        );
        // Nor can a slot be declared twice, or over another.
        assert!(a.ptr(top, 16).is_err());
        assert!(a.value(top, 20, 4, Restore::No).is_err());
        // Nor may a value be set through the wrong kind of slot.
        assert!(a.set_value(top, 16, 5).is_err());
        let other = a.block(&[0; 8], 8).unwrap();
        assert!(a.point(top, 24, other).is_err(), "not a pointer slot");
        assert!(a.point(top, 16, top).is_err(), "not itself");
    }

    #[test]
    fn a_block_shorter_than_the_host_copies_is_never_handed_over() {
        fn never(_: RawFd, _: u64, _: &mut Arg<'_>) -> i32 {
            panic!("called")
        }
        let mut a = Arena::new();
        let top = a.small(&[0; 16]);
        assert_eq!(a.call(&FnKernel(never), -1, REQ, top), -libc::EINVAL);
        // A guarded block of 16 has its slack: the host may copy 32.
        let g = a.block(&[0; 16], 16).unwrap();
        fn ok(_: RawFd, _: u64, arg: &mut Arg<'_>) -> i32 {
            assert!(arg.bytes().len() >= 32);
            0
        }
        assert_eq!(a.call(&FnKernel(ok), -1, REQ, g), 0);
    }

    #[test]
    fn a_fake_host_reaches_the_calls_blocks_and_nothing_else() {
        fn host(_: RawFd, _: u64, arg: &mut Arg<'_>) -> i32 {
            let p = u64::from_le_bytes(arg.bytes()[16..24].try_into().unwrap());
            assert_eq!(arg.peek(p, 4), 0x0403_0201);
            arg.poke(p, 4, 0xaabb_ccdd);
            let outside = Box::new([0u8; 64]);
            let q = &*outside as *const [u8; 64] as u64;
            assert_eq!(arg.reach(q, 1), None, "the heap is no block of the call");
            0
        }
        let mut a = Arena::new();
        let top = a.block(&guest_block(), 32).unwrap();
        a.ptr(top, 16).unwrap();
        let nested = a.block(&[1, 2, 3, 4], 4).unwrap();
        a.point(top, 16, nested).unwrap();
        assert_eq!(a.call(&FnKernel(host), -1, REQ, top), 0);
        assert_eq!(a.bytes(nested), &0xaabb_ccddu32.to_le_bytes());
    }

    #[test]
    fn only_a_descriptor_the_host_wrote_is_claimed() {
        fn host(_: RawFd, _: u64, arg: &mut Arg<'_>) -> i32 {
            assert_eq!(&arg.bytes()[4..8], &[0xff; 4], "starts at -1");
            let fd = crate::sys::fd::eventfd(0).unwrap();
            let raw = std::os::fd::IntoRawFd::into_raw_fd(fd);
            arg.bytes()[4..8].copy_from_slice(&raw.to_le_bytes());
            0
        }
        let mut a = Arena::new();
        let top = a.small(&[0; 8]);
        a.fd_out(top, 4, 4).unwrap();
        assert!(a.claim_fd(top, 4).is_none(), "no call yet");
        assert_eq!(a.call(&FnKernel(host), -1, 0, top), 0);
        let fd = a.claim_fd(top, 4).expect("the host's descriptor");
        assert!(a.claim_fd(top, 4).is_none(), "claimed once");
        drop(fd);
    }
}
