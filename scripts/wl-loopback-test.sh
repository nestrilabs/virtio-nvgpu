#!/usr/bin/env bash
# Run the Wayland proxy end to end without a VM: real clients (wayland-info,
# weston-presentation-shm, wl-copy/wl-paste) → the guest daemon → an
# in-process channel → the backend's WlConn (directly, and again through the
# whole dispatcher, NvidiaBackend::serve) → a headless sway or weston.
#
# Usage: scripts/wl-loopback-test.sh [extra cargo test args]
#
# Needs nix (for sway, wayland-utils, wl-clipboard and weston); no GPU, no
# guest. CARGO_TARGET_DIR is honoured. The compositor's socket lives under
# $NVWL_TEST_DIR (default: the system temp dir), which must be short: a unix
# socket path is at most 108 bytes.
set -euo pipefail

cd "$(dirname "$0")/.."
export NIX_CONFIG="experimental-features = nix-command flakes"
exec nix shell nixpkgs#sway nixpkgs#weston nixpkgs#wayland-utils nixpkgs#wl-clipboard \
    nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#pkg-config -c \
    cargo test -p nvgpu-wl-guest --test loopback -- --ignored --nocapture "$@"
