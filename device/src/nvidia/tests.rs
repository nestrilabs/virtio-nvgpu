//! Unit tests for the backend. Those that need /dev/nvidiactl skip without it.

use super::*;

#[cfg(test)]
mod abi_tests {
    use super::*;
    use abi::ioctl::*;

    /// The exact reply the Tesla T4 gave to NV_ESC_CHECK_VERSION_STR on driver
    /// 580.178.04, taken from gen/fixtures. Using the captured bytes rather
    /// than a hand-built buffer keeps the parser honest about real padding.
    fn t4_version_reply() -> Vec<u8> {
        let mut b = vec![0u8; 72];
        b[4] = 1; // reply = 1
        b[8..18].copy_from_slice(b"580.178.04");
        b
    }

    fn backend() -> NvidiaBackend {
        NvidiaBackend::with_default_zones()
    }

    #[test]
    fn no_profile_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn uvm_initialize_always_gets_sharing_mode_and_no_hmm() {
        use abi::version::DriverVersion as V;
        // 595 takes 0x1 and 0x2 (mask 0x3); 610.43.02 adds 0x4.
        assert_eq!(uvm_init_flags(V::new(595, 104, 2)), 0x3);
        assert_eq!(uvm_init_flags(V::new(580, 178, 4)), 0x3);
        assert_eq!(uvm_init_flags(V::new(610, 43, 1)), 0x3);
        assert_eq!(uvm_init_flags(V::new(610, 43, 2)), 0x7);
        assert_eq!(uvm_init_flags(V::new(615, 71, 9)), 0x7);
    }

    #[test]
    fn learns_the_driver_version_from_a_real_reply() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert_eq!(
            b.driver,
            Some(abi::version::DriverVersion::new(580, 178, 4))
        );
        assert!(b.abi.is_some(), "580.178.04 must select a profile");
    }

    /// The property the tables exist for: an escape nobody described does not
    /// reach the host driver. This is the check that was a log line until the
    /// question was asked in public.
    #[test]
    fn an_escape_outside_the_profile_is_refused() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert!(b.abi.is_some());

        // 0x7f is not an NVIDIA escape and is in no profile.
        assert_eq!(b.check_abi(0x7f, 16), AbiCheck::UnknownEscape);

        // And a size the host does not agree with, on an escape that exists.
        assert!(matches!(
            b.check_abi(NV_ESC_RM_CONTROL, 31),
            AbiCheck::SizeMismatch { .. }
        ));
    }

    /// Before CHECK_VERSION_STR is answered there is no profile to check
    /// against, and refusing then would refuse the call that establishes one.
    #[test]
    fn nothing_is_refused_before_the_version_is_known() {
        let b = backend();
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn accepts_the_sizes_the_t4_actually_sent() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        for (escape, size) in [
            (NV_ESC_RM_CONTROL, 32),
            (NV_ESC_RM_ALLOC, 48),
            (NV_ESC_RM_FREE, 16),
            (NV_ESC_RM_MAP_MEMORY, 56),
            (NV_ESC_RM_MAP_MEMORY_DMA, 64),
            (NV_ESC_RM_UNMAP_MEMORY_DMA, 48),
            (NV_ESC_RM_VID_HEAP_CONTROL, 184),
        ] {
            assert_eq!(
                b.check_abi(escape, size),
                AbiCheck::Ok,
                "escape {escape:#04x} at {size} bytes was captured from hardware"
            );
        }
    }

    #[test]
    fn catches_the_stale_map_memory_dma_size() {
        // The hand-written table had this at 48; 580 uses NVOS46_PARAMETERS_V580,
        // which is 64. This is the bug the ABI check exists to catch.
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        assert_eq!(
            b.check_abi(NV_ESC_RM_MAP_MEMORY_DMA, 48),
            AbiCheck::SizeMismatch {
                expected: 64,
                actual: 48
            }
        );
    }

    #[test]
    fn variable_length_escapes_are_not_size_checked() {
        let mut b = backend();
        b.learn_driver_version(&t4_version_reply());
        // CARD_INFO is an array; the T4 sent 2304 bytes in one call.
        assert_eq!(
            b.check_abi(NV_ESC_CARD_INFO, 2304),
            AbiCheck::VariableLength
        );
    }

    #[test]
    fn a_garbled_version_string_leaves_the_backend_unconfigured() {
        let mut b = backend();
        let mut junk = vec![0u8; 72];
        junk[8..12].copy_from_slice(b"oops");
        b.learn_driver_version(&junk);
        assert!(b.driver.is_none());
        assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
    }

    #[test]
    fn a_short_reply_is_ignored_rather_than_panicking() {
        let mut b = backend();
        b.learn_driver_version(&[0u8; 4]);
        assert!(b.driver.is_none());
    }
}

#[cfg(test)]
mod tests {
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
                padding: 0,
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

    /// A complete `Ioctl` message with no nested block.
    #[allow(dead_code)]
    fn ioctl_msg(handle: u64, escape: u32, params: &[u8]) -> Vec<u8> {
        let mut v = hdr(MsgType::Ioctl, handle);
        append(
            &mut v,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(escape, params.len() as u32) as u32,
                data_len: params.len() as u32,
                nested_offset: 0,
                nested_len: 0,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        v.extend_from_slice(params);
        v
    }

    fn append<T: Copy>(v: &mut Vec<u8>, val: &T) {
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
    fn is_err(buf: &[u8], want: Status) -> bool {
        parse_resp(buf).status == -want.errno()
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
        assert!(is_err(&resp, Status::InvalidDevice));
    }

    #[test]
    fn uvm_is_not_opened_without_compute() {
        let mut be = NvidiaBackend::for_test();
        assert!(!be.caps().has(crate::caps::COMPUTE), "compute is opt-in");
        for kind in [DeviceKind::Uvm, DeviceKind::UvmTools] {
            let mut resp = vec![0u8; 64];
            be.dispatch(&open_msg(kind), &mut resp);
            assert_eq!(parse_resp(&resp).status, -libc::ENODEV, "{kind:?}");
        }
        assert_eq!(be.handles.len(), 0);
    }

    /// The tools device pins user buffers and copies through process memory.
    /// No capability serves it.
    #[test]
    fn uvm_tools_is_refused_even_with_compute() {
        let mut be = NvidiaBackend::for_test();
        be.set_caps(crate::caps::Caps::parse("compute,graphics,video,utility").unwrap());
        let mut resp = vec![0u8; 64];
        be.dispatch(&open_msg(DeviceKind::UvmTools), &mut resp);
        assert_eq!(parse_resp(&resp).status, -libc::ENODEV);
        assert_eq!(be.handles.len(), 0);
    }

    #[test]
    fn modeset_and_render_nodes_need_graphics() {
        let mut be = NvidiaBackend::for_test();
        be.set_caps(crate::caps::Caps::parse("compute").unwrap());
        for kind in [DeviceKind::Modeset, DeviceKind::Dri(0)] {
            let mut resp = vec![0u8; 64];
            be.dispatch(&open_msg(kind), &mut resp);
            assert_eq!(parse_resp(&resp).status, -libc::ENODEV, "{kind:?}");
        }
        assert_eq!(be.handles.len(), 0);
    }

    /// A class outside the guest's capabilities is answered the way RM answers
    /// a class the GPU lacks, without calling the host. The handle is
    /// /dev/null: reaching it would fail the call with ENOTTY, so a pass also
    /// shows the host was never asked.
    #[test]
    fn an_alloc_outside_the_caps_gets_invalid_class_from_us() {
        let mut be = NvidiaBackend::for_test();
        be.set_caps(crate::caps::Caps::parse("graphics,compute").unwrap());
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        let h = be.handles.insert(OwnedFd::from(null));

        let mut nvos64 = vec![0u8; 48];
        nvos64[12..16].copy_from_slice(&0xc7b7u32.to_le_bytes()); // Ampere NVENC
        let mut resp = vec![0u8; 256];
        be.dispatch(
            &ioctl_msg(h, abi::ioctl::NV_ESC_RM_ALLOC, &nvos64),
            &mut resp,
        );

        assert_eq!(parse_resp(&resp).status, 0, "the ioctl itself succeeds");
        let body = size_of::<MsgHeader>() + size_of::<IoctlResp>();
        let status = u32::from_le_bytes(resp[body + 40..body + 44].try_into().unwrap());
        assert_eq!(status, 0x22, "NV_ERR_INVALID_CLASS in NVOS64.status");
    }

    /// A fake host driver: records every request that reaches it and answers
    /// each one successfully without touching the parameters.
    #[derive(Clone, Default)]
    struct CountingHost(std::sync::Arc<std::sync::Mutex<Vec<u64>>>);

    impl HostDriver for CountingHost {
        fn ioctl(&self, _fd: RawFd, request: u64, _arg: &mut [u8]) -> std::result::Result<(), i32> {
            self.0.lock().unwrap().push(request);
            Ok(())
        }
    }

    impl CountingHost {
        fn calls(&self) -> Vec<u64> {
            self.0.lock().unwrap().clone()
        }
    }

    fn backend_on(host: &CountingHost) -> (NvidiaBackend, u64) {
        let mut be = NvidiaBackend::for_test();
        // A release, so the RM allowlist has tables to apply. Without one the
        // backend refuses every control and class, which is the right default
        // and is what `nothing_is_served_without_a_release` checks.
        be.set_host_driver_version(abi::version::DriverVersion::new(615, 71, 9))
            .expect("615.71.09 has tables");
        be.set_host(Box::new(host.clone()));
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        let h = be.handles.insert(OwnedFd::from(null));
        (be, h)
    }

    /// The seam itself: an allocation the guest may make reaches the host
    /// exactly once, through the trait and not around it.
    #[test]
    fn a_served_alloc_reaches_the_host_once() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        be.set_caps(crate::caps::Caps::parse("graphics,video").unwrap());

        let mut nvos64 = vec![0u8; 48];
        nvos64[12..16].copy_from_slice(&0xc7b7u32.to_le_bytes());
        let mut resp = vec![0u8; 256];
        be.dispatch(
            &ioctl_msg(h, abi::ioctl::NV_ESC_RM_ALLOC, &nvos64),
            &mut resp,
        );

        assert_eq!(parse_resp(&resp).status, 0);
        assert_eq!(host.calls().len(), 1, "one host call: {:x?}", host.calls());
    }

    /// The other side of the seam: a refusal for want of a capability is
    /// answered here, and the fake proves the host never heard of it.
    #[test]
    fn a_caps_refusal_never_reaches_the_host() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        be.set_caps(crate::caps::Caps::parse("graphics,compute").unwrap());

        let mut nvos64 = vec![0u8; 48];
        nvos64[12..16].copy_from_slice(&0xc7b7u32.to_le_bytes());
        let mut resp = vec![0u8; 256];
        be.dispatch(
            &ioctl_msg(h, abi::ioctl::NV_ESC_RM_ALLOC, &nvos64),
            &mut resp,
        );

        assert_eq!(parse_resp(&resp).status, 0);
        assert!(
            host.calls().is_empty(),
            "host was called: {:x?}",
            host.calls()
        );
    }

    // ------------------------------------------------------------------
    // M4: RM's own privilege rule, applied on the guest's behalf.
    //
    // Each of these proves a refusal by what the fake host did *not* hear,
    // which is the only evidence that distinguishes a refusal from a call
    // that happened to fail.
    // ------------------------------------------------------------------

    fn rm_control(cmd: u32, params_size: u32) -> Vec<u8> {
        let mut p = vec![0u8; 32];
        p[8..12].copy_from_slice(&cmd.to_le_bytes());
        p[24..28].copy_from_slice(&params_size.to_le_bytes());
        p
    }

    fn rm_alloc(class: u32) -> Vec<u8> {
        let mut p = vec![0u8; 48];
        p[12..16].copy_from_slice(&class.to_le_bytes());
        p
    }

    fn send(be: &mut NvidiaBackend, h: u64, escape: u32, params: &[u8]) -> Vec<u8> {
        let mut resp = vec![0u8; 512];
        be.dispatch(&ioctl_msg(h, escape, params), &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "the ioctl itself must succeed");
        resp
    }

    /// An ioctl whose top-level struct is followed by a nested block: the
    /// allocation parameters of an RM_ALLOC, or the parameter block of an
    /// RM_CONTROL. That block is what the pointer in the struct addresses, and
    /// its length is what the backend sizes against.
    fn send_nested(
        be: &mut NvidiaBackend,
        h: u64,
        escape: u32,
        top: &[u8],
        nested: usize,
    ) -> Vec<u8> {
        let mut v = hdr(MsgType::Ioctl, h);
        append(
            &mut v,
            &IoctlReq {
                cmd: abi::ioctl::_IOWR(escape, top.len() as u32) as u32,
                data_len: top.len() as u32,
                nested_offset: if nested > 0 { 16 } else { 0 },
                nested_len: nested as u32,
                deep_ptr_offset: 0,
                deep_len: 0,
            },
        );
        v.extend_from_slice(top);
        v.extend_from_slice(&vec![0u8; nested]);
        // Big enough for the refusal to come back whole: GET_PIDS alone
        // carries a few thousand handles.
        let mut resp = vec![0u8; 4096 + top.len() + nested];
        be.dispatch(&v, &mut resp);
        assert_eq!(parse_resp(&resp).status, 0, "the ioctl itself must succeed");
        resp
    }

    fn send_alloc(be: &mut NvidiaBackend, h: u64, class: u32, nested: usize) -> Vec<u8> {
        send_nested(be, h, abi::ioctl::NV_ESC_RM_ALLOC, &rm_alloc(class), nested)
    }

    fn send_control(
        be: &mut NvidiaBackend,
        h: u64,
        cmd: u32,
        declared: u32,
        sent: usize,
    ) -> Vec<u8> {
        send_nested(
            be,
            h,
            abi::ioctl::NV_ESC_RM_CONTROL,
            &rm_control(cmd, declared),
            sent,
        )
    }

    /// The whole point of the deny list. RM marks GET_PIDS non-privileged in
    /// every release here, so the allowlist alone would forward it, and the
    /// guest would get the host's process list.
    #[test]
    fn the_host_s_process_list_is_refused_and_never_asked_for() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        let size = abi::rmallow::v615_71_09::CTRL
            .iter()
            .find(|e| e.cmd == 0x2080018d)
            .expect("GET_PIDS is in the table RM exports")
            .params_size;

        send_control(&mut be, h, 0x2080018d, size, size as usize);
        assert!(
            host.calls().is_empty(),
            "the host was asked for its process list: {:x?}",
            host.calls()
        );
    }

    /// A control RM does not export to an unprivileged caller.
    /// NV0000_CTRL_CMD_GPUACCT_SET_ACCOUNTING_STATE is privileged in RM
    /// itself, so it is in no table.
    #[test]
    fn a_privileged_control_never_reaches_the_host() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        assert!(
            !abi::rmallow::v615_71_09::CTRL
                .iter()
                .any(|e| e.cmd == 0x00000b01),
            "SET_ACCOUNTING_STATE must not be in the allowlist"
        );

        send_control(&mut be, h, 0x00000b01, 8, 8);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());
    }

    /// The size has to be RM's size. A block that is not tells the host to
    /// read or write a different number of bytes than the struct holds.
    #[test]
    fn a_control_at_the_wrong_size_never_reaches_the_host() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        let e = abi::rmallow::v615_71_09::CTRL
            .iter()
            .find(|e| e.params_size > 8 && abi::rmallow::denied(e.cmd).is_none())
            .expect("some control has parameters");

        send_control(
            &mut be,
            h,
            e.cmd,
            e.params_size - 4,
            e.params_size as usize - 4,
        );
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // Declaring RM's size and sending less is refused too: RM copies the
        // declared number of bytes, so the rest would be whatever follows.
        send_control(&mut be, h, e.cmd, e.params_size, e.params_size as usize - 4);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // ...and the same control at RM's size goes through, or the tests
        // above would pass for the wrong reason.
        send_control(&mut be, h, e.cmd, e.params_size, e.params_size as usize);
        assert_eq!(host.calls().len(), 1, "{:x?}", host.calls());
    }

    /// The three controls the probe runs caught the backend refusing while
    /// draw, encode and Vulkan all still needed them. None has a control flag
    /// anywhere, because none of them reaches the exported-method tables: RM
    /// rewrites the first, forwards the second to GSP firmware, and hands the
    /// third to a class whose control is a passthrough.
    #[test]
    fn the_controls_rm_serves_without_a_control_flag_reach_the_host() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);

        // NV2080_CTRL_CMD_FB_GET_INFO, rewritten into FB_GET_INFO_V2. The
        // size is the legacy struct's, not the modern one's.
        send_control(&mut be, h, 0x2080_1301, 16, 16);
        assert_eq!(host.calls().len(), 1, "{:x?}", host.calls());

        // A GSP-forwarded command. RM reads no struct, so any size up to its
        // ceiling is RM's business and not the backend's.
        send_control(&mut be, h, 0x2080_852e, 64, 64);
        assert_eq!(host.calls().len(), 2, "{:x?}", host.calls());

        // A command for NV2081_BINAPI, whose control forwards anything.
        send_control(&mut be, h, 0x2081_0108, 32, 32);
        assert_eq!(host.calls().len(), 3, "{:x?}", host.calls());
    }

    /// The same paths refuse. Each of these is the thing the rule above exists
    /// to let through, with the one bit changed that RM refuses on.
    #[test]
    fn the_same_paths_refuse_what_rm_refuses() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);

        // Rewritten into a command RM marks privileged.
        send_control(&mut be, h, 0x0073_136a, 64, 64);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // The same GSP-forwarded command with RM's privileged bits set: RM
        // answers that one to root only, and the backend is root on nobody's
        // behalf.
        send_control(&mut be, h, 0x2080_c52e, 64, 64);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // RM copies no more than this for an unprivileged caller, and a size
        // RM will not copy is a host allocation sized by the guest.
        let max = be
            .rmallow
            .expect("the fixture learned a release")
            .max_params();
        send_control(&mut be, h, 0x2080_852e, max + 1, 64);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // The privileged twin of the catch-all class. RM has two classes here
        // so that exactly this is refused.
        send_control(&mut be, h, 0x2082_0108, 32, 32);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());
    }

    /// The guest driver has to know how many bytes of allocation parameters
    /// to copy before it can forward anything, and `paramsSize` is usually
    /// zero, so it used a table compiled into the module. That table was
    /// generated from 595.58.03 and the host here runs 615.71.09, which added
    /// two words to NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS -- so the guest
    /// forwarded 20 bytes of a 28-byte struct and RM read the other eight from
    /// past the end of the buffer. The backend knows the host's release, so it
    /// sends the sizes and the guest stops guessing.
    #[test]
    fn the_guest_is_told_what_rm_sizes_an_allocation_at() {
        let host = CountingHost::default();
        let (mut be, _h) = backend_on(&host);
        let mut buf = vec![0u8; 8192];
        let n = be.write_alloc_size_section(&mut buf);
        assert!(n >= 8, "the section was not written");

        let word = |i: usize| u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(word(0), NvidiaBackend::ALLOC_SIZE_MAGIC);
        let count = word(1) as usize;
        assert_eq!(n, 8 + count * 8);

        let sizes: std::collections::BTreeMap<u32, u32> = (0..count)
            .map(|i| (word(2 + i * 2), word(3 + i * 2)))
            .collect();

        // The two the probe runs caught, at RM's size for 615.71.09 rather
        // than the 20 and 368 the module was built with.
        assert_eq!(sizes.get(&0xa06c), Some(&28), "KEPLER_CHANNEL_GROUP_A");
        assert_eq!(sizes.get(&0xc56f), Some(&376), "AMPERE_CHANNEL_GPFIFO_A");

        // Every size is RM's own, and a class with no parameters is left out
        // rather than sent as zero -- a zero would read as "copy nothing".
        for c in abi::rmallow::v615_71_09::CLASS {
            assert_eq!(
                sizes.get(&c.class_id),
                (c.params_size > 0).then_some(&c.params_size),
                "class {:#x}",
                c.class_id
            );
        }
    }

    /// Before the backend has learned a release it has nothing to say, and
    /// saying nothing has to mean nothing: the guest reads this section by a
    /// magic word for exactly that reason, and a zero-length section leaves it
    /// on the table it was built with.
    #[test]
    fn a_backend_with_no_release_sends_no_sizes() {
        let mut be = NvidiaBackend::for_test();
        let mut buf = vec![0u8; 8192];
        assert_eq!(be.write_alloc_size_section(&mut buf), 0);
        assert!(buf.iter().all(|b| *b == 0), "something was written anyway");
    }

    /// A class RM does not let an unprivileged caller allocate.
    #[test]
    fn a_class_rm_does_not_export_never_reaches_the_host() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        let class = 0x0000dead;
        assert!(
            !abi::rmallow::v615_71_09::CLASS
                .iter()
                .any(|c| c.class_id == class)
        );

        send_alloc(&mut be, h, class, 0);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());
    }

    /// RS_REQUIRED means the allocation parameters have to be there.
    #[test]
    fn a_class_that_requires_parameters_is_refused_without_them() {
        let host = CountingHost::default();
        let (mut be, h) = backend_on(&host);
        let c = abi::rmallow::v615_71_09::CLASS
            .iter()
            .find(|c| c.params_required)
            .expect("some class requires parameters");

        send_alloc(&mut be, h, c.class_id, 0);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        // Short is refused too: RM reads its own struct's worth of bytes
        // through pAllocParms, so a short block is a read past what was sent.
        send_alloc(&mut be, h, c.class_id, c.params_size as usize - 4);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());

        send_alloc(&mut be, h, c.class_id, c.params_size as usize);
        assert_eq!(host.calls().len(), 1, "{:x?}", host.calls());
    }

    /// A backend that never learned the host's release has no rule to apply,
    /// so it applies none of the guest's traffic to the host.
    #[test]
    fn nothing_is_served_without_a_release() {
        let host = CountingHost::default();
        let mut be = NvidiaBackend::for_test();
        be.set_host(Box::new(host.clone()));
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        let h = be.handles.insert(OwnedFd::from(null));

        send_alloc(&mut be, h, 0xc7b7, 12);
        send_control(&mut be, h, 0x20800110, 8, 8);
        assert!(host.calls().is_empty(), "{:x?}", host.calls());
    }

    #[test]
    fn a_release_older_than_every_profile_is_refused() {
        use abi::version::DriverVersion as V;
        let mut be = NvidiaBackend::for_test();
        assert!(be.set_host_driver_version(V::new(470, 0, 0)).is_err());
        assert!(be.driver.is_none());
        assert!(be.set_host_driver_version(V::new(595, 104, 2)).is_ok());
        assert_eq!(be.driver, Some(V::new(595, 104, 2)));
    }

    #[test]
    fn close_unknown_handle() {
        let mut be = NvidiaBackend::for_test();
        let req = close_msg(0xCAFE);
        let mut resp = vec![0u8; 32];
        be.dispatch(&req, &mut resp);
        assert!(is_err(&resp, Status::BadHandle));
    }

    #[test]
    fn short_request_rejected() {
        let mut be = NvidiaBackend::for_test();
        be.dispatch(&[0u8; 4], &mut vec![0u8; 32]);
        // just must not panic
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
            r.status == -0 || r.status == -Status::IoctlFailed.errno(),
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
        assert!(is_err(&iresp, Status::BadHandle));
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
            -Status::BadHandle.errno(),
            "FD translation should have succeeded"
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
    // GPU registers, not system memory.
    // ------------------------------------------------------------------

    const NV01_ROOT_CLIENT: u32 = 0x41;
    const NV01_DEVICE_0: u32 = 0x80;
    const NV20_SUBDEVICE_0: u32 = 0x2080;
    const TURING_USERMODE_A: u32 = 0xc461;

    /// NVOS64_PARAMETERS field offsets.
    const A_ROOT: usize = 0;
    const A_PARENT: usize = 4;
    const A_NEW: usize = 8;
    const A_CLASS: usize = 12;
    const A_PARAMS_SIZE: usize = 32;
    const A_STATUS: usize = 40;
    const ALLOC_OUTER: usize = 48;

    struct Chain {
        be: NvidiaBackend,
        ctl: u64,
        gpu: u64,
        cookie: u64,
    }

    impl Chain {
        fn new() -> Self {
            // Not for_test(): its write-combine zone is 16 KiB, and the
            // smallest real mapping here is 64 KiB.
            let mut be = NvidiaBackend::with_default_zones();
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

            let memory = self.alloc(client, subdevice, TURING_USERMODE_A, &[]);
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

        let (off, len, linear) = c.map(client, sub, mem, 65536, fd);
        assert_eq!(len, 65536, "mapped length");
        assert_ne!(linear, 0, "pLinearAddress should be the SHM offset");
        assert_eq!(
            linear, off,
            "pLinearAddress must be the SHM offset the guest sees"
        );

        // The SHM window now aliases GPU registers. Reading must not fault.
        let base = c.be.shm_base_ptr();
        assert!(!base.is_null(), "SHM base");
        let first = unsafe { std::ptr::read_volatile(base.add(off as usize) as *const u32) };
        eprintln!("TURING_USERMODE_A first dword through SHM: {first:#010x}");

        c.unmap(client, sub, mem, linear);
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
            let mem = c.alloc(client, sub, TURING_USERMODE_A, &[]);
            let fd = c.map_fd();
            let (off, _len, _linear) = c.map(client, sub, mem, 65536, fd);
            assert_ne!(off, 0, "run {i}: no SHM offset");

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

        // TURING_USERMODE_A permits one mapping per object -- a second map of
        // a still-mapped object is refused with NV_ERR_STATE_IN_USE -- so each
        // cycle allocates its own.
        let before = c.be.shm_free_bytes();
        let handles_before = c.be.handle_count();
        let cycles = 100;
        for i in 0..cycles {
            let mem = c.alloc(client, sub, TURING_USERMODE_A, &[]);
            let fd = c.map_fd();
            eprintln!("cycle {i}: mem={mem:#x} fd={fd}");
            let (off, _len, linear) = c.map(client, sub, mem, 65536, fd);
            assert_ne!(off, 0, "iteration {i}: no SHM offset");
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
}
