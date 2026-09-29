// SPDX-License-Identifier: Apache-2.0
//! RM controls the backend answers itself, without asking the host.
//!
//! An RM control the host release's allowlist has (`rmallow.rs`) is one any
//! unprivileged host process could call, and an unprivileged guest process
//! calls it as the backend, a client of the host's RM like any other. These
//! are answered here, before that list is consulted, with RM's own answer
//! to a caller without the privilege. Most controls say something only
//! about the guest's own objects or about the GPU. These say something about
//! every other client of the GPU on the host (S-24): the host PIDs of the
//! host compositor, of other VMs' backends and of host CUDA jobs, and per PID
//! how much video memory each holds and which engine each was running when
//! sampled. That is a cross-VM activity side channel and a list of host PIDs
//! for whatever comes next.
//!
//! RM walks every client for these (gpuGetProcWithObject, gpu_rmapi.c:855-873)
//! and filters only by PID namespace (os_find_ns_pid, gpu_rmapi.c:980-988).
//! Running each backend in a PID namespace of its own (systemd
//! `PrivatePIDs=yes`, `bwrap --unshare-pid`) makes RM itself answer with the
//! backend alone, and is the real fix; this list is the defence that holds
//! whatever the launcher did. Nothing a GPU workload runs on needs them:
//! they are what nvidia-smi and NVML's process and accounting queries are
//! built on.
//!
//! The answer is RM's own for a caller without the privilege,
//! NV_ERR_INSUFFICIENT_PERMISSIONS in the NVOS54 status with the call itself
//! succeeding: the caller sees the refusal RM gives an unprivileged caller
//! natively, not a failed ioctl.
//! Command numbers are FINN interface ids and the same in 535.129.03,
//! 580.95.05, 595.58.03 and 610.57.04.

#![forbid(unsafe_code)]

use crate::le;
use crate::nvos::{
    self, NV_ERR_INSUFFICIENT_PERMISSIONS, NV_ERR_NOT_SUPPORTED, NVOS54_CMD, NVOS54_STATUS,
};

/// Controls that report other RM clients' host PIDs or per-process usage.
pub const HOST_PID_CONTROLS: &[(u32, &str)] = &[
    // PIDs of every client using an object; per-PID video memory.
    (0x2080_018d, "NV2080_CTRL_CMD_GPU_GET_PIDS"),
    (0x2080_018e, "NV2080_CTRL_CMD_GPU_GET_PID_INFO"),
    // Encoder and capture sessions, each with its owner's processId.
    (0x2080_016e, "NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO"),
    (0x2080_01af, "NV2080_CTRL_GPU_GET_NVENC_SW_SESSION_INFO_V2"),
    (0x2080_017c, "NV2080_CTRL_GPU_GET_NVFBC_SW_SESSION_INFO"),
    // Engine utilisation samples, each naming the process that was running.
    (
        0x2080_2096,
        "NV2080_CTRL_CMD_PERF_GET_GPUMON_PERFMON_UTIL_SAMPLES_V2",
    ),
    // Every allocation of video memory with the PID that made or shares it.
    (0x2080_1349, "NV2080_CTRL_CMD_FB_GET_CLIENT_ALLOCATION_INFO"),
    // Accounting: the PIDs that ran, and each one's usage.
    (
        0x0000_0b03,
        "NV0000_CTRL_CMD_GPUACCT_GET_PROC_ACCOUNTING_INFO",
    ),
    (0x0000_0b04, "NV0000_CTRL_CMD_GPUACCT_GET_ACCOUNTING_PIDS"),
    (
        0x0000_0b06,
        "NV0000_CTRL_CMD_GPUACCT_GET_PROC_ACCOUNTING_INFO_V2",
    ),
    // The PID holding the profiler reservation.
    (0x90cc_0103, "NV90CC_CTRL_CMD_HWPM_GET_RESERVATION_INFO"),
    // Robust-channel error reports, with the faulting processId.
    (0x0000_0607, "NV0000_CTRL_CMD_NVD_GET_RCERR_RPT"),
];

/// The name of the host-PID control `params` (an NVOS54 block, as sent)
/// names, if it names one.
pub fn host_pid_control(params: &[u8]) -> Option<&'static str> {
    let cmd = le::u32_at(params, NVOS54_CMD)?;
    HOST_PID_CONTROLS
        .iter()
        .find(|(c, _)| *c == cmd)
        .map(|(_, name)| *name)
}

/// The reply RM gives a caller without the privilege for the control in
/// `params` (the NVOS54 block and the parameters after it, as sent): the
/// same bytes, with the status word saying so. Nothing of the parameters is
/// written, as RM writes nothing before refusing.
pub fn refusal(params: &[u8]) -> Vec<u8> {
    nvos::with_status(params, NVOS54_STATUS, NV_ERR_INSUFFICIENT_PERMISSIONS)
}

// ─────────────────────── NV0000's OS_UNIX controls ───────────────────────
//
// ctrl0000unix.h, 0x3d00-0x3dff on the root client. Six of them name a
// control file by descriptor inside their parameters -- where objects are
// exported to, imported from, or asked about -- and RM resolves the number
// in the calling process (nv_get_file_private, os.c:2465-2834): the backend,
// which holds every guest process's control files. So an untranslated
// number is another guest process's file, and RM would import that
// process's exported objects into the caller's client, or overwrite its
// export slots. Each is translated from the guest's handle to the backend's
// descriptor of that very file, and only a control file (RM's own `NV_TRUE`
// in the lookup) is accepted. -1 is the one value that passes as it is: RM
// refuses it itself. The two MEMACCT controls name a cgroup by descriptor,
// a host cgroup to the backend; nothing translates one, so they, and every
// 0x3dxx command RM does not define, are answered NOT_SUPPORTED without RM.
// FLUSH_USER_CACHE and GET_GPU_INFO carry no descriptor and pass.

/// How the backend treats one OS_UNIX control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnixCtl {
    /// A control file's descriptor at this offset of the parameters.
    Fd { at: usize },
    /// No descriptor: forwarded as any control is.
    Plain,
    /// Answered NOT_SUPPORTED here.
    Refused(&'static str),
}

/// What the OS_UNIX control `cmd` is, or `None` for a command outside
/// 0x3d00-0x3dff of the root client.
pub fn unix_control(cmd: u32) -> Option<UnixCtl> {
    if cmd & 0xffff_ff00 != 0x3d00 {
        return None;
    }
    Some(match cmd {
        // FLUSH_USER_CACHE: a range of the caller's own memory object.
        0x3d02 => UnixCtl::Plain,
        // GET_GPU_INFO: a gpuId's device minor.
        0x3d07 => UnixCtl::Plain,
        // EXPORT_OBJECT_TO_FD: after the 16-byte object.
        0x3d05 => UnixCtl::Fd { at: 16 },
        // IMPORT_OBJECT_FROM_FD, GET_EXPORT_OBJECT_INFO, EXPORT_OBJECTS_TO_FD,
        // IMPORT_OBJECTS_FROM_FD: first.
        0x3d06 | 0x3d08 | 0x3d0b | 0x3d0c => UnixCtl::Fd { at: 0 },
        // CREATE_EXPORT_OBJECT_FD: after hDevice, maxObjects and 64 bytes of
        // metadata, aligned to 4.
        0x3d0a => UnixCtl::Fd { at: 72 },
        0x3d0d => UnixCtl::Refused("NV0000_CTRL_OS_UNIX_CMD_MEMACCT_SET_LIMITS"),
        0x3d0e => UnixCtl::Refused("NV0000_CTRL_OS_UNIX_CMD_MEMACCT_GET_LIMITS"),
        _ => UnixCtl::Refused("an OS_UNIX control RM does not define"),
    })
}

/// The control `params` (an NVOS54 block) names, when it is an OS_UNIX one
/// the backend refuses: its name.
pub fn unix_refused(params: &[u8]) -> Option<&'static str> {
    let cmd = le::u32_at(params, NVOS54_CMD)?;
    match unix_control(cmd)? {
        UnixCtl::Refused(name) => Some(name),
        _ => None,
    }
}

/// `params` answered NOT_SUPPORTED, as RM answers a control it does not
/// serve.
pub fn unsupported(params: &[u8]) -> Vec<u8> {
    nvos::with_status(params, NVOS54_STATUS, NV_ERR_NOT_SUPPORTED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_os_unix_control_that_names_a_file_is_translated_and_the_rest_refused() {
        // The four that were forwarded with the guest's number (R1).
        for (cmd, at) in [(0x3d08, 0), (0x3d0a, 72), (0x3d0b, 0), (0x3d0c, 0)] {
            assert_eq!(unix_control(cmd), Some(UnixCtl::Fd { at }), "{cmd:#x}");
        }
        assert_eq!(unix_control(0x3d05), Some(UnixCtl::Fd { at: 16 }));
        assert_eq!(unix_control(0x3d06), Some(UnixCtl::Fd { at: 0 }));
        assert_eq!(unix_control(0x3d02), Some(UnixCtl::Plain));
        assert_eq!(unix_control(0x3d07), Some(UnixCtl::Plain));
        for cmd in [0x3d0d, 0x3d0e, 0x3d04, 0x3d09, 0x3d0f, 0x3dff] {
            assert!(
                matches!(unix_control(cmd), Some(UnixCtl::Refused(_))),
                "{cmd:#x}"
            );
        }
        // Not OS_UNIX: another interface of the root client, another class.
        assert_eq!(unix_control(0x3e05), None);
        assert_eq!(unix_control(0x2080_3d05), None);
        let mut b = nvos54(0x3d0d);
        assert!(unix_refused(&b).is_some());
        b = unsupported(&b);
        assert_eq!(
            u32::from_le_bytes(b[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap()),
            NV_ERR_NOT_SUPPORTED
        );
        assert!(unix_refused(&nvos54(0x3d0c)).is_none());
    }

    fn nvos54(cmd: u32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[NVOS54_CMD..NVOS54_CMD + 4].copy_from_slice(&cmd.to_le_bytes());
        b
    }

    #[test]
    fn a_control_listing_host_pids_is_recognised_by_its_number() {
        assert_eq!(
            host_pid_control(&nvos54(0x2080_018d)),
            Some("NV2080_CTRL_CMD_GPU_GET_PIDS")
        );
        assert_eq!(host_pid_control(&nvos54(0x2080_0101)), None);
        assert_eq!(host_pid_control(&[0u8; 8]), None, "too short to name one");
    }

    #[test]
    fn the_refusal_is_rms_own_status_with_the_parameters_untouched() {
        let mut p = nvos54(0x2080_018e);
        p.extend_from_slice(&[0x5a; 64]);
        let r = refusal(&p);
        assert_eq!(&r[28..32], &NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes());
        assert_eq!(&r[..28], &p[..28]);
        assert_eq!(&r[32..], &p[32..]);
    }

    #[test]
    fn every_listed_control_is_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for (c, name) in HOST_PID_CONTROLS {
            assert!(seen.insert(*c), "{name} twice");
        }
    }
}
