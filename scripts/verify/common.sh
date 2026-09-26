#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Shared helpers for the verify scripts: finding a C toolchain and libdrm's
# headers, whether we are on a dev box with nix or inside a guest with a plain
# toolchain. Source it; do not run it.
#
# The rule everywhere: prefer what is already on the machine (a system `cc` and
# a working `pkg-config libdrm`), and reach for `nix shell` only when it is not.
# The guest rootfs rarely has nix; the dev/GPU box always does.
set -euo pipefail

# Print the C compiler to use. Honours $CC.
verify_cc() {
    if [ -n "${CC:-}" ]; then echo "$CC"; return; fi
    if command -v cc >/dev/null 2>&1; then echo cc; return; fi
    if command -v gcc >/dev/null 2>&1; then echo gcc; return; fi
    echo cc
}

# Echo the cflags and libs for libdrm on stdout, two lines: cflags, then libs.
# Prefers a working `pkg-config libdrm`; on a nix box where pkg-config cannot see
# it, points PKG_CONFIG_PATH at libdrm's dev output (which also carries the right
# libdir), so both cflags and libs resolve correctly.
verify_libdrm_flags() {
    if command -v pkg-config >/dev/null 2>&1; then
        if ! pkg-config --exists libdrm 2>/dev/null && command -v nix >/dev/null 2>&1; then
            export NIX_CONFIG="experimental-features = nix-command flakes"
            local dev
            dev=$(nix build --no-link --print-out-paths 'nixpkgs#libdrm.dev')
            export PKG_CONFIG_PATH="${dev}/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
        fi
        if pkg-config --exists libdrm 2>/dev/null; then
            pkg-config --cflags libdrm
            pkg-config --libs libdrm
            return
        fi
    fi
    echo "verify: cannot find libdrm (need pkg-config libdrm, or nix)" >&2
    return 1
}

# Are we running inside the guest? A rough test: the forwarded nvidia nodes
# exist and the virtio module is loaded.
verify_in_guest() {
    [ -e /dev/nvidiactl ] && [ -d /proc/driver/nvidia ] &&
        grep -q '^virtio_gpu_nv' <<<"$(lsmod 2>/dev/null || true)"
}
