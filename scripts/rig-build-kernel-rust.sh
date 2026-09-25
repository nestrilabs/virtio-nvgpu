#!/usr/bin/env bash
# Rig: the guest kernel with CONFIG_RUST, and the guest module with its
# untrusted-input parsers in Rust (NVGPU_RUST=1). A separate build from
# .rig/build-kernel.sh's, whose outputs it never touches:
#
#   $RIG/kernel-rust/build/      the kernel build tree (O=), the module's KDIR
#   $RIG/kernel-rust/mod/        the copy of driver/ the module is built from
#   $RIG/kernel-rust/vmlinux     the guest kernel, debug info stripped (PVH)
#   $RIG/kernel-rust/nvgpu.ko    the module (virtio_gpu_nv), with DWARF
#   $RIG/kernel-rust/config      the .config it was all built from
#   $RIG/logs/build-kernel-rust.log
#
# RIG defaults to this checkout's .rig; LINUX_SRC to $RIG/src/linux (the rig's
# 7.2.7 tree, which an O= build leaves clean, so two builds may share it). The
# toolchain is scripts/guest-toolchain-rust: the rig's pinned nixpkgs, plus
# rustc, bindgen and rust-src.
#
# Usage: scripts/rig-build-kernel-rust.sh [jobs]
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RIG="${RIG:-$REPO/.rig}"
LINUX_SRC="${LINUX_SRC:-$RIG/src/linux}"
JOBS="${1:-$(nproc)}"
K="$RIG/kernel-rust"
LOG="$RIG/logs/build-kernel-rust.log"
mkdir -p "$K" "$RIG/logs"
export NIX_CONFIG="experimental-features = nix-command flakes"

in_toolchain() { nix develop "path:$REPO/scripts/guest-toolchain-rust" -c "$@"; }

echo "== building (log: $LOG)"
if ! NVGPU_RUST=1 MODULE_DIR="$K/mod" in_toolchain \
    "$REPO/scripts/build-guest-kernel.sh" "$LINUX_SRC" "$JOBS" "$K/build" \
    >"$LOG" 2>&1; then
    tail -40 "$LOG"
    echo "FAIL: build failed; see $LOG" >&2
    exit 1
fi

MODLOG="$RIG/logs/build-module-rust.log"
sed -n '/^== building the guest module against it/,$p' "$LOG" >"$MODLOG"
if grep -Ei 'warning|error|undefined' "$MODLOG"; then
    echo "FAIL: the module build was not clean; see $MODLOG" >&2
    exit 1
fi

KREL="$(in_toolchain make -s -C "$LINUX_SRC" O="$K/build" kernelrelease)"
KO="$K/mod/virtio_gpu_nv.ko"
VERMAGIC="$(in_toolchain modinfo -F vermagic "$KO")"
case "$VERMAGIC" in
"$KREL "*) ;;
*)
    echo "FAIL: module vermagic '$VERMAGIC' is not for kernel $KREL" >&2
    exit 1
    ;;
esac
# The Rust parsers are what got linked, not the C ones they replace.
if ! in_toolchain nm "$KO" | grep -q ' T nvgpu_rs_i2_ioctl$' ||
    in_toolchain nm "$KO" | grep -q ' T nvgpu_osdesc_ioctl$'; then
    echo "FAIL: $KO does not have the Rust parsers in place of the C" >&2
    exit 1
fi
# And they cannot panic: no path in them reaches a Rust panic (an overflow
# check, a bounds check, an unwrap), which in the kernel is a BUG().
if in_toolchain nm -u "$K/mod/nvgpu_rs.o" | grep -i 'panic'; then
    echo "FAIL: nvgpu_rs.o has a panic path (above)" >&2
    exit 1
fi

if ! in_toolchain readelf -n "$K/build/vmlinux" | grep -q 'Xen.*0x00000012'; then
    echo "FAIL: vmlinux has no PVH entry note" >&2
    exit 1
fi

if [ ! -e "$K/vmlinux" ] || [ "$K/build/vmlinux" -nt "$K/vmlinux" ]; then
    in_toolchain objcopy --strip-debug "$K/build/vmlinux" "$K/vmlinux.tmp"
    mv "$K/vmlinux.tmp" "$K/vmlinux"
fi
install -m 0644 "$KO" "$K/nvgpu.ko"
install -m 0644 "$K/build/.config" "$K/config"

echo "PASS: kernel $KREL (CONFIG_RUST=y)"
echo "  $K/vmlinux ($(du -h "$K/vmlinux" | cut -f1), PVH)"
echo "  $K/nvgpu.ko (vermagic: $VERMAGIC)"
echo "  $K/config"
