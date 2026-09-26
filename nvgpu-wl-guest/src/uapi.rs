//! Mirror of `driver/uapi/nvgpu_wl.h`, field for field, with the same sizes
//! asserted. The frame layout itself is `wlwire::frame`, which the header's
//! frame structs mirror in turn.

#![forbid(unsafe_code)]

pub const UAPI_VERSION: u32 = 1;

pub const CAP_WAYLAND: u32 = 1 << 0;
pub const CAP_EXPORT: u32 = 1 << 1;
pub const CAP_DRM_FILE: u32 = 1 << 2;
pub const CAP_DMABUF_IMPORT: u32 = 1 << 3;
pub const CAP_SYNCOBJ: u32 = 1 << 4;

pub const MAX_DEVMAP: usize = 8;
pub const DEV_RENDER: u32 = 1 << 0;
pub const DEV_CARD: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Devmap {
    pub host_major: u32,
    pub host_minor: u32,
    pub guest_major: u32,
    pub guest_minor: u32,
    pub flags: u32,
    pub pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Hello {
    pub version: u32,
    pub caps: u32,
    pub clock_offset_ns: i64,
    pub max_frame: u32,
    pub ndev: u32,
    pub dev: [Devmap; MAX_DEVMAP],
}

pub const CONNECT: u32 = 0;
pub const LISTEN: u32 = 1;
pub const ACCEPT: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Connect {
    pub mode: u32,
    pub flags: u32,
}

/// `struct nvgpu_wl_connect_for`: CONNECT charged to the client process
/// `pid` rather than to the daemon (the backend's per-process share of the
/// VM's channels, device/src/quota.rs).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConnectFor {
    pub mode: u32,
    pub flags: u32,
    pub pid: i32,
    pub pad: u32,
}

pub const XFER_MORE: u32 = 1 << 0;

/// `NVGPU_WL_MIN_FRAME`: the smallest RECV buffer the host accepts.
pub const MIN_FRAME: usize = 16 + 32 * 24 + 16 + 65536;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Xfer {
    pub frame: u64,
    pub len: u32,
    pub max_desc: u32,
    pub card_fd: i32,
    pub render_fd: i32,
    pub flags: u32,
    pub backlog: u32,
}

const fn ioc(dir: u32, ty: u8, nr: u8, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr as u32) as libc::c_ulong
}
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

pub const IOC_HELLO: libc::c_ulong = ioc(IOC_READ, b'W', 0x40, size_of::<Hello>());
pub const IOC_CONNECT: libc::c_ulong = ioc(IOC_WRITE, b'W', 0x41, size_of::<Connect>());
pub const IOC_SEND: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, b'W', 0x42, size_of::<Xfer>());
pub const IOC_RECV: libc::c_ulong = ioc(IOC_READ | IOC_WRITE, b'W', 0x43, size_of::<Xfer>());
pub const IOC_CONNECT_FOR: libc::c_ulong = ioc(IOC_WRITE, b'W', 0x44, size_of::<ConnectFor>());

const _: () = {
    assert!(size_of::<Devmap>() == 24);
    assert!(size_of::<Hello>() == 216);
    assert!(size_of::<Connect>() == 8);
    assert!(size_of::<ConnectFor>() == 16);
    assert!(size_of::<Xfer>() == 32);
    // The frame structs the header declares are wlwire's.
    assert!(wlwire::frame::FRAME_HDR_LEN == 16);
    assert!(wlwire::frame::DESC_LEN == 24);
    assert!(wlwire::frame::REC_HDR_LEN == 16);
    assert!(MIN_FRAME == wlwire::frame::MIN_FRAME);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_c_macros() {
        // _IOR('W', 0x40, 216), _IOW('W', 0x41, 8), _IOWR('W', 0x42/0x43, 32)
        assert_eq!(IOC_HELLO, 0x80d8_5740);
        assert_eq!(IOC_CONNECT, 0x4008_5741);
        assert_eq!(IOC_SEND, 0xc020_5742);
        assert_eq!(IOC_RECV, 0xc020_5743);
        // _IOW('W', 0x44, 16)
        assert_eq!(IOC_CONNECT_FOR, 0x4010_5744);
    }
}
