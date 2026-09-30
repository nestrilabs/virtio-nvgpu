// SPDX-License-Identifier: Apache-2.0
//! `wl_shm`, stage 1: the server's side owns a memfd per pool, and the
//! client's side copies into it at every commit that shows a buffer.
//!
//! A client's shared memory is guest RAM; the host compositor cannot map it.
//! So the side facing the server creates a memfd the size of the pool and
//! hands that over in the client's pool's place, and the side facing the
//! client, when a surface commits with an shm buffer, sends the bytes of that
//! buffer that may have changed (`SHM_SYNC`) ahead of the commit. The commit
//! is the right moment: a client may not touch a committed buffer until the
//! compositor releases it, so what is copied then is exactly what the
//! compositor may read; `wl_buffer.release` is forwarded untouched, so the
//! client's pacing is the compositor's.
//!
//! "May have changed" is tracked per buffer, in rows: everything the surface
//! was damaged by since this buffer was last copied (a client repainting a
//! buffer that is two frames old repaints two frames' damage, but reports only
//! one). Buffer-space damage is used as rows; surface-space damage, which
//! would need scale, transform and viewport to map back, counts as the whole
//! buffer. A buffer seen for the first time is copied whole, and so is one
//! its surface has shown [`SURFACE_BUFFERS`] others since: damage is
//! collected for that many per surface, so that a commit costs as many
//! steps, not one per buffer the client ever made.
//!
//! The client's descriptor is only ever read with `pread`, never mapped: a
//! client that truncates its pool under us gets short copies, not a SIGBUS in
//! the proxy, and its connection goes on: what a copy was counted at on the
//! channel's backlog is given back however short it came (`job.rs`).
//!
//! **What the server's side may hold.** Its memfds are the one place a peer
//! decides how much memory the proxy commits: every `SHM_SYNC` is written into
//! them at once, no commit or compositor consent needed, and the pages are
//! shmem that no process's RSS shows, so the host OOM killer, when they have
//! eaten the host, picks somebody else. So what is charged to [`ShmBudget`]s is
//! what a memfd can come to hold, not how large it says it is. A pool's memfd
//! is made at the pool's size, sparse, and costs nothing until written, and
//! `SHM_SYNC` only ever writes inside a live buffer. A buffer is charged when
//! it is made, for the pages it touches that no other live buffer of its pool
//! already does (clients double-buffer in one pool, and overlap), and refused
//! if that would pass a budget. When the last buffer over a page goes -- a
//! buffer destroyed while its surface shows it goes once the surface shows
//! something else ([`Shm::forget`]) -- the page is punched out of the memfd
//! (`FALLOC_FL_PUNCH_HOLE`) and its charge
//! given back; the compositor, which maps the whole pool, reads zeros there,
//! as it would from a client that punched its own pool. That is how foot runs:
//! it makes a 512 MiB pool per window and scrolls by sliding one buffer
//! through it and punching behind, so it holds what its buffers take.
//!
//! The budgets: one per connection ([`MAX_POOL_BYTES`], [`MAX_POOLS`]), and on
//! the backend one per VM that every connection of the VM shares
//! ([`Engine::set_shm_budget`](crate::engine::Engine::set_shm_budget)), since a
//! guest can open as many connections as it is allowed channels. A pool counts
//! against the pool count from before its memfd exists until the last
//! reference to it (its own id, or a buffer made from it) is gone; its size is
//! held only to what the protocol can say, an `int32`.
//!
//! **The client's side** charges its buffers the same way, to the same
//! per-connection limit, though its pools are the client's own memory and
//! nothing is punched there: what its buffers cover is what a commit copies,
//! and a buffer the server's side would refuse is refused here first, with
//! the client told why, rather than read and carried across only to be
//! refused there. The copy itself is made lazily ([`SyncJob`]): a commit
//! queues where to read, and the bytes are read a record at a time as the
//! channel takes them, so what a commit costs this side is one record, not
//! the buffer.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::budget::Budgets;
use crate::frame::{MAX_REC_PAYLOAD, REC_SHM_SYNC, Unit, record_with};
use crate::job::Job;
use crate::sys;

/// The largest pool the protocol can make or resize to: its size is an
/// `int32`.
pub const MAX_POOL_SIZE: u64 = i32::MAX as u64;

pub struct Pool {
    /// Client side: the client's own pool descriptor. Server side: our memfd.
    pub fd: OwnedFd,
    pub size: AtomicU64,
    /// What this pool holds of its budgets, given back when it goes.
    charge: Charge,
    /// The pages of the pool some live buffer covers, which are what the
    /// pool is charged for.
    held: Mutex<Extents>,
    /// Server side: pages no live buffer covers any more are punched out of
    /// our memfd. The client's pool is the client's and is never written.
    punch: bool,
}

impl Pool {
    /// Pages `[a, b)` of the pool that `[off, off + len)` touches.
    fn pages(off: u64, len: u64) -> (u64, u64) {
        let pg = sys::page_size();
        (off / pg, (off + len).div_ceil(pg))
    }

    /// A buffer over `[off, off + len)` is made: charge the pages of it no
    /// live buffer covers yet, or refuse it and take nothing.
    fn hold(&self, off: u64, len: u64) -> Result<(), ShmError> {
        let (a, b) = Self::pages(off, len);
        let mut ext = self.held.lock().unwrap_or_else(|e| e.into_inner());
        if !self.charge.grow(ext.uncovered(a, b) * sys::page_size()) {
            return Err(ShmError::TooBig);
        }
        ext.add(a, b);
        Ok(())
    }

    /// A buffer over `[off, off + len)` is gone: punch out the pages no live
    /// buffer covers any more (server side), and give back their charge.
    /// Pages the kernel would not punch stay charged, until the pool goes.
    fn release(&self, off: u64, len: u64) {
        let (a, b) = Self::pages(off, len);
        let pg = sys::page_size();
        let freed = self
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(a, b);
        for (x, y) in freed {
            let (at, n) = (x * pg, (y - x) * pg);
            if !self.punch || sys::punch_hole(self.fd.as_raw_fd(), at, n).is_ok() {
                self.charge.shrink(n);
            }
        }
    }
}

/// How many live buffers cover each page of a pool, as steps: a key is the
/// first page of a run that many buffers cover, up to the next key; before
/// the first key and from the last one on, none do. There are only as many
/// steps as buffer edges, so a pool costs in proportion to its buffers.
#[derive(Default)]
struct Extents {
    steps: BTreeMap<u64, u32>,
}

impl Extents {
    fn at(&self, p: u64) -> u32 {
        self.steps.range(..=p).next_back().map_or(0, |(_, &c)| c)
    }

    /// Pages of `[a, b)` no buffer covers.
    fn uncovered(&self, a: u64, b: u64) -> u64 {
        if a >= b {
            return 0;
        }
        let (mut from, mut c, mut n) = (a, self.at(a), 0);
        for (&k, &v) in self.steps.range(a + 1..b) {
            if c == 0 {
                n += k - from;
            }
            (from, c) = (k, v);
        }
        if c == 0 {
            n += b - from;
        }
        n
    }

    /// A step at `p`, if there is none, of the count already there.
    fn split(&mut self, p: u64) {
        let c = self.at(p);
        self.steps.entry(p).or_insert(c);
    }

    /// The step at `p` goes if it changes nothing.
    fn tidy(&mut self, p: u64) {
        if let Some(&c) = self.steps.get(&p) {
            let before = p.checked_sub(1).map_or(0, |q| self.at(q));
            if before == c {
                self.steps.remove(&p);
            }
        }
    }

    fn add(&mut self, a: u64, b: u64) {
        if a >= b {
            return;
        }
        self.split(a);
        self.split(b);
        for (_, c) in self.steps.range_mut(a..b) {
            *c += 1;
        }
        self.tidy(a);
        self.tidy(b);
    }

    /// Take away one buffer over `[a, b)`, added before: the runs of pages it
    /// was the last to cover.
    fn remove(&mut self, a: u64, b: u64) -> Vec<(u64, u64)> {
        if a >= b {
            return Vec::new();
        }
        self.split(a);
        self.split(b);
        let mut freed: Vec<(u64, u64)> = Vec::new();
        let mut run: Option<u64> = None;
        for (&k, c) in self.steps.range_mut(a..=b) {
            if let Some(s) = run.take() {
                match freed.last_mut() {
                    Some(last) if last.1 == s => last.1 = k,
                    _ => freed.push((s, k)),
                }
            }
            if k < b {
                *c = c.saturating_sub(1);
                if *c == 0 {
                    run = Some(k);
                }
            }
        }
        self.tidy(a);
        self.tidy(b);
        freed
    }
}

/// A limit on pool memory and pool count, shared by whoever holds an `Arc` of
/// it: every pool charged to it draws from the same two counters, lock-free,
/// from whichever connection's thread.
#[derive(Debug)]
pub struct ShmBudget {
    max_bytes: u64,
    max_pools: u64,
    bytes: AtomicU64,
    pools: AtomicU64,
}

impl ShmBudget {
    pub fn new(max_bytes: u64, max_pools: u64) -> Self {
        Self {
            max_bytes,
            max_pools,
            bytes: AtomicU64::new(0),
            pools: AtomicU64::new(0),
        }
    }

    /// (bytes, pools) charged now.
    pub fn used(&self) -> (u64, u64) {
        (
            self.bytes.load(Ordering::Relaxed),
            self.pools.load(Ordering::Relaxed),
        )
    }

    /// (bytes, pools) it allows.
    pub fn limits(&self) -> (u64, u64) {
        (self.max_bytes, self.max_pools)
    }

    /// Both or neither: a charge that would pass either limit takes nothing.
    pub(crate) fn take(&self, bytes: u64, pools: u64) -> bool {
        let add = |c: &AtomicU64, n: u64, max: u64| {
            c.fetch_update(Ordering::AcqRel, Ordering::Acquire, |u| {
                u.checked_add(n).filter(|&t| t <= max)
            })
            .is_ok()
        };
        if !add(&self.bytes, bytes, self.max_bytes) {
            return false;
        }
        if !add(&self.pools, pools, self.max_pools) {
            self.bytes.fetch_sub(bytes, Ordering::AcqRel);
            return false;
        }
        true
    }

    pub(crate) fn give(&self, bytes: u64, pools: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.pools.fetch_sub(pools, Ordering::AcqRel);
    }
}

/// Something shm pools and their memory are charged to besides the
/// connection's own limits: a [`ShmBudget`], or an owner's share of one
/// (the backend's per-guest-process share of the VM's, which keeps a last
/// part for processes that hold little). Both of a charge or neither.
pub trait ShmCharge: Send + Sync {
    /// `bytes` and `pools` more, or nothing.
    fn take(&self, bytes: u64, pools: u64) -> bool;
    /// `bytes` and `pools`, taken before, back.
    fn give(&self, bytes: u64, pools: u64);
}

impl ShmCharge for ShmBudget {
    fn take(&self, bytes: u64, pools: u64) -> bool {
        ShmBudget::take(self, bytes, pools)
    }
    fn give(&self, bytes: u64, pools: u64) {
        ShmBudget::give(self, bytes, pools)
    }
}

/// One pool's share of its budgets: one pool of the count, and the bytes its
/// live buffers cover. Made before the pool (so a refused pool never has a
/// memfd), owned by it after, and given back by `Drop` -- once, whichever way
/// the pool goes.
pub struct Charge {
    budgets: Budgets<dyn ShmCharge>,
    bytes: AtomicU64,
}

impl Charge {
    /// Take one pool from every budget, or from none.
    fn take(budgets: Budgets<dyn ShmCharge>) -> Result<Charge, ShmError> {
        if !budgets.take(0, 1) {
            return Err(ShmError::TooMany);
        }
        Ok(Charge {
            budgets,
            bytes: AtomicU64::new(0),
        })
    }

    /// `more` bytes on top, from every budget or from none.
    fn grow(&self, more: u64) -> bool {
        if more == 0 {
            return true;
        }
        if !self.budgets.take(more, 0) {
            return false;
        }
        self.bytes.fetch_add(more, Ordering::AcqRel);
        true
    }

    /// `less` bytes, taken before, back to every budget.
    fn shrink(&self, less: u64) {
        self.budgets.give(less, 0);
        self.bytes.fetch_sub(less, Ordering::AcqRel);
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        self.budgets.give(self.bytes.load(Ordering::Acquire), 1);
    }
}

pub struct Buffer {
    pool: Arc<Pool>,
    offset: u64,
    stride: u64,
    height: u64,
    synced: bool,
    /// Rows [a, b) that may differ from what was last copied.
    dirty: Option<(u64, u64)>,
    /// The surfaces that name this buffer: as the one they show
    /// (`Surface::current`), or among those their damage is collected for
    /// (`Surface::buffers`). What [`Shm::forget`] visits, rather than every
    /// surface of the connection.
    surfaces: HashSet<u32>,
}

impl Buffer {
    fn len(&self) -> u64 {
        self.stride * self.height
    }
}

/// However the buffer goes (destroyed, its id reused, the connection over),
/// the pages only it covered leave the memfd and the budgets.
impl Drop for Buffer {
    fn drop(&mut self) {
        self.pool.release(self.offset, self.len());
    }
}

#[derive(Default)]
struct Surface {
    /// `Some(x)`: an attach is pending, of buffer `x` (0 = null).
    pending: Option<u32>,
    current: u32,
    damage_full: bool,
    damage_rows: Option<(u64, u64)>,
    /// Client side: the buffers this surface has shown, most recent last,
    /// at most [`Shm::surface_buffers`]. A commit's damage is added to each, since
    /// a client repaints a buffer by what changed since it last showed it;
    /// the oldest past the limit is let go, and copied whole when next shown.
    buffers: VecDeque<u32>,
    /// The buffer this surface last committed, destroyed since: what the
    /// compositor still shows, so its pages stay (see [`Shm::forget`]).
    retired: Option<Buffer>,
}

/// Bytes one connection's live buffers may cover, in the server's side's
/// memfds, at once: what those memfds can come to hold, whatever their pools'
/// sizes. A 4K RGBA buffer is 33 MB, so a triple-buffered 4K window takes
/// 100 MB and this is five of them; a peer asking for more is after host
/// memory. The VM-wide budget (`--wayland-shm-budget` on the backend) is what
/// bounds the sum.
pub const MAX_POOL_BYTES: u64 = 512 << 20;

/// Buffers a surface collects damage for (client side), by default: what a
/// client cycles through, two or three and a few more for some toolkits,
/// and not what a client could make it, since every commit visits each of
/// them: unbounded, a client that shows one surface 20,000 buffers in turn
/// makes each of its commits cost the guest daemon a millisecond, on the
/// one thread every other application's messages wait on. A client that
/// cycles through more gets each of its buffers copied whole when next
/// shown, not only its damage. The guest daemon's `--surface-buffers`
/// sets another, up to [`MAX_SURFACE_BUFFERS`].
pub const SURFACE_BUFFERS: usize = 16;

/// The most buffers a surface may be set to collect damage for
/// ([`Shm::set_surface_buffers`]): each is visited at every commit, so this
/// bounds what one commit costs the daemon (64 measured about 3 µs more
/// than 16 a commit; 256, four times that).
pub const MAX_SURFACE_BUFFERS: usize = 256;

/// Pools one connection may hold at once, on either side: each is a
/// descriptor here (our memfd, or the client's own), and a toolkit makes a
/// handful -- one per buffer at most, cursors included.
pub const MAX_POOLS: u64 = 256;

pub struct Shm {
    pools: HashMap<u32, Arc<Pool>>,
    buffers: HashMap<u32, Buffer>,
    surfaces: HashMap<u32, Surface>,
    /// This connection's own limits.
    conn: Arc<ShmBudget>,
    /// Budgets shared with other connections: the VM's, and the guest
    /// process's the connection is for, if the owner of the engine set them.
    shared: Budgets<dyn ShmCharge>,
    /// Buffers a surface collects damage for ([`SURFACE_BUFFERS`] unless
    /// set).
    surface_buffers: usize,
    pub sync_bytes: u64,
    pub syncs: u64,
}

impl Default for Shm {
    fn default() -> Self {
        Self {
            pools: HashMap::new(),
            buffers: HashMap::new(),
            surfaces: HashMap::new(),
            conn: Arc::new(ShmBudget::new(MAX_POOL_BYTES, MAX_POOLS)),
            shared: Budgets::default(),
            surface_buffers: SURFACE_BUFFERS,
            sync_bytes: 0,
            syncs: 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ShmError {
    NoPool(u32),
    NoBuffer(u32),
    OutOfRange(u32),
    /// Past the pool count of this connection or the VM.
    TooMany,
    /// Past the bytes of this connection or the VM, or the protocol's size.
    TooBig,
    Io,
}

fn union(a: Option<(u64, u64)>, b: (u64, u64)) -> Option<(u64, u64)> {
    if b.0 >= b.1 {
        return a;
    }
    Some(match a {
        None => b,
        Some((x, y)) => (x.min(b.0), y.max(b.1)),
    })
}

impl Shm {
    /// Drop whatever is known about object `id` (it was destroyed, or its id
    /// is being reused).
    ///
    /// A buffer destroyed while it is what a surface last committed is not
    /// let go yet: `wl_buffer.destroy` leaves a pool's contents alone
    /// natively, and a compositor that reads shm when it paints rather than
    /// at commit (wlroots' pixman renderer) goes on reading those pages until
    /// the surface commits something else. Punched now, it would paint
    /// zeros. So the buffer, its pages and their charge stay with the
    /// surface until its next attach is committed, or the surface goes: one
    /// buffer per surface at most.
    ///
    /// Only the surfaces that name the buffer are visited (`Buffer::surfaces`),
    /// not every surface: a peer that made thousands of surfaces must not
    /// make every buffer it destroys cost as many steps.
    pub fn forget(&mut self, id: u32) {
        self.pools.remove(&id);
        if let Some(mut buf) = self.buffers.remove(&id) {
            let linked = std::mem::take(&mut buf.surfaces);
            let mut buf = Some(buf);
            for sid in linked {
                let Some(s) = self.surfaces.get_mut(&sid) else {
                    continue;
                };
                s.buffers.retain(|&b| b != id);
                if s.current == id {
                    s.current = 0;
                    if let Some(b) = buf.take() {
                        s.retired = Some(b);
                    }
                }
            }
        }
        if let Some(s) = self.surfaces.remove(&id) {
            for b in s.buffers.iter().chain(std::iter::once(&s.current)) {
                if let Some(buf) = self.buffers.get_mut(b) {
                    buf.surfaces.remove(&id);
                }
            }
        }
    }

    /// Surface `surface` no longer names buffer `buffer`, unless it still
    /// shows it or collects damage for it.
    fn unlink(&mut self, surface: u32, buffer: u32) {
        let named = self
            .surfaces
            .get(&surface)
            .is_some_and(|s| s.current == buffer || s.buffers.contains(&buffer));
        if !named && let Some(b) = self.buffers.get_mut(&buffer) {
            b.surfaces.remove(&surface);
        }
    }

    /// Surface `surface` shows buffer `b` from this commit on, in place of
    /// `old`.
    fn show(&mut self, surface: u32, old: u32, b: u32) {
        if let Some(buf) = self.buffers.get_mut(&b) {
            buf.surfaces.insert(surface);
        }
        if old != b {
            self.unlink(surface, old);
        }
    }

    /// Pools this connection holds a descriptor for: every one charged to its
    /// own count, which a pool leaves only when the last reference to it
    /// (its id, a buffer made from it, a commit's copy) goes.
    pub fn pool_fds(&self) -> u64 {
        self.conn.used().1
    }

    /// Collect damage for the last `n` buffers each surface showed, held to
    /// 1..=[`MAX_SURFACE_BUFFERS`]; set before the connection's first
    /// commit (a surface that already holds more lets the oldest go at its
    /// next one).
    pub fn set_surface_buffers(&mut self, n: usize) {
        self.surface_buffers = n.clamp(1, MAX_SURFACE_BUFFERS);
    }

    /// Buffers a surface collects damage for.
    pub fn surface_buffers(&self) -> usize {
        self.surface_buffers
    }

    /// Draw on a budget other connections share too (the VM's, a guest
    /// process's), beside this connection's own and any set before. Pools
    /// already made keep what they were charged to.
    pub fn set_shared_budget(&mut self, b: Arc<dyn ShmCharge>) {
        self.shared.push(b);
    }

    /// Everything known about pools, buffers and surfaces goes (the
    /// connection is over), and with the buffers their pages and charges. A
    /// pool still referenced from elsewhere keeps its place in the count
    /// until that reference goes too.
    pub fn clear(&mut self) {
        self.pools.clear();
        self.buffers.clear();
        self.surfaces.clear();
    }

    /// Charge a new pool before it exists: one of the count. Its bytes are
    /// charged by the buffers made from it. `TooMany` if this connection or
    /// the VM is at its limit, and then nothing is taken.
    pub fn charge(&self) -> Result<Charge, ShmError> {
        let mut budgets = Budgets::<dyn ShmCharge>::default();
        budgets.push(self.conn.clone());
        budgets.extend(&self.shared);
        Charge::take(budgets)
    }

    /// A pool of `size` bytes, over `fd`: on the server's side our memfd,
    /// on the client's the client's own descriptor. Either way its buffers
    /// are charged by what they cover.
    pub fn add_pool(&mut self, id: u32, fd: OwnedFd, size: u64, charge: Charge, server_side: bool) {
        self.pools.insert(
            id,
            Arc::new(Pool {
                fd,
                size: AtomicU64::new(size),
                charge,
                held: Mutex::default(),
                punch: server_side,
            }),
        );
    }

    pub fn pool(&self, id: u32) -> Option<&Arc<Pool>> {
        self.pools.get(&id)
    }

    /// `wl_shm_pool.resize`. On the server side the memfd grows first, so the
    /// compositor's remap on the forwarded resize sees the new size. Growing
    /// is free: the new pages are a hole until a buffer over them is written.
    pub fn resize(&mut self, id: u32, size: u64, server_side: bool) -> Result<(), ShmError> {
        let p = self.pools.get(&id).ok_or(ShmError::NoPool(id))?;
        if size > MAX_POOL_SIZE {
            return Err(ShmError::TooBig);
        }
        if size > p.size.load(Ordering::Relaxed) {
            if server_side {
                sys::ftruncate(p.fd.as_raw_fd(), size).map_err(|_| ShmError::Io)?;
            }
            p.size.store(size, Ordering::Relaxed);
        }
        Ok(())
    }

    /// `wl_shm_pool.create_buffer`. The pages it covers that no other live
    /// buffer of the pool does are charged now, on either side, and past a
    /// budget it is `TooBig` and not made. Negative or overflowing geometry,
    /// or a buffer that does not fit its pool, is left for the compositor to
    /// refuse; it is simply not tracked here, so a commit of it copies
    /// nothing and an `SHM_SYNC` for it is refused.
    pub fn add_buffer(
        &mut self,
        pool: u32,
        id: u32,
        offset: i32,
        height: i32,
        stride: i32,
    ) -> Result<(), ShmError> {
        let Some(p) = self.pools.get(&pool) else {
            return Ok(());
        };
        if offset < 0 || height <= 0 || stride <= 0 {
            return Ok(());
        }
        let (offset, stride, height) = (offset as u64, stride as u64, height as u64);
        if offset + stride * height > p.size.load(Ordering::Relaxed) {
            return Ok(());
        }
        p.hold(offset, stride * height)?;
        self.buffers.insert(
            id,
            Buffer {
                pool: p.clone(),
                offset,
                stride,
                height,
                synced: false,
                dirty: None,
                surfaces: HashSet::new(),
            },
        );
        Ok(())
    }

    pub fn attach(&mut self, surface: u32, buffer: u32) {
        self.surfaces.entry(surface).or_default().pending = Some(buffer);
    }

    pub fn damage_surface(&mut self, surface: u32) {
        self.surfaces.entry(surface).or_default().damage_full = true;
    }

    pub fn damage_buffer(&mut self, surface: u32, y: i32, h: i32) {
        let s = self.surfaces.entry(surface).or_default();
        let a = y.max(0) as u64;
        let b = (y as i64 + h.max(0) as i64).max(0) as u64;
        s.damage_rows = union(s.damage_rows, (a, b));
    }

    /// `wl_surface.commit` on the server's side: only what the surface now
    /// shows, for [`Shm::forget`].
    pub fn commit_server(&mut self, surface: u32) {
        if let Some(s) = self.surfaces.get_mut(&surface)
            && let Some(b) = s.pending.take()
        {
            let old = std::mem::replace(&mut s.current, b);
            s.retired = None;
            self.show(surface, old, b);
        }
    }

    /// `wl_surface.commit` on the client's side: what the commit needs copied
    /// ahead of it, as a job the channel reads from when it has room
    /// ([`SyncJob`]), not as bytes read now.
    pub fn commit(&mut self, surface: u32) -> Option<SyncJob> {
        let s = self.surfaces.get_mut(&surface)?;
        if let Some(b) = s.pending.take() {
            let old = std::mem::replace(&mut s.current, b);
            s.retired = None;
            let mut evicted = None;
            if b != 0 && self.buffers.contains_key(&b) {
                s.buffers.retain(|&x| x != b);
                s.buffers.push_back(b);
                if s.buffers.len() > self.surface_buffers {
                    evicted = s.buffers.pop_front();
                }
            }
            self.show(surface, old, b);
            if let Some(e) = evicted {
                // This surface's damage is no longer collected for it, so
                // whatever it next shows is copied whole.
                if let Some(buf) = self.buffers.get_mut(&e) {
                    buf.synced = false;
                    buf.dirty = None;
                }
                self.unlink(surface, e);
            }
        }
        let s = self.surfaces.get_mut(&surface)?;
        let full = std::mem::take(&mut s.damage_full);
        let rows = s.damage_rows.take();
        for id in &s.buffers {
            if let Some(buf) = self.buffers.get_mut(id) {
                let d = if full { Some((0, buf.height)) } else { rows };
                if let Some(d) = d {
                    buf.dirty = union(buf.dirty, d);
                }
            }
        }
        let cur = s.current;
        let buf = self.buffers.get_mut(&cur)?;
        let range = if !buf.synced {
            Some((0, buf.height))
        } else {
            buf.dirty.take()
        };
        buf.synced = true;
        buf.dirty = None;
        let (y0, y1) = range?;
        let (y0, y1) = (y0.min(buf.height), y1.min(buf.height));
        if y0 >= y1 {
            return None;
        }
        let pool_size = buf.pool.size.load(Ordering::Relaxed);
        let start = y0 * buf.stride;
        let end = (y1 * buf.stride).min(pool_size.saturating_sub(buf.offset));
        if start >= end {
            return None;
        }
        self.syncs += 1;
        Some(SyncJob {
            pool: buf.pool.clone(),
            buffer: cur,
            base: buf.offset,
            off: start,
            end,
        })
    }

    /// An SHM_SYNC record on the server's side: store the bytes, within a
    /// live buffer and within the pool. This is what keeps every page the
    /// memfd holds one some live buffer is charged for: a buffer that was
    /// never tracked, or is gone, takes nothing.
    pub fn sync(&mut self, buffer: u32, off: u32, bytes: &[u8]) -> Result<(), ShmError> {
        let b = self
            .buffers
            .get(&buffer)
            .ok_or(ShmError::NoBuffer(buffer))?;
        let end = off as u64 + bytes.len() as u64;
        if end > b.len() || b.offset + end > b.pool.size.load(Ordering::Relaxed) {
            return Err(ShmError::OutOfRange(buffer));
        }
        sys::pwrite_full(b.pool.fd.as_raw_fd(), bytes, b.offset + off as u64)
            .map_err(|_| ShmError::Io)?;
        self.sync_bytes += bytes.len() as u64;
        self.syncs += 1;
        Ok(())
    }
}

/// The copy one commit needs, made as the channel takes it: bytes
/// `[off, end)` of buffer `buffer`, which starts at `base` in its pool, as
/// `SHM_SYNC` records. The pool is held (not the buffer: the client may
/// destroy that once it has committed), so the read still has its
/// descriptor. Reading later than the commit is as good as reading at it:
/// the client may not touch a committed buffer until the compositor
/// releases it, and the compositor has not seen the commit yet.
pub struct SyncJob {
    pool: Arc<Pool>,
    buffer: u32,
    base: u64,
    off: u64,
    end: u64,
}

impl Job for SyncJob {
    /// Bytes still to read.
    fn remaining(&self) -> u64 {
        self.end - self.off
    }

    /// The next record, and how many bytes of the buffer it carries; `None`
    /// once done, or where the client's file ends short (it truncated its
    /// own pool: the compositor keeps what it had there, and the rest of the
    /// copy is not sent).
    fn next_unit(&mut self) -> Option<(Unit, usize)> {
        if self.off >= self.end {
            return None;
        }
        let n = ((self.end - self.off) as usize).min(MAX_REC_PAYLOAD);
        let (fd, at) = (self.pool.fd.as_raw_fd(), self.base + self.off);
        let mut got = 0;
        // Read straight into the record: one copy of the pixels, not two.
        let rec = record_with(REC_SHM_SYNC, self.buffer, self.off as u32, n, |buf| {
            got = sys::pread_full(fd, buf, at).unwrap_or(0);
            got
        });
        if got == 0 {
            self.off = self.end;
            return None;
        }
        let u = Unit {
            rec,
            descs: Vec::new(),
        };
        self.off += got as u64;
        if got < n {
            self.off = self.end;
        }
        Some((u, got))
    }
}

#[cfg(test)]
mod extents_tests {
    use super::Extents;

    /// Against a count per page, over buffers made and destroyed at random.
    #[test]
    #[cfg_attr(miri, ignore = "exhaustive: too slow under Miri, and safe code")]
    fn steps_agree_with_a_count_per_page() {
        const PAGES: usize = 64;
        let mut ext = Extents::default();
        let mut count = [0u32; PAGES];
        let mut live: Vec<(u64, u64)> = Vec::new();
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        let mut rand = |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        for _ in 0..20_000 {
            if live.is_empty() || rand(3) != 0 {
                let a = rand(PAGES as u64);
                let b = a + 1 + rand(PAGES as u64 - a);
                let want = (a..b).filter(|&p| count[p as usize] == 0).count() as u64;
                assert_eq!(ext.uncovered(a, b), want);
                ext.add(a, b);
                (a..b).for_each(|p| count[p as usize] += 1);
                live.push((a, b));
            } else {
                let (a, b) = live.swap_remove(rand(live.len() as u64) as usize);
                (a..b).for_each(|p| count[p as usize] -= 1);
                let freed = ext.remove(a, b);
                let mut want: Vec<(u64, u64)> = Vec::new();
                for p in a..b {
                    if count[p as usize] == 0 {
                        match want.last_mut() {
                            Some(r) if r.1 == p => r.1 = p + 1,
                            _ => want.push((p, p + 1)),
                        }
                    }
                }
                assert_eq!(freed, want);
            }
            for p in 0..PAGES as u64 {
                assert_eq!(ext.at(p), count[p as usize]);
            }
            // No step repeats the one before it.
            let mut prev = 0;
            for &c in ext.steps.values() {
                assert_ne!(c, prev);
                prev = c;
            }
        }
        for (a, b) in live.drain(..) {
            ext.remove(a, b);
        }
        assert!(ext.steps.is_empty());
    }
}

#[cfg(test)]
mod surface_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    /// Rows per test buffer, each a page long over one page of the pool.
    const ROWS: u64 = 8;
    const PAGES: u64 = 4;

    /// A client-side `Shm` with pool 1 of `PAGES` pages.
    fn shm() -> Shm {
        let mut shm = Shm::default();
        let size = PAGES * sys::page_size();
        let fd = sys::memfd(c"t", size).unwrap();
        let charge = shm.charge().unwrap();
        shm.add_pool(1, fd, size, charge, false);
        shm
    }

    /// Buffer `id` over page `page` of pool 1, `ROWS` rows of it, as the
    /// engine makes one: whatever the id named before is forgotten first.
    fn buffer(shm: &mut Shm, id: u32, page: u64) {
        shm.forget(id);
        let pg = sys::page_size();
        let stride = (pg / ROWS) as i32;
        shm.add_buffer(1, id, (page * pg) as i32, ROWS as i32, stride)
            .unwrap();
    }

    fn commit(shm: &mut Shm, s: u32) -> Option<(u32, u64, u64)> {
        shm.commit(s).map(|j| (j.buffer, j.off, j.end))
    }

    fn named_by(shm: &Shm, s: u32) -> usize {
        shm.buffers
            .values()
            .filter(|b| b.surfaces.contains(&s))
            .count()
    }

    /// A surface collects damage for its last `SURFACE_BUFFERS` buffers,
    /// not for every buffer it ever showed; one it let go is copied whole
    /// when shown again.
    #[test]
    fn a_surface_collects_damage_for_a_bounded_number_of_buffers() {
        let mut shm = shm();
        let row = sys::page_size() / ROWS;
        for id in 100..1100 {
            buffer(&mut shm, id, 0);
            shm.attach(7, id);
            assert_eq!(commit(&mut shm, 7), Some((id, 0, ROWS * row)));
        }
        assert_eq!(shm.surfaces[&7].buffers.len(), SURFACE_BUFFERS);
        assert_eq!(named_by(&shm, 7), SURFACE_BUFFERS);
        // One still collected, not damaged since it was shown: no copy.
        shm.attach(7, 1099 - 3);
        assert_eq!(commit(&mut shm, 7), None);
        // One let go long ago: all of it, damage or none.
        shm.attach(7, 100);
        assert_eq!(commit(&mut shm, 7), Some((100, 0, ROWS * row)));
        assert_eq!(named_by(&shm, 7), SURFACE_BUFFERS);
    }

    /// With `set_surface_buffers`, a surface collects damage for that many:
    /// a client cycling through 32 buffers has only its damage copied at
    /// 32, and each buffer whole at the default 16. Held to 1..=256.
    #[test]
    fn the_surface_buffer_count_is_settable_and_bounded() {
        let row = sys::page_size() / ROWS;
        for (n, cycle, copies_whole) in [(SURFACE_BUFFERS, 32u32, true), (32, 32, false)] {
            let mut shm = shm();
            shm.set_surface_buffers(n);
            for id in 0..cycle {
                buffer(&mut shm, 100 + id, 0);
            }
            // Two rounds: the first shows each buffer for the first time.
            for round in 0..2 {
                for id in 0..cycle {
                    shm.damage_buffer(7, 0, 1);
                    shm.attach(7, 100 + id);
                    let got = commit(&mut shm, 7);
                    if round == 1 {
                        let whole = got == Some((100 + id, 0, ROWS * row));
                        assert_eq!(whole, copies_whole, "n {n}, buffer {id}: {got:?}");
                    }
                }
            }
            assert_eq!(shm.surfaces[&7].buffers.len(), n.min(cycle as usize));
        }
        let mut shm = shm();
        shm.set_surface_buffers(0);
        assert_eq!(shm.surface_buffers(), 1);
        shm.set_surface_buffers(usize::MAX);
        assert_eq!(shm.surface_buffers(), MAX_SURFACE_BUFFERS);
    }

    /// A buffer destroyed while its surface shows it stays with that
    /// surface; the thousand other surfaces are not visited for it, and a
    /// buffer no surface named goes at once.
    #[test]
    fn forget_visits_only_the_surfaces_that_name_the_buffer() {
        let mut shm = shm();
        for s in 1000..2000 {
            shm.damage_surface(s);
        }
        buffer(&mut shm, 100, 0);
        buffer(&mut shm, 101, 1);
        shm.attach(7, 100);
        assert!(commit(&mut shm, 7).is_some());
        let pg = sys::page_size();
        assert_eq!(shm.conn.used().0, 2 * pg);
        shm.forget(101);
        assert_eq!(shm.conn.used().0, pg, "a buffer nothing shows goes at once");
        shm.forget(100);
        assert_eq!(shm.conn.used().0, pg, "a shown one stays with its surface");
        assert!(shm.surfaces[&7].retired.is_some());
        assert_eq!(shm.surfaces[&7].current, 0);
        shm.forget(7);
        assert_eq!(shm.conn.used().0, 0);
    }

    /// The bookkeeping as it was before the list was bounded, for one
    /// surface: every buffer it ever showed collects its damage. A buffer
    /// is retired when the very object the surface's last commit showed is
    /// destroyed -- not a later buffer given the same id (which the
    /// compositor does not show, and which the old walk over every surface
    /// took for it).
    struct RefBuf {
        /// Which object, of the ids given out over time.
        obj: u64,
        synced: bool,
        dirty: Option<(u64, u64)>,
    }

    #[derive(Default)]
    struct Ref {
        bufs: HashMap<u32, RefBuf>,
        next_obj: u64,
        pending: Option<u32>,
        current: u32,
        /// The object `current` named when it was committed, if it was one.
        shows: Option<u64>,
        full: bool,
        rows: Option<(u64, u64)>,
        shown: HashSet<u32>,
        /// The page the retired buffer holds, if there is one.
        retired: Option<u64>,
        /// The page each live buffer holds.
        pages: HashMap<u32, u64>,
    }

    impl Ref {
        fn create(&mut self, id: u32, page: u64) {
            self.forget(id);
            self.next_obj += 1;
            let b = RefBuf {
                obj: self.next_obj,
                synced: false,
                dirty: None,
            };
            self.bufs.insert(id, b);
            self.pages.insert(id, page);
        }

        fn forget(&mut self, id: u32) {
            if let Some(RefBuf { obj, .. }) = self.bufs.remove(&id) {
                self.shown.remove(&id);
                let page = self.pages.remove(&id);
                if self.shows == Some(obj) {
                    self.current = 0;
                    self.shows = None;
                    self.retired = page;
                }
            }
        }

        fn forget_surface(&mut self) {
            self.retired = None;
            self.pending = None;
            self.current = 0;
            self.shows = None;
            self.full = false;
            self.rows = None;
            self.shown.clear();
        }

        fn commit(&mut self) -> Option<(u32, u64, u64)> {
            if let Some(b) = self.pending.take() {
                self.current = b;
                self.shows = self.bufs.get(&b).map(|x| x.obj);
                self.retired = None;
                if b != 0 && self.bufs.contains_key(&b) {
                    self.shown.insert(b);
                }
            }
            let full = std::mem::take(&mut self.full);
            let rows = self.rows.take();
            for id in &self.shown {
                let b = self.bufs.get_mut(id).unwrap();
                if let Some(d) = if full { Some((0, ROWS)) } else { rows } {
                    b.dirty = union(b.dirty, d);
                }
            }
            let cur = self.current;
            let b = self.bufs.get_mut(&cur)?;
            let range = if !b.synced {
                Some((0, ROWS))
            } else {
                b.dirty.take()
            };
            (b.synced, b.dirty) = (true, None);
            let (y0, y1) = range?;
            let (y0, y1) = (y0.min(ROWS), y1.min(ROWS));
            if y0 >= y1 {
                return None;
            }
            let row = sys::page_size() / ROWS;
            Some((cur, y0 * row, y1 * row))
        }

        fn charged(&self) -> u64 {
            let pages: HashSet<u64> = self.pages.values().chain(&self.retired).copied().collect();
            pages.len() as u64 * sys::page_size()
        }
    }

    /// Against the unbounded bookkeeping, over random buffers, attaches,
    /// damage, commits and destroys on one surface: with no more buffer ids
    /// than the list holds, every commit copies exactly what it copied; with
    /// more, at least that. Pages are held and let go the same either way.
    #[test]
    #[cfg_attr(miri, ignore = "randomized: too slow under Miri, and safe code")]
    fn bounded_damage_copies_what_unbounded_did_or_more() {
        for ids in [SURFACE_BUFFERS as u64, 3 * SURFACE_BUFFERS as u64] {
            let mut seed: u64 = 0x9e37_79b9_7f4a_7c15 ^ ids;
            let mut rand = |n: u64| {
                seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (seed >> 33) % n
            };
            let (mut shm, mut r) = (shm(), Ref::default());
            for _ in 0..50_000 {
                let id = 100 + rand(ids) as u32;
                match rand(8) {
                    0 => {
                        let page = rand(PAGES);
                        r.create(id, page);
                        buffer(&mut shm, id, page);
                    }
                    1 => {
                        r.forget(id);
                        shm.forget(id);
                    }
                    2 => {
                        let b = if rand(8) == 0 { 0 } else { id };
                        r.pending = Some(b);
                        shm.attach(7, b);
                    }
                    3 => {
                        let (y, h) = (rand(ROWS + 2) as i32 - 1, rand(ROWS) as i32);
                        let a = y.max(0) as u64;
                        let b = (y as i64 + h.max(0) as i64).max(0) as u64;
                        r.rows = union(r.rows, (a, b));
                        shm.damage_buffer(7, y, h);
                    }
                    4 => {
                        r.full = true;
                        shm.damage_surface(7);
                    }
                    5 if rand(16) == 0 => {
                        r.forget_surface();
                        shm.forget(7);
                    }
                    _ => {
                        let (want, got) = (r.commit(), commit(&mut shm, 7));
                        if ids as usize <= SURFACE_BUFFERS {
                            assert_eq!(got, want);
                        } else if let Some((b, o, e)) = want {
                            let (gb, go, ge) = got.expect("a copy the unbounded one made");
                            assert!(gb == b && go <= o && ge >= e, "{got:?} short of {want:?}");
                        }
                    }
                }
                assert_eq!(shm.conn.used().0, r.charged());
            }
        }
    }
}
