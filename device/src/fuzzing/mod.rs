// SPDX-License-Identifier: Apache-2.0
//! Fuzzing entry points, and the fake host they run against.
//!
//! Compiled only with `--cfg fuzzing`, which `cargo fuzz` passes to every
//! crate it builds (`fuzz/`, `scripts/fuzz.sh`): nothing here is in the
//! backend. Each `pub fn` takes the fuzzer's bytes as what a guest (or, for
//! the Wayland engine, a peer) sent, and panics on anything that must never
//! happen: a panic of the code under test, a guest value reaching the host
//! kernel as a pointer, the host kernel handed a buffer shorter than it
//! copies, a backend address in a reply, a placement outside the window or
//! over another, the pages RM pins not being the ones the guest named, a
//! descriptor leaked.
//!
//! The host kernel is `host.rs`: RM, UVM, NVKMS and nvidia-drm as far as
//! their parameter blocks go -- it follows every pointer the real driver
//! would, for as many bytes as the real driver would copy, and checks each
//! against what the guest sent. Nothing reaches a real device: every path
//! the backend opens becomes `/dev/null` (`nvidia/`, `session.rs`,
//! `semsurf.rs`, `hostfd.rs`, under `cfg(fuzzing)`), and [`sandboxed`]
//! refuses to run where `/dev/nvidiactl` can be opened at all
//! (`scripts/fuzz.sh` runs every target in a bubblewrap sandbox with a
//! `/dev` of its own).

#![forbid(unsafe_code)]

pub mod backend;
pub mod host;
pub mod inject;
pub mod parsers;
#[cfg(feature = "vhost-user")]
pub mod vring;
pub mod wayland;

use std::sync::Once;

/// Refuse to run anywhere a real NVIDIA device is reachable: the fake host
/// is meant to stand in for every host call, and if one path were missed it
/// must find nothing to talk to.
pub fn sandboxed() {
    static CHECK: Once = Once::new();
    CHECK.call_once(|| {
        for p in [
            "/dev/nvidiactl",
            "/dev/nvidia0",
            "/dev/nvidia-uvm",
            "/dev/nvidia-modeset",
            "/dev/dri",
            "/dev/udmabuf",
        ] {
            if std::path::Path::new(p).exists()
                && std::env::var_os("NVGPU_FUZZ_UNSANDBOXED").is_none()
            {
                panic!(
                    "{p} exists: run the fuzzers through scripts/fuzz.sh, which gives them a /dev \
                     without the GPU (or set NVGPU_FUZZ_UNSANDBOXED=1 on a machine with none)"
                );
            }
        }
        // Quiet: the backend logs every refusal, and the fuzzer makes
        // millions of them.
        log::set_max_level(log::LevelFilter::Off);
    });
}

/// A little-endian cursor over the fuzzer's bytes. Running out is not an
/// error: every read past the end is zero, so every input means something.
pub struct Bytes<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Bytes<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    pub fn is_empty(&self) -> bool {
        self.at >= self.b.len()
    }
    pub fn u8(&mut self) -> u8 {
        let v = self.b.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        v
    }
    pub fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }
    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    pub fn u64(&mut self) -> u64 {
        u64::from(self.u32()) | u64::from(self.u32()) << 32
    }
    /// Up to `n` bytes, fewer at the end.
    pub fn take(&mut self, n: usize) -> &'a [u8] {
        let start = self.at.min(self.b.len());
        let end = self.at.saturating_add(n).min(self.b.len());
        self.at = self.at.saturating_add(n);
        &self.b[start..end]
    }
    /// A u16 length (less than `max`), then that many bytes.
    pub fn chunk(&mut self, max: usize) -> &'a [u8] {
        let n = self.u16() as usize % max.max(1);
        self.take(n)
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let start = self.at.min(self.b.len());
        self.at = self.b.len();
        &self.b[start..]
    }
}

/// How many descriptors this process has open.
pub fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count())
}
