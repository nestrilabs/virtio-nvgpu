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
//! - **A duplicate's source is this VM's, and its guest process's**
//!   ([`DupVerdict`]). NV_ESC_RM_DUP_OBJECT (NVOS55) names two clients: both
//!   must be clients this VM allocated and has not freed. And as RM would if
//!   the guest's processes were the host's, the source client must have been
//!   made by the same guest process as the destination client -- or by the
//!   process making the call (its own object, into a client it holds a file
//!   of), or the source object (or its client, whose policy every object of
//!   it inherits) must carry a CLIENT grant to the destination client for
//!   DUP_OBJECT. Which process made a client, and which makes a call, only
//!   the guest kernel knows: it says, with [`ProcId`] (BCAP_PROC_ID). A guest
//!   that cannot keeps what it had -- its processes share RM objects freely,
//!   noted once per session -- as it did before this was checked; the rules
//!   about other VMs and the host hold for it all the same.
//! - **No other client is named as a second one** ([`alloc_named`],
//!   [`control_named`]). Allocation classes and controls that name a client
//!   besides the caller's -- a device sharing another client's VA space, an
//!   event on another client's object, a debugger or profiler of another
//!   client's context, register operations on another client's channel --
//!   are checked by RM against the caller's security token, which matches
//!   any host process of the backend's uid, or not at all; the client they
//!   name must be none (the caller's own) or this VM's.
//!
//! A refusal is RM's own for a caller without the right,
//! NV_ERR_INSUFFICIENT_PERMISSIONS in the parameters' status with the ioctl
//! itself succeeding, as for the controls in rmctl.rs: the caller sees what
//! it would natively for a share or a duplicate RM will not allow.

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

/// Recorded grants a session may hold. Each is a policy RM also holds; this
/// bounds what the backend keeps, not what RM does.
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
}

/// NV_ESC_RM_DUP_OBJECT's `(hClient, hClientSrc, hObjectSrc)`.
pub fn dup_names(params: &[u8]) -> Option<(u32, u32, u32)> {
    Some((
        rd32(params, 0)?,
        rd32(params, OS55_CLIENT_SRC)?,
        rd32(params, OS55_OBJECT_SRC)?,
    ))
}

/// Allocation classes whose parameters name a client besides the caller's,
/// with where (class parameters, after NVOS64). Zero there is the caller's
/// own client (or none) to RM. Measured on 610.57.04's SDK headers and the
/// same in 595.99.02:
///
/// - 0x0080 NV01_DEVICE_0: `hClientShare` (4), the client whose VA space the
///   device shares, checked by RM with clientValidate (the security token,
///   device.c `_deviceValidateClientShare`); `hTargetClient` (8), vGPU's.
/// - 0x0005 NV01_EVENT, 0x0079 NV01_EVENT_OS_EVENT: `hParentClient` (0), the
///   client of the object the event is on, not checked at all
///   (event_api.c, eventConstruct). 0x0078/0x007e are refused whole
///   (guestptr.rs).
/// - 0x83de GT200_DEBUGGER: `hAppClient` (4), the debugged client; RM wants
///   RS_ACCESS_DEBUG shared, which only a CLIENT grant this VM may make can.
/// - 0xb2cc MAXWELL_PROFILER_DEVICE: `hClientTarget` (0), checked by
///   security token (kern_profiler_v2.c) -- and not at all for a caller at
///   USER_ROOT.
/// - 0xc574 UVM_CHANNEL_RETAINER: `hClient` (0); kernel-privileged in RM.
/// - 0xcb33 NV_CONFIDENTIAL_COMPUTE: `hClient` (0).
pub const ALLOC_CLIENT_FIELDS: &[(u32, &str, &[usize])] = &[
    (0x0080, "NV01_DEVICE_0", &[4, 8]),
    (0x0005, "NV01_EVENT", &[0]),
    (0x0079, "NV01_EVENT_OS_EVENT", &[0]),
    (0x83de, "GT200_DEBUGGER", &[4]),
    (0xb2cc, "MAXWELL_PROFILER_DEVICE", &[0]),
    (0xc574, "UVM_CHANNEL_RETAINER", &[0]),
    (0xcb33, "NV_CONFIDENTIAL_COMPUTE", &[0]),
];

/// NV2080_CTRL_CMD_GPU_{INITIALIZE,PROMOTE,EVICT}_CTX: `hClient` at 4 and
/// `hChanClient` at 12.
const CTX_FIELDS: &[usize] = &[4, 12];

/// Controls whose parameters name a client besides the caller's, with where.
/// Every one with a `NvHandle h*Client*` field in 610.57.04's ctrl headers
/// that a user client reaches (RMCTRL_FLAGS NON_PRIVILEGED or PRIVILEGED
/// in the exported method tables; the kernel-privileged ones RM refuses the
/// backend anyway, and the vGPU host plugin's, diagnostics' and INTERNAL
/// ones are not for it either). RM checks these, when it does, by security
/// token (`_kfifoValidateTargetClient`, the regops path), which a host
/// process of the backend's uid passes.
pub const CONTROL_CLIENT_FIELDS: &[(u32, &str, &[usize])] = &[
    (
        0x0000_0d03,
        "NV0000_CTRL_CMD_CLIENT_GET_ACCESS_RIGHTS",
        &[4],
    ),
    (0x503c_0106, "NV503C_CTRL_CMD_REGISTER_PID", &[0]),
    (0x2080_1205, "NV2080_CTRL_CMD_GR_CTXSW_ZCULL_MODE", &[4]),
    (0x2080_1208, "NV2080_CTRL_CMD_GR_CTXSW_ZCULL_BIND", &[0]),
    (0x2080_1209, "NV2080_CTRL_CMD_GR_CTXSW_PM_BIND", &[0]),
    (0x2080_123a, "NV2080_CTRL_CMD_GR_CTXSW_SETUP_BIND", &[0]),
    (
        0x2080_1211,
        "NV2080_CTRL_CMD_GR_CTXSW_PREEMPTION_BIND",
        &[4],
    ),
    (0x2080_0122, "NV2080_CTRL_CMD_GPU_EXEC_REG_OPS", &[0]),
    (0x2080_01a6, "NV2080_CTRL_CMD_GPU_MIGRATABLE_OPS", &[0]),
    (0x2080_012b, "NV2080_CTRL_CMD_GPU_PROMOTE_CTX", CTX_FIELDS),
    (0x2080_012c, "NV2080_CTRL_CMD_GPU_EVICT_CTX", CTX_FIELDS),
    (
        0x2080_012d,
        "NV2080_CTRL_CMD_GPU_INITIALIZE_CTX",
        CTX_FIELDS,
    ),
    (
        0x2080_1116,
        "NV2080_CTRL_CMD_FIFO_UPDATE_CHANNEL_INFO",
        &[0],
    ),
    (
        0x2080_1123,
        "NV2080_CTRL_CMD_FIFO_GET_CHANNEL_GROUP_UNIQUE_ID_INFO",
        &[0],
    ),
    (0x2080_2502, "NV2080_CTRL_CMD_DMA_INVALIDATE_TLB", &[0]),
];

/// NV5080_CTRL_CMD_DEFERRED_API and _V2: `{hApiHandle, cmd, flags,
/// hClientVA, hDeviceVA, union api_bundle}` with the bundle at 24, holding
/// the parameters of the control `cmd` names, run later at the caller's
/// privilege (deferred_api.c).
const DEFERRED_API: [u32; 2] = [0x5080_0101, 0x5080_0103];
const DEFERRED_CMD: usize = 4;
const DEFERRED_CLIENT_VA: usize = 12;
const DEFERRED_BUNDLE: usize = 24;

/// The client handles an RM_ALLOC's class parameters (`nested`, after NVOS64)
/// name besides the caller's, zeros left out; `Err` names a field the block
/// is too short to hold. No parameters at all is RM's defaults: none named.
pub fn alloc_named(class: u32, nested: &[u8]) -> Result<Vec<(u32, &'static str)>, &'static str> {
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
pub fn control_named(cmd: u32, params: &[u8]) -> Result<Vec<(u32, &'static str)>, &'static str> {
    if DEFERRED_API.contains(&cmd) {
        let mut out = named(
            params,
            &[DEFERRED_CLIENT_VA],
            "NV5080_CTRL_CMD_DEFERRED_API",
        )?;
        let inner = rd32(params, DEFERRED_CMD).ok_or("NV5080_CTRL_CMD_DEFERRED_API")?;
        if let Some(&(_, name, fields)) = CONTROL_CLIENT_FIELDS.iter().find(|(c, _, _)| *c == inner)
        {
            let bundle = params.get(DEFERRED_BUNDLE..).ok_or(name)?;
            out.extend(named(bundle, fields, name)?);
        }
        return Ok(out);
    }
    match CONTROL_CLIENT_FIELDS.iter().find(|(c, _, _)| *c == cmd) {
        Some(&(_, name, fields)) => named(params, fields, name),
        None => Ok(Vec::new()),
    }
}

fn named(
    b: &[u8],
    fields: &[usize],
    name: &'static str,
) -> Result<Vec<(u32, &'static str)>, &'static str> {
    let mut out = Vec::new();
    for &at in fields {
        match rd32(b, at).ok_or(name)? {
            0 => {}
            h => out.push((h, name)),
        }
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
/// and the CLIENT grants RM took.
#[derive(Default)]
pub struct Ownership {
    pub owners: HashMap<u32, ProcId>,
    pub grants: HashMap<(u32, u32), Vec<Grant>>,
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
        self.grants.retain(|_, l| !l.is_empty());
        self.recount();
    }

    /// `(client, object)` was freed: its policy list with it.
    pub fn object_freed(&mut self, client: u32, object: u32) {
        if self.grants.remove(&(client, object)).is_some() {
            self.recount();
        }
    }

    fn recount(&mut self) {
        self.grant_count = self.grants.values().map(Vec::len).sum();
    }

    /// A share RM answered NV_OK for.
    pub fn shared(&mut self, owner: u32, object: u32, p: &Policy) {
        let list = self.grants.entry((owner, object)).or_default();
        apply_share(list, owner, p);
        if list.is_empty() {
            self.grants.remove(&(owner, object));
        }
        self.recount();
    }

    /// Whether a share could add a grant past [`GRANT_CAP`].
    pub fn full_for(&self, owner: u32, p: &Policy) -> bool {
        p.kind == RS_SHARE_TYPE_CLIENT
            && p.target != owner
            && p.action & (RS_SHARE_ACTION_FLAG_REVOKE | RS_SHARE_ACTION_FLAG_REQUIRE) == 0
            && self.grant_count >= GRANT_CAP
    }

    /// Whether a grant on the object or on its client (the policy every
    /// object of it inherits until its own is set) lets `dst` duplicate
    /// `(src, obj)`. Grants on objects between the two (a device, say) are
    /// not followed: the backend does not know an object's parents, so such
    /// a duplicate is refused where RM would allow it.
    fn granted(&self, src: u32, obj: u32, dst: u32) -> bool {
        [(src, obj), (src, src)].iter().any(|k| {
            self.grants.get(k).is_some_and(|l| {
                l.iter()
                    .any(|g| g.target == dst && g.mask & RS_ACCESS_DUP_OBJECT_BIT != 0)
            })
        })
    }

    /// The verdict on a duplicate from `(src, obj)` into client `dst` made by
    /// `caller`. `live` says whether a handle is a client this VM has;
    /// `per_process` whether the guest says which process made each client.
    pub fn dup_verdict(
        &self,
        live: impl Fn(u32) -> bool,
        per_process: bool,
        caller: Option<ProcId>,
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
        if !per_process || src == dst {
            return DupVerdict::Allowed;
        }
        let (Some(s), Some(d)) = (self.owners.get(&src), self.owners.get(&dst)) else {
            // Made before the guest said who makes what (a HELLO without
            // FRESH after v1 calls): kept to no process, as before.
            return DupVerdict::Allowed;
        };
        if s == d || caller.as_ref() == Some(s) || self.granted(src, obj, dst) {
            DupVerdict::Allowed
        } else {
            DupVerdict::OtherProcess
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
    /// maker is its owner, and an RM_DUP_OBJECT.
    pub(crate) fn rm_proc_id(
        &self,
        escape: u32,
        params: &[u8],
        trailer: &[u8],
    ) -> Result<Option<ProcId>, i32> {
        if !self.session.proc_ids {
            return Ok(None);
        }
        let needed = match escape {
            NV_ESC_RM_DUP_OBJECT => true,
            NV_ESC_RM_ALLOC => {
                rd32(params, OS64_CLASS).is_some_and(|c| crate::semsurf::ROOT_CLASSES.contains(&c))
            }
            _ => false,
        };
        let id = trailer.get(..size_of::<ProcId>()).map(|b| ProcId {
            start_ns: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            tgid: u32::from_le_bytes(b[8..12].try_into().unwrap()),
            flags: u32::from_le_bytes(b[12..16].try_into().unwrap()),
        });
        match id {
            Some(id) => Ok(Some(id)),
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
        caller: Option<ProcId>,
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
                ShareVerdict::Forward if self.semsurf.grants_full_for(owner, &p) => {
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
                let per_process = self.session.proc_ids;
                if !per_process && self.session.v2 && !self.session.proc_ids_noted {
                    self.session.proc_ids_noted = true;
                    log::info!(
                        "this guest does not say which of its processes makes each RM call: \
                         its processes may duplicate each other's RM objects, as before"
                    );
                }
                let v = self.semsurf.dup_verdict(per_process, caller, dst, src, obj);
                if v != DupVerdict::Allowed {
                    log::warn!(
                        "RM_DUP_OBJECT of {src:#x}/{obj:#x} into client {dst:#x} refused: {}",
                        match v {
                            DupVerdict::ForeignSource => "the source client is not this VM's",
                            DupVerdict::ForeignDestination => "the client is not this VM's",
                            _ => "another guest process's client, not shared with this one",
                        }
                    );
                    return Err(Refuse::Status(NV_ERR_INSUFFICIENT_PERMISSIONS));
                }
            }
            NV_ESC_RM_ALLOC => {
                let class = rd32(params, OS64_CLASS).unwrap_or(0);
                let nested = params.get(OS64_SIZE..).unwrap_or(&[]);
                self.named_clients_ok(alloc_named(class, nested))
                    .map_err(Refuse::Status)?;
            }
            NV_ESC_RM_CONTROL => {
                let cmd = rd32(params, OS54_CMD).unwrap_or(0);
                let ctl = params.get(OS54_SIZE..).unwrap_or(&[]);
                self.named_clients_ok(control_named(cmd, ctl))
                    .map_err(Refuse::Status)?;
            }
            _ => {}
        }
        Ok(pending)
    }

    fn named_clients_ok(
        &self,
        named: Result<Vec<(u32, &'static str)>, &'static str>,
    ) -> Result<(), u32> {
        let named = named.map_err(|what| {
            log::warn!("{what}: parameters too short to hold the client they name; refused");
            NV_ERR_INSUFFICIENT_PERMISSIONS
        })?;
        for (h, what) in named {
            if !self.semsurf.owns_client(h) {
                log::warn!("{what} names client {h:#x}, which is not this VM's; refused");
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

    fn id(tgid: u32) -> ProcId {
        ProcId {
            start_ns: u64::from(tgid) * 1000,
            tgid,
            flags: 0,
        }
    }

    #[test]
    fn a_duplicate_is_kept_to_the_process_that_made_both_clients() {
        let mut o = Ownership::default();
        o.owners.insert(OWNER, id(10));
        o.owners.insert(PEER, id(20));
        let live = vm;
        use DupVerdict::*;
        // Across processes: refused; within one: allowed.
        assert_eq!(
            o.dup_verdict(live, true, Some(id(20)), PEER, OWNER, 0x55),
            OtherProcess
        );
        assert_eq!(
            o.dup_verdict(live, true, Some(id(10)), OWNER, OWNER, 0x55),
            Allowed
        );
        // The caller's own object into a client it holds a file of.
        assert_eq!(
            o.dup_verdict(live, true, Some(id(10)), PEER, OWNER, 0x55),
            Allowed
        );
        // Not this VM's at either end.
        assert_eq!(
            o.dup_verdict(live, true, Some(id(10)), PEER, HOST, 1),
            ForeignSource
        );
        assert_eq!(
            o.dup_verdict(live, true, Some(id(10)), HOST, OWNER, 1),
            ForeignDestination
        );
        assert_eq!(
            o.dup_verdict(live, false, None, PEER, HOST, 1),
            ForeignSource
        );
        // Without process ids: as before, inside the VM.
        assert_eq!(o.dup_verdict(live, false, None, PEER, OWNER, 0x55), Allowed);
        // A CLIENT grant on the object, or on its client, opens it to PEER.
        let grant = policy(RS_SHARE_TYPE_CLIENT, RS_SHARE_ACTION_FLAG_COMPOSE, PEER);
        o.shared(OWNER, 0x55, &grant);
        assert_eq!(
            o.dup_verdict(live, true, Some(id(20)), PEER, OWNER, 0x55),
            Allowed
        );
        assert_eq!(
            o.dup_verdict(live, true, Some(id(20)), PEER, OWNER, 0x56),
            OtherProcess
        );
        o.shared(OWNER, OWNER, &grant);
        assert_eq!(
            o.dup_verdict(live, true, Some(id(20)), PEER, OWNER, 0x56),
            Allowed
        );
        // A grant without DUP_OBJECT does not.
        let mut o2 = Ownership::default();
        o2.owners = o.owners.clone();
        o2.shared(
            OWNER,
            0x55,
            &Policy {
                mask: 1 << 2,
                ..grant
            },
        );
        assert_eq!(
            o2.dup_verdict(live, true, Some(id(20)), PEER, OWNER, 0x55),
            OtherProcess
        );
        // Freeing the object, or either client, takes its grants.
        o.object_freed(OWNER, OWNER);
        o.object_freed(OWNER, 0x55);
        assert_eq!(o.grant_count, 0);
        o.shared(OWNER, 0x55, &grant);
        o.forget_clients(&[PEER]);
        assert!(o.grants.is_empty());
        assert!(!o.owners.contains_key(&PEER));
    }

    #[test]
    fn second_clients_are_found_in_class_and_control_parameters() {
        let mut dev = vec![0u8; 56];
        assert_eq!(alloc_named(0x80, &dev), Ok(vec![]));
        dev[4..8].copy_from_slice(&HOST.to_le_bytes());
        assert_eq!(alloc_named(0x80, &dev), Ok(vec![(HOST, "NV01_DEVICE_0")]));
        // No parameters: RM's defaults. Too few: refused.
        assert_eq!(alloc_named(0x80, &[]), Ok(vec![]));
        assert!(alloc_named(0x83de, &[0u8; 6]).is_err());
        // A class with no second client names none.
        assert_eq!(alloc_named(0x90f1, &dev), Ok(vec![]));

        let mut regops = vec![0u8; 48];
        regops[0..4].copy_from_slice(&HOST.to_le_bytes());
        assert_eq!(
            control_named(0x2080_0122, &regops),
            Ok(vec![(HOST, "NV2080_CTRL_CMD_GPU_EXEC_REG_OPS")])
        );
        // DEFERRED_API: its own VA client and the bundled control's.
        let mut d = vec![0u8; 584];
        d[4..8].copy_from_slice(&0x2080_012bu32.to_le_bytes());
        d[12..16].copy_from_slice(&OWNER.to_le_bytes());
        d[24 + 12..24 + 16].copy_from_slice(&HOST.to_le_bytes());
        let n = control_named(0x5080_0101, &d).unwrap();
        assert_eq!(n.iter().map(|x| x.0).collect::<Vec<_>>(), [OWNER, HOST]);
        assert_eq!(control_named(0x2080_0101, &d), Ok(vec![]));
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
            assert!(fields.iter().all(|&f| f + 4 <= size.1), "{name}");
        }
        let mut seen = std::collections::HashSet::new();
        for (c, name, _) in CONTROL_CLIENT_FIELDS.iter().chain(ALLOC_CLIENT_FIELDS) {
            assert!(seen.insert(*c), "{name} twice");
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
        BCAP_PROC_ID, DeviceKind, GCAP_PROC_ID, HELLO_F_FRESH, HelloReq, MsgType, PROTO_V2,
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
    unsafe fn fake_rm(_: RawFd, request: u64, arg: *mut u8) -> i32 {
        let request = request as u32;
        // SAFETY: the HostIoctl contract, `arg` holds _IOC_SIZE bytes.
        let a = unsafe { std::slice::from_raw_parts_mut(arg, hostfd::ioc_size(request)) };
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

    /// A session, v2 with or without process ids, and two control files.
    fn vm(proc_ids: bool) -> (NvidiaBackend, u32, u32) {
        seen();
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_rm);
        let hello = HelloReq {
            proto: PROTO_V2,
            flags: HELLO_F_FRESH,
            guest_caps: if proc_ids { GCAP_PROC_ID } else { 0 },
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
            proc_ids,
            "offered only when asked for"
        );
        let null = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
        let a = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        let b = be.adopt_for_test(null(), HandleKind::Dev(DeviceKind::Ctl));
        (be, a, b)
    }

    fn pid(tgid: u32) -> ProcId {
        ProcId {
            start_ns: 1_000_000 + u64::from(tgid),
            tgid,
            flags: 0,
        }
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
            req.extend_from_slice(&p.flags.to_le_bytes());
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
        let (mut be, f1, f2) = vm(true);
        let a = alloc_client(&mut be, f1, Some(pid(10)));
        let b = alloc_client(&mut be, f2, Some(pid(10)));
        assert_eq!(be.semsurf.owner_of(a), Some(pid(10)));
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
        let (mut be, f1, f2) = vm(true);
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
        // The child pushing its own object into the parent's client (a file
        // the parent passed it): its own to give.
        let r = call(
            &mut be,
            f2,
            DUP,
            &dup(parent, child, 0x55),
            &[],
            Some(pid(20)),
        );
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
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
            let (mut be, f1, _) = vm(proc_ids);
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
    fn an_old_guest_keeps_duplicating_between_its_processes() {
        // No GCAP_PROC_ID: no process ids, and none needed.
        let (mut be, f1, f2) = vm(false);
        let a = alloc_client(&mut be, f1, None);
        let b = alloc_client(&mut be, f2, None);
        assert_eq!(be.semsurf.owner_of(a), None);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x55), &[], None);
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        assert!(reached(0x34));
    }

    #[test]
    fn a_guest_that_said_it_would_name_the_caller_must() {
        let (mut be, f1, _) = vm(true);
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
        let (mut be, f1, _) = vm(true);
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
        let r = call(&mut be, f1, CONTROL, &ctl, &p, None);
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        ctl[8..12].copy_from_slice(&CTRL_SET_INHERITED_SHARE_POLICY.to_le_bytes());
        let mut p = vec![0u8; 12];
        p[8..10].copy_from_slice(&RS_SHARE_TYPE_OS_SECURITY_TOKEN.to_le_bytes());
        let r = call(&mut be, f1, CONTROL, &ctl, &p, None);
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a));
    }

    #[test]
    fn a_client_grant_opens_an_object_to_another_process_until_revoked() {
        let (mut be, f1, f2) = vm(true);
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
        // On the client (SET_INHERITED_SHARE_POLICY), every object of it.
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
        let r = call(&mut be, f1, CONTROL, &ctl, &p, None);
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
        let r = call(&mut be, f2, DUP, &dup(b, a, 0x77), &[], Some(pid(20)));
        assert_eq!(rm_status(&r, OS55_STATUS), 0);
        // A revoke without COMPOSE empties the client's list.
        p[10] = RS_SHARE_ACTION_FLAG_REVOKE;
        let r = call(&mut be, f1, CONTROL, &ctl, &p, None);
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
        let (mut be, f1, _) = vm(true);
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
        let r = call(&mut be, f1, CONTROL, &ctl, &words(&[(0, HOST)], 48), None);
        assert_eq!(rm_status(&r, OS54_STATUS), NV_ERR_INSUFFICIENT_PERMISSIONS);
        assert!(!reached(0x2a));
        let r = call(&mut be, f1, CONTROL, &ctl, &words(&[(0, a)], 48), None);
        assert_eq!(rm_status(&r, OS54_STATUS), 0);
    }

    #[test]
    fn a_closed_file_takes_its_clients_ownership_and_grants() {
        let (mut be, f1, f2) = vm(true);
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
