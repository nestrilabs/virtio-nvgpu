// The sandbox, checked from inside one.
//
// `sandbox::enter` cannot be undone and must run single-threaded, so the
// checks run in a process of their own: `nvgpu-sandbox-selftest` enters the
// sandbox and tries each thing it is supposed to refuse, and each thing the
// backend still needs. Its exit status is the test.

#[test]
fn the_sandbox_refuses_what_it_says_it_refuses() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_nvgpu-sandbox-selftest"))
        .output()
        .expect("run the selftest binary");
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
