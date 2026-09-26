// SPDX-License-Identifier: Apache-2.0
//! The capture-injection socket's packets, the registry's checks and
//! INJECT_OPEN (device/src/fuzzing/inject.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::inject::run(&mut device::fuzzing::Bytes::new(data));
});
