// SPDX-License-Identifier: Apache-2.0
//! RM share/duplicate parsing, the named-client tables, RM controls answered locally (device/src/rmshare.rs, rmctl.rs), and the ownership state.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    let mut b = device::fuzzing::Bytes::new(data);
    if b.u8() & 1 == 0 {
        device::fuzzing::parsers::rm_share(&mut b)
    } else {
        device::fuzzing::parsers::ownership(&mut b)
    }
});
