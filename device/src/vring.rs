// SPDX-License-Identifier: Apache-2.0
//! A request's descriptor chain, as the vhost-user transport takes it off
//! the control queue: summed before a byte is read, gathered from guest
//! memory, and the reply scattered back.
//!
//! The descriptors are the guest's -- addresses, lengths, which are writable,
//! how many -- so nothing here trusts them: a request over the transport's
//! limit is refused before it is copied, a length or address outside guest
//! memory fails the copy, and the reply goes into the writable descriptors
//! in order and no further. Here rather than in the binary so it can be
//! fuzzed with the rest (`fuzz/`, the `vring` target).

#![forbid(unsafe_code)]

use vm_memory::{Bytes, GuestAddress, GuestMemory};

/// What a chain's descriptors add up to, before anything is read.
#[derive(Debug, PartialEq, Eq)]
pub struct Layout {
    pub readable: Vec<(GuestAddress, u32)>,
    pub req_len: usize,
    pub writable: Vec<(GuestAddress, u32)>,
    pub cap: usize,
}

/// Sum a chain's descriptors: `(write_only, addr, len)` in chain order.
///
/// `Err(Layout)` -- with no readable descriptors kept -- when the request is
/// larger than `max_req`, found before a byte of it is copied: the lengths are
/// the guest's, and the old loop allocated each one as it came. Writable
/// capacity is counted in full; the caller refuses a response it cannot hold.
pub fn layout(
    descs: impl Iterator<Item = (bool, GuestAddress, u32)>,
    max_req: usize,
) -> Result<Layout, Layout> {
    let mut l = Layout {
        readable: Vec::new(),
        req_len: 0,
        writable: Vec::new(),
        cap: 0,
    };
    let mut too_big = false;
    for (write_only, addr, len) in descs {
        if write_only {
            l.cap = l.cap.saturating_add(len as usize);
            l.writable.push((addr, len));
        } else if !too_big {
            l.req_len = l.req_len.saturating_add(len as usize);
            if l.req_len > max_req {
                too_big = true;
                l.readable.clear();
            } else {
                l.readable.push((addr, len));
            }
        }
    }
    if too_big { Err(l) } else { Ok(l) }
}

/// Copy `bytes` over the writable descriptors in order. Returns what was
/// written, which is what the used ring reports.
pub fn scatter<G: GuestMemory>(mem: &G, writable: &[(GuestAddress, u32)], bytes: &[u8]) -> usize {
    let mut off = 0;
    for &(addr, len) in writable {
        if off == bytes.len() {
            break;
        }
        let n = (len as usize).min(bytes.len() - off);
        if let Err(e) = mem.write_slice(&bytes[off..off + n], addr) {
            log::warn!("writing a response into guest memory at {:#x}: {e}", addr.0);
            break;
        }
        off += n;
    }
    off
}

/// Read a request out of its readable descriptors.
pub fn gather<G: GuestMemory>(
    mem: &G,
    readable: &[(GuestAddress, u32)],
    len: usize,
) -> Option<Vec<u8>> {
    let mut req = vec![0u8; len];
    let mut off = 0;
    for &(addr, n) in readable {
        let n = n as usize;
        if let Err(e) = mem.read_slice(&mut req[off..off + n], addr) {
            log::warn!("reading a request from guest memory at {:#x}: {e}", addr.0);
            return None;
        }
        off += n;
    }
    Some(req)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::GuestMemoryMmap;

    fn a(x: u64) -> GuestAddress {
        GuestAddress(x)
    }

    #[test]
    fn a_request_is_gathered_from_every_readable_descriptor() {
        let l = layout(
            [
                (false, a(0), 16),
                (false, a(100), 40),
                (true, a(200), 64),
                (true, a(400), 64),
            ]
            .into_iter(),
            1024,
        )
        .unwrap();
        assert_eq!(l.req_len, 56);
        assert_eq!(l.readable.len(), 2);
        assert_eq!(l.cap, 128);
        assert_eq!(l.writable, vec![(a(200), 64), (a(400), 64)]);
    }

    #[test]
    fn a_request_over_the_limit_is_refused_before_it_is_read() {
        let l = layout(
            [
                (false, a(0), 600),
                (false, a(1000), 600),
                (true, a(2000), 16),
            ]
            .into_iter(),
            1024,
        )
        .unwrap_err();
        assert!(l.readable.is_empty(), "nothing of it is kept to be read");
        assert_eq!(l.cap, 16, "there is still somewhere to say so");
    }

    fn memory() -> GuestMemoryMmap {
        GuestMemoryMmap::from_ranges(&[(a(0), 0x10000)]).unwrap()
    }

    #[test]
    fn a_response_is_scattered_across_every_writable_descriptor() {
        let mem = memory();
        let bytes: Vec<u8> = (0..100u8).collect();
        let n = scatter(
            &mem,
            &[(a(0x1000), 30), (a(0x3000), 50), (a(0x5000), 50)],
            &bytes,
        );
        assert_eq!(n, 100);
        let mut back = vec![0u8; 100];
        mem.read_slice(&mut back[..30], a(0x1000)).unwrap();
        mem.read_slice(&mut back[30..80], a(0x3000)).unwrap();
        mem.read_slice(&mut back[80..], a(0x5000)).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn gather_reads_descriptors_in_order() {
        let mem = memory();
        mem.write_slice(b"hello ", a(0x100)).unwrap();
        mem.write_slice(b"world", a(0x900)).unwrap();
        let req = gather(&mem, &[(a(0x100), 6), (a(0x900), 5)], 11).unwrap();
        assert_eq!(req, b"hello world");
    }
}
