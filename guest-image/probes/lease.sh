#!/bin/bash
# TESTING.md stage 4 (and stage 9's lease round trip): the host compositor
# leases an output; the guest takes the lease through nvgpu-wl-guest with
# nvgpu-lease and drives it with ordinary KMS. Launch with --wayland-socket
# ... --wayland-lease and a leasable monitor.
#   nvgpu_connector=NAME   which offered connector (default: the first)
#   nvgpu_frames=N         flips / frames (default 120 / 300 for kmscube)
#   nvgpu_flip_hold=S      seconds lease-flip shows its first frame still
#                          (colour bars), for someone watching (default 3)
#   nvgpu_lease_gap=S      pause between leases (default 6; the backend spaces
#                          one VM's lease requests, --wayland-lease-interval)
#
# Each KMS tool gets the lease as a descriptor (nvgpu-lease runs it as a
# child); path-only tools open /dev/dri/lease through libnvgpu-shim.so.
. /opt/nvgpu/probe-common.sh
probe_init lease 170
FRAMES=$(arg frames 120)
GAP=$(arg lease_gap 6)
CONN=$(arg connector)
LEASE=(nvgpu-lease --timeout 20 ${CONN:+--connector "$CONN"} --)
SHIM=(env LD_PRELOAD="$NVGPU_SHIM")

load_module || finish
section "daemon"
start_wl_daemon wayland-0 || finish

section "lease device"
if ! step "nvgpu-lease --list (connectors on offer)" 30 nvgpu-lease --timeout 20 --list; then
    say "no lease device or no connector: host needs --wayland-lease and a 'leasable' monitor"
    finish
fi

section "lease -> lease-flip"
step "lease-flip on the lease fd, $FRAMES flips" 60 \
    "${LEASE[@]}" "$NVGPU_VERIFY/lease-flip.sh" --fd '{fd}' --frames "$FRAMES" --hold "$(arg flip_hold 3)"

sleep "$GAP"
section "lease -> drm_info / modetest"
step "drm_info on the lease" 30 "${LEASE[@]}" "${SHIM[@]}" drm_info /dev/dri/lease

sleep "$GAP"
step "modetest -c -p on the lease" 30 "${LEASE[@]}" "${SHIM[@]}" modetest -D /dev/dri/lease -c -p

sleep "$GAP"
section "lease -> kmscube (GBM + NVIDIA EGL on the leased output)"
# NVIDIA's GBM buffers on a lease fd: SETCRTC answers EINVAL natively too
# (kmscube on the host against the same Hyprland lease, NVIDIA 595.99.02,
# 2026-09-26), while dumb buffers (lease-flip, modetest) scan out. So that
# exact failure is a SKIP; anything else about kmscube still FAILs.
say "---- kmscube on the lease, 300 frames: ${LEASE[*]} ${SHIM[*]} kmscube -D /dev/dri/lease -c 300"
timeout -k 5 60 "${LEASE[@]}" "${SHIM[@]}" kmscube -D /dev/dri/lease -c 300 >/tmp/kmscube.log 2>&1
rc=$?
grep -v extensions /tmp/kmscube.log
if [ "$rc" = 0 ]; then
    pass "kmscube on the lease, 300 frames"
elif grep -q 'failed to set mode: Invalid argument' /tmp/kmscube.log; then
    skip "kmscube on the lease: SETCRTC EINVAL on an NVIDIA GBM buffer; it does the same natively"
else
    fail "kmscube on the lease, 300 frames (exit $rc)"
fi

sleep "$GAP"
section "lease round trip (stage 9): take it again after release"
step "second lease + lease-flip, 60 flips" 45 \
    "${LEASE[@]}" "$NVGPU_VERIFY/lease-flip.sh" --fd '{fd}' --frames 60

section "daemon"
wl_daemon_alive
say "on the HOST now: the leased monitor must be back on the desktop, not dark (stage 9)"
finish
