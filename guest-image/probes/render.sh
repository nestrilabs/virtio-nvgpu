#!/bin/bash
# Rendering without a display: the NVIDIA 595.99.02 userspace in the guest
# drives the host GPU through RM/UVM forwarding. nvidia-smi, Vulkan, EGL, and
# the CUDA driver API (with a kernel launch). Any display mode.
#   nvgpu_frames=N   (unused here)
#   nvgpu_compute=1  the backend runs with --allow-compute (run-guest.sh
#                    --allow-compute or NVGPU_COMPUTE=1 passes it): CUDA must
#                    work. Otherwise (0, the default) the guest must have no
#                    UVM device, CUDA must fail cleanly, and everything else
#                    must work without it.
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
if [ "$(arg compute 0)" = 1 ]; then
    # Context creation needs the UVM aperture (semaphore pools) and
    # OS-descriptor memory by guest page (ARCHITECTURE.md §5); both are
    # served with --allow-compute, so any failure is one.
    if [ -c /dev/nvidia-uvm ]; then
        pass "/dev/nvidia-uvm is there (--allow-compute)"
    else
        fail "no /dev/nvidia-uvm although the backend was started with --allow-compute"
    fi
    step "cuda-smoke (driver API, 16 MiB round trip, PTX kernel)" 90 cuda-smoke
else
    # Without --allow-compute the backend serves no UVM (SECURITY.md,
    # "Compute"): the guest makes no /dev/nvidia-uvm, as on a host whose
    # nvidia-uvm is not loaded, and CUDA finds nothing to run on. What this
    # section checks is that it says so and exits, rather than crashing or
    # hanging -- and the sections above, which ran without UVM, are the proof
    # that graphics does not need it.
    if [ -e /dev/nvidia-uvm ] || grep -q ' nvidia-uvm$' /proc/devices; then
        fail "compute is off, yet the guest has a UVM device ($(ls -l /dev/nvidia-uvm* 2>&1 | tr '\n' ' '))"
    else
        pass "no UVM device without --allow-compute"
    fi
    out=$(timeout -k 5 60 cuda-smoke 2>&1)
    rc=$?
    printf '%s\n' "$out" | head -n 10 | sed 's/^/    /'
    if [ "$rc" = 0 ]; then
        fail "cuda-smoke succeeded without --allow-compute"
    elif [ "$rc" -lt 124 ] && printf '%s\n' "$out" |
        grep -Eq 'FAIL (cuInit|cuDriverGetVersion|cuDeviceGet[A-Za-z]*|cuCtxCreate|no CUDA device)'; then
        pass "cuda-smoke finds no usable device without --allow-compute (exit $rc: $(printf '%s\n' "$out" | grep -m1 'FAIL' | sed 's/^cuda-smoke: //'))"
    else
        fail "cuda-smoke: exit $rc without --allow-compute (a crash, a hang, or a failure past context creation)"
    fi
fi

section "vkcube"
# vkcube has no WSI that needs no display (no headless surface); the Wayland
# and display stages run it.
skip "vkcube: no display-less WSI; run wayland.sh / vkdisplay.sh"
finish
