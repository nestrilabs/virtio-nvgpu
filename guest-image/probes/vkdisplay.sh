#!/bin/bash
# TESTING.md stage 5: VK_EXT_acquire_drm_display / VK_KHR_display on a leased
# output. Launch as for lease.sh (--wayland-socket ... --wayland-lease, a
# leasable monitor), or in compositor-VM mode (--kms-card), where the guest
# card node is the DRM fd.
#   nvgpu_frames=N       frames to present (default 300)
#   nvgpu_connector=NAME which offered connector (default: the first)
#   nvgpu_lease_gap=S    pause between leases (default 6)
. /opt/nvgpu/probe-common.sh
probe_init vkdisplay 170
FRAMES=$(arg frames 300)
GAP=$(arg lease_gap 6)
CONN=$(arg connector)

load_module || finish

section "extensions"
vk=$(timeout 60 vulkaninfo 2>/dev/null)
for ext in VK_KHR_display VK_EXT_direct_mode_display VK_EXT_acquire_drm_display; do
    if grep -q "$ext" <<<"$vk"; then pass "$ext advertised"; else fail "$ext not advertised"; fi
done

if [ -e /dev/nvgpu-wl ]; then
    LEASE=(nvgpu-lease --timeout 20 ${CONN:+--connector "$CONN"} --)
    section "daemon"
    start_wl_daemon wayland-0 || finish
    section "vkAcquireDrmDisplayEXT on the lease"
    step "vk-acquire-display, $FRAMES frames" 90 "${LEASE[@]}" vk-acquire-display --frames "$FRAMES"
    sleep "$GAP"
    section "vkcube --wsi display (VK_KHR_display, lease held)"
    # vkcube does not acquire the display through the lease fd; whether the
    # driver lets it use the head the lease holds is what this shows.
    # With only a lease held, vkcube segfaults natively too (host vkcube on
    # the same Hyprland lease, NVIDIA 595.99.02, 2026-09-26): SIGSEGV is a
    # SKIP, anything else is judged as usual.
    say "---- vkcube --wsi display --c $FRAMES: ${LEASE[*]} vkcube --wsi display --c $FRAMES"
    timeout -k 5 60 "${LEASE[@]}" vkcube --wsi display --c "$FRAMES"
    rc=$?
    if [ "$rc" = 0 ]; then
        pass "vkcube --wsi display --c $FRAMES"
    elif [ "$rc" = 139 ]; then
        skip "vkcube --wsi display with a lease held: SIGSEGV; it does the same natively"
    else
        fail "vkcube --wsi display --c $FRAMES (exit $rc)"
    fi
    section "daemon"
    wl_daemon_alive
else
    card=$(arg card "$(ls /dev/dri/card* 2>/dev/null | head -n 1)")
    if [ -z "$card" ]; then
        fail "neither /dev/nvgpu-wl (lease mode) nor a /dev/dri/card* (compositor-VM mode)"
        finish
    fi
    section "vkAcquireDrmDisplayEXT on $card (compositor-VM mode)"
    step "vk-acquire-display --device $card, $FRAMES frames" 90 vk-acquire-display --device "$card" --frames "$FRAMES"
    section "vkcube --wsi display"
    step "vkcube --wsi display --c $FRAMES" 60 vkcube --wsi display --c "$FRAMES"
fi
finish
