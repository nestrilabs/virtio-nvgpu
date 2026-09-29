// SPDX-License-Identifier: Apache-2.0
//! The backend's process sandbox: what it may call, open and reach once it
//! has opened what it must at start.
//!
//! `posture` makes the backend an unprivileged process: no capabilities,
//! no_new_privs, undumpable. That leaves it everything its uid has -- every
//! file the uid can open, every socket it can connect to, every syscall the
//! kernel offers -- and one process per VM maps all of that guest's RAM and
//! parses everything the guest sends (SECURITY.md, "The backend
//! process"). This takes the rest
//! away, in four layers, each installed once, before the first guest message
//! and before the process has a second thread:
//!
//! 1. **No network.** The backend needs none: every socket it uses is a
//!    pathname UNIX socket or a netlink socket opened before this. If the
//!    launcher already put it in a network namespace of its own (one with
//!    only a loopback interface, as `rig/run-guest.sh` does as root),
//!    that is kept; otherwise it unshares a user namespace and a network
//!    namespace together, which needs no privilege where unprivileged user
//!    namespaces are allowed. The user namespace maps only the backend's own
//!    uid and gid onto themselves, so every uid check it makes and every
//!    check the host kernel makes of it are unchanged (supplementary groups
//!    keep working: they are kernel ids, only their names inside are lost),
//!    and the full capability set the new namespace grants is dropped again
//!    at once. The kernel's uevents reach only network namespaces of the
//!    initial user namespace, which is why the hotplug socket is opened
//!    before this.
//! 2. **Landlock.** From here on the backend opens: the host GPU's device
//!    nodes (read, write and ioctl), `/dev/null`, `/dev/udmabuf`, the render
//!    nodes of this GPU and, in compositor-VM mode, its card nodes -- no other
//!    GPU's, even of the same uid; reads `/proc/driver/nvidia`, its own
//!    `/proc/self`, `/proc/cpuinfo` and the sysfs directory of each GPU; and
//!    connects to the compositor's socket and its own export socket. Nothing
//!    else: no write anywhere in the filesystem, no directory created, no
//!    file removed, no other UNIX socket (with Landlock ABI 9), no signal to
//!    a process outside the sandbox and no abstract socket outside it (ABI
//!    6), no TCP or UDP (ABI 4 and 10). The vhost-user socket and the export
//!    socket are bound before this, so no directory needs to be writable, and
//!    the log is the inherited stderr.
//! 3. **seccomp.** An allowlist of syscall numbers, with arguments checked
//!    where it is cheap and matters: threads may be made, processes may not
//!    (`clone` only with CLONE_THREAD and no namespace flag, `clone3` answered
//!    ENOSYS so the C library falls back to it); no mapping or protection
//!    change may add PROT_EXEC; only AF_UNIX sockets; no TIOCSTI or TIOCLINUX
//!    on an inherited terminal; `prctl` only to name threads and to read;
//!    `tgkill` only within this process; the SIGSYS handler cannot be
//!    replaced. `ioctl` itself is allowed: which file a descriptor is cannot
//!    be seen from a filter, and every guest call is one. `unlink` answers
//!    EPERM, since the only one the backend makes is tidying its own
//!    sockets on the way out. Anything else stops the process: the SIGSYS
//!    handler writes the syscall number to the log and exits with 159.
//! 4. **Limits.** RLIMIT_CORE 0 on top of undumpable. RLIMIT_NOFILE is
//!    raised, not lowered, by `posture::raise_nofile` (the handle table is
//!    sized from it), and memory is the launcher's cgroup's to bound.
//!
//! A layer the host kernel lacks, or has only in part, is reported in a
//! line beginning `sandbox: DEGRADED`, and with `--sandbox=on` (the default)
//! the backend then refuses to start: a guest is never served by a backend
//! less confined than this describes. `--sandbox=best-effort` runs with what
//! the kernel has, for a host being brought up to it; `--sandbox=off` turns
//! all four off, for finding out whether the sandbox is what broke
//! something. Both are diagnostic flags (`--diagnostic`), and say so at
//! every start.
//!
//! What this does not do: it keeps a compromised backend from the rest of
//! the host, and from other VMs' backends and VMMs, but not from the host
//! kernel it can still call (ioctl on the GPU's nodes above all) or from the
//! guest whose memory it maps. The GPU's own isolation between clients is
//! RM's page tables, which no process sandbox touches.

#![forbid(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `--sandbox`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Every layer, in full, or the backend does not start.
    #[default]
    On,
    /// Every layer the host kernel has; the rest reported DEGRADED and
    /// done without. For a host that lacks one, while it is being fixed.
    BestEffort,
    /// None. For diagnosis only.
    Off,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "on" => Ok(Mode::On),
            "best-effort" => Ok(Mode::BestEffort),
            "off" => Ok(Mode::Off),
            _ => Err(format!("{s:?}: on, best-effort or off")),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::On => "on",
            Mode::BestEffort => "best-effort",
            Mode::Off => "off",
        })
    }
}

/// What one layer came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Layer {
    /// In force, as described.
    Enforced(String),
    /// Not in force, or only in part, and why.
    Degraded(String),
}

impl Layer {
    pub fn is_enforced(&self) -> bool {
        matches!(self, Layer::Enforced(_))
    }
}

/// What `apply` did, layer by layer.
#[derive(Debug, Clone)]
pub struct Report {
    pub network: Layer,
    pub limits: Layer,
    pub landlock: Layer,
    pub seccomp: Layer,
}

impl Report {
    /// Every layer in force in full.
    pub fn complete(&self) -> bool {
        [&self.network, &self.limits, &self.landlock, &self.seccomp]
            .iter()
            .all(|l| l.is_enforced())
    }

    /// One line per layer: info when in force, a warning when not.
    pub fn log(&self) {
        for (name, l) in [
            ("network", &self.network),
            ("limits", &self.limits),
            ("landlock", &self.landlock),
            ("seccomp", &self.seccomp),
        ] {
            match l {
                Layer::Enforced(s) => log::info!("sandbox: {name}: {s}"),
                Layer::Degraded(s) => log::warn!("sandbox: DEGRADED: {name}: {s}"),
            }
        }
    }
}

/// What the sandboxed process may still reach in the filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// Device nodes opened read-write and driven by ioctl.
    pub devices: Vec<PathBuf>,
    /// Files or trees read (and directories listed).
    pub read: Vec<PathBuf>,
    /// Pathname UNIX sockets connected to.
    pub connect: Vec<PathBuf>,
}

/// What the backend reaches after start, from what it was started with.
#[derive(Debug, Clone)]
pub struct BackendPaths<'a> {
    /// `--proc-nvidia`.
    pub proc_nvidia: &'a Path,
    /// The PCI addresses of the host driver's GPUs (`gpus/` under it).
    pub gpus: Vec<String>,
    /// `--allow-compute`: the UVM devices.
    pub compute: bool,
    /// `--kms-card`: the card nodes.
    pub kms_card: bool,
    /// `--wayland-socket`.
    pub wayland_socket: Option<&'a Path>,
    /// `--wayland-export`, bound before the sandbox.
    pub export_socket: Option<&'a Path>,
}

impl Plan {
    /// The backend's plan. `dev` and `sys` are `/dev` and `/sys`, parameters
    /// so a test can hand it a fixture tree.
    pub fn backend(p: &BackendPaths<'_>, dev: &Path, sys: &Path) -> Self {
        let mut plan = Plan::default();
        // /dev/nvidiactl, /dev/nvidia-modeset and /dev/nvidiaN: the control
        // node, NVKMS and every GPU minor the host has (a guest opens a GPU by
        // minor; a node made after start is not reachable, and says so).
        let mut names: Vec<String> = std::fs::read_dir(dev)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| {
                        n.strip_prefix("nvidia")
                            .is_some_and(|r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names.insert(0, "nvidiactl".into());
        names.insert(1, "nvidia-modeset".into());
        if p.compute {
            names.push("nvidia-uvm".into());
            names.push("nvidia-uvm-tools".into());
        }
        names.push("udmabuf".into());
        names.push("null".into());
        plan.devices.extend(names.iter().map(|n| dev.join(n)));

        plan.read.push(p.proc_nvidia.to_path_buf());
        plan.read.push(PathBuf::from("/proc/self"));
        plan.read.push(PathBuf::from("/proc/cpuinfo"));
        for addr in &p.gpus {
            // The device's own directory, not /sys/bus: its `config`, its
            // `drm/` listing and each node's `dev` all resolve beneath it.
            let pci = sys.join("bus/pci/devices").join(addr);
            let dir = std::fs::canonicalize(&pci).unwrap_or(pci);
            if let Ok(d) = std::fs::read_dir(dir.join("drm")) {
                let mut nodes: Vec<String> = d
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("renderD") || (p.kms_card && n.starts_with("card")))
                    .collect();
                nodes.sort();
                plan.devices
                    .extend(nodes.iter().map(|n| dev.join("dri").join(n)));
            }
            plan.read.push(dir);
        }
        plan.connect.extend(p.wayland_socket.map(Path::to_path_buf));
        plan.connect.extend(p.export_socket.map(Path::to_path_buf));
        plan
    }
}

/// Apply every layer. Call once, before the process has a second thread
/// (a user namespace cannot be entered after, and Landlock and the filter
/// reach only the calling thread and what it later creates), after
/// `posture::drop_all_caps` (which sets no_new_privs), and after opening
/// everything `plan` does not cover.
pub fn apply(plan: &Plan) -> Report {
    // A second thread would be outside Landlock's domain (it reaches only
    // the calling thread's descendants) and cannot follow into the user
    // namespace; the filter would reach it (TSYNC), the rest not. That is a
    // bug in the start-up order, and nothing is installed half-way.
    match thread_count() {
        Some(1) => {}
        n => {
            let why = match n {
                Some(n) => format!(
                    "{n} threads already; the sandbox is installed with one (a bug in the \
                     start-up order), so no layer was"
                ),
                None => "cannot read the thread count from /proc/self/status, so no layer \
                         was installed"
                    .to_string(),
            };
            return Report {
                network: Layer::Degraded(why.clone()),
                limits: Layer::Degraded(why.clone()),
                landlock: Layer::Degraded(why.clone()),
                seccomp: Layer::Degraded(why),
            };
        }
    }
    let network = private_network();
    let limits = limits();
    let landlock = landlock(plan);
    let seccomp = seccomp();
    let mut r = Report {
        network,
        limits,
        landlock,
        seccomp,
    };
    verify(&mut r);
    r
}

// ── 1. The network ───────────────────────────────────────────────────────────

const CLONE_NEWUSER: libc::c_int = 0x1000_0000;
const CLONE_NEWNET: libc::c_int = 0x4000_0000;

/// The interfaces of this process's network namespace, from `/proc/self/net/dev`.
fn interfaces() -> io::Result<Vec<String>> {
    let text = std::fs::read_to_string("/proc/self/net/dev")?;
    Ok(text
        .lines()
        .skip(2)
        .filter_map(|l| l.split_once(':'))
        .map(|(n, _)| n.trim().to_string())
        .collect())
}

fn only_loopback() -> io::Result<bool> {
    Ok(interfaces()?.iter().all(|n| n == "lo"))
}

fn private_network() -> Layer {
    match only_loopback() {
        Ok(true) => {
            return Layer::Enforced(
                "no interface but loopback (a network namespace the launcher made)".into(),
            );
        }
        Ok(false) => {}
        Err(e) => return Layer::Degraded(format!("cannot read /proc/self/net/dev: {e}")),
    }
    let (uid, gid) = (crate::sys::proc::euid(), crate::sys::proc::egid());
    if let Err(e) = crate::sys::proc::unshare(CLONE_NEWUSER | CLONE_NEWNET) {
        return Layer::Degraded(format!(
            "no network namespace: unshare(CLONE_NEWUSER|CLONE_NEWNET): {e}. Unprivileged \
             user namespaces are off on this host (user.max_user_namespaces, \
             kernel.unprivileged_userns_clone, an AppArmor userns restriction); the backend \
             keeps the host's network. Start it in one (unshare --net as root, \
             PrivateNetwork=yes) to close this"
        ));
    }
    // The maps are written once, and until they are the process's own ids
    // read as the overflow id. A failure here leaves a process that cannot
    // be trusted to compare uids, so it does not go on.
    //
    // An undumpable process's /proc/self files belong to root, so it cannot
    // write its own maps: dumpable for the three writes, and undumpable
    // again before anything else happens. Nothing is mapped yet, there is
    // one thread, and the new namespace's capabilities are not yet dropped
    // -- but reaching them takes being this process already.
    let was_dumpable = crate::sys::proc::dumpable();
    let _ = crate::sys::proc::set_dumpable(true);
    let maps = [
        ("/proc/self/setgroups", "deny".to_string()),
        ("/proc/self/uid_map", format!("{uid} {uid} 1")),
        ("/proc/self/gid_map", format!("{gid} {gid} 1")),
    ];
    for (path, text) in &maps {
        if let Err(e) = std::fs::write(path, text) {
            log::error!("sandbox: writing {path} in the new user namespace: {e}; stopping");
            // Nothing has been served.
            crate::sys::proc::exit_now(1);
        }
    }
    if was_dumpable != 1
        && let Err(e) = crate::sys::proc::set_dumpable(false)
    {
        log::error!("sandbox: undumpable again: {e}; stopping");
        crate::sys::proc::exit_now(1);
    }
    // The new namespace gave this process every capability in it. None of
    // them reach anything outside it, and none of them are kept.
    if let Err(e) = crate::posture::drop_all_caps() {
        log::error!("sandbox: dropping the new user namespace's capabilities: {e}; stopping");
        crate::sys::proc::exit_now(1);
    }
    match only_loopback() {
        Ok(true) => Layer::Enforced(
            "a network namespace of its own (with a user namespace mapping only its own uid \
             and gid; capabilities dropped again)"
                .into(),
        ),
        Ok(false) => Layer::Degraded("unshared, but interfaces other than lo remain".into()),
        Err(e) => Layer::Degraded(format!("unshared, but cannot confirm: {e}")),
    }
}

// ── 4. Limits ────────────────────────────────────────────────────────────────

fn limits() -> Layer {
    if let Err(e) = crate::sys::proc::set_rlimit(libc::RLIMIT_CORE, 0, 0) {
        return Layer::Degraded(format!("RLIMIT_CORE: {e}"));
    }
    Layer::Enforced("RLIMIT_CORE 0; no_new_privs and undumpable (posture)".into())
}

// ── 2. Landlock ──────────────────────────────────────────────────────────────

mod ll {
    pub const EXECUTE: u64 = 1 << 0;
    pub const WRITE_FILE: u64 = 1 << 1;
    pub const READ_FILE: u64 = 1 << 2;
    pub const READ_DIR: u64 = 1 << 3;
    pub const REFER: u64 = 1 << 13;
    pub const TRUNCATE: u64 = 1 << 14;
    pub const IOCTL_DEV: u64 = 1 << 15;
    pub const RESOLVE_UNIX: u64 = 1 << 16;
    /// What a rule on a non-directory may grant (security/landlock/fs.c).
    pub const ACCESS_FILE: u64 =
        EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV | RESOLVE_UNIX;

    pub const NET_BIND_TCP: u64 = 1 << 0;
    pub const NET_CONNECT_TCP: u64 = 1 << 1;
    pub const NET_BIND_UDP: u64 = 1 << 2;
    pub const NET_CONNECT_SEND_UDP: u64 = 1 << 3;

    pub const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
    pub const SCOPE_SIGNAL: u64 = 1 << 1;
}

/// The Landlock ABI this kernel speaks, or why none.
fn landlock_abi() -> Result<i64, io::Error> {
    crate::sys::proc::landlock_abi()
}

/// Every filesystem access an ABI can restrict.
fn handled_fs(abi: i64) -> u64 {
    let mut m = (1u64 << 13) - 1;
    if abi >= 2 {
        m |= ll::REFER;
    }
    if abi >= 3 {
        m |= ll::TRUNCATE;
    }
    if abi >= 5 {
        m |= ll::IOCTL_DEV;
    }
    if abi >= 9 {
        m |= ll::RESOLVE_UNIX;
    }
    m
}

fn handled_net(abi: i64) -> u64 {
    let mut m = 0;
    if abi >= 4 {
        m |= ll::NET_BIND_TCP | ll::NET_CONNECT_TCP;
    }
    if abi >= 10 {
        m |= ll::NET_BIND_UDP | ll::NET_CONNECT_SEND_UDP;
    }
    m
}

fn scoped(abi: i64) -> u64 {
    if abi >= 6 {
        ll::SCOPE_ABSTRACT_UNIX_SOCKET | ll::SCOPE_SIGNAL
    } else {
        0
    }
}

/// The attribute's size for an ABI: a field an older kernel does not know
/// must not be passed at all.
fn attr_size(abi: i64) -> usize {
    if abi >= 6 {
        24
    } else if abi >= 4 {
        16
    } else {
        8
    }
}

/// Add one rule; `Ok(false)` when there is nothing at `path` (a node that
/// does not exist at start stays unreachable, which the caller reports).
fn add_rule(ruleset: &OwnedFd, path: &Path, access: u64, handled: u64) -> io::Result<bool> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    let fd = match crate::sys::fd::open(&c, libc::O_PATH | libc::O_CLOEXEC) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let st = crate::sys::fd::fstat(fd.as_raw_fd())?;
    let mut allowed = access & handled;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        allowed &= ll::ACCESS_FILE;
    }
    if allowed == 0 {
        // Nothing this kernel restricts: nothing to grant.
        return Ok(true);
    }
    crate::sys::proc::landlock_allow_beneath(ruleset, &fd, allowed)?;
    Ok(true)
}

fn landlock(plan: &Plan) -> Layer {
    let abi = match landlock_abi() {
        Ok(v) => v,
        Err(e) => {
            return Layer::Degraded(format!(
                "this kernel has no Landlock ({e}; it needs CONFIG_SECURITY_LANDLOCK and \
                 \"landlock\" in lsm=): the backend can open every file its uid can, and \
                 connect to every socket"
            ));
        }
    };
    let fs = handled_fs(abi);
    let ruleset =
        match crate::sys::proc::landlock_ruleset(fs, handled_net(abi), scoped(abi), attr_size(abi))
        {
            Ok(fd) => fd,
            Err(e) => return Layer::Degraded(format!("landlock_create_ruleset (ABI {abi}): {e}")),
        };

    let rules = plan
        .devices
        .iter()
        .map(|p| (p, ll::READ_FILE | ll::WRITE_FILE | ll::IOCTL_DEV))
        .chain(plan.read.iter().map(|p| (p, ll::READ_FILE | ll::READ_DIR)))
        .chain(plan.connect.iter().map(|p| (p, ll::RESOLVE_UNIX)));
    let mut absent = Vec::new();
    let mut granted = 0usize;
    for (path, access) in rules {
        match add_rule(&ruleset, path, access, fs) {
            Ok(true) => granted += 1,
            Ok(false) => absent.push(path.display().to_string()),
            Err(e) => {
                return Layer::Degraded(format!(
                    "the rule for {} failed ({e}); no Landlock domain installed",
                    path.display()
                ));
            }
        }
    }
    if !absent.is_empty() {
        // Ordinary: /dev/udmabuf or the UVM nodes on a host without them.
        log::info!(
            "sandbox: not present at start, so not reachable after: {}",
            absent.join(", ")
        );
    }
    // no_new_privs is set (posture::drop_all_caps).
    if let Err(e) = crate::sys::proc::landlock_restrict_self(&ruleset) {
        return Layer::Degraded(format!("landlock_restrict_self: {e}"));
    }
    let what = format!(
        "ABI {abi}, {granted} paths: the GPU's nodes, /proc/driver/nvidia, /proc/self, the \
         GPUs' sysfs; no write, create or remove anywhere"
    );
    // IP is the network namespace's to take away; the rest is this layer's.
    let mut ip = Vec::new();
    if abi < 4 {
        ip.push("TCP");
    }
    if abi < 10 {
        ip.push("UDP");
    }
    let ip = if ip.is_empty() {
        String::new()
    } else {
        format!("; {} left to the network namespace", ip.join(" and "))
    };
    let mut gaps = Vec::new();
    if abi < 6 {
        gaps.push("signals to, and abstract sockets of, this uid's other processes (ABI 6)");
    }
    if abi < 9 {
        gaps.push(
            "connecting to any other pathname UNIX socket this uid can reach -- another VM's \
             export socket, the session bus -- (ABI 9)",
        );
    }
    if gaps.is_empty() {
        Layer::Enforced(format!("{what}{ip}"))
    } else {
        Layer::Degraded(format!(
            "{what}{ip}; this kernel does not restrict {}",
            gaps.join(", ")
        ))
    }
}

// ── 3. seccomp ───────────────────────────────────────────────────────────────

const BPF_LD_W_ABS: u16 = 0x20;
const BPF_JEQ_K: u16 = 0x15;
const BPF_JSET_K: u16 = 0x45;
const BPF_RET_K: u16 = 0x06;

const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

/// `struct seccomp_data`.
const DATA_NR: u32 = 0;
const DATA_ARCH: u32 = 4;
/// The low word of argument `i` (little-endian).
const fn arg(i: u32) -> u32 {
    16 + 8 * i
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// The exit status after a refused syscall: what a death by SIGSYS reads as.
pub const REFUSED_EXIT: i32 = 128 + libc::SIGSYS;

const CLONE_THREAD: u32 = 0x0001_0000;
/// Every CLONE_NEW* flag clone(2) takes: NS, CGROUP, UTS, IPC, USER, PID and
/// NET. (CLONE_NEWTIME shares its bit with the exit signal there, and is
/// clone3's and unshare's alone.)
const CLONE_NS: u32 =
    0x0002_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;
const TIOCSTI: u32 = 0x5412;
const TIOCLINUX: u32 = 0x541C;
const PR_SET_VMA: u32 = 0x5356_4d41;

use crate::sys::proc::Insn;

const fn ld(k: u32) -> Insn {
    Insn {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k,
    }
}
const fn jeq(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: BPF_JEQ_K,
        jt,
        jf,
        k,
    }
}
const fn jset(k: u32, jt: u8, jf: u8) -> Insn {
    Insn {
        code: BPF_JSET_K,
        jt,
        jf,
        k,
    }
}
const fn ret(k: u32) -> Insn {
    Insn {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Syscalls allowed whatever their arguments.
fn plain() -> Vec<libc::c_long> {
    let mut v = vec![
        // Descriptors and I/O.
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_readv,
        libc::SYS_writev,
        libc::SYS_pread64,
        libc::SYS_pwrite64,
        libc::SYS_lseek,
        libc::SYS_close,
        libc::SYS_dup,
        libc::SYS_dup3,
        libc::SYS_fcntl,
        libc::SYS_pipe2,
        libc::SYS_eventfd2,
        // Paths: Landlock decides which.
        libc::SYS_openat,
        libc::SYS_fstat,
        libc::SYS_newfstatat,
        libc::SYS_statx,
        libc::SYS_readlinkat,
        // Which filesystem a descriptor handed over is on (hostfd.rs,
        // classify).
        libc::SYS_fstatfs,
        libc::SYS_getdents64,
        // Waiting.
        libc::SYS_ppoll,
        libc::SYS_epoll_create1,
        libc::SYS_epoll_ctl,
        libc::SYS_epoll_pwait,
        libc::SYS_epoll_pwait2,
        libc::SYS_futex,
        libc::SYS_nanosleep,
        libc::SYS_clock_nanosleep,
        libc::SYS_clock_gettime,
        libc::SYS_clock_getres,
        libc::SYS_gettimeofday,
        libc::SYS_sched_yield,
        libc::SYS_sched_getaffinity,
        libc::SYS_restart_syscall,
        // Threads and signals (clone, tgkill, rt_sigaction are filtered below).
        libc::SYS_set_robust_list,
        libc::SYS_rseq,
        libc::SYS_rt_sigprocmask,
        libc::SYS_rt_sigreturn,
        libc::SYS_sigaltstack,
        libc::SYS_exit,
        libc::SYS_exit_group,
        // Identity.
        libc::SYS_getpid,
        libc::SYS_gettid,
        libc::SYS_getuid,
        libc::SYS_geteuid,
        libc::SYS_getgid,
        libc::SYS_getegid,
        libc::SYS_capget,
        libc::SYS_getrandom,
        // Memory (mmap and mprotect are filtered below). Guest RAM, the window,
        // shm pools and blobs are memfds.
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_madvise,
        libc::SYS_brk,
        libc::SYS_mincore,
        libc::SYS_memfd_create,
        libc::SYS_ftruncate,
        libc::SYS_fallocate,
        // UNIX sockets (socket and socketpair are filtered below): the VMM's,
        // the compositor's, export peers', and descriptors passed on them.
        libc::SYS_connect,
        libc::SYS_accept4,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
        libc::SYS_shutdown,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
        libc::SYS_getsockname,
        libc::SYS_getpeername,
    ];
    #[cfg(target_arch = "x86_64")]
    v.extend([
        libc::SYS_dup2,
        libc::SYS_readlink,
        libc::SYS_poll,
        libc::SYS_epoll_wait,
    ]);
    v.sort_unstable();
    v.dedup();
    v
}

/// Syscalls allowed with conditions, each a block that ends in a return,
/// and so entered only by a match on its number (the accumulator holds the
/// number until a block loads an argument).
fn special(pid: u32, violation: u32) -> Vec<(libc::c_long, Vec<Insn>)> {
    let allow = ret(SECCOMP_RET_ALLOW);
    let deny = ret(violation);
    let no_exec = vec![ld(arg(2)), jset(libc::PROT_EXEC as u32, 1, 0), allow, deny];
    let unix_only = vec![ld(arg(0)), jeq(libc::AF_UNIX as u32, 0, 1), allow, deny];
    let mut prctl = vec![ld(arg(0))];
    for opt in [
        libc::PR_SET_NAME as u32,
        libc::PR_GET_NAME as u32,
        libc::PR_GET_DUMPABLE as u32,
        libc::PR_GET_NO_NEW_PRIVS as u32,
        libc::PR_GET_SECCOMP as u32,
        libc::PR_CAPBSET_READ as u32,
        PR_SET_VMA,
    ] {
        prctl.extend([jeq(opt, 0, 1), allow]);
    }
    prctl.push(deny);

    let mut v = vec![
        (libc::SYS_mmap, no_exec.clone()),
        (libc::SYS_mprotect, no_exec),
        (libc::SYS_socket, unix_only.clone()),
        (libc::SYS_socketpair, unix_only),
        // A thread, never a process: CLONE_THREAD, and no namespace.
        (
            libc::SYS_clone,
            vec![
                ld(arg(0)),
                jset(CLONE_THREAD, 0, 2),
                jset(CLONE_NS, 1, 0),
                allow,
                deny,
            ],
        ),
        // Its flags are behind a pointer no filter can read; ENOSYS makes the
        // C library use clone instead.
        (
            libc::SYS_clone3,
            vec![ret(SECCOMP_RET_ERRNO | libc::ENOSYS as u32)],
        ),
        // Any request on any descriptor, but none that pushes input into a
        // terminal the backend inherited.
        (
            libc::SYS_ioctl,
            vec![
                ld(arg(1)),
                jeq(TIOCSTI, 1, 0),
                jeq(TIOCLINUX, 0, 1),
                deny,
                allow,
            ],
        ),
        (libc::SYS_prctl, prctl),
        // Only this process's own threads (abort() raises through it).
        (
            libc::SYS_tgkill,
            vec![ld(arg(0)), jeq(pid, 0, 1), allow, deny],
        ),
        // Only its own limits.
        (
            libc::SYS_prlimit64,
            vec![ld(arg(0)), jeq(0, 0, 1), allow, deny],
        ),
        // Any handler but SIGSYS's: that one reports the violation.
        (
            libc::SYS_rt_sigaction,
            vec![
                ld(arg(0)),
                jeq(libc::SIGSYS as u32, 0, 1),
                ret(SECCOMP_RET_KILL_PROCESS),
                allow,
            ],
        ),
        // Removing its own sockets on the way out; nothing else removes files,
        // and Landlock would refuse it anyway.
        (
            libc::SYS_unlinkat,
            vec![ret(SECCOMP_RET_ERRNO | libc::EPERM as u32)],
        ),
    ];
    #[cfg(target_arch = "x86_64")]
    v.push((
        libc::SYS_unlink,
        vec![ret(SECCOMP_RET_ERRNO | libc::EPERM as u32)],
    ));
    v
}

/// The whole program: architecture, then the plain list, then the blocks,
/// then `violation`.
fn program(pid: u32, violation: u32) -> Vec<Insn> {
    let mut p = vec![
        ld(DATA_ARCH),
        jeq(AUDIT_ARCH, 1, 0),
        ret(SECCOMP_RET_KILL_PROCESS),
        ld(DATA_NR),
    ];
    for nr in plain() {
        p.push(jeq(nr as u32, 0, 1));
        p.push(ret(SECCOMP_RET_ALLOW));
    }
    for (nr, block) in special(pid, violation) {
        let skip = u8::try_from(block.len()).expect("a block fits a jump");
        p.push(jeq(nr as u32, 0, skip));
        p.extend(block);
    }
    p.push(ret(violation));
    p
}

fn install_sigsys_handler() -> io::Result<()> {
    crate::sys::proc::install_sigsys_report(REFUSED_EXIT)
}

fn install(prog: &[Insn]) -> io::Result<()> {
    crate::sys::proc::seccomp_install(prog)
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn seccomp() -> Layer {
    if let Err(e) = install_sigsys_handler() {
        return Layer::Degraded(format!("SIGSYS handler: {e}; no filter installed"));
    }
    let pid = crate::sys::proc::pid() as u32;
    let prog = program(pid, SECCOMP_RET_TRAP);
    match install(&prog) {
        Ok(()) => Layer::Enforced(format!(
            "{} syscalls allowed ({} with their arguments checked), {} instructions; \
             anything else stops the process",
            plain().len() + special(pid, SECCOMP_RET_TRAP).len(),
            special(pid, SECCOMP_RET_TRAP).len(),
            prog.len()
        )),
        Err(e) => Layer::Degraded(format!(
            "seccomp(SET_MODE_FILTER): {e}: this kernel has no seccomp filters \
             (CONFIG_SECCOMP_FILTER); every syscall stays open"
        )),
    }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn seccomp() -> Layer {
    Layer::Degraded("no syscall list for this architecture".into())
}

// ── The self-test ────────────────────────────────────────────────────────────

fn thread_count() -> Option<usize> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("Threads:"))?
        .trim()
        .parse()
        .ok()
}

fn status_field(name: &str) -> Option<String> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix(name))
        .map(|v| v.trim().to_string())
}

/// Check each layer from the inside, and demote any that does not hold.
fn verify(r: &mut Report) {
    if r.seccomp.is_enforced() && status_field("Seccomp:").as_deref() != Some("2") {
        r.seccomp = Layer::Degraded("installed, but /proc/self/status does not say so".into());
    }
    if r.landlock.is_enforced() {
        // The root directory is never on the plan.
        if crate::sys::fd::open(c"/", libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC).is_ok()
        {
            r.landlock = Layer::Degraded("installed, but / still opens".into());
        }
    }
    if r.network.is_enforced() && !matches!(only_loopback(), Ok(true)) {
        r.network = Layer::Degraded("interfaces other than lo are visible".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    use crate::sys::proc::testing::{End, forked};

    fn nnp() {
        crate::sys::proc::set_no_new_privs().unwrap();
    }

    fn filter() {
        nnp();
        install_sigsys_handler().unwrap();
        let pid = crate::sys::proc::pid() as u32;
        install(&program(pid, SECCOMP_RET_TRAP)).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nvgpu-sandbox-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_filter_is_well_formed() {
        let p = program(1234, SECCOMP_RET_TRAP);
        assert!(p.len() < 4096, "{} instructions", p.len());
        // The architecture first, and a mismatch is a kill, not a fall-through.
        assert_eq!(p[0], ld(DATA_ARCH));
        assert_eq!(p[1].k, AUDIT_ARCH);
        assert_eq!(p[2], ret(SECCOMP_RET_KILL_PROCESS));
        assert_eq!(p[3], ld(DATA_NR));
        assert_eq!(*p.last().unwrap(), ret(SECCOMP_RET_TRAP));
        // Every jump lands inside the program.
        for (i, insn) in p.iter().enumerate() {
            if insn.code == BPF_JEQ_K || insn.code == BPF_JSET_K {
                assert!(
                    i + 1 + (insn.jt.max(insn.jf) as usize) < p.len(),
                    "jump at {i}"
                );
            }
        }
        // No number is on both lists, and nothing that changes the host is on either.
        let plain = plain();
        let special: Vec<_> = special(1234, SECCOMP_RET_TRAP)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for nr in &special {
            assert!(!plain.contains(nr), "{nr} is on both lists");
        }
        for forbidden in [
            libc::SYS_execve,
            libc::SYS_execveat,
            libc::SYS_ptrace,
            libc::SYS_process_vm_readv,
            libc::SYS_process_vm_writev,
            libc::SYS_mount,
            libc::SYS_unshare,
            libc::SYS_setns,
            libc::SYS_bpf,
            libc::SYS_perf_event_open,
            libc::SYS_userfaultfd,
            libc::SYS_keyctl,
            libc::SYS_init_module,
            libc::SYS_kill,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_landlock_restrict_self,
            libc::SYS_seccomp,
            libc::SYS_io_uring_setup,
            libc::SYS_pkey_mprotect,
        ] {
            assert!(
                !plain.contains(&forbidden) && !special.contains(&forbidden),
                "{forbidden}"
            );
        }
    }

    #[test]
    fn a_filtered_child_keeps_working() {
        let end = forked(|| {
            filter();
            // Threads (clone3 answered ENOSYS, clone with CLONE_THREAD),
            // allocation, memfds mapped read-write, UNIX socket pairs,
            // descriptors passed, eventfds and epoll.
            let t = std::thread::Builder::new()
                .name("sandboxed".into())
                .spawn(|| {
                    let v: Vec<u8> = vec![7; 1 << 20];
                    v.iter().map(|&b| b as u64).sum::<u64>()
                })
                .unwrap();
            if t.join().unwrap() != 7 << 20 {
                return 2;
            }
            if let Err(step) = crate::sys::proc::testing::memfd_mapping_works() {
                return step;
            }
            let (a, b) = UnixStream::pair().unwrap();
            drop((a, b));
            // Its own limits, and its own thread names.
            if crate::posture::raise_nofile().is_err() {
                return 6;
            }
            let _ = std::fs::read_dir("/proc/self/fd").unwrap().count();
            // unlink is refused, quietly.
            if std::fs::remove_file("/nonexistent-nvgpu-sandbox").map_err(|e| e.raw_os_error())
                != Err(Some(libc::EPERM))
            {
                return 7;
            }
            0
        });
        assert_eq!(end, End::Exit(0));
    }

    #[test]
    fn a_forbidden_syscall_stops_a_filtered_child() {
        // Not on the list at all.
        let end = forked(|| {
            filter();
            // A syscall the filter refuses; the child never returns.
            crate::sys::proc::testing::ptrace_traceme();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // A process, not a thread.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::clone_process();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // An executable mapping.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::map_executable();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // An IP socket.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::ip_socket();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // Making the process dumpable again.
        let end = forked(|| {
            filter();
            let _ = crate::sys::proc::set_dumpable(true);
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // Taking the SIGSYS handler away is a kill without it.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::sigsys_default();
            0
        });
        assert_eq!(end, End::Signal(libc::SIGSYS));
        // Pushing input into a terminal.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::tiocsti();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
    }

    /// Whether this kernel has Landlock at `min` or above; a test that needs
    /// it says why it is skipped rather than failing on an older host.
    fn landlock_at(min: i64) -> bool {
        match landlock_abi() {
            Ok(v) if v >= min => true,
            other => {
                eprintln!("skipped: Landlock ABI {min} needed, have {other:?}");
                false
            }
        }
    }

    #[test]
    fn landlock_confines_a_child_to_its_plan() {
        if !landlock_at(1) {
            return;
        }
        let d = tmpdir("ll");
        std::fs::create_dir_all(d.join("allowed")).unwrap();
        std::fs::create_dir_all(d.join("denied")).unwrap();
        std::fs::write(d.join("allowed/f"), b"ok").unwrap();
        std::fs::write(d.join("denied/f"), b"no").unwrap();
        let theirs = d.join("theirs.sock");
        let _theirs = UnixListener::bind(&theirs).unwrap();
        let ours = d.join("ours.sock");
        let _ours = UnixListener::bind(&ours).unwrap();
        let plan = Plan {
            devices: vec![PathBuf::from("/dev/null")],
            read: vec![d.join("allowed"), PathBuf::from("/proc/self")],
            connect: vec![ours.clone()],
        };
        let abi = landlock_abi().unwrap();
        let parent = crate::sys::proc::ppid();
        let end = forked(move || {
            nnp();
            let l = landlock(&plan);
            if abi >= 9 && !l.is_enforced() {
                eprintln!("{l:?}");
                return 2;
            }
            if std::fs::read(d.join("allowed/f")).ok().as_deref() != Some(b"ok".as_slice()) {
                return 3;
            }
            if std::fs::read(d.join("denied/f")).map_err(|e| e.raw_os_error())
                != Err(Some(libc::EACCES))
            {
                return 4;
            }
            // No write, even where reading is allowed.
            if std::fs::write(d.join("allowed/g"), b"x").is_ok() {
                return 5;
            }
            if std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .is_err()
            {
                return 6;
            }
            // A memfd reopened through /proc/self/fd, as shm.rs does.
            let Ok(m) = crate::sys::fd::memfd(c"m", libc::MFD_CLOEXEC) else {
                return 7;
            };
            if std::fs::File::open(format!("/proc/self/fd/{}", m.as_raw_fd())).is_err() {
                return 7;
            }
            if abi >= 9 {
                if UnixStream::connect(&ours).is_err() {
                    return 8;
                }
                if UnixStream::connect(&theirs).is_ok() {
                    return 9;
                }
            }
            if abi >= 6 {
                // Signal 0 only asks.
                if crate::sys::proc::kill(parent, 0).is_ok() {
                    return 10;
                }
            }
            0
        });
        assert_eq!(end, End::Exit(0));
    }

    /// A process with a second thread already gets no layer at all: the
    /// sandbox is not installed half-way.
    #[test]
    fn a_process_with_two_threads_gets_no_layer() {
        let end = forked(|| {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let t = std::thread::spawn(move || rx.recv().ok());
            // Unchanged, not 0: a build sandbox (Nix's) may have filtered
            // the test process before it started. The filter count where
            // the kernel reports one (5.9 on), else the mode.
            let filters = || status_field("Seccomp_filters:").or_else(|| status_field("Seccomp:"));
            let seccomp_before = filters();
            let r = apply(&Plan::default());
            let none = [&r.network, &r.limits, &r.landlock, &r.seccomp]
                .iter()
                .all(|l| matches!(l, Layer::Degraded(s) if s.contains("threads already")));
            let unfiltered = filters() == seccomp_before;
            drop(tx);
            let _ = t.join();
            if r.complete() {
                return 2;
            }
            if !none {
                return 3;
            }
            if !unfiltered {
                return 4;
            }
            0
        });
        assert_eq!(end, End::Exit(0));
    }

    /// A release build panics with `panic = "abort"`: the default hook
    /// writes the message, then `abort()` blocks signals, raises SIGABRT
    /// with `tgkill` on its own thread, and resets the handler if one
    /// caught it. All of that must be on the list, from the main thread and
    /// from a worker, or a panic would read as a sandbox violation (status
    /// 159) instead of the abort it is.
    #[test]
    fn a_panic_under_abort_is_an_abort_not_a_violation() {
        fn panic_aborting() -> ! {
            let default = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                default(info);
                std::process::abort();
            }));
            panic!("a release build's panic");
        }
        let end = forked(|| {
            filter();
            panic_aborting();
        });
        assert_eq!(end, End::Signal(libc::SIGABRT));
        let end = forked(|| {
            filter();
            let _ = std::thread::Builder::new()
                .name("vring-worker".into())
                .spawn(|| panic_aborting())
                .unwrap()
                .join();
            0
        });
        assert_eq!(end, End::Signal(libc::SIGABRT));
        // And an abort() with a handler installed, which glibc resets to
        // the default and raises again.
        let end = forked(|| {
            filter();
            crate::sys::proc::testing::sigabrt_ignored();
            std::process::abort();
        });
        assert_eq!(end, End::Signal(libc::SIGABRT));
    }

    /// The filter reaches a thread that existed before it (TSYNC).
    #[test]
    fn the_filter_reaches_a_thread_made_before_it() {
        let end = forked(|| {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            let t = std::thread::spawn(move || {
                if rx.recv().is_ok() {
                    crate::sys::proc::testing::ptrace_traceme();
                }
                0
            });
            filter();
            let _ = tx.send(());
            let _ = t.join();
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
    }

    #[test]
    fn the_modes_parse_and_print() {
        for (s, m) in [
            ("on", Mode::On),
            ("best-effort", Mode::BestEffort),
            ("off", Mode::Off),
        ] {
            assert_eq!(s.parse::<Mode>(), Ok(m));
            assert_eq!(m.to_string(), s);
        }
        assert!("partial".parse::<Mode>().is_err());
        assert_eq!(Mode::default(), Mode::On);
    }

    #[test]
    fn the_whole_sandbox_applies_in_a_single_threaded_child() {
        // The fork below leaves one thread, which is what the network half
        // needs; a host without unprivileged user namespaces or Landlock
        // reports DEGRADED, which this checks is said rather than hidden.
        let end = forked(|| {
            // As the backend is by then.
            if crate::posture::drop_all_caps().is_err() || crate::posture::set_undumpable().is_err()
            {
                return 2;
            }
            let plan = Plan {
                devices: vec![PathBuf::from("/dev/null")],
                read: vec![PathBuf::from("/proc/self")],
                connect: vec![],
            };
            let r = apply(&plan);
            eprintln!("{r:#?}");
            if !r.seccomp.is_enforced() || !r.limits.is_enforced() {
                return 3;
            }
            if r.network.is_enforced() && !matches!(only_loopback(), Ok(true)) {
                return 4;
            }
            // Still works: a thread, and our own uid unchanged by the namespace.
            let t = std::thread::spawn(|| 5).join().unwrap();
            if t != 5 {
                return 5;
            }
            if crate::sys::proc::euid() == 65534 {
                return 6;
            }
            if crate::sys::proc::dumpable() != 0 {
                return 8;
            }
            // And what it takes away, it takes away.
            if r.landlock.is_enforced() && std::fs::read_dir("/").is_ok() {
                return 7;
            }
            0
        });
        assert_eq!(end, End::Exit(0));
    }

    /// On a host with the NVIDIA driver: what the backend reads and opens at
    /// run time (nvidia/hostnodes.rs's node enumeration, GET_PROC_FILES and
    /// GET_SYS_FILES, a guest's OPEN) stays reachable under the plan, and a
    /// path off it does not. Opens only; no ioctl reaches the driver.
    #[test]
    fn the_real_hosts_gpu_paths_stay_reachable() {
        let proc = Path::new(crate::host::PROC_NVIDIA);
        let gpus: Vec<String> = crate::host::gpu_slots(proc)
            .iter()
            .map(|g| g.address())
            .collect();
        if gpus.is_empty() || !Path::new("/dev/nvidiactl").exists() || !landlock_at(5) {
            eprintln!("skipped: no NVIDIA GPU or Landlock ABI 5 here");
            return;
        }
        let plan = Plan::backend(
            &BackendPaths {
                proc_nvidia: proc,
                gpus: gpus.clone(),
                compute: false,
                kms_card: false,
                wayland_socket: None,
                export_socket: None,
            },
            Path::new("/dev"),
            Path::new("/sys"),
        );
        let end = forked(move || {
            nnp();
            if !landlock(&plan).is_enforced() {
                return 2;
            }
            let rw = |p: &str| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(p)
                    .is_ok()
            };
            if !rw("/dev/nvidiactl") || !rw("/dev/nvidia0") || !rw("/dev/null") {
                return 3;
            }
            if std::fs::read_to_string("/proc/driver/nvidia/version").is_err()
                || std::fs::read_to_string("/proc/cpuinfo").is_err()
            {
                return 4;
            }
            for addr in &gpus {
                let dir = format!("/sys/bus/pci/devices/{addr}");
                if std::fs::read(format!("{dir}/config")).is_err() {
                    return 5;
                }
                let Ok(nodes) = std::fs::read_dir(format!("{dir}/drm")) else {
                    return 6;
                };
                for n in nodes.flatten() {
                    let n = n.file_name().to_string_lossy().into_owned();
                    if !n.starts_with("renderD") {
                        continue;
                    }
                    if std::fs::read_to_string(format!("/sys/class/drm/{n}/dev")).is_err() {
                        return 7;
                    }
                    if !rw(&format!("/dev/dri/{n}")) {
                        return 8;
                    }
                }
            }
            // Off the plan: another device, other sysfs, the host's files.
            if rw("/dev/kvm") || std::fs::read_dir("/sys/class/net").is_ok() {
                return 9;
            }
            if std::fs::read("/etc/hostname").is_ok() || std::fs::read_dir("/home").is_ok() {
                return 10;
            }
            0
        });
        assert_eq!(end, End::Exit(0));
    }

    #[test]
    fn the_backend_plan_names_this_gpus_nodes_only() {
        let d = tmpdir("plan");
        let dev = d.join("dev");
        let sys = d.join("sys");
        std::fs::create_dir_all(dev.join("dri")).unwrap();
        for n in [
            "nvidia0",
            "nvidia1",
            "nvidiactl",
            "nvidia-modeset",
            "nvidia-uvm",
            "nvidia-caps",
            "kvm",
        ] {
            std::fs::write(dev.join(n), b"").unwrap();
        }
        let gpu = sys.join("devices/pci0000:00/0000:01:00.0");
        std::fs::create_dir_all(gpu.join("drm/card1")).unwrap();
        std::fs::create_dir_all(gpu.join("drm/renderD129")).unwrap();
        std::fs::create_dir_all(sys.join("bus/pci/devices")).unwrap();
        std::os::unix::fs::symlink(&gpu, sys.join("bus/pci/devices/0000:01:00.0")).unwrap();
        let p = BackendPaths {
            proc_nvidia: Path::new("/proc/driver/nvidia"),
            gpus: vec!["0000:01:00.0".into()],
            compute: false,
            kms_card: false,
            wayland_socket: Some(Path::new("/run/user/1000/wayland-1")),
            export_socket: None,
        };
        let plan = Plan::backend(&p, &dev, &sys);
        let names: Vec<String> = plan
            .devices
            .iter()
            .map(|p| p.strip_prefix(&dev).unwrap().display().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "nvidiactl",
                "nvidia-modeset",
                "nvidia0",
                "nvidia1",
                "udmabuf",
                "null",
                "dri/renderD129"
            ]
        );
        assert!(plan.read.contains(&std::fs::canonicalize(&gpu).unwrap()));
        assert_eq!(plan.connect, [PathBuf::from("/run/user/1000/wayland-1")]);

        let p = BackendPaths {
            compute: true,
            kms_card: true,
            ..p
        };
        let plan = Plan::backend(&p, &dev, &sys);
        let has = |n: &str| plan.devices.contains(&dev.join(n));
        assert!(has("nvidia-uvm") && has("nvidia-uvm-tools") && has("dri/card1"));
        assert!(!has("kvm") && !has("nvidia-caps"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
