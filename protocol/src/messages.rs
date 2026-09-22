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
// Every layout here mirrors `driver/virtio_gpu_nv.c`. That file is the wire
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
    pub padding: u32,
}

impl MsgHeader {
    /// A success response carrying `handle`.
    pub fn ok(msg_type: MsgType, handle: u32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle,
            status: 0,
            padding: 0,
        }
    }

    /// A failure response. `errno` is given as a positive number and stored
    /// negated, which is the one direction that is easy to get wrong.
    pub fn err(msg_type: MsgType, errno: i32) -> Self {
        Self {
            msg_type: msg_type as u32,
            handle: 0,
            status: -errno.abs(),
            padding: 0,
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
/// Render nodes start here.
pub const DEV_DRI_BASE: u32 = 512;
/// Highest GPU index expressible before the control device's value.
pub const MAX_GPU_INDEX: u32 = 254;

/// Which device an `Open` refers to.
///
/// The wire encoding is one flat `u32`, not a kind and an index: a GPU is its
/// own minor number, and the singleton devices take values above every possible
/// minor. Decoding is therefore a range check, not a table lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// `/dev/nvidiaN`, where N is the guest's minor number.
    Gpu(u32),
    Ctl,
    Uvm,
    UvmTools,
    Modeset,
    /// A DRM render node, by its index in the list the device reported.
    Dri(u32),
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
        ] {
            assert_eq!(MsgType::from_u32(t as u32), Some(t));
        }
        assert_eq!(MsgType::from_u32(0), None);
        assert_eq!(MsgType::from_u32(8), None);
    }
}
