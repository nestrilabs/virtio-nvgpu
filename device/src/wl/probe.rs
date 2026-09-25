//! Which of the compositor's lease devices are for our GPU.
//!
//! `wp_drm_lease_device_v1` is one global per DRM device the compositor
//! drives, and nothing in the registry says which device: that is only known
//! from the `drm_fd` it sends after a bind. A guest can only ever use a lease
//! of the GPU it is proxying (the lease file's KMS ioctls run against our
//! nvidia-drm), so a lease device for any other GPU must be hidden -- and
//! hidden *before* the guest sees the global, not after it bound it.
//!
//! So the first time a connection sees a lease-device global whose name is not
//! yet known, the backend opens a private connection of its own, binds every
//! lease device, classifies each `drm_fd`, and remembers the answer by global
//! name for every later connection (global names are per compositor, not per
//! client). The probe is a few round trips on a socket nobody else sees, bounded
//! by a timeout; an unanswered one counts as "not ours".

use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wlwire::proto::{self, Dir, IfaceId, iface, op};
use wlwire::sys;
use wlwire::wire::{self, MsgBuilder, Val, peek_header};

use crate::hostfd::HandleKind;
use crate::wl::conn::HostFds;

const TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct LeaseCache {
    known: Mutex<HashMap<u32, bool>>,
}

impl LeaseCache {
    /// Whether lease-device global `name` hands out files of our GPU, probing
    /// the compositor if this name has not been seen.
    pub fn is_ours(&self, socket: &Path, host: &dyn HostFds, name: u32) -> bool {
        let mut known = self.known.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(&v) = known.get(&name) {
            return v;
        }
        match probe(socket, host) {
            Ok(found) => {
                for (n, ours) in found {
                    known.insert(n, ours);
                }
            }
            Err(e) => log::warn!("wayland: probing the compositor's lease devices failed: {e}"),
        }
        *known.entry(name).or_insert(false)
    }
}

/// A minimal client: enough object tracking to parse the replies, and to
/// count descriptors.
struct Probe {
    sock: UnixStream,
    objects: HashMap<u32, IfaceId>,
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Probe {
    fn send(&mut self, m: Vec<u8>) -> io::Result<()> {
        let mut off = 0;
        let deadline = Instant::now() + TIMEOUT;
        while off < m.len() {
            match sys::send_with_fds(self.sock.as_raw_fd(), &m[off..], &[]) {
                Ok(n) => off += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Read events until `callback.done`, handing each to `f` with the
    /// descriptors it carries.
    fn until_done(
        &mut self,
        callback: u32,
        f: &mut dyn FnMut(u32, u16, &[wire::At<'_>], Vec<OwnedFd>),
    ) -> io::Result<()> {
        let deadline = Instant::now() + TIMEOUT;
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            while let Some(h) = peek_header(&self.buf) {
                let size = h.size as usize;
                if size < 8 || size > wire::MAX_MSG {
                    return Err(io::Error::other("bad message from the compositor"));
                }
                if self.buf.len() < size {
                    break;
                }
                let msg: Vec<u8> = self.buf.drain(..size).collect();
                let Some(&ifc) = self.objects.get(&h.object) else {
                    return Err(io::Error::other("event for an unknown object"));
                };
                let Some(desc) = iface(ifc).messages(Dir::Event).get(h.opcode as usize) else {
                    return Err(io::Error::other("unknown event"));
                };
                let args =
                    wire::parse(desc, &msg).map_err(|e| io::Error::other(format!("{e:?}")))?;
                let mut fds = Vec::new();
                for _ in 0..desc.nfds {
                    fds.push(
                        self.fds
                            .pop_front()
                            .ok_or_else(|| io::Error::other("descriptor expected"))?,
                    );
                }
                for (a, at) in desc.args.iter().zip(&args) {
                    if let (Val::NewId { id, .. }, Some(i)) = (at.val, a.iface) {
                        self.objects.insert(id, i);
                    }
                }
                if h.object == callback && ifc == proto::WL_CALLBACK {
                    return Ok(());
                }
                f(h.object, h.opcode, &args, fds);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let mut p = libc::pollfd {
                fd: self.sock.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut p, 1, left.as_millis() as i32) };
            let mut got = Vec::new();
            match sys::recv_with_fds(self.sock.as_raw_fd(), &mut chunk, &mut got) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    self.fds.extend(got);
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Bind every lease device on a private connection and classify its drm_fd.
/// Returns (global name, is ours) for each.
pub fn probe(socket: &Path, host: &dyn HostFds) -> io::Result<Vec<(u32, bool)>> {
    let sock = UnixStream::connect(socket)?;
    sock.set_nonblocking(true)?;
    let mut p = Probe {
        sock,
        objects: HashMap::from([(1, proto::WL_DISPLAY)]),
        buf: Vec::new(),
        fds: VecDeque::new(),
    };
    p.objects.insert(2, proto::WL_REGISTRY);
    p.objects.insert(3, proto::WL_CALLBACK);
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
    )?;
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(3)
            .finish(),
    )?;
    let mut devices = Vec::new();
    p.until_done(3, &mut |obj, opc, args, _| {
        if obj == 2 && opc == op::wl_registry::EVT_GLOBAL {
            if let (Val::Uint(name), Val::Str(Some(b"wp_drm_lease_device_v1"))) =
                (args[0].val, args[1].val)
            {
                devices.push(name);
            }
        }
    })?;
    if devices.is_empty() {
        return Ok(Vec::new());
    }
    let mut by_obj = HashMap::new();
    for (i, name) in devices.iter().enumerate() {
        let id = 10 + i as u32;
        p.objects.insert(id, proto::WP_DRM_LEASE_DEVICE_V1);
        by_obj.insert(id, *name);
        p.send(
            MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                .uint(*name)
                .generic_new_id("wp_drm_lease_device_v1", 1, id)
                .finish(),
        )?;
    }
    p.objects.insert(4, proto::WL_CALLBACK);
    p.send(
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(4)
            .finish(),
    )?;
    let mut result: HashMap<u32, bool> = devices.iter().map(|n| (*n, false)).collect();
    p.until_done(4, &mut |obj, opc, _, fds| {
        if opc == op::wp_drm_lease_device_v1::EVT_DRM_FD {
            if let (Some(name), Some(fd)) = (by_obj.get(&obj), fds.first()) {
                let ours = matches!(host.classify(fd.as_fd()), HandleKind::DrmLease(_));
                result.insert(*name, ours);
            }
        }
    })?;
    Ok(result.into_iter().collect())
}
