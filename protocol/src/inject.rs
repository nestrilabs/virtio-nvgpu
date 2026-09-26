// SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0-or-later
// protocol/src/inject.rs
//
// The capture-injection socket: a host helper hands the backend dma-bufs a
// guest may then open (ARCHITECTURE.md, "Capture injection"; SECURITY.md
// §18). Nothing here crosses the virtqueue: this is the host-side socket's
// format, kept beside the wire format because the helper that speaks it is
// built elsewhere (the NixOS side) and needs one normative description.
//
// The socket is AF_UNIX SOCK_SEQPACKET: one message per packet, and a packet
// is exactly the size its op says, or the peer is disconnected. Every integer
// is little-endian. The helper sends; the backend answers each request with
// one [`InjReply`] (48 bytes), in order.
//
//   HELLO    16 bytes, first, once. The version must be [`INJ_VERSION`].
//   IMPORT   64 bytes, with exactly `nplanes` descriptors (SCM_RIGHTS), one
//            per plane, in plane order. The same dma-buf may be sent for
//            several planes; every plane must resolve to one GEM object.
//   IMPORT_SYNCOBJ
//            8 bytes, with exactly one descriptor: a DRM syncobj file
//            (drmSyncobjHandleToFD without EXPORT_SYNC_FILE), for the guest
//            to wait on and signal points of by value (explicit sync). Its
//            own id and token, which HOST_OP INJECT_OPEN_SYNCOBJ takes.
//   RELEASE  8 bytes. Stops new opens of the id (a buffer's or a syncobj's);
//            guests that opened it keep their references, and the memory
//            lives until the last is gone.
//
// A request that fails is answered with a negative errno in `status` and
// the connection stays up, except for a malformed packet (wrong size,
// unknown op, a request before HELLO, descriptors on a message that takes
// none), which ends it. Closing the connection releases every id it
// imported.
//
// Only the helper creates ids: no guest message lists, enumerates or makes
// one. A guest opens one by HOST_OP INJECT_OPEN with the id and the 16-byte
// token IMPORT returned (messages.rs, OP_INJECT_OPEN).

/// `InjHello::version`.
pub const INJ_VERSION: u32 = 1;

pub const INJ_OP_HELLO: u32 = 1;
pub const INJ_OP_IMPORT: u32 = 2;
pub const INJ_OP_RELEASE: u32 = 3;
pub const INJ_OP_IMPORT_SYNCOBJ: u32 = 4;

/// Planes one buffer may have.
pub const INJ_MAX_PLANES: usize = 4;

/// `InjImport::flags`: the buffer's rows run bottom to top. Carried to the
/// guest (`InjectInfo::flags`) as metadata; the backend does nothing else
/// with it.
pub const INJ_F_Y_INVERT: u32 = 1 << 0;
/// Every flag an IMPORT may carry; any other bit is refused (EINVAL).
pub const INJ_F_ALL: u32 = INJ_F_Y_INVERT;

pub const INJ_HELLO_SIZE: usize = 16;
pub const INJ_IMPORT_SIZE: usize = 64;
pub const INJ_RELEASE_SIZE: usize = 8;
pub const INJ_IMPORT_SYNCOBJ_SIZE: usize = 8;
pub const INJ_REPLY_SIZE: usize = 48;

/// The largest packet a peer may send; a longer one is truncated by the
/// kernel (MSG_TRUNC) and ends the connection.
pub const INJ_MAX_PACKET: usize = INJ_IMPORT_SIZE;

/// `{op = HELLO, version, flags = 0, reserved = 0}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjHello {
    pub version: u32,
    pub flags: u32,
}

/// `{op = IMPORT, nplanes, width, height, fourcc, flags, modifier,
/// offsets[4], strides[4]}`: offsets 0, 4, 8, 12, 16, 20, 24, 32, 48.
/// Planes past `nplanes` must be zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjImport {
    pub nplanes: u32,
    pub width: u32,
    pub height: u32,
    /// A DRM fourcc (drm_fourcc.h).
    pub fourcc: u32,
    pub flags: u32,
    /// A DRM format modifier; `DRM_FORMAT_MOD_INVALID` is refused.
    pub modifier: u64,
    pub offsets: [u32; INJ_MAX_PLANES],
    pub strides: [u32; INJ_MAX_PLANES],
}

/// `{op = RELEASE, id}`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjRelease {
    pub id: u32,
}

/// `{op = IMPORT_SYNCOBJ, flags = 0}`, with the syncobj file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjImportSyncobj {
    pub flags: u32,
}

/// A request, as [`parse_request`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjRequest {
    Hello(InjHello),
    Import(InjImport),
    Release(InjRelease),
    ImportSyncobj(InjImportSyncobj),
}

/// The one reply shape: `{op, status, id, version, token[16], max_buffers,
/// max_syncobjs, max_bytes}` at 0, 4, 8, 12, 16, 32, 36, 40.
///
/// `op` echoes the request's. `status` is 0 or a negative errno. IMPORT and
/// IMPORT_SYNCOBJ fill `id` and `token`; HELLO fills `version` and the
/// backend's bounds, per VM: `max_buffers` buffer ids and `max_bytes` bytes
/// of them at once, and `max_syncobjs` syncobj ids. Everything else is
/// zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjReply {
    pub op: u32,
    pub status: i32,
    pub id: u32,
    pub version: u32,
    pub token: [u8; 16],
    pub max_buffers: u32,
    pub max_syncobjs: u32,
    pub max_bytes: u64,
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(w)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(w)
}

/// Why a packet is not a request. Each ends the connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjMalformed {
    /// Shorter than an op, or not the size its op has.
    Size,
    /// No such op.
    Op,
    /// A reserved field is not zero.
    Reserved,
}

/// Read one packet. The size must be exactly the op's; reserved words zero.
/// Nothing else is judged here (the backend's `inject.rs` validates the
/// values).
pub fn parse_request(b: &[u8]) -> Result<InjRequest, InjMalformed> {
    if b.len() < 4 {
        return Err(InjMalformed::Size);
    }
    match u32_at(b, 0) {
        INJ_OP_HELLO => {
            if b.len() != INJ_HELLO_SIZE {
                return Err(InjMalformed::Size);
            }
            if u32_at(b, 12) != 0 {
                return Err(InjMalformed::Reserved);
            }
            Ok(InjRequest::Hello(InjHello {
                version: u32_at(b, 4),
                flags: u32_at(b, 8),
            }))
        }
        INJ_OP_IMPORT => {
            if b.len() != INJ_IMPORT_SIZE {
                return Err(InjMalformed::Size);
            }
            let mut offsets = [0u32; INJ_MAX_PLANES];
            let mut strides = [0u32; INJ_MAX_PLANES];
            for i in 0..INJ_MAX_PLANES {
                offsets[i] = u32_at(b, 32 + 4 * i);
                strides[i] = u32_at(b, 48 + 4 * i);
            }
            Ok(InjRequest::Import(InjImport {
                nplanes: u32_at(b, 4),
                width: u32_at(b, 8),
                height: u32_at(b, 12),
                fourcc: u32_at(b, 16),
                flags: u32_at(b, 20),
                modifier: u64_at(b, 24),
                offsets,
                strides,
            }))
        }
        INJ_OP_RELEASE => {
            if b.len() != INJ_RELEASE_SIZE {
                return Err(InjMalformed::Size);
            }
            Ok(InjRequest::Release(InjRelease { id: u32_at(b, 4) }))
        }
        INJ_OP_IMPORT_SYNCOBJ => {
            if b.len() != INJ_IMPORT_SYNCOBJ_SIZE {
                return Err(InjMalformed::Size);
            }
            Ok(InjRequest::ImportSyncobj(InjImportSyncobj {
                flags: u32_at(b, 4),
            }))
        }
        _ => Err(InjMalformed::Op),
    }
}

impl InjHello {
    pub fn to_bytes(&self) -> [u8; INJ_HELLO_SIZE] {
        let mut b = [0u8; INJ_HELLO_SIZE];
        b[0..4].copy_from_slice(&INJ_OP_HELLO.to_le_bytes());
        b[4..8].copy_from_slice(&self.version.to_le_bytes());
        b[8..12].copy_from_slice(&self.flags.to_le_bytes());
        b
    }
}

impl InjImport {
    pub fn to_bytes(&self) -> [u8; INJ_IMPORT_SIZE] {
        let mut b = [0u8; INJ_IMPORT_SIZE];
        b[0..4].copy_from_slice(&INJ_OP_IMPORT.to_le_bytes());
        b[4..8].copy_from_slice(&self.nplanes.to_le_bytes());
        b[8..12].copy_from_slice(&self.width.to_le_bytes());
        b[12..16].copy_from_slice(&self.height.to_le_bytes());
        b[16..20].copy_from_slice(&self.fourcc.to_le_bytes());
        b[20..24].copy_from_slice(&self.flags.to_le_bytes());
        b[24..32].copy_from_slice(&self.modifier.to_le_bytes());
        for i in 0..INJ_MAX_PLANES {
            b[32 + 4 * i..36 + 4 * i].copy_from_slice(&self.offsets[i].to_le_bytes());
            b[48 + 4 * i..52 + 4 * i].copy_from_slice(&self.strides[i].to_le_bytes());
        }
        b
    }
}

impl InjRelease {
    pub fn to_bytes(&self) -> [u8; INJ_RELEASE_SIZE] {
        let mut b = [0u8; INJ_RELEASE_SIZE];
        b[0..4].copy_from_slice(&INJ_OP_RELEASE.to_le_bytes());
        b[4..8].copy_from_slice(&self.id.to_le_bytes());
        b
    }
}

impl InjImportSyncobj {
    pub fn to_bytes(&self) -> [u8; INJ_IMPORT_SYNCOBJ_SIZE] {
        let mut b = [0u8; INJ_IMPORT_SYNCOBJ_SIZE];
        b[0..4].copy_from_slice(&INJ_OP_IMPORT_SYNCOBJ.to_le_bytes());
        b[4..8].copy_from_slice(&self.flags.to_le_bytes());
        b
    }
}

impl InjReply {
    pub fn to_bytes(&self) -> [u8; INJ_REPLY_SIZE] {
        let mut b = [0u8; INJ_REPLY_SIZE];
        b[0..4].copy_from_slice(&self.op.to_le_bytes());
        b[4..8].copy_from_slice(&self.status.to_le_bytes());
        b[8..12].copy_from_slice(&self.id.to_le_bytes());
        b[12..16].copy_from_slice(&self.version.to_le_bytes());
        b[16..32].copy_from_slice(&self.token);
        b[32..36].copy_from_slice(&self.max_buffers.to_le_bytes());
        b[36..40].copy_from_slice(&self.max_syncobjs.to_le_bytes());
        b[40..48].copy_from_slice(&self.max_bytes.to_le_bytes());
        b
    }

    /// For a helper (and the tests): a reply as the backend wrote it.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != INJ_REPLY_SIZE {
            return None;
        }
        let mut token = [0u8; 16];
        token.copy_from_slice(&b[16..32]);
        Some(InjReply {
            op: u32_at(b, 0),
            status: u32_at(b, 4) as i32,
            id: u32_at(b, 8),
            version: u32_at(b, 12),
            token,
            max_buffers: u32_at(b, 32),
            max_syncobjs: u32_at(b, 36),
            max_bytes: u64_at(b, 40),
        })
    }
}

/// What INJECT_OPEN tells the guest about the buffer, after its
/// `HostOpResp` (messages.rs, [`crate::messages::OP_INJECT_OPEN`]):
/// `{width, height, fourcc, nplanes, modifier, offsets[4], strides[4],
/// flags, reserved}` at 0, 4, 8, 12, 16, 24, 40, 56, 60: 64 bytes, the
/// values the helper's IMPORT gave, as the backend checked them.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InjectInfo {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub nplanes: u32,
    pub modifier: u64,
    pub offsets: [u32; INJ_MAX_PLANES],
    pub strides: [u32; INJ_MAX_PLANES],
    pub flags: u32,
    pub reserved: u32,
}

impl InjectInfo {
    pub fn to_bytes(&self) -> [u8; 64] {
        let mut b = [0u8; 64];
        b[0..4].copy_from_slice(&self.width.to_le_bytes());
        b[4..8].copy_from_slice(&self.height.to_le_bytes());
        b[8..12].copy_from_slice(&self.fourcc.to_le_bytes());
        b[12..16].copy_from_slice(&self.nplanes.to_le_bytes());
        b[16..24].copy_from_slice(&self.modifier.to_le_bytes());
        for i in 0..INJ_MAX_PLANES {
            b[24 + 4 * i..28 + 4 * i].copy_from_slice(&self.offsets[i].to_le_bytes());
            b[40 + 4 * i..44 + 4 * i].copy_from_slice(&self.strides[i].to_le_bytes());
        }
        b[56..60].copy_from_slice(&self.flags.to_le_bytes());
        b
    }
}

const _: () = {
    assert!(size_of::<InjectInfo>() == 64);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip_and_sizes_are_exact() {
        let imp = InjImport {
            nplanes: 2,
            width: 2560,
            height: 1440,
            fourcc: 0x3231_564e,
            flags: INJ_F_Y_INVERT,
            modifier: 0x0300_0000_0060_6015,
            offsets: [0, 3_686_400, 0, 0],
            strides: [2560, 2560, 0, 0],
        };
        let b = imp.to_bytes();
        assert_eq!(parse_request(&b), Ok(InjRequest::Import(imp)));
        assert_eq!(parse_request(&b[..63]), Err(InjMalformed::Size));
        let mut long = [0u8; 65];
        long[..64].copy_from_slice(&b);
        assert_eq!(parse_request(&long), Err(InjMalformed::Size));

        let h = InjHello {
            version: INJ_VERSION,
            flags: 0,
        };
        assert_eq!(parse_request(&h.to_bytes()), Ok(InjRequest::Hello(h)));
        let mut hb = h.to_bytes();
        hb[12] = 1;
        assert_eq!(parse_request(&hb), Err(InjMalformed::Reserved));

        let r = InjRelease { id: 7 };
        assert_eq!(parse_request(&r.to_bytes()), Ok(InjRequest::Release(r)));
        let so = InjImportSyncobj { flags: 0 };
        assert_eq!(
            parse_request(&so.to_bytes()),
            Ok(InjRequest::ImportSyncobj(so))
        );
        assert_eq!(parse_request(&so.to_bytes()[..6]), Err(InjMalformed::Size));
        assert_eq!(parse_request(&[9, 0, 0, 0]), Err(InjMalformed::Op));
        assert_eq!(parse_request(&[]), Err(InjMalformed::Size));
        assert_eq!(parse_request(&[1, 0]), Err(InjMalformed::Size));
    }

    #[test]
    fn a_reply_reads_back_as_written() {
        let r = InjReply {
            op: INJ_OP_IMPORT,
            status: -22,
            id: 3,
            version: 0,
            token: [0xa5; 16],
            max_buffers: 32,
            max_syncobjs: 16,
            max_bytes: 1 << 30,
        };
        assert_eq!(InjReply::from_bytes(&r.to_bytes()), Some(r));
        assert_eq!(InjReply::from_bytes(&[0; 47]), None);
    }

    /// The guest reads InjectInfo by these offsets (driver/nvgpu_wire.h).
    #[test]
    fn inject_info_offsets_are_the_drivers() {
        assert_eq!(core::mem::offset_of!(InjectInfo, modifier), 16);
        assert_eq!(core::mem::offset_of!(InjectInfo, offsets), 24);
        assert_eq!(core::mem::offset_of!(InjectInfo, strides), 40);
        assert_eq!(core::mem::offset_of!(InjectInfo, flags), 56);
        let i = InjectInfo {
            width: 1,
            height: 2,
            fourcc: 3,
            nplanes: 1,
            modifier: 0x0102_0304_0506_0708,
            offsets: [9, 0, 0, 0],
            strides: [10, 0, 0, 0],
            flags: 1,
            reserved: 0,
        };
        let b = i.to_bytes();
        assert_eq!(&b[16..24], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&b[24..28], &9u32.to_le_bytes());
        assert_eq!(&b[40..44], &10u32.to_le_bytes());
        assert_eq!(&b[56..60], &1u32.to_le_bytes());
    }
}
