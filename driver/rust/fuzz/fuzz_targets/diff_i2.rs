// SPDX-License-Identifier: GPL-2.0-only
//! IOCTL2: the C interpreter and the Rust one must agree on a call the
//! fuzzer's bytes lay out by the schema.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nvgpu_guest_difftest::scen;
use nvgpu_guest_difftest::world::{hash, Rng};

fuzz_target!(|data: &[u8]| {
    let s = scen::gen_i2_from(Rng::fuzz(data), hash(0, data));
    if let Err(e) = scen::diff(&s) {
        panic!("{e}");
    }
});
