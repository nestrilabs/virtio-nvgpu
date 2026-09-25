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
/// memfds that are sparse until written). Four 4K HDR swapchains fit many
/// times over; a peer asking for more is only after host memory.
pub const MAX_POOL_BYTES: u64 = 8 << 30;

#[derive(Default)]
pub struct Shm {
    pools: HashMap<u32, Arc<Pool>>,
    buffers: HashMap<u32, Buffer>,
    surfaces: HashMap<u32, Surface>,
    pub sync_bytes: u64,
    pub syncs: u64,
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

    pub fn add_pool(&mut self, id: u32, fd: OwnedFd, size: u64) {
        self.pools.insert(
            id,
            Arc::new(Pool {
                fd,
                size: AtomicU64::new(size),
            }),
        );
    }

    /// Whether the server's side may take on `more` pool bytes. Counts every
    /// pool still referenced, by its own id or by a buffer made from it.
    pub fn may_grow(&self, more: u64) -> bool {
        let mut seen: Vec<*const Pool> = Vec::new();
        let mut total = 0u64;
        let pools = self
            .pools
            .values()
            .chain(self.buffers.values().map(|b| &b.pool));
        for p in pools {
            let ptr = Arc::as_ptr(p);
            if !seen.contains(&ptr) {
                seen.push(ptr);
                total += p.size.load(Ordering::Relaxed);
            }
        }
        total.saturating_add(more) <= MAX_POOL_BYTES
    }

    pub fn pool(&self, id: u32) -> Option<&Arc<Pool>> {
        self.pools.get(&id)
    }

    /// `wl_shm_pool.resize`. On the server side the memfd grows first, so the
    /// compositor's remap on the forwarded resize sees the new size.
    pub fn resize(&mut self, id: u32, size: u64, server_side: bool) -> Result<(), ShmError> {
        let cur = self
            .pools
            .get(&id)
            .ok_or(ShmError::NoPool(id))?
            .size
            .load(Ordering::Relaxed);
        if server_side && size > cur && !self.may_grow(size - cur) {
            return Err(ShmError::TooBig);
        }
        let p = self.pools.get(&id).ok_or(ShmError::NoPool(id))?;
        if size > p.size.load(Ordering::Relaxed) {
            if server_side {
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
