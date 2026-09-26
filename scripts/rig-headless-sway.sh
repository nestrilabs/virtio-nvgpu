#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A compositor of the rig's own, with no monitor, for the Wayland stages that
# must not touch the live desktop.
#
# Usage: rig-headless-sway.sh [--renderer vulkan|gles2|pixman] [--mode WxH@HZ]
#                             [--render-node /dev/dri/renderDN] [--gl DIR]
#   --renderer     wlroots renderer (default vulkan; gles2 is the other one
#                  that renders on the GPU; pixman is CPU-only, for checking
#                  the plumbing where there is no GPU)
#   --mode         the headless output's mode (default 1920x1080@60Hz)
#   --render-node  the node to render on (default: the NVIDIA render node,
#                  found by PCI vendor 0x10de in /sys/class/drm)
#   --gl DIR       an NVIDIA userspace laid out like /run/opengl-driver
#                  (default /run/opengl-driver when it exists, else the host's
#                  graphics-drivers in /nix/store for the loaded driver)
#
# Runs in the foreground until interrupted. While it runs, the socket's path
# is in $NVGPU_RIG/run/headless-sway.socket, for
#
#   scripts/run-guest.sh --wayland-socket "$(cat .rig/run/headless-sway.socket)" <probe>
#
# Why a separate compositor, and why sway. The Wayland proxy has never run
# against a real compositor, and a Hyprland session hands any client its
# virtual keyboard and pointer and screencopy. The proxy's allowlist hides
# those (wlwire/src/policy_table.rs), but that allowlist is part of what is
# under test: until it has been seen to hold, the guest's clients should reach
# a compositor where getting past it gains nothing. Hyprland 0.56 itself cannot
# be that compositor: aquamarine's headless backend has no allocator of its
# own and needs either DRM -- the card, through a seat, which the live
# desktop holds -- or a parent Wayland compositor. wlroots' headless backend
# renders on a render node alone, with no seat and no card, and sway is the
# wlroots compositor nixpkgs ships.
#
# Everything sway has lives in a fresh 0700 directory it is given as its
# XDG_RUNTIME_DIR, so its socket is not one the user's own programs will find
# by accident, and WAYLAND_DISPLAY is unset so wlroots does not nest into the
# live desktop instead.
set -euo pipefail

REPO=$(cd -- "$(dirname -- "$0")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
RENDERER=vulkan
MODE=1920x1080@60Hz
NODE=
GL=${NVGPU_HOST_GL:-}
while [ $# -gt 0 ]; do
    case $1 in
        --renderer) RENDERER=${2:?}; shift 2 ;;
        --mode) MODE=${2:?}; shift 2 ;;
        --render-node) NODE=${2:?}; shift 2 ;;
        --gl) GL=${2:?}; shift 2 ;;
        -h | --help) sed -n '2,23p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "unknown argument $1 (see --help)" >&2; exit 2 ;;
    esac
done
case $RENDERER in vulkan | gles2 | pixman) ;; *) echo "--renderer: vulkan, gles2 or pixman" >&2; exit 2 ;; esac

die() {
    echo "rig-headless-sway: $*" >&2
    exit 1
}

ENVS=(WLR_BACKENDS=headless WLR_HEADLESS_OUTPUTS=1 "WLR_RENDERER=$RENDERER"
    WLR_LIBINPUT_NO_DEVICES=1 SWAY_UNSUPPORTED_GPU=true)

if [ "$RENDERER" != pixman ]; then
    if [ -z "$NODE" ]; then
        for d in /sys/class/drm/renderD*; do
            [ "$(cat "$d/device/vendor" 2>/dev/null)" = 0x10de ] && { NODE=/dev/dri/${d##*/}; break; }
        done
        [ -n "$NODE" ] || die "no NVIDIA render node found in /sys/class/drm; pass --render-node"
    fi
    # A render node only. wlroots opens whatever this names, and a card
    # (primary) node opened while the desktop has dropped DRM master -- a VT
    # switch -- makes sway master of the monitors the desktop runs on.
    # Render nodes are DRM minors 128 and up and have no master at all.
    [ -c "$NODE" ] || die "$NODE is not a device node"
    NODE_MAJ=$((16#$(stat -L -c %t "$NODE")))
    NODE_MIN=$((16#$(stat -L -c %T "$NODE")))
    [ "$NODE_MAJ" = 226 ] && [ "$NODE_MIN" -ge 128 ] ||
        die "$NODE ($NODE_MAJ:$NODE_MIN) is not a DRM render node; pass /dev/dri/renderDN, never a card"
    [ -r "$NODE" ] && [ -w "$NODE" ] || die "$NODE is not rw for this user"
    ENVS+=("WLR_RENDER_DRM_DEVICE=$NODE")

    # The NVIDIA userspace, for a nix-built sway: on the host it is
    # /run/opengl-driver; inside the sandbox that is not bound, but the
    # store path it points to is, and the loaded driver's version names it.
    if [ -z "$GL" ] && [ -d /run/opengl-driver/lib ]; then
        GL=/run/opengl-driver
    elif [ -z "$GL" ]; then
        v=$(grep -o -E '[0-9]{3}\.[0-9]+(\.[0-9]+)?' /proc/driver/nvidia/version 2>/dev/null | head -n 1) ||
            die "no /proc/driver/nvidia/version to find the userspace by; pass --gl"
        for d in /nix/store/*-graphics-drivers; do
            [ -e "$d/lib/libEGL_nvidia.so.$v" ] && { GL=$d; break; }
        done
        [ -n "$GL" ] || die "no graphics-drivers in /nix/store with NVIDIA $v; pass --gl"
    fi
    [ -d "$GL/lib" ] || die "--gl $GL has no lib/"
    echo "NVIDIA userspace: $GL" >&2
    icds=$(find "$GL/share/vulkan/icd.d" -name 'nvidia*.json' 2>/dev/null | paste -s -d:)
    ENVS+=("LD_LIBRARY_PATH=$GL/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        "__EGL_VENDOR_LIBRARY_DIRS=$GL/share/glvnd/egl_vendor.d"
        "__EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=$GL/share/egl/egl_external_platform.d"
        "GBM_BACKENDS_PATH=$GL/lib/gbm"
        "VK_DRIVER_FILES=$icds" "VK_ICD_FILENAMES=$icds")
fi

if command -v sway >/dev/null && [ -z "${NVGPU_SWAY_FROM_NIX:-}" ]; then
    SWAY=(sway)
else
    export NIX_CONFIG=${NIX_CONFIG:-experimental-features = nix-command flakes}
    SWAY=(nix shell nixpkgs#sway -c sway)
fi

mkdir -p "$RIG/logs"
(umask 077 && mkdir -p "$RIG/run")
PARENT=${XDG_RUNTIME_DIR:-$RIG/run}
RT=$(mktemp -d "$PARENT/nvgpu-sway.XXXXXX")
chmod 0700 "$RT"
POINTER=$RIG/run/headless-sway.socket
LOG=$RIG/logs/headless-sway.$(date +%Y%m%d-%H%M%S).log
SWAY_PID=
# shellcheck disable=SC2329  # run by the EXIT trap
cleanup() {
    [ -z "$SWAY_PID" ] || kill "$SWAY_PID" 2>/dev/null || true
    [ -z "$SWAY_PID" ] || wait "$SWAY_PID" 2>/dev/null || true
    # Only our own pointer: another instance may have written a newer one.
    if [ -f "$POINTER" ] && [ "$(cat "$POINTER")" = "${SOCK:-}" ]; then rm -f "$POINTER"; fi
    rm -rf "$RT"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Nothing to spawn -- no bar, no background, no Xwayland -- and one output.
cat > "$RT/config" <<EOF
xwayland disable
output HEADLESS-1 mode $MODE
EOF

env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET -u DISPLAY -u SWAYSOCK \
    "XDG_RUNTIME_DIR=$RT" "${ENVS[@]}" \
    "${SWAY[@]}" --unsupported-gpu -c "$RT/config" > "$LOG" 2>&1 < /dev/null &
SWAY_PID=$!

SOCK=
for _ in $(seq 1 300); do
    for s in "$RT"/wayland-*; do
        [ -S "$s" ] && { SOCK=$s; break 2; }
    done
    kill -0 "$SWAY_PID" 2>/dev/null || break
    sleep 0.1
done
[ -n "$SOCK" ] || { tail -n 20 "$LOG" >&2; die "sway did not come up; see $LOG"; }
printf '%s\n' "$SOCK" > "$POINTER"
echo "headless sway: renderer $RENDERER${NODE:+ on $NODE}, output HEADLESS-1 $MODE"
echo "socket:  $SOCK   (also in $POINTER)"
echo "log:     $LOG"
echo "run:     scripts/run-guest.sh --wayland-socket $SOCK <probe>"
echo "Ctrl-C stops it."
RC=0
wait "$SWAY_PID" || RC=$?
SWAY_PID=
echo "sway exited ($RC); see $LOG" >&2
exit "$RC"
