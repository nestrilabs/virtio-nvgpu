#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The first thing to run inside a guest: is the transport up, are the nodes
# there, and do the driver's own libraries advertise the extensions the display
# paths need? It changes nothing and drives no display, so it is safe to run any
# time. A red line here explains a failure three steps later.
#
# Usage: guest-check.sh
# Exit status is the number of checks that FAILED (warnings do not count).
set -euo pipefail


fails=0
ok()   { printf '  ok    %s\n' "$1"; }
bad()  { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
warn() { printf '  warn  %s\n' "$1"; }

echo "== protocol =="
# The guest driver logs the negotiated protocol at probe. v2 is what every
# display feature rides on; v1 means an old backend, or HELLO failed.
# Read the log once: under pipefail, `dmesg | grep -q` fails when grep exits
# at the first match and dmesg dies of SIGPIPE -- a match reads as a miss.
klog=$(dmesg 2>/dev/null || true)
if grep -q 'virtio-gpu-nv: protocol v2' <<<"$klog"; then
    line=$(grep 'virtio-gpu-nv: protocol v2' <<<"$klog" | tail -n 1)
    ok "protocol v2 negotiated"
    printf '        %s\n' "${line#*] }"
elif grep -q 'virtio-gpu-nv: backend speaks protocol v1' <<<"$klog"; then
    bad "backend is v1 only -- no display features (update the backend)"
elif grep -q 'virtio-gpu-nv' <<<"$klog"; then
    warn "driver present but no v2 HELLO line (check RUST_LOG on the backend)"
else
    bad "virtio_gpu_nv not loaded, or dmesg unreadable"
fi

echo "== device nodes =="
# UVM is served only with the backend's --allow-compute (BCAP_COMPUTE, bit 10
# of the caps the HELLO line reports); without it the node must NOT exist.
caps=$(grep -o 'backend caps 0x[0-9a-f]*' <<<"$klog" | tail -n 1 | awk '{print $3}')
compute=$(( ${caps:-0} & (1 << 10) ))
nodes="/dev/nvidiactl /dev/nvidia0 /dev/nvidia-modeset"
[ "$compute" != 0 ] && nodes="$nodes /dev/nvidia-uvm"
for n in $nodes; do
    if [ -e "$n" ]; then ok "$n"; else bad "$n missing"; fi
done
if [ "$compute" = 0 ]; then
    if [ -e /dev/nvidia-uvm ]; then bad "/dev/nvidia-uvm exists though the backend serves no compute"
    else ok "no /dev/nvidia-uvm (backend without --allow-compute)"; fi
fi
if ls /dev/dri/card* >/dev/null 2>&1; then
    ok "DRI card node(s): $(echo /dev/dri/card* | tr ' ' ',')"
else
    warn "no /dev/dri/card* (expected only in compositor-VM mode, --kms-card)"
fi
if ls /dev/dri/renderD* >/dev/null 2>&1; then
    ok "render node(s): $(echo /dev/dri/renderD* | tr ' ' ',')"
else
    bad "no /dev/dri/renderD* render node"
fi

echo "== Vulkan =="
if command -v vulkaninfo >/dev/null 2>&1; then
    vk=$(vulkaninfo 2>/dev/null || true)
    if [ -z "$vk" ]; then
        bad "vulkaninfo produced no output (ICD not found? check VK_DRIVER_FILES)"
    else
        # Instance extensions for picking a DRM display, and the device
        # extension that ties a VkPhysicalDevice to a DRM node.
        for ext in VK_KHR_display VK_EXT_acquire_drm_display VK_EXT_physical_device_drm; do
            if grep -q "$ext" <<<"$vk"; then ok "$ext"; else
                # acquire_drm_display / KHR_display only matter for the direct
                # (VK_KHR_display / vkAcquireDrmDisplayEXT) path.
                warn "$ext not advertised"
            fi
        done
    fi
else
    warn "vulkaninfo not on PATH (nix shell nixpkgs#vulkan-tools)"
fi

echo "== EGL =="
# EGL_ANDROID_native_fence_sync is what carries explicit fences out of EGL.
if command -v eglinfo >/dev/null 2>&1; then
    if grep -q EGL_ANDROID_native_fence_sync <<<"$(eglinfo 2>/dev/null || true)"; then
        ok "EGL_ANDROID_native_fence_sync"
    else
        warn "EGL_ANDROID_native_fence_sync not advertised (explicit-sync EGL clients)"
    fi
else
    warn "eglinfo not on PATH (nix shell nixpkgs#mesa-demos); skipping EGL checks"
fi

echo
if [ "$fails" -eq 0 ]; then
    echo "guest-check: transport and nodes look good."
else
    echo "guest-check: $fails hard check(s) failed."
fi
exit "$fails"
