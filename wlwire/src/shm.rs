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
//! buffer. A buffer seen for the first time is copied whole.
//!
//! The client's descriptor is only ever read with `pread`, never mapped: a
//! client that truncates its pool under us gets short copies, not a SIGBUS in
//! the proxy.
//!
//! **What the server's side may hold.** Its memfds are the one place a peer
//! decides how much memory the proxy commits: every `SHM_SYNC` is written into
//! them at once, no commit or compositor consent needed, and the pages are
//! shmem that no process's RSS shows, so the host OOM killer, when they have
//! eaten the host, picks somebody else. Every pool is therefore charged to
//! [`ShmBudget`]s before its memfd exists and at every grow, and gives the
//! charge back only when the last reference to it (its own id, or a buffer made
//! from it) is gone: one budget per connection ([`MAX_POOL_BYTES`],
//! [`MAX_POOLS`]), and on the backend one per VM that every connection of the
//! VM shares ([`Engine::set_shm_budget`](crate::engine::Engine::set_shm_budget)),
//! since a guest can open as many connections as it is allowed channels. The
//! client's side charges only the count: its pools are the client's own
//! memory, but each is a descriptor held here.

use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::frame::{MAX_REC_PAYLOAD, REC_SHM_SYNC, Unit, record};
use crate::sys;

pub struct Pool {
    /// Client side: the client's own pool descriptor. Server side: our memfd.
    pub fd: OwnedFd,
    pub size: AtomicU64,
    /// What this pool holds of its budgets, given back when it goes.
    charge: Charge,
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
    fn take(&self, bytes: u64, pools: u64) -> bool {
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

    fn give(&self, bytes: u64, pools: u64) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.pools.fetch_sub(pools, Ordering::AcqRel);
    }
}

/// One pool's share of its budgets: one pool of the count, and `bytes`. Made
/// before the pool (so a refused pool never has a memfd), owned by it after,
/// and given back by `Drop` -- once, whichever way the pool goes.
pub struct Charge {
    budgets: Vec<Arc<ShmBudget>>,
    bytes: AtomicU64,
}

impl Charge {
    /// Take one pool and `bytes` from every budget, or from none.
    fn take(budgets: Vec<Arc<ShmBudget>>, bytes: u64) -> Result<Charge, ShmError> {
        for (i, b) in budgets.iter().enumerate() {
            if !b.take(bytes, 1) {
                for done in &budgets[..i] {
                    done.give(bytes, 1);
                }
                return Err(ShmError::TooBig);
            }
        }
        Ok(Charge {
            budgets,
            bytes: AtomicU64::new(bytes),
        })
    }

    /// `more` bytes on top, from every budget or from none.
    fn grow(&self, more: u64) -> bool {
        for (i, b) in self.budgets.iter().enumerate() {
            if !b.take(more, 0) {
                for done in &self.budgets[..i] {
                    done.give(more, 0);
                }
                return false;
            }
        }
        self.bytes.fetch_add(more, Ordering::AcqRel);
        true
    }
}

impl Drop for Charge {
    fn drop(&mut self) {
        let bytes = self.bytes.load(Ordering::Acquire);
        for b in &self.budgets {
            b.give(bytes, 1);
        }
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
}

impl Buffer {
    fn len(&self) -> u64 {
        self.stride * self.height
    }
}

#[derive(Default)]
struct Surface {
    /// `Some(x)`: an attach is pending, of buffer `x` (0 = null).
    pending: Option<u32>,
    current: u32,
    damage_full: bool,
    damage_rows: Option<(u64, u64)>,
    buffers: HashSet<u32>,
}

/// Pool bytes one connection may have the server's side hold at once (in
/// memfds that are sparse until written). A 4K RGBA buffer is 33 MB, so a
/// triple-buffered 4K window takes 100 MB and this is five of them, resizes
/// included; a peer asking for more is after host memory. The VM-wide budget
/// (`--wayland-shm-budget` on the backend) is what bounds the sum.
pub const MAX_POOL_BYTES: u64 = 512 << 20;

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
    /// Everyone's (the VM's), if the owner of the engine set one.
    shared: Option<Arc<ShmBudget>>,
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
            shared: None,
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
    pub fn forget(&mut self, id: u32) {
        self.pools.remove(&id);
        if self.buffers.remove(&id).is_some() {
            for s in self.surfaces.values_mut() {
                s.buffers.remove(&id);
                if s.current == id {
                    s.current = 0;
                }
            }
        }
        self.surfaces.remove(&id);
    }

    /// Draw on a budget every connection of the VM shares, beside this
    /// connection's own. Pools already made keep what they were charged to.
    pub fn set_shared_budget(&mut self, b: Arc<ShmBudget>) {
        self.shared = Some(b);
    }

    /// Everything known about pools, buffers and surfaces goes (the
    /// connection is over). A pool still referenced from elsewhere keeps its
    /// charge until that reference goes too.
    pub fn clear(&mut self) {
        self.pools.clear();
        self.buffers.clear();
        self.surfaces.clear();
    }

    /// Charge a new pool before it exists: one pool, and `bytes` of memory
    /// this side will hold (the pool's size for our own memfd, 0 for a
    /// client's descriptor). `TooBig` if this connection or the VM is at its
    /// limit, and then nothing is taken.
    pub fn charge(&self, bytes: u64) -> Result<Charge, ShmError> {
        let mut budgets = vec![self.conn.clone()];
        budgets.extend(self.shared.iter().cloned());
        Charge::take(budgets, bytes)
    }

    pub fn add_pool(&mut self, id: u32, fd: OwnedFd, size: u64, charge: Charge) {
        self.pools.insert(
            id,
            Arc::new(Pool {
                fd,
                size: AtomicU64::new(size),
                charge,
            }),
        );
    }

    pub fn pool(&self, id: u32) -> Option<&Arc<Pool>> {
        self.pools.get(&id)
    }

    /// `wl_shm_pool.resize`. On the server side the memfd grows first, so the
    /// compositor's remap on the forwarded resize sees the new size.
    pub fn resize(&mut self, id: u32, size: u64, server_side: bool) -> Result<(), ShmError> {
        let p = self.pools.get(&id).ok_or(ShmError::NoPool(id))?;
        let cur = p.size.load(Ordering::Relaxed);
        if size > cur {
            if server_side {
                // Charged before the memfd grows; a grow the kernel then
                // refuses keeps the charge, which the pool gives back whole.
                if !p.charge.grow(size - cur) {
                    return Err(ShmError::TooBig);
                }
                sys::ftruncate(p.fd.as_raw_fd(), size).map_err(|_| ShmError::Io)?;
            }
            p.size.store(size, Ordering::Relaxed);
        }
        Ok(())
    }

    /// `wl_shm_pool.create_buffer`. Negative or overflowing geometry is left
    /// for the compositor to refuse; it is simply not tracked here, and a
    /// commit of it copies nothing.
    pub fn add_buffer(&mut self, pool: u32, id: u32, offset: i32, height: i32, stride: i32) {
        let Some(p) = self.pools.get(&pool) else {
            return;
        };
        if offset < 0 || height <= 0 || stride <= 0 {
            return;
        }
        self.buffers.insert(
            id,
            Buffer {
                pool: p.clone(),
                offset: offset as u64,
                stride: stride as u64,
                height: height as u64,
                synced: false,
                dirty: None,
            },
        );
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

    /// `wl_surface.commit` on the client's side: queue the SHM_SYNC records
    /// the commit needs, in `out`, to go before it.
    pub fn commit(&mut self, surface: u32, out: &mut Vec<Unit>) {
        let Some(s) = self.surfaces.get_mut(&surface) else {
            return;
        };
        if let Some(b) = s.pending.take() {
            s.current = b;
            if b != 0 && self.buffers.contains_key(&b) {
                s.buffers.insert(b);
            }
        }
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
        let Some(buf) = self.buffers.get_mut(&cur) else {
            return;
        };
        let range = if !buf.synced {
            Some((0, buf.height))
        } else {
            buf.dirty.take()
        };
        buf.synced = true;
        buf.dirty = None;
        let Some((y0, y1)) = range else { return };
        let (y0, y1) = (y0.min(buf.height), y1.min(buf.height));
        if y0 >= y1 {
            return;
        }
        let pool_size = buf.pool.size.load(Ordering::Relaxed);
        let start = y0 * buf.stride;
        let end = (y1 * buf.stride).min(pool_size.saturating_sub(buf.offset));
        let mut off = start;
        let mut chunk = vec![0u8; MAX_REC_PAYLOAD];
        while off < end {
            let n = ((end - off) as usize).min(MAX_REC_PAYLOAD);
            let got = sys::pread_full(buf.pool.fd.as_raw_fd(), &mut chunk[..n], buf.offset + off)
                .unwrap_or(0);
            if got == 0 {
                break;
            }
            out.push(Unit {
                rec: record(REC_SHM_SYNC, cur, off as u32, &chunk[..got]),
                descs: Vec::new(),
            });
            self.sync_bytes += got as u64;
            off += got as u64;
        }
        self.syncs += 1;
    }

    /// An SHM_SYNC record on the server's side: store the bytes, within the
    /// buffer and within the pool.
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
