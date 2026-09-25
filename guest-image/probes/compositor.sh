#!/bin/bash
# TESTING.md stage 6: compositor-VM mode. The guest drives the host card
# directly. Launch with --kms-card, and no compositor on the host card.
#   nvgpu_card=PATH          the guest card node (default: the first /dev/dri/card*)
#   nvgpu_secs=S             how long the compositor runs (default 30)
#   nvgpu_frames=N           flips for lease-flip (default 120)
#   nvgpu_comp=sway|hyprland the guest compositor (default sway)
#   nvgpu_wlr_renderer=R     WLR_RENDERER for sway: vulkan (default) or gles2
. /opt/nvgpu/probe-common.sh
probe_init compositor 170
SECS=$(arg secs 30)
FRAMES=$(arg frames 120)

load_module || finish

section "guest-check.sh (card nodes expected)"
step "scripts/verify/guest-check.sh" 60 bash "$NVGPU_VERIFY/guest-check.sh"
CARD=$(arg card "$(ls /dev/dri/card* 2>/dev/null | head -n 1)")
if [ -z "$CARD" ] || [ ! -e "$CARD" ]; then
    fail "no guest card node (backend not started with --kms-card?)"
    finish
fi
if ! kms_card_offered; then
    fail "the backend offers no card nodes (backend caps $(printf '0x%x' "$(backend_caps)")): start it with --kms-card"
    finish
fi
pass "guest card node: $CARD"

section "KMS enumerate / drive ($CARD)"
# modetest -D takes a bus id, not a path; the shim lets kms-smoke.sh's
# `modetest -D /dev/dri/cardN` open the path it means.
step "kms-smoke.sh $CARD (enumerate)" 30 env LD_PRELOAD="$NVGPU_SHIM" bash "$NVGPU_VERIFY/kms-smoke.sh" "$CARD"
step "drm_info $CARD" 30 drm_info "$CARD"
step "lease-flip on $CARD, $FRAMES flips (modeset + page flips)" 60 \
    "$NVGPU_VERIFY/lease-flip.sh" --device "$CARD" --frames "$FRAMES"

. /opt/nvgpu/compositor-common.sh
section "$COMP on $CARD"
start_comp "$CARD" || finish
WAYLAND_DISPLAY=$COMP_WL step "vkcube --wsi wayland on the guest compositor, 300 frames" 60 \
    vkcube --wsi wayland --c 300
WAYLAND_DISPLAY=$COMP_WL run_for "weston-simple-egl on the guest compositor" 8 weston-simple-egl
section "master arbitration"
# The compositor is DRM master of $CARD; another opener must not modeset it.
say "---- lease-flip while $COMP holds master (a refusal is the PASS)"
if timeout -k 5 20 "$NVGPU_VERIFY/lease-flip.sh" --device "$CARD" --frames 1; then
    fail "a second opener modeset $CARD while $COMP was master"
else
    pass "a second opener could not modeset $CARD while $COMP was master"
fi
rest=$((SECS - (SECONDS - COMP_T0)))
[ "$rest" -gt 0 ] && sleep "$rest"
stop_comp
finish
