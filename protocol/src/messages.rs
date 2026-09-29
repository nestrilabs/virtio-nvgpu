// SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0-or-later
// protocol/src/messages.rs
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
    /// One of the two messages that travel this way (the other is
    /// [`MsgType::EventData`]). NVIDIA's user-mode driver waits
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
    ///
    /// [`DEEP_SEGMENTED`] instead says the deep block is a segment table
    /// ([`DeepSegHdr`]) carrying what several pointers address.
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

/// `IoctlReq::deep_ptr_offset` for a segmented deep block: several pointers
/// of one parameter block, each sent with the bytes it addresses. Only to a
/// backend that says [`BCAP_DEEP_SEGS`]. An older one would refuse the call
/// (an RM control's: the offset is outside any nested block) or ignore the
/// block (IDLE_CHANNELS', and then refuse the list), and a guest that sees
/// no bit sends what it always did: the pointers go unrelocated, and the
/// backend zeroes them.
///
/// The deep block is a [`DeepSegHdr`], `count` [`DeepSeg`]s, and then each
/// segment's bytes, back to back in table order and nothing after. The block
/// whose pointers the segments name is fixed by the call: an RM_CONTROL's
/// parameters (the nested block), or NV_ESC_RM_IDLE_CHANNELS' top-level
/// NVOS30. The backend sizes every segment from that block itself -- how much
/// RM will copy through the pointer, from the count fields RM reads -- and
/// refuses the call if a length differs, if a pointer is named twice or is
/// not one RM follows there, or if the table and the bytes do not add up.
/// Pointers of the block with no segment are zeroed, as ever.
///
/// The reply's deep block, when there is one, is laid out as the request's
/// was, with each segment's bytes as RM left them; the guest copies back the
/// ones RM writes. A reply with no deep block (IDLE_CHANNELS, whose arrays
/// RM only reads) has nothing to copy back.
pub const DEEP_SEGMENTED: u32 = u32::MAX;

/// Most segments in one deep block: RM's embedded copies have four slots
/// (embedded_param_copy.c, `paramCopies[4]`).
pub const DEEP_SEGS_MAX: u32 = 4;

/// Most bytes all of one call's segments may carry together. RM's own bound
/// on one embedded copy is the same (RMAPI_PARAM_COPY_MAX_PARAMS_SIZE).
pub const DEEP_SEGS_MAX_BYTES: u32 = 1 << 20;

/// Most channels an NV_ESC_RM_IDLE_CHANNELS list may name (RM has no bound
/// of its own: "this should have a max", rmapi_deprecated_misc.c).
pub const IDLE_CHANNELS_MAX: u32 = 4096;

/// Head of a segmented deep block.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeepSegHdr {
    /// Segments in the table, 1 to [`DEEP_SEGS_MAX`].
    pub count: u32,
    /// Zero.
    pub reserved: u32,
}

/// One pointer of a segmented deep block.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeepSeg {
    /// Where the pointer sits in the block that holds it.
    pub ptr_offset: u32,
    /// Bytes it addresses, sent after the table.
    pub len: u32,
}

/// `IoctlReq::deep_ptr_offset` for an OS-descriptor page list: memory the
/// caller already has, registered with RM by the guest-physical pages behind
/// it rather than by its address. Only to a backend that says
/// [`BCAP_OS_DESC`], and only on the three calls that register it:
/// NV_ESC_RM_ALLOC_MEMORY and NV_ESC_RM_ALLOC of
/// NV01_MEMORY_SYSTEM_OS_DESCRIPTOR (0x71), and NV_ESC_RM_VID_HEAP_CONTROL's
/// NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR. The descriptor type must be the user
/// virtual address one, and the address stays in the block as the caller
/// wrote it: the backend reads its offset inside the first page from it.
///
/// The deep block is an [`OsDescHdr`] and `nruns` [`OsDescRun`]s, nothing
/// after. The guest has pinned the pages (as RM pins them: for writing
/// unless the call asks for read-only memory, and says which in
/// [`OSDESC_F_WRITE`]) and keeps them pinned until the backend reports the
/// registration released ([`OP_OSDESC_REAP`]). The runs cover exactly the
/// pages RM would pin: from the one holding the address to the one holding
/// its last byte, `limit + 1` bytes on. The backend checks every page lies in
/// guest RAM and hands RM an address of its own mapping those pages, in
/// order; anything else refuses the call before it reaches the host.
///
/// A reply whose RM status is NV_OK carries a deep block of 8 bytes: the
/// registration's id, which a later reap names. Otherwise there is none,
/// nothing was registered, and the guest unpins at once.
pub const DEEP_PAGE_LIST: u32 = u32::MAX - 1;

/// [`OsDescHdr::flags`]: the pages were pinned for writing, which is what
/// RM asks for unless the call says the memory is read-only to the CPU.
/// The backend maps them read-only otherwise, and refuses a list whose flag
/// disagrees with the call.
pub const OSDESC_F_WRITE: u32 = 1 << 0;

/// Most runs one page list may carry: what the backend maps separately for
/// scattered pages, and a list that fits a request without indirect
/// descriptors.
pub const OSDESC_MAX_RUNS: u32 = 8192;

/// Most pages one registration may name (4 GiB).
pub const OSDESC_MAX_PAGES: u32 = 1 << 20;

/// Head of an OS-descriptor page list.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OsDescHdr {
    /// Runs following, 1 to [`OSDESC_MAX_RUNS`].
    pub nruns: u32,
    /// `OSDESC_F_*`.
    pub flags: u32,
}

/// Guest-physically contiguous pages of a page list, in the order they
/// appear in the caller's range.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct OsDescRun {
    /// Page-aligned guest-physical address.
    pub gpa: u64,
    /// Pages from there, at least one.
    pub pages: u32,
    /// Zero.
    pub reserved: u32,
}

/// The guest process an IOCTL is made by, sent after its blocks (after the
/// `deep_len` bytes) on `NV_ESC_RM_ALLOC` and `NV_ESC_RM_DUP_OBJECT` -- and on
/// `NV_ESC_RM_CONTROL` too once the backend answered [`BCAP_PROC_EUID`] -- by
/// a guest that said [`GCAP_PROC_ID`] to a backend that answered
/// [`BCAP_PROC_ID`]. Every guest process's RM calls are the backend's on the
/// host, so RM sees one process where the guest has many; this is how the
/// backend tells them apart (rmshare.rs).
///
/// The same guest also sends one after the fixed part of every `Open`
/// ([`OpenReq`]) and `HostOp` ([`HostOpReq`]): what those make is charged to
/// that process, which may hold only a share of the VM's handles and other
/// budgets (device/src/quota.rs). A backend that predates it ignores the
/// bytes after the fixed part.
///
/// The guest kernel fills it from the calling thread's group leader: its
/// thread-group id in the initial PID namespace and its start time
/// (CLOCK_MONOTONIC ns, which exec keeps and a fork does not share). A PID is
/// unique among live processes, and a reused one comes with a later start
/// time, so the pair names one process for the guest's lifetime, whatever
/// PID namespace it runs in. The backend only compares the pair.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ProcId {
    pub start_ns: u64,
    pub tgid: u32,
    /// The calling task's effective uid, as the guest kernel's initial user
    /// namespace numbers it (`current_euid()`), when the session has
    /// [`BCAP_PROC_EUID`]: what RM's security token holds for a host
    /// process (os_get_euid). Zero, and not read, otherwise.
    pub euid: u32,
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
    /// How the guest must map it (`MMAP_CACHE_*`). Was padding before
    /// protocol v2, and a v2 backend still leaves it zero for a v1 session,
    /// so zero keeps meaning what it always did: the guest's own choice,
    /// write-combining. The host picks a memory type per mapping (nv-mmap.c
    /// forces UC on registers, uses the allocation's own type for system
    /// memory), and a guest mapping of a different type is either slow (WC
    /// reads of cached memory) or wrong (a WC doorbell).
    pub caching: u8,
    /// `MMAP_F_*`.
    pub flags: u8,
    pub reserved: u16,
}

/// `MmapResp::caching`: 0 is a v1 backend's answer (write-combining).
pub const MMAP_CACHE_DEFAULT: u8 = 0;
pub const MMAP_CACHE_WB: u8 = 1;
pub const MMAP_CACHE_WC: u8 = 2;
pub const MMAP_CACHE_UC: u8 = 3;

/// `MmapResp::flags`: the host mapping is read-only. nvidia.ko clears
/// VM_WRITE and VM_MAYWRITE when the mapping context lacks WRITEABLE
/// (nv-mmap.c:756-761), so the placement is read-only too, and a guest write
/// through it would reach KVM as an unresolvable write fault that stops the
/// whole VM. The guest clears VM_WRITE and VM_MAYWRITE on its side instead,
/// so the write faults in the guest process, as it would on the host.
pub const MMAP_F_READ_ONLY: u8 = 1 << 0;

/// `MmapResp::flags`: `guest_phys_addr` is an offset in the UVM aperture
/// (shared memory region [`SHM_ID_UVM`]), not in the window. Only in reply to
/// an MMAP of a UVM file, and only from a backend that said [`BCAP_UVM_MAP`]
/// to a guest that said [`GCAP_UVM_APERTURE`]: the mapping is a UVM
/// semaphore pool the VMM maps at the pool's own host address (UVM requires
/// address == offset), with a memory slot of its own in the aperture.
/// Always write-back, never read-only.
pub const MMAP_F_UVM_APERTURE: u8 = 1 << 1;

/// The shared memory region id of the UVM aperture. The window is id 1.
pub const SHM_ID_UVM: u8 = 2;

/// The host addresses a UVM pool may be mapped at, in the VMM: [4 GiB,
/// 32 TiB). The address is the guest's choice, so it is held to a band where
/// a 64-bit VMM has nothing of its own: its executable and heap sit at
/// two-thirds of the 47-bit space (85 TiB), its mappings grow down from
/// below the stack or, under a legacy layout, up from a third of it
/// (42.7 TiB). The backend, the VMM and the guest driver all check it.
pub const UVM_HVA_MIN: u64 = 1 << 32;
pub const UVM_HVA_MAX: u64 = 1 << 45;

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
    assert!(size_of::<DeepSegHdr>() == 8);
    assert!(size_of::<DeepSeg>() == 8);
    assert!(size_of::<OsDescHdr>() == 8);
    assert!(size_of::<OsDescRun>() == 16);
    assert!(size_of::<ProcId>() == 16);
    assert!(size_of::<MmapReq>() == 24);
    assert!(size_of::<MmapResp>() == 24);
    assert!(core::mem::offset_of!(MmapResp, caching) == 20);
    assert!(core::mem::offset_of!(MmapResp, flags) == 21);
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
/// Segmented deep blocks ([`DEEP_SEGMENTED`]) are understood.
pub const BCAP_DEEP_SEGS: u32 = 1 << 5;
/// An MMAP of a UVM file that names one of its semaphore pools exactly is
/// placed in the UVM aperture ([`MMAP_F_UVM_APERTURE`]). Offered only to a
/// guest that said [`GCAP_UVM_APERTURE`], and only when the host's UVM takes
/// multi-process sharing mode.
pub const BCAP_UVM_MAP: u32 = 1 << 6;
/// Memory the caller already has is registered by its guest-physical pages
/// ([`DEEP_PAGE_LIST`]) and released through [`OP_OSDESC_REAP`]. Offered
/// only when the backend holds guest RAM (the vhost-user memory table).
pub const BCAP_OS_DESC: u32 = 1 << 7;
/// Every `NV_ESC_RM_ALLOC` and `NV_ESC_RM_DUP_OBJECT` is to carry the calling
/// guest process ([`ProcId`]) after its blocks, and RM objects are kept to
/// the process that made their client (rmshare.rs). Offered only to a guest
/// that said [`GCAP_PROC_ID`]. Without it the backend knows no guest process:
/// a duplicate between two clients, and a call naming a client other than
/// its own, are refused.
pub const BCAP_PROC_ID: u32 = 1 << 8;
/// With [`BCAP_PROC_ID`]: `ProcId::euid` is the caller's effective uid, and
/// every `NV_ESC_RM_CONTROL` carries a [`ProcId`] too, so that a control
/// naming a second client is held to RM's rule for it (rmshare.rs). Offered
/// only to a guest that said [`GCAP_PROC_EUID`].
pub const BCAP_PROC_EUID: u32 = 1 << 9;
/// The compute paths are served (`--allow-compute`): `/dev/nvidia-uvm`,
/// and with it UVM's sharing mode and the UVM aperture ([`BCAP_UVM_MAP`]),
/// and memory registered by its pages ([`BCAP_OS_DESC`]). Without it the
/// backend refuses every UVM open and never offers those two, and the guest
/// makes no UVM device: to NVIDIA's userspace, a host whose nvidia-uvm is not
/// loaded.
pub const BCAP_COMPUTE: u32 = 1 << 10;
/// Host buffers a helper injected may be opened ([`OP_INJECT_OPEN`];
/// `--inject-socket`). Without it the guest makes no `/dev/nvgpu-capture`.
pub const BCAP_INJECT: u32 = 1 << 11;
/// Readiness of a device handle's host descriptor (the legacy watch every
/// OPEN makes, RM's event queue) is reported once per [`W_ARM`], not once
/// per host event. Offered only to a guest that said [`GCAP_ARMS_READY`].
pub const BCAP_ARMED_READY: u32 = 1 << 12;

/// `HelloReq::guest_caps` bits.
///
/// The guest found the UVM aperture (shared memory region [`SHM_ID_UVM`])
/// and says how large it is in `HelloReq::uvm_aperture_mib`.
pub const GCAP_UVM_APERTURE: u32 = 1 << 0;
/// The guest can say which of its processes makes each RM call
/// ([`ProcId`], [`BCAP_PROC_ID`]).
pub const GCAP_PROC_ID: u32 = 1 << 1;
/// The guest's [`ProcId`] carries the caller's effective uid, and it can send
/// one on every RM control ([`BCAP_PROC_EUID`]).
pub const GCAP_PROC_EUID: u32 = 1 << 2;
/// The guest arms each report of a device handle's readiness with a
/// [`W_ARM`] WATCH when something of its own waits on the handle
/// ([`BCAP_ARMED_READY`]).
pub const GCAP_ARMS_READY: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HelloReq {
    pub proto: u32,
    pub flags: u32,
    /// `GCAP_*`. Zero from a guest that predates them.
    pub guest_caps: u32,
    /// The UVM aperture's length in MiB, 0 when the guest has none. Was
    /// reserved (zero), so an older guest reads as having none.
    pub uvm_aperture_mib: u32,
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

/// [`TimeSyncResp`] with the host's other two clocks, read in the same
/// instant: the ones RM stamps GPU/CPU time-correlation samples with
/// (NV2080_CTRL_CMD_TIMER_GET_GPU_CPU_TIME_CORRELATION_INFO: OSTIME is
/// `CLOCK_REALTIME` in microseconds, PLATFORM_API `CLOCK_MONOTONIC_RAW` in
/// nanoseconds). Sent only when the reply buffer has room for it: an older
/// guest posts room for the 8-byte form and gets that, and an older backend
/// fills only the first 8 bytes of a larger buffer, which the used length
/// tells the guest.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TimeSyncResp2 {
    pub host_mono_ns: u64,
    pub host_realtime_ns: u64,
    pub host_mono_raw_ns: u64,
    pub reserved: u64,
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
/// Alone, with [`BCAP_ARMED_READY`]: report the device handle's readiness
/// once more -- at once if its host descriptor had an event since the last
/// report, else at its next one.
pub const W_ARM: u32 = 1 << 4;

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
/// `(ack) -> (last, count)`, then `count` u64 registration ids after the
/// [`HostOpResp`]: the OS-descriptor registrations ([`DEEP_PAGE_LIST`]) RM
/// has let go of, whose pages the guest may now unpin. Each release has a
/// sequence number; the reply names the ones after `ack`, oldest first, at
/// most [`OSDESC_REAP_MAX`], and `last` is the sequence number of the last
/// one named (`ack` itself when none is). The guest passes `last` as the
/// next `ack`, and only then does the backend forget them: a reply that is
/// lost is answered again. A release not yet acknowledged still counts
/// against the VM's registrations, so a guest that never reaps runs out of
/// its own budget and nothing else.
pub const OP_OSDESC_REAP: u32 = 11;
/// Most ids one reap reply names.
pub const OSDESC_REAP_MAX: u32 = 256;
/// `(render file, id, token[0..8], token[8..16]) -> (gem, size, type)`, then
/// an [`crate::inject::InjectInfo`] (64 bytes) after the [`HostOpResp`]: a
/// buffer the host's capture helper injected (`--inject-socket`,
/// device/src/inject/), imported into the render file as a GEM handle the
/// guest makes a proxy of. The token is the one IMPORT gave the helper, as
/// two little-endian words; an id that is not live, or a token that does
/// not match, is -ENOENT either way. `type` is always 0 (NVKMS): nothing
/// else is injected. Only with [`BCAP_INJECT`].
pub const OP_INJECT_OPEN: u32 = 12;
/// `(render file, id, token[0..8], token[8..16]) -> (syncobj handle)`: a
/// syncobj the capture helper injected (IMPORT_SYNCOBJ), imported into the
/// render file, where the guest's handle is the host's number (fences are
/// the host's, ARCHITECTURE.md, "Fences"). -ENOENT for an id that is not a live
/// syncobj or a token that does not match. Only with [`BCAP_INJECT`].
pub const OP_INJECT_OPEN_SYNCOBJ: u32 = 13;

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
    /// When the host's fences signalled, host `CLOCK_MONOTONIC` ns (the
    /// latest of them, from SYNC_IOC_FILE_INFO's per-fence array); 0 when
    /// unknown. A guest that predates it reads the first 8 bytes, and an
    /// older backend's 8-byte record reads as 0.
    pub timestamp_ns: u64,
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
    assert!(size_of::<TimeSyncResp2>() == 32);
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
    assert!(size_of::<EvFence>() == 16);
    assert!(size_of::<EvHotplug>() == 8);
    assert!(size_of::<CardRecord>() == 16);
};

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    fn wire_header() -> crate::cheader::Header {
        crate::cheader::Header::parse(include_str!("../../driver/nvgpu_wire.h"))
    }

    /// Every integer the C header defines is one of these, with this value.
    /// A define added on either side and not here fails
    /// `every_define_in_the_wire_header_is_mirrored`.
    const WIRE: &[(&str, u64)] = &[
        ("NVGPU_MSG_OPEN", MsgType::Open as u64),
        ("NVGPU_MSG_CLOSE", MsgType::Close as u64),
        ("NVGPU_MSG_IOCTL", MsgType::Ioctl as u64),
        ("NVGPU_MSG_MMAP", MsgType::Mmap as u64),
        ("NVGPU_MSG_MUNMAP", MsgType::Munmap as u64),
        ("NVGPU_MSG_GET_PROC_FILES", MsgType::GetProcFiles as u64),
        ("NVGPU_MSG_GET_SYS_FILES", MsgType::GetSysFiles as u64),
        ("NVGPU_MSG_EVENT_READY", MsgType::EventReady as u64),
        ("NVGPU_MSG_HELLO", MsgType::Hello as u64),
        ("NVGPU_MSG_IOCTL2", MsgType::Ioctl2 as u64),
        ("NVGPU_MSG_TIME_SYNC", MsgType::TimeSync as u64),
        ("NVGPU_MSG_EVENT_DATA", MsgType::EventData as u64),
        ("NVGPU_MSG_WATCH", MsgType::Watch as u64),
        ("NVGPU_MSG_UNWATCH", MsgType::Unwatch as u64),
        ("NVGPU_MSG_HOST_OP", MsgType::HostOp as u64),
        ("NVGPU_MSG_WL_SEND", MsgType::WlSend as u64),
        ("NVGPU_MSG_WL_RECV", MsgType::WlRecv as u64),
        ("NVGPU_DEV_CTL", DEV_CTL as u64),
        ("NVGPU_DEV_UVM", DEV_UVM as u64),
        ("NVGPU_DEV_UVM_TOOLS", DEV_UVM_TOOLS as u64),
        ("NVGPU_DEV_MODESET", DEV_MODESET as u64),
        ("NVGPU_DEV_WAYLAND", DEV_WAYLAND as u64),
        ("NVGPU_DEV_DRI_BASE", DEV_DRI_BASE as u64),
        ("NVGPU_DEV_DRI_CARD_BASE", DEV_DRI_CARD_BASE as u64),
        ("NVGPU_DEEP_SEGMENTED", DEEP_SEGMENTED as u64),
        ("NVGPU_DEEP_SEGS_MAX", DEEP_SEGS_MAX as u64),
        ("NVGPU_DEEP_SEGS_MAX_BYTES", DEEP_SEGS_MAX_BYTES as u64),
        ("NVGPU_IDLE_CHANNELS_MAX", IDLE_CHANNELS_MAX as u64),
        ("NVGPU_DEEP_PAGE_LIST", DEEP_PAGE_LIST as u64),
        ("NVGPU_OSDESC_F_WRITE", OSDESC_F_WRITE as u64),
        ("NVGPU_OSDESC_MAX_RUNS", OSDESC_MAX_RUNS as u64),
        ("NVGPU_OSDESC_MAX_PAGES", OSDESC_MAX_PAGES as u64),
        ("NVGPU_MMAP_CACHE_DEFAULT", MMAP_CACHE_DEFAULT as u64),
        ("NVGPU_MMAP_CACHE_WB", MMAP_CACHE_WB as u64),
        ("NVGPU_MMAP_CACHE_WC", MMAP_CACHE_WC as u64),
        ("NVGPU_MMAP_CACHE_UC", MMAP_CACHE_UC as u64),
        ("NVGPU_MMAP_F_READ_ONLY", MMAP_F_READ_ONLY as u64),
        ("NVGPU_MMAP_F_UVM_APERTURE", MMAP_F_UVM_APERTURE as u64),
        ("NVGPU_SHM_ID_UVM", SHM_ID_UVM as u64),
        ("NVGPU_UVM_HVA_MIN", UVM_HVA_MIN),
        ("NVGPU_UVM_HVA_MAX", UVM_HVA_MAX),
        ("NVGPU_PROTO_V2", PROTO_V2 as u64),
        ("NVGPU_HELLO_F_FRESH", HELLO_F_FRESH as u64),
        ("NVGPU_BCAP_KMS_CARD", BCAP_KMS_CARD as u64),
        ("NVGPU_BCAP_WAYLAND", BCAP_WAYLAND as u64),
        ("NVGPU_BCAP_FENCES", BCAP_FENCES as u64),
        ("NVGPU_BCAP_NVKMS_TABLE", BCAP_NVKMS_TABLE as u64),
        ("NVGPU_BCAP_WL_EXPORT", BCAP_WL_EXPORT as u64),
        ("NVGPU_BCAP_DEEP_SEGS", BCAP_DEEP_SEGS as u64),
        ("NVGPU_BCAP_UVM_MAP", BCAP_UVM_MAP as u64),
        ("NVGPU_BCAP_OS_DESC", BCAP_OS_DESC as u64),
        ("NVGPU_BCAP_PROC_ID", BCAP_PROC_ID as u64),
        ("NVGPU_BCAP_PROC_EUID", BCAP_PROC_EUID as u64),
        ("NVGPU_BCAP_COMPUTE", BCAP_COMPUTE as u64),
        ("NVGPU_BCAP_INJECT", BCAP_INJECT as u64),
        ("NVGPU_BCAP_ARMED_READY", BCAP_ARMED_READY as u64),
        ("NVGPU_GCAP_UVM_APERTURE", GCAP_UVM_APERTURE as u64),
        ("NVGPU_GCAP_PROC_ID", GCAP_PROC_ID as u64),
        ("NVGPU_GCAP_PROC_EUID", GCAP_PROC_EUID as u64),
        ("NVGPU_GCAP_ARMS_READY", GCAP_ARMS_READY as u64),
        ("NVGPU_I2_MAX_BUFS", I2_MAX_BUFS as u64),
        ("NVGPU_I2_MAX_RECS", I2_MAX_RECS as u64),
        ("NVGPU_I2_FD_CONSUME", I2_FD_CONSUME as u64),
        ("NVGPU_I2_DYN_OUT_FENCE", I2_DYN_OUT_FENCE as u64),
        ("NVGPU_HK_DEV", HK_DEV as u64),
        ("NVGPU_HK_DRI_RENDER", HK_DRI_RENDER as u64),
        ("NVGPU_HK_DRM_CARD", HK_DRM_CARD as u64),
        ("NVGPU_HK_DRM_LEASE", HK_DRM_LEASE as u64),
        ("NVGPU_HK_SYNC_FILE", HK_SYNC_FILE as u64),
        ("NVGPU_HK_SYNCOBJ", HK_SYNCOBJ as u64),
        ("NVGPU_HK_DMABUF", HK_DMABUF as u64),
        ("NVGPU_HK_EVENTFD", HK_EVENTFD as u64),
        ("NVGPU_HK_MEMFD", HK_MEMFD as u64),
        ("NVGPU_HK_WAYLAND", HK_WAYLAND as u64),
        ("NVGPU_HK_OTHER", HK_OTHER as u64),
        ("NVGPU_W_ONESHOT", W_ONESHOT as u64),
        ("NVGPU_W_FENCE", W_FENCE as u64),
        ("NVGPU_W_DRM", W_DRM as u64),
        ("NVGPU_W_READY", W_READY as u64),
        ("NVGPU_W_ARM", W_ARM as u64),
        ("NVGPU_OP_PRIME_EXPORT", OP_PRIME_EXPORT as u64),
        ("NVGPU_OP_DMABUF_IMPORT", OP_DMABUF_IMPORT as u64),
        ("NVGPU_OP_SYNC_MERGE", OP_SYNC_MERGE as u64),
        ("NVGPU_OP_NEW_EVENTFD", OP_NEW_EVENTFD as u64),
        ("NVGPU_OP_FD_KIND", OP_FD_KIND as u64),
        ("NVGPU_OP_SIGNALED_SYNC_FILE", OP_SIGNALED_SYNC_FILE as u64),
        ("NVGPU_OP_OPEN_KMS", OP_OPEN_KMS as u64),
        ("NVGPU_OP_DROP_IF_MASTER", OP_DROP_IF_MASTER as u64),
        ("NVGPU_OP_CLOSE_MANY", OP_CLOSE_MANY as u64),
        ("NVGPU_OP_SYNCOBJ_WATCH", OP_SYNCOBJ_WATCH as u64),
        ("NVGPU_OP_OSDESC_REAP", OP_OSDESC_REAP as u64),
        ("NVGPU_OSDESC_REAP_MAX", OSDESC_REAP_MAX as u64),
        ("NVGPU_OP_INJECT_OPEN", OP_INJECT_OPEN as u64),
        (
            "NVGPU_OP_INJECT_OPEN_SYNCOBJ",
            OP_INJECT_OPEN_SYNCOBJ as u64,
        ),
        ("NVGPU_OP_MAX_ARGS", OP_MAX_ARGS as u64),
        ("NVGPU_OP_MAX_RES", OP_MAX_RES as u64),
        ("NVGPU_EV_DRM", EV_DRM as u64),
        ("NVGPU_EV_FENCE", EV_FENCE as u64),
        ("NVGPU_EV_READY", EV_READY as u64),
        ("NVGPU_EV_HOTPLUG", EV_HOTPLUG as u64),
        ("NVGPU_EVENT_BUF_SIZE", EVENT_BUF_SIZE as u64),
        ("NVGPU_EV_HOTPLUG_F_HOTPLUG", EV_HOTPLUG_F_HOTPLUG as u64),
        ("NVGPU_EV_HOTPLUG_F_LEASE", EV_HOTPLUG_F_LEASE as u64),
    ];

    /// Defines of the header this crate does not mirror, and why.
    const NOT_HERE: &[&str] = &[
        // The config space's, which device/src/virtio.rs holds and checks
        // against this header.
        "VIRTIO_ID_GPU_NV",
        "NVGPU_FDT_UVM",
        // The guest's feature table and the config space's `caps`: the
        // backend offers none of these features and leaves `caps` zero.
        "VIRTIO_GPU_NV_F_UVM",
        "VIRTIO_GPU_NV_F_ENCODE",
        "VIRTIO_GPU_NV_F_GRAPHICS",
        "NVGPU_CAP_COMPUTE",
        "NVGPU_CAP_GRAPHICS",
        "NVGPU_CAP_VIDEO",
        "NVGPU_CAP_UTILITY",
    ];

    /// Every integer nvgpu_wire.h defines has its value here, and this table
    /// names nothing the header lacks: nothing else checks that the two
    /// halves, which ship separately, agree.
    #[test]
    fn every_define_in_the_wire_header_is_mirrored() {
        let h = wire_header();
        assert_eq!(h.bare, ["NVGPU_WIRE_H"]);
        for (name, value) in &h.defines {
            if NOT_HERE.contains(&name.as_str()) {
                continue;
            }
            let Some((_, ours)) = WIRE.iter().find(|(n, _)| n == name) else {
                panic!("{name} is not in WIRE");
            };
            assert_eq!(value, ours, "{name}");
        }
        for (name, _) in WIRE {
            assert!(h.defines.contains_key(*name), "{name} is not in the header");
        }
        for name in NOT_HERE {
            assert!(h.defines.contains_key(*name), "{name} is not in the header");
        }
    }

    /// Every struct of nvgpu_wire.h is laid out as its mirror here: the same
    /// size, and each field at the same offset under the same name. Most
    /// mirrors leave out the leading `hdr` (a `MsgHeader` read separately).
    #[test]
    fn every_wire_struct_is_laid_out_as_the_header_says() {
        use core::mem::{offset_of, size_of};
        let h = wire_header();
        // C struct, whether its mirror leaves out `hdr`, mirror's size,
        // mirror's fields as (name, offset).
        type Mirror<'a> = (&'a str, bool, usize, &'a [(&'a str, usize)]);
        macro_rules! mirror {
            ($c:literal, $hdr:literal, $t:ty, [$($f:ident),* $(,)?]) => {
                ($c, $hdr, size_of::<$t>(), &[$((stringify!($f), offset_of!($t, $f))),*][..])
            };
        }
        let mirrors: &[Mirror] = &[
            mirror!(
                "nvgpu_msg_hdr",
                false,
                MsgHeader,
                [msg_type, handle, status, req_id]
            ),
            mirror!("nvgpu_open_req", true, OpenReq, [device_type, flags]),
            mirror!("nvgpu_open_resp", true, (), []),
            mirror!(
                "nvgpu_ioctl_req",
                true,
                IoctlReq,
                [
                    cmd,
                    data_len,
                    nested_offset,
                    nested_len,
                    deep_ptr_offset,
                    deep_len,
                ]
            ),
            mirror!(
                "nvgpu_ioctl_resp",
                true,
                IoctlResp,
                [data_len, nested_len, deep_len]
            ),
            mirror!("nvgpu_deep_seg_hdr", false, DeepSegHdr, [count, reserved]),
            mirror!("nvgpu_deep_seg", false, DeepSeg, [ptr_offset, len]),
            mirror!("nvgpu_osdesc_hdr", false, OsDescHdr, [nruns, flags]),
            mirror!("nvgpu_osdesc_run", false, OsDescRun, [gpa, pages, reserved]),
            mirror!(
                "nvgpu_mmap_req",
                true,
                MmapReq,
                [size, offset, prot, padding]
            ),
            mirror!(
                "nvgpu_mmap_resp",
                true,
                MmapResp,
                [guest_phys_addr, size, mapping_id, caching, flags, reserved,]
            ),
            mirror!("nvgpu_munmap_req", true, MunmapReq, [mapping_id, padding]),
            mirror!("nvgpu_munmap_resp", true, (), []),
            mirror!(
                "nvgpu_proc_file_entry",
                false,
                FileEntry,
                [path_len, content_len]
            ),
            mirror!("nvgpu_proc_id", false, ProcId, [start_ns, tgid, euid]),
            mirror!(
                "nvgpu_hello_req",
                false,
                HelloReq,
                [proto, flags, guest_caps, uvm_aperture_mib,]
            ),
            mirror!(
                "nvgpu_hello_resp",
                false,
                HelloResp,
                [proto, backend_caps, max_req, max_resp, num_cards, reserved,]
            ),
            mirror!("nvgpu_time_sync_resp", false, TimeSyncResp, [host_mono_ns]),
            mirror!(
                "nvgpu_time_sync_resp2",
                false,
                TimeSyncResp2,
                [host_mono_ns, host_realtime_ns, host_mono_raw_ns, reserved,]
            ),
            mirror!(
                "nvgpu_i2_req",
                false,
                Ioctl2Req,
                [cmd, flags, nbuf, nfd, ngem, ndyn, data_len, render,]
            ),
            mirror!(
                "nvgpu_i2_fd_in",
                false,
                Ioctl2FdIn,
                [buf, off, handle, flags]
            ),
            mirror!(
                "nvgpu_i2_gem_in",
                false,
                Ioctl2GemIn,
                [buf, off, owner, gem]
            ),
            mirror!("nvgpu_i2_dyn", false, Ioctl2Dyn, [kind, buf, off, len]),
            mirror!(
                "nvgpu_i2_resp",
                false,
                Ioctl2Resp,
                [ret, nbuf, nfd, ngem, data_len, reserved,]
            ),
            mirror!(
                "nvgpu_i2_fd_out",
                false,
                Ioctl2FdOut,
                [buf, off, handle, kind]
            ),
            mirror!(
                "nvgpu_i2_gem_out",
                false,
                Ioctl2GemOut,
                [buf, off, gem, reserved, size]
            ),
            mirror!("nvgpu_watch_req", false, WatchReq, [handle, flags, cookie]),
            mirror!("nvgpu_unwatch_req", false, UnwatchReq, [handle, pad]),
            mirror!("nvgpu_host_op_req", false, HostOpReq, [op, nargs, args]),
            mirror!("nvgpu_host_op_resp", false, HostOpResp, [nres, pad, res]),
            mirror!(
                "nvgpu_inject_info",
                false,
                crate::inject::InjectInfo,
                [
                    width, height, fourcc, nplanes, modifier, offsets, strides, flags, reserved,
                ]
            ),
            mirror!("nvgpu_ev_rec", false, EvRec, [kind, len, cookie]),
            mirror!(
                "nvgpu_ev_fence",
                false,
                EvFence,
                [status, pad, timestamp_ns]
            ),
            mirror!("nvgpu_ev_hotplug", false, EvHotplug, [flags, pad]),
            mirror!(
                "nvgpu_card_record",
                false,
                CardRecord,
                [name_len, major, minor, render_index,]
            ),
            mirror!("nvgpu_wl_recv_req", false, WlRecvReq, [max_bytes, max_desc]),
            mirror!("nvgpu_wl_send_resp", false, WlSendResp, [accepted, backlog]),
        ];
        // Structs mirrored elsewhere, or not as one struct.
        let elsewhere = [
            // device/src/virtio.rs, against this header.
            "virtio_gpu_nv_gpu_slot",
            "nvgpu_fd_translation_entry",
            "virtio_gpu_nv_config",
            // An OPEN request followed by a ProcId, checked below.
            "nvgpu_open_req_proc",
        ];
        for (name, c) in &h.structs {
            if elsewhere.contains(&name.as_str()) {
                continue;
            }
            let Some(&(_, hdr, size, fields)) = mirrors.iter().find(|m| m.0 == name) else {
                panic!("struct {name} has no mirror");
            };
            let mut c_fields = c.fields.iter().map(|(n, o)| (n.as_str(), *o)).peekable();
            let base = if hdr {
                assert_eq!(c_fields.next(), Some(("hdr", 0)), "{name}");
                size_of::<MsgHeader>()
            } else {
                0
            };
            assert_eq!(c.size, base + size, "sizeof(struct {name})");
            let c_fields: std::vec::Vec<_> = c_fields.map(|(n, o)| (n, o - base)).collect();
            assert_eq!(c_fields, fields, "struct {name}");
        }
        for (name, ..) in mirrors {
            assert!(
                h.structs.contains_key(*name),
                "struct {name} is not in the header"
            );
        }
        let proc_open = h.layout("nvgpu_open_req_proc");
        assert_eq!(
            proc_open.offset("proc"),
            size_of::<MsgHeader>() + size_of::<OpenReq>()
        );
        assert_eq!(
            proc_open.size,
            proc_open.offset("proc") + size_of::<ProcId>()
        );
    }

    /// Every capability bit is distinct, on each side.
    #[test]
    fn capability_bits_are_distinct() {
        let bcaps = [
            BCAP_KMS_CARD,
            BCAP_WAYLAND,
            BCAP_FENCES,
            BCAP_NVKMS_TABLE,
            BCAP_WL_EXPORT,
            BCAP_DEEP_SEGS,
            BCAP_UVM_MAP,
            BCAP_OS_DESC,
            BCAP_PROC_ID,
            BCAP_PROC_EUID,
            BCAP_COMPUTE,
            BCAP_INJECT,
            BCAP_ARMED_READY,
        ];
        assert_eq!(
            bcaps.iter().fold(0, |a, b| a | b).count_ones() as usize,
            bcaps.len()
        );
        let gcaps = [
            GCAP_UVM_APERTURE,
            GCAP_PROC_ID,
            GCAP_PROC_EUID,
            GCAP_ARMS_READY,
        ];
        assert_eq!(
            gcaps.iter().fold(0, |a, b| a | b).count_ones() as usize,
            gcaps.len()
        );
    }

    #[test]
    fn device_types_decode_the_way_the_driver_encodes_them() {
        assert_eq!(DeviceKind::from_device_type(0), Some(DeviceKind::Gpu(0)));
        assert_eq!(DeviceKind::from_device_type(3), Some(DeviceKind::Gpu(3)));
        assert_eq!(DeviceKind::from_device_type(255), Some(DeviceKind::Ctl));
        assert_eq!(DeviceKind::from_device_type(256), Some(DeviceKind::Uvm));
        assert_eq!(
            DeviceKind::from_device_type(257),
            Some(DeviceKind::UvmTools)
        );
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
        assert_eq!(
            DeviceKind::from_device_type(DEV_WAYLAND),
            Some(DeviceKind::Wayland)
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_CARD_BASE + 1),
            Some(DeviceKind::DriCard(1))
        );
        assert_eq!(
            DeviceKind::from_device_type(DEV_DRI_CARD_BASE - 1),
            Some(DeviceKind::Dri(DEV_DRI_CARD_BASE - 1 - DEV_DRI_BASE))
        );
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
