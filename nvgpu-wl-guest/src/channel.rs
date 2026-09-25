//! The channel to the host: `/dev/nvgpu-wl`, or anything else that moves
//! frames the same way (the loopback test puts the backend's own connection
//! object here, in process).

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;

use wlwire::engine::DevPair;
use wlwire::frame::{self, Desc};

use crate::uapi;

/// What the kernel told us once, at start.
#[derive(Clone, Debug, Default)]
pub struct HostInfo {
    pub caps: u32,
    pub clock_offset_ns: i64,
    pub max_frame: usize,
    pub devmap: Vec<DevPair>,
}

pub struct Received {
    pub frame: Vec<u8>,
    /// Per descriptor in the frame: what the kernel installed for it.
    pub fds: Vec<Option<OwnedFd>>,
    pub more: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Sent {
    Accepted {
        backlog: u32,
    },
    /// The host compositor is not reading; try again later.
    Busy,
}

pub trait Channel {
    /// One frame, and per descriptor the descriptor the kernel should resolve
    /// (a client's dma-buf), whose number goes in the desc's `fd`.
    fn send(&mut self, frame: &mut [u8], fds: &[Option<OwnedFd>]) -> io::Result<Sent>;
    /// Whatever the host has, up to `max` bytes. `card`/`render`: templates
    /// for DRM files and dma-bufs, if the frame has any.
    fn recv(
        &mut self,
        max: usize,
        card: Option<RawFd>,
        render: Option<RawFd>,
    ) -> io::Result<Received>;
    /// Readable when the host has something.
    fn poll_fd(&self) -> RawFd;
}

pub trait Connector {
    fn info(&mut self) -> io::Result<HostInfo>;
    /// A new channel: `uapi::CONNECT` (to the host compositor), `LISTEN` or
    /// `ACCEPT` (export mode).
    fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>>;
}

/// Write each pending descriptor's number into its desc's `fd` field.
pub fn fill_fds(frame: &mut [u8], fds: &[Option<OwnedFd>]) {
    for (i, fd) in fds.iter().enumerate() {
        if let Some(fd) = fd {
            let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
            let mut d = Desc::read(&frame[at..at + frame::DESC_LEN]);
            d.fd = fd.as_raw_fd();
            let mut b = Vec::with_capacity(frame::DESC_LEN);
            d.write(&mut b);
            frame[at..at + frame::DESC_LEN].copy_from_slice(&b);
        }
    }
}

/// The real thing.
pub struct DevConnector {
    pub path: PathBuf,
}

pub struct DevChannel {
    file: File,
    /// Where RECV writes, kept from call to call: a RECV comes at least twice
    /// per presented frame (frame done, release, feedback), and a fresh
    /// zeroed buffer of the whole receive size each time was calloc or
    /// mmap churn on that path. The kernel writes what it returns, so the
    /// buffer is never cleared; only the frame's own bytes are copied out.
    rbuf: Vec<u8>,
}

fn ioctl<T>(f: &File, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    let r = unsafe { libc::ioctl(f.as_raw_fd(), req as _, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

impl DevConnector {
    fn open(&self) -> io::Result<File> {
        OpenOptions::new().read(true).write(true).open(&self.path)
    }
}

impl Connector for DevConnector {
    fn info(&mut self) -> io::Result<HostInfo> {
        let f = self.open()?;
        let mut h = uapi::Hello::default();
        ioctl(&f, uapi::IOC_HELLO, &mut h)?;
        if h.version != uapi::UAPI_VERSION {
            return Err(io::Error::other(format!(
                "kernel speaks /dev/nvgpu-wl version {}",
                h.version
            )));
        }
        Ok(HostInfo {
            caps: h.caps,
            clock_offset_ns: h.clock_offset_ns,
            max_frame: h.max_frame as usize,
            devmap: h.dev[..(h.ndev as usize).min(uapi::MAX_DEVMAP)]
                .iter()
                .map(|d| DevPair {
                    host: (d.host_major, d.host_minor),
                    guest: (d.guest_major, d.guest_minor),
                })
                .collect(),
        })
    }

    fn connect(&mut self, mode: u32) -> io::Result<Box<dyn Channel>> {
        let file = self.open()?;
        let mut c = uapi::Connect { mode, flags: 0 };
        ioctl(&file, uapi::IOC_CONNECT, &mut c)?;
        Ok(Box::new(DevChannel {
            file,
            rbuf: Vec::new(),
        }))
    }
}

impl Channel for DevChannel {
    fn send(&mut self, frame: &mut [u8], fds: &[Option<OwnedFd>]) -> io::Result<Sent> {
        fill_fds(frame, fds);
        let mut x = uapi::Xfer {
            frame: frame.as_ptr() as u64,
            len: frame.len() as u32,
            card_fd: -1,
            render_fd: -1,
            ..Default::default()
        };
        match ioctl(&self.file, uapi::IOC_SEND, &mut x) {
            Ok(()) => Ok(Sent::Accepted { backlog: x.backlog }),
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => Ok(Sent::Busy),
            Err(e) => Err(e),
        }
    }

    fn recv(
        &mut self,
        max: usize,
        card: Option<RawFd>,
        render: Option<RawFd>,
    ) -> io::Result<Received> {
        if self.rbuf.len() < max {
            self.rbuf.resize(max, 0);
        }
        let mut x = uapi::Xfer {
            frame: self.rbuf.as_mut_ptr() as u64,
            len: max as u32,
            max_desc: frame::MAX_DESC as u32,
            card_fd: card.unwrap_or(-1),
            render_fd: render.unwrap_or(-1),
            ..Default::default()
        };
        ioctl(&self.file, uapi::IOC_RECV, &mut x)?;
        let buf = self.rbuf[..(x.len as usize).min(max)].to_vec();
        let mut fds = Vec::new();
        if buf.len() >= frame::FRAME_HDR_LEN {
            let n = u16::from_le_bytes(buf[6..8].try_into().unwrap()) as usize;
            for i in 0..n {
                let at = frame::FRAME_HDR_LEN + i * frame::DESC_LEN;
                let Some(b) = buf.get(at..at + frame::DESC_LEN) else {
                    break;
                };
                let d = Desc::read(b);
                fds.push((d.fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(d.fd) }));
            }
        }
        Ok(Received {
            frame: buf,
            fds,
            more: x.flags & uapi::XFER_MORE != 0,
        })
    }

    fn poll_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}
