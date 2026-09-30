// SPDX-License-Identifier: Apache-2.0
//! Opt-in RM allowlist groups (`--rm-allow-group NAME[,NAME...]`): controls
//! and classes the default list leaves out, each added by name and held here
//! to what it is for.
//!
//! The members of each group are in `gen/rmallow_extract.py` (`RM_GROUPS`),
//! measured per release like every other entry (`abi::rmallow::Group`); the
//! allowlist (`rmallow.rs`) lets a member through only when its group was
//! named at start. Every member then passes the rule below for it, whatever
//! host release, before RM sees the call. None of this is on by default, and
//! with no group named the backend behaves as it did before groups existed.
//! SECURITY.md, "Opt-in RM groups", says what each exposes and to whom.
//!
//! - **thermal**: THERMAL_SYSTEM_EXECUTE_V2 (580 and later). Only the eight
//!   read opcodes NVIDIA's header defines (ctrl2080thermal.h, every release
//!   measured: target and sensor information, and GET_STATUS_SENSOR_READING,
//!   the temperature), in at most 32 instructions, at API version 1.0 and
//!   RM's own instruction size. RM forwards a call with any other version or
//!   size to GSP-RM whole (subdevice_ctrl_gpu_kernel.c,
//!   subdeviceCtrlCmdThermalSystemExecuteV2_IMPL), where an opcode the
//!   header does not name could mean anything; so neither goes.
//! - **health**: nvidia-smi -q's ECC, InfoROM, retired-page and black-box
//!   flush queries. Read only, no field to judge: RM's size, nothing more.
//! - **memacct**: MEMACCT_GET_LIMITS (610 and later) and GET_IMPL (615).
//!   `cgroupFd` names a cgroup by descriptor, a host cgroup in the backend,
//!   so RM is always handed NV0000_CTRL_CMD_OS_UNIX_MEMACCT_CURRENT_PROCESS
//!   (-1) there, the backend's own cgroup -- this VM's, under the units --
//!   and the caller reads back its own value, as gVisor's nvproxy does
//!   (frontend.go, ctrlOsUnixMemacctGetLimits).
//! - **debug**: GT200_DEBUGGER's MMU and error-barrier debug modes, and
//!   READ_MEMORY and WRITE_MEMORY, which copy `length` bytes between a
//!   memory object of the debugger's own client and `buffer`. The buffer
//!   must come as a deep segment of exactly `length` bytes (deepseg.rs
//!   checks it against the rule gen/rmctrl measured), `length` at most
//!   [`DEBUG_MEMORY_MAX`], and RM is asked the memory's class first
//!   (NV0000_CTRL_CMD_CLIENT_GET_HANDLE_INFO): only system and video memory
//!   the client allocated, and for READ_MEMORY memory registered by its
//!   pages, pass. RM would read and write through any Memory object
//!   (kernel_sm_debugger_session_ctrl.c, _nv83deCtrlCmdDebugAccessMemory),
//!   and NV01_MEMORY_LOCAL_PRIVILEGED, on the default list for Vulkan, is
//!   the GPU's register aperture; a registration of read-only guest pages
//!   is written by no one.
//! - **profiling**: the NVB0CC profiler of the caller's own context. A
//!   MAXWELL_PROFILER_DEVICE only with a context to bind to (the device-wide
//!   one counts every tenant's work), every reservation and PMA stream
//!   context-switched (`ctxsw`), no PMA wait (RM spins for up to its default
//!   GPU timeout), a PMA stream only into system or video memory the client
//!   allocated, and EXEC_REG_OPS reads alone, of the context's register
//!   image (the GR_CTX types). Register writes are left out: whether one
//!   stays within the caller's context is decided inside GSP-RM, whose
//!   source is not published, so it cannot be shown here. So are the
//!   device-wide parts: the power features and dynamic MMA boost (GPU-wide
//!   clocks and power), PC sampling and HES (no context parameter), the
//!   legacy GF100_PROFILER, and the event buffers, whose FECS and video
//!   binds report every context of the binder's uid (fecs_event_list.c),
//!   every VM of one backend user's.
//!
//! Every debug and profiling call must come from the guest process that
//! made the client it is made through (rmshare.rs, the maker a guest module
//! reports with BCAP_PROC_ID), and both groups need `--allow-compute`.
//! RM applies its own rules on top: profiling is refused to an unprivileged
//! caller without CAP_PERFMON unless the host loads nvidia.ko with
//! NVreg_RmProfilingAdminOnly=0 (kern_profiler_v2.c,
//! profilerBaseQueryCapabilities_IMPL), and the backend is never root.

#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;

use crate::le;
use crate::nvos::{
    NV_ERR_INSUFFICIENT_PERMISSIONS, NV_ERR_INVALID_ARGUMENT, NV_ERR_INVALID_PARAM_STRUCT,
};

/// One opt-in group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Group {
    Thermal,
    Health,
    Memacct,
    Debug,
    Profiling,
}

impl Group {
    pub const ALL: [Group; 5] = [
        Group::Thermal,
        Group::Health,
        Group::Memacct,
        Group::Debug,
        Group::Profiling,
    ];

    /// Its name on the command line and in `abi::rmallow::GROUP_NAMES`.
    pub fn name(self) -> &'static str {
        match self {
            Group::Thermal => "thermal",
            Group::Health => "health",
            Group::Memacct => "memacct",
            Group::Debug => "debug",
            Group::Profiling => "profiling",
        }
    }

    /// Whether it is served only with `--allow-compute`.
    pub fn needs_compute(self) -> bool {
        matches!(self, Group::Debug | Group::Profiling)
    }

    fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

impl FromStr for Group {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Group::ALL
            .into_iter()
            .find(|g| g.name() == s)
            .ok_or_else(|| {
                format!(
                    "{s:?}: no such RM group (one of {})",
                    Group::ALL.map(Group::name).join(", ")
                )
            })
    }
}

/// The groups a backend was started with: none by default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Groups(u8);

impl Groups {
    pub fn contains(self, g: Group) -> bool {
        self.0 & g.bit() != 0
    }

    pub fn insert(&mut self, g: Group) {
        self.0 |= g.bit();
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn iter(self) -> impl Iterator<Item = Group> {
        Group::ALL.into_iter().filter(move |&g| self.contains(g))
    }

    /// The groups of `--rm-allow-group` values, each a comma-separated list.
    pub fn parse<'a>(values: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut out = Groups::default();
        for v in values {
            for name in v.split(',') {
                out.insert(name.trim().parse()?);
            }
        }
        Ok(out)
    }

    /// The first group named that needs `--allow-compute`.
    pub fn needing_compute(self) -> Option<Group> {
        self.iter().find(|g| g.needs_compute())
    }
}

impl fmt::Display for Groups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let names: Vec<&str> = self.iter().map(Group::name).collect();
        f.write_str(&names.join(","))
    }
}

/// For the start-up line: each group named, with what it adds on the host's
/// release (`release` its list).
pub fn describe(groups: Groups, release: &abi::rmallow::Release) -> String {
    if groups.is_empty() {
        return "opt-in RM groups: none".into();
    }
    let each: Vec<String> = groups
        .iter()
        .map(|g| match release.group(g.name()) {
            Some(r) => format!(
                "{} ({} control(s), {} class(es))",
                g.name(),
                r.controls.len(),
                r.classes.len()
            ),
            None => format!("{} (nothing on this release)", g.name()),
        })
        .collect();
    format!("opt-in RM groups: {}", each.join(", "))
}

// ───────────────────────────── the members ─────────────────────────────

pub const THERMAL_SYSTEM_EXECUTE_V2: u32 = 0x2080_0513;

pub const QUERY_INFOROM_ECC_SUPPORT: u32 = 0x2080_0157;
pub const QUERY_ECC_CONFIGURATION: u32 = 0x2080_0133;
pub const FB_GET_OFFLINED_PAGES: u32 = 0x2080_1322;
pub const BBX_GET_LAST_FLUSH_TIME: u32 = 0x90e7_0113;

pub const MEMACCT_GET_LIMITS: u32 = 0x3d0e;
pub const MEMACCT_GET_IMPL: u32 = 0x3d0f;

pub const DEBUG_SET_MODE_MMU_DEBUG: u32 = 0x83de_0307;
pub const DEBUG_SET_MODE_ERRBAR_DEBUG: u32 = 0x83de_031f;
pub const DEBUG_READ_MEMORY: u32 = 0x83de_0315;
pub const DEBUG_WRITE_MEMORY: u32 = 0x83de_0316;

pub const PROF_RESERVE_HWPM_LEGACY: u32 = 0xb0cc_0101;
pub const PROF_RELEASE_HWPM_LEGACY: u32 = 0xb0cc_0102;
pub const PROF_RESERVE_PM_AREA_SMPC: u32 = 0xb0cc_0103;
pub const PROF_RELEASE_PM_AREA_SMPC: u32 = 0xb0cc_0104;
pub const PROF_ALLOC_PMA_STREAM: u32 = 0xb0cc_0105;
pub const PROF_FREE_PMA_STREAM: u32 = 0xb0cc_0106;
pub const PROF_BIND_PM_RESOURCES: u32 = 0xb0cc_0107;
pub const PROF_UNBIND_PM_RESOURCES: u32 = 0xb0cc_0108;
pub const PROF_PMA_STREAM_UPDATE_GET_PUT: u32 = 0xb0cc_0109;
pub const PROF_EXEC_REG_OPS: u32 = 0xb0cc_010a;
pub const PROF_GET_TOTAL_HS_CREDITS: u32 = 0xb0cc_010d;
pub const PROF_SET_HS_CREDITS: u32 = 0xb0cc_010e;
pub const PROF_GET_HS_CREDITS: u32 = 0xb0cc_010f;
pub const PROF_GET_CHIPLET_HS_CREDIT_POOL: u32 = 0xb0cc_0115;
pub const PROF_GET_HS_CREDITS_MAPPING: u32 = 0xb0cc_0116;
pub const PROF_RESERVE_CCU_PROF: u32 = 0xb0cc_0119;
pub const PROF_RELEASE_CCU_PROF: u32 = 0xb0cc_011a;

/// MAXWELL_PROFILER_CONTEXT and MAXWELL_PROFILER_DEVICE.
pub const MAXWELL_PROFILER_CONTEXT: u32 = 0xb1cc;
pub const MAXWELL_PROFILER_DEVICE: u32 = 0xb2cc;

/// What a member's parameters must hold beyond RM's size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    /// Nothing: out-only, or no parameters.
    Size,
    Thermal,
    ForceCgroupFd,
    /// `action` at 0 is one of these.
    Action(&'static [u32]),
    Memory {
        write: bool,
    },
    /// `ctxsw` (an NvBool) at this offset must be true.
    Ctxsw(usize),
    PmaUpdate,
    RegOps,
}

/// Every member control: its group, RM's parameter size in every release
/// measured that has it (`gen/rmallow/*.json`; the tests hold the tables to
/// it), and its rule.
const MEMBERS: &[(u32, Group, usize, Rule)] = &[
    (
        THERMAL_SYSTEM_EXECUTE_V2,
        Group::Thermal,
        1432,
        Rule::Thermal,
    ),
    (QUERY_INFOROM_ECC_SUPPORT, Group::Health, 0, Rule::Size),
    (QUERY_ECC_CONFIGURATION, Group::Health, 8, Rule::Size),
    (FB_GET_OFFLINED_PAGES, Group::Health, 2056, Rule::Size),
    (BBX_GET_LAST_FLUSH_TIME, Group::Health, 16, Rule::Size),
    (
        MEMACCT_GET_LIMITS,
        Group::Memacct,
        1032,
        Rule::ForceCgroupFd,
    ),
    (MEMACCT_GET_IMPL, Group::Memacct, 4, Rule::Size),
    // ENABLE (1) and DISABLE (2); not RELEASE_MMU_DEBUG_REQUESTS (3).
    (
        DEBUG_SET_MODE_MMU_DEBUG,
        Group::Debug,
        4,
        Rule::Action(&[1, 2]),
    ),
    // DISABLE (0) and ENABLE (1).
    (
        DEBUG_SET_MODE_ERRBAR_DEBUG,
        Group::Debug,
        4,
        Rule::Action(&[0, 1]),
    ),
    (
        DEBUG_READ_MEMORY,
        Group::Debug,
        24,
        Rule::Memory { write: false },
    ),
    (
        DEBUG_WRITE_MEMORY,
        Group::Debug,
        24,
        Rule::Memory { write: true },
    ),
    (
        PROF_RESERVE_HWPM_LEGACY,
        Group::Profiling,
        1,
        Rule::Ctxsw(0),
    ),
    (PROF_RELEASE_HWPM_LEGACY, Group::Profiling, 0, Rule::Size),
    (
        PROF_RESERVE_PM_AREA_SMPC,
        Group::Profiling,
        1,
        Rule::Ctxsw(0),
    ),
    (PROF_RELEASE_PM_AREA_SMPC, Group::Profiling, 0, Rule::Size),
    (
        PROF_ALLOC_PMA_STREAM,
        Group::Profiling,
        56,
        Rule::Ctxsw(PMA_CTXSW),
    ),
    (PROF_FREE_PMA_STREAM, Group::Profiling, 4, Rule::Size),
    (PROF_BIND_PM_RESOURCES, Group::Profiling, 0, Rule::Size),
    (PROF_UNBIND_PM_RESOURCES, Group::Profiling, 0, Rule::Size),
    (
        PROF_PMA_STREAM_UPDATE_GET_PUT,
        Group::Profiling,
        48,
        Rule::PmaUpdate,
    ),
    (PROF_EXEC_REG_OPS, Group::Profiling, 3980, Rule::RegOps),
    (PROF_GET_TOTAL_HS_CREDITS, Group::Profiling, 4, Rule::Size),
    (PROF_SET_HS_CREDITS, Group::Profiling, 256, Rule::Size),
    (PROF_GET_HS_CREDITS, Group::Profiling, 256, Rule::Size),
    (
        PROF_GET_CHIPLET_HS_CREDIT_POOL,
        Group::Profiling,
        124,
        Rule::Size,
    ),
    (
        PROF_GET_HS_CREDITS_MAPPING,
        Group::Profiling,
        194,
        Rule::Size,
    ),
    (PROF_RESERVE_CCU_PROF, Group::Profiling, 1, Rule::Ctxsw(0)),
    (PROF_RELEASE_CCU_PROF, Group::Profiling, 0, Rule::Size),
];

/// The group control `cmd` belongs to, if any.
pub fn group_of(cmd: u32) -> Option<Group> {
    member(cmd).map(|m| m.1)
}

fn member(cmd: u32) -> Option<&'static (u32, Group, usize, Rule)> {
    MEMBERS.iter().find(|m| m.0 == cmd)
}

// THERMAL_SYSTEM_EXECUTE_V2's layout (ctrl2080thermal.h): six NvU32s, then
// 32 instructions of {result, executed, opcode, operands[8]}.
const THERMAL_API_VER: usize = 0;
const THERMAL_API_REV: usize = 4;
const THERMAL_INSTR_SIZEOF: usize = 8;
const THERMAL_FLAGS: usize = 12;
const THERMAL_LIST_SIZE: usize = 20;
const THERMAL_LIST: usize = 24;
const THERMAL_INSTR: usize = 44;
const THERMAL_OPCODE: usize = 8;
const THERMAL_MAX: usize = 32;
/// THERMAL_SYSTEM_API_VER, _REV.
const THERMAL_VER: u32 = 1;
const THERMAL_REV: u32 = 0;
/// NV2080_CTRL_THERMAL_SYSTEM_EXECUTE_FLAGS_DEFAULT | _IGNORE_FAIL.
const THERMAL_FLAGS_KNOWN: u32 = 1;
/// The opcodes ctrl2080thermal.h defines, every one a GET: targets
/// available, target type, provider type, sensors available, sensor
/// provider, sensor target, sensor reading range, and the reading itself.
pub const THERMAL_READ_OPCODES: [u32; 8] =
    [0x100, 0x101, 0x301, 0x500, 0x510, 0x520, 0x540, 0x1500];
const _: () = assert!(THERMAL_LIST + THERMAL_MAX * THERMAL_INSTR == 1432);

/// NV0000_CTRL_OS_UNIX_MEMACCT_GET_LIMITS_PARAMS.cgroupFd, and the value
/// RM takes for the calling process (ctrl0000unix.h).
const MEMACCT_CGROUP_FD: usize = 0;
pub const MEMACCT_CURRENT_PROCESS: i32 = -1;

// NV83DE_CTRL_DEBUG_{READ,WRITE}_MEMORY_PARAMS: hMemory, length, offset,
// buffer.
const DEBUG_MEM_H_MEMORY: usize = 0;
const DEBUG_MEM_LENGTH: usize = 4;
const DEBUG_MEM_BUFFER: usize = 16;
/// The most READ_MEMORY or WRITE_MEMORY may copy in one call. RM takes up
/// to 4 GiB (it sizes the buffer without its usual cap,
/// RMAPI_PARAM_COPY_FLAGS_DISABLE_MAX_SIZE_CHECK) and gVisor caps it at 1
/// GiB; a debugger reads a register file or a few pages at a time, and a
/// deep segment is at most 1 MiB anyway.
pub const DEBUG_MEMORY_MAX: u32 = 256 << 10;

// NVB0CC_CTRL_ALLOC_PMA_STREAM_PARAMS: hMemPmaBuffer, pmaBufferOffset,
// pmaBufferSize, hMemPmaBytesAvailable, pmaBytesAvailableOffset, ctxsw.
const PMA_H_BUFFER: usize = 0;
const PMA_H_BYTES_AVAILABLE: usize = 24;
const PMA_CTXSW: usize = 40;
/// NVB0CC_CTRL_PMA_STREAM_UPDATE_GET_PUT_PARAMS.bWait.
const PMA_UPDATE_WAIT: usize = 9;

// NVB0CC_CTRL_EXEC_REG_OPS_PARAMS: regOpCount, mode, bPassed, bDirect,
// then NVB0CC_REGOPS_MAX_COUNT NV2080_CTRL_GPU_REG_OPs of 32 bytes.
const REGOPS_COUNT: usize = 0;
const REGOPS_MODE: usize = 4;
const REGOPS_LIST: usize = 12;
const REGOP_SIZE: usize = 32;
const REGOPS_MAX: u32 = 124;
const _: () = assert!(REGOPS_LIST + REGOPS_MAX as usize * REGOP_SIZE == 3980);
/// NV2080_CTRL_GPU_REG_OP_READ_32, _READ_64, _READ_08.
const REGOP_READS: [u8; 3] = [0, 2, 4];
/// NV2080_CTRL_GPU_REG_OP_TYPE_GR_CTX, _TPC, _SM, _CROP, _ZROP, _QUAD: the
/// context's own register image. Not GLOBAL (0), FB (0x20) or DEVICE (0x80).
const REGOP_CONTEXT_TYPES: [u8; 6] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x40];

// NVB2CC_ALLOC_PARAMETERS: hClientTarget, hContextTarget.
const B2CC_H_CLIENT_TARGET: usize = 0;
const B2CC_H_CONTEXT_TARGET: usize = 4;

/// Memory classes a debugger or a PMA stream may name: NV01_MEMORY_SYSTEM
/// and NV01_MEMORY_LOCAL_USER, and for READ_MEMORY alone
/// NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (memory registered by its pages, which
/// may be read-only to the guest).
const MEMORY_WRITABLE: [u32; 2] = [0x3e, 0x40];
const MEMORY_READABLE: [u32; 3] = [0x3e, 0x40, 0x71];

/// Why a member was answered here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    pub status: u32,
    pub why: &'static str,
}

const fn refused(status: u32, why: &'static str) -> Refused {
    Refused { status, why }
}

/// A word of the parameters the backend hands RM in place of the guest's,
/// and puts back in the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fix {
    /// Offset in the parameters (after NVOS54).
    pub at: usize,
    pub value: u32,
}

/// What the gate needs of the backend.
pub struct Ctx<'a> {
    /// Whether client `h` was made by the calling guest process (or is the
    /// call's own client, made by it): rmshare.rs's Process rule.
    pub callers: &'a dyn Fn(u32) -> bool,
    /// The call's own client (NVOS54's hClient, NVOS64's hRoot).
    pub own: u32,
    /// The call came with deep segments; with one deep block.
    pub deep_segments: bool,
    pub deep_single: bool,
    /// `--allow-compute`.
    pub compute: bool,
}

/// What a member control may go to RM with: a word to force, and the memory
/// objects (of the call's own client) whose class RM must be asked first,
/// each with whether it may be written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub fix: Option<Fix>,
    pub memory: Vec<(u32, bool)>,
}

/// Judge RM control `cmd` of `size` bytes (`params`, as the guest sent them)
/// for `groups`. `Ok(None)`: not a member of a group named, the gate's
/// business ends. A member of a group not named is the allowlist's to refuse.
pub fn control(
    groups: Groups,
    cmd: u32,
    params: &[u8],
    size: u32,
    ctx: &Ctx<'_>,
) -> Result<Option<Plan>, Refused> {
    let Some(&(_, group, want, rule)) = member(cmd) else {
        return Ok(None);
    };
    if !groups.contains(group) {
        return Ok(None);
    }
    if group.needs_compute() && !ctx.compute {
        return Err(refused(
            NV_ERR_INSUFFICIENT_PERMISSIONS,
            "its group is served only with --allow-compute",
        ));
    }
    // RM's size, on every host: the fields below are read where it puts them.
    if size as usize != want || params.len() < want {
        return Err(refused(
            NV_ERR_INVALID_PARAM_STRUCT,
            "parameters of a size RM does not take",
        ));
    }
    if matches!(group, Group::Debug | Group::Profiling) && !(ctx.callers)(ctx.own) {
        return Err(refused(
            NV_ERR_INSUFFICIENT_PERMISSIONS,
            "its client was not made by the calling guest process",
        ));
    }
    let word = |at: usize| le::u32_at(params, at).unwrap_or(u32::MAX);
    let byte = |at: usize| params.get(at).copied().unwrap_or(u8::MAX);
    let mut plan = Plan::default();
    match rule {
        Rule::Size => {}
        Rule::Thermal => thermal(params)?,
        Rule::ForceCgroupFd => {
            plan.fix = Some(Fix {
                at: MEMACCT_CGROUP_FD,
                value: MEMACCT_CURRENT_PROCESS as u32,
            });
        }
        Rule::Action(ok) => {
            if !ok.contains(&word(0)) {
                return Err(refused(NV_ERR_INVALID_ARGUMENT, "an action other than set"));
            }
        }
        Rule::Memory { write } => {
            let length = word(DEBUG_MEM_LENGTH);
            let buffer = le::u64_at(params, DEBUG_MEM_BUFFER).unwrap_or(0);
            if length == 0 || buffer == 0 {
                return Err(refused(NV_ERR_INVALID_ARGUMENT, "no buffer or no length"));
            }
            if length > DEBUG_MEMORY_MAX {
                return Err(refused(
                    NV_ERR_INVALID_ARGUMENT,
                    "a length past the debug group's cap",
                ));
            }
            if !ctx.deep_segments || ctx.deep_single {
                return Err(refused(
                    NV_ERR_INVALID_ARGUMENT,
                    "its buffer did not come as a deep segment",
                ));
            }
            plan.memory.push((word(DEBUG_MEM_H_MEMORY), write));
        }
        Rule::Ctxsw(at) => {
            if byte(at) != 1 {
                return Err(refused(
                    NV_ERR_INSUFFICIENT_PERMISSIONS,
                    "a reservation that is not context-switched counts every tenant's work",
                ));
            }
            if cmd == PROF_ALLOC_PMA_STREAM {
                plan.memory.push((word(PMA_H_BUFFER), true));
                plan.memory.push((word(PMA_H_BYTES_AVAILABLE), true));
            }
        }
        Rule::PmaUpdate => {
            if byte(PMA_UPDATE_WAIT) != 0 {
                return Err(refused(
                    NV_ERR_INVALID_ARGUMENT,
                    "a wait, which RM spins through with the GPU's locks held",
                ));
            }
        }
        Rule::RegOps => regops(params)?,
    }
    Ok(Some(plan))
}

fn thermal(p: &[u8]) -> Result<(), Refused> {
    let w = |at: usize| le::u32_at(p, at).unwrap_or(u32::MAX);
    if w(THERMAL_API_VER) != THERMAL_VER
        || w(THERMAL_API_REV) != THERMAL_REV
        || w(THERMAL_INSTR_SIZEOF) != THERMAL_INSTR as u32
    {
        return Err(refused(
            NV_ERR_INVALID_ARGUMENT,
            "an API version or instruction size RM would hand GSP-RM whole",
        ));
    }
    if w(THERMAL_FLAGS) & !THERMAL_FLAGS_KNOWN != 0 {
        return Err(refused(NV_ERR_INVALID_ARGUMENT, "an unknown execute flag"));
    }
    let n = w(THERMAL_LIST_SIZE) as usize;
    if n > THERMAL_MAX {
        return Err(refused(
            NV_ERR_INVALID_ARGUMENT,
            "more instructions than fit",
        ));
    }
    for i in 0..n {
        let op = w(THERMAL_LIST + i * THERMAL_INSTR + THERMAL_OPCODE);
        if !THERMAL_READ_OPCODES.contains(&op) {
            return Err(refused(
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "an instruction that is not one of the read opcodes",
            ));
        }
    }
    Ok(())
}

fn regops(p: &[u8]) -> Result<(), Refused> {
    let n = le::u32_at(p, REGOPS_COUNT).unwrap_or(0);
    if n == 0 || n > REGOPS_MAX {
        return Err(refused(
            NV_ERR_INVALID_ARGUMENT,
            "a register operation count out of range",
        ));
    }
    if le::u32_at(p, REGOPS_MODE).unwrap_or(u32::MAX) > 1 {
        return Err(refused(
            NV_ERR_INVALID_ARGUMENT,
            "an unknown register operation mode",
        ));
    }
    for i in 0..n as usize {
        let at = REGOPS_LIST + i * REGOP_SIZE;
        let (op, ty) = (
            p.get(at).copied().unwrap_or(u8::MAX),
            p.get(at + 1).copied().unwrap_or(u8::MAX),
        );
        if !REGOP_READS.contains(&op) {
            return Err(refused(
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "a register write, which only GSP-RM can say stays in the caller's context",
            ));
        }
        if !REGOP_CONTEXT_TYPES.contains(&ty) {
            return Err(refused(
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "a register read outside the context's own image",
            ));
        }
    }
    Ok(())
}

/// Whether memory of `class` may be named where `write` says.
pub fn memory_class_ok(class: u32, write: bool) -> Result<(), Refused> {
    let ok: &[u32] = if write {
        &MEMORY_WRITABLE
    } else {
        &MEMORY_READABLE
    };
    if ok.contains(&class) {
        Ok(())
    } else {
        Err(refused(
            NV_ERR_INSUFFICIENT_PERMISSIONS,
            "memory of a class the group does not reach (the register aperture, memory \
             registered read-only, ...)",
        ))
    }
}

/// Judge RM_ALLOC of `class` with `nested` (the class parameters, as sent)
/// for `groups`. `Ok(false)`: not a member of a group named.
pub fn alloc(groups: Groups, class: u32, nested: &[u8], ctx: &Ctx<'_>) -> Result<bool, Refused> {
    if !matches!(class, MAXWELL_PROFILER_CONTEXT | MAXWELL_PROFILER_DEVICE)
        || !groups.contains(Group::Profiling)
    {
        return Ok(false);
    }
    if !ctx.compute {
        return Err(refused(
            NV_ERR_INSUFFICIENT_PERMISSIONS,
            "its group is served only with --allow-compute",
        ));
    }
    if !(ctx.callers)(ctx.own) {
        return Err(refused(
            NV_ERR_INSUFFICIENT_PERMISSIONS,
            "its client was not made by the calling guest process",
        ));
    }
    if class == MAXWELL_PROFILER_DEVICE {
        let (Some(client), Some(context)) = (
            le::u32_at(nested, B2CC_H_CLIENT_TARGET),
            le::u32_at(nested, B2CC_H_CONTEXT_TARGET),
        ) else {
            return Err(refused(
                NV_ERR_INVALID_ARGUMENT,
                "parameters too short to name a context",
            ));
        };
        if client == 0 || context == 0 {
            return Err(refused(
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "a device-wide profiler, which counts every tenant's work",
            ));
        }
        if client != ctx.own && !(ctx.callers)(client) {
            return Err(refused(
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "a context of a client the calling guest process did not make",
            ));
        }
    }
    Ok(true)
}

// ───────────────────────────── the backend ─────────────────────────────

use crate::nvidia::NvidiaBackend;
use crate::nvos::{NVOS54_CMD, NVOS54_H_CLIENT, NVOS54_PARAMS_SIZE, NVOS54_SIZE, NVOS64_H_CLASS};
use crate::nvos::{NVOS64_H_ROOT, NVOS64_SIZE};
use std::os::fd::RawFd;

/// NV0000_CTRL_CMD_CLIENT_GET_HANDLE_INFO and its CLASSID index; the
/// parameters are {hObject, index, data (NvU64)}.
const GET_HANDLE_INFO: u32 = 0x0d02;
const GET_HANDLE_INFO_CLASSID: u32 = 2;
const GET_HANDLE_INFO_SIZE: usize = 16;

impl NvidiaBackend {
    /// The groups this backend was started with.
    pub fn rm_groups(&self) -> Groups {
        self.rmallow.groups()
    }

    /// Serve the members of `groups` (`--rm-allow-group`).
    pub fn set_rm_groups(&mut self, groups: Groups) {
        self.rmallow.set_groups(groups);
    }

    /// The groups served, with what each adds on the host's release.
    pub fn rm_groups_summary(&self) -> String {
        describe(self.rm_groups(), self.rmallow.release())
    }

    /// An OS_UNIX control the backend answers itself (rmctl.rs), unless it
    /// is a MEMACCT query the memacct group serves.
    pub(crate) fn rm_unix_refused(&self, params: &[u8]) -> Option<&'static str> {
        let cmd = le::u32_at(params, NVOS54_CMD)?;
        if self.rm_groups().contains(Group::Memacct)
            && matches!(cmd, MEMACCT_GET_LIMITS | MEMACCT_GET_IMPL)
        {
            return None;
        }
        crate::rmctl::unix_refused(params)
    }

    /// The opt-in groups' rules on an RM_CONTROL or RM_ALLOC (`params`: the
    /// top-level block and what follows it, as sent), after the share gate
    /// named the calling process. `Ok` carries the word to force, if any;
    /// `Err` the RM status to answer with, the host not called for the
    /// control itself.
    pub(crate) fn rm_group_gate(
        &mut self,
        host_fd: RawFd,
        escape: u32,
        params: &[u8],
        deep_segments: bool,
        deep_single: bool,
    ) -> Result<Option<Fix>, u32> {
        let groups = self.rm_groups();
        if groups.is_empty() {
            return Ok(None);
        }
        let caller = self.current_proc;
        let semsurf = self.semsurf.clone();
        let callers = |h: u32| {
            let n = crate::rmshare::Named {
                h,
                what: "an opt-in RM group's call",
                rule: crate::rmshare::Rule::Process,
                obj: 0,
            };
            semsurf.owns_client(h) && semsurf.named_ok(caller.as_ref(), 0, &n)
        };
        let compute = self.config.allow_compute;
        match escape {
            abi::ioctl::NV_ESC_RM_CONTROL => {
                let (Some(own), Some(cmd), Some(size)) = (
                    le::u32_at(params, NVOS54_H_CLIENT),
                    le::u32_at(params, NVOS54_CMD),
                    le::u32_at(params, NVOS54_PARAMS_SIZE),
                ) else {
                    return Ok(None);
                };
                let ctl = params.get(NVOS54_SIZE..).unwrap_or(&[]);
                let ctx = Ctx {
                    callers: &callers,
                    own,
                    deep_segments,
                    deep_single,
                    compute,
                };
                let plan = match control(groups, cmd, ctl, size, &ctx) {
                    Ok(Some(p)) => p,
                    Ok(None) => return Ok(None),
                    Err(r) => return Err(self.rm_group_refused(cmd, r)),
                };
                for &(h, write) in &plan.memory {
                    let class = self.rm_handle_class(host_fd, own, h);
                    let verdict = match class {
                        Some(c) => memory_class_ok(c, write),
                        None => Err(refused(
                            NV_ERR_INSUFFICIENT_PERMISSIONS,
                            "memory RM could not name the class of",
                        )),
                    };
                    verdict.map_err(|r| self.rm_group_refused(cmd, r))?;
                }
                Ok(plan.fix)
            }
            abi::ioctl::NV_ESC_RM_ALLOC => {
                let (Some(own), Some(class)) = (
                    le::u32_at(params, NVOS64_H_ROOT),
                    le::u32_at(params, NVOS64_H_CLASS),
                ) else {
                    return Ok(None);
                };
                let nested = params.get(NVOS64_SIZE..).unwrap_or(&[]);
                let ctx = Ctx {
                    callers: &callers,
                    own,
                    deep_segments,
                    deep_single,
                    compute,
                };
                alloc(groups, class, nested, &ctx)
                    .map(|_| None)
                    .map_err(|r| {
                        log::warn!(
                            "RM class {} ({class:#06x}) of an opt-in group refused: {}",
                            abi::rmallow::class_name(class).unwrap_or("?"),
                            r.why
                        );
                        r.status
                    })
            }
            _ => Ok(None),
        }
    }

    fn rm_group_refused(&self, cmd: u32, r: Refused) -> u32 {
        log::warn!(
            "RM control {} ({cmd:#010x}) of an opt-in group refused: {}",
            abi::rmallow::control_name(cmd).unwrap_or("?"),
            r.why
        );
        r.status
    }

    /// RM's class of `handle` in `client`, asked on `host_fd` (the file the
    /// client lives on) as RM itself looks the memory up: `None` if RM
    /// would not say.
    fn rm_handle_class(&self, host_fd: RawFd, client: u32, handle: u32) -> Option<u32> {
        use crate::sys::block::{Arena, Restore, SlotKind};
        let mut top = [0u8; NVOS54_SIZE];
        for (at, v) in [
            (NVOS54_H_CLIENT, client),
            (crate::nvos::NVOS54_H_OBJECT, client),
            (NVOS54_CMD, GET_HANDLE_INFO),
            (NVOS54_PARAMS_SIZE, GET_HANDLE_INFO_SIZE as u32),
        ] {
            le::put_u32(&mut top, at, v)?;
        }
        let mut p = [0u8; GET_HANDLE_INFO_SIZE];
        le::put_u32(&mut p, 0, handle)?;
        le::put_u32(&mut p, 4, GET_HANDLE_INFO_CLASSID)?;
        let mut a = Arena::new();
        let t = a.small(&top);
        let n = a.small(&p);
        a.slot(
            t,
            crate::nvos::NVOS54_PARAMS,
            8,
            SlotKind::Ptr,
            Restore::Yes,
        )
        .ok()?;
        a.point(t, crate::nvos::NVOS54_PARAMS, n).ok()?;
        let request = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_CONTROL, NVOS54_SIZE as u32);
        self.host_call(&mut a, host_fd, request, t).ok()?;
        if le::u32_at(a.bytes(t), crate::nvos::NVOS54_STATUS)? != crate::nvos::NV_OK {
            return None;
        }
        let class = le::u64_at(a.bytes(n), 8)?;
        u32::try_from(class).ok()
    }
}

/// The parameters with `fix` applied (`params`: NVOS54 and what follows).
pub(crate) fn apply(params: &[u8], fix: Fix) -> Vec<u8> {
    let mut v = params.to_vec();
    let _ = le::put_u32(&mut v, NVOS54_SIZE + fix.at, fix.value);
    v
}

/// Put the caller's own word back where `fix` forced one, in the reply
/// (laid out as the request).
pub(crate) fn restore(reply: &mut [u8], fix: Fix, sent: &[u8]) {
    if let Some(w) = le::u32_at(sent, NVOS54_SIZE + fix.at) {
        let _ = le::put_u32(reply, NVOS54_SIZE + fix.at, w);
    }
}

#[cfg(test)]
mod tests;
