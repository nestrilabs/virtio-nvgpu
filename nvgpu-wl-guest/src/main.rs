// SPDX-License-Identifier: Apache-2.0
//! nvgpu-wl-guest [--socket NAME|PATH] [--device PATH] [--card PATH]
//!                [--export NAME|PATH] [--render PATH] [--log LEVEL]
//!                [--surface-buffers N]
//!
//! Normal mode: serve `--socket` (default `wayland-0` in `$XDG_RUNTIME_DIR`)
//! and proxy every client to the host compositor. `--export NAME`: proxy the
//! host's export socket to the guest compositor at NAME instead. `--help`
//! says what each flag does.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use nvgpu_wl_guest::channel::DevConnector;
use nvgpu_wl_guest::daemon::{Config, Daemon};
use nvgpu_wl_guest::log::{self, Level};

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

/// A bare name is relative to `$XDG_RUNTIME_DIR`, as `WAYLAND_DISPLAY` is.
fn socket_path(s: &str) -> Result<PathBuf, String> {
    if s.contains('/') {
        return Ok(PathBuf::from(s));
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(dir).join(s))
}

const USAGE: &str = "usage: nvgpu-wl-guest [--socket NAME|PATH] [--device PATH] [--card PATH] [--export NAME|PATH] [--render PATH] [--log LEVEL] [--surface-buffers N]";

const HELP: &str = "\
The guest half of virtio-nvgpu's Wayland proxy: guest applications connect to
it as to a compositor, and each becomes a client of the host's compositor
through /dev/nvgpu-wl. Run one per session, as the session's user, with the
nvgpu-wl group (setgid; DEPLOY.md, \"The guest\").

  --socket NAME|PATH  where to listen; a bare name is in $XDG_RUNTIME_DIR
                      [default: wayland-0]
  --device PATH       the channel device [default: /dev/nvgpu-wl]
  --card PATH         the guest card node DRM files (leases) from the host are
                      made as [default: the first virtio-nvgpu card in
                      /dev/dri]
  --render PATH       export mode: the guest render node host clients'
                      dma-bufs are imported into [default: the first
                      virtio-nvgpu render node in /dev/dri]
  --export NAME|PATH  export mode: carry host clients of the backend's
                      --wayland-export socket to the guest compositor at NAME,
                      instead of serving guest clients
  --log LEVEL         error, warn, info or debug [default: info, or
                      $NVGPU_WL_LOG]
  --surface-buffers N the wl_shm buffers of one surface whose damage is
                      tracked, 1 to 256 [default: 16]: a client that cycles
                      through more has each buffer copied whole when next
                      shown, not only what changed. Every commit visits
                      each one, on the thread all clients share (DEPLOY.md,
                      \"The guest\")
  -h, --help          this text";

/// `--surface-buffers`: 1 to wlwire's `MAX_SURFACE_BUFFERS`, and nothing
/// else.
fn parse_surface_buffers(v: &str) -> Result<usize, String> {
    let max = wlwire::shm::MAX_SURFACE_BUFFERS;
    match v.parse::<usize>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!("--surface-buffers {v:?}: 1 to {max}")),
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn main() {
    let mut socket = String::from("wayland-0");
    let mut device = PathBuf::from("/dev/nvgpu-wl");
    let mut card = None;
    let mut render = None;
    let mut export = None;
    let mut surface_buffers = wlwire::shm::SURFACE_BUFFERS;
    let mut level = match std::env::var("NVGPU_WL_LOG") {
        Ok(v) => Level::parse(&v).unwrap_or_else(|| {
            eprintln!("nvgpu-wl-guest: NVGPU_WL_LOG={v:?} is not a level; using info");
            log::DEFAULT
        }),
        Err(_) => log::DEFAULT,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--socket" => socket = val(),
            "--device" => device = PathBuf::from(val()),
            "--card" => card = Some(PathBuf::from(val())),
            "--render" => render = Some(PathBuf::from(val())),
            "--export" => export = Some(val()),
            "--log" => level = Level::parse(&val()).unwrap_or_else(|| usage()),
            "--surface-buffers" => {
                surface_buffers = parse_surface_buffers(&val()).unwrap_or_else(|e| {
                    eprintln!("nvgpu-wl-guest: {e}");
                    usage()
                })
            }
            "-h" | "--help" => {
                println!("{USAGE}\n\n{HELP}");
                return;
            }
            _ => usage(),
        }
    }
    log::set(level);
    // Every client's descriptors pass through this one process: the soft
    // limit (1024 in most sessions) is raised to the hard one, as
    // compositors raise theirs, and what clients may hold of it is shared
    // out among them (daemon.rs, budget.rs).
    if let Err(e) = nvgpu_wl_guest::sys::raise_nofile() {
        log::say(
            Level::Warn,
            &format!("nvgpu-wl-guest: cannot raise the descriptor limit: {e}"),
        );
    }
    let run = || -> Result<(), String> {
        let mut cfg = Config::new(socket_path(&socket)?);
        cfg.card = card;
        cfg.render = render;
        cfg.export_to = export.as_deref().map(socket_path).transpose()?;
        cfg.surface_buffers = surface_buffers;
        let path = device.clone();
        let mut d = Daemon::new(cfg.clone(), Box::new(DevConnector { path: device })).map_err(
            |e| match e.kind() {
                // The node is 0660 root:root unless udev gives it a group
                // (contrib/udev/70-nvgpu-wl.rules).
                std::io::ErrorKind::PermissionDenied => format!(
                    "{}: {e}; run as a member of the group the node belongs to \
                     (contrib/udev/70-nvgpu-wl.rules makes it nvgpu-wl)",
                    path.display()
                ),
                _ => e.to_string(),
            },
        )?;
        nvgpu_wl_guest::sys::on_terminate(on_signal);
        let stop = d.stop_flag();
        match &cfg.export_to {
            Some(t) => log::say(
                Level::Info,
                &format!(
                    "nvgpu-wl-guest: exporting the host's socket to {}",
                    t.display()
                ),
            ),
            None => log::say(
                Level::Info,
                &format!("nvgpu-wl-guest: serving {}", cfg.listen.display()),
            ),
        }
        while !STOP.load(Ordering::Relaxed) {
            d.turn(200).map_err(|e| e.to_string())?;
        }
        stop.store(true, Ordering::Relaxed);
        d.run().map_err(|e| e.to_string())?;
        // Dropping the daemon removes its socket and the socket's lock.
        Ok(())
    };
    if let Err(e) = run() {
        log::say(Level::Error, &format!("nvgpu-wl-guest: {e}"));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn surface_buffers_are_held_to_their_bounds() {
        assert_eq!(super::parse_surface_buffers("16"), Ok(16));
        assert_eq!(super::parse_surface_buffers("1"), Ok(1));
        assert_eq!(super::parse_surface_buffers("256"), Ok(256));
        for bad in ["0", "257", "-1", "", "16x", "99999999999999999999999"] {
            assert!(super::parse_surface_buffers(bad).is_err(), "{bad}");
        }
    }
}
