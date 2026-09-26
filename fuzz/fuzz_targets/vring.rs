// SPDX-License-Identifier: Apache-2.0
//! The control queue as the guest writes it: virtqueue walk, chain layout, gather, serve, scatter (device/src/fuzzing/vring.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::vring::run(data);
});
