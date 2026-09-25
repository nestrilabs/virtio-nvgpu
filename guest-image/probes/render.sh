#!/bin/bash
# Rendering without a display: the NVIDIA 595.99.02 userspace in the guest
# drives the host GPU through RM/UVM forwarding. nvidia-smi, Vulkan, EGL, and
# the CUDA driver API (with a kernel launch). Any display mode.
#   nvgpu_frames=N   (unused here)
. /opt/nvgpu/probe-common.sh
probe_init render 170

load_module || finish
check_versions

section "nvidia-smi"
if step "nvidia-smi -L" 30 nvidia-smi -L; then
    out=$(timeout -k 5 45 nvidia-smi -q 2>&1)
    rc=$?
    printf '%s\n' "$out" | sed 's/^/    /'
    img=$(sed -n 's/^nvidia-userspace \([^ ]*\).*/\1/p' /etc/nvgpu/manifest)
    if [ "$rc" = 0 ] && printf '%s\n' "$out" | grep -Eq "Driver Version *: *$img"; then
        pass "nvidia-smi -q (Driver Version $img)"
    else
        fail "nvidia-smi -q (exit $rc, or Driver Version is not $img)"
    fi
fi

section "Vulkan"
out=$(timeout -k 5 60 vulkaninfo --summary 2>&1)
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
if [ "$rc" = 0 ] && printf '%s\n' "$out" | grep -q 'driverID *= *DRIVER_ID_NVIDIA_PROPRIETARY'; then
    pass "vulkaninfo --summary (NVIDIA proprietary driver)"
else
    fail "vulkaninfo --summary (exit $rc, or no NVIDIA proprietary device)"
fi
for ext in VK_EXT_physical_device_drm VK_KHR_display VK_EXT_acquire_drm_display \
    VK_EXT_image_drm_format_modifier VK_KHR_external_semaphore_fd; do
    if timeout 60 vulkaninfo 2>/dev/null | grep -q "$ext"; then
        say "advertised: $ext"
    else
        say "NOT advertised: $ext"
    fi
done

section "EGL"
# One platform at a time, so one that crashes (egl-gbm dereferences a NULL
# gbm device when no DRM node opens) does not hide the others.
egl_ok=0
for plat in gbm surfaceless all; do
    if [ "$plat" = all ]; then out=$(timeout -k 5 60 eglinfo -B 2>&1); else out=$(timeout -k 5 60 eglinfo -B -p "$plat" 2>&1); fi
    rc=$?
    printf '%s\n' "$out" | sed "s/^/    [$plat] /"
    if printf '%s\n' "$out" | grep -q 'EGL vendor string: NVIDIA'; then
        say "eglinfo -p $plat: NVIDIA EGL display up"
        egl_ok=1
    else
        say "eglinfo -p $plat: no NVIDIA EGL display (exit $rc)"
    fi
done
if [ "$egl_ok" = 1 ]; then pass "EGL: an NVIDIA EGL display initialised"; else fail "EGL: no platform initialised the NVIDIA EGL vendor"; fi

section "CUDA"
step "cuda-smoke (driver API, 16 MiB round trip, PTX kernel)" 90 cuda-smoke

section "vkcube"
# vkcube has no WSI that needs no display (no headless surface); the Wayland
# and display stages run it.
skip "vkcube: no display-less WSI; run wayland.sh / vkdisplay.sh"
finish
