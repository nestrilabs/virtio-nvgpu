#!/bin/bash
# TESTING.md stage 1 (and 0.1): the module loads, HELLO negotiates protocol v2,
# the nodes are there, and the driver's own libraries advertise the extensions
# the later stages need (scripts/verify/guest-check.sh). Any display mode.
. /opt/nvgpu/probe-common.sh
probe_init stage1 90

if load_module; then
    section "HELLO"
    if dmesg | grep -q 'virtio-gpu-nv: protocol v2'; then
        pass "protocol v2 negotiated: $(dmesg | grep 'virtio-gpu-nv: protocol v2' | tail -n 1 | sed 's/^\[[^]]*\] //')"
    elif dmesg | grep -q 'backend speaks protocol v1'; then
        fail "backend is protocol v1 only (update the backend)"
    else
        fail "no 'virtio-gpu-nv: protocol v2' line in dmesg"
    fi
    check_versions

    section "guest-check.sh"
    step "scripts/verify/guest-check.sh" 60 bash "$NVGPU_VERIFY/guest-check.sh"
fi
finish
