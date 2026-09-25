//! The backend's process sandbox: what it may call, open and reach once it
//! has opened what it must at start.
//!
//! `posture` makes the backend an unprivileged process: no capabilities,
//! no_new_privs, undumpable. That leaves it everything its uid has -- every
//! file the uid can open, every socket it can connect to, every syscall the
//! kernel offers -- and one process per VM maps all of that guest's RAM and
//! parses everything the guest sends (SECURITY.md §4). This takes the rest
//! away, in four layers, each installed once, before the first guest message
//! and before the process has a second thread:
//!
//! 1. **No network.** The backend needs none: every socket it uses is a
//!    pathname UNIX socket or a netlink socket opened before this. If the
//!    launcher already put it in a network namespace of its own (one with
//!    only a loopback interface, as `scripts/run-guest.sh` does as root),
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
//! Every layer degrades rather than stops the backend on a kernel that lacks
//! it -- a host whose kernel has no Landlock still runs guests -- and says so
//! in a line beginning `sandbox: DEGRADED`, at warning level, every start.
//! `--sandbox=off` turns all four off, for finding out whether the sandbox
//! is what broke something, and says so just as loudly.
//!
//! What this does not do: it keeps a compromised backend from the rest of
//! the host, and from other VMs' backends and VMMs, but not from the host
//! kernel it can still call (ioctl on the GPU's nodes above all) or from the
//! guest whose memory it maps. The GPU's own isolation between clients is
//! RM's page tables, which no process sandbox touches.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// `--sandbox`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    On,
    /// For diagnosis only.
    Off,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "on" => Ok(Mode::On),
            "off" => Ok(Mode::Off),
            _ => Err(format!("{s:?}: on or off")),
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Mode::On => "on",
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
    let threads = thread_count();
    let network = match threads {
        Some(n) if n > 1 => Layer::Degraded(format!(
            "{n} threads already; a user namespace needs one (a bug in the start-up order)"
        )),
        _ => private_network(),
    };
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
    // SAFETY: plain syscalls.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    // SAFETY: unshare with constant flags and no pointers.
    if unsafe { libc::unshare(CLONE_NEWUSER | CLONE_NEWNET) } != 0 {
        let e = io::Error::last_os_error();
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
    // SAFETY: plain prctls.
    let was_dumpable = unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) };
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
    let maps = [
        ("/proc/self/setgroups", "deny".to_string()),
        ("/proc/self/uid_map", format!("{uid} {uid} 1")),
        ("/proc/self/gid_map", format!("{gid} {gid} 1")),
    ];
    for (path, text) in &maps {
        if let Err(e) = std::fs::write(path, text) {
            log::error!("sandbox: writing {path} in the new user namespace: {e}; stopping");
            // SAFETY: plain exit; nothing has been served.
            unsafe { libc::_exit(1) };
        }
    }
    // SAFETY: plain prctl.
    if was_dumpable != 1 && unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        log::error!(
            "sandbox: undumpable again: {}; stopping",
            io::Error::last_os_error()
        );
        // SAFETY: as above.
        unsafe { libc::_exit(1) };
    }
    // The new namespace gave this process every capability in it. None of
    // them reach anything outside it, and none of them are kept.
    if let Err(e) = crate::posture::drop_all_caps() {
        log::error!("sandbox: dropping the new user namespace's capabilities: {e}; stopping");
        // SAFETY: as above.
        unsafe { libc::_exit(1) };
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
    let zero = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: setrlimit from a local.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &zero) } != 0 {
        return Layer::Degraded(format!("RLIMIT_CORE: {}", io::Error::last_os_error()));
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

    pub const CREATE_RULESET_VERSION: u32 = 1 << 0;
    pub const RULE_PATH_BENEATH: libc::c_int = 1;
}

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

/// The Landlock ABI this kernel speaks, or why none.
fn landlock_abi() -> Result<i64, io::Error> {
    // SAFETY: the version query takes no attribute.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            ll::CREATE_RULESET_VERSION,
        )
    };
    if v < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
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

fn owned(fd: libc::c_long) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just returned, owned from here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) })
}

/// Add one rule; `Ok(false)` when there is nothing at `path` (a node that
/// does not exist at start stays unreachable, which the caller reports).
fn add_rule(ruleset: &OwnedFd, path: &Path, access: u64, handled: u64) -> io::Result<bool> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: a NUL-terminated path; the result is owned below.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(e)
        };
    }
    let fd = owned(fd as libc::c_long)?;
    // SAFETY: fstat into a local.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut allowed = access & handled;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        allowed &= ll::ACCESS_FILE;
    }
    if allowed == 0 {
        // Nothing this kernel restricts: nothing to grant.
        return Ok(true);
    }
    let attr = PathBeneathAttr {
        allowed_access: allowed,
        parent_fd: fd.as_raw_fd(),
    };
    // SAFETY: a path-beneath attribute of the kernel's layout.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset.as_raw_fd(),
            ll::RULE_PATH_BENEATH,
            &attr as *const PathBeneathAttr,
            0u32,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
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
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: handled_net(abi),
        scoped: scoped(abi),
    };
    // SAFETY: the attribute, of the size this ABI takes.
    let ruleset = match owned(unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            attr_size(abi),
            0u32,
        )
    }) {
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
    // SAFETY: no_new_privs is set (posture::drop_all_caps).
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0u32) } != 0 {
        return Layer::Degraded(format!(
            "landlock_restrict_self: {}",
            io::Error::last_os_error()
        ));
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

const SECCOMP_SET_MODE_FILTER: libc::c_ulong = 1;

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

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct Fprog {
    len: u16,
    filter: *const Insn,
}

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

/// The SIGSYS handler: one line on stderr naming the syscall, then exit.
/// Async-signal-safe: `write` and `exit_group`, both on the list.
extern "C" fn on_sigsys(_sig: libc::c_int, info: *mut libc::siginfo_t, _ctx: *mut libc::c_void) {
    // siginfo's _sigsys: call_addr at 16, syscall at 24, arch at 28, on both
    // 64-bit architectures this builds for.
    let nr = if info.is_null() {
        -1
    } else {
        // SAFETY: the kernel hands a full siginfo_t for a seccomp SIGSYS.
        unsafe { *(info.cast::<u8>().add(24).cast::<i32>()) }
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
    // SAFETY: a local buffer and its length; then an exit that never returns.
    unsafe {
        libc::write(2, buf.as_ptr().cast(), n);
        libc::syscall(libc::SYS_exit_group, REFUSED_EXIT);
    }
}

fn install_sigsys_handler() -> io::Result<()> {
    // SAFETY: a zeroed sigaction is valid; the fields used are set.
    let mut sa: libc::sigaction = unsafe { std::mem::zeroed() };
    sa.sa_sigaction = on_sigsys as *const () as usize;
    sa.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER;
    // SAFETY: an initialised sigaction that outlives the call.
    if unsafe { libc::sigaction(libc::SIGSYS, &sa, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn install(prog: &[Insn]) -> io::Result<()> {
    let len = u16::try_from(prog.len()).map_err(|_| io::Error::other("filter too long"))?;
    let fprog = Fprog {
        len,
        filter: prog.as_ptr(),
    };
    // SAFETY: the program outlives the call, and the kernel copies it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0u32,
            &fprog as *const Fprog,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn seccomp() -> Layer {
    if let Err(e) = install_sigsys_handler() {
        return Layer::Degraded(format!("SIGSYS handler: {e}; no filter installed"));
    }
    // SAFETY: plain syscall.
    let pid = unsafe { libc::getpid() } as u32;
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
        // SAFETY: a constant path; closed below if it opens.
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: the descriptor just opened.
            unsafe { libc::close(fd) };
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

    /// How a forked child ended.
    #[derive(Debug, PartialEq, Eq)]
    enum End {
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
    fn forked(f: impl FnOnce() -> i32) -> End {
        // SAFETY: fork; the child only runs `f` and exits without unwinding.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            // SAFETY: closes this child's own copies; the parent's are untouched.
            unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 0u32) };
            let code = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(101);
            // SAFETY: the child ends here.
            unsafe { libc::_exit(code) };
        }
        let mut status = 0;
        // SAFETY: waits for the child just forked.
        assert_eq!(unsafe { libc::waitpid(pid, &mut status, 0) }, pid);
        if libc::WIFEXITED(status) {
            End::Exit(libc::WEXITSTATUS(status))
        } else {
            End::Signal(libc::WTERMSIG(status))
        }
    }

    fn nnp() {
        // SAFETY: plain prctl.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
            0
        );
    }

    fn filter() {
        nnp();
        install_sigsys_handler().unwrap();
        // SAFETY: plain syscall.
        let pid = unsafe { libc::getpid() } as u32;
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
            // SAFETY: plain syscalls on descriptors made here.
            unsafe {
                let m = libc::memfd_create(c"t".as_ptr(), libc::MFD_CLOEXEC);
                if m < 0 || libc::ftruncate(m, 4096) != 0 {
                    return 3;
                }
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    4096,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    m,
                    0,
                );
                if p == libc::MAP_FAILED {
                    return 4;
                }
                *(p as *mut u8) = 1;
                libc::munmap(p, 4096);
                let e = libc::eventfd(0, libc::EFD_CLOEXEC);
                let ep = libc::epoll_create1(libc::EPOLL_CLOEXEC);
                if e < 0 || ep < 0 {
                    return 5;
                }
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
            // SAFETY: a syscall the filter refuses; the child never returns.
            unsafe { libc::syscall(libc::SYS_ptrace, libc::PTRACE_TRACEME, 0, 0, 0) };
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // A process, not a thread.
        let end = forked(|| {
            filter();
            // SAFETY: as above.
            unsafe { libc::syscall(libc::SYS_clone, libc::SIGCHLD, 0, 0, 0, 0) };
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // An executable mapping.
        let end = forked(|| {
            filter();
            // SAFETY: as above.
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
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // An IP socket.
        let end = forked(|| {
            filter();
            // SAFETY: as above.
            unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // Making the process dumpable again.
        let end = forked(|| {
            filter();
            // SAFETY: as above.
            unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
            0
        });
        assert_eq!(end, End::Exit(REFUSED_EXIT));
        // Taking the SIGSYS handler away is a kill without it.
        let end = forked(|| {
            filter();
            // SAFETY: as above.
            unsafe { libc::signal(libc::SIGSYS, libc::SIG_DFL) };
            0
        });
        assert_eq!(end, End::Signal(libc::SIGSYS));
        // Pushing input into a terminal.
        let end = forked(|| {
            filter();
            let c = 0u8;
            // SAFETY: as above.
            unsafe { libc::ioctl(0, TIOCSTI as _, &c) };
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
        // SAFETY: plain syscall.
        let parent = unsafe { libc::getppid() };
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
            // SAFETY: plain syscall.
            let m = unsafe { libc::memfd_create(c"m".as_ptr(), libc::MFD_CLOEXEC) };
            if m < 0 || std::fs::File::open(format!("/proc/self/fd/{m}")).is_err() {
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
                // SAFETY: signal 0 only asks.
                if unsafe { libc::kill(parent, 0) } == 0 {
                    return 10;
                }
            }
            0
        });
        assert_eq!(end, End::Exit(0));
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
            // SAFETY: plain syscall.
            if unsafe { libc::geteuid() } == 65534 {
                return 6;
            }
            // SAFETY: plain prctl.
            if unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0 {
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
    /// run time (nvidia.rs's node enumeration, GET_PROC_FILES and
    /// GET_SYS_FILES, a guest's OPEN) stays reachable under the plan, and a
    /// path off it does not. Opens only; no ioctl reaches the driver.
    #[test]
    fn the_real_hosts_gpu_paths_stay_reachable() {
        let proc = Path::new(crate::host::PROC_NVIDIA);
        let gpus: Vec<String> = crate::host::gpu_slots(proc)
            .iter()
            .map(|g| {
                let end = g
                    .pci_addr
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(g.pci_addr.len());
                String::from_utf8_lossy(&g.pci_addr[..end]).into_owned()
            })
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
