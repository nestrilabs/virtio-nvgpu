// SPDX-License-Identifier: Apache-2.0
//
// The backend's own privileges, checked and dropped before it serves a guest.
//
// The host NVIDIA driver takes a caller's privilege from its credentials: RM
// treats a process with CAP_SYS_ADMIN as an administrator, and every guest
// process's RM calls are made by this process. A backend running as root
// therefore makes every guest process an RM administrator, which among other
// things lets it map all of BAR0 read-write. So the backend refuses to start
// as root or with CAP_SYS_ADMIN, and drops whatever capabilities it has before
// it opens a device.

use std::io;

/// CAP_SYS_ADMIN's bit (linux/capability.h).
const CAP_SYS_ADMIN: u32 = 21;

/// The capability sets of a process, as `/proc/<pid>/status` gives them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub effective: u64,
    pub permitted: u64,
}

impl Caps {
    /// The sets of this process.
    pub fn current() -> io::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string("/proc/self/status")?))
    }

    /// The `CapEff:` and `CapPrm:` lines of a status file. A set the file does
    /// not have reads as empty.
    pub fn parse(status: &str) -> Self {
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
                .unwrap_or(0)
        };
        Self {
            effective: field("CapEff:"),
            permitted: field("CapPrm:"),
        }
    }

    pub fn has_sys_admin(&self) -> bool {
        (self.effective | self.permitted) & (1 << CAP_SYS_ADMIN) != 0
    }
}

/// Why this process should not serve a guest, if it should not.
pub fn refusal(euid: u32, caps: Caps) -> Option<String> {
    if euid == 0 {
        return Some("it runs as root".into());
    }
    if caps.has_sys_admin() {
        return Some(format!(
            "it has CAP_SYS_ADMIN (effective {:#x}, permitted {:#x})",
            caps.effective, caps.permitted
        ));
    }
    None
}

/// Check the process may serve a guest, then drop every capability it has and
/// set no_new_privs. `allow_root` lets a root or CAP_SYS_ADMIN process go on,
/// with a warning: guest processes are then RM administrators.
pub fn enforce(allow_root: bool) -> anyhow::Result<()> {
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    let caps = Caps::current()?;
    if let Some(why) = refusal(euid, caps) {
        if !allow_root {
            anyhow::bail!(
                "refusing to start: {why}. The host driver takes a guest's privilege from the \
                 backend's, so every guest process would be an RM administrator. Run the \
                 backend as an unprivileged user that can open /dev/nvidia* and the GPU's \
                 render node, or pass --allow-root-unsafe for a test rig"
            );
        }
        log::warn!(
            "--allow-root-unsafe: serving although {why}; every guest process is an RM \
             administrator. Never run a guest you do not trust like this"
        );
        return Ok(());
    }
    drop_all()?;
    let after = Caps::current()?;
    log::info!(
        "privileges: uid {euid}, capabilities effective {:#x}, permitted {:#x}, no_new_privs set",
        after.effective,
        after.permitted
    );
    Ok(())
}

/// Empty the ambient, inheritable, effective and permitted capability sets and
/// set no_new_privs. The bounding set is left as it is: changing it takes
/// CAP_SETPCAP, which this process has just been made not to have, and with
/// no_new_privs nothing it execs can gain from it.
fn drop_all() -> io::Result<()> {
    // SAFETY: prctl with PR_CAP_AMBIENT_CLEAR_ALL takes no pointers.
    let r = unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    // EINVAL: a kernel without ambient capabilities, which has none to clear.
    if r != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) {
        return Err(io::Error::last_os_error());
    }
    // linux/capability.h, _LINUX_CAPABILITY_VERSION_3: two data words.
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [Data::default(); 2];
    // SAFETY: capset reads one header and two data words, which both live for
    // the call; pid 0 is this thread.
    let r = unsafe { libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr()) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: prctl with PR_SET_NO_NEW_PRIVS takes no pointers.
    let r = unsafe {
        libc::prctl(
            libc::PR_SET_NO_NEW_PRIVS,
            1 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
            0 as libc::c_ulong,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_sys_admin_are_refused_and_nothing_else() {
        let none = Caps::default();
        assert!(refusal(0, none).is_some());
        assert!(refusal(1000, none).is_none());
        let admin = Caps {
            effective: 0,
            permitted: 1 << CAP_SYS_ADMIN,
        };
        assert!(refusal(1000, admin).is_some());
        let other = Caps {
            effective: 1 << 12, // CAP_NET_ADMIN
            permitted: 1 << 12,
        };
        assert!(refusal(1000, other).is_none());
    }

    #[test]
    fn capability_sets_are_read_from_the_status_file() {
        let status = "Name:\tx\nCapInh:\t0000000000000000\nCapPrm:\t000001ffffffffff\n\
                      CapEff:\t0000000000200000\nCapBnd:\t000001ffffffffff\n";
        let c = Caps::parse(status);
        assert_eq!(c.permitted, 0x1ff_ffff_ffff);
        assert_eq!(c.effective, 1 << CAP_SYS_ADMIN);
        assert!(c.has_sys_admin());
        assert_eq!(Caps::parse(""), Caps::default());
    }
}
