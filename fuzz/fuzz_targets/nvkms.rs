//! The NVKMS policy on v1 messages (device/src/nvkms.rs).
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::parsers::nvkms_v1(&mut device::fuzzing::Bytes::new(data));
});
