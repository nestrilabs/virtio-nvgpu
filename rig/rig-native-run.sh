#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Run a command on the host the way the guest runs it, to tell a failure of
# ours from one the app has natively: the guest image's own NVIDIA userspace
# (its /run/opengl-driver, the same 595.99.02 files, pinned the way
# rig/guest-image/probes/env.sh pins them) and the image's own programs on PATH,
# against the rig's headless sway -- never the desktop.
#
# Usage: rig/rig-native-run.sh [--live] [--timeout S] -- CMD [ARGS...]
#   --live      against the session's compositor instead (a window appears
#               on the desktop; for comparing with a --live app pass only)
#   --timeout   seconds before CMD is stopped (default 60); the exit status
#               is CMD's, 124 when it ran that long
#   --no-uvm    without /dev/nvidia-uvm, as a guest without --allow-compute
#               has it (bubblewrap: a /dev of its own with the other nodes)
#
# Needs a built image (.rig/guest/result) and, without --live,
# rig/rig-headless-sway.sh running. The image's store paths are read from
# the chroot store's copy when there is one (nix without root).
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=rig/lib.sh
. "$REPO/rig/lib.sh"
RIG=${NVGPU_RIG:-$REPO/.rig}
LIVE=0
NOUVM=0
SECS=60
while [ $# -gt 0 ]; do
    case $1 in
        --live) LIVE=1; shift ;;
        --timeout) SECS=$2; shift 2 ;;
        --no-uvm) NOUVM=1; shift ;;
        --) shift; break ;;
        *) break ;;
    esac
done
[ $# -gt 0 ] || { sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }

root=$(readlink "$RIG/guest/result") || { echo "no $RIG/guest/result: build the image" >&2; exit 1; }
OD=$(readlink "$(rig_phys "$root")/etc/nvgpu/opengl-driver")
SW=$(readlink "$(rig_phys "$root")/etc/nvgpu/sw")
rig_nv_env "$OD"

if [ "$LIVE" = 1 ]; then
    WL=${XDG_RUNTIME_DIR:-}/${WAYLAND_DISPLAY:-}
else
    WL=$(cat "$RIG/run/headless-sway.socket" 2>/dev/null) || WL=
fi
[ -S "$WL" ] || { echo "no compositor socket ($WL); start rig/rig-headless-sway.sh" >&2; exit 1; }
RT=$(mktemp -d "${TMPDIR:-/tmp}/nvgpu-native.XXXXXX")
chmod 0700 "$RT"
ln -s "$WL" "$RT/wayland-0"
trap 'rm -rf "$RT"' EXIT

export NIX_CONFIG=${NIX_CONFIG:-experimental-features = nix-command flakes}
PRE=()
PKGS=("$SW" "$OD")
if [ "$NOUVM" = 1 ]; then
    PKGS+=(nixpkgs#bubblewrap)
    PRE=(bwrap --dev-bind / / --dev /dev)
    for n in /dev/nvidiactl /dev/nvidia0 /dev/nvidia-modeset /dev/dri /dev/shm /dev/udmabuf; do
        [ -e "$n" ] && PRE+=(--dev-bind "$n" "$n")
    done
    PRE+=(--)
fi
# nix shell makes the chroot store's paths visible; the image's sw env first.
nix shell "${PKGS[@]}" -c ${PRE[@]+"${PRE[@]}"} env -u DISPLAY \
    XDG_RUNTIME_DIR="$RT" WAYLAND_DISPLAY=wayland-0 \
    "${NV_ENV[@]}" \
    LIBVA_DRIVERS_PATH="$OD/lib/dri" \
    OCL_ICD_VENDORS="$OD/etc/OpenCL/vendors" \
    FONTCONFIG_FILE="$(rig_phys "$root")/etc/fonts/fonts.conf" \
    timeout -s TERM -k 5 "$SECS" "$@"
