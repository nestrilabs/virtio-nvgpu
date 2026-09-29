// SPDX-License-Identifier: GPL-2.0-only
//! The world both implementations run in: the caller's memory, its
//! descriptor table, the backend, the hooks, pinning -- and a log of every
//! effect either has on it, which is what the test compares.

use std::collections::BTreeMap;

/// Where the user half of the address space ends (x86-64's, and the shim's
/// `DIFFTEST_USER_END`): memory the world maps above it is kernel memory.
pub const USER_END: u64 = 0x0000_8000_0000_0000;
/// A kernel address for an argument the DRM node's entry copied in
/// (`nvgpu_i2_call.karg`).
pub const KARG: u64 = 0xffff_8880_0010_0000;

/// Something one of the implementations did that the other must do too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ev {
    /// A message sent to the backend, byte for byte (req_id zeroed).
    Send(Vec<u8>),
    /// The transport flags an IOCTL2 was sent with.
    XferFlags(u32),
    /// `nvgpu_close_handle_async()`.
    Close(u32),
    /// `nvgpu_gem_close_async()`.
    GemClose(u32, u32),
    /// A log line, by its format string.
    Warn(String),
    /// `nvgpu_osdesc_reap()`.
    Reap,
    /// Pages pinned.
    Pin { start: u64, n: u64, write: bool },
    /// Pinned pages kept under a registration id.
    Keep { id: u64, n: u64, write: bool },
    /// Pinned pages released.
    Unpin { n: u64, write: bool },
    /// Pinned pages handed to the transport with a request it abandoned.
    HandOver { n: u64, write: bool },
    /// An IOCTL2 hook ran, with these arguments.
    Hook(Hook),
}

/// An IOCTL2 hook call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hook {
    FdIn {
        buf: u32,
        off: u32,
        v: i64,
        kinds: u32,
    },
    GemIn {
        buf: u32,
        off: u32,
        guest: u32,
    },
    FdOut {
        buf: u32,
        off: u32,
        handle: u32,
        kind: u32,
    },
    GemOut {
        buf: u32,
        off: u32,
        gem: u32,
        size: u64,
    },
    Special {
        id: u32,
        phase: i32,
    },
    Phase {
        phase: i32,
    },
    AObj {
        obj: u32,
    },
    AProp {
        id: u32,
    },
    /// `commit`: what the parse had said about the commit by then.
    AInFence {
        buf: u32,
        off: u32,
        fd: i64,
        commit: bool,
    },
    AOutFence {
        buf: u32,
        off: u32,
        uptr: u64,
    },
    ALearn {
        obj: u32,
        crtc: u32,
    },
    AReserve {
        crtc: u32,
        user_data: u64,
    },
    AtomicOut {
        commit: bool,
        values_buf: u32,
    },
}

/// A deterministic generator (SplitMix64), or, for the fuzzer, the fuzzer's
/// bytes first.
#[derive(Clone, Debug)]
pub struct Rng(pub u64, pub Vec<u8>);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed, Vec::new())
    }

    /// Draw from `data` while it lasts, then from a seed made of it.
    pub fn fuzz(data: &[u8]) -> Rng {
        let mut r = Rng(hash(0, data), data.to_vec());
        r.1.reverse();
        r
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u64 {
        if self.1.len() >= 8 {
            let mut v = [0u8; 8];
            for x in v.iter_mut() {
                *x = self.1.pop().unwrap_or(0);
            }
            return u64::from_le_bytes(v);
        }
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `[0, n)`; 0 for `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

/// A hash of bytes, to seed a reply from the request that asked for it.
pub fn hash(seed: u64, b: &[u8]) -> u64 {
    let mut h = seed ^ 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// How the IOCTL2 hooks answer.
#[derive(Clone, Debug, Default)]
pub struct Hooks {
    /// Which exist: bit 0 fd_in, 1 gem_in, 2 fd_out, 3 gem_out, 4 special,
    /// 5 phase.
    pub mask: u32,
    /// Varies what they answer.
    pub seed: u64,
    /// gem_out fails for this host handle.
    pub fail_gem: Option<u32>,
}

/// What an IOCTL2 looked like to the Rust interpreter, so the fake backend
/// can answer both implementations the same, plausible reply.
#[derive(Clone, Debug, Default)]
pub struct I2Shape {
    /// Every buffer: (length, direction).
    pub bufs: Vec<(u32, u8)>,
    /// Descriptor-out positions (schema slots and dyn records).
    pub fd_out: Vec<(u32, u32)>,
    /// GEM-out positions.
    pub gem_out: Vec<(u32, u32)>,
}

/// The world.
#[derive(Clone, Debug, Default)]
pub struct World {
    /// The caller's memory: base address to bytes. Regions never touch.
    pub mem: BTreeMap<u64, Vec<u8>>,
    /// The caller's descriptors that are ours, and the backend's handle for
    /// each.
    pub fds: BTreeMap<i32, u32>,
    /// Effects, in order.
    pub events: Vec<Ev>,
    /// `struct nvgpu_proc_id` as the caller's process has it.
    pub proc_id: [u8; 16],
    /// The guest's clock minus the host's; `None`: no conversion.
    pub clock: Option<i64>,
    /// `in_compat_syscall()`.
    pub compat: bool,
    /// Seeds the backend's replies.
    pub backend_seed: u64,
    /// How the backend misbehaves: 0 never .. 8 often.
    pub chaos: u64,
    /// Pinning fails.
    pub pin_fails: bool,
    /// Seeds the pinned pages' physical addresses.
    pub pin_seed: u64,
    /// The hooks.
    pub hooks: Hooks,
    /// A C allocation's canary was overwritten.
    pub canary: bool,
    /// What the Rust interpreter recorded, for the backend.
    pub shape: Option<I2Shape>,
    /// Registration ids handed out so far.
    pub next_id: u64,
    /// Replies to give, in order, instead of the fake backend's.
    pub canned: Vec<Vec<u8>>,
    /// Transport errors to give, in order, before any reply (-EINTR, say:
    /// the caller gave up on a request that did go out).
    pub fail: Vec<i32>,
    /// The ATOMIC special runs the atomic parse, with this fence bridge.
    pub atomic: Option<bool>,
    /// Out-fences taken in this call.
    pub nfence: u32,
}

impl World {
    /// The region holding all of `[addr, addr + n)`, and the offset in it.
    fn region(&mut self, addr: u64, n: usize) -> Option<(&mut Vec<u8>, usize)> {
        let (&base, r) = self.mem.range_mut(..=addr).next_back()?;
        let off = usize::try_from(addr - base).ok()?;
        (off.checked_add(n)? <= r.len()).then_some((r, off))
    }

    /// Whether `[addr, addr + n)` is all in the user half (the kernel's
    /// `access_ok()`; the shim's has the same bound).
    pub fn user_range(addr: u64, n: usize) -> bool {
        addr < USER_END && (n as u64) <= USER_END - addr
    }

    /// A kernel address's bytes, as the kernel's plain copy reads them (the
    /// `.kernel` / `.karg` paths). Kernel memory is the world's `mem` above
    /// the user half; one the test did not map is a test bug.
    pub fn kread(&mut self, dst: &mut [u8], src: u64) {
        if dst.is_empty() {
            return;
        }
        let (r, off) = self
            .region(src, dst.len())
            .unwrap_or_else(|| panic!("a kernel read of {src:#x} the test did not map"));
        dst.copy_from_slice(&r[off..off + dst.len()]);
    }

    pub fn kwrite(&mut self, dst: u64, src: &[u8]) {
        if src.is_empty() {
            return;
        }
        let (r, off) = self
            .region(dst, src.len())
            .unwrap_or_else(|| panic!("a kernel write of {dst:#x} the test did not map"));
        r[off..off + src.len()].copy_from_slice(src);
    }

    pub fn copy_from_user(&mut self, dst: &mut [u8], src: u64) -> bool {
        if dst.is_empty() {
            return true;
        }
        if !Self::user_range(src, dst.len()) {
            return false;
        }
        match self.region(src, dst.len()) {
            Some((r, off)) => {
                dst.copy_from_slice(&r[off..off + dst.len()]);
                true
            }
            None => false,
        }
    }

    pub fn copy_to_user(&mut self, dst: u64, src: &[u8]) -> bool {
        if src.is_empty() {
            return true;
        }
        if !Self::user_range(dst, src.len()) {
            return false;
        }
        match self.region(dst, src.len()) {
            Some((r, off)) => {
                r[off..off + src.len()].copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    pub fn handle_for_fd(&mut self, fd: i32) -> Result<u32, i32> {
        self.fds.get(&fd).copied().ok_or(-libc_ebadf())
    }

    pub fn clock_to_guest(&mut self, raw: bool, host: i64) -> Option<i64> {
        let off = self.clock?;
        Some(host.wrapping_add(if raw { off / 2 } else { off }))
    }

    /// Pin: the physical address of each page, from `pin_seed`, in runs.
    pub fn pin(&mut self, start: u64, n: u64, write: bool) -> Option<Vec<u64>> {
        self.events.push(Ev::Pin { start, n, write });
        if self.pin_fails {
            return None;
        }
        let mut rng = Rng::new(self.pin_seed ^ start);
        let mut pa = 0x1_0000_0000 + (rng.below(1 << 20) << 12);
        let mut v = Vec::with_capacity(n as usize);
        for _ in 0..n {
            v.push(pa);
            pa = if rng.chance(1, 4) {
                0x1_0000_0000 + (rng.below(1 << 20) << 12)
            } else {
                pa + 4096
            };
        }
        Some(v)
    }
}

/// EBADF, as the kernel numbers it.
pub fn libc_ebadf() -> i32 {
    9
}
