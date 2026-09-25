// crates/protocol/src/messages.rs
//
// Wire message types for virtio-gpu-nv.
//
// Descriptor chain layout (one chain per operation):
//
//   [readable: MsgHeader + payload] → [writable: MsgHeader + payload]
//
// The guest driver:
//   1. Fills in the readable buffer (header + request payload).
//   2. Adds the writable buffer to the chain.
//   3. Posts the chain to the virtqueue and waits for a used-ring notification.
//   4. Reads the writable buffer (header + response payload).
//
// The backend:
//   1. Receives the readable buffer.
//   2. Dispatches on `MsgHeader::msg_type`.
//   3. Writes the response into the writable buffer.
//   4. Pushes the chain back to the used ring.
//
// Every layout here mirrors `driver/nvgpu_wire.h`. That file is the wire
// format: it is the half compiled into a guest kernel, and it cannot negotiate.
//
// These definitions previously described a different protocol entirely -- a
// header of `{msg_type, pad, cookie}` against the driver's
// `{msg_type, handle, status, padding}`, an open request of `{kind, index}`
// against the driver's flat `device_type`, and no message at all for four of
// the seven the driver sends. Both halves compiled, and a guest reached the
// point of creating its device nodes before anything went wrong. Nothing checks
// this agreement except the tests here and a guest that fails oddly, so treat a
// change on either side as a change to both.

// ---------------------------------------------------------------------------
// Message type discriminants
// ---------------------------------------------------------------------------

/// Identifies the kind of message in a `MsgHeader`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsgType {
    /// Guest → host: open a `/dev/nvidia*` device.
    Open = 1,
    /// Guest → host: close a previously opened handle.
    Close = 2,
    /// Guest → host: forward a raw ioctl.
    Ioctl = 3,
    /// Guest → host: map device memory into the shared window.
    Mmap = 4,
    /// Guest → host: release a mapping made by `Mmap`.
    Munmap = 5,
    /// Guest → host: the contents of the host's `/proc/driver/nvidia` tree,
    /// which the guest republishes so its own userspace can read them.
    GetProcFiles = 6,
    /// Guest → host: the same for the sysfs attributes the userspace driver
    /// looks for.
    GetSysFiles = 7,
    /// **Host → guest**, on the event queue: the descriptor named by
    /// `MsgHeader::handle` has something to report.
    ///
    /// The only message that travels this way. NVIDIA's user-mode driver waits
    /// for the GPU by polling the descriptor an RM event is delivered on; the
    /// host driver takes the interrupt and makes *its* descriptor readable, and
    /// this carries that edge across so the guest's can do the same. Without
    /// it a guest cannot wait at all: a `file_operations` with no `.poll` is
    /// reported ready by the VFS every time it is asked, and a driver that
    /// meant to sleep spins instead.
    ///
    /// No payload. The handle in the header is the whole message.
    EventReady = 8,
    /// Guest → host: start protocol v2. See [`HelloReq`].
    Hello = 9,
    /// Guest → host: a schema-driven vectored ioctl. See [`Ioctl2Req`].
    Ioctl2 = 10,
    /// Guest → host: read the host's `CLOCK_MONOTONIC`, for timestamp translation.
    TimeSync = 11,
    /// **Host → guest**, on the event queue: a batch of [`EvRec`] records.
    EventData = 12,
    /// Guest → host: start reporting a handle's readiness. See [`WatchReq`].
    Watch = 13,
    /// Guest → host: stop reporting it.
    Unwatch = 14,
    /// Guest → host: a helper operation on host descriptors. See [`HostOpReq`].
    HostOp = 15,
    /// Guest → host: bytes and descriptors for a Wayland channel.
    WlSend = 16,
    /// Guest → host: take what a Wayland channel has pending.
    WlRecv = 17,
}

impl MsgType {
    /// Decode a wire discriminant.
    pub fn from_u32(v: u32) -> Option<Self> {
        Some(match v {
            1 => Self::Open,
            2 => Self::Close,
            3 => Self::Ioctl,
            4 => Self::Mmap,
            5 => Self::Munmap,
            6 => Self::GetProcFiles,
            7 => Self::GetSysFiles,
            8 => Self::EventReady,
            9 => Self::Hello,
            10 => Self::Ioctl2,
            11 => Self::TimeSync,
            12 => Self::EventData,
            13 => Self::Watch,
            14 => Self::Unwatch,
            15 => Self::HostOp,
            16 => Self::WlSend,
            17 => Self::WlRecv,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Common header, used for both directions
// ---------------------------------------------------------------------------

/// Every message starts with this header, in both directions.
///
/// One type rather than a request and a response type, because the driver uses
/// one: `struct nvgpu_msg_hdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsgHeader {
    /// One of the `MsgType` values.
    pub msg_type: u32,
    /// On a request, the handle to act on. On an `Open` response, the handle
    /// the guest should use from then on.
    pub handle: u32,
    /// Result, and **signed**: zero on success, negative errno on failure. The
    /// driver tests `(s32)status < 0`, so writing an unsigned error code here
    /// reads as success.
    pub status: i32,
    /// Was padding until protocol v2. A v2 guest puts a non-zero id in every
    /// request and the backend echoes it; zero means "none" (a v1 guest).
    pub req_id: u32,
}

impl MsgHeader {
    /// A success response carrying `handle`.
    pub fn ok(msg_type: MsgType, handle: u32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle,
            status: 0,
            req_id: 0,
        }
    }

    /// A failure response. `errno` is given as a positive number and stored
    /// negated, which is the one direction that is easy to get wrong.
    pub fn err(msg_type: MsgType, errno: i32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle: 0,
            status: -errno.abs(),
            req_id: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Device identification
// ---------------------------------------------------------------------------

/// `/dev/nvidiactl`.
pub const DEV_CTL: u32 = 255;
/// `/dev/nvidia-uvm`.
pub const DEV_UVM: u32 = 256;
/// `/dev/nvidia-uvm-tools`.
pub const DEV_UVM_TOOLS: u32 = 257;
/// `/dev/nvidia-modeset`.
pub const DEV_MODESET: u32 = 258;
/// A Wayland channel to the host compositor (protocol v2).
pub const DEV_WAYLAND: u32 = 259;
/// Render nodes start here.
pub const DEV_DRI_BASE: u32 = 512;
/// Card (primary) nodes start here; offered only in compositor-VM mode.
///
/// Decoded before the render range, so an old backend reads 1024 as render
/// node 512, fails the lookup and answers ENODEV rather than opening something.
pub const DEV_DRI_CARD_BASE: u32 = 1024;
/// Highest GPU index expressible before the control device's value.
pub const MAX_GPU_INDEX: u32 = 254;

/// Which device an `Open` refers to.
///
/// The wire encoding is one flat `u32`, not a kind and an index: a GPU is its
/// own minor number, and the singleton devices take values above every possible
/// minor. Decoding is therefore a range check, not a table lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    /// `/dev/nvidiaN`, where N is the guest's minor number.
    Gpu(u32),
    Ctl,
    Uvm,
    UvmTools,
    Modeset,
    /// A DRM render node, by its index in the list the device reported.
    Dri(u32),
    /// A DRM card node, by its index in GET_SYS_FILES section 3.
    DriCard(u32),
    /// A channel to the host Wayland compositor.
    Wayland,
}

impl DeviceKind {
    /// Decode the `device_type` field of an `OpenReq`.
    ///
    /// Returns `None` for anything unrecognised.
    ///
    /// Render nodes are openable, and have to be: NVIDIA's Vulkan and EGL
    /// userspace enumerates the GPU through the DRM render node rather than
    /// through `/dev/nvidia*`, which carry compute. Refusing them here is what
    /// a guest sees as a Vulkan loader that finds a driver, loads it, and is
    /// then told there are none.
    pub fn from_device_type(v: u32) -> Option<Self> {
        Some(match v {
            0..=MAX_GPU_INDEX => Self::Gpu(v),
            DEV_CTL => Self::Ctl,
            DEV_UVM => Self::Uvm,
            DEV_UVM_TOOLS => Self::UvmTools,
            DEV_MODESET => Self::Modeset,
            DEV_WAYLAND => Self::Wayland,
            _ if v >= DEV_DRI_CARD_BASE => Self::DriCard(v - DEV_DRI_CARD_BASE),
            _ if v >= DEV_DRI_BASE => Self::Dri(v - DEV_DRI_BASE),
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// OPEN
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Open`, following a `MsgHeader`.
///
/// The response is a bare `MsgHeader`: the new handle travels in
/// `MsgHeader::handle`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenReq {
    /// See [`DeviceKind::from_device_type`].
    pub device_type: u32,
    /// The guest's `open(2)` flags.
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// IOCTL
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Ioctl`, following a `MsgHeader`.
///
/// Layout: `MsgHeader` | `IoctlReq` | `data_len` bytes | `nested_len` bytes |
/// `deep_len` bytes.
///
/// The nested block is data an ioctl parameter points at. The guest cannot pass
/// a pointer that means anything on the host, so it sends the pointed-to bytes
/// alongside and says where in the top-level struct the pointer sits.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlReq {
    /// The ioctl request number.
    pub cmd: u32,
    /// Bytes of top-level parameter struct following this header.
    pub data_len: u32,
    /// Where the nested block begins, measured from the start of the payload
    /// -- so in practice always equal to `data_len`, since the driver lays the
    /// nested block immediately after the top-level struct
    /// (`req->nested_offset = cpu_to_le32(sizeof(params))`). Zero when there is
    /// no nested block. It is *not* the offset of the pointer field being
    /// replaced, which is what the name suggests.
    pub nested_offset: u32,
    /// Bytes of nested data following the top-level struct.
    pub nested_len: u32,
    /// Where, inside the nested block, a further pointer sits, when the nested
    /// block carries one. Only meaningful when `deep_len` is non-zero.
    pub deep_ptr_offset: u32,
    /// Bytes of a second-level block following the nested block: what the
    /// pointer at `deep_ptr_offset` points at in the guest.
    ///
    /// Some parameter blocks hold a pointer of their own. The guest cannot
    /// send an address that means anything here, so it sends those bytes too
    /// and the backend gives them a host address before the call. Zero when
    /// the nested block carries no pointer.
    pub deep_len: u32,
}

/// Response payload for `MsgType::Ioctl`, following a `MsgHeader`.
///
/// Layout: `MsgHeader` | `IoctlResp` | `data_len` bytes | `nested_len` bytes |
/// `deep_len` bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoctlResp {
    pub data_len: u32,
    pub nested_len: u32,
    /// Bytes of second-level block following the nested block, for the guest
    /// to copy back to where its own pointer points.
    pub deep_len: u32,
}

// ---------------------------------------------------------------------------
// MMAP / MUNMAP
// ---------------------------------------------------------------------------

/// Request payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapReq {
    pub size: u64,
    pub offset: u64,
    pub prot: u32,
    pub padding: u32,
}

/// Response payload for `MsgType::Mmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MmapResp {
    /// Where the guest should map from, in its own physical address space.
    pub guest_phys_addr: u64,
    pub size: u64,
    /// Identifier the guest passes back to `Munmap`.
    pub mapping_id: u32,
    pub padding: u32,
}

/// Request payload for `MsgType::Munmap`, following a `MsgHeader`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MunmapReq {
    pub mapping_id: u32,
    pub padding: u32,
}

// ---------------------------------------------------------------------------
// GET_PROC_FILES / GET_SYS_FILES
// ---------------------------------------------------------------------------

/// One file in the response to `GetProcFiles` or `GetSysFiles`.
///
/// The response is a bare stream of these -- **no `MsgHeader`** -- each
/// followed by `path_len` bytes of path and `content_len` bytes of content,
/// terminated by an entry whose `path_len` is zero. The driver reads from the
/// first byte of the response buffer, so prefixing a header shifts everything
/// and it decodes the header as a length.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FileEntry {
    /// Zero marks the end of the stream.
    pub path_len: u32,
    pub content_len: u32,
}

// ---------------------------------------------------------------------------
// Size assertions (compile-time, no_std compatible)
// ---------------------------------------------------------------------------
//
// These catch accidental padding changes that would break the C header.

const _: () = {
    assert!(size_of::<MsgHeader>() == 16);
    assert!(size_of::<OpenReq>() == 8);
    assert!(size_of::<IoctlReq>() == 24);
    assert!(size_of::<IoctlResp>() == 12);
    assert!(size_of::<MmapReq>() == 24);
    assert!(size_of::<MmapResp>() == 24);
    assert!(size_of::<MunmapReq>() == 8);
    assert!(size_of::<FileEntry>() == 8);
};

// ---------------------------------------------------------------------------
// Protocol v2
// ---------------------------------------------------------------------------
//
// Negotiated at run time by HELLO, not by a feature bit: the VMM may not pass
// device feature bits through, and an old backend answers an unknown message
// with -EPROTO, which is all the guest needs to stay on v1. Mirrors the v2
// section of driver/nvgpu_wire.h, field for field.

/// The protocol version HELLO carries.
pub const PROTO_V2: u32 = 2;

/// `HelloReq::flags`: a new guest driver instance. The session is reset.
pub const HELLO_F_FRESH: u32 = 1 << 0;

/// `HelloResp::backend_caps` bits.
pub const BCAP_KMS_CARD: u32 = 1 << 0;
pub const BCAP_WAYLAND: u32 = 1 << 1;
pub const BCAP_FENCES: u32 = 1 << 2;
pub const BCAP_NVKMS_TABLE: u32 = 1 << 3;
pub const BCAP_WL_EXPORT: u32 = 1 << 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HelloReq {
    pub proto: u32,
    pub flags: u32,
    pub guest_caps: u32,
    pub reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HelloResp {
    pub proto: u32,
    pub backend_caps: u32,
    pub max_req: u32,
    pub max_resp: u32,
    pub num_cards: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeSyncResp {
    /// Host `CLOCK_MONOTONIC`, stamped just before the chain is handed back.
    pub host_mono_ns: u64,
}

/// Most buffers, and most fd/GEM/dyn records, one IOCTL2 may carry.
pub const I2_MAX_BUFS: u32 = 256;
pub const I2_MAX_RECS: u32 = 256;

/// `Ioctl2FdIn::flags`: close the handle once the host call has used it.
pub const I2_FD_CONSUME: u32 = 1 << 0;

/// `Ioctl2Dyn::kind`: an ATOMIC OUT_FENCE_PTR, i.e. a 4-byte OUT buffer the
/// host writes a sync_file descriptor into.
pub const I2_DYN_OUT_FENCE: u32 = 1;

/// Request payload of `MsgType::Ioctl2`.
///
/// Followed by `u32 buf_len[nbuf]`, `Ioctl2FdIn[nfd]`, `Ioctl2GemIn[ngem]`,
/// `Ioctl2Dyn[ndyn]`, then `data_len` bytes: the IN bytes of every IN/INOUT
/// buffer in buffer order, each padded to 8. Buffer 0 is the ioctl argument;
/// the rest follow the schema's canonical traversal. The backend recomputes
/// all of it from its own schema and refuses a request that disagrees.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Ioctl2Req {
    pub cmd: u32,
    pub flags: u32,
    pub nbuf: u32,
    pub nfd: u32,
    pub ngem: u32,
    pub ndyn: u32,
    pub data_len: u32,
    /// Render handle of the calling guest file: GEM handles the call creates
    /// are re-homed into it (host KMS files never own guest objects), and a
    /// GEM_IN owned by it needs no re-homing. Equal to the header's handle for
    /// a call made on a render handle.
    pub render: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ioctl2FdIn {
    pub buf: u32,
    pub off: u32,
    pub handle: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ioctl2GemIn {
    pub buf: u32,
    pub off: u32,
    /// Backend handle of the file the host GEM handle lives in.
    pub owner: u32,
    pub gem: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ioctl2Dyn {
    pub kind: u32,
    pub buf: u32,
    pub off: u32,
    pub len: u32,
}

/// Response payload of `MsgType::Ioctl2`.
///
/// Followed by `data_len` bytes (the OUT bytes of every OUT/INOUT buffer, full
/// length, buffer order, padded to 8), `Ioctl2FdOut[nfd]`, `Ioctl2GemOut[ngem]`.
/// OUT buffers come back even when the host ioctl failed, as DRM and NVKMS both
/// copy back on error.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Ioctl2Resp {
    /// Host ioctl result: 0 or a negative errno.
    pub ret: i32,
    pub nbuf: u32,
    pub nfd: u32,
    pub ngem: u32,
    pub data_len: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ioctl2FdOut {
    pub buf: u32,
    pub off: u32,
    pub handle: u32,
    /// One of the `HK_*` kinds.
    pub kind: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ioctl2GemOut {
    pub buf: u32,
    pub off: u32,
    /// Host GEM handle, valid in the calling file's render handle.
    pub gem: u32,
    pub reserved: u32,
    pub size: u64,
}

/// Handle kinds, as the backend classifies a host descriptor.
pub const HK_DEV: u32 = 1;
pub const HK_DRI_RENDER: u32 = 2;
pub const HK_DRM_CARD: u32 = 3;
pub const HK_DRM_LEASE: u32 = 4;
pub const HK_SYNC_FILE: u32 = 5;
pub const HK_SYNCOBJ: u32 = 6;
pub const HK_DMABUF: u32 = 7;
pub const HK_EVENTFD: u32 = 8;
pub const HK_MEMFD: u32 = 9;
pub const HK_WAYLAND: u32 = 10;
pub const HK_OTHER: u32 = 11;

/// `WatchReq::flags`.
pub const W_ONESHOT: u32 = 1 << 0;
pub const W_FENCE: u32 = 1 << 1;
pub const W_DRM: u32 = 1 << 2;
pub const W_READY: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WatchReq {
    pub handle: u32,
    pub flags: u32,
    pub cookie: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct UnwatchReq {
    pub handle: u32,
    pub pad: u32,
}

/// HOST_OP operations.
pub const OP_PRIME_EXPORT: u32 = 1;
/// `(render file, dmabuf) -> (gem, size, type)`: `type` is the imported
/// object's GEM_IDENTIFY_OBJECT answer (0 NVKMS, 1 DMABUF, 2 USERMEMORY). A
/// guest that predates it reads the first two; a backend that predates it
/// sends two, which the guest reads as NVKMS.
pub const OP_DMABUF_IMPORT: u32 = 2;
pub const OP_SYNC_MERGE: u32 = 3;
pub const OP_NEW_EVENTFD: u32 = 4;
pub const OP_FD_KIND: u32 = 5;
pub const OP_SIGNALED_SYNC_FILE: u32 = 6;
pub const OP_OPEN_KMS: u32 = 7;
pub const OP_DROP_IF_MASTER: u32 = 8;
pub const OP_CLOSE_MANY: u32 = 9;
/// (render, syncobj, point, flags, cookie) -> (reporting cookie, joined):
/// a shared, capped SYNCOBJ_EVENTFD registration reported as EV_READY
/// (device/src/fence.rs). -EAGAIN over the per-VM cap.
pub const OP_SYNCOBJ_WATCH: u32 = 10;

pub const OP_MAX_ARGS: usize = 6;
pub const OP_MAX_RES: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HostOpReq {
    pub op: u32,
    pub nargs: u32,
    pub args: [u64; OP_MAX_ARGS],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HostOpResp {
    pub nres: u32,
    pub pad: u32,
    pub res: [u64; OP_MAX_RES],
}

/// EVENT_DATA record kinds.
pub const EV_DRM: u32 = 1;
pub const EV_FENCE: u32 = 2;
pub const EV_READY: u32 = 3;
pub const EV_HOTPLUG: u32 = 4;

/// Size of each buffer a v2 guest posts on the event queue.
pub const EVENT_BUF_SIZE: usize = 8192;

/// One EVENT_DATA record: this header, then `len` bytes, padded to 8.
///
/// The message itself is a `MsgHeader` whose `req_id` carries the payload
/// length. A record never splits a `struct drm_event`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EvRec {
    pub kind: u32,
    pub len: u32,
    pub cookie: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EvFence {
    /// 1 signalled, negative on error.
    pub status: i32,
    pub pad: u32,
}

pub const EV_HOTPLUG_F_HOTPLUG: u32 = 1 << 0;
pub const EV_HOTPLUG_F_LEASE: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct EvHotplug {
    pub flags: u32,
    pub pad: u32,
}

/// GET_SYS_FILES section 3: one card node, followed by its name.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CardRecord {
    pub name_len: u32,
    pub major: u32,
    pub minor: u32,
    pub render_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WlRecvReq {
    pub max_bytes: u32,
    pub max_desc: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WlSendResp {
    pub accepted: u32,
    pub backlog: u32,
}

const _: () = {
    assert!(size_of::<HelloReq>() == 16);
    assert!(size_of::<HelloResp>() == 32);
    assert!(size_of::<TimeSyncResp>() == 8);
    assert!(size_of::<Ioctl2Req>() == 32);
    assert!(size_of::<Ioctl2FdIn>() == 16);
    assert!(size_of::<Ioctl2GemIn>() == 16);
    assert!(size_of::<Ioctl2Dyn>() == 16);
    assert!(size_of::<Ioctl2Resp>() == 32);
    assert!(size_of::<Ioctl2FdOut>() == 16);
    assert!(size_of::<Ioctl2GemOut>() == 24);
    assert!(size_of::<WatchReq>() == 16);
    assert!(size_of::<UnwatchReq>() == 8);
    assert!(size_of::<HostOpReq>() == 56);
    assert!(size_of::<HostOpResp>() == 40);
    assert!(size_of::<EvRec>() == 16);
    assert!(size_of::<EvFence>() == 8);
    assert!(size_of::<EvHotplug>() == 8);
    assert!(size_of::<CardRecord>() == 16);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_types_decode_the_way_the_driver_encodes_them() {
        assert_eq!(DeviceKind::from_device_type(0), Some(DeviceKind::Gpu(0)));
        assert_eq!(DeviceKind::from_device_type(3), Some(DeviceKind::Gpu(3)));
        assert_eq!(DeviceKind::from_device_type(255), Some(DeviceKind::Ctl));
        assert_eq!(DeviceKind::from_device_type(256), Some(DeviceKind::Uvm));
        assert_eq!(DeviceKind::from_device_type(257), Some(DeviceKind::UvmTools));
        assert_eq!(DeviceKind::from_device_type(258), Some(DeviceKind::Modeset));
    }

    /// A render node is how NVIDIA's Vulkan userspace finds the GPU, so it is
    /// addressable; the gap between the singletons and the render nodes is not.
    #[test]
    fn render_nodes_are_addressable_and_nonsense_is_not() {
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE),
            Some(DeviceKind::Dri(0))
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_BASE + 3),
            Some(DeviceKind::Dri(3))
        );
        assert_eq!(DeviceKind::from_device_type(300), None);
        assert_eq!(DeviceKind::from_device_type(DEV_WAYLAND), Some(DeviceKind::Wayland));
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_CARD_BASE + 1),
            Some(DeviceKind::DriCard(1))
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_CARD_BASE - 1),
            Some(DeviceKind::Dri(DEV_DRI_CARD_BASE - 1 - DEV_DRI_BASE))
        );
    }

    /// The driver tests `(s32)status < 0`. An unsigned error code stored here
    /// reads back as success and the guest proceeds on a failed call.
    #[test]
    fn an_error_status_is_negative() {
        let h = MsgHeader::err(MsgType::Open, 2);
        assert_eq!(h.status, -2);
        assert!(h.status < 0);
        assert_eq!(MsgHeader::err(MsgType::Open, -2).status, -2);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).status, 0);
        assert_eq!(MsgHeader::ok(MsgType::Open, 7).handle, 7);
    }

    #[test]
    fn message_types_round_trip() {
        for t in [
            MsgType::Open,
            MsgType::Close,
            MsgType::Ioctl,
            MsgType::Mmap,
            MsgType::Munmap,
            MsgType::GetProcFiles,
            MsgType::GetSysFiles,
            MsgType::EventReady,
            MsgType::Hello,
            MsgType::Ioctl2,
            MsgType::TimeSync,
            MsgType::EventData,
            MsgType::Watch,
            MsgType::Unwatch,
            MsgType::HostOp,
            MsgType::WlSend,
            MsgType::WlRecv,
        ] {
            assert_eq!(MsgType::from_u32(t as u32), Some(t));
        }
        assert_eq!(MsgType::from_u32(0), None);
        assert_eq!(MsgType::from_u32(18), None);
    }
}
