//! The channel frame decoder and the Wayland wire decoder.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::wayland::codec(data);
});
