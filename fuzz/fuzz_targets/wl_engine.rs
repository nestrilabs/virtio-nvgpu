// SPDX-License-Identifier: Apache-2.0
//! The Wayland engine, both ends, both directions (device/src/fuzzing/wayland.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::wayland::run(data);
});
