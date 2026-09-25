//! Deep segments (device/src/deepseg.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::parsers::deep_segments(&mut device::fuzzing::Bytes::new(data));
});
