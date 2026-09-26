#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# TESTING.md stage 3 (and stage 8's first check): guest clients of the host
# compositor through nvgpu-wl-guest. Launch with --wayland-socket.
#   nvgpu_secs=S     how long each timed client runs (default 10)
#   nvgpu_frames=N   vkcube frames (default 300)
#
# What to look at on the host while it runs: weston-simple-dmabuf-feedback and
# weston-simple-egl -f are fullscreen, so Hyprland (render:direct_scanout=2)
# should log "Entered a direct scanout" for them.
. /opt/nvgpu/probe-common.sh
probe_init wayland 170
SECS=$(arg secs 10)
FRAMES=$(arg frames 300)
TRACE=/var/log/nvgpu/vkcube-wayland.trace

load_module || finish
section "daemon"
start_wl_daemon wayland-0 || finish

section "globals"
if out=$(timeout -k 5 20 wayland-info 2>&1); then
    pass "wayland-info through the proxy"
else
    fail "wayland-info through the proxy (exit $?)"
fi
printf '%s\n' "$out" | grep -E "^interface: " | sed 's/^/    /'
for g in zwp_linux_dmabuf_v1 wp_linux_drm_syncobj_manager_v1 wp_drm_lease_device_v1 \
    wp_presentation xdg_wm_base; do
    if printf '%s\n' "$out" | grep -q "'$g'"; then say "global offered: $g"; else say "global NOT offered: $g"; fi
done
printf '%s\n' "$out" | sed -n '/zwp_linux_dmabuf_v1/,/^interface/p' | grep -iE 'main device|tranche|target device|flags|0x' | head -n 40 | sed 's/^/    /'

section "Vulkan WSI (vkcube --wsi wayland)"
# The WAYLAND_DEBUG trace holds the modifiers the client picked
# (zwp_linux_buffer_params_v1.add) and the syncobj timelines (stage 8.1).
say "---- vkcube --wsi wayland --c $FRAMES (WAYLAND_DEBUG trace in $TRACE)"
WAYLAND_DEBUG=1 timeout -k 5 90 vkcube --wsi wayland --c "$FRAMES" 2> "$TRACE"
rc=$?
if [ "$rc" = 0 ]; then pass "vkcube --wsi wayland, $FRAMES frames"; else fail "vkcube --wsi wayland (exit $rc)"; fi
grep -v '^\[' "$TRACE" | tail -n 5 | sed 's/^/    /'
say "dmabuf modifiers the client added (vendor 0x03 = NVIDIA):"
grep -E 'zwp_linux_buffer_params_v1#[0-9]+\.add\(' "$TRACE" | sed -E 's/.*add\(/add(/' | sort | uniq -c | head -n 8 | sed 's/^/    /'
if grep -q 'wp_linux_drm_syncobj' "$TRACE"; then
    say "explicit sync: wp_linux_drm_syncobj used ($(grep -c 'wp_linux_drm_syncobj_timeline_v1' "$TRACE") timeline messages)"
else
    say "explicit sync: no wp_linux_drm_syncobj traffic"
fi

section "EGL clients"
run_for "weston-simple-egl (windowed)" "$SECS" weston-simple-egl
run_for "weston-simple-egl -f (fullscreen: direct-scanout candidate)" "$SECS" weston-simple-egl -f
run_for "weston-simple-dmabuf-egl" "$SECS" weston-simple-dmabuf-egl
# These three also fail run natively on the host, against the headless sway,
# with this image's own 595.99.02 userspace and no proxy in between -- so a
# early exit here is theirs, not the proxy's, and is reported as such:
# - simple-dmabuf-feedback: NVIDIA's GBM returns no bo for what it asks
#   (create_dmabuf_buffer asserts), while simple-dmabuf-egl's bo is fine;
# - eglgears/es2gears: eglut's seat_capabilities destroys a pointer it never
#   made when the seat has no pointer, which a headless compositor's has not.
known_early_exit() {
    local name=$1 why=$2
    shift 2
    say "---- $name (for ${SECS}s): $*"
    timeout -s TERM -k 5 "$SECS" "$@"
    local rc=$?
    if [ "$rc" = 124 ]; then
        pass "$name (ran ${SECS}s)"
    else
        skip "$name exited early (status $rc): $why; it does the same natively"
    fi
}
known_early_exit "weston-simple-dmabuf-feedback (fullscreen, scanout tranches)" \
    "NVIDIA GBM gives no bo for its tranche's format" weston-simple-dmabuf-feedback
known_early_exit "eglgears_wayland" "eglut crashes on a seat with no pointer" eglgears_wayland
known_early_exit "es2gears_wayland" "eglut crashes on a seat with no pointer" es2gears_wayland

section "shm client"
run_for "weston-simple-shm (wl_shm copy path)" "$SECS" weston-simple-shm
run_for "foot (a terminal: shm + text)" "$SECS" foot --override='main.font=DejaVu Sans Mono:size=12' -e sh -c 'echo virtio-nvgpu; sleep 3600'

section "daemon"
wl_daemon_alive
finish
