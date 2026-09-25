//! The dispatcher, v1 or v2 as the input's first byte says (device/src/fuzzing/backend.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::backend::run(data);
});
