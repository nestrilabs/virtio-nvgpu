// SPDX-License-Identifier: Apache-2.0
//
// Enter the backend's sandbox and check that it refuses what it claims to.
//
// A binary rather than a test because the sandbox is a property of a whole
// process: `sandbox::enter` must run while the process is single-threaded and
// cannot be undone, and a test harness has a thread of its own. Exit status 0
// means every check held; `device/tests/sandbox.rs` runs this and reads it.
//
// Useful on its own: run it on a host to see whether that kernel can carry the
// backend's sandbox at all, without a GPU, a guest or a VMM.

fn main() -> std::process::ExitCode {
    // Landlock refuses an unprivileged caller that has not set this; the
    // backend's `posture::enforce` sets it before anything else.
    // SAFETY: prctl with integer arguments.
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };

    let dir = std::env::temp_dir().join(format!("nvgpu-sandbox-{}", std::process::id()));
    if let Err(e) =
        std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(dir.join("allowed"), b"x"))
    {
        eprintln!("could not make a directory to allow: {e}");
        return std::process::ExitCode::FAILURE;
    }

    let report = match device::sandbox::enter(&device::sandbox::Paths {
        devices: vec![],
        read_only: vec![dir.clone()],
        sockets: vec![],
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the sandbox did not go on: {e}");
            let _ = std::fs::remove_dir_all(&dir);
            return std::process::ExitCode::FAILURE;
        }
    };

    let bad = device::sandbox::selftest(&dir);
    // Landlock is already on, so this removes nothing outside the ruleset.
    let _ = std::fs::remove_dir_all(&dir);

    if bad.is_empty() {
        println!(
            "sandbox: landlock ABI {}, {} seccomp instructions; every check held",
            report.landlock_abi, report.seccomp_rules
        );
        std::process::ExitCode::SUCCESS
    } else {
        for what in &bad {
            eprintln!("the sandbox did not: {what}");
        }
        std::process::ExitCode::FAILURE
    }
}
