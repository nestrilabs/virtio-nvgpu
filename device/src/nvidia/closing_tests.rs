// SPDX-License-Identifier: Apache-2.0

#![forbid(unsafe_code)]

use super::*;

/// Holds the closer until told to go on (or two seconds pass).
struct Hold(std::sync::mpsc::Receiver<()>);

impl Drop for Hold {
    fn drop(&mut self) {
        let _ = self.0.recv_timeout(std::time::Duration::from_secs(2));
    }
}

/// A display file's last close waits on the closer, and while it does
/// the host file is open. The handle table let it go at CLOSE, so a
/// guest opening and closing render nodes while the closer was stuck
/// on a modeset queued host files without bound. They count against
/// the table until they are closed.
#[test]
fn files_still_closing_count_against_the_handle_table() {
    let devnull = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
    let mut be = NvidiaBackend::for_test();
    be.handles.set_limit(64);
    let (go, wait) = std::sync::mpsc::channel();
    crate::closer::close(Hold(wait));
    let hs: Vec<u32> = (0..64)
        .map(|_| be.adopt_for_test(devnull(), HandleKind::DriRender(0)))
        .collect();
    for h in hs {
        be.close_handle(h).unwrap();
    }
    assert_eq!(be.handle_count(), 0);
    let r = be.handles.insert(devnull(), HandleKind::Eventfd);
    go.send(()).unwrap();
    assert!(r.is_err(), "64 host files are still open");
    assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
    assert!(be.handles.insert(devnull(), HandleKind::Eventfd).is_ok());
}

/// Modeset files still closing count against the NVKMS open caps: else,
/// with the closer stalled on a modeset, one process looping open/close
/// would hold far more than 64 host NVKMS opens.
#[test]
fn modeset_files_still_closing_count_against_the_nvkms_caps() {
    let devnull = || -> OwnedFd { std::fs::File::open("/dev/null").unwrap().into() };
    let p = crate::quota::Owner::Proc {
        tgid: 77,
        start_ns: 1,
    };
    let mut be = NvidiaBackend::for_test();
    let modeset = HandleKind::Dev(DeviceKind::Modeset);
    let (go, wait) = std::sync::mpsc::channel();
    crate::closer::close(Hold(wait));
    let mut n = 0;
    while be.modeset_open_refused(p).is_none() {
        let h = be.adopt_for_test_as(devnull(), modeset, p);
        be.close_handle(h).unwrap();
        n += 1;
        assert!(n <= nvkms::MAX_MODESET_OPENS, "no cap while closing");
    }
    go.send(()).unwrap();
    assert!(crate::closer::wait_idle(std::time::Duration::from_secs(5)));
    assert_eq!(be.modeset_open_refused(p), None, "closed, the room is back");
}
