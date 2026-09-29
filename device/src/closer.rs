// SPDX-License-Identifier: Apache-2.0
//! The last close of a display file, off the threads that must not wait.
//!
//! Closing a descriptor is not free when it is the last reference to a DRM
//! or NVKMS file: the kernel's release runs in the closing thread, and for
//! these files it can block for as long as a modeset (S-33).
//!
//! - Every nvidia-drm file, render nodes included: nv_drm_postclose takes
//!   DRM_MODESET_LOCK_ALL and commits a connector disable for grants made
//!   through the file (nvidia-drm-drv.c:1478, 1502-1520), so it waits behind
//!   any blocking commit holding the locks.
//! - A file with framebuffers still on a plane: drm_fb_release removes them
//!   with a blocking disable commit (drm_framebuffer.c:807-823).
//! - A master (the card file of a guest compositor, a lease): master_drop
//!   locks every modeset lock and disables every head
//!   (nvidia-drm-drv.c:1038-1068), hundreds of milliseconds with DP link
//!   teardown.
//! - A modeset file: nvKmsClose under nvkms_lock.
//!
//! The backend's last reference is dropped by whichever thread lets go last:
//! the queue thread in CLOSE, the event pump when it stops watching (it holds
//! a duplicate of every watched file), an executor finishing a call under
//! the backend mutex. Each of those waiting on a modeset stalls the whole VM
//! -- every RM call, every event -- which is what ARCHITECTURE.md,
//! "Protocol v2", says the queue thread never does. So those drops come here
//! instead: one thread, `nvgpu-closer`, that does nothing but drop what it is handed, in order.
//! Anything whose drop closes a descriptor can be sent (an `OwnedFd`, a
//! `PrivateFd`, which unregisters itself on whichever thread drops it).
//!
//! Closing is deferred, so a file is still open for a moment after the guest
//! was told it was closed. What depends on that close having happened -- a
//! card opened as master right after the previous master closed -- waits for
//! the closer to be idle first ([`wait_idle`], OPEN_KMS on its executor).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::hostfd::HandleKind;
use protocol::messages::DeviceKind;

type Item = Box<dyn Send>;

struct Closer {
    tx: Mutex<Sender<Item>>,
    pending: AtomicUsize,
    idle: (Mutex<()>, Condvar),
}

fn closer() -> Option<&'static Closer> {
    static CLOSER: OnceLock<Option<Closer>> = OnceLock::new();
    CLOSER
        .get_or_init(|| {
            let (tx, rx) = channel::<Item>();
            let spawned = std::thread::Builder::new()
                .name("nvgpu-closer".into())
                .spawn(move || {
                    for item in rx {
                        drop(item);
                        let c = closer().expect("the closer runs only once made");
                        c.pending.fetch_sub(1, Ordering::AcqRel);
                        let _g = c.idle.0.lock().unwrap_or_else(|p| p.into_inner());
                        c.idle.1.notify_all();
                    }
                });
            match spawned {
                Ok(_) => Some(Closer {
                    tx: Mutex::new(tx),
                    pending: AtomicUsize::new(0),
                    idle: (Mutex::new(()), Condvar::new()),
                }),
                Err(e) => {
                    log::error!("no closer thread ({e}); display files close where dropped");
                    None
                }
            }
        })
        .as_ref()
}

/// Drop `what` on the closer thread. Without one (it could not be started)
/// it is dropped here, as before.
pub fn close<T: Send + 'static>(what: T) {
    let Some(c) = closer() else {
        drop(what);
        return;
    };
    c.pending.fetch_add(1, Ordering::AcqRel);
    let sent =
        c.tx.lock()
            .unwrap_or_else(|p| p.into_inner())
            .send(Box::new(what));
    if let Err(back) = sent {
        c.pending.fetch_sub(1, Ordering::AcqRel);
        drop(back.0);
    }
}

/// Whether the last close of a file of `kind` can wait on a modeset lock or
/// nvkms_lock (see the module comment).
pub fn slow(kind: HandleKind) -> bool {
    matches!(
        kind,
        HandleKind::DriRender(_)
            | HandleKind::DrmCard(_)
            | HandleKind::DrmLease(_)
            | HandleKind::Dev(DeviceKind::Modeset)
    )
}

/// Close a handle table's descriptor of `kind`: on the closer thread if its
/// last close can wait on the display, here otherwise (RM files, eventfds,
/// sync files and the like close without waiting on anything).
pub fn close_fd(fd: std::os::fd::OwnedFd, kind: HandleKind) {
    if slow(kind) {
        close(fd);
    } else {
        drop(fd);
    }
}

/// Wait until everything handed to the closer so far has been dropped, for
/// at most `timeout`. True if it has.
pub fn wait_idle(timeout: Duration) -> bool {
    let Some(c) = closer() else {
        return true;
    };
    let deadline = Instant::now() + timeout;
    let mut g = c.idle.0.lock().unwrap_or_else(|p| p.into_inner());
    while c.pending.load(Ordering::Acquire) != 0 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        g = c
            .idle
            .1
            .wait_timeout(g, left)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Says which thread dropped it.
    struct Probe(mpsc::Sender<String>);
    impl Drop for Probe {
        fn drop(&mut self) {
            let name = std::thread::current().name().unwrap_or("").to_string();
            let _ = self.0.send(name);
        }
    }

    #[test]
    fn what_is_handed_over_is_dropped_on_the_closer_thread() {
        let (tx, rx) = mpsc::channel();
        close(Probe(tx));
        assert!(wait_idle(Duration::from_secs(5)));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "nvgpu-closer"
        );
    }

    #[test]
    fn display_files_close_off_thread_and_the_rest_in_place() {
        for kind in [
            HandleKind::DriRender(0),
            HandleKind::DrmCard(0),
            HandleKind::DrmLease(1),
            HandleKind::Dev(DeviceKind::Modeset),
        ] {
            assert!(slow(kind), "{kind:?}");
        }
        for kind in [
            HandleKind::Dev(DeviceKind::Ctl),
            HandleKind::Dev(DeviceKind::Gpu(0)),
            HandleKind::Dev(DeviceKind::Uvm),
            HandleKind::SyncFile,
            HandleKind::Eventfd,
            HandleKind::Dmabuf,
        ] {
            assert!(!slow(kind), "{kind:?}");
        }
    }

    #[test]
    fn waiting_for_the_closer_sees_every_earlier_close_done() {
        let (tx, rx) = mpsc::channel();
        for _ in 0..50 {
            close(Probe(tx.clone()));
        }
        assert!(wait_idle(Duration::from_secs(5)));
        assert_eq!(rx.try_iter().count(), 50);
    }
}
