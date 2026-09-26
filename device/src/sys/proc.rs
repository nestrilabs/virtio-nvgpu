// SPDX-License-Identifier: Apache-2.0
//! The process: identity, privileges, limits, and the sandbox's system calls
//! (namespaces, Landlock, seccomp). Every function takes plain values or
//! locals it builds itself; none hands the kernel memory of its caller's
//! beyond the slices passed.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

fn cvt_l(r: libc::c_long) -> io::Result<libc::c_long> {
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

// ─────────────────────────────── identity ───────────────────────────────

pub fn uid() -> u32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::getuid() }
}

pub fn euid() -> u32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::geteuid() }
}

pub fn egid() -> u32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::getegid() }
}

pub fn pid() -> i32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::getpid() }
}

pub fn ppid() -> i32 {
    // SAFETY: no arguments; cannot fail.
    unsafe { libc::getppid() }
}

/// `kill(pid, sig)`.
pub fn kill(pid: i32, sig: i32) -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::kill(pid, sig) }).map(|_| ())
}

/// `umask(mode)`: the previous mask.
pub fn umask(mode: u32) -> u32 {
    // SAFETY: integer argument; cannot fail.
    unsafe { libc::umask(mode as libc::mode_t) as u32 }
}

/// Now on `clock`, in nanoseconds.
pub fn clock_ns(clock: libc::clockid_t) -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live local timespec the kernel writes.
    unsafe { libc::clock_gettime(clock, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// End the process now, with no unwinding and no exit handlers.
pub fn exit_now(code: i32) -> ! {
    // SAFETY: `_exit` never returns and touches no memory of the process.
    unsafe { libc::_exit(code) }
}

// ─────────────────────────────── privileges ───────────────────────────────

/// `unshare(flags)`.
pub fn unshare(flags: i32) -> io::Result<()> {
    // SAFETY: integer argument.
    cvt(unsafe { libc::unshare(flags) }).map(|_| ())
}

/// PR_GET_DUMPABLE.
pub fn dumpable() -> i32 {
    // SAFETY: integer arguments.
    unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) }
}

/// PR_SET_DUMPABLE.
pub fn set_dumpable(on: bool) -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, libc::c_ulong::from(on), 0, 0, 0) })
        .map(|_| ())
}

/// PR_GET_NO_NEW_PRIVS.
pub fn no_new_privs() -> io::Result<bool> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) }).map(|v| v == 1)
}

/// PR_SET_NO_NEW_PRIVS.
pub fn set_no_new_privs() -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) }).map(|_| ())
}

/// PR_CAP_AMBIENT_CLEAR_ALL. Anyone may; a failure changes nothing.
pub fn clear_ambient_caps() {
    // SAFETY: integer arguments.
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
}

/// PR_CAPBSET_DROP of `cap`; `false` past the kernel's last capability.
pub fn capbset_drop(cap: u32) -> bool {
    // SAFETY: integer arguments.
    unsafe { libc::prctl(libc::PR_CAPBSET_DROP, libc::c_ulong::from(cap), 0, 0, 0) == 0 }
}

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

/// One 32-bit half of each set: (effective, permitted, inheritable).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The calling thread's (effective, permitted, inheritable) sets, 64 bits
/// each.
pub fn capget() -> io::Result<(u64, u64, u64)> {
    let mut hdr = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let mut data = [CapData::default(); 2];
    // SAFETY: capget with a v3 header writes two CapData, which `data` holds.
    cvt_l(unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut hdr as *mut CapHeader,
            data.as_mut_ptr(),
        )
    })?;
    let wide = |lo: u32, hi: u32| u64::from(lo) | (u64::from(hi) << 32);
    Ok((
        wide(data[0].effective, data[1].effective),
        wide(data[0].permitted, data[1].permitted),
        wide(data[0].inheritable, data[1].inheritable),
    ))
}

/// Empty the calling thread's effective, permitted and inheritable sets.
pub fn capset_none() -> io::Result<()> {
    let mut hdr = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    // SAFETY: capset with a v3 header reads two CapData, which `data` holds.
    cvt_l(unsafe { libc::syscall(libc::SYS_capset, &mut hdr as *mut CapHeader, data.as_ptr()) })
        .map(|_| ())
}

// ─────────────────────────────── limits ───────────────────────────────

/// `getrlimit(resource)`: (soft, hard).
pub fn rlimit(resource: libc::__rlimit_resource_t) -> io::Result<(u64, u64)> {
    let mut r = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `r` is a live local rlimit the kernel writes.
    cvt(unsafe { libc::getrlimit(resource, &mut r) })?;
    Ok((r.rlim_cur, r.rlim_max))
}

/// `setrlimit(resource, {soft, hard})`.
pub fn set_rlimit(resource: libc::__rlimit_resource_t, soft: u64, hard: u64) -> io::Result<()> {
    let r = libc::rlimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    // SAFETY: `r` is a live local rlimit the kernel reads.
    cvt(unsafe { libc::setrlimit(resource, &r) }).map(|_| ())
}

// ─────────────────────────────── Landlock ───────────────────────────────

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The Landlock ABI this kernel speaks.
pub fn landlock_abi() -> io::Result<i64> {
    // SAFETY: the version query takes no attribute (null, size 0).
    cvt_l(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    })
}

/// A ruleset handling `fs`, `net` and `scoped`, with an attribute of `size`
/// bytes (8, 16 or 24: a field an older kernel does not know is not passed).
pub fn landlock_ruleset(fs: u64, net: u64, scoped: u64, size: usize) -> io::Result<OwnedFd> {
    if !matches!(size, 8 | 16 | 24) {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    }
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: net,
        scoped,
    };
    // SAFETY: the kernel reads `size` (at most 24) bytes of `attr`, a live
    // local of 24.
    let fd = cvt_l(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            size,
            0u32,
        )
    })?;
    // SAFETY: a descriptor the kernel just returned, known to nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
}

/// Allow `access` beneath `parent` (an O_PATH descriptor) in `ruleset`.
pub fn landlock_allow_beneath(ruleset: &OwnedFd, parent: &OwnedFd, access: u64) -> io::Result<()> {
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: parent.as_raw_fd(),
    };
    // SAFETY: a path-beneath attribute of the kernel's layout, live for the
    // call.
    cvt_l(unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    })
    .map(|_| ())
}

/// Enter `ruleset`'s domain, for good.
pub fn landlock_restrict_self(ruleset: &OwnedFd) -> io::Result<()> {
    // SAFETY: integer arguments.
    cvt_l(unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) })
        .map(|_| ())
}

// ─────────────────────────────── seccomp ───────────────────────────────

/// One classic BPF instruction (`struct sock_filter`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Insn {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

#[repr(C)]
struct Fprog {
    len: u16,
    filter: *const Insn,
}

const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1;

/// Install `prog` as a seccomp filter on every thread of the process (and
/// what they create from here): TSYNC, so a thread that already existed
/// cannot stay outside it. no_new_privs must be set.
pub fn seccomp_install(prog: &[Insn]) -> io::Result<()> {
    let len = u16::try_from(prog.len()).map_err(|_| io::Error::other("filter too long"))?;
    let fprog = Fprog {
        len,
        filter: prog.as_ptr(),
    };
    // SAFETY: `fprog` and the program it points at outlive the call, and the
    // kernel copies them.
    let r = cvt_l(unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            SECCOMP_FILTER_FLAG_TSYNC,
            &fprog as *const Fprog,
        )
    })?;
    // With TSYNC, a positive return is the thread that could not be brought
    // under the filter, and nothing was installed.
    if r > 0 {
        return Err(io::Error::other(format!(
            "thread {r} could not be synchronised to the filter"
        )));
    }
    Ok(())
}

/// The exit status of a process the seccomp handler stopped.
static SIGSYS_EXIT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);

/// The SIGSYS handler: one line on stderr naming the syscall, then exit.
/// Async-signal-safe: `write` and `exit_group`, both on the list.
extern "C" fn on_sigsys(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // siginfo's _sigsys: call_addr at 16, syscall at 24, arch at 28, on both
    // 64-bit architectures this builds for.
    let nr = if info.is_null() {
        -1
    } else {
        // SAFETY: the kernel hands a full siginfo_t for a seccomp SIGSYS, and
        // bytes 24..28 of it are the syscall number.
        unsafe { info.cast::<u8>().add(24).cast::<i32>().read_unaligned() }
    };
    let mut buf = [0u8; 256];
    let mut n = 0;
    let mut put = |b: &[u8]| {
        let k = b.len().min(buf.len() - n);
        buf[n..n + k].copy_from_slice(&b[..k]);
        n += k;
    };
    put(b"vhost-user-nvgpu: sandbox: syscall ");
    let mut digits = [0u8; 12];
    let mut i = digits.len();
    let mut x = i64::from(nr).unsigned_abs();
    loop {
        i -= 1;
        digits[i] = b'0' + (x % 10) as u8;
        x /= 10;
        if x == 0 {
            break;
        }
    }
    if nr < 0 {
        put(b"-");
    }
    put(&digits[i..]);
    put(
        b" is not on the seccomp allowlist (device/src/sandbox.rs); stopping. \
          --sandbox=off only to diagnose\n",
    );
    let code = SIGSYS_EXIT.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: a local buffer and its length; then an exit that never returns.
    unsafe {
        libc::write(2, buf.as_ptr().cast(), n);
        libc::syscall(libc::SYS_exit_group, code);
    }
}

/// Have a seccomp SIGSYS say which syscall it was and end the process with
/// `exit_code`.
pub fn install_sigsys_report(exit_code: i32) -> io::Result<()> {
    SIGSYS_EXIT.store(exit_code, std::sync::atomic::Ordering::Relaxed);
    // SAFETY: an all-zero sigaction is a valid value; the fields used are set.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_sigsys as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
    // SAFETY: an initialised sigaction that outlives the call, naming a
    // handler of the SA_SIGINFO signature.
    cvt(unsafe { libc::sigaction(libc::SIGSYS, &sa, std::ptr::null_mut()) }).map(|_| ())
}

// ─────────────────────────────── tests ───────────────────────────────

/// For the sandbox's tests: a forked child, and system calls the filter
/// exists to refuse.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// How a forked child ended.
    #[derive(Debug, PartialEq, Eq)]
    pub enum End {
        Exit(i32),
        Signal(i32),
    }

    /// Run `f` in a forked child and say how it ended. The child exits with
    /// what `f` returns, 101 on a panic. glibc keeps malloc usable across
    /// fork, and the children touch no lock another thread could hold.
    ///
    /// The child first closes every descriptor but 0-2: it inherits every
    /// other test thread's too, and holding them for the length of its test
    /// made other tests' "the backend closed its end" probes see a live
    /// peer. From fork to that close the copies still exist, which those
    /// probes allow for (testfd.rs). No child here uses a descriptor it did
    /// not open itself.
    pub fn forked(f: impl FnOnce() -> i32) -> End {
        // SAFETY: fork; the child only runs `f` and exits without unwinding.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: closes this child's own copies; the parent's are untouched.
            unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
            let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(101);
            exit_now(code);
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked, into a local.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        if libc::WIFEXITED(status) {
            End::Exit(libc::WEXITSTATUS(status))
        } else {
            End::Signal(libc::WTERMSIG(status))
        }
    }

    /// `ptrace(PTRACE_TRACEME)`.
    pub fn ptrace_traceme() {
        // SAFETY: integer arguments.
        unsafe { libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0) };
    }

    /// A `clone` that makes a process, not a thread. In the child (if the
    /// filter lets it be made) it exits at once.
    pub fn clone_process() {
        // SAFETY: a fork-like clone (no CLONE_VM, no new stack); the child
        // exits before touching anything.
        let r = unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
        if r == 0 {
            exit_now(0);
        }
    }

    /// An executable anonymous mapping, left mapped.
    pub fn map_executable() {
        // SAFETY: a new mapping at an address the kernel chooses.
        unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
    }

    /// An IP socket, leaked.
    pub fn ip_socket() {
        // SAFETY: integer arguments.
        unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    }

    /// SIGSYS back to its default action.
    pub fn sigsys_default() {
        // SAFETY: the default disposition.
        unsafe { libc::signal(libc::SIGSYS, libc::SIG_DFL) };
    }

    /// SIGABRT ignored, so `abort()` has to take the handler back itself.
    pub fn sigabrt_ignored() {
        // SAFETY: the ignore disposition.
        unsafe { libc::signal(libc::SIGABRT, libc::SIG_IGN) };
    }

    /// TIOCSTI on descriptor 0: push a byte into a terminal's input.
    pub fn tiocsti() {
        const TIOCSTI: libc::c_ulong = 0x5412;
        let c = 0u8;
        // SAFETY: TIOCSTI reads one byte, from a live local.
        unsafe { libc::ioctl(0, TIOCSTI as _, &c) };
    }

    /// A mapping of a new memfd, written once: the calls a filtered process
    /// must still be able to make. `Err` names the step that failed.
    pub fn memfd_mapping_works() -> Result<(), i32> {
        let m = crate::sys::fd::memfd(c"t", libc::MFD_CLOEXEC).map_err(|_| 3)?;
        crate::sys::fd::ftruncate(&m, 4096).map_err(|_| 3)?;
        let map = crate::sys::mem::Mapping::shared(&m, 4096, 0, true).map_err(|_| 4)?;
        map.write(0, &[1]);
        let _e = crate::sys::fd::eventfd(libc::EFD_CLOEXEC).map_err(|_| 5)?;
        let _ep = crate::sys::fd::epoll_create().map_err(|_| 5)?;
        Ok(())
    }
}
