//! nvgpu-wl-guest [--socket NAME|PATH] [--device PATH] [--card PATH]
//!                [--export NAME|PATH] [--render PATH]
//!
//! Normal mode: serve `--socket` (default `wayland-0` in `$XDG_RUNTIME_DIR`)
//! and proxy every client to the host compositor. `--export NAME`: proxy the
//! host's export socket to the guest compositor at NAME instead.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use nvgpu_wl_guest::channel::DevConnector;
use nvgpu_wl_guest::daemon::{Config, Daemon};

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

fn usage() -> ! {
    eprintln!(
        "usage: nvgpu-wl-guest [--socket NAME|PATH] [--device PATH] [--card PATH] [--export NAME|PATH] [--render PATH]"
    );
    std::process::exit(2)
}

fn main() {
    let mut socket = String::from("wayland-0");
    let mut device = PathBuf::from("/dev/nvgpu-wl");
    let mut card = None;
    let mut render = None;
    let mut export = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--socket" => socket = val(),
            "--device" => device = PathBuf::from(val()),
            "--card" => card = Some(PathBuf::from(val())),
            "--render" => render = Some(PathBuf::from(val())),
            "--export" => export = Some(val()),
            _ => usage(),
        }
    }
    let run = || -> Result<(), String> {
        let mut cfg = Config::new(socket_path(&socket)?);
        cfg.card = card;
        cfg.render = render;
        cfg.export_to = export.as_deref().map(socket_path).transpose()?;
        let mut d = Daemon::new(cfg.clone(), Box::new(DevConnector { path: device }))
            .map_err(|e| e.to_string())?;
        unsafe {
            libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        }
        let stop = d.stop_flag();
        match &cfg.export_to {
            Some(t) => eprintln!(
                "nvgpu-wl-guest: exporting the host's socket to {}",
                t.display()
            ),
            None => eprintln!("nvgpu-wl-guest: serving {}", cfg.listen.display()),
        }
        while !STOP.load(Ordering::Relaxed) {
            d.turn(200).map_err(|e| e.to_string())?;
        }
        stop.store(true, Ordering::Relaxed);
        d.run().map_err(|e| e.to_string())?;
        if cfg.export_to.is_none() {
            let _ = std::fs::remove_file(&cfg.listen);
        }
        Ok(())
    };
    if let Err(e) = run() {
        eprintln!("nvgpu-wl-guest: {e}");
        std::process::exit(1);
    }
}
