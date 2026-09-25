//! ATOMIC: the C atomic parse (nvgpu_atomic.c) and the Rust one must agree
//! on a commit the fuzzer's bytes lay out, through both interpreters.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nvgpu_guest_difftest::scen;
use nvgpu_guest_difftest::world::{hash, Rng};

fuzz_target!(|data: &[u8]| {
    let s = scen::gen_atomic_from(Rng::fuzz(data), hash(0, data));
    if let Err(e) = scen::diff(&s) {
        panic!("{e}");
    }
});
