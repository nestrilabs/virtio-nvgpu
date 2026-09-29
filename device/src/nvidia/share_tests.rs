// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;
use crate::quota::Owner;

fn devnull() -> OwnedFd {
    std::fs::File::open("/dev/null").unwrap().into()
}

/// One process holding every NVKMS open left the compositor with none
/// (B4): a process holds at most its share, and the VM cap stays.
#[test]
fn one_guest_process_cannot_hold_every_modeset_open() {
    let mut be = NvidiaBackend::for_test();
    let p = |t: u32| Owner::Proc {
        tgid: t,
        start_ns: 1,
    };
    let modeset = HandleKind::Dev(DeviceKind::Modeset);
    let mut n = 0;
    while be.modeset_open_refused(p(1)).is_none() {
        be.handles.insert_for(devnull(), modeset, p(1)).unwrap();
        n += 1;
    }
    assert_eq!(n, nvkms::MODESET_SHARE.per_owner);
    assert!(be.modeset_open_refused(p(2)).is_none());
    // Three more processes take theirs; the VM's last eight are kept
    // for processes holding at most two.
    for t in 2..5 {
        while be.modeset_open_refused(p(t)).is_none() {
            be.handles.insert_for(devnull(), modeset, p(t)).unwrap();
        }
    }
    assert!(
        be.modeset_open_refused(p(9)).is_none(),
        "a newcomer gets one"
    );
    for _ in 0..8 {
        if be.modeset_open_refused(Owner::Unknown).is_none() {
            be.handles.insert(devnull(), modeset).unwrap();
        }
    }
    assert!(
        be.modeset_open_refused(p(9)).is_some(),
        "the VM cap is still the outer bound"
    );
}
