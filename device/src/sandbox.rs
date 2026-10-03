// SPDX-License-Identifier: Apache-2.0
//
// The backend's sandbox, entered once its listening socket is bound and before
// any thread is started or any guest message is read.
//
// Two layers, both unprivileged and both required:
//
//   * Landlock confines what the backend can open. The host GPU nodes this
//     guest is served are read-write; the NVIDIA procfs tree and sysfs are
//     read-only; nothing else on the filesystem is reachable, and where the
//     kernel can scope them, TCP and signals to other processes are refused.
//   * seccomp confines which system calls it can make, and with which
//     arguments: no executable mappings, threads but no processes, and no
//     socket outside AF_UNIX. Everything not on the list is refused.
//
// seccomp does not filter ioctl command numbers. UVM's are plain integers
// rather than _IO-encoded, so there is no namespace byte to match on, and a
// filter cannot follow the pointer a command carries. Which files an ioctl can
// reach is Landlock's rule instead (FS_IOCTL_DEV on the GPU nodes and on
// nothing else), and which commands reach the driver is the ABI profile's and
// the RM tables' job, in the backend itself.
//
// Neither needs a user namespace, so the AppArmor restriction on unprivileged
// user namespaces (Ubuntu 23.10 and later) does not apply. A kernel without
// one of them is refused at start with the reason; there is no flag to go on
// without.
//
// Both apply to the calling thread and to threads it creates afterwards.
// Landlock has no way to reach threads that already exist, so `enter` must
// run while the process is single-threaded; it checks.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// What the backend may open once sandboxed.
#[derive(Debug, Default, Clone)]
pub struct Paths {
    /// Opened read-write, with ioctls: the GPU device nodes.
    pub devices: Vec<PathBuf>,
    /// Read-only trees.
    pub read_only: Vec<PathBuf>,
    /// Directories a listening socket may be created and removed in. The
    /// backend binds its vhost-user socket after the sandbox is on, because
    /// the sandbox has to go on before anything in this process starts a
    /// thread, and binding a socket is one of the few things it must still be
    /// able to do to a filesystem.
    pub sockets: Vec<PathBuf>,
}

/// What `enter` managed, for the start-up log line.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub landlock_abi: i64,
    pub seccomp_rules: usize,
}

/// The GPU device nodes a backend may need open.
///
/// Named rather than discovered: Landlock takes a path that exists now, and a
/// node that appears later cannot be added to a ruleset that is already in
/// force. Nodes this host does not have are skipped, so the list is the same
/// on every host and what it grants is not.
///
/// `/dev/dri` is included whole because a render node's name is per host and
/// the guest's index is resolved against the host's own list at run time.
pub fn gpu_nodes() -> Vec<PathBuf> {
    let mut v = vec![
        PathBuf::from("/dev/nvidiactl"),
        PathBuf::from("/dev/nvidia-uvm"),
        PathBuf::from("/dev/nvidia-modeset"),
        PathBuf::from("/dev/dri"),
    ];
    v.extend((0..16).map(|n| PathBuf::from(format!("/dev/nvidia{n}"))));
    v
}

pub fn enter(paths: &Paths) -> anyhow::Result<Report> {
    // Landlock confines the calling thread and every thread it starts
    // afterwards, and has no way to reach one that already exists. So this
    // runs at the top of `main`, before the backend is built and before
    // anything in this process starts a thread -- `VhostUserDaemon::new`
    // starts one, which is how this check first earned its keep.
    let threads = std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0);
    anyhow::ensure!(
        threads == 1,
        "the sandbox must be entered before this process starts a thread, and it has {threads}"
    );
    // Before Landlock closes /proc/self.
    let _ = set_hardening();
    let abi = landlock::restrict(paths)?;
    let rules = seccomp::install()?;
    Ok(Report {
        landlock_abi: abi,
        seccomp_rules: rules,
    })
}

/// No core dumps, not ptrace-attachable by other processes of the same user,
/// private files by default. Each is best effort: none of them is the
/// boundary, they only take away easy ways to read the backend's memory.
fn set_hardening() -> io::Result<()> {
    // SAFETY: prctl and setrlimit with plain integer arguments.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0 as libc::c_ulong, 0, 0, 0);
        libc::umask(0o077);
        let zero = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &zero);
    }
    Ok(())
}

/// Open a path for Landlock to name, without following a final symlink.
fn open_path(p: &Path) -> Option<OwnedFd> {
    let c = CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    // SAFETY: a NUL-terminated path; the result is checked before use.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    // SAFETY: open returned a descriptor nothing else owns.
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

mod landlock {
    use super::*;

    // linux/landlock.h
    const CREATE_RULESET_VERSION: u32 = 1;
    const RULE_PATH_BENEATH: u32 = 1;

    const FS_EXECUTE: u64 = 1 << 0;
    const FS_WRITE_FILE: u64 = 1 << 1;
    const FS_READ_FILE: u64 = 1 << 2;
    const FS_READ_DIR: u64 = 1 << 3;
    const FS_REMOVE_DIR: u64 = 1 << 4;
    const FS_REMOVE_FILE: u64 = 1 << 5;
    const FS_MAKE_CHAR: u64 = 1 << 6;
    const FS_MAKE_DIR: u64 = 1 << 7;
    const FS_MAKE_REG: u64 = 1 << 8;
    const FS_MAKE_SOCK: u64 = 1 << 9;
    const FS_MAKE_FIFO: u64 = 1 << 10;
    const FS_MAKE_BLOCK: u64 = 1 << 11;
    const FS_MAKE_SYM: u64 = 1 << 12;
    const FS_REFER: u64 = 1 << 13; // ABI 2
    const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
    const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5

    const NET_BIND_TCP: u64 = 1 << 0; // ABI 4
    const NET_CONNECT_TCP: u64 = 1 << 1;

    const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0; // ABI 6
    const SCOPE_SIGNAL: u64 = 1 << 1;

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

    /// Every filesystem right this ABI can name.
    fn fs_rights(abi: i64) -> u64 {
        let mut r = FS_EXECUTE
            | FS_WRITE_FILE
            | FS_READ_FILE
            | FS_READ_DIR
            | FS_REMOVE_DIR
            | FS_REMOVE_FILE
            | FS_MAKE_CHAR
            | FS_MAKE_DIR
            | FS_MAKE_REG
            | FS_MAKE_SOCK
            | FS_MAKE_FIFO
            | FS_MAKE_BLOCK
            | FS_MAKE_SYM;
        if abi >= 2 {
            r |= FS_REFER;
        }
        if abi >= 3 {
            r |= FS_TRUNCATE;
        }
        if abi >= 5 {
            r |= FS_IOCTL_DEV;
        }
        r
    }

    pub fn restrict(paths: &Paths) -> anyhow::Result<i64> {
        // SAFETY: the version query takes no attribute.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<RulesetAttr>(),
                0usize,
                CREATE_RULESET_VERSION,
            )
        };
        if abi < 1 {
            anyhow::bail!(
                "refusing to start: this kernel has no Landlock ({}), so the backend cannot \
                 confine what it opens. Landlock is in Linux 5.13 and later; check that it is \
                 in the lsm= list",
                io::Error::last_os_error()
            );
        }
        let handled = fs_rights(abi);
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: if abi >= 4 {
                NET_BIND_TCP | NET_CONNECT_TCP
            } else {
                0
            },
            scoped: if abi >= 6 {
                SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL
            } else {
                0
            },
        };
        // Older ABIs reject an attribute larger than they know.
        let size = match abi {
            1..=3 => 8,
            4 | 5 => 16,
            _ => size_of::<RulesetAttr>(),
        };
        // SAFETY: attr outlives the call and `size` does not exceed it.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &attr as *const RulesetAttr,
                size,
                0u32,
            )
        };
        if fd < 0 {
            anyhow::bail!("landlock_create_ruleset: {}", io::Error::last_os_error());
        }
        // SAFETY: the syscall returned a descriptor nothing else owns.
        let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };

        let dev = (FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV) & handled;
        let ro = FS_READ_FILE | FS_READ_DIR;
        // Enough to bind a socket and to take a stale one out of the way.
        // Not FS_MAKE_REG: nothing here writes a file.
        let sock = FS_MAKE_SOCK | FS_REMOVE_FILE | FS_READ_DIR;
        for (list, rights) in [
            (&paths.devices, dev),
            (&paths.read_only, ro),
            (&paths.sockets, sock),
        ] {
            for p in list {
                // A node this host does not have is simply not allowed.
                let Some(parent) = open_path(p) else { continue };
                let rule = PathBeneathAttr {
                    allowed_access: rights,
                    parent_fd: parent.as_raw_fd(),
                };
                // SAFETY: rule outlives the call.
                let r = unsafe {
                    libc::syscall(
                        libc::SYS_landlock_add_rule,
                        ruleset.as_raw_fd(),
                        RULE_PATH_BENEATH,
                        &rule as *const PathBeneathAttr,
                        0u32,
                    )
                };
                if r != 0 {
                    anyhow::bail!(
                        "landlock_add_rule {}: {}",
                        p.display(),
                        io::Error::last_os_error()
                    );
                }
            }
        }
        // no_new_privs is already set by posture::enforce, which Landlock
        // requires of an unprivileged caller.
        // SAFETY: a ruleset descriptor and no flags.
        let r =
            unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) };
        if r != 0 {
            anyhow::bail!("landlock_restrict_self: {}", io::Error::last_os_error());
        }
        Ok(abi)
    }
}

mod seccomp {
    use super::*;

    // linux/bpf_common.h, linux/filter.h
    const LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
    const JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
    const JSET_K: u16 = 0x45; // BPF_JMP | BPF_JSET | BPF_K
    const RET_K: u16 = 0x06; // BPF_RET | BPF_K

    // linux/seccomp.h
    const SET_MODE_FILTER: u32 = 1;
    const FILTER_FLAG_TSYNC: u32 = 1;
    const RET_KILL_PROCESS: u32 = 0x8000_0000;
    const RET_ERRNO: u32 = 0x0005_0000;
    const RET_ALLOW: u32 = 0x7fff_0000;

    // linux/audit.h, and the bit x32 sets in a syscall number.
    const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
    const X32_BIT: u32 = 0x4000_0000;

    /// Offsets into `struct seccomp_data`.
    const NR: u32 = 0;
    const ARCH: u32 = 4;
    const fn arg(n: u32) -> u32 {
        16 + 8 * n
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Insn {
        code: u16,
        jt: u8,
        jf: u8,
        k: u32,
    }

    #[repr(C)]
    struct Prog {
        len: u16,
        filter: *const Insn,
    }

    const fn stmt(code: u16, k: u32) -> Insn {
        Insn {
            code,
            jt: 0,
            jf: 0,
            k,
        }
    }

    const fn jump(code: u16, k: u32, jt: u8, jf: u8) -> Insn {
        Insn { code, jt, jf, k }
    }

    const fn ret(k: u32) -> Insn {
        stmt(RET_K, k)
    }

    const fn errno(e: i32) -> u32 {
        RET_ERRNO | (e as u32 & 0xffff)
    }

    /// System calls allowed whatever their arguments.
    ///
    /// The list is what this backend does: a vhost-user socket, epoll, the
    /// NVIDIA device nodes, mappings, threads and futexes. It is not a
    /// minimal set derived from a trace -- a call this misses kills no process
    /// but fails, which is findable in a log, while a trace taken from one
    /// workload would be wrong for the next.
    ///
    /// What it leaves out is the point: ptrace, process_vm_readv/writev,
    /// perf_event_open, bpf, userfaultfd, io_uring, keyctl, kexec, the mount
    /// and namespace calls, and every other way into kernel surface this
    /// process has no business touching.
    fn allowed() -> Vec<libc::c_long> {
        let mut v = vec![
            // files and descriptors
            libc::SYS_read,
            libc::SYS_write,
            libc::SYS_readv,
            libc::SYS_writev,
            libc::SYS_pread64,
            libc::SYS_pwrite64,
            libc::SYS_lseek,
            libc::SYS_close,
            libc::SYS_openat,
            libc::SYS_newfstatat,
            libc::SYS_statx,
            libc::SYS_fstat,
            libc::SYS_getdents64,
            libc::SYS_readlinkat,
            libc::SYS_faccessat2,
            libc::SYS_dup,
            libc::SYS_dup3,
            libc::SYS_fcntl,
            libc::SYS_ftruncate,
            libc::SYS_memfd_create,
            libc::SYS_getcwd,
            libc::SYS_unlinkat,
            // the device nodes. Which files an ioctl can reach is Landlock's
            // job (FS_IOCTL_DEV on the GPU nodes and nothing else); the
            // command numbers cannot be filtered here, see `install`.
            libc::SYS_ioctl,
            // memory
            libc::SYS_munmap,
            libc::SYS_brk,
            libc::SYS_madvise,
            libc::SYS_mremap,
            // waiting
            libc::SYS_futex,
            libc::SYS_sched_yield,
            libc::SYS_nanosleep,
            libc::SYS_clock_nanosleep,
            libc::SYS_clock_gettime,
            libc::SYS_gettimeofday,
            libc::SYS_epoll_create1,
            libc::SYS_epoll_ctl,
            libc::SYS_epoll_wait,
            libc::SYS_epoll_pwait,
            libc::SYS_poll,
            libc::SYS_ppoll,
            libc::SYS_eventfd2,
            libc::SYS_restart_syscall,
            // the vhost-user socket, already bound before this filter is in
            // place; `socket` and `socketpair` are checked by family below.
            libc::SYS_accept4,
            libc::SYS_recvmsg,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_sendto,
            libc::SYS_getsockname,
            libc::SYS_getpeername,
            libc::SYS_getsockopt,
            libc::SYS_setsockopt,
            libc::SYS_shutdown,
            libc::SYS_listen,
            libc::SYS_bind,
            libc::SYS_connect,
            // signals, threads and exit
            libc::SYS_rt_sigaction,
            libc::SYS_rt_sigprocmask,
            libc::SYS_rt_sigreturn,
            libc::SYS_sigaltstack,
            libc::SYS_tgkill,
            libc::SYS_exit,
            libc::SYS_exit_group,
            libc::SYS_set_robust_list,
            libc::SYS_set_tid_address,
            libc::SYS_rseq,
            libc::SYS_membarrier,
            libc::SYS_sched_getaffinity,
            // identity and odds and ends
            libc::SYS_getpid,
            libc::SYS_gettid,
            libc::SYS_getuid,
            libc::SYS_geteuid,
            libc::SYS_getgid,
            libc::SYS_getegid,
            libc::SYS_getrandom,
            libc::SYS_prctl,
            libc::SYS_arch_prctl,
            libc::SYS_uname,
            // The same calls under their older numbers. x86-64 keeps both, and
            // which one a libc uses is the libc's business: the backend ships
            // as a static musl binary, and musl calls `open` where glibc calls
            // `openat`. Without this the first file the backend opened --
            // /proc/driver/nvidia/version -- failed with EPERM, and it refused
            // to start because it could not read the host driver's release.
            //
            // Each one here is the twin of a call already allowed above, so
            // this widens nothing: it spells the same permission the other way.
            libc::SYS_open,
            libc::SYS_stat,
            libc::SYS_lstat,
            libc::SYS_access,
            libc::SYS_readlink,
            libc::SYS_unlink,
            libc::SYS_dup2,
            libc::SYS_accept,
            libc::SYS_epoll_create,
            libc::SYS_eventfd,
            libc::SYS_select,
            libc::SYS_pselect6,
            libc::SYS_getdents,
            libc::SYS_pipe,
            libc::SYS_pipe2,
        ];
        v.sort_unstable();
        v.dedup();
        v
    }

    /// The filter, as classic BPF.
    ///
    /// Shape: check the architecture, refuse the x32 numbering, then the
    /// argument-checked calls, then a run of plain comparisons, then the
    /// default. Blocks that read an argument reload the syscall number first,
    /// because reading an argument leaves it in the accumulator.
    fn program() -> Vec<Insn> {
        let mut p = vec![
            stmt(LD_W_ABS, ARCH),
            jump(JEQ_K, AUDIT_ARCH_X86_64, 1, 0),
            ret(RET_KILL_PROCESS),
            stmt(LD_W_ABS, NR),
            jump(JSET_K, X32_BIT, 0, 1),
            ret(RET_KILL_PROCESS),
        ];

        // No executable mappings, ever. The backend runs code it was built
        // with; a mapping it could execute is the first half of turning a bug
        // in this process into running code of someone else's choosing.
        for nr in [libc::SYS_mmap, libc::SYS_mprotect, libc::SYS_pkey_mprotect] {
            p.extend([
                stmt(LD_W_ABS, NR),
                jump(JEQ_K, nr as u32, 0, 4),
                stmt(LD_W_ABS, arg(2)), // prot
                jump(JSET_K, libc::PROT_EXEC as u32, 0, 1),
                ret(errno(libc::EPERM)),
                ret(RET_ALLOW),
            ]);
        }

        // Threads, not processes. CLONE_THREAD says the new task shares this
        // one's thread group; without it this is a fork, and a forked copy of
        // a sandboxed process is a second thing to reason about.
        p.extend([
            stmt(LD_W_ABS, NR),
            jump(JEQ_K, libc::SYS_clone as u32, 0, 4),
            stmt(LD_W_ABS, arg(0)), // flags
            jump(JSET_K, libc::CLONE_THREAD as u32, 1, 0),
            ret(errno(libc::EPERM)),
            ret(RET_ALLOW),
        ]);

        // clone3 has its flags behind a pointer, where a filter cannot read
        // them. ENOSYS rather than EPERM, because that is the answer a libc
        // falls back from to clone.
        p.extend([
            stmt(LD_W_ABS, NR),
            jump(JEQ_K, libc::SYS_clone3 as u32, 0, 1),
            ret(errno(libc::ENOSYS)),
        ]);

        // Unix sockets only. The backend talks to one VMM over one socket; a
        // network socket would be a way out of a process that is not supposed
        // to have one, and Landlock's TCP rules only cover kernels at ABI 4.
        for nr in [libc::SYS_socket, libc::SYS_socketpair] {
            p.extend([
                stmt(LD_W_ABS, NR),
                jump(JEQ_K, nr as u32, 0, 4),
                stmt(LD_W_ABS, arg(0)), // family
                jump(JEQ_K, libc::AF_UNIX as u32, 1, 0),
                ret(errno(libc::EAFNOSUPPORT)),
                ret(RET_ALLOW),
            ]);
        }

        let simple = allowed();
        let n = simple.len();
        p.push(stmt(LD_W_ABS, NR));
        for (i, nr) in simple.iter().enumerate() {
            let to_allow = (n - 1 - i) as u8;
            let jf = if i + 1 == n { 1 } else { 0 };
            p.push(jump(JEQ_K, *nr as u32, to_allow, jf));
        }
        p.push(ret(RET_ALLOW));

        // Everything else is refused, and refused rather than killed: a call
        // this list failed to anticipate is then a failure in a log with a
        // name on it, not a backend that vanished mid-frame. The syscall does
        // not run either way, which is what the filter is for.
        p.push(ret(errno(libc::EPERM)));
        p
    }

    pub fn install() -> anyhow::Result<usize> {
        // Is there a filter mode at all? A kernel without one takes the
        // arguments and fails with EINVAL on the mode; one with it reaches the
        // program pointer and fails with EFAULT.
        // SAFETY: a null program with a mode the kernel either knows or does not.
        let probe = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SET_MODE_FILTER,
                0,
                std::ptr::null::<Prog>(),
            )
        };
        let why = io::Error::last_os_error();
        if probe == 0 || why.raw_os_error() != Some(libc::EFAULT) {
            anyhow::bail!(
                "refusing to start: this kernel has no seccomp filtering ({why}), so the \
                 backend cannot confine which system calls it makes. It is CONFIG_SECCOMP_FILTER, \
                 in every distribution kernel"
            );
        }

        let prog = program();
        anyhow::ensure!(prog.len() <= 4096, "seccomp program too long");
        let fprog = Prog {
            len: prog.len() as u16,
            filter: prog.as_ptr(),
        };
        // TSYNC puts the filter on every thread of this process, and fails
        // rather than leaving one of them unfiltered. There is one thread here
        // -- `enter` checked -- so it is the threads started afterwards that
        // matter, and they inherit it.
        //
        // SAFETY: fprog and the program it points at outlive the call.
        let r = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                SET_MODE_FILTER,
                FILTER_FLAG_TSYNC,
                &fprog as *const Prog,
            )
        };
        if r != 0 {
            anyhow::bail!("seccomp(SET_MODE_FILTER): {}", io::Error::last_os_error());
        }
        Ok(prog.len())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Jump offsets are the whole correctness of a classic BPF program and
        /// are computed here, not written out. Every jump must land inside the
        /// program.
        #[test]
        fn every_jump_lands_inside_the_program() {
            let p = program();
            for (i, insn) in p.iter().enumerate() {
                if insn.code == RET_K {
                    continue;
                }
                if insn.code == JEQ_K || insn.code == JSET_K {
                    for off in [insn.jt, insn.jf] {
                        let target = i + 1 + off as usize;
                        assert!(target < p.len(), "instruction {i} jumps to {target}");
                    }
                }
            }
        }

        /// The last instruction has to be the default refusal: a classic BPF
        /// program that falls off the end is rejected by the kernel, and a
        /// program whose last instruction is an ALLOW is an allowlist that
        /// allows everything.
        #[test]
        fn the_program_ends_in_a_refusal() {
            let p = program();
            let last = p.last().expect("not empty");
            assert_eq!(last.code, RET_K);
            assert_eq!(last.k, errno(libc::EPERM));
        }

        /// A plain comparison that does not match must reach the default, not
        /// the ALLOW that follows the run.
        #[test]
        fn the_last_plain_comparison_falls_through_to_the_refusal() {
            let p = program();
            let allow_at = p
                .iter()
                .rposition(|i| i.code == RET_K && i.k == RET_ALLOW)
                .expect("a run of comparisons ends in one");
            let last_cmp = &p[allow_at - 1];
            assert_eq!(last_cmp.code, JEQ_K);
            assert_eq!(allow_at + last_cmp.jf as usize, p.len() - 1);
        }

        #[test]
        fn the_calls_that_matter_are_checked_rather_than_listed() {
            let listed = allowed();
            for nr in [
                libc::SYS_mmap,
                libc::SYS_mprotect,
                libc::SYS_clone,
                libc::SYS_socket,
                libc::SYS_socketpair,
            ] {
                assert!(!listed.contains(&nr), "{nr} is allowed unchecked");
            }
        }

        /// The ones whose absence is the reason for the filter.
        #[test]
        fn the_dangerous_calls_are_not_in_the_list() {
            let listed = allowed();
            for nr in [
                libc::SYS_ptrace,
                libc::SYS_process_vm_readv,
                libc::SYS_process_vm_writev,
                libc::SYS_perf_event_open,
                libc::SYS_bpf,
                libc::SYS_userfaultfd,
                libc::SYS_io_uring_setup,
                libc::SYS_keyctl,
                libc::SYS_unshare,
                libc::SYS_mount,
                libc::SYS_execve,
                libc::SYS_execveat,
                libc::SYS_fork,
                libc::SYS_vfork,
            ] {
                assert!(!listed.contains(&nr), "{nr} is allowed");
            }
        }
    }
}

/// What a sandboxed process can and cannot do, checked from inside one.
///
/// Returns what did not hold. Both halves matter: a sandbox that refuses what
/// the backend needs is as much a defect as one that lets something through,
/// so the allowed cases are checked too. This runs in the
/// `nvgpu-sandbox-selftest` binary, a process of its own because [`enter`]
/// cannot be undone and must run single-threaded -- which a test harness,
/// having a thread of its own, is not.
pub fn selftest(allowed_dir: &Path) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |what: &str, ok: bool| {
        if !ok {
            bad.push(what.to_string());
        }
    };

    check(
        "read a file inside the ruleset",
        std::fs::read(allowed_dir.join("allowed")).is_ok(),
    );
    // SAFETY: syscalls with plain arguments, every result checked rather than
    // used.
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        check("map memory", p != libc::MAP_FAILED);
        if p != libc::MAP_FAILED {
            libc::munmap(p, 4096);
        }

        let s = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0);
        check("open a unix socket", s >= 0);
        if s >= 0 {
            libc::close(s);
        }

        let x = libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_READ | libc::PROT_EXEC,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        );
        check("refuse an executable mapping", x == libc::MAP_FAILED);
        if x != libc::MAP_FAILED {
            libc::munmap(x, 4096);
        }

        let inet = libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0);
        check("refuse an internet socket", inet < 0);
        if inet >= 0 {
            libc::close(inet);
        }

        // Not on the list, so the default refusal answers it.
        check(
            "refuse ptrace",
            libc::syscall(libc::SYS_ptrace, 0, 0, 0, 0) < 0,
        );

        // A process rather than a thread: CLONE_THREAD is not set.
        check(
            "refuse a fork",
            libc::syscall(libc::SYS_clone, libc::SIGCHLD as libc::c_ulong, 0, 0, 0, 0) < 0,
        );
    }
    check(
        "refuse a path outside the ruleset",
        std::fs::read("/etc/hostname").is_err(),
    );

    // The legacy syscall numbers, which a static musl binary uses where glibc
    // uses the *at ones. This is checked here rather than left to a guest
    // probe: missing `open` cost a probe run, and it looked like a driver
    // problem rather than a sandbox one.
    // SAFETY: a path that exists, opened read-only and closed.
    unsafe {
        let path =
            std::ffi::CString::new(allowed_dir.join("allowed").as_os_str().as_encoded_bytes())
                .expect("no interior NUL");
        let fd = libc::syscall(libc::SYS_open, path.as_ptr(), libc::O_RDONLY);
        check("allow the legacy open(2)", fd >= 0);
        if fd >= 0 {
            libc::close(fd as i32);
        }
        let mut st: libc::stat = std::mem::zeroed();
        check(
            "allow the legacy stat(2)",
            libc::syscall(libc::SYS_stat, path.as_ptr(), &mut st as *mut libc::stat) == 0,
        );
    }
    bad
}
