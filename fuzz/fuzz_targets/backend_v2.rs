//! The dispatcher with a v2 session always said HELLO, compute and guest RAM on: IOCTL2, HOST_OP, WATCH, OS descriptors, the UVM aperture.
#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    device::fuzzing::backend::run_v2(data);
});
