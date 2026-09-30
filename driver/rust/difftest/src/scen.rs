// SPDX-License-Identifier: GPL-2.0-only
//! Scenarios: a device, a world and one call, generated from a seed; running
//! one through each implementation; and comparing what each did.

use std::cell::RefCell;
use std::ffi::CString;
use std::rc::Rc;

use nvgpu_guest_core::guest::i2::{self, State};
use nvgpu_guest_core::guest::schema::{
    SField, SchemaSet, SCLASS_MODESET, SFF_COND, SF_ARRAY, SF_FD_IN, SF_GEM_IN, SF_PTR, SLEN_CONST,
    SLEN_COUNT, SLEN_NVKMS_PARAMS, SLEN_PLANES,
};
use nvgpu_guest_core::guest::{dispatch, rm};

use crate::cabi;
use crate::renv::{
    self, Dev, I2Env, RStore, BCAP_DEEP_SEGS, BCAP_OS_DESC, BCAP_PROC_EUID, BCAP_PROC_ID,
};
use crate::world::{Ev, Hooks, Rng, World, KARG};

/// The device a scenario runs on.
#[derive(Clone, Debug)]
pub struct DevSpec {
    /// The tests' bad table (harness.c) in place of the generated ones.
    pub bad_schema: bool,
    pub version: String,
    pub v2: bool,
    pub caps: u32,
    pub max_req: u32,
    pub max_resp: u32,
    pub fdt: Vec<(u32, u32)>,
    pub handle: u32,
}

/// The call.
#[derive(Clone, Debug)]
pub enum Call {
    /// `nvgpu_ioctl_fd()`.
    Fd { cmd: u32, arg: u64 },
    /// `nvgpu_uvm_ioctl_fd()`.
    Uvm { cmd: u32, arg: u64 },
    /// `nvgpu_ioctl_modeset()` (a v1 backend's NVKMS command).
    Modeset { cmd: u32, arg: u64 },
    /// `nvgpu_i2_ioctl()`. `karg`: `uarg` is a kernel address, the
    /// argument as the DRM node's entry copied it in
    /// (`nvgpu_i2_call.karg`), everything it points at the caller's.
    I2 {
        sclass: u32,
        cmd: u32,
        uarg: u64,
        render: u32,
        xflags: u32,
        karg: bool,
    },
}

#[derive(Clone, Debug)]
pub struct Scenario {
    pub seed: u64,
    pub dev: DevSpec,
    pub world: World,
    pub call: Call,
}

/// What one implementation did.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub ret: i64,
    /// IOCTL2: `call->ret` afterwards.
    pub call_ret: Option<i32>,
    pub world: World,
}

struct CDevice {
    dev: *mut cabi::CDev,
    nfd: *mut cabi::CFd,
}

impl CDevice {
    fn new(d: &DevSpec) -> CDevice {
        let v = CString::new(d.version.clone()).expect("version");
        let nr: Vec<u32> = d.fdt.iter().map(|x| x.0).collect();
        let pl: Vec<u32> = d.fdt.iter().map(|x| x.1).collect();
        let dev = unsafe {
            cabi::harness_dev(
                v.as_ptr(),
                d.v2,
                d.caps,
                d.max_req,
                d.max_resp,
                nr.as_ptr(),
                pl.as_ptr(),
                nr.len() as u32,
            )
        };
        if d.bad_schema {
            unsafe { cabi::harness_dev_bad_schema(dev) };
        }
        let nfd = unsafe { cabi::harness_fd(dev, d.handle) };
        CDevice { dev, nfd }
    }

    fn rdev(&self, d: &DevSpec) -> Dev {
        Dev {
            dev: self.dev,
            v2: d.v2,
            caps: d.caps,
            version: d.version.clone(),
            fdt: d.fdt.clone(),
            handle: d.handle,
        }
    }
}

impl Drop for CDevice {
    fn drop(&mut self) {
        unsafe {
            cabi::harness_fd_free(self.nfd);
            cabi::harness_dev_free(self.dev);
        }
    }
}

/// The schema set the C device for `d` selects.
pub fn tables_for(d: &DevSpec) -> SchemaSet<'static> {
    let cd = CDevice::new(d);
    renv::tables(&cd.rdev(d))
}

/// The native command both implementations normalise a DRM-node caller's
/// `cmd` to (`nvgpu_i2_native_cmd()`, `i2::native_cmd()`) on device `d`:
/// (C, Rust).
pub fn native_cmd(d: &DevSpec, sclass: u32, cmd: u32) -> (u32, u32) {
    let cd = CDevice::new(d);
    let set = renv::tables(&cd.rdev(d));
    let c = unsafe { cabi::nvgpu_i2_native_cmd(cd.dev, sclass, cmd) };
    (c, i2::native_cmd(&set, sclass, cmd))
}

/// Run the scenario through the Rust core.
pub fn run_rust(s: &Scenario) -> Outcome {
    let cd = CDevice::new(&s.dev);
    let d = cd.rdev(&s.dev);
    let mut w = s.world.clone();
    match s.call {
        Call::Fd { cmd, arg } => {
            let ret = dispatch::ioctl_fd(&mut renv::RmEnv { w: &mut w, d: &d }, cmd, arg);
            Outcome {
                ret: i64::from(ret),
                call_ret: None,
                world: w,
            }
        }
        Call::Uvm { cmd, arg } => {
            let ret = dispatch::uvm_ioctl(&mut renv::RmEnv { w: &mut w, d: &d }, cmd, arg);
            Outcome {
                ret: i64::from(ret),
                call_ret: None,
                world: w,
            }
        }
        Call::Modeset { cmd, arg } => {
            let ret = rm::modeset_v1(&mut renv::RmEnv { w: &mut w, d: &d }, cmd, arg);
            Outcome {
                ret: i64::from(ret),
                call_ret: None,
                world: w,
            }
        }
        Call::I2 {
            sclass,
            cmd,
            uarg,
            render,
            xflags,
            karg,
        } => {
            if !s.dev.v2 {
                return Outcome {
                    ret: -95,
                    call_ret: Some(0),
                    world: w,
                };
            }
            let set = renv::tables(&d);
            let compat = w.compat;
            let shared = Rc::new(RefCell::new(w));
            let mut st = Box::new(State::new(RStore::new(shared.clone(), karg)));
            let mut env = I2Env {
                w: shared.clone(),
                render,
            };
            let args = i2::Args {
                sclass,
                cmd,
                uarg,
                handle: s.dev.handle,
                render,
                xflags,
                compat,
                max_req: u64::from(s.dev.max_req),
                max_resp: u64::from(s.dev.max_resp),
            };
            let ret = i2::run(&mut env, &mut st, &set, &args);
            let call_ret = st.ret;
            drop(st);
            drop(env);
            let w = Rc::try_unwrap(shared)
                .expect("world still shared")
                .into_inner();
            Outcome {
                ret: i64::from(ret),
                call_ret: Some(call_ret),
                world: w,
            }
        }
    }
}

/// Run the scenario through the C, in `world` (the scenario's, with what
/// the Rust run recorded for the backend).
pub fn run_c(s: &Scenario, world: World) -> Outcome {
    let cd = CDevice::new(&s.dev);
    match s.call {
        Call::Fd { cmd, arg } => {
            let (ret, w) =
                cabi::in_world(world, || unsafe { cabi::nvgpu_ioctl_fd(cd.nfd, cmd, arg) });
            Outcome {
                ret,
                call_ret: None,
                world: w,
            }
        }
        Call::Uvm { cmd, arg } => {
            let (ret, w) = cabi::in_world(world, || unsafe {
                cabi::nvgpu_uvm_ioctl_fd(cd.nfd, cmd, arg)
            });
            Outcome {
                ret,
                call_ret: None,
                world: w,
            }
        }
        Call::Modeset { cmd, arg } => {
            let (ret, w) = cabi::in_world(world, || unsafe {
                cabi::nvgpu_ioctl_modeset(cd.nfd, cmd, arg)
            });
            Outcome {
                ret,
                call_ret: None,
                world: w,
            }
        }
        Call::I2 {
            sclass,
            cmd,
            uarg,
            render,
            xflags,
            karg,
        } => {
            let mask = world.hooks.mask;
            let mut call_ret = 0i32;
            let (ret, w) = cabi::in_world(world, || unsafe {
                cabi::harness_i2(
                    cd.dev,
                    sclass,
                    cmd,
                    uarg,
                    s.dev.handle,
                    render,
                    xflags,
                    false,
                    karg,
                    mask,
                    &mut call_ret,
                )
            });
            Outcome {
                ret,
                call_ret: Some(call_ret),
                world: w,
            }
        }
    }
}

/// A difference the port makes on purpose (see the report in
/// driver/rust/README.md), recognised so the rest still has to agree. None
/// is left: the C reads each block once too, and TIME_CORRELATION's TSC
/// refusal is decided on the whole block, as in the Rust.
pub fn intended(_s: &Scenario, _c: &Outcome, _r: &Outcome) -> Option<&'static str> {
    None
}

/// Run both and say how they differ, if they do.
pub fn diff(s: &Scenario) -> Result<Outcome, String> {
    let r = run_rust(s);
    let mut cw = s.world.clone();
    cw.shape = r.world.shape.clone();
    let c = run_c(s, cw);
    if c.world.canary {
        return Err(format!(
            "seed {:#x}: the C wrote past the end of an allocation",
            s.seed
        ));
    }
    if intended(s, &c, &r).is_some() {
        return Ok(r);
    }
    let mut why = Vec::new();
    if c.ret != r.ret {
        why.push(format!("returned C {} Rust {}", c.ret, r.ret));
    }
    if c.call_ret != r.call_ret {
        why.push(format!(
            "call->ret C {:?} Rust {:?}",
            c.call_ret, r.call_ret
        ));
    }
    if c.world.events != r.world.events {
        let i = c
            .world
            .events
            .iter()
            .zip(&r.world.events)
            .take_while(|(a, b)| a == b)
            .count();
        why.push(format!(
            "events differ at {} of {}/{}:\n  C    {:?}\n  Rust {:?}",
            i,
            c.world.events.len(),
            r.world.events.len(),
            c.world.events.get(i),
            r.world.events.get(i)
        ));
    }
    if c.world.mem != r.world.mem {
        for (a, (x, y)) in c
            .world
            .mem
            .iter()
            .zip(r.world.mem.values())
            .map(|((a, x), y)| (a, (x, y)))
        {
            if x != y {
                let j = x.iter().zip(y).take_while(|(p, q)| p == q).count();
                why.push(format!(
                    "memory at {a:#x}+{j} differs: C {:?} Rust {:?}",
                    &x[j..(j + 8).min(x.len())],
                    &y[j..(j + 8).min(y.len())]
                ));
                break;
            }
        }
    }
    if std::env::var_os("DIFFTEST_VERBOSE").is_some() {
        why.push(format!(
            "\n  C ret {} events {:#?}\n  Rust ret {} events {:#?}",
            c.ret, c.world.events, r.ret, r.world.events
        ));
    }
    if why.is_empty() {
        Ok(r)
    } else {
        Err(format!(
            "seed {:#x} {:?}: {}",
            s.seed,
            s.call,
            why.join("; ")
        ))
    }
}

// ───────────────────────── generation ─────────────────────────

/// The caller's memory being laid out: regions apart from each other.
pub struct Layout {
    next: u64,
    pub world: World,
}

impl Layout {
    pub fn new(world: World) -> Layout {
        Layout {
            next: 0x7f00_0000_0000,
            world,
        }
    }

    /// A region of `bytes`, somewhere; its address.
    pub fn put(&mut self, r: &mut Rng, bytes: Vec<u8>) -> u64 {
        let at = self.next + r.below(64) * 8 + if r.chance(1, 4) { r.below(8) } else { 0 };
        self.next = (at + bytes.len() as u64 + 0x1000 + 0xfff) & !0xfff;
        self.world.mem.insert(at, bytes);
        at
    }

    /// An address nothing is mapped at.
    pub fn hole(&mut self) -> u64 {
        self.next += 0x10000;
        self.next - 0x8000
    }

    /// A pointer to `len` fresh random bytes -- or, now and then, one that
    /// reaches only part of them, NULL, or nothing.
    pub fn ptr(&mut self, r: &mut Rng, len: usize) -> u64 {
        match r.below(20) {
            0 => 0,
            1 => self.hole(),
            2 if len > 0 => {
                let short = r.below(len as u64) as usize;
                let b = r.bytes(short);
                self.put(r, b)
            }
            _ => {
                let b = r.bytes(len);
                self.put(r, b)
            }
        }
    }
}

fn put(b: &mut [u8], off: usize, width: usize, v: u64) {
    if off + width <= b.len() {
        b[off..off + width].copy_from_slice(&v.to_le_bytes()[..width]);
    }
}

fn ioc(dir: u32, ty: u8, nr: u32, size: u32) -> u32 {
    (dir << 30) | (size << 16) | (u32::from(ty) << 8) | nr
}

pub const FDS: [(i32, u32); 5] = [(3, 101), (4, 102), (7, 103), (10, 200), (12, 201)];

/// A device and a world, from `r`.
pub fn base(r: &mut Rng, seed: u64) -> (DevSpec, World) {
    let version = r
        .pick(&[
            "610.57.04",
            "595.71.05",
            "580.178.04",
            "535.129.03",
            "615.71.09",
            "banana",
        ])
        .to_string();
    let mut caps = 0;
    for b in [BCAP_DEEP_SEGS, BCAP_PROC_ID, BCAP_OS_DESC, BCAP_PROC_EUID] {
        if r.chance(3, 4) {
            caps |= b;
        }
    }
    let mut fdt = Vec::new();
    if r.chance(3, 4) {
        fdt.push((0x27, 48));
    }
    if r.chance(1, 4) {
        fdt.push((0x4e, r.pick(&[0u32, 4, 8, 60])));
    }
    if r.chance(1, 2) {
        // UVM commands naming a file, packed as the backend states them.
        for _ in 0..r.below(4) {
            let cmd = 1 + r.below(0x50) as u32;
            let size = r.pick(&[16u32, 24, 40, 64]);
            let off = r.pick(&[0u32, 4, 8, 12, 60]);
            fdt.push((0x8000_0000 | cmd, (size << 16) | off));
        }
    }
    let dev = DevSpec {
        bad_schema: false,
        version,
        v2: r.chance(7, 8),
        caps,
        max_req: r.pick(&[1 << 20, 4 << 20, 64 << 10, 4096]),
        max_resp: r.pick(&[1 << 20, 4 << 20, 64 << 10, 4096]),
        fdt,
        handle: 1 + r.below(50) as u32,
    };
    let mut world = World {
        fds: FDS.iter().copied().collect(),
        backend_seed: r.next(),
        chaos: r.pick(&[0, 0, 1, 2, 4, 8]),
        pin_fails: r.chance(1, 10),
        pin_seed: r.next(),
        clock: if r.chance(3, 4) {
            Some(r.below(1 << 40) as i64 - (1 << 39))
        } else {
            None
        },
        compat: r.chance(1, 10),
        hooks: Hooks {
            mask: if r.chance(4, 5) {
                0x3f
            } else {
                r.below(64) as u32
            },
            seed: r.next(),
            fail_gem: None,
        },
        ..World::default()
    };
    world.proc_id.copy_from_slice(&r.bytes(16));
    let _ = seed;
    (dev, world)
}

fn some_fd(r: &mut Rng) -> i64 {
    r.pick(&[
        3i64,
        4,
        7,
        10,
        -1,
        -2,
        -5,
        99,
        0,
        1000,
        i64::from(i32::MAX) + 5,
    ])
}

/// An RM escape, UVM command or v1 NVKMS command.
pub fn gen_rm(seed: u64) -> Scenario {
    gen_rm_from(Rng::new(seed), seed)
}

/// [`gen_rm`], drawing from `r` (the fuzzer's bytes).
pub fn gen_rm_from(mut r: Rng, seed: u64) -> Scenario {
    let (dev, world) = base(&mut r, seed);
    let mut m = Layout::new(world);
    let call = match r.below(12) {
        0..=2 => gen_control(&mut r, &mut m),
        3 | 4 => gen_alloc(&mut r, &mut m),
        5 => gen_idle(&mut r, &mut m),
        6 => gen_alloc_memory(&mut r, &mut m),
        7 => gen_vid_heap(&mut r, &mut m),
        8 => {
            // UVM.
            let cmd = if r.chance(1, 2) {
                1 + r.below(0x50) as u32
            } else {
                r.next() as u32
            };
            let mut b = r.bytes(128);
            for o in [0usize, 4, 8, 12, 60] {
                if r.chance(1, 3) {
                    put(&mut b, o, 4, some_fd(&mut r) as u64);
                }
            }
            let arg = m.put(&mut r, b);
            Call::Uvm { cmd, arg }
        }
        9 => {
            // A v1 backend's NVKMS command.
            let mut outer = vec![0u8; 16];
            let any = r.next() as u32;
            let cmd = r.pick(&[16u32, 17, 3, any]);
            let size = r.pick(&[0u32, 8, 24, 64, 200, 2 << 20]);
            put(&mut outer, 0, 4, u64::from(cmd));
            put(&mut outer, 4, 4, u64::from(size));
            let mut nested = r.bytes(size.min(4096) as usize);
            // useFd, one byte, and the padding after it, which is not
            // read (garbage in it must not make useFd true).
            put(&mut nested, 4, 1, r.below(3));
            if r.chance(1, 3) {
                put(&mut nested, 5, 3, r.below(1 << 24));
            }
            put(&mut nested, 16, 4, some_fd(&mut r) as u64);
            let p = if r.chance(1, 8) {
                0
            } else {
                m.put(&mut r, nested)
            };
            put(&mut outer, 8, 8, p);
            let arg = m.put(&mut r, outer);
            Call::Modeset {
                cmd: ioc(3, 0x6d, 0, r.pick(&[16, 16, 8])),
                arg,
            }
        }
        _ => {
            // Anything else: flat, or a descriptor at a fixed offset.
            let any = r.below(256) as u32;
            let nr = r.pick(&[0x34u32, 0x29, 0x4e, 0x27, 0x41, 0x2a, 0x2b, any]);
            let ty = if r.chance(4, 5) { b'F' } else { b'd' };
            let size = r.pick(&[0u32, 4, 16, 32, 48, 56, 64, 256]);
            let mut b = r.bytes(size as usize);
            for o in [0usize, 4, 48, 60] {
                if r.chance(1, 3) {
                    put(&mut b, o, 4, some_fd(&mut r) as u64);
                }
            }
            let arg = if r.chance(1, 10) {
                m.hole()
            } else {
                m.put(&mut r, b)
            };
            Call::Fd {
                cmd: ioc(r.pick(&[3, 1, 2, 0]), ty, nr, size),
                arg,
            }
        }
    };
    Scenario {
        seed,
        dev,
        world: m.world,
        call,
    }
}

/// RM controls with something to say about their parameters.
const CONTROLS: &[u32] = &[
    0x0000_0101, // GET_BUILD_VERSION
    0x2080_0406, // TIME_CORRELATION
    0x0000_3d05,
    0x0000_3d06,
    0x0000_3d08,
    0x0000_3d0a,
    0x0000_3d0b,
    0x0000_3d0c, // OS_UNIX
    0x00da_0003,
    0x00da_0005, // semaphore surface waiters
    0x0080_170d,
    0x0000_0127, // deep segments
    0x0080_1102,
    0x2080_0101,
    0x0000_0202,
    0x2080_0803,
    0xa0bc_0101, // V1V2 table
    0x0041_0110, // deep-only
    0x2080_0110,
    0x0000_0000,
];

fn gen_control(r: &mut Rng, m: &mut Layout) -> Call {
    let ctl = if r.chance(9, 10) {
        r.pick(CONTROLS)
    } else {
        r.next() as u32
    };
    let size = match ctl {
        0x0000_0101 => 40,
        0x2080_0406 => 8 + 16 * 16,
        0x0000_0127 => 176,
        0x2080_0803 => 1308,
        _ => r.pick(&[8usize, 16, 24, 32, 64, 80]),
    };
    let mut n = r.bytes(size);
    for x in n.iter_mut() {
        if r.chance(1, 2) {
            *x &= 0x7;
        }
    }
    match ctl {
        0x0000_0101 => {
            put(&mut n, 0, 4, r.pick(&[0u64, 31, 32, 64, 10]));
            for o in [8, 16, 24] {
                let p = if r.chance(1, 3) { 0 } else { m.ptr(r, 64) };
                put(&mut n, o, 8, p);
            }
        }
        0x2080_0406 => {
            n[0] = r.pick(&[0x01u8, 0x02, 0x03, 0x11, 0x13, 0x0f]);
            n[1] = r.pick(&[0u8, 1, 2, 16, 17, 255]);
            for i in 0..16 {
                let v = if r.chance(1, 4) {
                    r.below(1000)
                } else {
                    r.below(1 << 50)
                };
                put(&mut n, 8 + i * 16, 8, v);
            }
        }
        0x0000_3d05 | 0x0000_3d06 | 0x0000_3d08 | 0x0000_3d0a | 0x0000_3d0b | 0x0000_3d0c => {
            if n.len() < 80 {
                n.resize(80, 0);
            }
            for o in [0usize, 16, 72] {
                put(&mut n, o, 4, some_fd(r) as u64);
            }
        }
        0x00da_0003 | 0x00da_0005 => {
            if n.len() < 32 {
                n.resize(32, 0);
            }
            for o in [16usize, 24] {
                put(
                    &mut n,
                    o,
                    8,
                    r.pick(&[0u64, 3, 4, 99, 1 << 40, 0x7fff_ffff, 0x8000_0000]),
                );
            }
        }
        0x0080_170d => {
            let c = r.pick(&[0u64, 1, 3, 17]);
            put(&mut n, 0, 4, c);
            for o in [8, 16] {
                let p = m.ptr(r, (c * 4) as usize);
                put(&mut n, o, 8, p);
            }
        }
        0x0000_0127 => {
            let c = r.pick(&[0u64, 1, 2, 4, 0x1_0000, 0x1_0001, 0x1_0003]);
            put(&mut n, 128, 4, c);
            for o in [160, 168] {
                let p = m.ptr(r, (c * c * 4).min(1 << 16) as usize);
                put(&mut n, o, 8, p);
            }
        }
        _ => {
            // V1V2 and deep-only: a count and a pointer at 8, 16 or 1048.
            let c = r.pick(&[0u64, 1, 3, 12, 0x2000_0001, 70_000]);
            put(&mut n, 0, 4, c);
            for o in [8usize, 16, 24, 1048] {
                if r.chance(1, 2) {
                    let p = m.ptr(r, (c.min(512) * 8) as usize);
                    put(&mut n, o, 8, p);
                }
            }
        }
    }
    let mut p = vec![0u8; 32];
    put(&mut p, 0, 4, r.next());
    put(&mut p, 4, 4, r.next());
    put(&mut p, 8, 4, u64::from(ctl));
    let nsize = match r.below(10) {
        0 => 0,
        1 => r.below(size as u64 * 2) as usize,
        2 => (1 << 20) + 1,
        _ => size,
    };
    let np = if r.chance(1, 12) {
        0
    } else if r.chance(1, 12) {
        m.hole()
    } else {
        m.put(r, n)
    };
    put(&mut p, 16, 8, np);
    put(&mut p, 24, 4, nsize as u64);
    let arg = m.put(r, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x2a, r.pick(&[32, 32, 32, 16, 48])),
        arg,
    }
}

fn gen_alloc(r: &mut Rng, m: &mut Layout) -> Call {
    let any = r.next() as u32;
    let class = r.pick(&[0x05u32, 0x79, 0x90cd, 0x71, 0x80, 0x2080, 0x3e, 0xc56f, any]);
    let size = match class {
        0x05 | 0x79 => 24,
        0x90cd => 64,
        0x71 => 40,
        _ => r.pick(&[4usize, 56, 128, 512]),
    };
    let mut n = r.bytes(size);
    match class {
        0x05 | 0x79 => put(&mut n, 16, 4, some_fd(r) as u64),
        0x90cd => put(&mut n, 40, 8, r.pick(&[0u64, 3, 4, 99, 1 << 33])),
        0x71 => {
            put(&mut n, 32, 4, r.pick(&[0u64, 0, 1]));
            let va = r.pick(&[0x1000u64, 0x7000_1234, 0, u64::MAX - 0x100]);
            put(&mut n, 16, 8, va);
            put(
                &mut n,
                24,
                8,
                r.pick(&[0u64, 0xfff, 0x5000, u64::MAX - 1, u64::MAX]),
            );
        }
        _ => {}
    }
    let mut p = vec![0u8; 48];
    put(&mut p, 0, 4, r.next());
    put(&mut p, 12, 4, u64::from(class));
    let np = if r.chance(1, 10) { 0 } else { m.put(r, n) };
    put(&mut p, 16, 8, np);
    put(
        &mut p,
        32,
        4,
        r.pick(&[0u64, size as u64, 40, 3, (1 << 20) + 5]),
    );
    let arg = m.put(r, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x2b, r.pick(&[48, 48, 48, 44, 64])),
        arg,
    }
}

fn gen_idle(r: &mut Rng, m: &mut Layout) -> Call {
    let mut p = r.bytes(56);
    let count = r.pick(&[0u64, 1, 3, 4096, 4097]);
    put(&mut p, 12, 4, count);
    put(&mut p, 40, 4, r.pick(&[0u64, 0x10, 0x100, 0xf0]));
    for o in [16, 24, 32] {
        let q = m.ptr(r, (count.min(64) * 4) as usize);
        put(&mut p, o, 8, q);
    }
    let arg = m.put(r, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x41, r.pick(&[56, 56, 48])),
        arg,
    }
}

fn osdesc_range(r: &mut Rng, m: &mut Layout) -> (u64, u64) {
    let va = match r.below(5) {
        0 => m.hole(),
        1 => 0x7000_0000_1234,
        2 => u64::MAX - 0x1000,
        3 => r.below(4096),
        _ => r.below(1 << 47),
    };
    // Sizes to the end of the address space and just short of it: the page
    // count must not round to zero.
    let limit = r.pick(&[
        0u64,
        0xfff,
        0x1000,
        0x3_0000,
        0x7f_ffff,
        (1 << 32) - 1,
        u64::MAX - 1,
        u64::MAX,
        u64::MAX - va,
        u64::MAX - va - 1,
        (u64::MAX - va).saturating_sub(4096),
    ]);
    (va, limit)
}

fn gen_alloc_memory(r: &mut Rng, m: &mut Layout) -> Call {
    let mut p = r.bytes(56);
    put(&mut p, 12, 4, if r.chance(4, 5) { 0x71 } else { 0x3e });
    put(&mut p, 16, 4, if r.chance(1, 3) { 1 << 21 } else { 0 });
    let (va, limit) = osdesc_range(r, m);
    put(&mut p, 24, 8, va);
    put(&mut p, 32, 8, limit);
    put(&mut p, 48, 4, some_fd(r) as u64);
    let arg = m.put(r, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x27, r.pick(&[56, 56, 48])),
        arg,
    }
}

fn gen_vid_heap(r: &mut Rng, m: &mut Layout) -> Call {
    let mut p = r.bytes(184);
    put(&mut p, 8, 4, if r.chance(4, 5) { 27 } else { 1 });
    put(&mut p, 56, 4, if r.chance(1, 3) { 1 << 22 } else { 0 });
    let (va, limit) = osdesc_range(r, m);
    put(&mut p, 64, 8, va);
    put(&mut p, 72, 8, limit);
    put(&mut p, 80, 4, if r.chance(4, 5) { 0 } else { 2 });
    let arg = m.put(r, p);
    Call::Fd {
        cmd: ioc(3, b'F', 0x4a, r.pick(&[184, 184, 180])),
        arg,
    }
}

/// An IOCTL2 call, laid out by the schema: pointers to buffers of the
/// lengths their counts say (mostly), descriptors ours and not, conditions
/// holding and not.
pub fn gen_i2(seed: u64) -> Scenario {
    gen_i2_from(Rng::new(seed), seed)
}

/// [`gen_i2`], drawing from `r` (the fuzzer's bytes).
pub fn gen_i2_from(mut r: Rng, seed: u64) -> Scenario {
    let (mut dev, mut world) = base(&mut r, seed);
    dev.v2 = r.chance(15, 16);
    world.chaos = r.pick(&[0, 0, 0, 1, 2, 4]);
    let cd = CDevice::new(&dev);
    let set = renv::tables(&cd.rdev(&dev));
    drop(cd);
    let mut m = Layout::new(world);

    let modeset = set.modeset.is_some() && r.chance(1, 3);
    let t = if modeset {
        set.modeset.unwrap()
    } else {
        set.drm
    };
    let e = t.ioctls[r.below(t.ioctls.len() as u64) as usize];
    let mut cmd = e.cmd;
    let mut sclass = u32::from(e.sclass);
    if r.chance(1, 20) {
        // A size or direction the schema does not have.
        cmd ^= r.pick(&[1u32 << 16, 1 << 30, 1 << 31]);
    }
    if r.chance(1, 20) {
        // A caller's struct from older or newer headers: shorter or longer
        // than the schema's. The DRM node normalises it before the
        // interpreter (nvgpu_drm_arg_in()); the interpreter itself still
        // takes the native size only.
        let size = (cmd >> 16) & 0x3fff;
        let other = if r.chance(1, 2) {
            size.saturating_sub(8)
        } else {
            (size + 8).min(0x3fff)
        };
        cmd = (cmd & !(0x3fff << 16)) | (other << 16);
    }
    if r.chance(1, 40) {
        sclass = r.pick(&[1u32, 2, 3]);
    }
    let size = ((cmd >> 16) & 0x3fff) as usize;
    let mut arg = r.bytes(size);
    for x in arg.iter_mut() {
        if r.chance(3, 4) {
            *x &= 1;
        }
    }
    if modeset && sclass == SCLASS_MODESET {
        put(
            &mut arg,
            0,
            4,
            u64::from(if r.chance(9, 10) {
                e.nvkms_cmd
            } else {
                r.below(40) as u32
            }),
        );
    }
    fill(
        &mut r, &mut m, &set, modeset, &mut arg, 0, e.field, e.nfield, 0,
    );
    // Now and then the argument is the DRM node entry's kernel copy
    // (.karg): buffer 0 read and written as kernel memory, what it points
    // at the caller's -- or, rarely, a user address, which the kernel copy
    // refuses (-EFAULT).
    let karg = r.chance(1, 6);
    let uarg = if karg {
        if r.chance(1, 20) {
            m.put(&mut r, arg)
        } else {
            m.world.mem.insert(KARG, arg);
            KARG
        }
    } else if r.chance(1, 30) {
        m.hole()
    } else {
        m.put(&mut r, arg)
    };
    let render = if r.chance(1, 2) {
        dev.handle
    } else {
        dev.handle + 1
    };
    let xflags = r.below(2) as u32;
    Scenario {
        seed,
        dev,
        world: m.world,
        call: Call::I2 {
            sclass,
            cmd,
            uarg,
            render,
            xflags,
            karg,
        },
    }
}

/// Lay out the fields `first..first + n` of the struct at `base` in `b`.
#[allow(clippy::too_many_arguments)]
fn fill(
    r: &mut Rng,
    m: &mut Layout,
    set: &SchemaSet<'static>,
    modeset: bool,
    b: &mut [u8],
    base: usize,
    first: u16,
    n: u16,
    depth: u32,
) {
    let t = if modeset {
        set.modeset.unwrap()
    } else {
        set.drm
    };
    if depth > 5 {
        return;
    }
    for i in 0..n as usize {
        let Some(f) = t.fields.get(first as usize + i).copied() else {
            return;
        };
        let at = base + f.off as usize;
        if f.flags & SFF_COND != 0 {
            let v = if r.chance(2, 3) {
                f.cond_value
            } else {
                r.next() as u32 & f.cond_mask
            };
            let cur = b
                .get(base + f.cond_off as usize..base + f.cond_off as usize + 4)
                .map_or(0, |s| u32::from_le_bytes(s.try_into().unwrap()));
            put(
                b,
                base + f.cond_off as usize,
                4,
                u64::from((cur & !f.cond_mask) | v),
            );
        }
        match f.kind {
            SF_PTR => fill_ptr(r, m, set, modeset, b, base, &f, depth),
            SF_ARRAY => {
                let mut ne = f.count as u64;
                if f.len_kind == SLEN_COUNT {
                    ne = if r.chance(1, 4) {
                        u64::from(f.count) + r.below(3)
                    } else {
                        r.below(u64::from(f.count) + 1)
                    };
                    put(b, base + f.len_a as usize, f.len_width as usize, ne);
                } else if f.len_kind == SLEN_PLANES {
                    put(
                        b,
                        base + f.len_a as usize,
                        4,
                        r.below(t.planes.len() as u64 + 2),
                    );
                }
                for e in 0..ne.min(u64::from(f.count)) {
                    fill(
                        r,
                        m,
                        set,
                        modeset,
                        b,
                        at + (e as usize) * f.stride as usize,
                        f.child,
                        f.nchild,
                        depth + 1,
                    );
                }
            }
            SF_FD_IN => {
                let v = if r.chance(1, 4) {
                    i64::from(f.none_value)
                } else {
                    some_fd(r)
                };
                put(b, at, f.width as usize, v as u64);
            }
            SF_GEM_IN => put(b, at, 4, r.pick(&[0u64, 1, 2, 3, 14, 22, 33])),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_ptr(
    r: &mut Rng,
    m: &mut Layout,
    set: &SchemaSet<'static>,
    modeset: bool,
    b: &mut [u8],
    base: usize,
    f: &SField,
    depth: u32,
) {
    let at = base + f.off as usize;
    let len: u64 = match f.len_kind {
        SLEN_CONST => u64::from(f.len_a),
        SLEN_COUNT => {
            let most = u64::from(f.max) / u64::from(f.len_elem.max(1));
            let n = match r.below(14) {
                0 => r.below(1 << 20),
                1 => 0,
                // Either side of the most the schema allows.
                2 => most + r.below(2),
                3 => most.saturating_sub(r.below(2)),
                _ => r.below(5),
            };
            put(b, base + f.len_a as usize, f.len_width as usize, n);
            n.saturating_mul(u64::from(f.len_elem))
        }
        SLEN_NVKMS_PARAMS => {
            let n = if r.chance(9, 10) {
                u64::from(f.max)
            } else {
                r.below(u64::from(f.max) + 16)
            };
            put(b, base + 4, 4, n);
            n
        }
        _ => r.below(64),
    };
    if f.cb_kind != 0 && r.chance(1, 3) {
        put(b, base + f.cb_off as usize, f.cb_width as usize, r.below(6));
    }
    if len > (1 << 16) {
        put(b, at, 8, m.hole());
        return;
    }
    let mut child = r.bytes(len as usize);
    for x in child.iter_mut() {
        if r.chance(3, 4) {
            *x &= 1;
        }
    }
    if f.nchild != 0 && f.stride != 0 {
        for e in 0..(len / u64::from(f.stride)).min(8) {
            fill(
                r,
                m,
                set,
                modeset,
                &mut child,
                (e * u64::from(f.stride)) as usize,
                f.child,
                f.nchild,
                depth + 1,
            );
        }
    }
    let p = match r.below(16) {
        0 => 0,
        1 => m.hole(),
        2 if !child.is_empty() => {
            let k = r.below(child.len() as u64) as usize;
            child.truncate(k);
            m.put(r, child)
        }
        _ => m.put(r, child),
    };
    put(b, at, 8, p);
}

/// Every event of kind `f` in a world.
pub fn count(w: &World, f: impl Fn(&Ev) -> bool) -> usize {
    w.events.iter().filter(|e| f(e)).count()
}

/// An atomic commit: objects, their property counts, properties and values
/// laid out for the schema walk, the ATOMIC special running the parse.
pub fn gen_atomic(seed: u64) -> Scenario {
    gen_atomic_from(Rng::new(seed), seed)
}

/// [`gen_atomic`], drawing from `r` (the fuzzer's bytes).
pub fn gen_atomic_from(mut r: Rng, seed: u64) -> Scenario {
    let (mut dev, mut world) = base(&mut r, seed);
    dev.v2 = true;
    dev.max_req = 1 << 20;
    dev.max_resp = 1 << 20;
    world.chaos = r.pick(&[0, 0, 1, 2]);
    world.atomic = Some(r.chance(3, 4));
    // The special always: ATOMIC's parse is what is under test.
    world.hooks.mask |= 1 << 4;
    let mut m = Layout::new(world);

    // Now and then a big one: more CRTC_IDs than a commit may teach (64).
    let big = r.chance(1, 8);
    let count = match r.below(10) {
        _ if big => 20 + r.below(40) as u32,
        0 => 0,
        1 => r.below(40) as u32,
        _ => 1 + r.below(6) as u32,
    };
    let mut objs = Vec::new();
    let mut cps = Vec::new();
    for _ in 0..count {
        let any = r.below(1 << 20) as u32;
        let obj = if big {
            r.pick(&[1u32, 2, 4, 5])
        } else {
            r.pick(&[1u32, 2, 3, 4, 5, 6, 9, 12, 13, 0xdead, any])
        };
        objs.extend_from_slice(&obj.to_le_bytes());
        let n = match r.below(12) {
            _ if big => 1 + r.below(8) as u32,
            0 => r.below(20) as u32,
            1 => 0,
            _ => r.below(4) as u32,
        };
        cps.extend_from_slice(&n.to_le_bytes());
    }
    let sum: u64 = cps
        .chunks(4)
        .map(|c| u64::from(u32::from_le_bytes(c.try_into().unwrap())))
        .sum();
    let mut props = Vec::new();
    let mut vals = Vec::new();
    for _ in 0..sum.min(4096) {
        let any = r.below(1000) as u32;
        let id = if big {
            r.pick(&[1u32, 5, 9, 2, 4, 8])
        } else {
            r.pick(&[1u32, 2, 3, 4, 5, 6, 7, 8, 21, 32, any])
        };
        props.extend_from_slice(&id.to_le_bytes());
        let v: u64 = match r.below(9) {
            0 => 0,
            1 => u64::MAX,
            2 => r.pick(&[3u64, 4, 7, 10, 99, (-5i64) as u64, 1 << 35]),
            3 => m.hole(),
            4 | 5 => {
                let b = r.bytes(4);
                m.put(&mut r, b)
            }
            _ => r.pick(&[1u64, 2, 3, 5, 6, 13, 30]),
        };
        vals.extend_from_slice(&v.to_le_bytes());
    }
    let mut a = vec![0u8; 56];
    let flags = r.pick(&[0u32, 1, 0x100, 0x101, 0x201, 0x200]);
    put(&mut a, 0, 4, u64::from(flags));
    let wrong = r.chance(1, 12);
    put(
        &mut a,
        4,
        4,
        u64::from(if wrong { count + 1 } else { count }),
    );
    let ptr = |r: &mut Rng, m: &mut Layout, b: Vec<u8>| -> u64 {
        match r.below(20) {
            0 => 0,
            1 => m.hole(),
            _ => m.put(r, b),
        }
    };
    let p1 = ptr(&mut r, &mut m, objs);
    let p2 = ptr(&mut r, &mut m, cps);
    let p3 = ptr(&mut r, &mut m, props);
    let p4 = ptr(&mut r, &mut m, vals);
    put(&mut a, 8, 8, p1);
    put(&mut a, 16, 8, p2);
    put(&mut a, 24, 8, p3);
    put(&mut a, 32, 8, p4);
    put(&mut a, 48, 8, r.next());
    let uarg = m.put(&mut r, a);
    Scenario {
        seed,
        dev,
        world: m.world,
        call: Call::I2 {
            sclass: 2,
            cmd: 0xc038_64bc,
            uarg,
            render: 5,
            xflags: 0,
            karg: false,
        },
    }
}
