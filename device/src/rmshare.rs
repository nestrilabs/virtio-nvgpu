//! RM objects between clients: who may share one, duplicate one, or name
//! another client at all.
//!
//! RM keeps a user client's objects to the process that made the client. Its
//! one default share policy is `RS_SHARE_TYPE_PID` for `RS_ACCESS_DUP_OBJECT`
//! (sharing.c, serverInitGlobalSharePolicies), and a PID policy matches when
//! the source client's ProcID is the duplicating client's (client_resource.c,
//! cliresShareCallback -- the policy's `target` is never read). But every RM
//! call a guest process makes is the backend's on the host, so RM sees all of
//! a VM's clients as one process, the backend's: any guest process could
//! duplicate any other's objects by handle, and whatever a guest shares
//! outward with NV_ESC_RM_SHARE is shared by RM with the host. Three rules,
//! applied before RM sees the call:
//!
//! - **Sharing only narrows outside the VM** ([`classify_share`]).
//!   NV_ESC_RM_SHARE (NVOS57) and the two controls that do the same
//!   (NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT and
//!   NV0000_CTRL_CMD_CLIENT_SET_INHERITED_SHARE_POLICY) go to RM when they
//!   revoke or only add a REQUIRE, and when they grant to one of this VM's
//!   clients, to the calling client itself, to type PID (the owner's own
//!   process, the backend's, which only this VM's clients and the backend's
//!   own have) or to type NONE (which matches nothing). A grant of type ALL,
//!   OS_SECURITY_TOKEN (the backend's uid: every host process running as it,
//!   other VMs' backends included), SMC_PARTITION, GPU, FM_CLIENT, a type RM
//!   does not define, or CLIENT naming a client that is not this VM's, is
//!   refused. What RM took is recorded ([`Grant`]), for the next rule.
//! - **A duplicate is RM's rule, kept to guest processes** ([`DupVerdict`]).
//!   NV_ESC_RM_DUP_OBJECT (NVOS55) names two clients: both must be clients
//!   this VM allocated and has not freed. RM's own rule between them is its
//!   PID default -- the source client's maker is the destination client's
//!   (cliresShareCallback) -- or a grant in the list RM checks the source
//!   object against; the backend applies exactly that, with guest processes
//!   for host ones: the two clients were made by one guest process, or the
//!   object's own list, or its client's while no other object of the client
//!   has one ([`Ownership`]), carries a CLIENT grant of DUP_OBJECT to the
//!   destination. Which process made a client only the guest kernel knows: it
//!   says, with [`ProcId`] (BCAP_PROC_ID). A guest that cannot gets no
//!   duplicate between two clients except by a recorded grant: without the
//!   process, RM's rule cannot be told from "any client of the VM".
//! - **A second client named in parameters, to RM's rule for it**
//!   ([`alloc_named`], [`control_named`], [`Rule`]). Allocation classes and
//!   controls that name a client besides the caller's -- a device sharing
//!   another client's VA space, an event on another client's object, a
//!   debugger or profiler of another client's context, register operations
//!   on another client's channel, other clients' channels disabled -- are
//!   checked by RM, when at all, with a security token (the caller's euid, or
//!   its process) that every guest process shares, since all are the
//!   backend's. Each field gets the rule RM applies to it between host
//!   processes, with the guest's processes and euids ([`Caller`], from
//!   BCAP_PROC_EUID) in place of the backend's; where RM checks nothing, the
//!   named client must be the calling process's own. The client must be this
//!   VM's first of all.
//!
//! A refusal is RM's own for a caller without the right,
//! NV_ERR_INSUFFICIENT_PERMISSIONS in the parameters' status with the ioctl
//! itself succeeding, as for the controls in rmctl.rs: the caller sees what
//! it would natively for a share or a duplicate RM will not allow.

#![forbid(unsafe_code)]

use std::collections::HashMap;

use abi::ioctl::{NV_ESC_RM_ALLOC, NV_ESC_RM_CONTROL, NV_ESC_RM_DUP_OBJECT, NV_ESC_RM_SHARE};
pub use protocol::messages::ProcId;

pub use crate::rmctl::NV_ERR_INSUFFICIENT_PERMISSIONS;

/// NV_ERR_INSUFFICIENT_RESOURCES (nvstatuscodes.h): more grants than
/// [`GRANT_CAP`].
pub const NV_ERR_INSUFFICIENT_RESOURCES: u32 = 0x1a;

/// `RS_SHARE_TYPE_*` (rs_access.h), the same in 535 through 610.
pub const RS_SHARE_TYPE_NONE: u16 = 0;
pub const RS_SHARE_TYPE_ALL: u16 = 1;
pub const RS_SHARE_TYPE_OS_SECURITY_TOKEN: u16 = 2;
pub const RS_SHARE_TYPE_CLIENT: u16 = 3;
pub const RS_SHARE_TYPE_PID: u16 = 4;
pub const RS_SHARE_TYPE_SMC_PARTITION: u16 = 5;
pub const RS_SHARE_TYPE_GPU: u16 = 6;
pub const RS_SHARE_TYPE_FM_CLIENT: u16 = 7;
/// `RS_SHARE_TYPE_MAX`: the first type RM does not define.
pub const RS_SHARE_TYPE_MAX: u16 = 8;

/// `RS_SHARE_ACTION_FLAG_*`.
pub const RS_SHARE_ACTION_FLAG_REVOKE: u8 = 1 << 0;
pub const RS_SHARE_ACTION_FLAG_REQUIRE: u8 = 1 << 1;
pub const RS_SHARE_ACTION_FLAG_COMPOSE: u8 = 1 << 2;

/// `RS_ACCESS_DUP_OBJECT`, as a bit of an `RS_ACCESS_MASK`'s one limb.
pub const RS_ACCESS_DUP_OBJECT_BIT: u32 = 1 << 0;

/// NVOS57_PARAMETERS: `{hClient, hObject, RS_SHARE_POLICY sharePolicy,
/// status}`, 24 bytes; the policy is `{target, accessMask, type (u16),
/// action (u8)}`, 12 bytes (nvos.h, rs_access.h; the same in 595.99.02 and
/// 610.57.04).
pub const OS57_SIZE: usize = 24;
const OS57_OBJECT: usize = 4;
const OS57_POLICY: usize = 8;
pub const OS57_STATUS: usize = 20;

/// NVOS55_PARAMETERS: `{hClient, hParent, hObject, hClientSrc, hObjectSrc,
/// flags, status}`, 28 bytes.
pub const OS55_SIZE: usize = 28;
const OS55_CLIENT_SRC: usize = 12;
const OS55_OBJECT_SRC: usize = 16;
pub const OS55_STATUS: usize = 24;

/// NVOS64: the class at 12, the status at 40, 48 bytes; the class
/// parameters follow.
const OS64_CLASS: usize = 12;
pub const OS64_STATUS: usize = 40;
const OS64_SIZE: usize = 48;

/// NVOS54: the command at 8, the status at 28, 32 bytes; the parameters
/// follow.
const OS54_CMD: usize = 8;
pub const OS54_STATUS: usize = 28;
const OS54_SIZE: usize = 32;

/// NV0000_CTRL_CMD_CLIENT_SET_INHERITED_SHARE_POLICY: `{RS_SHARE_POLICY}`,
/// applied to the calling client itself (cliresCtrlCmdClientSetInherited
/// SharePolicy: `hObject` is the client).
pub const CTRL_SET_INHERITED_SHARE_POLICY: u32 = 0x0000_0d04;
/// NV0000_CTRL_CMD_CLIENT_SHARE_OBJECT: `{hObject, RS_SHARE_POLICY}`.
pub const CTRL_SHARE_OBJECT: u32 = 0x0000_0d06;

/// Recorded share lists and grants a session may hold, together. Each is a
/// list or a policy RM also holds; this bounds what the backend keeps, not
/// what RM does. A share past it is refused, a revoke that would start a
/// list included.
pub const GRANT_CAP: usize = 4096;

fn rd32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// An `RS_SHARE_POLICY`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub target: u32,
    pub mask: u32,
    pub kind: u16,
    pub action: u8,
}

impl Policy {
    /// The 12-byte policy at `at` in `b`.
    pub fn read(b: &[u8], at: usize) -> Option<Self> {
        let p = b.get(at..at + 12)?;
        Some(Self {
            target: rd32(p, 0)?,
            mask: rd32(p, 4)?,
            kind: u16::from_le_bytes([p[8], p[9]]),
            action: p[10],
        })
    }

    fn revokes(&self) -> bool {
        self.action & RS_SHARE_ACTION_FLAG_REVOKE != 0
    }

    fn requires(&self) -> bool {
        self.action & RS_SHARE_ACTION_FLAG_REQUIRE != 0
    }

    fn composes(&self) -> bool {
        self.action & RS_SHARE_ACTION_FLAG_COMPOSE != 0
    }
}

/// What a share asks of RM, judged before RM sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShareVerdict {
    /// Goes to RM: it narrows, or grants only inside this VM.
    Forward,
    /// Refused: it would share outside this VM.
    Refused(&'static str),
}

/// Whether a share `p` of an object of client `owner` may reach RM.
/// `in_vm` says whether a client handle is one this VM allocated and has not
/// freed.
///
/// A revoke only takes away, and a REQUIRE policy only denies what fails it
/// (rsAccessGetSharedRights); REVOKE|REQUIRE removes a REQUIRE, which only
/// the guest can have put there (RM's own, the SMC partition one, is in a
/// list no share reaches, serverInitGlobalSharePolicies). Without COMPOSE, a
/// share replaces the object's list (clientShareResource), which for an
/// allow drops the PID default too: narrower still.
pub fn classify_share(owner: u32, p: &Policy, in_vm: impl Fn(u32) -> bool) -> ShareVerdict {
    if p.revokes() || p.requires() {
        return ShareVerdict::Forward;
    }
    match p.kind {
        // Matches nothing (resShareCallback).
        RS_SHARE_TYPE_NONE => ShareVerdict::Forward,
        // The owner's own process: the source client's ProcID against the
        // duplicating client's, `target` unread (cliresShareCallback). Every
        // client with the backend's ProcID is this VM's or the backend's own.
        RS_SHARE_TYPE_PID => ShareVerdict::Forward,
        // Its own access map (serverShareResourceAccess's special case for
        // hClientOwner == hClientTarget), or one of this VM's clients.
        RS_SHARE_TYPE_CLIENT if p.target == owner || in_vm(p.target) => ShareVerdict::Forward,
        RS_SHARE_TYPE_CLIENT => ShareVerdict::Refused("a CLIENT grant to a client not this VM's"),
        RS_SHARE_TYPE_ALL => ShareVerdict::Refused("a grant to every client on the host"),
        RS_SHARE_TYPE_OS_SECURITY_TOKEN => {
            ShareVerdict::Refused("a grant to every host process of the backend's uid")
        }
        RS_SHARE_TYPE_SMC_PARTITION => ShareVerdict::Refused("a grant to an SMC partition"),
        RS_SHARE_TYPE_GPU => ShareVerdict::Refused("a grant to every client of the GPU"),
        RS_SHARE_TYPE_FM_CLIENT => ShareVerdict::Refused("a grant to the fabric manager"),
        _ => ShareVerdict::Refused("a grant of a type RM does not define"),
    }
}

/// A CLIENT grant RM took for an object of this VM: `target` may duplicate it
/// with the rights in `mask`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    pub target: u32,
    pub mask: u32,
}

/// Apply a share RM answered NV_OK to `list`, the grants recorded for its
/// object, as clientShareResource applies it to the object's policy list:
/// without COMPOSE the list is replaced; a revoke takes its rights from the
/// matching policy (rsShareListRemove), dropping one left with none; a grant
/// adds them (rsShareListInsert). Only CLIENT policies naming another client
/// are kept: they are all [`DupVerdict`] reads. Returns how many entries the
/// list gained (negative when it lost some).
pub fn apply_share(list: &mut Vec<Grant>, owner: u32, p: &Policy) -> isize {
    let before = list.len() as isize;
    if !p.composes() {
        list.clear();
    }
    // A REQUIRE policy is a different entry from an allow with the same
    // target (rsShareListLookup compares the flag), and grants nothing.
    if p.kind == RS_SHARE_TYPE_CLIENT && p.target != owner && !p.requires() {
        let at = list.iter().position(|g| g.target == p.target);
        if p.revokes() {
            if let Some(i) = at {
                list[i].mask &= !p.mask;
                if list[i].mask == 0 {
                    list.remove(i);
                }
            }
        } else if let Some(i) = at {
            list[i].mask |= p.mask;
        } else if p.mask != 0 {
            list.push(Grant {
                target: p.target,
                mask: p.mask,
            });
        }
    }
    list.len() as isize - before
}

/// Who makes an RM call, or made a client, as the guest kernel says
/// ([`ProcId`]): one guest process, and its effective uid when the session
/// has BCAP_PROC_EUID.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub start_ns: u64,
    pub tgid: u32,
    pub euid: Option<u32>,
}

impl Caller {
    /// From the wire; `euid` says whether its euid field is one.
    pub fn from_wire(id: &ProcId, euid: bool) -> Self {
        Self {
            start_ns: id.start_ns,
            tgid: id.tgid,
            euid: euid.then_some(id.euid),
        }
    }

    /// One guest process: an exec keeps it, a setuid does not change it.
    pub fn same_process(&self, o: &Self) -> bool {
        self.start_ns == o.start_ns && self.tgid == o.tgid
    }

    /// RM's security-token match between two host processes
    /// (osValidateClientTokens, os.c: one euid, or one PID). An euid the
    /// guest did not say matches none.
    pub fn same_token(&self, o: &Self) -> bool {
        self.same_process(o) || matches!((self.euid, o.euid), (Some(a), Some(b)) if a == b)
    }
}

/// Whether a duplicate may reach RM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DupVerdict {
    Allowed,
    /// hClientSrc is not a client this VM allocated, or it has been freed.
    ForeignSource,
    /// hClient likewise.
    ForeignDestination,
    /// Both are this VM's, made by different guest processes, and nothing
    /// shares the source with the destination.
    OtherProcess,
    /// Both are this VM's, and which guest process made one of them is not
    /// known (a guest without BCAP_PROC_ID), nor does a grant share it.
    Unattributed,
}

/// NV_ESC_RM_DUP_OBJECT's `(hClient, hClientSrc, hObjectSrc)`.
pub fn dup_names(params: &[u8]) -> Option<(u32, u32, u32)> {
    Some((
        rd32(params, 0)?,
        rd32(params, OS55_CLIENT_SRC)?,
        rd32(params, OS55_OBJECT_SRC)?,
    ))
}

/// The rule a second client named in parameters is held to: RM's for that
/// field between two host processes, the guest's processes and euids in
/// place of the backend's one. The caller's own client, and none (zero),
/// pass every rule, as they do in RM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// Made by the calling guest process. For the fields RM checks nothing
    /// of, or checks only inside GSP-RM, whose security token is the GFID or
    /// none (kernel_sm_debugger_session.c, the comment above
    /// VALIDATE_MATCHING_SEC_TOKENS): any host process could name any client
    /// there, and the backend holds it to the one the guest process made.
    Process,
    /// clientValidate against the caller (client.c, rmclientValidate): with
    /// RM's default PDB_PROP_SYS_VALIDATE_CLIENT_HANDLE_STRICT the client
    /// must be one of the calling file's, which RM itself checks on the
    /// backend's file (one per guest file); without it, the calling
    /// process's security token: made by the calling guest process or by one
    /// of the caller's euid.
    Token,
    /// osValidateClientTokens between the caller's client and the named one
    /// (`_kfifoValidateTargetClient`, profilerDevConstruct): the two clients
    /// were made by one guest process, or by processes of one euid.
    ClientToken,
    /// A right RM grants from the object's share list (rsAccessCheckRights):
    /// its PID default, which the backend's process matches for every guest
    /// client, becomes "the two clients were made by one guest process"; a
    /// CLIENT grant of any of `rights` on the object at offset `obj` of the
    /// parameters, recorded when RM took it, counts as RM counts it.
    Shared { rights: u32, obj: usize },
}

/// A client field: its offset in the parameters, and its rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub at: usize,
    pub rule: Rule,
}

const fn f(at: usize, rule: Rule) -> Field {
    Field { at, rule }
}

/// `RS_ACCESS_DEBUG`, as a bit of an `RS_ACCESS_MASK`'s one limb.
pub const RS_ACCESS_DEBUG_BIT: u32 = 1 << 2;
/// Every right RM defines (RS_ACCESS_COUNT is 4).
const RS_ACCESS_ANY: u32 = 0xf;

/// Allocation classes whose parameters name a client besides the caller's,
/// with where (class parameters, after NVOS64) and RM's rule for it. Zero
/// there is the caller's own client (or none) to RM. Measured on
/// 610.57.04's SDK headers and the same in 595.99.02:
///
/// - 0x0080 NV01_DEVICE_0: `hClientShare` (4), the client whose VA space the
///   device shares: clientValidate (device.c `_deviceValidateClientShare`),
///   [`Rule::Token`]. `hTargetClient` (8) is stored for vGPU and never
///   checked: [`Rule::Process`].
/// - 0x0005 NV01_EVENT, 0x0079 NV01_EVENT_OS_EVENT: `hParentClient` (0), the
///   client of the object the event is on, not checked at all (event_api.c,
///   eventConstruct; rmapi_specific.c only moves the event under the
///   caller's client): [`Rule::Process`]. 0x0078/0x007e are refused whole
///   (guestptr.rs).
/// - 0x83de GT200_DEBUGGER: `hAppClient` (4), the debugged client; CPU-RM
///   wants RS_ACCESS_DEBUG on `hClass3dObject` (8) from its share list
///   (ksmdbgssnConstruct): [`Rule::Shared`].
/// - 0xb2cc MAXWELL_PROFILER_DEVICE: `hClientTarget` (0), the two clients'
///   security tokens (kern_profiler_v2.c, profilerDevConstruct; skipped for
///   a caller at USER_ROOT, which no guest process is to the backend):
///   [`Rule::ClientToken`].
/// - 0xc574 UVM_CHANNEL_RETAINER: `hClient` (0); RM makes it for kernel
///   clients only (uvmchanrtnrIsAllocationAllowed): [`Rule::Process`].
/// - 0xcb33 NV_CONFIDENTIAL_COMPUTE: `hClient` (0), never read by
///   confComputeApiConstruct: [`Rule::Process`].
pub const ALLOC_CLIENT_FIELDS: &[(u32, &str, &[Field])] = &[
    (
        0x0080,
        "NV01_DEVICE_0",
        &[f(4, Rule::Token), f(8, Rule::Process)],
    ),
    (0x0005, "NV01_EVENT", &[f(0, Rule::Process)]),
    (0x0079, "NV01_EVENT_OS_EVENT", &[f(0, Rule::Process)]),
    (
        0x83de,
        "GT200_DEBUGGER",
        &[f(
            4,
            Rule::Shared {
                rights: RS_ACCESS_DEBUG_BIT,
                obj: 8,
            },
        )],
    ),
    (
        0xb2cc,
        "MAXWELL_PROFILER_DEVICE",
        &[f(0, Rule::ClientToken)],
    ),
    (0xc574, "UVM_CHANNEL_RETAINER", &[f(0, Rule::Process)]),
    (0xcb33, "NV_CONFIDENTIAL_COMPUTE", &[f(0, Rule::Process)]),
];

/// NV2080_CTRL_CMD_GPU_{INITIALIZE,PROMOTE,EVICT}_CTX: `hClient` at 4 and
/// `hChanClient` at 12, handled in GSP-RM (RMCTRL_FLAGS ROUTE_TO_PHYSICAL).
const CTX_FIELDS: &[Field] = &[f(4, Rule::Process), f(12, Rule::Process)];
const P0: &[Field] = &[f(0, Rule::Process)];

/// Controls whose parameters name a client besides the caller's, with where
/// and RM's rule. Every one with a `NvHandle h*Client*` field in 610.57.04's
/// ctrl headers that a user client reaches (RMCTRL_FLAGS NON_PRIVILEGED or
/// PRIVILEGED in the exported method tables; the kernel-privileged ones RM
/// refuses the backend anyway, and the vGPU host plugin's, diagnostics' and
/// INTERNAL ones are not for it either). What RM does with each:
///
/// - CLIENT_GET_ACCESS_RIGHTS: nothing checked; the answer is the caller's
///   rights on `(hClient, hObject)` (hObject at 0) from the object's share
///   list (cliresCtrlCmdClientGetAccessRights): [`Rule::Shared`], any right.
/// - NV503C REGISTER_PID: nothing checked; registers the named client's
///   ProcID for third-party P2P (thirdpartyp2pCtrlCmdRegisterPid).
/// - the GR ctxsw binds, EXEC_REG_OPS and MIGRATABLE_OPS, PROMOTE, EVICT and
///   INITIALIZE_CTX: sent on to GSP-RM, CPU-RM checking nothing of the
///   client (subdevice_ctrl_gpu_regops.c; ROUTE_TO_PHYSICAL).
/// - FIFO_UPDATE_CHANNEL_INFO: looked up, not checked (kernel_fifo_ctrl.c).
/// - FIFO_GET_CHANNEL_GROUP_UNIQUE_ID_INFO: `_kfifoValidateTargetClient`,
///   the two clients' tokens: [`Rule::ClientToken`].
/// - DMA_INVALIDATE_TLB: not read by CPU-RM (the subdevice's own client is
///   used, dma.c).
///
/// All [`Rule::Process`] but the two named.
pub const CONTROL_CLIENT_FIELDS: &[(u32, &str, &[Field])] = &[
    (
        0x0000_0d03,
        "NV0000_CTRL_CMD_CLIENT_GET_ACCESS_RIGHTS",
        &[f(
            4,
            Rule::Shared {
                rights: RS_ACCESS_ANY,
                obj: 0,
            },
        )],
    ),
    (0x503c_0106, "NV503C_CTRL_CMD_REGISTER_PID", P0),
    (
        0x2080_1205,
        "NV2080_CTRL_CMD_GR_CTXSW_ZCULL_MODE",
        &[f(4, Rule::Process)],
    ),
    (0x2080_1208, "NV2080_CTRL_CMD_GR_CTXSW_ZCULL_BIND", P0),
    (0x2080_1209, "NV2080_CTRL_CMD_GR_CTXSW_PM_BIND", P0),
    (0x2080_123a, "NV2080_CTRL_CMD_GR_CTXSW_SETUP_BIND", P0),
    (
        0x2080_1211,
        "NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND",
        &[f(4, Rule::Process)],
    ),
    (0x2080_0122, "NV2080_CTRL_CMD_GPU_EXEC_REG_OPS", P0),
    (0x2080_01a6, "NV2080_CTRL_CMD_GPU_MIGRATABLE_OPS", P0),
    (0x2080_012b, "NV2080_CTRL_CMD_GPU_PROMOTE_CTX", CTX_FIELDS),
    (0x2080_012c, "NV2080_CTRL_CMD_GPU_EVICT_CTX", CTX_FIELDS),
    (
        0x2080_012d,
        "NV2080_CTRL_CMD_GPU_INITIALIZE_CTX",
        CTX_FIELDS,
    ),
    (0x2080_1116, "NV2080_CTRL_CMD_FIFO_UPDATE_CHANNEL_INFO", P0),
    (
        0x2080_1123,
        "NV2080_CTRL_CMD_FIFO_GET_CHANNEL_GROUP_UNIQUE_ID_INFO",
        &[f(0, Rule::ClientToken)],
    ),
    (0x2080_2502, "NV2080_CTRL_CMD_DMA_INVALIDATE_TLB", P0),
];

/// Controls whose parameters name clients in a list, with where the count
/// is, where the list starts, its length and RM's rule: `(cmd, name, count,
/// list, max, rule)`. RM reads the first `count` entries (NON_PRIVILEGED in
/// 595.99.02 and 610.57.04, the layouts identical). DISABLE_CHANNELS goes
/// to GSP-RM as it is (kernel_fifo_ctrl.c, subdeviceCtrlCmdFifoDisable
/// Channels): the channels of whatever clients it names, stopped, and so do
/// the two key-rotation ones; QUERY_CHANNEL_UNIQUE_ID checks the two
/// clients' tokens (`_kfifoValidateTargetClient`).
pub const CONTROL_CLIENT_LISTS: &[(u32, &str, usize, usize, usize, Rule)] = &[
    (
        0x2080_110b,
        "NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS",
        4,
        24,
        64,
        Rule::Process,
    ),
    (
        0x2080_111a,
        "NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS_FOR_KEY_ROTATION",
        0,
        4,
        64,
        Rule::Process,
    ),
    (
        0x2080_111c,
        "NV2080_CTRL_CMD_FIFO_ROTATE_KEYS",
        0,
        4,
        64,
        Rule::Process,
    ),
    (
        0x2080_1124,
        "NV2080_CTRL_CMD_FIFO_QUERY_CHANNEL_UNIQUE_ID",
        1024,
        0,
        128,
        Rule::ClientToken,
    ),
];

/// A second client a call names: the handle, the call's name, RM's rule, and
/// for [`Rule::Shared`] the object it is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Named {
    pub h: u32,
    pub what: &'static str,
    pub rule: Rule,
    pub obj: u32,
}

/// The clients a listed control names (`CONTROL_CLIENT_LISTS`), zeros left
/// out; a count past the list is refused whole, as RM refuses it.
fn named_list(
    b: &[u8],
    count: usize,
    list: usize,
    max: usize,
    name: &'static str,
    rule: Rule,
) -> Result<Vec<Named>, &'static str> {
    let n = rd32(b, count).ok_or(name)? as usize;
    if n > max {
        return Err(name);
    }
    let fields: Vec<Field> = (0..n).map(|i| f(list + 4 * i, rule)).collect();
    named(b, &fields, name)
}

/// The clients control `cmd`'s parameters `params` name, from either table.
fn control_fields(cmd: u32, params: &[u8]) -> Option<Result<Vec<Named>, &'static str>> {
    if let Some(&(_, name, fields)) = CONTROL_CLIENT_FIELDS.iter().find(|(c, _, _)| *c == cmd) {
        return Some(named(params, fields, name));
    }
    CONTROL_CLIENT_LISTS
        .iter()
        .find(|(c, ..)| *c == cmd)
        .map(|&(_, name, count, list, max, rule)| named_list(params, count, list, max, name, rule))
}

/// NV5080_CTRL_CMD_DEFERRED_API and _V2:`{hApiHandle, cmd, flags,
/// hClientVA, hDeviceVA, union api_bundle}` with the bundle at 24, holding
/// the parameters of the control `cmd` names, run later at the caller's
/// privilege (deferred_api.c). `hClientVA` is looked up, never checked:
/// [`Rule::Process`].
const DEFERRED_API: [u32; 2] = [0x5080_0101, 0x5080_0103];
const DEFERRED_CMD: usize = 4;
const DEFERRED_CLIENT_VA: usize = 12;
const DEFERRED_BUNDLE: usize = 24;

/// The client handles an RM_ALLOC's class parameters (`nested`, after NVOS64)
/// name besides the caller's, zeros left out; `Err` names a field the block
/// is too short to hold. No parameters at all is RM's defaults: none named.
pub fn alloc_named(class: u32, nested: &[u8]) -> Result<Vec<Named>, &'static str> {
    let Some(&(_, name, fields)) = ALLOC_CLIENT_FIELDS.iter().find(|(c, _, _)| *c == class) else {
        return Ok(Vec::new());
    };
    if nested.is_empty() {
        return Ok(Vec::new());
    }
    named(nested, fields, name)
}

/// The client handles an RM control's parameters (`params`, after NVOS54)
/// name besides the caller's.
pub fn control_named(cmd: u32, params: &[u8]) -> Result<Vec<Named>, &'static str> {
    if DEFERRED_API.contains(&cmd) {
        let mut out = named(
            params,
            &[f(DEFERRED_CLIENT_VA, Rule::Process)],
            "NV5080_CTRL_CMD_DEFERRED_API",
        )?;
        let inner = rd32(params, DEFERRED_CMD).ok_or("NV5080_CTRL_CMD_DEFERRED_API")?;
        let bundle = params.get(DEFERRED_BUNDLE..).unwrap_or(&[]);
        if let Some(r) = control_fields(inner, bundle) {
            out.extend(r?);
        }
        return Ok(out);
    }
    control_fields(cmd, params).unwrap_or(Ok(Vec::new()))
}

fn named(b: &[u8], fields: &[Field], name: &'static str) -> Result<Vec<Named>, &'static str> {
    let mut out = Vec::new();
    for fl in fields {
        let h = rd32(b, fl.at).ok_or(name)?;
        if h == 0 {
            continue;
        }
        let obj = match fl.rule {
            Rule::Shared { obj, .. } => rd32(b, obj).ok_or(name)?,
            _ => 0,
        };
        out.push(Named {
            h,
            what: name,
            rule: fl.rule,
            obj,
        });
    }
    Ok(out)
}

/// The share an RM call makes, if it makes one: `(owner client, object,
/// policy)`. NV_ESC_RM_SHARE carries it whole; the two NV0000 controls name
/// the owner as the control's client, and SET_INHERITED_SHARE_POLICY's
/// object is that client.
pub fn share_of(escape: u32, params: &[u8]) -> Option<(u32, u32, Policy)> {
    match escape {
        NV_ESC_RM_SHARE => Some((
            rd32(params, 0)?,
            rd32(params, OS57_OBJECT)?,
            Policy::read(params, OS57_POLICY)?,
        )),
        NV_ESC_RM_CONTROL => {
            let client = rd32(params, 0)?;
            let ctl = params.get(OS54_SIZE..)?;
            match rd32(params, OS54_CMD)? {
                CTRL_SET_INHERITED_SHARE_POLICY => Some((client, client, Policy::read(ctl, 0)?)),
                CTRL_SHARE_OBJECT => Some((client, rd32(ctl, 0)?, Policy::read(ctl, 4)?)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Where the RM status sits in the parameters of `escape`, for the calls
/// refused here.
pub fn status_at(escape: u32) -> Option<usize> {
    match escape {
        NV_ESC_RM_SHARE => Some(OS57_STATUS),
        NV_ESC_RM_DUP_OBJECT => Some(OS55_STATUS),
        NV_ESC_RM_ALLOC => Some(OS64_STATUS),
        NV_ESC_RM_CONTROL => Some(OS54_STATUS),
        _ => None,
    }
}

/// What the ownership state keeps per session (held in semsurf.rs's client
/// set, whose lifecycle it shares): which guest process made each client,
/// and the share lists RM keeps for this VM's objects.
///
/// `grants` has an entry for every object whose list a share RM took has
/// modified -- empty when it grants no other client anything -- holding the
/// CLIENT grants in it. RM checks a duplicate against the object's own list
/// once modified, else its nearest modified ancestor's, else the default
/// (rsAccessGetActiveShareList); the backend does not know an object's
/// ancestors, so it reads an object's own list, and its client's only while
/// no other object of that client has one.
#[derive(Default)]
pub struct Ownership {
    pub owners: HashMap<u32, Caller>,
    pub grants: HashMap<(u32, u32), Vec<Grant>>,
    /// Lists plus the grants in them, against [`GRANT_CAP`].
    pub grant_count: usize,
}

impl Ownership {
    /// `clients` are gone: whatever they made, and every grant to them, too
    /// (RM revokes a policy naming a freed client, clientFreeAccessBackRefs).
    pub fn forget_clients(&mut self, clients: &[u32]) {
        for c in clients {
            self.owners.remove(c);
        }
        self.grants.retain(|&(c, _), _| !clients.contains(&c));
        for list in self.grants.values_mut() {
            list.retain(|g| !clients.contains(&g.target));
        }
        self.recount();
    }

    /// `(client, object)` was freed, and with it whatever RM made under it,
    /// which the backend cannot tell from the client's other objects. Every
    /// other object's grants go too (a handle freed with its parent and made
    /// again would otherwise inherit the grants of the object it replaced);
    /// their lists stay, empty, so the client's own is not read for them in
    /// their place. The client's own list is untouched: it is not freed.
    pub fn object_freed(&mut self, client: u32, object: u32) {
        if object == client {
            return;
        }
        let mut changed = self.grants.remove(&(client, object)).is_some();
        for (&(c, o), list) in self.grants.iter_mut() {
            if c == client && o != client && !list.is_empty() {
                list.clear();
                changed = true;
            }
        }
        if changed {
            self.recount();
        }
    }

    fn recount(&mut self) {
        self.grant_count = self.grants.values().map(|l| 1 + l.len()).sum();
    }

    /// A share RM answered NV_OK for: the object's list is modified.
    pub fn shared(&mut self, owner: u32, object: u32, p: &Policy) {
        let list = self.grants.entry((owner, object)).or_default();
        apply_share(list, owner, p);
        self.recount();
    }

    /// Whether share `p` of `(owner, object)` could take the lists past
    /// [`GRANT_CAP`]: a new list, or a new grant in one.
    pub fn full_for(&self, owner: u32, object: u32, p: &Policy) -> bool {
        self.grant_count >= GRANT_CAP
            && (!self.grants.contains_key(&(owner, object))
                || (p.kind == RS_SHARE_TYPE_CLIENT
                    && p.target != owner
                    && p.action & (RS_SHARE_ACTION_FLAG_REVOKE | RS_SHARE_ACTION_FLAG_REQUIRE)
                        == 0))
    }

    /// Whether the list RM checks `dst`'s use of `(src, obj)` against grants
    /// `dst` any of `rights`: the object's own, if it has one; else its
    /// client's, while no other object of the client has one (it may be an
    /// ancestor of `obj`, whose list RM would read instead). Grants on
    /// intermediate objects (a device, say) are not followed, so such a use
    /// is refused where RM would allow it.
    fn granted(&self, src: u32, obj: u32, dst: u32, rights: u32) -> bool {
        let grants = |l: &Vec<Grant>| l.iter().any(|g| g.target == dst && g.mask & rights != 0);
        if let Some(l) = self.grants.get(&(src, obj)) {
            return grants(l);
        }
        if self.grants.keys().any(|&(c, o)| c == src && o != src) {
            return false;
        }
        self.grants.get(&(src, src)).is_some_and(grants)
    }

    /// Whether clients `a` and `b` were made by one guest process: RM's PID
    /// default between them. Unknown makers are not one.
    fn one_process(&self, a: u32, b: u32) -> bool {
        matches!((self.owners.get(&a), self.owners.get(&b)), (Some(x), Some(y)) if x.same_process(y))
    }

    /// The verdict on a duplicate from `(src, obj)` into client `dst`: RM's
    /// rule (a PID default, or a grant in the list), with guest processes
    /// for the backend's. `live` says whether a handle is a client this VM
    /// has. Within one client there is nothing to keep apart, and a grant is
    /// RM's own record; anything else rests on the guest having said who
    /// made both clients, and is refused when it has not.
    pub fn dup_verdict(
        &self,
        live: impl Fn(u32) -> bool,
        dst: u32,
        src: u32,
        obj: u32,
    ) -> DupVerdict {
        if !live(src) {
            return DupVerdict::ForeignSource;
        }
        if !live(dst) {
            return DupVerdict::ForeignDestination;
        }
        if src == dst
            || self.one_process(src, dst)
            || self.granted(src, obj, dst, RS_ACCESS_DUP_OBJECT_BIT)
        {
            return DupVerdict::Allowed;
        }
        if self.owners.contains_key(&src) && self.owners.contains_key(&dst) {
            DupVerdict::OtherProcess
        } else {
            DupVerdict::Unattributed
        }
    }

    /// Whether a call by `caller` through client `own` may name client `n.h`,
    /// a client of this VM other than `own`: `n.rule` (see [`Rule`]).
    /// Makers the guest did not say match nothing.
    pub fn named_ok(&self, caller: Option<&Caller>, own: u32, n: &Named) -> bool {
        let named = self.owners.get(&n.h);
        let mine = self.owners.get(&own);
        match n.rule {
            Rule::Process => matches!((caller, named), (Some(c), Some(o)) if c.same_process(o)),
            Rule::Token => matches!((caller, named), (Some(c), Some(o)) if c.same_token(o)),
            Rule::ClientToken => matches!((mine, named), (Some(c), Some(o)) if c.same_token(o)),
            Rule::Shared { rights, .. } => {
                self.one_process(own, n.h) || self.granted(n.h, n.obj, own, rights)
            }
        }
    }
}

// ─────────────────────────────── the backend ───────────────────────────────

use crate::nvidia::NvidiaBackend;

/// The reply to a call refused here: the parameters as the guest sent them,
/// with RM's status for a caller without the right at `status`.
pub fn refusal(params: &[u8], status_at: usize, status: u32) -> Vec<u8> {
    let mut out = params.to_vec();
    if let Some(s) = out.get_mut(status_at..status_at + 4) {
        s.copy_from_slice(&status.to_le_bytes());
    }
    out
}

/// How the gate turns a call away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refuse {
    /// RM's status at `status_at(escape)`, the ioctl itself succeeding.
    Status(u32),
    /// A block too short to hold what RM reads: the ioctl fails, as the
    /// host's would (EINVAL), since there is no status field to set.
    Errno(i32),
}

/// What a call the gate let through leaves to record once RM has answered.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pending {
    share: Option<(u32, u32, Policy)>,
}

impl NvidiaBackend {
    /// The calling guest process an RM call carries (`trailer`, what follows
    /// its blocks), if this session has them. `Err` is the errno for a call
    /// that must carry one and does not: an RM_ALLOC of a client, whose
    /// maker is its owner, an RM_DUP_OBJECT, and with BCAP_PROC_EUID every
    /// RM_CONTROL.
    pub(crate) fn rm_proc_id(
        &self,
        escape: u32,
        params: &[u8],
        trailer: &[u8],
    ) -> Result<Option<Caller>, i32> {
        if !self.session.proc_ids {
            return Ok(None);
        }
        let euid = self.session.proc_euid;
        let needed = match escape {
            NV_ESC_RM_DUP_OBJECT => true,
            NV_ESC_RM_CONTROL => euid,
            NV_ESC_RM_ALLOC => {
                rd32(params, OS64_CLASS).is_some_and(|c| crate::semsurf::ROOT_CLASSES.contains(&c))
            }
            _ => false,
        };
        let id = trailer.get(..size_of::<ProcId>()).map(|b| ProcId {
            start_ns: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            tgid: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            euid: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        });
        match id {
            Some(id) => Ok(Some(Caller::from_wire(&id, euid))),
            None if needed => {
                log::warn!(
                    "RM escape {escape:#04x} without the calling process this guest said it \
                     would send; refused"
                );
                Err(libc::EINVAL)
            }
            None => Ok(None),
        }
    }

    /// The checks above, on an RM escape's parameters as the guest sent
    /// them (`params`: the top-level block and the nested one). `Err` is the
    /// RM status to answer with, at `status_at(escape)`, the host never
    /// called.
    pub(crate) fn rm_share_gate(
        &mut self,
        escape: u32,
        params: &[u8],
        caller: Option<Caller>,
    ) -> Result<Pending, Refuse> {
        let mut pending = Pending::default();
        let share = share_of(escape, params);
        let shares = escape == NV_ESC_RM_SHARE
            || (escape == NV_ESC_RM_CONTROL
                && matches!(
                    rd32(params, OS54_CMD),
                    Some(CTRL_SET_INHERITED_SHARE_POLICY | CTRL_SHARE_OBJECT)
                ));
        if shares && (share.is_none() || (escape == NV_ESC_RM_SHARE && params.len() < OS57_SIZE)) {
            log::warn!("RM share ({escape:#04x}) too short to hold its policy; refused");
            return Err(Refuse::Errno(libc::EINVAL));
        }
        if let Some((owner, object, p)) = share {
            let own = self.semsurf.clone();
            match classify_share(owner, &p, |c| own.owns_client(c)) {
                ShareVerdict::Refused(why) => {
                    log::warn!(
                        "RM share of {owner:#x}/{object:#x} (type {}, action {:#x}, target \
                         {:#x}) refused: {why}",
                        p.kind,
                        p.action,
                        p.target
                    );
                    return Err(Refuse::Status(NV_ERR_INSUFFICIENT_PERMISSIONS));
                }
                ShareVerdict::Forward if self.semsurf.grants_full_for(owner, object, &p) => {
                    log::warn!(
                        "RM share of {owner:#x}/{object:#x} refused: {GRANT_CAP} grants \
                         recorded already"
                    );
                    return Err(Refuse::Status(NV_ERR_INSUFFICIENT_RESOURCES));
                }
                ShareVerdict::Forward => pending.share = Some((owner, object, p)),
            }
        }
        match escape {
            NV_ESC_RM_DUP_OBJECT => {
                let Some((dst, src, obj)) = dup_names(params).filter(|_| params.len() >= OS55_SIZE)
                else {
                    log::warn!("RM_DUP_OBJECT too short to name its clients; refused");
                    return Err(Refuse::Errno(libc::EINVAL));
                };
                self.note_no_proc_ids();
                let v = self.semsurf.dup_verdict(dst, src, obj);
                if v != DupVerdict::Allowed {
                    log::warn!(
                        "RM_DUP_OBJECT of {src:#x}/{obj:#x} into client {dst:#x} refused: {}",
                        match v {
                            DupVerdict::ForeignSource => "the source client is not this VM's",
                            DupVerdict::ForeignDestination => "the client is not this VM's",
                            DupVerdict::Unattributed => {
                                "the guest has not said which process made both clients"
                            }
                            _ => "another guest process's client, not shared with this one",
                        }
                    );
                    return Err(Refuse::Status(NV_ERR_INSUFFICIENT_PERMISSIONS));
                }
            }
            NV_ESC_RM_ALLOC => {
                let own = rd32(params, 0).unwrap_or(0);
                let class = rd32(params, OS64_CLASS).unwrap_or(0);
                let nested = params.get(OS64_SIZE..).unwrap_or(&[]);
                self.named_clients_ok(own, caller, alloc_named(class, nested))
                    .map_err(Refuse::Status)?;
            }
            NV_ESC_RM_CONTROL => {
                let own = rd32(params, 0).unwrap_or(0);
                let cmd = rd32(params, OS54_CMD).unwrap_or(0);
                let ctl = params.get(OS54_SIZE..).unwrap_or(&[]);
                self.named_clients_ok(own, caller, control_named(cmd, ctl))
                    .map_err(Refuse::Status)?;
            }
            _ => {}
        }
        Ok(pending)
    }

    /// Once a session: a guest that does not say which of its processes
    /// makes each call gets no RM object between two clients but by a grant.
    fn note_no_proc_ids(&mut self) {
        if !self.session.proc_ids && !self.session.proc_ids_noted {
            self.session.proc_ids_noted = true;
            log::warn!(
                "this guest does not say which of its processes makes each RM call: \
                 duplicates between its clients, and calls naming another client, are refused"
            );
        }
    }

    /// Every second client `named` by a call through client `own` from
    /// `caller`: this VM's, and to its field's rule ([`Rule`]).
    fn named_clients_ok(
        &mut self,
        own: u32,
        caller: Option<Caller>,
        named: Result<Vec<Named>, &'static str>,
    ) -> Result<(), u32> {
        let named = named.map_err(|what| {
            log::warn!("{what}: parameters too short to hold the client they name; refused");
            NV_ERR_INSUFFICIENT_PERMISSIONS
        })?;
        for n in named {
            // The caller's own client, as RM treats it: every rule passes.
            if n.h == own {
                continue;
            }
            if !self.semsurf.owns_client(n.h) {
                log::warn!(
                    "{} names client {:#x}, which is not this VM's; refused",
                    n.what,
                    n.h
                );
                return Err(NV_ERR_INSUFFICIENT_PERMISSIONS);
            }
            self.note_no_proc_ids();
            if !self.semsurf.named_ok(caller.as_ref(), own, &n) {
                log::warn!(
                    "{} names client {:#x}, which RM's rule for it ({:?}) does not let the \
                     caller name; refused",
                    n.what,
                    n.h,
                    n.rule
                );
                return Err(NV_ERR_INSUFFICIENT_PERMISSIONS);
            }
        }
        Ok(())
    }

    /// After RM answered a call the gate let through: the share it made, if
    /// RM made it (`reply`, the parameters RM left, status included).
    pub(crate) fn rm_share_after(&self, escape: u32, pending: Pending, reply: &[u8]) {
        let Some((owner, object, p)) = pending.share else {
            return;
        };
        if status_at(escape).and_then(|at| rd32(reply, at)) == Some(0) {
            self.semsurf.shared(owner, object, &p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(kind: u16, action: u8, target: u32) -> Policy {
        Policy {
            target,
            mask: RS_ACCESS_DUP_OBJECT_BIT,
            kind,
            action,
        }
    }

    const OWNER: u32 = 0xc1d0_0001;
    const PEER: u32 = 0xc1d0_0002;
    const HOST: u32 = 0xc1d0_0999;

    fn vm(c: u32) -> bool {
        c == OWNER || c == PEER
    }

    #[test]
    fn grants_outside_the_vm_are_refused_and_narrowing_goes_through() {
        use ShareVerdict::*;
        for kind in [
            RS_SHARE_TYPE_ALL,
            RS_SHARE_TYPE_OS_SECURITY_TOKEN,
            RS_SHARE_TYPE_SMC_PARTITION,
            RS_SHARE_TYPE_GPU,
            RS_SHARE_TYPE_FM_CLIENT,
            RS_SHARE_TYPE_MAX,
            0xffff,
        ] {
            for action in [0, RS_SHARE_ACTION_FLAG_COMPOSE] {
                assert!(
                    matches!(
                        classify_share(OWNER, &policy(kind, action, 0), vm),
                        Refused(_)
                    ),
                    "type {kind} action {action}"
                );
            }
            // A revoke or a REQUIRE of the same type only narrows.
            for action in [
                RS_SHARE_ACTION_FLAG_REVOKE,
                RS_SHARE_ACTION_FLAG_REQUIRE,
                RS_SHARE_ACTION_FLAG_REQUIRE | RS_SHARE_ACTION_FLAG_COMPOSE,
                RS_SHARE_ACTION_FLAG_REVOKE | RS_SHARE_ACTION_FLAG_REQUIRE,
            ] {
                assert_eq!(
                    classify_share(OWNER, &policy(kind, action, HOST), vm),
                    Forward,
                    "type {kind} action {action}"
                );
            }
        }
        // CLIENT: this VM's, or the owner itself; never another.
        assert_eq!(
            classify_share(OWNER, &policy(RS_SHARE_TYPE_CLIENT, 0, PEER), vm),
            Forward
        );
        assert_eq!(
            classify_share(OWNER, &policy(RS_SHARE_TYPE_CLIENT, 0, OWNER), vm),
            Forward
        );
        assert!(matches!(
            classify_share(OWNER, &policy(RS_SHARE_TYPE_CLIENT, 0, HOST), vm),
            Refused(_)
        ));
        // PID: the owner's process, whatever the target says; NONE: nobody.
        assert_eq!(
            classify_share(OWNER, &policy(RS_SHARE_TYPE_PID, 0, 1), vm),
            Forward
        );
        assert_eq!(
            classify_share(OWNER, &policy(RS_SHARE_TYPE_NONE, 0, 0), vm),
            Forward
        );
    }

    #[test]
    fn recorded_grants_follow_rms_list_semantics() {
        let mut l = Vec::new();
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        apply_share(&mut l, OWNER, &policy(RS_SHARE_TYPE_CLIENT, compose, PEER));
        assert_eq!(
            l,
            [Grant {
                target: PEER,
                mask: 1
            }]
        );
        // Another grant without COMPOSE replaces the list.
        apply_share(&mut l, OWNER, &policy(RS_SHARE_TYPE_CLIENT, 0, HOST));
        assert_eq!(
            l,
            [Grant {
                target: HOST,
                mask: 1
            }]
        );
        // A composed revoke takes the rights, and an empty entry goes.
        apply_share(
            &mut l,
            OWNER,
            &policy(
                RS_SHARE_TYPE_CLIENT,
                compose | RS_SHARE_ACTION_FLAG_REVOKE,
                HOST,
            ),
        );
        assert!(l.is_empty());
        // A PID or self grant records nothing, and without COMPOSE clears.
        apply_share(&mut l, OWNER, &policy(RS_SHARE_TYPE_CLIENT, compose, PEER));
        apply_share(&mut l, OWNER, &policy(RS_SHARE_TYPE_PID, compose, 0));
        assert_eq!(l.len(), 1);
        apply_share(&mut l, OWNER, &policy(RS_SHARE_TYPE_CLIENT, 0, OWNER));
        assert!(l.is_empty());
        // A REQUIRE is not a grant.
        apply_share(
            &mut l,
            OWNER,
            &policy(
                RS_SHARE_TYPE_CLIENT,
                compose | RS_SHARE_ACTION_FLAG_REQUIRE,
                PEER,
            ),
        );
        assert!(l.is_empty());
    }

    #[test]
    fn the_nvos57_policy_is_read_where_rm_keeps_it() {
        let mut p = [0u8; OS57_SIZE];
        p[0..4].copy_from_slice(&OWNER.to_le_bytes());
        p[4..8].copy_from_slice(&0x55u32.to_le_bytes());
        p[8..12].copy_from_slice(&PEER.to_le_bytes());
        p[12..16].copy_from_slice(&1u32.to_le_bytes());
        p[16..18].copy_from_slice(&RS_SHARE_TYPE_CLIENT.to_le_bytes());
        p[18] = RS_SHARE_ACTION_FLAG_COMPOSE;
        let (o, obj, pol) = share_of(NV_ESC_RM_SHARE, &p).unwrap();
        assert_eq!((o, obj), (OWNER, 0x55));
        assert_eq!(
            pol,
            policy(RS_SHARE_TYPE_CLIENT, RS_SHARE_ACTION_FLAG_COMPOSE, PEER)
        );
        // The controls: SHARE_OBJECT's object at 0 and policy at 4,
        // SET_INHERITED_SHARE_POLICY's policy at 0 on the client itself.
        let mut c = vec![0u8; OS54_SIZE + 16];
        c[0..4].copy_from_slice(&OWNER.to_le_bytes());
        c[8..12].copy_from_slice(&CTRL_SHARE_OBJECT.to_le_bytes());
        c[32..36].copy_from_slice(&0x77u32.to_le_bytes());
        c[36..40].copy_from_slice(&PEER.to_le_bytes());
        c[44..46].copy_from_slice(&RS_SHARE_TYPE_ALL.to_le_bytes());
        let (o, obj, pol) = share_of(NV_ESC_RM_CONTROL, &c).unwrap();
        assert_eq!(
            (o, obj, pol.kind, pol.target),
            (OWNER, 0x77, RS_SHARE_TYPE_ALL, PEER)
        );
        c[8..12].copy_from_slice(&CTRL_SET_INHERITED_SHARE_POLICY.to_le_bytes());
        c[40..42].copy_from_slice(&RS_SHARE_TYPE_GPU.to_le_bytes());
        let (o, obj, pol) = share_of(NV_ESC_RM_CONTROL, &c).unwrap();
        assert_eq!((o, obj, pol.kind), (OWNER, OWNER, RS_SHARE_TYPE_GPU));
        // Any other control shares nothing.
        c[8..12].copy_from_slice(&0x2080_0101u32.to_le_bytes());
        assert_eq!(share_of(NV_ESC_RM_CONTROL, &c), None);
    }

    fn id(tgid: u32) -> Caller {
        Caller {
            start_ns: u64::from(tgid) * 1000,
            tgid,
            euid: Some(1000 + tgid),
        }
    }

    fn as_uid(c: Caller, euid: u32) -> Caller {
        Caller {
            euid: Some(euid),
            ..c
        }
    }

    #[test]
    fn a_duplicate_is_rms_rule_between_guest_processes() {
        let mut o = Ownership::default();
        o.owners.insert(OWNER, id(10));
        o.owners.insert(PEER, id(20));
        let live = vm;
        use DupVerdict::*;
        // Across processes: refused, whoever asks -- the caller having made
        // the source is not RM's rule, the two clients' makers are.
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x55), OtherProcess);
        // Within one client, or between two of one process: allowed.
        assert_eq!(o.dup_verdict(live, OWNER, OWNER, 0x55), Allowed);
        o.owners.insert(PEER, id(10));
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x55), Allowed);
        // One process across a setuid is still one process.
        o.owners.insert(PEER, as_uid(id(10), 0));
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x55), Allowed);
        // One euid is not one process: RM's DUP default is by PID.
        o.owners.insert(PEER, as_uid(id(20), 1010));
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x55), OtherProcess);
        o.owners.insert(PEER, id(20));
        // Not this VM's at either end.
        assert_eq!(o.dup_verdict(live, PEER, HOST, 1), ForeignSource);
        assert_eq!(o.dup_verdict(live, HOST, OWNER, 1), ForeignDestination);
        // A client whose maker the guest never said: refused, not assumed.
        let mut anon = Ownership::default();
        assert_eq!(anon.dup_verdict(live, PEER, OWNER, 0x55), Unattributed);
        assert_eq!(anon.dup_verdict(live, OWNER, OWNER, 0x55), Allowed);
        anon.owners.insert(OWNER, id(10));
        assert_eq!(anon.dup_verdict(live, PEER, OWNER, 0x55), Unattributed);
        // ... but a grant RM took is RM's own record, and counts.
        let grant = policy(RS_SHARE_TYPE_CLIENT, RS_SHARE_ACTION_FLAG_COMPOSE, PEER);
        anon.shared(OWNER, 0x55, &grant);
        assert_eq!(anon.dup_verdict(live, PEER, OWNER, 0x55), Allowed);
        // A CLIENT grant on the object, or on its client, opens it to PEER.
        o.shared(OWNER, 0x55, &grant);
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x55), Allowed);
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x56), OtherProcess);
        o.shared(OWNER, OWNER, &grant);
        // The client's list is not read for 0x56 while 0x55 has one of its
        // own: 0x55 may be 0x56's parent, whose list RM would read.
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x56), OtherProcess);
        // A grant without DUP_OBJECT does not open anything.
        let mut o2 = Ownership {
            owners: o.owners.clone(),
            ..Default::default()
        };
        o2.shared(
            OWNER,
            0x55,
            &Policy {
                mask: RS_ACCESS_DEBUG_BIT,
                ..grant
            },
        );
        assert_eq!(o2.dup_verdict(live, PEER, OWNER, 0x55), OtherProcess);
        // Freeing the object takes its list, and the client's is read again.
        o.object_freed(OWNER, 0x55);
        assert_eq!(o.grant_count, 2, "the client's list and its one grant");
        assert_eq!(o.dup_verdict(live, PEER, OWNER, 0x56), Allowed);
        // Freeing a client takes everything of it, and every grant to it.
        o.shared(OWNER, 0x55, &grant);
        o.forget_clients(&[PEER]);
        assert!(o.grants.values().all(Vec::is_empty));
        assert!(!o.owners.contains_key(&PEER));
        o.forget_clients(&[OWNER]);
        assert!(o.grants.is_empty());
        assert_eq!(o.grant_count, 0);
    }

    /// Each rule a second client is held to, between guest processes.
    #[test]
    fn a_named_client_is_held_to_its_fields_rule() {
        let mut o = Ownership::default();
        let (me, same_uid, other) = (id(10), as_uid(id(20), 1010), id(30));
        o.owners.insert(OWNER, me);
        let n = |h: u32, rule: Rule| Named {
            h,
            what: "t",
            rule,
            obj: 0x55,
        };
        for (maker, process, token) in [
            (me, true, true),
            (same_uid, false, true),
            (other, false, false),
        ] {
            o.owners.insert(PEER, maker);
            assert_eq!(
                o.named_ok(Some(&me), OWNER, &n(PEER, Rule::Process)),
                process
            );
            assert_eq!(o.named_ok(Some(&me), OWNER, &n(PEER, Rule::Token)), token);
            assert_eq!(
                o.named_ok(Some(&me), OWNER, &n(PEER, Rule::ClientToken)),
                token
            );
            let shared = Rule::Shared {
                rights: RS_ACCESS_DEBUG_BIT,
                obj: 0,
            };
            assert_eq!(o.named_ok(Some(&me), OWNER, &n(PEER, shared)), process);
        }
        // Token is the caller's; ClientToken the caller's client's maker's.
        o.owners.insert(PEER, other);
        let other_as_me = as_uid(other, 1010);
        assert!(!o.named_ok(Some(&other_as_me), OWNER, &n(PEER, Rule::ClientToken)));
        assert!(o.named_ok(Some(&other), OWNER, &n(PEER, Rule::Token)));
        // An euid the guest never said matches nothing but the process.
        let quiet = |c: Caller| Caller { euid: None, ..c };
        o.owners.insert(PEER, quiet(same_uid));
        assert!(!o.named_ok(Some(&quiet(me)), OWNER, &n(PEER, Rule::Token)));
        // No caller, or an unattributed client: refused.
        o.owners.insert(PEER, me);
        assert!(!o.named_ok(None, OWNER, &n(PEER, Rule::Process)));
        assert!(!o.named_ok(Some(&me), OWNER, &n(HOST, Rule::Token)));
        // Shared: a CLIENT grant of the right, on that object, to the
        // caller's client.
        o.owners.insert(PEER, other);
        let debug = Rule::Shared {
            rights: RS_ACCESS_DEBUG_BIT,
            obj: 0,
        };
        o.shared(
            PEER,
            0x55,
            &Policy {
                target: OWNER,
                mask: RS_ACCESS_DUP_OBJECT_BIT,
                kind: RS_SHARE_TYPE_CLIENT,
                action: RS_SHARE_ACTION_FLAG_COMPOSE,
            },
        );
        assert!(
            !o.named_ok(Some(&me), OWNER, &n(PEER, debug)),
            "DUP is not DEBUG"
        );
        o.shared(
            PEER,
            0x55,
            &Policy {
                target: OWNER,
                mask: RS_ACCESS_DEBUG_BIT,
                kind: RS_SHARE_TYPE_CLIENT,
                action: RS_SHARE_ACTION_FLAG_COMPOSE,
            },
        );
        assert!(o.named_ok(Some(&me), OWNER, &n(PEER, debug)));
        assert!(!o.named_ok(
            Some(&me),
            OWNER,
            &Named {
                obj: 0x56,
                ..n(PEER, debug)
            }
        ));
    }

    /// RM checks a duplicate against the object's own list once a share
    /// has modified it, not its client's (rsAccessGetActiveShareList); and a
    /// handle freed with its parent can be made again, a new object.
    #[test]
    fn an_objects_own_list_and_a_freed_parent_are_followed() {
        let live = vm;
        use DupVerdict::*;
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        let mut o = Ownership::default();
        o.owners.insert(OWNER, id(10));
        o.owners.insert(PEER, id(20));
        let dup = |o: &Ownership, obj| o.dup_verdict(live, PEER, OWNER, obj);
        // The client grants PEER; the object revokes it: RM's list for the
        // object is its own, without PEER (and with the PID default, which
        // RM matches for every client of the backend's).
        o.shared(OWNER, OWNER, &policy(RS_SHARE_TYPE_CLIENT, compose, PEER));
        assert_eq!(dup(&o, 0x55), Allowed);
        o.shared(
            OWNER,
            0x55,
            &policy(
                RS_SHARE_TYPE_CLIENT,
                compose | RS_SHARE_ACTION_FLAG_REVOKE,
                PEER,
            ),
        );
        assert_eq!(dup(&o, 0x55), OtherProcess);
        // A PID-only list on it, likewise.
        o.shared(OWNER, 0x66, &policy(RS_SHARE_TYPE_PID, 0, 0));
        assert_eq!(dup(&o, 0x66), OtherProcess);
        // A grant on 0x77 whose parent is freed: 0x77 went with it, and a
        // new object at 0x77 is not the one granted.
        o.shared(OWNER, 0x77, &policy(RS_SHARE_TYPE_CLIENT, compose, PEER));
        assert_eq!(dup(&o, 0x77), Allowed);
        o.object_freed(OWNER, 0xde7);
        assert_eq!(dup(&o, 0x77), OtherProcess);
        // Nor does the client's list stand in for the one 0x77 had.
        assert!(o.grants.contains_key(&(OWNER, 0x77)));
        // Freeing the client itself is not an object's free.
        o.object_freed(OWNER, OWNER);
        assert!(o.grants.contains_key(&(OWNER, OWNER)));
    }

    #[test]

    #[cfg_attr(miri, ignore = "fills a cap of thousands: too slow under Miri")]
    fn the_cap_counts_lists_and_grants() {
        let mut o = Ownership::default();
        let grant = policy(RS_SHARE_TYPE_CLIENT, RS_SHARE_ACTION_FLAG_COMPOSE, PEER);
        let revoke = policy(RS_SHARE_TYPE_CLIENT, RS_SHARE_ACTION_FLAG_REVOKE, PEER);
        for obj in 0..(GRANT_CAP as u32 / 2) {
            assert!(!o.full_for(OWNER, obj + 0x100, &grant));
            o.shared(OWNER, obj + 0x100, &grant);
        }
        assert_eq!(o.grant_count, GRANT_CAP);
        // Full: no new list, not even a revoke's, and no new grant...
        assert!(o.full_for(OWNER, 0x5, &revoke));
        assert!(o.full_for(OWNER, 0x100, &policy(RS_SHARE_TYPE_CLIENT, 0, OWNER + 7)));
        // ...but a revoke in a list there is fine.
        assert!(!o.full_for(OWNER, 0x100, &revoke));
    }

    #[test]
    fn second_clients_are_found_in_class_and_control_parameters() {
        let mut dev = vec![0u8; 56];
        assert_eq!(alloc_named(0x80, &dev), Ok(vec![]));
        dev[4..8].copy_from_slice(&HOST.to_le_bytes());
        let hs = |v: Vec<Named>| v.iter().map(|n| (n.h, n.rule)).collect::<Vec<_>>();
        assert_eq!(hs(alloc_named(0x80, &dev).unwrap()), [(HOST, Rule::Token)]);
        dev[8..12].copy_from_slice(&PEER.to_le_bytes());
        assert_eq!(
            hs(alloc_named(0x80, &dev).unwrap()),
            [(HOST, Rule::Token), (PEER, Rule::Process)]
        );
        // The debugger names its object too.
        let mut dbg = vec![0u8; 12];
        dbg[4..8].copy_from_slice(&PEER.to_le_bytes());
        dbg[8..12].copy_from_slice(&0x3d0u32.to_le_bytes());
        let d = alloc_named(0x83de, &dbg).unwrap();
        assert_eq!((d[0].h, d[0].obj), (PEER, 0x3d0));
        assert!(matches!(
            d[0].rule,
            Rule::Shared {
                rights: RS_ACCESS_DEBUG_BIT,
                ..
            }
        ));
        // No parameters: RM's defaults. Too few: refused.
        assert_eq!(alloc_named(0x80, &[]), Ok(vec![]));
        assert!(alloc_named(0x83de, &[0u8; 6]).is_err());
        // A class with no second client names none.
        assert_eq!(alloc_named(0x90f1, &dev), Ok(vec![]));

        let mut regops = vec![0u8; 48];
        regops[0..4].copy_from_slice(&HOST.to_le_bytes());
        let r = control_named(0x2080_0122, &regops).unwrap();
        assert_eq!(
            (r[0].h, r[0].what, r[0].rule),
            (HOST, "NV2080_CTRL_CMD_GPU_EXEC_REG_OPS", Rule::Process)
        );
        // DEFERRED_API: its own VA client and the bundled control's.
        let mut d = vec![0u8; 584];
        d[4..8].copy_from_slice(&0x2080_012bu32.to_le_bytes());
        d[12..16].copy_from_slice(&OWNER.to_le_bytes());
        d[24 + 12..24 + 16].copy_from_slice(&HOST.to_le_bytes());
        let n = control_named(0x5080_0101, &d).unwrap();
        assert_eq!(n.iter().map(|x| x.h).collect::<Vec<_>>(), [OWNER, HOST]);
        assert_eq!(control_named(0x2080_0101, &d), Ok(vec![]));
    }

    #[test]
    fn clients_in_a_list_are_found_up_to_its_count() {
        // DISABLE_CHANNELS: numChannels at 4, hClientList at 24.
        let mut d = vec![0u8; 536];
        d[4..8].copy_from_slice(&2u32.to_le_bytes());
        d[24..28].copy_from_slice(&OWNER.to_le_bytes());
        d[28..32].copy_from_slice(&HOST.to_le_bytes());
        // Past the count: not read by RM, nor here.
        d[32..36].copy_from_slice(&PEER.to_le_bytes());
        let n = control_named(0x2080_110b, &d).unwrap();
        assert_eq!(n.iter().map(|x| x.h).collect::<Vec<_>>(), [OWNER, HOST]);
        assert!(n.iter().all(|x| x.rule == Rule::Process));
        // A count past the list: refused whole.
        d[4..8].copy_from_slice(&65u32.to_le_bytes());
        assert!(control_named(0x2080_110b, &d).is_err());
        // QUERY_CHANNEL_UNIQUE_ID: hClients at 0, numChannels at 1024.
        let mut q = vec![0u8; 1540];
        q[1024..1028].copy_from_slice(&1u32.to_le_bytes());
        q[0..4].copy_from_slice(&HOST.to_le_bytes());
        let r = control_named(0x2080_1124, &q).unwrap();
        assert_eq!(
            (r[0].h, r[0].what, r[0].rule),
            (
                HOST,
                "NV2080_CTRL_CMD_FIFO_QUERY_CHANNEL_UNIQUE_ID",
                Rule::ClientToken
            )
        );
        // ROTATE_KEYS and its sibling: numChannels at 0, the list at 4.
        let mut k = vec![0u8; 520];
        k[0..4].copy_from_slice(&1u32.to_le_bytes());
        k[4..8].copy_from_slice(&HOST.to_le_bytes());
        for cmd in [0x2080_111a, 0x2080_111c] {
            assert_eq!(control_named(cmd, &k).unwrap()[0].h, HOST);
        }
        // Too short for the count it gives: refused.
        assert!(control_named(0x2080_111c, &k[..6]).is_err());
    }

    /// Every control's fields fit the parameters it has on the host
    /// (sizeof, 610.57.04; checked against 595.99.02's headers too).
    #[test]
    fn every_listed_field_is_inside_its_structure() {
        let sizes: &[(u32, usize)] = &[
            (0x0000_0d03, 12),
            (0x503c_0106, 4),
            (0x2080_1205, 16),
            (0x2080_1208, 24),
            (0x2080_1209, 40),
            (0x2080_123a, 16),
            (0x2080_1211, 112),
            (0x2080_0122, 48),
            (0x2080_01a6, 1832),
            (0x2080_012b, 560),
            (0x2080_012c, 20),
            (0x2080_012d, 56),
            (0x2080_1116, 32),
            (0x2080_1123, 1552),
            (0x2080_2502, 16),
        ];
        for (cmd, name, fields) in CONTROL_CLIENT_FIELDS {
            let size = sizes
                .iter()
                .find(|(c, _)| c == cmd)
                .unwrap_or_else(|| panic!("{name}"));
            assert!(
                fields.iter().all(|f| {
                    f.at + 4 <= size.1
                        && match f.rule {
                            Rule::Shared { obj, .. } => obj + 4 <= size.1,
                            _ => true,
                        }
                }),
                "{name}"
            );
        }
        // The lists: (sizeof, 610.57.04 and 595.99.02).
        let list_sizes: &[(u32, usize)] = &[
            (0x2080_110b, 536),
            (0x2080_111a, 520),
            (0x2080_111c, 520),
            (0x2080_1124, 1540),
        ];
        for &(cmd, name, count, list, max, _) in CONTROL_CLIENT_LISTS {
            let size = list_sizes
                .iter()
                .find(|(c, _)| *c == cmd)
                .unwrap_or_else(|| panic!("{name}"));
            assert!(count + 4 <= size.1 && list + 4 * max <= size.1, "{name}");
        }
        let mut seen = std::collections::HashSet::new();
        let lists = CONTROL_CLIENT_LISTS.iter().map(|&(c, n, ..)| (c, n));
        for (c, name) in CONTROL_CLIENT_FIELDS
            .iter()
            .chain(ALLOC_CLIENT_FIELDS)
            .map(|&(c, n, _)| (c, n))
            .chain(lists)
        {
            assert!(seen.insert(c), "{name} twice");
        }
    }
}

/// Whole calls through the backend's v1 RM path, against a fake host RM
/// that records what reaches it: a refusal here is one the host never saw.
#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::hostfd::{self, HandleKind, IOC_RW, ioc};
    use protocol::messages::{
        BCAP_PROC_EUID, BCAP_PROC_ID, DeviceKind, GCAP_PROC_EUID, GCAP_PROC_ID, HELLO_F_FRESH,
        HelloReq, MsgType, PROTO_V2, ProcId,
    };
    use std::cell::{Cell, RefCell};
    use std::os::fd::{OwnedFd, RawFd};

    const ALLOC: u32 = ioc(IOC_RW, b'F', 0x2b, 48);
    const CONTROL: u32 = ioc(IOC_RW, b'F', 0x2a, 32);
    const FREE: u32 = ioc(IOC_RW, b'F', 0x29, 16);
    const DUP: u32 = ioc(IOC_RW, b'F', 0x34, OS55_SIZE);
    const SHARE: u32 = ioc(IOC_RW, b'F', 0x35, OS57_SIZE);
    /// A client of the host's that no guest allocated: another VM's backend,
    /// the host compositor, the backend's own private one.
    const HOST: u32 = 0xc1d0_0999;
    /// Where a v1 reply's parameters start: MsgHeader, IoctlResp.
    const BODY: usize = 16 + 12;

    std::thread_local! {
        /// (escape, hClient) of every RM call that reached the host.
        static SEEN: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
        static NEXT_CLIENT: Cell<u32> = const { Cell::new(0xc1d0_0001) };
    }

    fn seen() -> Vec<(u32, u32)> {
        SEEN.with(|s| std::mem::take(&mut *s.borrow_mut()))
    }

    fn reached(nr: u32) -> bool {
        seen().iter().any(|&(n, _)| n == nr)
    }

    /// A host RM that allocates clients with fresh handles and answers
    /// NV_OK to everything else.
    fn fake_rm(_: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let request = request as u32;
        let a = &mut arg.bytes()[..hostfd::ioc_size(request)];
        let nr = hostfd::ioc_nr(request);
        SEEN.with(|s| s.borrow_mut().push((nr, rd32(a, 0).unwrap())));
        let status = match nr {
            0x2b => {
                if crate::semsurf::ROOT_CLASSES.contains(&rd32(a, 12).unwrap()) {
                    let h = NEXT_CLIENT.with(|c| c.replace(c.get() + 1));
                    a[8..12].copy_from_slice(&h.to_le_bytes());
                }
                40
            }
            0x2a => 28,
            0x29 => 12,
            0x34 => 24,
            // A share of object 0xbad is one RM turns down.
            0x35 if rd32(a, 4) == Some(0xbad) => {
                a[20..24].copy_from_slice(&NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes());
                return 0;
            }
            0x35 => 20,
            _ => return 0,
        };
        a[status..status + 4].fill(0);
        0
    }

    /// What a current guest module says it can do.
    const FULL: u32 = GCAP_PROC_ID | GCAP_PROC_EUID;

    /// A session, v2 with the guest capabilities `gcaps`, and two control
    /// files.
    fn vm(gcaps: u32) -> (NvidiaBackend, u32, u32) {
        seen();
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_rm);
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: gcaps,
            uvm_aperture_mib: 0,
        };
        let mut req = Vec::new();
        for v in [MsgType::Hello as u32, 0, 0, 1] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        for v in [
            hello.proto,
            hello.flags,
            hello.guest_caps,
            hello.uvm_aperture_mib,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        let mut resp = vec![0u8; 256];
        let n = be.dispatch(&req, &mut resp);
        assert!(n >= 16 + 8);
        let caps = rd32(&resp, 16 + 4).unwrap();
        assert_eq!(
            caps & BCAP_PROC_ID != 0,
            gcaps & GCAP_PROC_ID != 0,
            "offered only when asked for"
        );
        assert_eq!(
            caps & BCAP_PROC_EUID != 0,
            gcaps & FULL == FULL,
            "and the euid only with the process"
        );
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let a = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        let b = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        (be, a, b)
    }

    /// Guest process `tgid`, running as uid `1000 + tgid`.
    fn pid(tgid: u32) -> ProcId {
        ProcId {
            start_ns: 1_000_000 + u64::from(tgid),
            tgid,
            euid: 1000 + tgid,
        }
    }

    /// The same, as `euid`.
    fn pid_as(tgid: u32, euid: u32) -> ProcId {
        ProcId { euid, ..pid(tgid) }
    }

    /// What the backend recorded of a maker.
    fn caller(p: ProcId) -> Caller {
        Caller::from_wire(&p, true)
    }

    /// A v1 IOCTL, with the calling process after its blocks when `by` says.
    fn call(
        be: &mut NvidiaBackend,
        handle: u32,
        cmd: u32,
        outer: &[u8],
        nested: &[u8],
        by: Option<ProcId>,
    ) -> Vec<u8> {
        let mut req = Vec::new();
        let nested_offset = if nested.is_empty() { 0 } else { outer.len() };
        for v in [
            MsgType::Ioctl as u32,
            handle,
            0,
            0,
            cmd,
            outer.len() as u32,
            nested_offset as u32,
            nested.len() as u32,
            0,
            0,
        ] {
            req.extend_from_slice(&v.to_le_bytes());
        }
        req.extend_from_slice(outer);
        req.extend_from_slice(nested);
        if let Some(p) = by {
            req.extend_from_slice(&p.start_ns.to_le_bytes());
            req.extend_from_slice(&p.tgid.to_le_bytes());
            req.extend_from_slice(&p.euid.to_le_bytes());
        }
        let mut resp = vec![0u8; 4096];
        let n = be.dispatch(&req, &mut resp);
        resp.truncate(n);
        resp
    }

    fn errno(r: &[u8]) -> i32 {
        i32::from_le_bytes(r[8..12].try_into().unwrap())
    }

    /// The RM status at `at` of a reply the ioctl itself succeeded with.
    fn rm_status(r: &[u8], at: usize) -> u32 {
        assert_eq!(errno(r), 0, "the ioctl succeeds; RM's status says");
        rd32(r, BODY + at).unwrap()
    }

    fn words(ws: &[(usize, u32)], len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        for &(at, v) in ws {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn alloc_client(be: &mut NvidiaBackend, on: u32, by: Option<ProcId>) -> u32 {
        let r = call(be, on, ALLOC, &words(&[(12, 0x41)], 48), &[], by);
        assert_eq!(rm_status(&r, OS64_STATUS), 0);
        rd32(&r, BODY + 8).unwrap()
    }

    fn dup(dst: u32, src: u32, obj: u32) -> Vec<u8> {
        words(
            &[(0, dst), (4, dst), (8, 0xd00d), (12, src), (16, obj)],
            OS55_SIZE,
        )
    }

    fn share(owner: u32, obj: u32, kind: u16, action: u8, target: u32) -> Vec<u8> {
        let mut p = words(&[(0, owner), (4, obj), (8, target), (12, 1)], OS57_SIZE);
        p[16..18].copy_from_slice(&kind.to_le_bytes());
        p[18] = action;
        p
    }

    #[test]
    fn a_duplicate_within_one_process_reaches_rm_across_its_files() {
        let (mut be, f1, f2) = vm(FULL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let b = alloc_client(&mut be, f2, Some(pid(10)));
        assert_eq!(be.semsurf.owner_of(a), Some(caller(pid(10))));
        seen();
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(10)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        assert!(reached(0x34));
        // Within one client, whoever asks.
        let r = call(&mut be, f1, DUP, &dup(a, a, 0x55), &[], Some(pid(11)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
    }

    #[test]
    fn a_duplicate_across_processes_is_refused_before_rm() {
        let (mut be, f1, f2) = vm(FULL);
        // A forked child's client and object; the parent's own client.
        let child = alloc_client(&mut be, f1, Some(pid(20)));
        let parent = alloc_client(&mut be, f2, Some(pid(10)));
        seen();
        let r = call(
            &mut be,
            f2,
            DUP,
            &dup(parent, child, 0x55),
            &[],
            Some(pid(10)),
        );
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x34), "the host never saw it");
        // Nor by the child, into the parent's client (a file the parent
        // passed it): RM's rule is the two clients' makers, not the
        // caller's, and a host process holding another's file could not
        // either.
        seen();
        let r = call(
            &mut be,
            f2,
            DUP,
            &dup(parent, child, 0x55),
            &[],
            Some(pid(20)),
        );
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x34));
        // Nor between two processes of one uid: RM's default is by PID.
        let same_uid = alloc_client(&mut be, f1, Some(pid_as(40, 1010)));
        let r = call(
            &mut be,
            f2,
            DUP,
            &dup(parent, same_uid, 0x55),
            &[],
            Some(pid(10)),
        );
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        // A client passed on keeps its maker: the parent using the child's
        // file is still not the child.
        let r = call(
            &mut be,
            f1,
            DUP,
            &dup(child, parent, 0x66),
            &[],
            Some(pid(30)),
        );
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
    }

    #[test]
    fn a_foreign_source_or_destination_client_is_refused_old_guest_or_new() {
        for proc_ids in [true, false] {
            let (mut be, f1, _) = vm(if proc_ids { FULL } else { 0 });
            let by = proc_ids.then(|| pid(10));
            let a = alloc_client(&mut be, f1, by);
            seen();
            let r = call(&mut be, f1, DUP, &dup(a, HOST, 1), &[], by);
            assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
            let r = call(&mut be, f1, DUP, &dup(HOST, a, 1), &[], by);
            assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
            assert!(!reached(0x34));
            // Freed is foreign too.
            let r = call(&mut be, f1, FREE, &words(&[(0, a), (8, a)], 16), &[], by);
            assert_eq!(errno(&r), 0);
            let b = alloc_client(&mut be, f1, by);
            let r = call(&mut be, f1, DUP, &dup(b, a, 1), &[], by);
            assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        }
    }

    #[test]
    fn a_guest_that_cannot_say_who_calls_fails_closed() {
        // No GCAP_PROC_ID: no process ids, and none can be asked for.
        let (mut be, f1, f2) = vm(0);
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let a = alloc_client(&mut be, f1, None);
        let b = alloc_client(&mut be, f2, None);
        assert_eq!(be.semsurf.owner_of(a), None);
        // Between two clients: refused, where it was let through before.
        seen();
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], None);
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x34));
        // Within one client: nothing to tell apart.
        let r = call(&mut be, f1, DUP, &dup(a, a, 0x55), &[], None);
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        // A grant RM took is RM's own rule, and holds without a process.
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x55, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], None);
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        // A second client named in parameters: refused, whatever the rule.
        seen();
        let regops = words(&[(0, a), (4, 0x2080), (8, 0x2080_0122), (24, 48)], 32);
        let r = call(&mut be, f1, CONTROL, &regops, &words(&[(0, b)], 48), None);
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let outer = words(&[(0, a), (4, a), (8, 0xde7), (12, 0x80), (32, 56)], 48);
        let r = call(&mut be, f1, ALLOC, &outer, &words(&[(4, b)], 56), None);
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a) && !reached(0x2b));
        // Its own client it may name.
        let r = call(&mut be, f1, CONTROL, &regops, &words(&[(0, a)], 48), None);
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
    }

    #[test]
    fn a_guest_that_said_it_would_name_the_caller_must() {
        let (mut be, f1, _) = vm(FULL);
        // A client, or a duplicate, without it: a broken guest, refused.
        let r = call(&mut be, f1, ALLOC, &words(&[(12, 0x41)], 48), &[], None);
        assert_eq!(errno(&r), -libc::EINVAL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let r = call(&mut be, f1, DUP, &dup(a, a, 1), &[], None);
        assert_eq!(errno(&r), -libc::EINVAL);
        // A share or a duplicate too short for RM's block fails as an ioctl.
        let short = ioc(IOC_RW, b'F', 0x35, 16);
        let r = call(
            &mut be,
            f1,
            short,
            &share(a, a, RS_SHARE_TYPE_ALL, 0, 0)[..16],
            &[],
            None,
        );
        assert_eq!(errno(&r), -libc::EINVAL);
        let short = ioc(IOC_RW, b'F', 0x34, 12);
        let r = call(&mut be, f1, short, &dup(a, a, 1)[..12], &[], Some(pid(10)));
        assert_eq!(errno(&r), -libc::EINVAL);
        // Any other allocation needs none.
        let r = call(
            &mut be,
            f1,
            ALLOC,
            &words(&[(0, a), (4, a), (12, 0x90f1)], 48),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS64_STATUS), 0);
    }

    #[test]
    fn outward_grants_are_refused_and_narrowing_reaches_rm() {
        let (mut be, f1, _) = vm(FULL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        for (kind, target) in [
            (RS_SHARE_TYPE_ALL, 0),
            (RS_SHARE_TYPE_OS_SECURITY_TOKEN, 0),
            (RS_SHARE_TYPE_SMC_PARTITION, 0),
            (RS_SHARE_TYPE_GPU, 0),
            (RS_SHARE_TYPE_FM_CLIENT, 0),
            (RS_SHARE_TYPE_CLIENT, HOST),
            (RS_SHARE_TYPE_MAX, 0),
        ] {
            seen();
            let r = call(
                &mut be,
                f1,
                SHARE,
                &share(a, 0x55, kind, 0, target),
                &[],
                None,
            );
            assert_eq!(
                rm_status(&r, OS57_STATUS),
                NV_ERR_INSUFFICIENT_PERMISSIONS,
                "type {kind}"
            );
            assert!(!reached(0x35), "type {kind} never reaches RM");
            // Revoking it, or requiring it, only narrows.
            for action in [RS_SHARE_ACTION_FLAG_REVOKE, RS_SHARE_ACTION_FLAG_REQUIRE] {
                let r = call(
                    &mut be,
                    f1,
                    SHARE,
                    &share(a, 0x55, kind, action, target),
                    &[],
                    None,
                );
                assert_eq!(rm_status(&r, OS57_STATUS), 0);
                assert!(reached(0x35), "type {kind} action {action}");
            }
        }
        // The same through the controls that share.
        let mut ctl = words(&[(0, a), (4, a), (8, CTRL_SHARE_OBJECT), (24, 16)], 32);
        let mut p = words(&[(0, 0x55)], 16);
        p[12..14].copy_from_slice(&RS_SHARE_TYPE_ALL.to_le_bytes());
        seen();
        let r = call(&mut be, f1, CONTROL, &ctl, &p, Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        ctl[8..12].copy_from_slice(&CTRL_SET_INHERITED_SHARE_POLICY.to_le_bytes());
        let mut p = vec![0u8; 12];
        p[8..10].copy_from_slice(&RS_SHARE_TYPE_OS_SECURITY_TOKEN.to_le_bytes());
        let r = call(&mut be, f1, CONTROL, &ctl, &p, Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a));
    }

    #[test]
    fn a_client_grant_opens_an_object_to_another_process_until_revoked() {
        let (mut be, f1, f2) = vm(FULL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let b = alloc_client(&mut be, f2, Some(pid(20)));
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        // A PID grant is the owner's own process: it reaches RM and opens
        // nothing to another process.
        seen();
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x55, RS_SHARE_TYPE_PID, compose, 20),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        assert!(reached(0x35));
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        // A CLIENT grant to b does, for that object.
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x55, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        assert!(reached(0x35), "RM holds the grant too");
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x56), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        // Revoked, it is closed again.
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(
                a,
                0x55,
                RS_SHARE_TYPE_CLIENT,
                compose | RS_SHARE_ACTION_FLAG_REVOKE,
                b,
            ),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        // On the client (SET_INHERITED_SHARE_POLICY), every object of it
        // that has no list of its own -- once 0x55, which has one, is gone.
        let r = call(
            &mut be,
            f1,
            FREE,
            &words(&[(0, a), (4, a), (8, 0x55)], 16),
            &[],
            None,
        );
        assert_eq!(errno(&r), 0);
        let ctl = words(
            &[
                (0, a),
                (4, a),
                (8, CTRL_SET_INHERITED_SHARE_POLICY),
                (24, 12),
            ],
            32,
        );
        let mut p = words(&[(0, b), (4, 1)], 12);
        p[8..10].copy_from_slice(&RS_SHARE_TYPE_CLIENT.to_le_bytes());
        p[10] = compose;
        let r = call(&mut be, f1, CONTROL, &ctl, &p, Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x77), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        // A revoke without COMPOSE empties the client's list.
        p[10] = RS_SHARE_ACTION_FLAG_REVOKE;
        let r = call(&mut be, f1, CONTROL, &ctl, &p, Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x77), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        // A grant RM refused is not recorded; one on a freed object is gone.
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0xbad, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0xbad), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x88, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(
            &mut be,
            f1,
            FREE,
            &words(&[(0, a), (4, a), (8, 0x88)], 16),
            &[],
            None,
        );
        assert_eq!(errno(&r), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x88), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
    }

    #[test]
    fn a_second_client_named_in_parameters_must_be_this_vms() {
        let (mut be, f1, _) = vm(FULL);
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        // A device sharing a host client's VA space.
        let dev = |share: u32| words(&[(4, share)], 56);
        let outer = words(&[(0, a), (4, a), (8, 0xde7), (12, 0x80), (32, 56)], 48);
        seen();
        let r = call(&mut be, f1, ALLOC, &outer, &dev(HOST), None);
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2b));
        let r = call(&mut be, f1, ALLOC, &outer, &dev(a), None);
        assert_eq!(rm_status(&r, OS64_STATUS), 0);
        let r = call(&mut be, f1, ALLOC, &outer, &dev(0), None);
        assert_eq!(rm_status(&r, OS64_STATUS), 0);
        // Register operations on a host client's channel.
        let ctl = words(&[(0, a), (4, 0x2080), (8, 0x2080_0122), (24, 48)], 32);
        seen();
        let r = call(
            &mut be,
            f1,
            CONTROL,
            &ctl,
            &words(&[(0, HOST)], 48),
            Some(pid(10)),
        );
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a));
        let r = call(
            &mut be,
            f1,
            CONTROL,
            &ctl,
            &words(&[(0, a)], 48),
            Some(pid(10)),
        );
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
    }

    #[test]
    fn a_second_client_is_held_to_rms_rule_for_its_field() {
        let (mut be, f1, f2) = vm(FULL);
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let me = pid(10);
        let a = alloc_client(&mut be, f1, Some(me));
        let mine_too = alloc_client(&mut be, f2, Some(me));
        let same_uid = alloc_client(&mut be, f2, Some(pid_as(20, me.euid)));
        let other = alloc_client(&mut be, f2, Some(pid(30)));
        let ok = |r: &[u8], at| rm_status(r, at) == 0;

        // NV01_DEVICE_0's hClientShare: clientValidate, the security token --
        // the caller's process or its euid.
        let outer = words(&[(0, a), (4, a), (8, 0xde7), (12, 0x80), (32, 56)], 48);
        let dev = |at: usize, c: u32| words(&[(at, c)], 56);
        for (c, want) in [(mine_too, true), (same_uid, true), (other, false)] {
            let r = call(&mut be, f1, ALLOC, &outer, &dev(4, c), Some(me));
            assert_eq!(ok(&r, OS64_STATUS), want, "hClientShare {c:#x}");
        }
        // The token is the caller's: the same call from the other process
        // may name its own client and not mine.
        let r = call(&mut be, f1, ALLOC, &outer, &dev(4, other), Some(pid(30)));
        assert!(ok(&r, OS64_STATUS));
        // hTargetClient, which RM never checks: the calling process's own.
        let r = call(&mut be, f1, ALLOC, &outer, &dev(8, same_uid), Some(me));
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let r = call(&mut be, f1, ALLOC, &outer, &dev(8, mine_too), Some(me));
        assert!(ok(&r, OS64_STATUS));

        // EXEC_REG_OPS, checked by nothing on the CPU side: the calling
        // process's clients only, not another of its uid.
        let regops = words(&[(0, a), (4, 0x2080), (8, 0x2080_0122), (24, 48)], 32);
        for (c, want) in [(mine_too, true), (same_uid, false), (other, false)] {
            seen();
            let r = call(
                &mut be,
                f1,
                CONTROL,
                &regops,
                &words(&[(0, c)], 48),
                Some(me),
            );
            assert_eq!(ok(&r, OS54_STATUS), want, "regops on {c:#x}");
            assert_eq!(reached(0x2a), want);
        }

        // QUERY_CHANNEL_UNIQUE_ID: the two clients' tokens.
        let q = words(&[(0, a), (4, 0x2080), (8, 0x2080_1124), (24, 1540)], 32);
        let list = |c: u32| words(&[(0, c), (1024, 1)], 1540);
        for (c, want) in [(same_uid, true), (other, false)] {
            let r = call(&mut be, f1, CONTROL, &q, &list(c), Some(me));
            assert_eq!(ok(&r, OS54_STATUS), want, "unique id of {c:#x}");
        }

        // GT200_DEBUGGER: RS_ACCESS_DEBUG on the object, from its list --
        // the other process's object only once it grants a DEBUG.
        let dbg_outer = words(&[(0, a), (4, a), (8, 0xdb9), (12, 0x83de), (32, 12)], 48);
        let dbg = words(&[(4, same_uid), (8, 0x3d)], 12);
        let r = call(&mut be, f1, ALLOC, &dbg_outer, &dbg, Some(me));
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let mut grant = share(
            same_uid,
            0x3d,
            RS_SHARE_TYPE_CLIENT,
            RS_SHARE_ACTION_FLAG_COMPOSE,
            a,
        );
        grant[12..16].copy_from_slice(&RS_ACCESS_DEBUG_BIT.to_le_bytes());
        let r = call(&mut be, f2, SHARE, &grant, &[], None);
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(&mut be, f1, ALLOC, &dbg_outer, &dbg, Some(me));
        assert!(ok(&r, OS64_STATUS));
        // ...for that object alone.
        let r = call(
            &mut be,
            f1,
            ALLOC,
            &dbg_outer,
            &words(&[(4, same_uid), (8, 0x3e)], 12),
            Some(me),
        );
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);

        // A control without the caller, from a guest that said it sends one.
        let r = call(&mut be, f1, CONTROL, &regops, &words(&[(0, a)], 48), None);
        assert_eq!(errno(&r), -libc::EINVAL);
    }

    #[test]
    fn without_the_euid_a_token_rule_is_the_process() {
        // GCAP_PROC_ID alone: the process, no euid, and no caller on
        // controls.
        let (mut be, f1, f2) = vm(GCAP_PROC_ID);
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let me = pid(10);
        let a = alloc_client(&mut be, f1, Some(me));
        let mine_too = alloc_client(&mut be, f2, Some(me));
        let same_uid = alloc_client(&mut be, f2, Some(pid_as(20, me.euid)));
        let outer = words(&[(0, a), (4, a), (8, 0xde7), (12, 0x80), (32, 56)], 48);
        let r = call(
            &mut be,
            f1,
            ALLOC,
            &outer,
            &words(&[(4, same_uid)], 56),
            Some(me),
        );
        assert_eq!(rm_status(&r, OS64_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let r = call(
            &mut be,
            f1,
            ALLOC,
            &outer,
            &words(&[(4, mine_too)], 56),
            Some(me),
        );
        assert_eq!(rm_status(&r, OS64_STATUS), 0);
        // A control carries no caller: another client named is refused, the
        // caller's own is fine.
        let regops = words(&[(0, a), (4, 0x2080), (8, 0x2080_0122), (24, 48)], 32);
        let r = call(
            &mut be,
            f1,
            CONTROL,
            &regops,
            &words(&[(0, mine_too)], 48),
            None,
        );
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        let r = call(&mut be, f1, CONTROL, &regops, &words(&[(0, a)], 48), None);
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
    }

    #[test]
    fn a_grant_does_not_outlive_its_object_freed_with_a_parent() {
        let (mut be, f1, f2) = vm(FULL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let b = alloc_client(&mut be, f2, Some(pid(20)));
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        // Memory 0x55 under device 0xde7, shared with b.
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x55, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        // The device is freed, 0x55 with it; a new object made at 0x55 is
        // a's own, and not shared.
        let r = call(
            &mut be,
            f1,
            FREE,
            &words(&[(0, a), (4, a), (8, 0xde7)], 16),
            &[],
            None,
        );
        assert_eq!(errno(&r), 0);
        seen();
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x34));
        // And a revoke on one object holds against a grant on its client.
        let (mut be, f1, f2) = vm(FULL);
        let a2 = alloc_client(&mut be, f1, Some(pid(10)));
        let b2 = alloc_client(&mut be, f2, Some(pid(20)));
        let ctl = words(
            &[
                (0, a2),
                (4, a2),
                (8, CTRL_SET_INHERITED_SHARE_POLICY),
                (24, 12),
            ],
            32,
        );
        let mut p = words(&[(0, b2), (4, 1)], 12);
        p[8..10].copy_from_slice(&RS_SHARE_TYPE_CLIENT.to_le_bytes());
        p[10] = compose;
        let r = call(&mut be, f1, CONTROL, &ctl, &p, Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b2, a2, 0x66), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(
                a2,
                0x66,
                RS_SHARE_TYPE_CLIENT,
                compose | RS_SHARE_ACTION_FLAG_REVOKE,
                b2,
            ),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b2, a2, 0x66), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
    }

    #[test]
    fn channels_of_another_vms_clients_are_not_disabled() {
        let (mut be, f1, _) = vm(FULL);
        // This module's gate alone: the RM allowlist in front of it
        // (rmallow.rs) refuses these calls first, and is tested there.
        be.set_rm_allowlist(crate::rmallow::Mode::Log);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let ctl = words(&[(0, a), (4, 0x2080), (8, 0x2080_110b), (24, 536)], 32);
        let list = |c: u32| words(&[(4, 1), (24, c), (280, 0xc4a)], 536);
        seen();
        let r = call(&mut be, f1, CONTROL, &ctl, &list(HOST), Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a));
        let r = call(&mut be, f1, CONTROL, &ctl, &list(a), Some(pid(10)));
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
        assert!(reached(0x2a));
    }

    #[test]
    fn a_closed_file_takes_its_clients_ownership_and_grants() {
        let (mut be, f1, f2) = vm(FULL);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let b = alloc_client(&mut be, f2, Some(pid(20)));
        let compose = RS_SHARE_ACTION_FLAG_COMPOSE;
        let r = call(
            &mut be,
            f1,
            SHARE,
            &share(a, 0x55, RS_SHARE_TYPE_CLIENT, compose, b),
            &[],
            None,
        );
        assert_eq!(rm_status(&r, OS57_STATUS), 0);
        be.close_handle(f2).unwrap();
        assert_eq!(be.semsurf.owner_of(b), None);
        // A later client RM gives b's number again inherits nothing.
        let r = call(&mut be, f1, DUP, &dup(b, a, 0x55), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
    }
}
