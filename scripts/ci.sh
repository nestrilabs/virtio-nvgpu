#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The project's checks, in three tiers by what they need.
#
# Usage: scripts/ci.sh [fast|kernel|nightly|all]...   (default: fast)
#
#   fast     no GPU, no network beyond crates, minutes: rustfmt, clippy with
#            -D warnings over the workspace and all targets, the workspace's
#            tests with the vhost-user backend, the unsafe-confinement check,
#            the NixOS module's patches against the patches they stand for,
#            and the scripts' syntax. The root flake's checks.x86_64-linux
#            runs the Rust half of it (`nix flake check`).
#   kernel   the guest module, C and Rust parsers, against a guest kernel's
#            build tree: no warning allowed, and the Rust object may name no
#            panic symbol (driver/Makefile). Needs
#              KDIR       a kernel build tree to build the C against
#              KDIR_RUST  a CONFIG_RUST=y build tree (skipped when unset)
#            and, if the build tools are not on PATH, KERNEL_TOOLCHAIN and
#            KERNEL_TOOLCHAIN_RUST: flakes whose dev shells have them (the
#            rig's .rig/kernel/toolchain and scripts/guest-toolchain-rust).
#   nightly  long, and some need the network: the fuzz targets
#            (scripts/fuzz.sh run, NIGHTLY_FUZZ_SECS each, default 600),
#            Miri over the unsafe-heavy tests, the guest parsers' differential
#            fuzzers (driver/rust/fuzz), the Wayland loopback against real
#            compositors, and scripts/gen-check.sh (every generated table
#            against NVIDIA's and gVisor's sources; GVISOR=fetch by default).
#
# Cargo is taken from PATH if there is one, else from nixpkgs through
# `nix shell`. CARGO_TARGET_DIR and CARGO_HOME are honoured. Exit status is
# 0 only if every step of every tier asked for passed; each failing step is
# named at the end.
set -uo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT" || exit 1
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"

failed=()
step() {
    local name=$1
    shift
    echo "== $name" >&2
    if ! "$@"; then
        echo "FAILED: $name" >&2
        failed+=("$name")
    fi
}

# Run a command with cargo, rustfmt and clippy available.
with_rust() {
    if command -v cargo >/dev/null && cargo clippy --version >/dev/null 2>&1 &&
        cargo fmt --version >/dev/null 2>&1; then
        "$@"
    else
        nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#clippy nixpkgs#rustfmt -c "$@"
    fi
}

# ── fast ─────────────────────────────────────────────────────────────────────

fmt_check() {
    with_rust cargo fmt --all -- --check &&
        with_rust cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check &&
        with_rust cargo fmt --manifest-path driver/rust/fuzz/Cargo.toml --all -- --check
}

clippy_check() {
    with_rust cargo clippy --workspace --all-targets --features device/vhost-user -- -D warnings &&
        with_rust cargo clippy -p device --all-targets --features vhost-user,test-bins -- -D warnings
}

test_check() {
    with_rust cargo test --workspace --features device/vhost-user
}

# patches/nixos holds the patches hyprland-lease.nix applies, beside it so
# the directory can be copied on its own: each must be exactly the patch it
# stands for (today a symlink to it; a copy would do as long as it matches).
nixos_patches_check() {
    local rc=0 pair nixos src
    for pair in \
        "0001-lease-desktop-outputs-efb5099.patch:../hyprland/0001-lease-desktop-outputs-efb5099.patch" \
        "0001-keep-leased-crtcs-1a10fe2.patch:../aquamarine/0001-keep-leased-crtcs-1a10fe2.patch" \
        "0001-keep-leased-crtcs-0.15.1.patch:../aquamarine/0001-keep-leased-crtcs.patch"; do
        nixos=patches/nixos/${pair%%:*}
        src=patches/nixos/${pair#*:}
        if ! cmp -s -- "$nixos" "$src"; then
            echo "$nixos is not $src" >&2
            rc=1
        fi
    done
    return $rc
}

scripts_syntax_check() {
    local rc=0 f
    for f in $(git ls-files '*.sh' contrib/systemd/nvgpu-socket-open); do
        bash -n "$f" || rc=1
    done
    return $rc
}

fast() {
    step "rustfmt" fmt_check
    step "clippy -D warnings" clippy_check
    step "cargo test (workspace, vhost-user)" test_check
    step "unsafe confinement" scripts/check-unsafe.sh
    step "patches/nixos match their sources" nixos_patches_check
    step "shell syntax" scripts_syntax_check
}

# ── kernel ───────────────────────────────────────────────────────────────────

# Build driver/ in a scratch copy against $1 (a kernel build tree), with
# $2 (NVGPU_RUST=0|1), in the dev shell of flake $3 if given.
module_build() {
    local kdir=$1 rust=$2 toolchain=${3:-} work log rc=0
    work=$(mktemp -d)
    cp -r driver "$work/driver"
    log=$work/build.log
    if [ -n "$toolchain" ]; then
        nix develop "path:$toolchain" -c make -C "$work/driver" KDIR="$kdir" NVGPU_RUST="$rust" >"$log" 2>&1 || rc=1
    else
        make -C "$work/driver" KDIR="$kdir" NVGPU_RUST="$rust" >"$log" 2>&1 || rc=1
    fi
    if [ $rc = 0 ] && grep -Ei 'warning|error|undefined' "$log" >&2; then
        rc=1
    fi
    [ $rc = 0 ] && [ -f "$work/driver/virtio_gpu_nv.ko" ] || { tail -40 "$log" >&2; rc=1; }
    rm -rf "$work"
    return $rc
}

kernel() {
    if [ -z "${KDIR:-}" ]; then
        echo "kernel tier: KDIR is not set (a guest kernel build tree)" >&2
        failed+=("kernel: KDIR unset")
        return
    fi
    step "guest module, C parsers" module_build "$KDIR" 0 "${KERNEL_TOOLCHAIN:-}"
    if [ -n "${KDIR_RUST:-}" ]; then
        step "guest module, Rust parsers" module_build "$KDIR_RUST" 1 "${KERNEL_TOOLCHAIN_RUST:-}"
    else
        echo "== guest module, Rust parsers: skipped (KDIR_RUST unset)" >&2
    fi
}

# ── nightly ──────────────────────────────────────────────────────────────────

guest_fuzz() {
    local secs=${NIGHTLY_FUZZ_SECS:-600} t rc=0
    for t in diff_i2 diff_rm diff_atomic i2_raw; do
        nix shell github:nix-community/fenix#minimal.toolchain nixpkgs#cargo-fuzz nixpkgs#gcc -c \
            bash -c 'export LD_LIBRARY_PATH=$(dirname $(gcc -print-file-name=libstdc++.so.6)); \
                     cargo fuzz run --fuzz-dir driver/rust/fuzz "$1" -- -max_total_time="$2"' _ "$t" "$secs" || rc=1
    done
    return $rc
}

nightly() {
    step "fuzz (backend, Wayland)" scripts/fuzz.sh run "${NIGHTLY_FUZZ_SECS:-600}"
    step "miri" scripts/fuzz.sh miri
    step "fuzz (guest parsers, differential)" guest_fuzz
    step "Wayland loopback" scripts/wl-loopback-test.sh
    step "generated tables against their sources" \
        env GVISOR="${GVISOR:-fetch}" nix shell nixpkgs#gcc nixpkgs#python3 nixpkgs#git -c scripts/gen-check.sh
}

# ─────────────────────────────────────────────────────────────────────────────

[ $# -gt 0 ] || set -- fast
for tier in "$@"; do
    case $tier in
    fast) fast ;;
    kernel) kernel ;;
    nightly) nightly ;;
    all) fast; kernel; nightly ;;
    *) echo "usage: scripts/ci.sh [fast|kernel|nightly|all]..." >&2; exit 2 ;;
    esac
done

if [ ${#failed[@]} -gt 0 ]; then
    echo "ci: failed: ${failed[*]}" >&2
    exit 1
fi
echo "ci: $* passed" >&2
