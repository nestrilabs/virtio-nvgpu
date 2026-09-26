#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# TESTING.md "Security negative tests": the guest-reachable requests the
# backend must refuse, fired from a real guest process
# (rig/verify/sec-negative.sh). Any mode; the KMS/lease tests run when a
# KMS file is available:
#   nvgpu_secneg_kms=auto|card|lease|none   (default auto: the card node when
#       the backend offers card nodes (--kms-card), else a lease if
#       /dev/nvgpu-wl exists, else none)
# Afterwards, on the HOST: dmesg clean, backend still serving.
. /opt/nvgpu/probe-common.sh
probe_init secneg 150
MODE=$(arg secneg_kms auto)

load_module || finish

section "ctl + render"
step "sec-negative.sh (ctl + render tests)" 60 bash "$NVGPU_VERIFY/sec-negative.sh"

CARD=$(arg card "$(ls /dev/dri/card* 2>/dev/null | head -n 1)")
case $MODE in
    auto)
        if [ -n "$CARD" ] && kms_card_offered; then MODE=card; elif [ -e /dev/nvgpu-wl ]; then MODE=lease; else MODE=none; fi ;;
esac
case $MODE in
    card)
        section "KMS tests on $CARD"
        step "sec-negative.sh --kms $CARD" 60 bash "$NVGPU_VERIFY/sec-negative.sh" --kms "$CARD"
        ;;
    lease)
        section "KMS tests on a lease"
        if start_wl_daemon wayland-0; then
            step "sec-negative.sh --kms-fd <lease>" 60 \
                nvgpu-lease --timeout 20 -- bash "$NVGPU_VERIFY/sec-negative.sh" --kms-fd '{fd}'
            wl_daemon_alive
        fi
        ;;
    *) skip "KMS/lease tests (no card node and no Wayland lease device)" ;;
esac
say "now on the HOST: dmesg must be clean and the backend still serving"
finish
