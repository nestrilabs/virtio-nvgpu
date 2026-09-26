#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The capture test's host half, as rig/run-guest.sh's NVGPU_BEFORE_VMM hook:
# start nvgpu-inject-test against the backend's --inject-socket in the
# background, with the guest image's copy of the host's NVIDIA userspace (the
# same EGL and GBM a portal would use), wait for its ids and tokens, and print
# them as the one line run-guest.sh adds to the guest's command line.
#
#   NVGPU_BEFORE_VMM=rig/rig-tools/inject-hook.sh \
#       rig/run-guest.sh --inject capture cap1
#
# The helper keeps its connection -- and so the ids -- until the backend hangs
# up at the end of the run, or NVGPU_INJECT_HOLD seconds (600), painting the
# live buffer meanwhile, and running NVGPU_INJECT_PINGPONG (200) frames of
# explicit sync with the guest. NVGPU_INJECT_ARGS adds arguments (--size,
# --buffers, --frames). Its output goes to NVGPU_HOOK_LOG, the explicit-sync
# timings at the end of the run.
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
: "${NVGPU_INJECT_SOCKET:?run with rig/run-guest.sh --inject}"
: "${NVGPU_RUN_DIR:?run as the NVGPU_BEFORE_VMM of rig/run-guest.sh}"
LOG=${NVGPU_HOOK_LOG:-$NVGPU_RUN_DIR/inject.log}
BIN=${NVGPU_INJECT_TEST:-$RIG/bin/nvgpu-inject-test}
[ -x "$BIN" ] || { echo "no $BIN: rig/rig-tools/build.sh" >&2; exit 1; }

root=$(readlink "$RIG/guest/result") || { echo "no $RIG/guest/result: build the image" >&2; exit 1; }
phys() { for b in "$HOME/.local/share/nix/root" ""; do [ -e "$b$1" ] && { echo "$b$1"; return; }; done; echo "$1"; }
OD=$(readlink "$(phys "$root")/etc/nvgpu/opengl-driver")

export NIX_CONFIG=${NIX_CONFIG:-experimental-features = nix-command flakes}
OUT=$NVGPU_RUN_DIR/inject.words
rm -f "$OUT"
# shellcheck disable=SC2086
setsid nix shell "$OD" -c env \
    LD_LIBRARY_PATH="$OD/lib" \
    __EGL_VENDOR_LIBRARY_FILENAMES="$OD/share/glvnd/egl_vendor.d/10_nvidia.json" \
    __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS="$OD/share/egl/egl_external_platform.d" \
    GBM_BACKENDS_PATH="$OD/lib/gbm" \
    "$BIN" --socket "$NVGPU_INJECT_SOCKET" --out "$OUT" --hold "${NVGPU_INJECT_HOLD:-600}" \
    --pingpong "${NVGPU_INJECT_PINGPONG:-200}" ${NVGPU_INJECT_ARGS:-} </dev/null >"$LOG" 2>&1 &
PID=$!
for _ in $(seq 1 600); do
    [ -s "$OUT" ] && break
    kill -0 "$PID" 2>/dev/null || break
    sleep 0.1
done
if [ ! -s "$OUT" ]; then
    echo "inject-hook: nvgpu-inject-test gave no ids; its log:" >&2
    tail -n 30 "$LOG" >&2
    kill "$PID" 2>/dev/null
    exit 1
fi
grep 'inject-test: ' "$LOG" >&2
cat "$OUT"
