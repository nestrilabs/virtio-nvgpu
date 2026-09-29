// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use abi::ioctl::*;

/// The exact reply the Tesla T4 gave to NV_ESC_CHECK_VERSION_STR on driver
fn backend() -> NvidiaBackend {
    NvidiaBackend::with_default_zones()
}

#[test]
fn no_profile_before_the_version_is_known() {
    let b = backend();
    assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
}

#[test]
fn a_measured_version_selects_a_profile() {
    let mut b = backend();
    b.set_host_driver_version("580.178.04");
    assert_eq!(
        b.driver,
        Some(abi::version::DriverVersion::new(580, 178, 4))
    );
    assert!(b.abi.is_some(), "580.178.04 must select a profile");
}

/// The version is the host's, never the guest's: a CHECK_VERSION_STR
/// reply, whose string RM leaves as the caller sent it, teaches nothing,
/// and with no version set an RM escape is refused, not forwarded
/// unchecked.
#[test]
fn with_no_host_version_no_rm_escape_passes_and_none_teaches_one() {
    let mut b = backend();
    b.unversioned_for_test = false;
    b.set_host_ioctl_for_test(|_, _, arg| {
        let (a, _) = arg.split();
        a[4] = 1;
        a[8..18].copy_from_slice(b"580.178.04");
        0
    });
    let ctl = b.adopt_for_test(
        std::fs::File::open("/dev/null").unwrap().into(),
        HandleKind::Dev(DeviceKind::Ctl),
    );
    let mut req = Vec::new();
    for v in [MsgType::Ioctl as u32, ctl, 0, 0] {
        req.extend_from_slice(&v.to_le_bytes());
    }
    let cmd = abi::ioctl::_IOWR(NV_ESC_CHECK_VERSION_STR, 72) as u32;
    for v in [cmd, 72, 72, 0, 0, 0] {
        req.extend_from_slice(&v.to_le_bytes());
    }
    req.extend_from_slice(&[0u8; 72]);
    let mut resp = vec![0u8; 256];
    b.dispatch(&req, &mut resp);
    assert_eq!(read_struct::<MsgHeader>(&resp, 0).status, -libc::EINVAL);
    assert!(b.driver.is_none());
}

/// The property the tables exist for: an escape nobody described does not
/// reach the host driver. This is the check that was a log line until the
/// question was asked in public.
#[test]
fn an_escape_outside_the_profile_is_refused() {
    let mut b = backend();
    b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
    assert!(b.abi.is_some());

    // 0x7f is not an NVIDIA escape and is in no profile.
    assert_eq!(b.check_abi(0x7f, 16), AbiCheck::UnknownEscape);
    assert_eq!(b.abi_policy, AbiPolicy::Enforce, "enforcing is the default");

    // And a size the host does not agree with, on an escape that exists.
    assert!(matches!(
        b.check_abi(NV_ESC_RM_CONTROL, 31),
        AbiCheck::SizeMismatch { .. }
    ));
}

/// S-15: an escape the table marks as carrying a descriptor that nothing
/// here translates is refused, not forwarded with the guest's number.
#[test]
fn a_descriptor_carrying_escape_with_no_translation_is_refused() {
    let mut b = backend();
    b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
    assert!(b.untranslated_fd_escape(NV_ESC_EXPORT_TO_DMABUF_FD));
    for translated in [
        NV_ESC_REGISTER_FD,
        NV_ESC_ALLOC_OS_EVENT,
        NV_ESC_FREE_OS_EVENT,
        NV_ESC_RM_ALLOC_MEMORY,
        NV_ESC_RM_CONTROL,
    ] {
        assert!(!b.untranslated_fd_escape(translated), "{translated:#x}");
    }
}

/// With no version there is no profile to check against (and outside
/// the tests' fake RMs, `dispatch` refuses every RM escape then).
#[test]
fn nothing_is_checked_before_the_version_is_known() {
    let b = backend();
    assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
}

#[test]
fn accepts_the_sizes_the_t4_actually_sent() {
    let mut b = backend();
    b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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
    b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
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
    b.set_driver_version(abi::version::DriverVersion::new(580, 178, 4));
    // CARD_INFO is an array; the T4 sent 2304 bytes in one call.
    assert_eq!(
        b.check_abi(NV_ESC_CARD_INFO, 2304),
        AbiCheck::VariableLength
    );
}

#[test]
fn a_garbled_version_string_leaves_the_backend_unconfigured() {
    let mut b = backend();
    b.set_host_driver_version("oops");
    assert!(b.driver.is_none());
    assert_eq!(b.check_abi(NV_ESC_RM_CONTROL, 32), AbiCheck::NoProfile);
}
