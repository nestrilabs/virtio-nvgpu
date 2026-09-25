//! The Rust core alone on raw bytes (debug assertions on, so an arithmetic
//! overflow is a crash here even where the kernel build would wrap): an
//! ioctl number, an argument and up to eight more regions of memory at
//! small fixed addresses a pointer in the fuzzer's bytes can name, the RM
//! paths and the IOCTL2 interpreter both.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nvgpu_guest_difftest::scen::{self, Call, DevSpec, Scenario, FDS};
use nvgpu_guest_difftest::world::{Hooks, World};

fuzz_target!(|data: &[u8]| {
    if data.len() < 12 {
        return;
    }
    let cmd = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let knobs = u32::from_le_bytes(data[4..8].try_into().unwrap());
    let mut w = World {
        fds: FDS.iter().copied().collect(),
        clock: Some(12345),
        hooks: Hooks { mask: knobs & 0x3f, seed: u64::from(knobs), fail_gem: None },
        chaos: u64::from((knobs >> 6) & 7),
        backend_seed: u64::from(knobs),
        ..World::default()
    };
    // Regions split at 0xff 0xfe markers: region k at 0x10000 * (k + 1).
    let mut k = 0u64;
    for chunk in data[8..].split(|&b| b == 0xfe).take(9) {
        w.mem.insert(0x10000 * (k + 1), chunk.to_vec());
        k += 1;
    }
    let dev = DevSpec {
        bad_schema: false,
        version: "610.57.04".into(),
        v2: knobs & (1 << 9) == 0,
        caps: (knobs >> 10) & 0x3e0,
        max_req: 1 << 20,
        max_resp: 1 << 20,
        fdt: vec![(0x27, 48)],
        handle: 5,
    };
    let call = match (knobs >> 20) & 3 {
        0 => Call::Fd { cmd, arg: 0x10000 },
        1 => Call::Uvm { cmd: cmd & 0xff, arg: 0x10000 },
        2 => Call::Modeset { cmd, arg: 0x10000 },
        _ => Call::I2 { sclass: 1 + (knobs >> 22) % 3, cmd, uarg: 0x10000, render: 5, xflags: 0 },
    };
    let s = Scenario { seed: 0, dev, world: w, call };
    let _ = scen::run_rust(&s);
});
