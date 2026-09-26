// SPDX-License-Identifier: Apache-2.0
//! The control queue as the guest can write it: descriptor table, available
//! ring and every buffer, all bytes of the input, walked by `virtio-queue`
//! and taken apart by `vring.rs` exactly as the vhost-user transport's
//! `process` does -- summed, gathered, served by the whole backend on the
//! fake host, the reply scattered back, the chain returned.
//!
//! Guest memory is two regions with a hole between them, so descriptors can
//! point into nothing, across a region's end, or at the rings themselves.

#![forbid(unsafe_code)]

use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes as _, GuestAddress, GuestMemoryMmap};

use super::Bytes;
use super::backend::{CFG_HELLO, CFG_VERSION_SHIFT, Vm, no_fd_leak};
use crate::vring::{gather, layout, scatter};

const DESC: u64 = 0x0;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const HDR: usize = 16;

pub fn run(data: &[u8]) {
    super::sandboxed();
    let mut b = Bytes::new(data);
    let cfg = b.u8();
    let size = 1u16 << (b.u8() % 9);
    let (max_req, max_resp) = (256 << 10, 64 << 10);
    let mem: GuestMemoryMmap = GuestMemoryMmap::from_ranges(&[
        (GuestAddress(0), 0x10000),
        (GuestAddress(0x10_0000), 0x4000),
    ])
    .unwrap();
    // The guest's side of the queue: whatever the input says.
    let image = b.rest();
    mem.write_slice(&image[..image.len().min(0x10000)], GuestAddress(0))
        .unwrap();
    let mut q = Queue::new(256).unwrap();
    q.set_size(size);
    q.set_desc_table_address(Some(DESC as u32), Some(0));
    q.set_avail_ring_address(Some(AVAIL as u32), Some(0));
    q.set_used_ring_address(Some(USED as u32), Some(0));
    q.set_event_idx(cfg & 0x80 != 0);
    q.set_ready(true);
    if !q.is_valid(&mem) {
        return;
    }
    no_fd_leak(|| {
        let mut vm = Vm::new((cfg & 0x3f) | CFG_HELLO | 3 << CFG_VERSION_SHIFT, &[]);
        for _ in 0..32 {
            let Ok(mut avail) = q.iter(&mem) else { break };
            let Some(chain) = avail.next() else { break };
            drop(avail);
            let head = chain.head_index();
            let descs = chain.map(|d| (d.is_write_only(), d.addr(), d.len()));
            let (l, refused) = match layout(descs, max_req) {
                Ok(l) => (l, false),
                Err(l) => (l, true),
            };
            let n = if l.cap < HDR || refused {
                0
            } else {
                match gather(&mem, &l.readable, l.req_len) {
                    Some(req) => {
                        assert_eq!(req.len(), l.req_len);
                        let reply = vm.serve(&req, l.cap.min(max_resp));
                        let n = scatter(&mem, &l.writable, &reply);
                        assert!(n <= l.cap && n <= reply.len());
                        n
                    }
                    None => 0,
                }
            };
            if q.add_used(&mem, head, n as u32).is_err() {
                break;
            }
        }
        vm.end();
    });
}
