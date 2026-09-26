// SPDX-License-Identifier: GPL-2.0-only
//! A fake backend: answers a request with a reply that is a function of the
//! request's bytes and a seed, so both implementations, sending the same
//! bytes, read the same answer. Mostly well formed, sometimes not, as a
//! backend that is not trusted to be right might answer.

use crate::world::{hash, Ev, Rng, World};

const HDR: usize = 16;

fn le32(b: &[u8], off: usize) -> u32 {
    b.get(off..off + 4).map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()))
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Serve one request into `resp` (its capacity); the bytes written, or a
/// transport error.
pub fn serve(w: &mut World, req: &[u8], resp: &mut [u8]) -> Result<u32, i32> {
    let mut logged = req.to_vec();
    if logged.len() >= HDR {
        // req_id is the transport's, not the parser's.
        logged[12..16].fill(0);
    }
    w.events.push(Ev::Send(logged));
    if !w.fail.is_empty() {
        return Err(w.fail.remove(0));
    }
    if !w.canned.is_empty() {
        let reply = w.canned.remove(0);
        let n = reply.len().min(resp.len());
        resp[..n].copy_from_slice(&reply[..n]);
        return Ok(n as u32);
    }
    let mut rng = Rng::new(hash(w.backend_seed, req));
    if w.chaos > 0 && rng.chance(w.chaos, 96) {
        return Err(rng.pick(&[-110, -4, -19]));
    }
    let reply = match le32(req, 0) {
        3 => ioctl_reply(w, &mut rng, req),
        10 => i2_reply(w, &mut rng, req),
        _ => status_only(-95),
    };
    let n = reply.len().min(resp.len());
    resp[..n].copy_from_slice(&reply[..n]);
    let mut used = n;
    if w.chaos > 0 && rng.chance(w.chaos, 48) {
        used = rng.below(n as u64 + 1) as usize;
    }
    Ok(used as u32)
}

fn status_only(status: i32) -> Vec<u8> {
    let mut r = Vec::new();
    put32(&mut r, 0);
    put32(&mut r, 0);
    put32(&mut r, status as u32);
    put32(&mut r, 0);
    r
}

fn some_status(w: &World, rng: &mut Rng) -> i32 {
    if w.chaos == 0 || !rng.chance(w.chaos, 24) {
        return 0;
    }
    rng.pick(&[-22, -1, -2, -95, 5, -5000, i32::MIN])
}

/// A protocol-v1 IOCTL: the blocks echoed back, as RM leaves them, with
/// RM's status set, some bytes changed, and now and then the lengths wrong.
fn ioctl_reply(w: &mut World, rng: &mut Rng, req: &[u8]) -> Vec<u8> {
    let status = some_status(w, rng);
    if status != 0 && rng.chance(1, 2) {
        return status_only(status);
    }
    let data_len = le32(req, 20) as usize;
    let nested_len = le32(req, 28) as usize;
    let deep_off = le32(req, 32);
    let deep_len = le32(req, 36) as usize;
    let body = &req[40.min(req.len())..];
    let take = |from: usize, n: usize| -> Vec<u8> {
        let mut v = body.get(from..).unwrap_or(&[]).to_vec();
        v.resize(n, 0);
        v
    };
    let mut data = take(0, data_len);
    let mut nested = take(data_len, nested_len);
    let mut deep = take(data_len + nested_len, deep_len);

    // RM_CONTROL / RM_ALLOC report RM's own status in the block.
    if data_len >= 32 && rng.chance(3, 4) {
        let at = if data_len == 48 { 40 } else { 28 };
        let st: u32 = if rng.chance(4, 5) { 0 } else { rng.pick(&[0x1e, 0x56, 0x1f]) };
        data[at..at + 4].copy_from_slice(&st.to_le_bytes());
    }
    for b in [&mut data, &mut nested, &mut deep] {
        if !b.is_empty() && rng.chance(1, 2) {
            let n = 1 + rng.below(4);
            for _ in 0..n {
                let i = rng.below(b.len() as u64) as usize;
                b[i] = rng.next() as u8;
            }
        }
    }
    // A registration by pages answers with its id.
    if deep_off == 0xffff_fffe {
        deep = if rng.chance(4, 5) {
            w.next_id += 1;
            (w.next_id * 7).to_le_bytes().to_vec()
        } else {
            Vec::new()
        };
    }
    if w.chaos > 0 && rng.chance(w.chaos, 32) {
        match rng.below(3) {
            0 => data.resize(rng.below(data_len as u64 + 16) as usize, 0x77),
            1 => nested.resize(rng.below(nested_len as u64 + 16) as usize, 0x66),
            _ => deep.resize(rng.below(deep_len as u64 + 16) as usize, 0x55),
        }
    }
    let mut r = Vec::new();
    put32(&mut r, 3);
    put32(&mut r, le32(req, 4));
    put32(&mut r, status as u32);
    put32(&mut r, 0);
    put32(&mut r, data.len() as u32);
    put32(&mut r, nested.len() as u32);
    put32(&mut r, deep.len() as u32);
    r.extend_from_slice(&data);
    r.extend_from_slice(&nested);
    r.extend_from_slice(&deep);
    r
}

/// An IOCTL2: every OUT buffer as the host might have left it, and
/// descriptors and GEM handles at the positions the schema names -- mostly;
/// sometimes elsewhere, twice, or with the counts wrong.
fn i2_reply(w: &mut World, rng: &mut Rng, _req: &[u8]) -> Vec<u8> {
    let status = some_status(w, rng);
    if status != 0 {
        return status_only(status);
    }
    let shape = w.shape.clone().unwrap_or_default();
    let chaos = w.chaos;
    let odd = |rng: &mut Rng| chaos > 0 && rng.chance(chaos, 40);

    let ret: i32 = if rng.chance(3, 4) { 0 } else { rng.pick(&[-22, -2, -13, -4096]) };
    let mut data = Vec::new();
    for &(len, dir) in &shape.bufs {
        if dir & 2 == 0 {
            continue;
        }
        // Counts the host writes back are mostly small: word by word.
        let mut b = Vec::with_capacity(len as usize + 4);
        while b.len() < len as usize {
            let w: u32 = if rng.chance(3, 4) { rng.below(6) as u32 } else { rng.next() as u32 };
            b.extend_from_slice(&w.to_le_bytes());
        }
        b.truncate(len as usize);
        b.resize(align8(len as usize), 0);
        data.extend_from_slice(&b);
    }
    let mut fds = Vec::new();
    for &(buf, off) in &shape.fd_out {
        if rng.chance(2, 3) {
            let h = if rng.chance(1, 8) { 0 } else { 1 + rng.below(40) as u32 };
            fds.push((buf, off, h, rng.pick(&[1u32, 2, 5, 6])));
        }
    }
    let mut gems = Vec::new();
    for &(buf, off) in &shape.gem_out {
        if rng.chance(2, 3) {
            let h = if rng.chance(1, 10) { 0 } else { 1 + rng.below(4) as u32 };
            gems.push((buf, off, h, rng.below(1 << 20)));
        }
    }
    if odd(rng) || rng.chance(1, 12) {
        fds.push((rng.below(3) as u32, rng.below(64) as u32 & !3, 9, 1));
    }
    if (odd(rng) || rng.chance(1, 6)) && !fds.is_empty() {
        let d = fds[rng.below(fds.len() as u64) as usize];
        fds.push(d);
    }
    if odd(rng) || rng.chance(1, 12) {
        gems.push((0, rng.below(64) as u32 & !3, 3, 0));
    }
    if rng.chance(1, 6) && !gems.is_empty() {
        let d = gems[rng.below(gems.len() as u64) as usize];
        gems.push(d);
    }
    let mut nbuf = shape.bufs.len() as u32;
    let mut data_len = data.len() as u32;
    if odd(rng) {
        nbuf = nbuf.wrapping_add(1);
    }
    if odd(rng) {
        data_len = data_len.wrapping_sub(8);
    }
    let mut r = Vec::new();
    put32(&mut r, 10);
    put32(&mut r, 0);
    put32(&mut r, 0);
    put32(&mut r, 0);
    put32(&mut r, ret as u32);
    put32(&mut r, nbuf);
    put32(&mut r, fds.len() as u32);
    put32(&mut r, gems.len() as u32);
    put32(&mut r, data_len);
    r.extend_from_slice(&[0u8; 12]);
    r.extend_from_slice(&data);
    for (b, o, h, k) in fds {
        put32(&mut r, b);
        put32(&mut r, o);
        put32(&mut r, h);
        put32(&mut r, k);
    }
    for (b, o, h, s) in gems {
        put32(&mut r, b);
        put32(&mut r, o);
        put32(&mut r, h);
        put32(&mut r, 0);
        r.extend_from_slice(&s.to_le_bytes());
    }
    r
}
