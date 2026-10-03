//! Holding a guest to `--vram-limit-mib`, on the RM path.
//!
//! `crate::vram` keeps the books; this is what feeds them and what acts on
//! them. Two hooks around every forwarded NVIDIA escape:
//!
//! - **Before the host sees it**, an allocation that takes video memory is
//!   admitted against what is left, or refused the way RM refuses one it
//!   cannot back: `NV_ERR_NO_MEMORY` in the caller's block, the ioctl itself
//!   succeeding. A guest over its limit fails exactly as a guest on a full
//!   card does.
//! - **After the host answered**, what RM actually made is charged (RM rounds
//!   the size up and writes it back), every object is recorded under its
//!   parent so a free of an ancestor releases what is under it, a duplicate
//!   holds the same memory once, and every figure that tells the guest how
//!   much video memory there is or is free is brought down to the limit and
//!   to what is left under it.
//!
//! Video memory is taken two ways, and both are covered: `RM_ALLOC` of
//! `NV01_MEMORY_LOCAL_USER` (what CUDA uses) and `VID_HEAP_CONTROL` (what
//! NVIDIA's Vulkan and GL userspace use). Field offsets come from
//! `abi::vidmem`, generated per release from its own headers. On a release
//! with no table of its own nothing is rewritten and nothing is charged by
//! guesswork; with a limit set, the backend says so at start.
//!
//! What this does not see: memory RM allocates on the guest's behalf inside
//! another object -- channel and context buffers -- which never appears as a
//! sized allocation. The limit undercounts by that much.

use super::*;
use abi::vidmem::{Alloc, Layout, Role};

const ESC_RM_FREE: u32 = 0x29;
const ESC_RM_CONTROL: u32 = 0x2a;
const ESC_RM_ALLOC: u32 = 0x2b;
const ESC_RM_DUP_OBJECT: u32 = 0x34;
const ESC_VID_HEAP_CONTROL: u32 = 0x4a;

/// The class whose allocation takes video memory through RM_ALLOC.
const NV01_MEMORY_LOCAL_USER: u32 = 0x40;
/// Its parameters, `NV_MEMORY_ALLOCATION_PARAMS`, travel as the nested block.
const NVOS21_STATUS: usize = 28;

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    if let Some(s) = b.get_mut(at..at + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    if let Some(s) = b.get_mut(at..at + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

/// Which NVOS32 function allocates, and where its fields are.
fn nvos32_alloc(l: &Layout, function: u32) -> Option<Alloc> {
    if function == l.nvos32_fn_alloc_size {
        Some(l.nvos32_alloc_size)
    } else if function == l.nvos32_fn_alloc_tiled_pitch_height {
        Some(l.nvos32_alloc_tiled_pitch_height)
    } else if function == l.nvos32_fn_alloc_size_range {
        Some(l.nvos32_alloc_size_range)
    } else {
        None
    }
}

/// Bring every memory-reporting FB_INFO entry in `list` down to the limit:
/// a total to at most `total_kib`, a free count to at most `free_kib`.
/// Returns how many entries were changed.
pub(super) fn clamp_fb_info(
    l: &Layout,
    list: &mut [u8],
    count: usize,
    total_kib: u32,
    free_kib: u32,
) -> usize {
    let mut changed = 0;
    let size = l.fb_info_entry_size as usize;
    for i in 0..count.min(l.fb_info_max_list as usize) {
        let at = i * size;
        let (Some(index), Some(data)) = (
            u32_at(list, at + l.fb_info_entry_index as usize),
            u32_at(list, at + l.fb_info_entry_data as usize),
        ) else {
            break;
        };
        let cap = match l.role(index) {
            Some(Role::Total) => total_kib,
            Some(Role::Free) => free_kib,
            _ => continue,
        };
        if data > cap {
            put_u32(list, at + l.fb_info_entry_data as usize, cap);
            changed += 1;
        }
    }
    changed
}

impl NvidiaBackend {
    /// The layout to act on: this release's own, never a neighbour's, whose
    /// index numbers would rewrite whichever field now holds them.
    fn vidmem_layout(&self) -> Option<&'static Layout> {
        self.vidmem.filter(|s| s.exact).map(|s| s.layout)
    }

    /// Admit a video-memory allocation before the host sees it, or answer it
    /// with `NV_ERR_NO_MEMORY` here. `Some(n)` is a reply already written.
    pub(super) fn vidmem_admit(
        &mut self,
        cookie: u64,
        payload: &[u8],
        resp_buf: &mut [u8],
    ) -> Option<usize> {
        let l = self.vidmem_layout()?;
        let req = read_struct::<IoctlReq>(payload, 0);
        if (req.cmd >> 8) & 0xFF != b'F' as u32 {
            return None;
        }
        let body = payload.get(size_of::<IoctlReq>()..)?;
        let data_len = req.data_len as usize;
        let top = body.get(..data_len)?;
        let nested = body.get(data_len..data_len + req.nested_len as usize)?;

        let (bytes, status_at) = match req.cmd & 0xFF {
            ESC_RM_ALLOC => {
                if u32_at(top, 12)? != NV01_MEMORY_LOCAL_USER {
                    return None;
                }
                let a = l.mem_alloc;
                if nested.len() < l.mem_alloc_size as usize {
                    return None;
                }
                let (attr, flags) = (
                    u32_at(nested, a.attr as usize)?,
                    u32_at(nested, a.flags as usize)?,
                );
                if !l.takes_vidmem(attr, flags) {
                    return None;
                }
                let status = if data_len >= NVOS64_STATUS + 4 {
                    NVOS64_STATUS
                } else {
                    NVOS21_STATUS
                };
                (u64_at(nested, a.size as usize)?, status)
            }
            ESC_VID_HEAP_CONTROL => {
                let a = nvos32_alloc(l, u32_at(top, l.nvos32_function as usize)?)?;
                let (attr, flags) = (
                    u32_at(top, a.attr as usize)?,
                    u32_at(top, a.flags as usize)?,
                );
                if !l.takes_vidmem(attr, flags) {
                    return None;
                }
                (u64_at(top, a.size as usize)?, l.nvos32_status as usize)
            }
            _ => return None,
        };

        if self.vram.admit(bytes) {
            return None;
        }
        if self.vram.refused() == 1 {
            log::warn!(
                "video memory: refused {} MiB with {} of {} MiB in use; further \
                 refusals are counted and reported at teardown",
                bytes.div_ceil(1 << 20),
                self.vram.in_use() >> 20,
                self.vram.limit_mib()
            );
        }
        let param_in = body.get(..data_len + req.nested_len as usize)?;
        let mut out = param_in.to_vec();
        put_u32(&mut out, status_at, rmctrl::NV_ERR_NO_MEMORY);
        self.current_data_len = req.data_len;
        Some(self.write_ioctl_resp(resp_buf, cookie, &out))
    }

    /// After the host answered: charge, release and rewrite.
    pub(super) fn vidmem_note(&mut self, payload: &[u8], resp: &mut [u8]) {
        let Some(l) = self.vidmem_layout() else {
            return;
        };
        let req = read_struct::<IoctlReq>(payload, 0);
        if (req.cmd >> 8) & 0xFF != b'F' as u32 {
            return;
        }
        let head = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        if resp.len() < head || read_struct::<MsgHeader>(resp, 0).status != 0 {
            return;
        }
        let r = read_struct::<IoctlResp>(resp, size_of::<MsgHeader>());
        let (data_len, nested_len, deep_len) = (
            r.data_len as usize,
            r.nested_len as usize,
            r.deep_len as usize,
        );
        let Some(out) = resp.get_mut(head..head + data_len + nested_len + deep_len) else {
            return;
        };
        let (top, rest) = out.split_at_mut(data_len);
        let (nested, deep) = rest.split_at_mut(nested_len);
        let w = |b: &[u8], at: usize| u32_at(b, at).unwrap_or(u32::MAX);

        match req.cmd & 0xFF {
            ESC_RM_ALLOC if data_len >= NVOS21_STATUS + 4 => {
                let status = if data_len >= NVOS64_STATUS + 4 {
                    w(top, NVOS64_STATUS)
                } else {
                    w(top, NVOS21_STATUS)
                };
                let (client, parent, handle, class) = (w(top, 0), w(top, 4), w(top, 8), w(top, 12));
                if status != NV_OK || ROOT_CLASSES.contains(&class) {
                    return;
                }
                let a = l.mem_alloc;
                let bytes = (class == NV01_MEMORY_LOCAL_USER
                    && l.takes_vidmem(w(nested, a.attr as usize), w(nested, a.flags as usize)))
                .then(|| u64_at(nested, a.size as usize))
                .flatten();
                self.vram.allocated(client, parent, handle, bytes);
            }
            ESC_VID_HEAP_CONTROL if data_len >= l.nvos32_size as usize => {
                if w(top, l.nvos32_status as usize) != NV_OK {
                    return;
                }
                let function = w(top, l.nvos32_function as usize);
                if let Some(a) = nvos32_alloc(l, function) {
                    if !l.takes_vidmem(w(top, a.attr as usize), w(top, a.flags as usize)) {
                        return;
                    }
                    let bytes = u64_at(top, a.size as usize);
                    self.vram
                        .allocated(w(top, 0), w(top, 4), w(top, a.h_memory as usize), bytes);
                } else if function == l.nvos32_fn_info {
                    let (Some(limit), Some(left)) = (self.vram.limit(), self.vram.remaining())
                    else {
                        return;
                    };
                    for (at, cap) in [
                        (l.nvos32_total, limit),
                        (l.nvos32_free, left),
                        (l.nvos32_info_size, left),
                    ] {
                        if let Some(v) = u64_at(top, at as usize) {
                            put_u64(top, at as usize, v.min(cap));
                        }
                    }
                } else if function == l.nvos32_fn_free {
                    // Frees through RM_FREE are what NVIDIA's userspace sends;
                    // this one has no layout in the table yet. Counted, so a
                    // workload that uses it shows up rather than leaking a
                    // charge silently.
                    self.vidmem_untracked_frees += 1;
                }
            }
            ESC_RM_FREE if data_len >= 16 => {
                let (client, handle) = (w(top, 0), w(top, 8));
                if w(top, 12) == NV_OK && handle != client {
                    self.vram.freed(client, handle);
                }
            }
            ESC_RM_DUP_OBJECT if data_len >= 28 => {
                // NVOS55: hClient, hParent, hObject, hClientSrc, hObjectSrc,
                // flags, status.
                if w(top, 24) == NV_OK {
                    self.vram
                        .duplicated(w(top, 0), w(top, 4), w(top, 8), w(top, 12), w(top, 16));
                }
            }
            ESC_RM_CONTROL if data_len >= NVOS54_STATUS + 4 => {
                if w(top, NVOS54_STATUS) != NV_OK {
                    return;
                }
                let (Some(limit), Some(left)) = (self.vram.limit(), self.vram.remaining()) else {
                    return;
                };
                let total_kib = (limit >> 10).min(u32::MAX as u64) as u32;
                let free_kib = (left >> 10).min(u32::MAX as u64) as u32;
                let cmd = w(top, NVOS54_CMD);
                if cmd == l.cmd_fb_get_info_v2 {
                    let b = l.fb_get_info_v2;
                    let count = w(nested, b.count as usize) as usize;
                    if let Some(list) = nested.get_mut(b.list as usize..) {
                        clamp_fb_info(l, list, count, total_kib, free_kib);
                    }
                } else if cmd == l.cmd_fb_get_info {
                    // V1 carries its list behind a pointer: the deep block.
                    let count = w(nested, l.fb_get_info_v1.count as usize) as usize;
                    clamp_fb_info(l, deep, count, total_kib, free_kib);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(list: &mut Vec<u8>, index: u32, data: u32) {
        list.extend_from_slice(&index.to_le_bytes());
        list.extend_from_slice(&data.to_le_bytes());
    }

    #[test]
    fn totals_and_frees_are_clamped_and_nothing_else_is() {
        let l = &abi::vidmem::v615_71_09::LAYOUT;
        let mut list = Vec::new();
        let gib6 = 6 << 20; // KiB
        entry(&mut list, l.index("HEAP_SIZE").unwrap(), gib6);
        entry(&mut list, l.index("RAM_SIZE").unwrap(), gib6);
        entry(&mut list, l.index("HEAP_FREE").unwrap(), gib6 - 1000);
        entry(&mut list, l.index("BAR1_SIZE").unwrap(), 256 << 10);
        entry(&mut list, l.index("HEAP_BASE_KB").unwrap(), 12345);
        entry(&mut list, l.index("HEAP_SIZE").unwrap(), 100);
        let n = clamp_fb_info(l, &mut list, 6, 2 << 20, 1 << 20);
        assert_eq!(n, 3);
        let data = |i: usize| u32_at(&list, i * 8 + 4).unwrap();
        assert_eq!(data(0), 2 << 20);
        assert_eq!(data(1), 2 << 20);
        assert_eq!(data(2), 1 << 20);
        assert_eq!(data(3), 256 << 10, "BAR1 is a window, not memory");
        assert_eq!(data(4), 12345, "a position is not a size");
        assert_eq!(data(5), 100, "already under the cap");
    }

    #[test]
    fn a_count_past_the_list_stops_at_its_end() {
        let l = &abi::vidmem::v615_71_09::LAYOUT;
        let mut list = Vec::new();
        entry(&mut list, l.index("HEAP_SIZE").unwrap(), 6 << 20);
        assert_eq!(clamp_fb_info(l, &mut list, 1000, 1 << 20, 1 << 20), 1);
    }
}
