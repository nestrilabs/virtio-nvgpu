// SPDX-License-Identifier: Apache-2.0
//! Fence argument rewrites, uevents, KMS property names.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::parsers::small(&mut device::fuzzing::Bytes::new(data));
});
