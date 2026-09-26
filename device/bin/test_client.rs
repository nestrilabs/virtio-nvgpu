// crates/device/src/bin/test_client.rs
//
// Minimal test client for the test harness.
//
// Connects to the Unix socket, exercises the Phase 1/2 message flow:
//   1. OPEN /dev/nvidiactl
//   2. IOCTL NV_ESC_CHECK_VERSION_STR (Phase 2)
//   3. CLOSE
//
// Usage:
//   cargo run --bin test-client [--socket /tmp/nv-vhost.sock]

#![forbid(unsafe_code)]

use clap::Parser;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::unix::net::UnixStream;
// We can't depend on the `protocol` crate's no_std types easily from a
// binary with std, so we duplicate the repr(C) layouts here.  These MUST
// match crates/protocol/src/messages.rs exactly.

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MsgType {
    Open = 1,
    Close = 2,
    Ioctl = 3,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ok = 0,
    InvalidMsgType = 1,
    InvalidDevice = 2,
    OpenFailed = 3,
    BadHandle = 4,
    IoctlFailed = 5,
    BufferTooSmall = 6,
}

impl Status {
    fn from_u32(v: u32) -> Self {
        match v {
            0 => Self::Ok,
            1 => Self::InvalidMsgType,
            2 => Self::InvalidDevice,
            3 => Self::OpenFailed,
            4 => Self::BadHandle,
            5 => Self::IoctlFailed,
            6 => Self::BufferTooSmall,
            _ => panic!("unknown status {}", v),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct MsgHeader {
    msg_type: u32,
    _pad: u32,
    cookie: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct RespHeader {
    status: u32,
    errno_host: i32,
    cookie: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct OpenReq {
    kind: u8,
    index: u8,
    _pad: [u8; 6],
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct OpenResp {
    guest_handle: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct CloseReq {
    guest_handle: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct IoctlReq {
    guest_handle: u64,
    request: u64,
    param_size: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct IoctlResp {
    param_size: u32,
    _pad: u32,
    shm_offset: u64,
    shm_length: u64,
    pgprot: u8,
    _pad2: [u8; 7],
}

// ---------------------------------------------------------------------------
// Serialisation helpers
// ---------------------------------------------------------------------------

/// The wire form of a message struct, written and read field by field in
/// the order and at the widths of its `repr(C)` layout (which has no
/// padding: every struct here is laid out in 8-byte units).
trait Wire: Sized {
    fn put(&self, out: &mut Vec<u8>);
    fn get(b: &[u8]) -> Self;
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

impl Wire for MsgHeader {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.msg_type.to_le_bytes());
        out.extend_from_slice(&self._pad.to_le_bytes());
        out.extend_from_slice(&self.cookie.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        Self {
            msg_type: u32_at(b, 0),
            _pad: u32_at(b, 4),
            cookie: u64_at(b, 8),
        }
    }
}

impl Wire for RespHeader {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.status.to_le_bytes());
        out.extend_from_slice(&self.errno_host.to_le_bytes());
        out.extend_from_slice(&self.cookie.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        Self {
            status: u32_at(b, 0),
            errno_host: u32_at(b, 4) as i32,
            cookie: u64_at(b, 8),
        }
    }
}

impl Wire for OpenReq {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&[self.kind, self.index]);
        out.extend_from_slice(&self._pad);
    }
    fn get(b: &[u8]) -> Self {
        Self {
            kind: b[0],
            index: b[1],
            _pad: b[2..8].try_into().unwrap(),
        }
    }
}

impl Wire for OpenResp {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.guest_handle.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        Self {
            guest_handle: u64_at(b, 0),
        }
    }
}

impl Wire for CloseReq {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.guest_handle.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        Self {
            guest_handle: u64_at(b, 0),
        }
    }
}

impl Wire for IoctlReq {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.guest_handle.to_le_bytes());
        out.extend_from_slice(&self.request.to_le_bytes());
        out.extend_from_slice(&self.param_size.to_le_bytes());
        out.extend_from_slice(&self._pad.to_le_bytes());
    }
    fn get(b: &[u8]) -> Self {
        Self {
            guest_handle: u64_at(b, 0),
            request: u64_at(b, 8),
            param_size: u32_at(b, 16),
            _pad: u32_at(b, 20),
        }
    }
}

impl Wire for IoctlResp {
    fn put(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.param_size.to_le_bytes());
        out.extend_from_slice(&self._pad.to_le_bytes());
        out.extend_from_slice(&self.shm_offset.to_le_bytes());
        out.extend_from_slice(&self.shm_length.to_le_bytes());
        out.push(self.pgprot);
        out.extend_from_slice(&self._pad2);
    }
    fn get(b: &[u8]) -> Self {
        Self {
            param_size: u32_at(b, 0),
            _pad: u32_at(b, 4),
            shm_offset: u64_at(b, 8),
            shm_length: u64_at(b, 16),
            pgprot: b[24],
            _pad2: b[25..32].try_into().unwrap(),
        }
    }
}

fn as_bytes<T: Wire>(val: &T) -> Vec<u8> {
    let mut out = Vec::new();
    val.put(&mut out);
    out
}

fn from_bytes<T: Wire>(buf: &[u8], offset: usize) -> T {
    assert!(buf.len() >= offset + size_of::<T>());
    T::get(&buf[offset..])
}

// ---------------------------------------------------------------------------
// Wire helpers — length-prefixed framing matching test_harness.rs
// ---------------------------------------------------------------------------

fn send_msg(stream: &mut UnixStream, payload: &[u8]) {
    let len = (payload.len() as u32).to_le_bytes();
    stream.write_all(&len).expect("write length prefix");
    stream.write_all(payload).expect("write payload");
}

fn recv_msg(stream: &mut UnixStream) -> Vec<u8> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length prefix");
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).expect("read response body");
    buf
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

fn build_open_ctl(cookie: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&as_bytes(&MsgHeader {
        msg_type: MsgType::Open as u32,
        _pad: 0,
        cookie,
    }));
    buf.extend_from_slice(&as_bytes(&OpenReq {
        kind: 0, // DeviceKind::Ctl
        _pad: [0; 6],
        index: 0,
    }));
    buf
}

fn build_close(cookie: u64, guest_handle: u64) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&as_bytes(&MsgHeader {
        msg_type: MsgType::Close as u32,
        _pad: 0,
        cookie,
    }));
    buf.extend_from_slice(&as_bytes(&CloseReq { guest_handle }));
    buf
}

fn build_check_version_str(cookie: u64, guest_handle: u64) -> Vec<u8> {
    // nv_ioctl_rm_api_version_t:
    //   NvU32 cmd;                              // offset 0,  4 bytes
    //   NvU32 reply;                            // offset 4,  4 bytes
    //   char  versionString[64];                // offset 8, 64 bytes
    // Total: 72 bytes
    //
    // We send cmd=NV_RM_API_VERSION_CMD_QUERY ('2' = 0x32) so the driver
    // just fills in its version string without checking for a match.
    let param_size: u32 = 72;
    let mut params = vec![0u8; param_size as usize];
    // cmd = '2' (query mode)
    params[0] = 0x32;

    let ioctl_nr: u64 = abi::ioctl::_IOWR(abi::ioctl::NV_ESC_CHECK_VERSION_STR, param_size);

    let mut buf = Vec::new();
    buf.extend_from_slice(&as_bytes(&MsgHeader {
        msg_type: MsgType::Ioctl as u32,
        _pad: 0,
        cookie,
    }));
    buf.extend_from_slice(&as_bytes(&IoctlReq {
        guest_handle,
        request: ioctl_nr,
        param_size,
        _pad: 0,
    }));
    buf.extend_from_slice(&params);
    buf
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Socket path to listen on
    #[arg(long, default_value = "/tmp/nv-vhost.sock")]
    socket_path: String,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(tracing_subscriber::filter::LevelFilter::INFO.into())
                .from_env()?,
        )
        .init();

    let args = Args::parse();

    tracing::info!("connecting to {}", args.socket_path);
    let mut stream = UnixStream::connect(&args.socket_path).expect("connect failed");
    tracing::info!("connected");

    // ---- Step 1: OPEN /dev/nvidiactl ----
    tracing::info!("=== OPEN /dev/nvidiactl ===");
    send_msg(&mut stream, &build_open_ctl(1));
    let resp = recv_msg(&mut stream);

    let rhdr: RespHeader = from_bytes(&resp, 0);
    let status = Status::from_u32(rhdr.status);
    tracing::info!("  status:     {:?}", status);
    tracing::info!("  errno_host: {}", rhdr.errno_host);
    tracing::info!("  cookie:     {}", rhdr.cookie);

    if status != Status::Ok {
        return Err(anyhow::anyhow!("Open failed"));
    }

    let open_resp: OpenResp = from_bytes(&resp, size_of::<RespHeader>());
    let handle = open_resp.guest_handle;
    tracing::info!("  handle:     {}", handle);

    // ---- Step 2: IOCTL NV_ESC_CHECK_VERSION_STR ----
    tracing::info!("=== IOCTL NV_ESC_CHECK_VERSION_STR ===");
    send_msg(&mut stream, &build_check_version_str(2, handle));
    let resp = recv_msg(&mut stream);

    let rhdr: RespHeader = from_bytes(&resp, 0);
    let status = Status::from_u32(rhdr.status);
    tracing::info!("  status:     {:?}", status);
    tracing::info!("  errno_host: {}", rhdr.errno_host);

    if status == Status::Ok {
        let iresp: IoctlResp = from_bytes(&resp, size_of::<RespHeader>());
        tracing::info!("  param_size: {}", iresp.param_size);

        if iresp.param_size >= 72 {
            let param_start = size_of::<RespHeader>() + size_of::<IoctlResp>();
            let params = &resp[param_start..param_start + iresp.param_size as usize];

            // cmd at offset 0
            let cmd = u32::from_le_bytes(params[0..4].try_into().unwrap());
            // reply at offset 4
            let reply = u32::from_le_bytes(params[4..8].try_into().unwrap());
            // versionString at offset 8, 64 bytes, null-terminated
            let ver_bytes = &params[8..72];
            let ver_end = ver_bytes.iter().position(|&b| b == 0).unwrap_or(64);
            let version = std::str::from_utf8(&ver_bytes[..ver_end]).unwrap_or("<invalid utf8>");

            tracing::info!("  cmd:        0x{:x}", cmd);
            tracing::info!("  reply:      {} (1=recognized)", reply);
            tracing::info!("  version:    \"{}\"", version);
        }
    }

    // ---- Step 3: CLOSE ----
    tracing::info!("=== CLOSE handle={} ===", handle);
    send_msg(&mut stream, &build_close(3, handle));
    let resp = recv_msg(&mut stream);

    let rhdr: RespHeader = from_bytes(&resp, 0);
    let status = Status::from_u32(rhdr.status);
    tracing::info!("  status:     {:?}", status);
    tracing::info!("  cookie:     {}", rhdr.cookie);

    tracing::info!("done");

    Ok(())
}
