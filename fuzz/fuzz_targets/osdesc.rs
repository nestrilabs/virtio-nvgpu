// SPDX-License-Identifier: Apache-2.0
//! The OS-descriptor call, page list, guest RAM lookup and mapping (device/src/osdesc.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::parsers::os_descriptor(&mut device::fuzzing::Bytes::new(data));
});
