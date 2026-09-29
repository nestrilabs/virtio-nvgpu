// SPDX-License-Identifier: Apache-2.0
//! The daemon's descriptors, shared among its clients, at a descriptor
//! limit scaled down from a session's usual 1024. A file of its own: the
//! limit is the whole test process's.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nvgpu_wl_guest::channel::{Channel, Connector, HostInfo, Received, Sent};
use nvgpu_wl_guest::daemon::{Config, Daemon};
use nvgpu_wl_guest::uapi;
use wlwire::frame::{self, Unit};
use wlwire::proto::op;
use wlwire::sys;
use wlwire::wire::MsgBuilder;

/// A host that takes every frame and says HELLO.
#[derive(Clone)]
struct Script {
    inbox: Arc<Mutex<VecDeque<Vec<u8>>>>,
    ready: Arc<OwnedFd>,
}

struct Ch(Script);

impl Channel for Ch {
    fn send(&mut self, _f: &mut [u8], _fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
        Ok(Sent::Accepted { backlog: 0 })
    }
    fn recv(&mut self, _m: usize, _c: Option<RawFd>, _r: Option<RawFd>) -> io::Result<Received> {
        let mut inbox = self.0.inbox.lock().unwrap();
        let frame = inbox
            .pop_front()
            .unwrap_or_else(|| frame::pack(&mut VecDeque::new(), frame::MIN_FRAME, 0, false).0);
        if inbox.is_empty() {
            sys::eventfd_clear(self.0.ready.as_raw_fd());
        }
        Ok(Received {
            frame,
            fds: Vec::new(),
            more: !inbox.is_empty(),
        })
    }
    fn poll_fd(&self) -> RawFd {
        self.0.ready.as_raw_fd()
    }
}

struct Host(Script);

impl Connector for Host {
    fn info(&mut self) -> io::Result<HostInfo> {
        Ok(HostInfo {
            caps: uapi::CAP_WAYLAND,
            clock_offset_ns: 0,
            max_frame: 256 * 1024,
            devmap: vec![],
        })
    }
    fn connect(&mut self, _m: u32) -> io::Result<Box<dyn Channel>> {
        let hello = frame::Hello {
            version: frame::WL_PROTO_VERSION,
            caps: 0,
        };
        let mut q: VecDeque<Unit> = vec![Unit {
            rec: frame::record(frame::REC_HELLO, 0, 0, &hello.encode()),
            descs: vec![],
        }]
        .into();
        let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
        self.0.inbox.lock().unwrap().push_back(f);
        sys::eventfd_signal(self.0.ready.as_raw_fd());
        Ok(Box::new(Ch(self.0.clone())))
    }
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count() - 1
}

const CHILD: &str = "NVWL_NOFILE_CHILD";

/// One process that sends descriptors beside requests that take none, as
/// fast as the daemon reads them, is closed once it holds its share of the
/// daemon's descriptors, and another process can still connect. Before, the
/// only cap was per connection and above the daemon's own limit: the first
/// process took every descriptor the daemon had, every other client stayed
/// unaccepted, and the daemon spun on the listener, logging each failed
/// accept (117,024 lines in 300 ms at this limit).
#[test]
fn one_process_holding_descriptors_leaves_room_for_the_next() {
    if let Ok(sock) = std::env::var(CHILD) {
        // The other process: connect, and stay a while.
        let _s = UnixStream::connect(sock).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        return;
    }
    nvgpu_wl_guest::sys::set_nofile(128, 128).unwrap();
    let dir = std::env::temp_dir().join(format!("nvwl-nofile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("wayland-0");
    let s = Script {
        inbox: Default::default(),
        ready: Arc::new(sys::eventfd().unwrap()),
    };
    let mut d = Daemon::new(Config::new(&sock), Box::new(Host(s.clone()))).unwrap();
    let mut a = UnixStream::connect(&sock).unwrap();
    d.turn(50).unwrap();
    let e = sys::eventfd().unwrap();
    for id in 100..300 {
        let m = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(id)
            .finish();
        if sys::send_with_fds(a.as_raw_fd(), &m, &[e.as_raw_fd()]).is_err() {
            break;
        }
        d.turn(10).unwrap();
        assert!(
            open_fds() < 127,
            "the daemon let one process take every descriptor"
        );
    }
    // A was told why, and closed.
    a.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut got = Vec::new();
    let _ = a.read_to_end(&mut got);
    let h = wlwire::wire::peek_header(&got).expect("an error");
    assert_eq!(
        (h.object, h.opcode),
        (1, op::wl_display::EVT_ERROR),
        "the client is told"
    );
    assert_eq!(
        u32::from_ne_bytes(got[12..16].try_into().unwrap()),
        wlwire::engine::ERR_NO_MEMORY
    );
    // Another process connects, and is taken, while the daemon idles.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "one_process_holding_descriptors_leaves_room_for_the_next",
            "--nocapture",
        ])
        .env(CHILD, &sock)
        .spawn()
        .unwrap();
    let t = Instant::now();
    let mut turns = 0;
    while child.try_wait().unwrap().is_none() {
        d.turn(100).unwrap();
        turns += 1;
        assert!(t.elapsed() < Duration::from_secs(10));
    }
    assert!(child.wait().unwrap().success());
    let secs = t.elapsed().as_secs_f64();
    assert!(
        (turns as f64) < 60.0 * secs + 10.0,
        "{turns} turns in {secs:.2} s: the daemon spins"
    );
    assert_eq!(d.snapshot().clients, 2, "the second process was accepted");

    // The process runs out of descriptors for reasons of its own while a
    // client waits to be accepted: every accept fails at once with EMFILE,
    // and the listener rests rather than wake every wait. Once descriptors
    // are free again the client is taken.
    let _c = UnixStream::connect(&sock).unwrap();
    let mut filler = Vec::new();
    while let Ok(f) = sys::eventfd() {
        filler.push(f);
    }
    let t = Instant::now();
    let mut turns = 0;
    while t.elapsed() < Duration::from_millis(300) {
        d.turn(100).unwrap();
        turns += 1;
    }
    assert!(
        turns < 40,
        "{turns} turns in 300 ms: the daemon spins on accept"
    );
    drop(filler);
    let t = Instant::now();
    while d.snapshot().clients < 3 {
        d.turn(100).unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "never accepted");
    }
}
