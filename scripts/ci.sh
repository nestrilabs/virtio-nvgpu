#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The project's checks, in three tiers by what they need.
#
# Usage: scripts/ci.sh [fast|deploy|kernel|nightly|all]...   (default: fast)
#
#   fast     no GPU, no network beyond crates and the flake's nixpkgs,
#            minutes: rustfmt, clippy with -D warnings over the workspace and
#            all targets, the workspace's tests with the vhost-user backend,
#            the unsafe-confinement check, the NixOS module's patches against
#            the patches they stand for, the scripts' syntax (and shellcheck
#            of the launcher and its dry run), and the deployment:
#            nix/module.nix evaluated with its assertions tried and its units
#            compared with contrib/systemd's (checks.module-eval), those
#            units through `systemd-analyze verify`, patches/crosvm applied
#            in order to c0474109d64d (from CROSVM_SRC, or the rig's crosvm
#            checkout; skipped, and said, where there is none), and, on a
#            NixOS host with user namespaces, the launcher's dry run
#            (rig/verify/launcher-dryrun). `deploy` runs that last part
#            (from the scripts' syntax on) alone. The root flake's
#            checks.x86_64-linux runs the Rust half and the module's
#            evaluation (`nix flake check`).
#   kernel   the guest module, C and Rust parsers, against a guest kernel's
#            build tree: no warning allowed, the Rust object may name no
#            panic symbol (driver/Makefile), and with no NVGPU_RUST a Rust
#            kernel's module has the Rust parsers. Needs
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
    for f in $(git ls-files '*.sh'); do
        bash -n "$f" || rc=1
    done
    # The root launcher (with the pieces it sources, -x) and what checks it:
    # warnings are defects there.
    if command -v shellcheck >/dev/null; then
        shellcheck -x -S warning -e SC1007 rig/run-guest.sh rig/verify/launcher-dryrun/*.sh \
            scripts/verify-units.sh || rc=1
    else
        echo "shellcheck: skipped (none on PATH)" >&2
    fi
    return $rc
}

# nix/module.nix evaluated: its assertions refuse what they must, and its
# units say what contrib/systemd's do (nix/module-test.nix).
module_eval_check() {
    nix build --no-link .#checks.x86_64-linux.module-eval
}

units_check() {
    if command -v systemd-analyze >/dev/null; then
        scripts/verify-units.sh
    else
        echo "systemd units: skipped (no systemd-analyze)" >&2
    fi
}

# patches/crosvm applied in order to the commit it was made against, in a
# scratch index (no checkout): a series that no longer applies is found
# here, not by the next person to build it.
CROSVM_BASE=c0474109d64d
crosvm_patches_check() {
    local src=${CROSVM_SRC:-} c w p rc=0
    if [ -z "$src" ]; then
        for c in "${NVGPU_RIG:-.rig}/src/crosvm" "${NVGPU_RIG:-.rig}/src/crosvm-compute"; do
            if [ -e "$c/.git" ] && git -C "$c" cat-file -e "$CROSVM_BASE^{commit}" 2>/dev/null; then
                src=$c
                break
            fi
        done
    fi
    if [ -z "$src" ]; then
        echo "crosvm patches: skipped (no crosvm checkout with $CROSVM_BASE; set CROSVM_SRC)" >&2
        return 0
    fi
    w=$(mktemp -d)
    if git clone -q --bare --shared "$src" "$w/git" &&
        GIT_DIR=$w/git GIT_INDEX_FILE=$w/index git read-tree "$CROSVM_BASE"; then
        for p in patches/crosvm/*.patch; do
            GIT_DIR=$w/git GIT_INDEX_FILE=$w/index git apply --cached -- "$p" || {
                echo "crosvm patches: $p does not apply after the ones before it" >&2
                rc=1
                break
            }
        done
    else
        rc=1
    fi
    rm -rf "$w"
    return $rc
}

# The launcher's dry run (stubs, a user namespace, the NixOS tools).
launcher_dryrun_check() {
    local out
    if [ ! -e /run/current-system/sw/bin ] || ! unshare --user --map-root-user true 2>/dev/null; then
        echo "launcher dry run: skipped (needs NixOS and unprivileged user namespaces)" >&2
        return 0
    fi
    out=$(rig/verify/launcher-dryrun/run.sh 2>&1) || { printf '%s\n' "$out" >&2; return 1; }
    # What the new launcher must do (rig/verify/launcher-dryrun/inside.sh).
    local want=(
        "stub backend: LISTEN_FDS=1 LISTEN_FDNAMES=vhost-user LISTEN_PID is mine: yes"
        "ok: the run's directory stayed root's"
        "are for diagnosis only: NVGPU_DIAGNOSTIC=1 as well"
        "WARNING: diagnostic flag --allow-unmeasured-release"
        "another run with the tag dup.vm0 is running"
        "1 cap line"
        "ESC bytes on the launcher's output: 0"
        "jailer stub: environment clean"
        "is not root's (uid 65534)"
        "the launcher's guest.sh /rig/launcher/guest.sh: /rig/launcher/guest.sh is writable by others"
        "the stale backend: killed"
        "WARNING: diagnostic flag --permissive-abi"
    )
    local w rc=0
    for w in "${want[@]}"; do
        grep -qF -- "$w" <<<"$out" || { echo "launcher dry run: no \"$w\"" >&2; rc=1; }
    done
    [ $rc = 0 ] || printf '%s\n' "$out" >&2
    return $rc
}

fast() {
    step "rustfmt" fmt_check
    step "clippy -D warnings" clippy_check
    step "cargo test (workspace, vhost-user)" test_check
    step "unsafe confinement" scripts/check-unsafe.sh
    step "patches/nixos match their sources" nixos_patches_check
    deploy
}

# The deployment half of fast, alone (scripts/ci.sh deploy).
deploy() {
    step "shell syntax" scripts_syntax_check
    step "NixOS module evaluated" module_eval_check
    step "systemd units" units_check
    step "patches/crosvm apply to $CROSVM_BASE" crosvm_patches_check
    step "launcher dry run" launcher_dryrun_check
}

# ── kernel ───────────────────────────────────────────────────────────────────

# Build driver/ in a scratch copy against $1 (a kernel build tree), with
# $2 (NVGPU_RUST=0|1, or "" for the Makefile's own choice), in the dev shell
# of flake $3 if given; with $4, the module must say it has those parsers
# (modinfo's "parsers": c or rust).
module_build() {
    local kdir=$1 rust=$2 toolchain=${3:-} want=${4:-} work log rc=0 got mk
    work=$(mktemp -d)
    cp -r driver "$work/driver"
    log=$work/build.log
    mk=(make -C "$work/driver" KDIR="$kdir")
    [ -n "$rust" ] && mk+=(NVGPU_RUST="$rust")
    if [ -n "$toolchain" ]; then
        nix develop "path:$toolchain" -c "${mk[@]}" >"$log" 2>&1 || rc=1
    else
        "${mk[@]}" >"$log" 2>&1 || rc=1
    fi
    if [ $rc = 0 ] && grep -Ei 'warning|error|undefined' "$log" >&2; then
        rc=1
    fi
    [ $rc = 0 ] && [ -f "$work/driver/virtio_gpu_nv.ko" ] || { tail -40 "$log" >&2; rc=1; }
    if [ $rc = 0 ] && [ -n "$want" ]; then
        got=$(grep -ao 'parsers=[a-z]*' "$work/driver/virtio_gpu_nv.ko" | head -1)
        if [ "$got" != "parsers=$want" ]; then
            echo "the module has ${got:-no parsers tag}, not parsers=$want" >&2
            rc=1
        fi
    fi
    rm -rf "$work"
    return $rc
}

kernel() {
    if [ -z "${KDIR:-}" ]; then
        echo "kernel tier: KDIR is not set (a guest kernel build tree)" >&2
        failed+=("kernel: KDIR unset")
        return
    fi
    step "guest module, C parsers" module_build "$KDIR" 0 "${KERNEL_TOOLCHAIN:-}" c
    if [ -n "${KDIR_RUST:-}" ]; then
        step "guest module, Rust parsers" module_build "$KDIR_RUST" 1 "${KERNEL_TOOLCHAIN_RUST:-}" rust
        # The Makefile's own choice: the Rust on a kernel with CONFIG_RUST.
        step "guest module, default parsers on a Rust kernel" module_build "$KDIR_RUST" "" "${KERNEL_TOOLCHAIN_RUST:-}" rust
        step "guest module, C parsers on a Rust kernel (NVGPU_RUST=0)" module_build "$KDIR_RUST" 0 "${KERNEL_TOOLCHAIN_RUST:-}" c
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
    deploy) deploy ;;
    kernel) kernel ;;
    nightly) nightly ;;
    all) fast; kernel; nightly ;;
    *) echo "usage: scripts/ci.sh [fast|deploy|kernel|nightly|all]..." >&2; exit 2 ;;
    esac
done

if [ ${#failed[@]} -gt 0 ]; then
    echo "ci: failed: ${failed[*]}" >&2
    exit 1
fi
echo "ci: $* passed" >&2
