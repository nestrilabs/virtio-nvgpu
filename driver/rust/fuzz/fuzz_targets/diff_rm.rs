// SPDX-License-Identifier: GPL-2.0-only
//! RM escapes, UVM and v1 NVKMS commands: the C and the Rust must agree on a
//! scenario the fuzzer's bytes lay out.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nvgpu_guest_difftest::scen;
use nvgpu_guest_difftest::world::{hash, Rng};

fuzz_target!(|data: &[u8]| {
    let s = scen::gen_rm_from(Rng::fuzz(data), hash(0, data));
    if let Err(e) = scen::diff(&s) {
        panic!("{e}");
    }
});
