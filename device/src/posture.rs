//! What the backend process is allowed to be: its privileges and its socket.
//!
//! **Privileges** (S-5). Every host driver the backend forwards to decides a
//! guest's privilege by the backend's credentials, because the backend is
//! the caller. NVIDIA RM makes the client of a process with CAP_SYS_ADMIN an
//! administrator (escape.c:381, osIsAdministrator = capable(CAP_SYS_ADMIN),
//! os-interface.c:389-391): BAR0 mappable read-write in full
//! (osapi.c:2209-2213, 2536) and the register allowlist skipped
//! (gpu_access.c:1230). DRM files opened with it are authenticated
//! (drm_file.c:149) and pass every master check (drm_auth.c:239). So a
//! backend run as root makes every unprivileged guest process all of that,
//! and from BAR0 and the GPU's copy engines, the host kernel is one DMA away.
//! Nothing here needs any of it: /dev/nvidia* is 0666, render and card nodes
//! are group video/render, /dev/udmabuf group kvm, card master comes from
//! being the first opener, and the uevent group is readable unprivileged.
//!
//! So the backend refuses to start with euid 0 or with CAP_SYS_ADMIN
//! effective or permitted unless the operator says `--allow-root-unsafe`,
//! and in every case gives up every capability before its first thread
//! exists: effective, permitted and inheritable emptied, ambient cleared,
//! the bounding set emptied where it may be, and no_new_privs set so nothing
//! it executes can gain one back. Capabilities are per thread, which is why
//! it has to be first: a thread made before the drop would keep them. Every
//! host open after that (per guest process, at run time) is a plain file
//! open under the backend's uid and groups.
//!
//! **Socket** (S-23). The vhost-user socket is how a VMM hands the backend
//! all of a guest's memory. A path another local user can create first is a
//! path at which that user receives it: the VMM connects to whatever is
//! listening there. The default is therefore under `$XDG_RUNTIME_DIR` in a
//! directory of the backend's own (mode 0700), not /tmp, and whatever path is
//! used, a file already there is removed only if it is a socket the backend's
//! user owns -- anything else stops the start instead of being ignored.

use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// CAP_SYS_ADMIN (linux/capability.h).
pub const CAP_SYS_ADMIN: u32 = 21;
/// CAP_SETPCAP: what emptying the bounding set takes.
const CAP_SETPCAP: u32 = 8;

const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// The calling thread's capability sets, 64 bits each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Caps {
    pub effective: u64,
    pub permitted: u64,
    pub inheritable: u64,
}

impl Caps {
    /// The calling thread's.
    pub fn current() -> io::Result<Self> {
        let mut hdr = CapHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapData::default(); 2];
        // SAFETY: capget with a v3 header writes two CapData.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_capget,
                &mut hdr as *mut CapHeader,
                data.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        let wide = |lo: u32, hi: u32| u64::from(lo) | (u64::from(hi) << 32);
        Ok(Self {
            effective: wide(data[0].effective, data[1].effective),
            permitted: wide(data[0].permitted, data[1].permitted),
            inheritable: wide(data[0].inheritable, data[1].inheritable),
        })
    }

    pub fn has(&self, cap: u32) -> bool {
        (self.effective | self.permitted) & (1u64 << cap) != 0
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

impl std::fmt::Display for Caps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "effective {:#x}, permitted {:#x}, inheritable {:#x}",
            self.effective, self.permitted, self.inheritable
        )
    }
}

/// Why the backend will not start with the privileges it has, if it will
/// not: `euid` and `caps` as it found them.
pub fn too_privileged(euid: u32, caps: &Caps) -> Option<String> {
    if euid == 0 {
        Some("it runs as root (euid 0)".into())
    } else if caps.has(CAP_SYS_ADMIN) {
        Some(format!("it holds CAP_SYS_ADMIN ({caps})"))
    } else {
        None
    }
}

/// Give up every capability for good, on the calling thread and everything
/// it later creates or executes. Call before the first thread is spawned.
pub fn drop_all_caps() -> io::Result<()> {
    // Ambient first: it may be cleared by anyone, and an ambient capability
    // would otherwise survive an execve of a plain binary.
    // SAFETY: plain prctls with integer arguments.
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };
    // The bounding set takes CAP_SETPCAP to shrink, so only a process that
    // could have used it does; for any other no_new_privs below is what
    // stops an execve gaining file capabilities.
    if Caps::current()?.effective & (1u64 << CAP_SETPCAP) != 0 {
        let mut cap = 0;
        // SAFETY: as above; EINVAL past the kernel's last capability ends it.
        while unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) } == 0 {
            cap += 1;
        }
    }
    let mut hdr = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    // SAFETY: capset with a v3 header reads two CapData.
    let rc = unsafe { libc::syscall(libc::SYS_capset, &mut hdr as *mut CapHeader, data.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: plain prctl.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let left = Caps::current()?;
    if !left.is_empty() {
        return Err(io::Error::other(format!(
            "capabilities left after dropping them all: {left}"
        )));
    }
    Ok(())
}

/// Make the process undumpable: no core file of guest memory, and no
/// ptrace or /proc/<pid>/mem by other processes of the same uid. The
/// process's own /proc/self/fd stays readable to it (proc_fd_permission,
/// and ptrace_may_access passes its own thread group).
pub fn set_undumpable() -> io::Result<()> {
    // SAFETY: plain prctl.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The socket path used when none is given: `$XDG_RUNTIME_DIR/nvgpu/nvgpu.sock`.
/// `None` without an XDG_RUNTIME_DIR; there is no shared-directory default.
pub fn default_socket(xdg_runtime_dir: Option<&Path>) -> Option<PathBuf> {
    Some(xdg_runtime_dir?.join("nvgpu").join("nvgpu.sock"))
}

/// Make `dir` a directory only `euid` can use: created with mode 0700 if
/// missing; refused if it exists owned by another user or open to others.
pub fn private_dir(dir: &Path, euid: u32) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let m = std::fs::symlink_metadata(dir)?;
    if !m.is_dir() {
        return Err(io::Error::other(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    if m.uid() != euid {
        return Err(io::Error::other(format!(
            "{} belongs to uid {}, not to us ({euid})",
            dir.display(),
            m.uid()
        )));
    }
    if m.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::other(format!(
            "{} is open to other users (mode {:o})",
            dir.display(),
            m.permissions().mode() & 0o777
        )));
    }
    Ok(())
}

/// Clear the way to bind a socket at `path`: nothing there is fine; a socket
/// of `euid`'s is a stale one of ours and is removed; anything else -- a file
/// that is not a socket, or a socket another user made -- stops the start,
/// as does a removal that fails. Previously the removal's result was
/// ignored, so a socket another user had put in a shared directory stayed,
/// the bind failed, and a VMM started alongside connected to that user's
/// socket instead.
pub fn clear_socket_path(path: &Path, euid: u32) -> io::Result<()> {
    let m = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if !m.file_type().is_socket() {
        return Err(io::Error::other(format!(
            "{} exists and is not a socket; not removing it",
            path.display()
        )));
    }
    if m.uid() != euid {
        return Err(io::Error::other(format!(
            "{} is a socket of uid {}, not ours ({euid}): someone else may be listening there",
            path.display(),
            m.uid()
        )));
    }
    std::fs::remove_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nvgpu-posture-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir(&d).unwrap();
        d
    }

    fn euid() -> u32 {
        // SAFETY: plain syscall.
        unsafe { libc::geteuid() }
    }

    #[test]
    fn root_or_sys_admin_is_too_privileged_and_nothing_else_is() {
        let admin = Caps {
            permitted: 1 << CAP_SYS_ADMIN,
            ..Caps::default()
        };
        assert!(too_privileged(0, &Caps::default()).is_some());
        assert!(too_privileged(1000, &admin).is_some());
        let net = Caps {
            effective: 1 << 12,
            permitted: 1 << 12,
            ..Caps::default()
        };
        assert!(too_privileged(1000, &net).is_none());
        assert!(too_privileged(1000, &Caps::default()).is_none());
    }

    /// Run on a thread of its own: capabilities and no_new_privs are per
    /// thread, and the drop must not reach the rest of the test binary.
    #[test]
    fn after_the_drop_no_capability_is_left_and_none_can_be_gained() {
        std::thread::spawn(|| {
            drop_all_caps().unwrap();
            assert!(Caps::current().unwrap().is_empty());
            // SAFETY: plain prctl.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) },
                1
            );
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_default_socket_lives_in_the_runtime_dir_or_nowhere() {
        assert_eq!(
            default_socket(Some(Path::new("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/nvgpu/nvgpu.sock"))
        );
        assert_eq!(default_socket(None), None);
    }

    #[test]
    fn a_private_dir_is_made_0700_and_one_open_to_others_is_refused() {
        let d = tmpdir("dir");
        let p = d.join("nvgpu");
        private_dir(&p, euid()).unwrap();
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o700
        );
        private_dir(&p, euid()).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o733)).unwrap();
        assert!(private_dir(&p, euid()).is_err());
        assert!(private_dir(&p, euid().wrapping_add(1)).is_err(), "not ours");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_a_stale_socket_of_ours_is_cleared_away() {
        let d = tmpdir("sock");
        let s = d.join("nvgpu.sock");
        clear_socket_path(&s, euid()).unwrap();

        drop(UnixListener::bind(&s).unwrap());
        assert!(
            clear_socket_path(&s, euid().wrapping_add(1)).is_err(),
            "someone else's"
        );
        assert!(s.exists());
        clear_socket_path(&s, euid()).unwrap();
        assert!(!s.exists());

        std::fs::write(&s, b"not a socket").unwrap();
        assert!(clear_socket_path(&s, euid()).is_err());
        assert!(s.exists(), "left alone");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
