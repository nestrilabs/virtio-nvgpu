// SPDX-License-Identifier: Apache-2.0
//! The pointer scrub of RM escapes, controls and UVM, and IDLE_CHANNELS lists (device/src/guestptr.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut b = device::fuzzing::Bytes::new(data);
    if b.u8() & 1 == 0 {
        device::fuzzing::parsers::pointer_scrub(&mut b)
    } else {
        device::fuzzing::parsers::idle_channels(&mut b)
    }
});
