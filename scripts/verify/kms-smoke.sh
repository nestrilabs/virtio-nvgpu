#!/usr/bin/env bash
# modetest smoke for a guest KMS device: enumerate, then drive it. Use it in
# compositor-VM mode (--kms-card, a guest card node) or on an adopted lease fd's
# device path. It leans on libdrm's `modetest`; get it with
#   nix shell nixpkgs#libdrm       (modetest ships in the libdrm package)
#
# Usage: kms-smoke.sh <device> [connector@crtc:mode]
#   kms-smoke.sh /dev/dri/card1
#   kms-smoke.sh /dev/dri/card1 DP-2@crtc:preferred   (drive a specific output)
#
# With no output given it only enumerates (safe). With one, it does a blocking
# modeset and holds the picture until you press enter -- watch the monitor.
set -euo pipefail

if [ "$#" -lt 1 ]; then
    echo "usage: kms-smoke.sh <device> [connector@crtc:mode]" >&2
    exit 2
fi
dev="$1"
spec="${2:-}"

if ! command -v modetest >/dev/null 2>&1; then
    echo "modetest not found (nix shell nixpkgs#libdrm)" >&2
    exit 2
fi

# modetest addresses a device by DRM module name or by path. -M nvidia-drm picks
# the guest's nvidia-drm node; if several exist, pass the exact path with -D.
MT=(modetest -D "$dev")

echo "== connectors, encoders, CRTCs, planes =="
"${MT[@]}" -c -e -p || {
    echo "modetest could not read $dev -- is it a KMS/lease fd with master?" >&2
    exit 1
}

echo
echo "== formats and modifiers on planes (for the modifier cross-check) =="
"${MT[@]}" -p | sed -n '/planes:/,$p' || true

if [ -n "$spec" ]; then
    conn="${spec%@*}"
    rest="${spec#*@}"
    crtc="${rest%%:*}"
    mode="${rest#*:}"
    echo
    echo "== modeset $conn on $crtc at $mode (blocking) =="
    # -s does a full modeset with a test pattern and blocks until enter; a
    # commit that never returns, or ADDFB failing where bare metal works, is the
    # failure. Nonblocking -v flips continuously and prints the rate.
    "${MT[@]}" -s "${conn}@${crtc}:${mode}"
fi
