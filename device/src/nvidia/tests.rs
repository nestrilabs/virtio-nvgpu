// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;

/// A request header. The handle travels here now, not in the payload.
fn hdr(msg_type: MsgType, handle: u64) -> Vec<u8> {
    let mut v = vec![0u8; size_of::<MsgHeader>()];
    write_struct(
        &mut v,
        &MsgHeader {
            msg_type: msg_type as u32,
            handle: handle as u32,
            status: 0,
            req_id: 0,
        },
    );
    v
}

/// `device_type` for an `OpenReq`, as the driver encodes it: a GPU is its
/// own minor number and the singletons take values above every minor.
fn dev_type(kind: DeviceKind) -> u32 {
    match kind {
        DeviceKind::Gpu(n) => n,
        DeviceKind::Ctl => DEV_CTL,
        DeviceKind::Uvm => DEV_UVM,
        DeviceKind::UvmTools => DEV_UVM_TOOLS,
        DeviceKind::Modeset => DEV_MODESET,
        DeviceKind::Dri(n) => DEV_DRI_BASE + n,
        DeviceKind::DriCard(n) => DEV_DRI_CARD_BASE + n,
        DeviceKind::Wayland => DEV_WAYLAND,
    }
}

/// A complete `Open` message.
fn open_msg(kind: DeviceKind) -> Vec<u8> {
    let mut v = hdr(MsgType::Open, 0);
    append(
        &mut v,
        &OpenReq {
            device_type: dev_type(kind),
            flags: 0,
        },
    );
    v
}

/// A complete `Close` message. The handle is the header's.
fn close_msg(handle: u64) -> Vec<u8> {
    hdr(MsgType::Close, handle)
}

fn append<T: crate::sys::pod::Pod>(v: &mut Vec<u8>, val: &T) {
    let start = v.len();
    v.resize(start + size_of::<T>(), 0);
    write_struct(&mut v[start..], val);
}

/// The response header. `status` is signed: zero on success, negative
/// errno on failure -- there is no separate status vocabulary on the wire.
fn parse_resp(buf: &[u8]) -> MsgHeader {
    read_struct::<MsgHeader>(buf, 0)
}

/// Whether a response reports the errno `want` maps to.
fn is_err(buf: &[u8], errno: i32) -> bool {
    parse_resp(buf).status == -errno
}

/// The handle an `Open` returned, which now arrives in the header.
fn opened_handle(buf: &[u8]) -> u64 {
    parse_resp(buf).handle as u64
}

/// Offset of an ioctl response's parameter block.
const IOCTL_BODY: usize = size_of::<MsgHeader>() + size_of::<IoctlResp>();

/// Whether the GPU-backed tests can run here.
///
/// These tests return early without a GPU, which means they report as
/// passes on a machine that never exercised a line of the code they cover.
/// Say so on stderr, so that `cargo test -- --nocapture` distinguishes
/// "verified against a driver" from "skipped, and green either way".
#[track_caller]
fn nvidiactl_present() -> bool {
    let present = std::path::Path::new("/dev/nvidiactl").exists();
    if !present {
        eprintln!(
            "SKIP {}: needs /dev/nvidiactl; this test passes without testing anything",
            std::panic::Location::caller()
        );
    }
    present
}

// ---- error paths (no GPU required) ----

#[test]
fn open_invalid_gpu_index() {
    let mut be = NvidiaBackend::for_test();
    let req = open_msg(DeviceKind::Gpu(200));
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert!(is_err(&resp, libc::ENODEV));
}

#[test]
fn close_unknown_handle() {
    let mut be = NvidiaBackend::for_test();
    let req = close_msg(0xCAFE);
    let mut resp = vec![0u8; 32];
    be.dispatch(&req, &mut resp);
    assert!(is_err(&resp, libc::EBADF));
}

#[test]
fn short_request_rejected() {
    let mut be = NvidiaBackend::for_test();
    be.dispatch(&[0u8; 4], &mut [0u8; 32]);
    // just must not panic
}

/// Fuzzing (backend target): a request too short for a header, or of no
/// known type, was answered with a 16-byte header whatever capacity the
/// guest posted. `serve` promises no reply longer than `cap`; the
/// vhost-user transport never posts less than a header, another
/// transport might.
#[test]
fn a_malformed_request_is_answered_within_the_posted_capacity() {
    let mut be = NvidiaBackend::for_test();
    for req in [&[0u8; 4][..], &[0xffu8; 16][..]] {
        for cap in [0, 2, 15, 16] {
            let Outcome::Reply(r) = be.serve(req, cap) else {
                panic!("an IOCTL2 from nothing");
            };
            assert!(r.bytes.len() <= cap, "{} bytes for {cap}", r.bytes.len());
        }
    }
}

// ---- teardown tests (no GPU required) ----

#[test]
fn teardown_empties_handles() {
    if !nvidiactl_present() {
        return;
    }
    let mut be = NvidiaBackend::for_test();

    // Open two fds
    for _ in 0..2 {
        let req = open_msg(DeviceKind::Ctl);
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
    }
    assert_eq!(be.handle_count(), 2);

    be.teardown();
    assert_eq!(be.handle_count(), 0);
}

#[test]
fn drop_closes_remaining_handles() {
    if !nvidiactl_present() {
        return;
    }
    // Open a handle, then drop the backend without calling teardown().
    // The Drop impl should drain the table and not panic.
    let mut be = NvidiaBackend::for_test();
    let req = open_msg(DeviceKind::Ctl);
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert_eq!(be.handle_count(), 1);
    drop(be); // must not panic; Drop closes the fd
}

// ---- GPU-present round-trip tests ----

#[test]
fn open_close_nvidiactl() {
    if !nvidiactl_present() {
        return;
    }
    let mut be = NvidiaBackend::for_test();

    let req = open_msg(DeviceKind::Ctl);
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    let r = parse_resp(&resp);
    assert_eq!(r.status, 0);

    let h = opened_handle(&resp);
    assert!(h > 0);

    let req2 = close_msg(h);
    let mut resp2 = vec![0u8; 32];
    be.dispatch(&req2, &mut resp2);
    assert_eq!(parse_resp(&resp2).status, 0);
    assert_eq!(be.handle_count(), 0);
}

#[test]
fn check_version_str() {
    if !nvidiactl_present() {
        return;
    }
    let mut be = NvidiaBackend::for_test();

    let oreq = open_msg(DeviceKind::Ctl);
    let mut oresp = vec![0u8; 64];
    be.dispatch(&oreq, &mut oresp);
    let gh = opened_handle(&oresp);
    assert!(gh > 0);

    // nv_ioctl_rm_api_version_t: cmd(4) + reply(4) + versionString(64) = 72 bytes
    let param_size: u32 = 72;
    let mut ireq = hdr(MsgType::Ioctl, gh);
    append(
        &mut ireq,
        &IoctlReq {
            cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_CHECK_VERSION_STR, param_size) as u32,
            data_len: param_size,
            nested_offset: 0,
            nested_len: 0,
            deep_ptr_offset: 0,
            deep_len: 0,
        },
    );
    // First 4 bytes = cmd field. Set to '2' (0x32) for query mode.
    let mut params = vec![0u8; param_size as usize];
    params[0] = 0x32;
    ireq.extend(params);

    let mut iresp = vec![0u8; 512];
    be.dispatch(&ireq, &mut iresp);
    let r = parse_resp(&iresp);
    assert!(
        r.status == -0 || r.status == -libc::EIO,
        "unexpected status {}",
        r.status
    );
}

/// NV_ESC_RM_MAP_MEMORY round-trip.
///
/// We can't test a real mapping without a valid RM client/device/memory
/// triple, but we CAN test that:
///   1. The dispatch path is reached (not hitting "unhandled escape").
///   2. The embedded FD is translated correctly.
///   3. The host ioctl failure is reported cleanly (since we don't have
///      valid RM handles, the host driver will reject the call).
#[test]
fn map_memory_rejects_bad_fd_handle() {
    let mut be = NvidiaBackend::for_test();

    // We need an open nvidiactl fd as the "outer" fd for the ioctl.
    if !nvidiactl_present() {
        return;
    }

    // Open nvidiactl.
    let oreq = open_msg(DeviceKind::Ctl);
    let mut oresp = vec![0u8; 64];
    be.dispatch(&oreq, &mut oresp);
    let gh = opened_handle(&oresp);
    assert!(gh > 0);

    // Build a NV_ESC_RM_MAP_MEMORY ioctl with a bogus embedded FD handle.
    // IoctlNVOS33ParametersWithFD = 56 bytes.
    let param_size: u32 = 56;
    let mut ireq = hdr(MsgType::Ioctl, gh);
    append(
        &mut ireq,
        &IoctlReq {
            cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size) as u32,
            data_len: param_size,
            nested_offset: 0,
            nested_len: 0,
            deep_ptr_offset: 0,
            deep_len: 0,
        },
    );

    // 56 bytes of zeroed params — the embedded FD at offset 48 is 0,
    // which is not a valid guest handle.
    let mut params = vec![0u8; param_size as usize];
    // Write a bogus FD handle (0xDEAD) at offset 48.
    params[48..52].copy_from_slice(&0xDEADu32.to_le_bytes());
    ireq.extend(params);

    let mut iresp = vec![0u8; 512];
    be.dispatch(&ireq, &mut iresp);
    // Should fail with BadHandle since 0xDEAD is not in the handle table.
    assert!(is_err(&iresp, libc::EBADF));
}

#[test]
fn map_memory_translates_fd_and_forwards() {
    if !nvidiactl_present() {
        return;
    }

    let mut be = NvidiaBackend::for_test();

    // Open nvidiactl — this is both the "outer" fd and the "map" fd.
    let oreq = open_msg(DeviceKind::Ctl);
    let mut oresp = vec![0u8; 64];
    be.dispatch(&oreq, &mut oresp);
    let ctl_handle = opened_handle(&oresp);

    // Open a second nvidiactl fd to use as the embedded map FD.
    let oreq2 = open_msg(DeviceKind::Ctl);
    let mut oresp2 = vec![0u8; 64];
    be.dispatch(&oreq2, &mut oresp2);
    let map_handle = opened_handle(&oresp2);

    // Build IoctlNVOS33ParametersWithFD with the map_handle as embedded FD.
    let param_size: u32 = 56;
    let mut ireq = hdr(MsgType::Ioctl, ctl_handle);
    append(
        &mut ireq,
        &IoctlReq {
            cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, param_size) as u32,
            data_len: param_size,
            nested_offset: 0,
            nested_len: 0,
            deep_ptr_offset: 0,
            deep_len: 0,
        },
    );

    let mut params = vec![0u8; param_size as usize];
    // Embedded FD at offset 48 = map_handle.
    params[48..52].copy_from_slice(&(map_handle as u32).to_le_bytes());
    ireq.extend(params);

    let mut iresp = vec![0u8; 512];
    be.dispatch(&ireq, &mut iresp);
    let r = parse_resp(&iresp);

    // The host ioctl will fail (we have no valid RM objects) but the
    // dispatch path should reach the host ioctl — so we expect either
    // IoctlFailed (host rejected it) or Ok (unlikely without valid handles).
    // The key thing: it should NOT be BadHandle, proving FD translation worked.
    assert_ne!(
        r.status,
        -libc::EBADF,
        "FD translation should have succeeded"
    );
}

/// What the backend adds to an RM control, on the real driver: the same
/// cheap control (GPU_GET_ATTACHED_IDS on a root client) served through
/// `dispatch` and issued directly, each timed over many calls. A timing,
/// not a check: `cargo test --release -p device bench_rm_control_service
/// -- --ignored --nocapture` (BENCHMARKS.md).
#[test]
#[ignore]
fn bench_rm_control_service() {
    if !nvidiactl_present() {
        return;
    }
    let mut be = NvidiaBackend::for_test();
    let mut oresp = vec![0u8; 64];
    be.dispatch(&open_msg(DeviceKind::Ctl), &mut oresp);
    let ctl = opened_handle(&oresp);
    assert!(ctl > 0);
    let ioctl = |be: &mut NvidiaBackend, nr: u32, params: &[u8], nested: &[u8], at: u32| {
        let mut req = hdr(MsgType::Ioctl, ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(nr, params.len() as u32) as u32,
                data_len: params.len() as u32,
                nested_offset: at,
                nested_len: nested.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(params);
        req.extend_from_slice(nested);
        let mut resp = vec![0u8; 1024];
        be.dispatch(&req, &mut resp);
        resp
    };
    // RM_ALLOC of a root client: NVOS64 with no parameters.
    let mut alloc = [0u8; 48];
    alloc[12..16].copy_from_slice(&0x41u32.to_le_bytes());
    let r = ioctl(&mut be, abi::ioctl::NV_ESC_RM_ALLOC, &alloc, &[], 0);
    assert_eq!(parse_resp(&r).status, 0);
    let body = &r[size_of::<MsgHeader>() + size_of::<IoctlResp>()..];
    let client = u32::from_le_bytes(body[8..12].try_into().unwrap());
    assert_eq!(u32::from_le_bytes(body[40..44].try_into().unwrap()), 0);
    // NVOS54: hClient, hObject, cmd, flags, params (at 16), paramsSize, status.
    let mut ctl54 = [0u8; 32];
    ctl54[0..4].copy_from_slice(&client.to_le_bytes());
    ctl54[4..8].copy_from_slice(&client.to_le_bytes());
    ctl54[8..12].copy_from_slice(&0x0000_0201u32.to_le_bytes());
    ctl54[24..28].copy_from_slice(&128u32.to_le_bytes());
    let ids = [0u8; 128];
    let n: u32 = std::env::var("NVGPU_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200_000);
    for _ in 0..1000 {
        ioctl(&mut be, abi::ioctl::NV_ESC_RM_CONTROL, &ctl54, &ids, 16);
    }
    let t0 = std::time::Instant::now();
    for _ in 0..n {
        let r = ioctl(&mut be, abi::ioctl::NV_ESC_RM_CONTROL, &ctl54, &ids, 16);
        debug_assert_eq!(parse_resp(&r).status, 0);
    }
    // Natively the call takes about 1.4 us (nvgpu-bench rm-ctl).
    eprintln!(
        "RM_CONTROL GPU_GET_ATTACHED_IDS served in {:?} a call",
        t0.elapsed() / n
    );
}

// ------------------------------------------------------------------
// Real mapping round-trip
//
// Everything above stops at the host ioctl and expects it to fail, because
// building a mappable RM object takes a chain of allocations. That left the
// SHM allocate / mmap / free path never actually executed against a driver.
//
// The chain below is the shortest one a Tesla T4 was observed using before
// its first successful NV_ESC_RM_MAP_MEMORY, taken from a captured trace:
//
//   NV01_ROOT_CLIENT (0x41)   -> hClient
//   NV01_DEVICE_0    (0x80)   -> hDevice
//   NV20_SUBDEVICE_0 (0x2080) -> hSubdevice
//   TURING_USERMODE_A(0xc461) -> hMemory, mapped at 64 KiB
//
// TURING_USERMODE_A is the usermode doorbell aperture, so this maps real
// GPU registers, not system memory. Later GPUs have their own usermode
// class (a GB202 driver allocates HOPPER_USERMODE_A); the chain takes the
// newest one the device lists, as userspace chooses from that list too.
// ------------------------------------------------------------------

const NV01_ROOT_CLIENT: u32 = 0x41;
const NV01_DEVICE_0: u32 = 0x80;
const NV20_SUBDEVICE_0: u32 = 0x2080;
/// VOLTA .. BLACKWELL_USERMODE_A, oldest first (rmmem.rs treats every
/// one of them as registers).
const USERMODE_CLASSES: [u32; 5] = [0xc361, 0xc461, 0xc561, 0xc661, 0xc761];
const NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2: u32 = 0x0080_0292;
const NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE: usize = 200;

/// NVOS64_PARAMETERS field offsets.
const A_ROOT: usize = 0;
const A_PARENT: usize = 4;
const A_NEW: usize = 8;
const A_CLASS: usize = 12;
const A_PARAMS_SIZE: usize = 32;
const A_STATUS: usize = 40;
const ALLOC_OUTER: usize = 48;

/// Places device memory in this process's own SHM window, the way the
/// VMM places it in the guest's (bin/vhost-user-nvgpu.rs `VhostWindow`):
/// `MAP_FIXED` of the device fd over the range, and the memfd back on
/// withdraw. With it the window really aliases what RM mapped, so a test
/// can read the GPU through it.
struct LocalWindow(Arc<crate::sys::mem::Window>);

impl crate::shm::WindowPlacer for LocalWindow {
    fn place(&self, off: u64, len: u64, fd: RawFd, fo: u64, writable: bool) -> Result<()> {
        Ok(self.0.place(off, len, fd, fo, writable)?)
    }
    fn withdraw(&self, off: u64, len: u64) -> Result<()> {
        Ok(self.0.restore(off, len)?)
    }
}

struct Chain {
    be: NvidiaBackend,
    ctl: u64,
    gpu: u64,
    cookie: u64,
    /// The usermode class the device lists, found by `usermode_object`.
    usermode: u32,
}

impl Chain {
    fn new() -> Self {
        // Not for_test(): its write-combine zone is 16 KiB, and the
        // smallest real mapping here is 64 KiB.
        let mut be = NvidiaBackend::with_default_zones();
        // A mapping is refused (EOPNOTSUPP) until something can place it
        // where the guest reaches it. The VMM does that in a real run;
        // here this process is the VMM, and the window is its own.
        be.set_window(Box::new(LocalWindow(be.shm_window())));
        let req = open_msg(DeviceKind::Ctl);
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "open /dev/nvidiactl");
        let ctl = opened_handle(&resp);
        let mut c = Self {
            be,
            ctl,
            gpu: 0,
            cookie: 2,
            usermode: 0,
        };
        // The driver always issues these two before allocating a client.
        // Without them the device allocation is refused with
        // NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b).
        c.simple(abi::ioctl::NV_ESC_SYS_PARAMS, 8);
        c.simple(abi::ioctl::NV_ESC_CARD_INFO, 2304);
        // The driver opens /dev/nvidia0 and registers the control fd
        // against it before allocating a device. Skipping this is refused
        // with NV_ERR_INSUFFICIENT_PERMISSIONS (0x1b).
        c.gpu = c.open_dev(DeviceKind::Gpu(0));
        c.register_fd(c.gpu, c.ctl);
        c
    }

    /// Open one of the character devices and return its guest handle.
    fn open_dev(&mut self, kind: DeviceKind) -> u64 {
        self.cookie += 1;
        let req = open_msg(kind);
        let mut resp = vec![0u8; 64];
        self.be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "open {kind:?}");
        opened_handle(&resp)
    }

    /// NV_ESC_REGISTER_FD: attach `fd_handle` to the device `on`.
    fn register_fd(&mut self, on: u64, fd_handle: u64) {
        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, on);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_REGISTER_FD, 4) as u32,
                data_len: 4,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&(fd_handle as u32).to_le_bytes());
        let mut resp = vec![0u8; 256];
        self.be.dispatch(&req, &mut resp);
    }

    /// Issue a parameterless escape whose payload is just a zeroed buffer.
    fn simple(&mut self, escape: u32, size: u32) {
        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(escape, size) as u32,
                data_len: size,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&vec![0u8; size as usize]);
        let mut resp = vec![0u8; size as usize + 256];
        self.be.dispatch(&req, &mut resp);
    }

    /// Issue an RM_ALLOC and return the handle RM assigned.
    fn alloc(&mut self, root: u32, parent: u32, class: u32, params: &[u8]) -> u32 {
        let declared = params.len() as u32;
        self.alloc_with(root, parent, class, params, declared)
    }

    /// Same, but with an explicit `paramsSize` field.
    ///
    /// The captured driver sends the parameter block with `paramsSize` set
    /// to 0 and lets RM use the size the class defines. Passing the byte
    /// count instead is rejected with NV_ERR_INVALID_ARGUMENT.
    fn alloc_with(
        &mut self,
        root: u32,
        parent: u32,
        class: u32,
        params: &[u8],
        declared: u32,
    ) -> u32 {
        let mut outer = vec![0u8; ALLOC_OUTER];
        outer[A_ROOT..A_ROOT + 4].copy_from_slice(&root.to_le_bytes());
        outer[A_PARENT..A_PARENT + 4].copy_from_slice(&parent.to_le_bytes());
        outer[A_CLASS..A_CLASS + 4].copy_from_slice(&class.to_le_bytes());
        outer[A_PARAMS_SIZE..A_PARAMS_SIZE + 4].copy_from_slice(&declared.to_le_bytes());

        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_ALLOC, ALLOC_OUTER as u32) as u32,
                data_len: ALLOC_OUTER as u32,
                // The class parameters follow the top-level struct, which
                // is where the driver puts them.
                nested_offset: ALLOC_OUTER as u32,
                nested_len: params.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&outer);
        req.extend_from_slice(params);

        let mut resp = vec![0u8; 4096];
        let n = self.be.dispatch(&req, &mut resp);
        assert!(n > 0, "alloc class {class:#x}: empty response");
        assert_eq!(
            parse_resp(&resp).status,
            0,
            "alloc class {class:#x}: transport status"
        );
        let body = IOCTL_BODY;
        let out = &resp[body..body + ALLOC_OUTER];
        let status = u32::from_le_bytes(out[A_STATUS..A_STATUS + 4].try_into().unwrap());
        assert_eq!(status, 0, "alloc class {class:#x}: RM status {status:#x}");
        u32::from_le_bytes(out[A_NEW..A_NEW + 4].try_into().unwrap())
    }

    /// NV_ESC_RM_CONTROL with inline parameters; returns them as RM
    /// left them.
    fn control(&mut self, client: u32, object: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
        let mut outer = vec![0u8; NVOS54_SIZE];
        outer[0..4].copy_from_slice(&client.to_le_bytes());
        outer[4..8].copy_from_slice(&object.to_le_bytes());
        outer[NVOS54_CMD..NVOS54_CMD + 4].copy_from_slice(&cmd.to_le_bytes());
        outer[NVOS54_PARAMS_SIZE..NVOS54_PARAMS_SIZE + 4]
            .copy_from_slice(&(params.len() as u32).to_le_bytes());

        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_CONTROL, NVOS54_SIZE as u32) as u32,
                data_len: NVOS54_SIZE as u32,
                nested_offset: NVOS54_SIZE as u32,
                nested_len: params.len() as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&outer);
        req.extend_from_slice(params);

        let mut resp = vec![0u8; 8192];
        self.be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "control {cmd:#x}: transport");
        let out = &resp[IOCTL_BODY..IOCTL_BODY + NVOS54_SIZE + params.len()];
        let st = u32::from_le_bytes(out[NVOS54_STATUS..NVOS54_STATUS + 4].try_into().unwrap());
        assert_eq!(st, 0, "control {cmd:#x}: RM status {st:#x}");
        out[NVOS54_SIZE..].to_vec()
    }

    /// The newest usermode class the device lists.
    fn usermode_class(&mut self, client: u32, device: u32) -> u32 {
        let size = 4 + 4 * NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE;
        let out = self.control(
            client,
            device,
            NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2,
            &vec![0u8; size],
        );
        let n = u32::from_le_bytes(out[0..4].try_into().unwrap()) as usize;
        let listed: Vec<u32> = out[4..4 + 4 * n.min(NV0080_CTRL_GPU_CLASSLIST_MAX_SIZE)]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|&c| u32::from_le_bytes(c))
            .collect();
        let class = *USERMODE_CLASSES
            .iter()
            .rev()
            .find(|c| listed.contains(c))
            .expect("the device lists no usermode class");
        eprintln!("usermode class {class:#x} (of {n} classes listed)");
        class
    }

    /// Build the object chain and return (hClient, hSubdevice, hMemory).
    fn usermode_object(&mut self) -> (u32, u32, u32) {
        let client = self.alloc(0, 0, NV01_ROOT_CLIENT, &[]);
        assert_ne!(client, 0, "RM assigned no client handle");

        // The captured driver passes paramsSize 0 for all of these; RM
        // uses the class's own parameter size rather than trusting the
        // caller, so sending none is what the real sequence does.
        // NV0080_ALLOC_PARAMETERS, zeroed apart from hClientShare.
        let mut dev_params = vec![0u8; 56];
        dev_params[4..8].copy_from_slice(&client.to_le_bytes());
        let device = self.alloc(client, client, NV01_DEVICE_0, &dev_params);

        // NV2080_ALLOC_PARAMETERS is a single subDeviceID.
        let subdevice = self.alloc(client, device, NV20_SUBDEVICE_0, &0u32.to_le_bytes());

        self.usermode = self.usermode_class(client, device);
        let memory = self.alloc(client, subdevice, self.usermode, &[]);
        (client, subdevice, memory)
    }

    /// A dedicated fd to carry the mapping.
    ///
    /// This is a **/dev/nvidia0** fd, not /dev/nvidiactl: the trace shows
    /// the mmap landing on the per-GPU node even though the
    /// NV_ESC_RM_MAP_MEMORY that defines it is issued on the control node.
    /// The fd is registered against the control fd first, as the driver
    /// does for every fd it maps on.
    fn map_fd(&mut self) -> u64 {
        let h = self.open_dev(DeviceKind::Gpu(0));
        self.register_fd(h, self.ctl);
        h
    }

    /// NV_ESC_RM_MAP_MEMORY. Returns (shm_offset, shm_length, pLinearAddress).
    fn map(&mut self, client: u32, dev: u32, mem: u32, len: u64, fd: u64) -> (u64, u64, u64) {
        let mut p = vec![0u8; 56];
        p[0..4].copy_from_slice(&client.to_le_bytes());
        p[4..8].copy_from_slice(&dev.to_le_bytes());
        p[8..12].copy_from_slice(&mem.to_le_bytes());
        p[24..32].copy_from_slice(&len.to_le_bytes());
        // The flags the driver sends for this mapping. Zero is rejected
        // with NV_ERR_INVALID_ARGUMENT; bits 23-25 are the caching type,
        // here 6 (default), which the device resolves to write-combine.
        p[44..48].copy_from_slice(&0x0308_0002u32.to_le_bytes());
        p[48..52].copy_from_slice(&(fd as u32).to_le_bytes());

        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_MAP_MEMORY, 56) as u32,
                data_len: 56,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&p);

        let mut resp = vec![0u8; 4096];
        self.be.dispatch(&req, &mut resp);
        let rh = parse_resp(&resp);
        assert_eq!(rh.status, 0,);
        let body = IOCTL_BODY;
        let out = &resp[body..body + 56];
        let rm = u32::from_le_bytes(out[40..44].try_into().unwrap());
        assert_eq!(rm, 0, "map: RM status {rm:#x}");
        // The SHM offset comes back in pLinearAddress, not in a reply
        // struct: the guest quotes it in a separate Mmap message, and that
        // is where placement and caching are decided.
        let linear = u64::from_le_bytes(out[32..40].try_into().unwrap());
        (linear, len, linear)
    }

    /// Close a device handle, as a guest does when its fd goes away.
    fn close_dev(&mut self, handle: u64) {
        self.cookie += 1;
        let req = close_msg(handle);
        let mut resp = vec![0u8; 128];
        self.be.dispatch(&req, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "close handle {handle}");
    }

    /// NV_ESC_RM_FREE of one object.
    fn free_obj(&mut self, root: u32, parent: u32, object: u32) {
        let mut p = vec![0u8; 16];
        p[0..4].copy_from_slice(&root.to_le_bytes());
        p[4..8].copy_from_slice(&parent.to_le_bytes());
        p[8..12].copy_from_slice(&object.to_le_bytes());
        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_FREE, 16) as u32,
                data_len: 16,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&p);
        let mut resp = vec![0u8; 512];
        self.be.dispatch(&req, &mut resp);
        let rh = parse_resp(&resp);
        assert_eq!(rh.status, 0, "free: transport status, ",);
        let body = IOCTL_BODY;
        let rm = u32::from_le_bytes(resp[body + 12..body + 16].try_into().unwrap());
        assert_eq!(rm, 0, "free of {object:#x}: RM status {rm:#x}");
    }

    /// NV_ESC_RM_UNMAP_MEMORY, keyed by the pLinearAddress the map returned.
    fn unmap(&mut self, client: u32, dev: u32, mem: u32, linear: u64) {
        let mut p = vec![0u8; 32];
        p[0..4].copy_from_slice(&client.to_le_bytes());
        p[4..8].copy_from_slice(&dev.to_le_bytes());
        p[8..12].copy_from_slice(&mem.to_le_bytes());
        p[16..24].copy_from_slice(&linear.to_le_bytes());

        self.cookie += 1;
        let mut req = hdr(MsgType::Ioctl, self.ctl);
        append(
            &mut req,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(abi::ioctl::NV_ESC_RM_UNMAP_MEMORY, 32) as u32,
                data_len: 32,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        req.extend_from_slice(&p);
        let mut resp = vec![0u8; 4096];
        self.be.dispatch(&req, &mut resp);
        let rh = parse_resp(&resp);
        assert_eq!(rh.status, 0,);
        let body = IOCTL_BODY;
        let rm = u32::from_le_bytes(resp[body + 24..body + 28].try_into().unwrap());
        assert_eq!(rm, 0, "unmap: RM status {rm:#x}");
    }
}

#[test]
fn maps_turing_usermode_aperture_for_real() {
    if !nvidiactl_present() {
        return;
    }
    let mut c = Chain::new();
    let (client, sub, mem) = c.usermode_object();
    let fd = c.map_fd();

    let (uc_before, wc_before, wb_before) = c.be.shm_free_bytes();
    let (off, len, linear) = c.map(client, sub, mem, 65536, fd);
    assert_eq!(len, 65536, "mapped length");
    assert_eq!(
        linear, off,
        "pLinearAddress must be the SHM offset the guest sees"
    );
    // Registers are mapped uncached (rmmem.rs), so the extent comes from
    // the uncached zone. That zone starts the window, which makes offset
    // 0 a real answer: the first usermode mapping of a session gets it.
    assert_eq!(
        c.be.shm_free_bytes(),
        (uc_before - 65536, wc_before, wb_before),
        "usermode registers must take 64 KiB of the uncached zone"
    );
    assert!(
        off < ZoneConfig::default_1gib().uc_size,
        "offset {off:#x} outside UC"
    );

    // The SHM window now aliases GPU registers. Reading must not fault.
    let first = c.be.shm_window().read_u32(off);
    eprintln!(
        "usermode {:#x} first dword through SHM: {first:#010x}",
        c.usermode
    );
    // NV_USERMODE_CFG0: the low half is the chip's usermode class, which
    // the memfd behind an unplaced window would read as zero.
    assert!(
        USERMODE_CLASSES.contains(&(first & 0xffff)),
        "the window does not alias the usermode registers (read {first:#x})"
    );

    c.unmap(client, sub, mem, linear);
    assert_eq!(
        c.be.shm_free_bytes(),
        (uc_before, wc_before, wb_before),
        "unmap must return the extent"
    );
    c.be.teardown();
}

/// Closing a device fd must release whatever it was mapping.
///
/// This is the shape of a real CUDA client, which maps 29 times in a run
/// and issues no NV_ESC_RM_UNMAP_MEMORY at all -- the mappings go away
/// because the process exits and its fds close. Releasing only on unmap
/// leaves ~68 MiB of write-combine spent per run for the life of the VM,
/// so a third run has nowhere to map.
#[test]
fn closing_the_fd_releases_its_mapping_without_any_unmap() {
    if !nvidiactl_present() {
        return;
    }
    let mut c = Chain::new();
    let (client, sub, first) = c.usermode_object();
    c.free_obj(client, sub, first);

    let before = c.be.shm_free_bytes();
    for i in 0..50 {
        let class = c.usermode;
        let mem = c.alloc(client, sub, class, &[]);
        let fd = c.map_fd();
        c.map(client, sub, mem, 65536, fd);
        assert_ne!(
            before,
            c.be.shm_free_bytes(),
            "run {i}: mapping took no SHM"
        );

        // Exit the way CUDA does: free the object and drop the fd, with no
        // unmap anywhere.
        c.free_obj(client, sub, mem);
        c.close_dev(fd);

        assert_eq!(
            before,
            c.be.shm_free_bytes(),
            "run {i}: closing the fd did not release its mapping"
        );
    }
    c.be.teardown();
}

/// A hundred map/unmap cycles against the real aperture, asserting the SHM
/// zones end exactly as full as they started.
///
/// **A file descriptor that has carried a mapping cannot carry another.**
/// Reusing one gives NV_ERR_STATE_IN_USE (0x63) on the second
/// NV_ESC_RM_MAP_MEMORY even though the preceding NV_ESC_RM_UNMAP_MEMORY
/// and NV_ESC_RM_FREE both returned NV_OK. The captured driver behaves the
/// same way: it opens a fresh /dev/nvidia0 fd per mapping.
///
/// Ruled out along the way, all on a T4 running 580.178.04: it is not the
/// unmap address (the driver passes back exactly the cookie the map
/// returned, which is what the device does, and passing our own mapping
/// address instead gives NV_ERR_OBJECT_NOT_FOUND); not the node the unmap
/// is issued on (the GPU node returns EINVAL, so the control node is
/// right); and not a leaked RM object (RM_FREE succeeds).
///
/// The consequence for the device is a lifetime rule, not a bug fix: a host
/// fd is single-use for mapping, so one must be opened per mapping and
/// closed when the guest closes its own. This test closes each fd to prove
/// no handle is leaked in the process.
#[test]
fn repeated_map_unmap_does_not_exhaust_the_zone() {
    if !nvidiactl_present() {
        return;
    }
    let mut c = Chain::new();
    let (client, sub, first) = c.usermode_object();
    // The usermode aperture allows one live mapping, so the object built
    // during setup has to go before the loop makes its own.
    c.free_obj(client, sub, first);

    // The usermode aperture permits one mapping per object -- a second map of
    // a still-mapped object is refused with NV_ERR_STATE_IN_USE -- so each
    // cycle allocates its own.
    let before = c.be.shm_free_bytes();
    let handles_before = c.be.handle_count();
    let cycles = 100;
    for i in 0..cycles {
        let class = c.usermode;
        let mem = c.alloc(client, sub, class, &[]);
        let fd = c.map_fd();
        eprintln!("cycle {i}: mem={mem:#x} fd={fd}");
        let (_off, _len, linear) = c.map(client, sub, mem, 65536, fd);
        assert_ne!(
            before,
            c.be.shm_free_bytes(),
            "iteration {i}: mapping took no SHM"
        );
        c.unmap(client, sub, mem, linear);
        c.free_obj(client, sub, mem);
        c.close_dev(fd);
    }
    let after = c.be.shm_free_bytes();
    assert_eq!(
        before, after,
        "{cycles} map/unmap cycles did not return every byte to the zones"
    );
    assert_eq!(
        handles_before,
        c.be.handle_count(),
        "{cycles} cycles leaked host file descriptors"
    );
    c.be.teardown();
}

// ------------------------------------------------------------------
// v1 gating, argument sizing and window ownership (no GPU required)
// ------------------------------------------------------------------

fn devnull() -> OwnedFd {
    crate::sys::fd::open(c"/dev/null", libc::O_RDWR | libc::O_CLOEXEC).unwrap()
}

std::thread_local! {
    /// Commands the fake host driver was handed, on this test's thread.
    static FORWARDED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A host driver that behaves like drm_ioctl and nvidia.ko: it writes
/// back `_IOC_SIZE(request)` bytes, whatever the caller allocated.
fn fake_ioctl(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    FORWARDED.with(|f| f.borrow_mut().push(request));
    // The argument holds at least _IOC_SIZE bytes (sys/block.rs).
    arg.bytes()[..hostfd::ioc_size(request as u32)].fill(0xaa);
    0
}

fn forwarded() -> Vec<u64> {
    FORWARDED.with(|f| std::mem::take(&mut *f.borrow_mut()))
}

fn v1_ioctl(be: &mut NvidiaBackend, handle: u32, cmd: u32, params: &[u8]) -> Vec<u8> {
    let mut req = hdr(MsgType::Ioctl, handle as u64);
    append(
        &mut req,
        &IoctlReq {
            cmd,
            data_len: params.len() as u32,
            ..Default::default()
        },
    );
    req.extend_from_slice(params);
    let mut resp = vec![0u8; 256 + params.len()];
    let n = be.dispatch(&req, &mut resp);
    resp.truncate(n);
    resp
}

fn gated_backend() -> NvidiaBackend {
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(fake_ioctl);
    be
}

#[test]
fn ioctl_buffers_are_sized_for_what_the_host_copies() {
    assert_eq!(
        ioctl_arg_len(hostfd::ioc(hostfd::IOC_RW, b'd', 1, 4096) as u64, 8),
        4096
    );
    assert_eq!(
        ioctl_arg_len(hostfd::ioc(hostfd::IOC_RW, b'F', 1, 16) as u64, 72),
        72
    );
    assert_eq!(
        ioctl_arg_len(0x3000_0001, 0),
        0x3000,
        "UVM numbers carry a size field too"
    );
    assert_eq!(ioctl_arg_len(hostfd::ioc(0, b'd', 0x1f, 0) as u64, 0), 1);
}

/// The heap overflow the sizing fixes: a guest sends 8 bytes with a
/// command whose size field says 8 KiB, and the host writes 8 KiB back.
/// With the buffer sized to the command this is just a call.
#[test]
fn a_small_payload_with_a_large_command_size_cannot_overflow() {
    let mut be = gated_backend();
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    // RM_FREE, pointer-free, with a size field of 8 KiB.
    let cmd = hostfd::ioc(hostfd::IOC_RW, b'F', 0x29, 8 * 1024);
    let resp = v1_ioctl(&mut be, ctl, cmd, &[0u8; 8]);
    assert_eq!(parse_resp(&resp).status, 0);
    assert_eq!(forwarded(), vec![cmd as u64]);
    // Only what the guest sent comes back.
    assert_eq!(&resp[IOCTL_BODY..], &[0xaa; 8]);

    // A UVM number with a size field is no UVM command (uvm_ioctl.h
    // numbers them plainly) and is refused before the host
    // (guestptr.rs).
    let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
    let cmd = hostfd::ioc(hostfd::IOC_RW, 0, 0x21, 8 * 1024);
    let resp = v1_ioctl(&mut be, uvm, cmd, &[0u8; 8]);
    assert_eq!(parse_resp(&resp).status, -libc::EPERM);
    assert!(forwarded().is_empty());
}

std::thread_local! {
    /// The command byte CHECK_VERSION_STR reached the fake host with.
    static VERSION_CMD: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// RM's SYS_PARAMS for a memory block size other than its first
/// caller's (EBUSY), and its CHECK_VERSION_STR (the reply word set).
fn fake_sys_params_busy(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    match (request & 0xff) as u32 {
        abi::ioctl::NV_ESC_SYS_PARAMS => -libc::EBUSY,
        abi::ioctl::NV_ESC_CHECK_VERSION_STR => {
            VERSION_CMD.with(|c| c.set(Some(arg.bytes()[0])));
            arg.bytes()[4] = 1;
            0
        }
        _ => 0,
    }
}

/// SYS_PARAMS's EBUSY reaches the guest as EBUSY, not a made-up
/// success; CHECK_VERSION_STR reaches RM with the guest's own command,
/// so RM compares the guest userspace's version with its own.
#[test]
fn sys_params_and_check_version_go_as_sent_and_come_back_as_answered() {
    use abi::ioctl::{NV_ESC_CHECK_VERSION_STR, NV_ESC_SYS_PARAMS};
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(fake_sys_params_busy);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));

    let sys = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_SYS_PARAMS, 8);
    let resp = v1_ioctl(&mut be, ctl, sys, &(128u64 << 20).to_le_bytes());
    assert_eq!(parse_resp(&resp).status, -libc::EBUSY);

    let check = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_CHECK_VERSION_STR, 72);
    for cmd in [0u8, b'1'] {
        let mut p = vec![0u8; 72];
        p[0] = cmd;
        p[8..17].copy_from_slice(b"595.99.02");
        let resp = v1_ioctl(&mut be, ctl, check, &p);
        assert_eq!(parse_resp(&resp).status, 0);
        assert_eq!(VERSION_CMD.with(|c| c.take()), Some(cmd), "not rewritten");
        assert_eq!(resp[IOCTL_BODY + 4], 1, "the host's reply word");
    }
}

/// CHECK_VERSION_STR that RM fails: the guest reads RM's reply word
/// and its version string with the errno, as nvidia.ko copies the block
/// out on failure, and libnvidia can say which versions disagree
/// EFAULT copies nothing.
#[test]
fn a_failed_call_comes_back_with_the_block_as_the_host_left_it() {
    use abi::ioctl::NV_ESC_CHECK_VERSION_STR;
    fn mismatch(_: RawFd, _: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
        let b = arg.bytes();
        b[4] = 1;
        b[8..72].fill(0);
        b[8..17].copy_from_slice(b"595.99.02");
        -libc::EINVAL
    }
    fn fault(_: RawFd, _: u64, _: &mut crate::sys::block::Arg<'_>) -> i32 {
        -libc::EFAULT
    }
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(mismatch);
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let check = hostfd::ioc(hostfd::IOC_RW, b'F', NV_ESC_CHECK_VERSION_STR, 72);
    let mut p = vec![0u8; 72];
    p[0] = b'1';
    p[8..17].copy_from_slice(b"580.95.05");
    let resp = v1_ioctl(&mut be, ctl, check, &p);
    assert_eq!(parse_resp(&resp).status, -libc::EINVAL);
    assert_eq!(resp[IOCTL_BODY + 4], 1, "RM's reply word");
    assert_eq!(
        &resp[IOCTL_BODY + 8..IOCTL_BODY + 17],
        b"595.99.02",
        "RM's version"
    );
    be.set_host_ioctl_for_test(fault);
    let resp = v1_ioctl(&mut be, ctl, check, &p);
    assert_eq!(parse_resp(&resp).status, -libc::EFAULT);
    assert_eq!(resp.len(), size_of::<MsgHeader>(), "a bare header");
}

/// The DRI section's count is of the records written: a count of every
/// device over fewer records would have the guest read the card section
/// after them as DRI records.
#[test]
fn a_truncated_dri_section_counts_only_what_it_holds() {
    let dev = |name: &str| DriDevice {
        name: name.into(),
        major: 226,
        minor: 128,
        slot_index: 0,
        dev_info: [0; NV_DEV_INFO_WORDS],
        dev_info_size: 0,
    };
    let one = 16 + 4 * NV_DEV_INFO_WORDS + "renderD128".len();
    let mut buf = vec![0u8; 4 + one + 8];
    let n = write_dri_section(&[dev("renderD128"), dev("renderD129")], &mut buf);
    assert_eq!(n, 4 + one);
    assert_eq!(u32::from_le_bytes(buf[..4].try_into().unwrap()), 1);
}

/// Each descriptor field takes its own "none" and the handles it
/// accepts, and nothing else: a number no handle has, one of another
/// kind, or a block too short for the field.
#[test]
fn a_descriptor_field_takes_its_none_and_the_handles_it_accepts() {
    let mut be = gated_backend();
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let gpu = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Gpu(0)));
    let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let field = |accept, none, width| FdField {
        at: 4,
        width,
        accept,
        none,
    };
    let block = |v: u64| {
        let mut b = vec![0xa5u8; 12];
        b[4..12].copy_from_slice(&v.to_le_bytes());
        b
    };
    let got = |be: &NvidiaBackend, f: FdField, v: u64| match be.fd_field(&block(v), f) {
        Ok(FdIn::None) => Ok(None),
        Ok(FdIn::File { handle, .. }) => Ok(Some(handle)),
        Err(e) => Err(e),
    };
    let minus_one = u64::from(u32::MAX);
    let dev = field(FdAccept::Device, FdNone::MinusOne, 4);
    assert_eq!(got(&be, dev, minus_one), Ok(None));
    assert_eq!(got(&be, dev, minus_one - 1), Err(libc::EBADF), "-2");
    assert_eq!(got(&be, dev, gpu.into()), Ok(Some(gpu)));
    assert_eq!(got(&be, dev, ctl.into()), Ok(Some(ctl)));
    assert_eq!(got(&be, dev, render.into()), Err(libc::EBADF));
    assert_eq!(got(&be, dev, 999), Err(libc::EBADF));
    let only_ctl = field(FdAccept::ControlFile, FdNone::MinusOne, 4);
    assert_eq!(got(&be, only_ctl, gpu.into()), Err(libc::EBADF));
    assert_eq!(got(&be, only_ctl, ctl.into()), Ok(Some(ctl)));
    let never = field(FdAccept::Device, FdNone::Never, 4);
    assert_eq!(got(&be, never, minus_one), Err(libc::EBADF));
    let uvm = field(
        FdAccept::Kind(HandleKind::DriRender(0)),
        FdNone::Negative,
        4,
    );
    assert_eq!(got(&be, uvm, minus_one - 4), Ok(None));
    assert_eq!(got(&be, uvm, render.into()), Ok(Some(render)));
    assert_eq!(got(&be, uvm, gpu.into()), Err(libc::EBADF));
    let event = field(FdAccept::Device, FdNone::Zero, 8);
    assert_eq!(got(&be, event, 0), Ok(None));
    assert_eq!(got(&be, event, gpu.into()), Ok(Some(gpu)));
    assert_eq!(got(&be, event, 1 << 32 | u64::from(gpu)), Err(libc::EBADF));
    let past = FdField { at: 9, ..dev };
    assert!(matches!(be.fd_field(&block(0), past), Err(libc::EINVAL)));
    be.teardown();
}

#[test]
fn v1_ioctls_are_refused_on_every_new_handle_kind() {
    let mut be = gated_backend();
    for kind in [
        HandleKind::DrmCard(0),
        HandleKind::DrmLease(0),
        HandleKind::SyncFile,
        HandleKind::Syncobj,
        HandleKind::Dmabuf,
        HandleKind::Eventfd,
        HandleKind::Memfd,
        HandleKind::Wayland,
        HandleKind::Other,
    ] {
        let h = be.adopt_for_test(devnull(), kind);
        // ADDFB2 as a v1 ioctl: the path around every IOCTL2 check.
        let addfb2 = hostfd::ioc(hostfd::IOC_RW, b'd', 0xb8, 104);
        let r = v1_ioctl(&mut be, h, addfb2, &[0u8; 104]);
        assert_eq!(parse_resp(&r).status, -libc::EPERM, "{kind:?}");
    }
    assert!(forwarded().is_empty());
}

/// The nvidia-drm commands the v1 route takes are the IOCTL2 schema's,
/// size included (gen/schema/nvidia_drm.py), where the schema has them.
#[test]
fn the_v1_render_commands_are_the_schemas_numbers() {
    use crate::schema::{Class, DRM_TABLE};
    for cmd in [
        DRM_IOCTL_NVIDIA_GEM_IMPORT_NVKMS_MEMORY,
        DRM_IOCTL_NVIDIA_GEM_EXPORT_NVKMS_MEMORY,
        DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY,
        DRM_IOCTL_NVIDIA_GEM_EXPORT_DMABUF_MEMORY,
    ] {
        let e = DRM_TABLE.lookup(Class::Render, cmd).expect("in the schema");
        assert_eq!(e.cmd, cmd, "{}", e.name);
        assert_eq!(e.size as usize, hostfd::ioc_size(cmd), "{}", e.name);
    }
    // Refused by absence from IOCTL2, which the v1 route still takes.
    assert!(
        DRM_TABLE
            .lookup(Class::Render, hostfd::DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET)
            .is_none()
    );
    assert!(
        DRM_TABLE
            .lookup(Class::Render, hostfd::DRM_IOCTL_GEM_CLOSE)
            .is_none()
    );
}

#[test]
fn a_render_handle_takes_only_the_v1_drm_calls_the_guest_sends() {
    let mut be = gated_backend();
    let render = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let refused = [
        // GEM_IMPORT_USERSPACE_MEMORY, GEM_FLINK, GEM_OPEN: never.
        (hostfd::ioc(hostfd::IOC_RW, b'd', 0x42, 24), 24, libc::EPERM),
        (hostfd::DRM_IOCTL_GEM_FLINK, 8, libc::EPERM),
        (hostfd::DRM_IOCTL_GEM_OPEN, 16, libc::EPERM),
        // An RM escape on a DRM file.
        (hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32), 32, libc::EPERM),
        // A core KMS ioctl.
        (hostfd::ioc(hostfd::IOC_RW, b'd', 0xa0, 64), 64, libc::EPERM),
        // A known command with a size that disagrees with it.
        (hostfd::DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET, 8, libc::EINVAL),
    ];
    for (cmd, len, errno) in refused {
        let r = v1_ioctl(&mut be, render, cmd, &vec![0u8; len]);
        assert_eq!(parse_resp(&r).status, -errno, "cmd {cmd:#x}");
    }
    assert!(forwarded().is_empty(), "nothing refused reached the host");

    for cmd in [
        hostfd::DRM_IOCTL_GEM_CLOSE,
        hostfd::DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET,
        DRM_IOCTL_NVIDIA_GEM_ALLOC_NVKMS_MEMORY,
    ] {
        let r = v1_ioctl(&mut be, render, cmd, &vec![0u8; hostfd::ioc_size(cmd)]);
        assert_eq!(parse_resp(&r).status, 0, "cmd {cmd:#x}");
    }
    assert_eq!(forwarded().len(), 3);
}

#[test]
fn each_nvidia_device_takes_only_its_own_namespace() {
    let mut be = gated_backend();
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let modeset = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Modeset));
    let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
    let d = hostfd::ioc(hostfd::IOC_RW, b'd', 0x4b, 24);
    let m = hostfd::ioc(hostfd::IOC_RW, b'm', 0, 16);
    // RM's frontend and nvidia-modeset both dispatch on the number alone,
    // so a foreign type byte would reach them as one of their own
    // commands, unchecked. Each is refused with the device's own answer
    // to a command it does not know.
    for (h, cmd, errno) in [
        (ctl, d, libc::EINVAL),
        (ctl, m, libc::EINVAL),
        (modeset, d, libc::ENOTTY),
        (uvm, m, libc::ENOSYS),
        (ctl, 0x3000_0001, libc::EINVAL),
    ] {
        let r = v1_ioctl(&mut be, h, cmd, &[0u8; 24]);
        assert_eq!(parse_resp(&r).status, -errno, "handle {h} cmd {cmd:#x}");
    }
    assert!(forwarded().is_empty());
}

/// Records what the window was asked to do.
#[derive(Clone, Default)]
struct FakeWindow(Arc<std::sync::Mutex<Vec<(&'static str, u64)>>>);

impl crate::shm::WindowPlacer for FakeWindow {
    fn place(&self, off: u64, _len: u64, _fd: RawFd, _fo: u64, _w: bool) -> Result<()> {
        self.0.lock().unwrap().push(("place", off));
        Ok(())
    }
    fn withdraw(&self, off: u64, _len: u64) -> Result<()> {
        self.0.lock().unwrap().push(("withdraw", off));
        Ok(())
    }
}

fn mmap(be: &mut NvidiaBackend, handle: u32, offset: u64) -> MmapResp {
    let mut req = hdr(MsgType::Mmap, handle as u64);
    append(
        &mut req,
        &MmapReq {
            size: 4096,
            offset,
            prot: 3,
            padding: 0,
        },
    );
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert_eq!(parse_resp(&resp).status, 0, "mmap");
    read_struct::<MmapResp>(&resp, size_of::<MsgHeader>())
}

fn munmap(be: &mut NvidiaBackend, id: u32) {
    let mut req = hdr(MsgType::Munmap, 0);
    append(
        &mut req,
        &MunmapReq {
            mapping_id: id,
            padding: 0,
        },
    );
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert_eq!(parse_resp(&resp).status, 0, "munmap");
}

/// R:internals §11.1: a proxy that outlives the file that placed it keeps
/// its placement; the extent is withdrawn and freed once, by the last
/// MUNMAP, and never handed to anyone else while it is mapped.
#[test]
fn a_placement_outlives_its_owner_and_is_freed_exactly_once() {
    let mut be = gated_backend();
    let window = FakeWindow::default();
    be.set_window(Box::new(window.clone()));
    let empty = be.shm_free_bytes();
    let owner = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let placed = mmap(&mut be, owner, 0x10000);
    assert_ne!(placed.mapping_id, 0);
    let held = be.shm_free_bytes();
    assert_ne!(held, empty);

    // The client exits: its file closes, its buffer lives on elsewhere.
    be.dispatch(&close_msg(owner as u64), &mut [0u8; 32]);
    assert_eq!(
        be.shm_free_bytes(),
        held,
        "closing the owner freed a mapped extent"
    );
    assert!(
        !window
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(op, _)| *op == "withdraw")
    );

    // Someone else maps meanwhile: a different extent, not the live one.
    let other = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let second = mmap(&mut be, other, 0x10000);
    assert_ne!(second.guest_phys_addr, placed.guest_phys_addr);

    // The proxy goes: one withdraw, one free, of its own extent only.
    munmap(&mut be, placed.mapping_id);
    munmap(&mut be, placed.mapping_id);
    let withdrawn: Vec<u64> = window
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|(op, _)| *op == "withdraw")
        .map(|(_, off)| *off)
        .collect();
    assert_eq!(withdrawn, vec![placed.guest_phys_addr]);
    munmap(&mut be, second.mapping_id);
    assert_eq!(be.shm_free_bytes(), empty);
}

#[test]
fn the_same_object_mapped_twice_is_released_by_the_second_munmap() {
    let mut be = gated_backend();
    be.set_window(Box::new(FakeWindow::default()));
    let empty = be.shm_free_bytes();
    let h = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let a = mmap(&mut be, h, 0x2000);
    let b = mmap(&mut be, h, 0x2000);
    assert_eq!(a.mapping_id, b.mapping_id);
    munmap(&mut be, a.mapping_id);
    assert_ne!(be.shm_free_bytes(), empty, "still mapped once");
    munmap(&mut be, b.mapping_id);
    assert_eq!(be.shm_free_bytes(), empty);
}

/// A placement mapped u32::MAX times is refused one more MMAP, not
/// counted past it: the count is the guest kernel's to raise, and an
/// overflow would abort the backend.
#[test]
fn a_placement_mapped_u32_max_times_is_refused_another() {
    let mut be = gated_backend();
    be.set_window(Box::new(FakeWindow::default()));
    let h = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    let a = mmap(&mut be, h, 0x2000);
    be.live_maps.get_mut(&a.mapping_id).unwrap().refs = u32::MAX;
    let mut req = hdr(MsgType::Mmap, h as u64);
    append(
        &mut req,
        &MmapReq {
            size: 4096,
            offset: 0x2000,
            prot: 3,
            padding: 0,
        },
    );
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert_eq!(parse_resp(&resp).status, -libc::ENOMEM);
    assert_eq!(be.live_maps[&a.mapping_id].refs, u32::MAX);
}

/// An MMAP of a file already placed, asking for more than the placement
/// holds, is refused: the guest maps what it asked for from the
/// placement's offset, so the rest would be the window's next extents --
/// another guest process's -- or unplaced window. The control file's
/// ALLOC_MEMORY mappings take this path (nothing records them).
#[test]
fn a_second_mmap_larger_than_the_placement_is_refused() {
    let mut be = gated_backend();
    be.set_window(Box::new(FakeWindow::default()));
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let first = mmap(&mut be, ctl, 0);
    let ask = |be: &mut NvidiaBackend, size: u64| {
        let mut req = hdr(MsgType::Mmap, ctl as u64);
        append(
            &mut req,
            &MmapReq {
                size,
                offset: 0,
                prot: 3,
                padding: 0,
            },
        );
        let mut resp = vec![0u8; 64];
        be.dispatch(&req, &mut resp);
        (
            parse_resp(&resp).status,
            read_struct::<MmapResp>(&resp, size_of::<MsgHeader>()),
        )
    };
    let (st, _) = ask(&mut be, 64 << 20);
    assert_eq!(st, -libc::EINVAL, "past the placement");
    // The same size, or less, is the same placement.
    let (st, again) = ask(&mut be, 4096);
    assert_eq!(st, 0);
    assert_eq!(
        (again.guest_phys_addr, again.mapping_id, again.size),
        (first.guest_phys_addr, first.mapping_id, 4096)
    );
    let (st, _) = ask(&mut be, 100);
    assert_eq!(st, 0);
}

#[test]
fn a_session_reset_releases_every_placement_and_handle() {
    let mut be = gated_backend();
    let window = FakeWindow::default();
    be.set_window(Box::new(window.clone()));
    let empty = be.shm_free_bytes();
    let h = be.adopt_for_test(devnull(), HandleKind::DriRender(0));
    mmap(&mut be, h, 0x3000);
    be.session_reset("test");
    assert_eq!(be.shm_free_bytes(), empty);
    assert_eq!(be.handle_count(), 0);
    assert_eq!(window.0.lock().unwrap().last().unwrap().0, "withdraw");
}

#[test]
fn mmap_is_refused_on_handles_that_carry_no_device_memory() {
    let mut be = gated_backend();
    be.set_window(Box::new(FakeWindow::default()));
    let memfd = be.adopt_for_test(devnull(), HandleKind::Memfd);
    let mut req = hdr(MsgType::Mmap, memfd as u64);
    append(
        &mut req,
        &MmapReq {
            size: 4096,
            offset: 0,
            prot: 3,
            padding: 0,
        },
    );
    let mut resp = vec![0u8; 64];
    be.dispatch(&req, &mut resp);
    assert_eq!(parse_resp(&resp).status, -libc::EPERM);
}

#[test]
fn opening_a_card_node_directly_is_refused() {
    let mut be = gated_backend();
    let mut resp = vec![0u8; 64];
    be.dispatch(&open_msg(DeviceKind::DriCard(0)), &mut resp);
    assert_eq!(parse_resp(&resp).status, -libc::EPERM);
}

#[test]
fn responses_echo_the_request_id() {
    let mut be = gated_backend();
    let mut req = close_msg(0xCAFE);
    req[12..16].copy_from_slice(&0x1234u32.to_le_bytes());
    let mut resp = vec![0u8; 32];
    be.dispatch(&req, &mut resp);
    assert_eq!(parse_resp(&resp).req_id, 0x1234);
}

/// A probe buffer as the host leaves it: its own struct over the front,
/// the filler after.
fn probe_with(host: &[u32]) -> [u32; NV_DEV_INFO_PROBE_WORDS] {
    let mut p = [DEV_INFO_UNWRITTEN; NV_DEV_INFO_PROBE_WORDS];
    p[..host.len()].copy_from_slice(host);
    p
}

#[test]
fn the_get_dev_info_probe_asks_with_a_64_byte_size_field() {
    assert_eq!(DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE, 0xC040_6443);
    assert_eq!(
        hostfd::ioc_size(DRM_IOCTL_NVIDIA_GET_DEV_INFO_PROBE as u32),
        64
    );
}

#[test]
fn the_host_layout_is_measured_from_where_the_filler_starts() {
    // 535: gpu_id, primary_index, kind, generation, sector layout -- with
    // a zero in the middle, which is still a written word.
    assert_eq!(dev_info_host_size(&probe_with(&[0x100, 0, 6, 2, 1])), 20);
    assert_eq!(
        dev_info_host_size(&probe_with(&[0x100, 1, 6, 2, 1, 0, 0])),
        28
    );
    assert_eq!(
        dev_info_host_size(&probe_with(&[0x100, 1, 1, 6, 2, 1, 1, 1])),
        32
    );
    assert_eq!(
        dev_info_host_size(&probe_with(&[0x100, 0, 1, 1, 6, 2, 1, 0, 0])),
        36
    );
    assert_eq!(dev_info_host_size(&probe_with(&[])), 0);
}

#[test]
fn a_535_answer_is_not_read_as_the_610_layout() {
    let raw = probe_with(&[0x100, 1, 6, 2, 1]);
    // The old reading: primary_index as mig_device, the page kind as
    // supports_alloc, the generation as the page kind.
    let info = normalise_dev_info(&raw, 20, true).unwrap();
    assert_eq!(info, [0x100, 0, 1, 1, 6, 2, 1, 0, 0]);
}

#[test]
fn supports_alloc_comes_from_dmabuf_supported_where_the_layout_lacks_it() {
    let raw = probe_with(&[0x100, 1, 6, 2, 1]);
    assert_eq!(normalise_dev_info(&raw, 20, false).unwrap()[3], 0);
    let raw = probe_with(&[0x100, 1, 6, 2, 1, 1, 1]);
    assert_eq!(
        normalise_dev_info(&raw, 28, true).unwrap(),
        [0x100, 0, 1, 1, 6, 2, 1, 1, 1]
    );
}

#[test]
fn a_32_byte_answer_keeps_its_own_supports_alloc() {
    // modeset=0 on 550: supports_alloc false and every kind zero, whatever
    // DMABUF_SUPPORTED would say.
    let raw = probe_with(&[0x100, 1, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        normalise_dev_info(&raw, 32, true).unwrap(),
        [0x100, 0, 1, 0, 0, 0, 0, 0, 0]
    );
}

#[test]
fn a_36_byte_answer_passes_through_unchanged() {
    let host = [0x100, 3, 1, 1, 6, 2, 1, 1, 1];
    assert_eq!(
        normalise_dev_info(&probe_with(&host), 36, false).unwrap(),
        host
    );
}

#[test]
fn an_unknown_layout_is_not_guessed_at() {
    let raw = probe_with(&[1; 10]);
    assert_eq!(normalise_dev_info(&raw, 40, true), None);
    assert_eq!(normalise_dev_info(&raw, 0, true), None);
}

#[test]
fn the_dev_info_sizes_follow_the_card_section_in_dri_order() {
    let dri = |size| DriDevice {
        name: "renderD128".into(),
        major: 226,
        minor: 128,
        slot_index: 0,
        dev_info: [0; NV_DEV_INFO_WORDS],
        dev_info_size: size,
    };
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(vec![dri(20), dri(36)], Vec::new());
    let mut buf = vec![0u8; 1 << 20];
    let n = be.handle_get_files(FileTree::Sys, &mut buf);
    let tail: Vec<u32> = buf[n - 12..n]
        .chunks(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(tail, [2, 20, 36]);
    // ...and the empty card section is right before them.
    assert_eq!(read_struct::<u32>(&buf, n - 16), 0);
    assert_eq!(write_dev_info_sizes(&[dri(20)], &mut [0u8; 7]), 0);
}

/// A launcher's snapshot of the whole config space extends the 64 bytes
/// an unprivileged read gets, only if it is of the same device.
#[test]
fn a_pci_config_snapshot_extends_the_live_header_of_its_own_device() {
    let mut live = vec![0u8; 64];
    live[0..4].copy_from_slice(&[0xde, 0x10, 0x85, 0x2b]);
    live[8..12].copy_from_slice(&[0xa1, 0, 0, 3]);
    live[0x2c..0x30].copy_from_slice(&[0x43, 0x10, 0x11, 0x22]);
    live[0x34] = 0x60;
    let mut snap = live.clone();
    snap[4] = 0xff; // a status bit that moved since: the live one wins
    snap.resize(4096, 0);
    snap[0x60] = 0x10; // the PCIe capability
    snap[0x100] = 0x01; // an extended capability
    let m = merge_pci_config(&live, &snap).unwrap();
    assert_eq!(m.len(), 4096);
    assert_eq!(&m[..64], &live[..]);
    assert_eq!((m[0x60], m[0x100]), (0x10, 0x01));
    let mut other = snap.clone();
    other[2] = 0x86;
    assert_eq!(merge_pci_config(&live, &other), None, "another device");
    assert_eq!(merge_pci_config(&live, &snap[..64]), None, "nothing more");
    assert_eq!(merge_pci_config(&live, &vec![0u8; 8192]), None, "too long");
    assert_eq!(
        merge_pci_config(&snap, &snap),
        None,
        "the live read was whole"
    );
}

#[test]
fn the_card_section_is_written_whole_or_not_at_all() {
    let cards = vec![CardNode {
        name: "card1".into(),
        major: 226,
        minor: 1,
        render_index: 0,
    }];
    let mut buf = vec![0u8; 64];
    let n = write_card_section(&cards, &mut buf);
    assert_eq!(n, 4 + 16 + 5);
    assert_eq!(u32::from_le_bytes(buf[0..4].try_into().unwrap()), 1);
    let rec = read_struct::<CardRecord>(&buf, 4);
    assert_eq!(
        (rec.name_len, rec.major, rec.minor, rec.render_index),
        (5, 226, 1, 0)
    );
    assert_eq!(&buf[20..25], b"card1");
    assert_eq!(write_card_section(&cards, &mut [0u8; 10]), 0);
}

/// Plain Wayland mode needs the host card numbers too (the devmap), so
/// GET_SYS_FILES carries them without --kms-card; only BCAP_KMS_CARD
/// makes them openable.
#[test]
fn get_sys_files_names_the_cards_in_every_mode() {
    for kms_card in [false, true] {
        let mut be = NvidiaBackend::for_test();
        be.config.kms_card = kms_card;
        be.set_host_nodes_for_test(
            Vec::new(),
            vec![CardNode {
                name: "card1".into(),
                major: 226,
                minor: 1,
                render_index: 0,
            }],
        );
        let mut buf = vec![0u8; 1 << 20];
        let n = be.handle_get_files(FileTree::Sys, &mut buf);
        // The file stream, up to its terminator.
        let mut off = 0;
        loop {
            let e = read_struct::<FileEntry>(&buf, off);
            off += size_of::<FileEntry>();
            if e.path_len == 0 && e.content_len == 0 {
                break;
            }
            off += (e.path_len + e.content_len) as usize;
        }
        assert_eq!(read_struct::<u32>(&buf, off), 0, "no DRI records");
        off += 4;
        assert_eq!(read_struct::<u32>(&buf, off), 1, "kms_card {kms_card}");
        let rec = read_struct::<CardRecord>(&buf, off + 4);
        assert_eq!((rec.major, rec.minor), (226, 1));
        off += 4 + 16 + 5;
        assert_eq!(read_struct::<u32>(&buf, off), 0, "no GET_DEV_INFO sizes");
        assert_eq!(n, off + 4);
    }
}

/// RM never writes UPDATE_DEVICE_MAPPING_INFO's pOld/pNew, so the
/// caller reads back what it passed -- not zero, and not the host
/// address the backend put there for the call (L-6).
#[test]
fn update_device_mapping_info_gives_the_callers_addresses_back() {
    let mut be = gated_backend();
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    // NVOS56 {hClient, hDevice, hMemory, pad, pOld, pNew, status, pad}.
    let mut p = [0u8; 40];
    p[16..24].copy_from_slice(&0x7f00_1000u64.to_le_bytes());
    p[24..32].copy_from_slice(&0x7f00_2000u64.to_le_bytes());
    let cmd = hostfd::ioc(hostfd::IOC_RW, b'F', 0x5e, 40);
    let resp = v1_ioctl(&mut be, ctl, cmd, &p);
    assert_eq!(parse_resp(&resp).status, 0);
    assert_eq!(forwarded(), vec![cmd as u64]);
    let body = &resp[IOCTL_BODY..];
    assert_eq!(&body[16..32], &p[16..32], "the caller's own pOld and pNew");
    assert_eq!(&body[32..36], &[0xaa; 4], "the host's status");
}

std::thread_local! {
    /// The descriptor the fake UVM was handed in MM_INITIALIZE.
    static UVM_FD_SEEN: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
}

fn fake_uvm(_fd: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    if request == 75 {
        // MM_INITIALIZE's 8 bytes.
        let v = i32::from_le_bytes(arg.bytes()[..4].try_into().unwrap());
        UVM_FD_SEEN.with(|s| s.set(Some(v)));
    }
    0
}

/// UVM fgets MM_INITIALIZE's uvmFd in the calling process: the handle
/// the guest driver sent must reach it as our descriptor of that UVM
/// file, come back as the handle, and name nothing else.
#[test]
fn a_uvm_descriptor_field_reaches_the_host_as_our_descriptor_of_that_file() {
    use std::os::fd::AsRawFd;
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    be.set_host_ioctl_for_test(fake_uvm);
    be.set_host_driver_version("610.57.04");
    let primary_fd = devnull();
    let primary_raw = primary_fd.as_raw_fd();
    let primary = be.adopt_for_test(primary_fd, HandleKind::Dev(DeviceKind::Uvm));
    let second = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));

    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&primary.to_le_bytes());
    let resp = v1_ioctl(&mut be, second, 75, &p);
    assert_eq!(parse_resp(&resp).status, 0);
    assert_eq!(UVM_FD_SEEN.with(|s| s.take()), Some(primary_raw));
    assert_eq!(
        &resp[IOCTL_BODY..IOCTL_BODY + 4],
        &primary.to_le_bytes(),
        "the handle comes back, never our descriptor"
    );

    // An RM control file is not a UVM file, and a number that is no
    // handle of ours is not passed on as one.
    for bad in [ctl, 0x7777] {
        p[..4].copy_from_slice(&bad.to_le_bytes());
        let resp = v1_ioctl(&mut be, second, 75, &p);
        assert_eq!(parse_resp(&resp).status, -libc::EBADF);
        assert_eq!(UVM_FD_SEEN.with(|s| s.take()), None);
    }

    // -1 is UVM's "none", and names nothing anywhere.
    p[..4].copy_from_slice(&(-1i32).to_le_bytes());
    v1_ioctl(&mut be, second, 75, &p);
    assert_eq!(UVM_FD_SEEN.with(|s| s.take()), Some(-1));
}

/// UVM copies exactly sizeof its parameters each way, and its numbers
/// say nothing of that size: a block must be the size the host's release
/// has for the command, and a command the release lacks goes nowhere.
#[test]
fn a_uvm_block_must_be_the_size_the_hosts_release_copies() {
    let uvm_backend = |version: &str| {
        let mut be = NvidiaBackend::for_test();
        be.set_host_nodes_for_test(Vec::new(), Vec::new());
        be.set_host_ioctl_for_test(fake_uvm);
        be.set_host_driver_version(version);
        let h = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
        (be, h)
    };
    // UVM_FREE lost its length in 590.44.01: 24 bytes before, 16 after.
    let (mut be, uvm) = uvm_backend("610.57.04");
    for (len, want) in [(16, 0), (24, -libc::EINVAL), (8, -libc::EINVAL)] {
        let r = v1_ioctl(&mut be, uvm, 34, &vec![0u8; len]);
        assert_eq!(parse_resp(&r).status, want, "FREE, {len} bytes on 610");
    }
    // UVM_INITIALIZE's number says 0x3000; its block is 16 bytes.
    let r = v1_ioctl(&mut be, uvm, 0x3000_0001, &[0u8; 0x3000]);
    assert_eq!(parse_resp(&r).status, -libc::EINVAL);
    let r = v1_ioctl(&mut be, uvm, 0x3000_0001, &[0u8; 16]);
    assert_eq!(parse_resp(&r).status, 0);

    let (mut be, uvm) = uvm_backend("580.95.05");
    let r = v1_ioctl(&mut be, uvm, 34, &[0u8; 24]);
    assert_eq!(parse_resp(&r).status, 0, "FREE, 24 bytes on 580");
    // DISCARD (80) came in 580.65.06; 535 has no such command.
    let (mut be, uvm) = uvm_backend("535.129.03");
    let r = v1_ioctl(&mut be, uvm, 80, &[0u8; 32]);
    assert_eq!(parse_resp(&r).status, -libc::ENOSYS, "UVM's own answer");
    // And a host with no table refuses them all.
    let mut be = NvidiaBackend::for_test();
    be.set_host_ioctl_for_test(fake_uvm);
    let uvm = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Uvm));
    let r = v1_ioctl(&mut be, uvm, 34, &[0u8; 16]);
    assert_eq!(parse_resp(&r).status, -libc::ENOSYS);
}

/// S-33: a display file's handle-table descriptor is closed by the
/// closer, not by the queue thread in CLOSE.
#[test]
fn closing_a_card_handle_leaves_the_last_close_to_the_closer() {
    let mut be = gated_backend();
    let (r, w) = crate::sys::fd::pipe2(libc::O_CLOEXEC).unwrap();
    let card = be.adopt_for_test(r, HandleKind::DrmCard(0));
    be.close_handle(card).unwrap();
    assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
    assert!(
        crate::sys::fd::write(&w, b"x").is_err(),
        "the read end is closed"
    );
}

/// S-24: a control that lists the host's GPU processes never reaches
/// RM; the caller reads RM's own "insufficient permissions" and its
/// parameters back as sent.
#[test]
fn a_control_listing_host_pids_is_answered_without_rm() {
    let mut be = gated_backend();
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let control = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32);
    let mut p = [0u8; 32];
    p[8..12].copy_from_slice(&0x2080_018du32.to_le_bytes());
    let resp = v1_ioctl(&mut be, ctl, control, &p);
    assert_eq!(parse_resp(&resp).status, 0, "the call itself succeeds");
    assert!(forwarded().is_empty(), "RM never saw it");
    let body = &resp[IOCTL_BODY..];
    assert_eq!(
        &body[28..32],
        &crate::nvos::NV_ERR_INSUFFICIENT_PERMISSIONS.to_le_bytes()
    );
    assert_eq!(&body[..28], &p[..28]);
    assert!(be.rm_controls.is_empty());
}

/// S-17: the tallies are keyed by the guest's u32s, so only what RM
/// served is counted -- a guest walking the command space adds nothing.
#[test]
fn only_controls_and_classes_rm_served_are_tallied() {
    let mut be = NvidiaBackend::for_test();
    be.set_host_nodes_for_test(Vec::new(), Vec::new());
    crate::testing::rm::install(&mut be);
    crate::testing::rm::with(|rm| {
        rm.only_controls(&[0x2080_0101]);
        rm.only_classes(&[0x3e]);
    });
    let ctl = be.adopt_for_test(devnull(), HandleKind::Dev(DeviceKind::Ctl));
    let control = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2a, 32);
    let alloc = hostfd::ioc(hostfd::IOC_RW, b'F', 0x2b, 48);
    let mut p = [0u8; 48];
    p[12..16].copy_from_slice(&NV01_ROOT_CLIENT.to_le_bytes());
    let r = v1_ioctl(&mut be, ctl, alloc, &p);
    let client = crate::le::u32_at(&r, 16 + 12 + 8).unwrap();
    let block = |at: usize, v: u32, len: usize| {
        let mut p = vec![0u8; len];
        p[..4].copy_from_slice(&client.to_le_bytes());
        p[4..8].copy_from_slice(&client.to_le_bytes());
        p[at..at + 4].copy_from_slice(&v.to_le_bytes());
        p
    };
    for k in 0..200u32 {
        v1_ioctl(&mut be, ctl, control, &block(8, 0x1234_0000 + k, 32));
        v1_ioctl(&mut be, ctl, alloc, &block(12, 0x9000 + k, 48));
    }
    // Refused by RM, named by nothing it answered: only the client counts.
    assert!(be.rm_controls.is_empty());
    assert_eq!(be.rm_classes.get(NV01_ROOT_CLIENT), Some(1));
    assert_eq!(be.rm_classes.len(), 1);

    v1_ioctl(&mut be, ctl, control, &block(8, 0x2080_0101, 32));
    v1_ioctl(&mut be, ctl, alloc, &block(12, 0x3e, 48));
    assert_eq!(be.rm_controls.get(0x2080_0101), Some(1));
    assert_eq!(be.rm_classes.get(0x3e), Some(1));
    assert_eq!((be.rm_controls.len(), be.rm_classes.len()), (1, 2));
}
