# SPDX-License-Identifier: Apache-2.0
# The guest Wayland daemon, from this repo's workspace.
#
# nvgpuSrc is the filtered copy mkimage.sh stages (Cargo.toml, Cargo.lock and
# the workspace members; no target/, .rig/, nvidia-driver/, hyprland/). The
# whole workspace has to be there for cargo to read it, but only the daemon is
# built.
{ pkgs, nvgpuSrc }:
pkgs.rustPlatform.buildRustPackage {
  pname = "nvgpu-wl-guest";
  version = "0.1.0";
  src = nvgpuSrc;
  cargoLock.lockFile = "${nvgpuSrc}/Cargo.lock";
  cargoBuildFlags = [
    "-p"
    "nvgpu-wl-guest"
    "--bin"
    "nvgpu-wl-guest"
  ];
  # Its tests run the backend in process (device/), which is not what the
  # image is for; they run in the repo's own `cargo test`.
  doCheck = false;
  meta.mainProgram = "nvgpu-wl-guest";
}
