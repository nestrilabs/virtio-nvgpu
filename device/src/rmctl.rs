//! RM controls the backend answers itself, without asking the host.
//!
//! RM_CONTROL is forwarded without an allowlist (the tallies in `tally.rs`
//! are what one will be written from), so every control a host process could
//! call, an unprivileged guest process can call too -- as the backend, a
//! client of the host's RM like any other. Most controls say something only
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

/// NV_ERR_INSUFFICIENT_PERMISSIONS (nvstatuscodes.h).
pub const NV_ERR_INSUFFICIENT_PERMISSIONS: u32 = 0x1b;

/// NVOS54: `cmd` at 8, `status` at 28, 32 bytes.
const OS54_CMD: usize = 8;
const OS54_STATUS: usize = 28;
const OS54_SIZE: usize = 32;

/// The name of the host-PID control `params` (an NVOS54 block, as sent)
/// names, if it names one.
pub fn host_pid_control(params: &[u8]) -> Option<&'static str> {
    let cmd = u32::from_le_bytes(params.get(OS54_CMD..OS54_CMD + 4)?.try_into().ok()?);
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
    let mut out = params.to_vec();
    if out.len() >= OS54_SIZE {
        out[OS54_STATUS..OS54_STATUS + 4]
            .copy_from_slice(&NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nvos54(cmd: u32) -> Vec<u8> {
        let mut b = vec![0u8; 32];
        b[OS54_CMD..OS54_CMD + 4].copy_from_slice(&cmd.to_le_bytes());
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
