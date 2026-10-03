# Sourced by every guest probe. The one place that says what a guest needs in
# its environment to use the host's driver userspace over virtiofs.
#
# It is a file rather than three lines copied into each probe because the third
# line below was missing for five months and cost a full day on the second
# machine. Add a variable here, not in a probe.

NVGPU_SHARE=${NVGPU_SHARE:-/mnt/nvidia}

# The driver's own libraries, exported read-only from the host.
export LD_LIBRARY_PATH=$NVGPU_SHARE/lib

# The Vulkan ICD manifest. Without it the loader enumerates only lavapipe and a
# "Vulkan works" run silently measures the CPU rasteriser.
export VK_DRIVER_FILES=$NVGPU_SHARE/share/vulkan/icd.d/nvidia_icd.json

# The GLVND EGL vendor config, and the line that was missing.
#
# On Linux the Vulkan ICD *is* libGLX_nvidia.so.0, and it will not hand out its
# Vulkan entrypoints until its EGL side has found an NVIDIA vendor through
# GLVND. With no vendor config on the search path, dlopen succeeds,
# vk_icdGetInstanceProcAddr resolves, and then every global entrypoint comes
# back NULL: vkCreateInstance = (nil), negotiate() = -3, and the loader reports
# ERROR_INCOMPATIBLE_DRIVER -- which reads like a version problem and is not
# one. No ioctl is ever issued, so the backend log is empty and a trace diff
# shows nothing. See ../the-guest-needs-more-than-libraries.md.
#
# GLVND's default search path is /usr/share/glvnd/egl_vendor.d in the GUEST
# rootfs. The share already carries the file; only the path was never set.
export __EGL_VENDOR_LIBRARY_DIRS=$NVGPU_SHARE/share/glvnd/egl_vendor.d

# Fail loudly and by name. Each of these has been a silent, differently-shaped
# failure at least once, and the symptom never names the missing file.
nvgpu_env_check() {
    local missing=0 f
    for f in "$VK_DRIVER_FILES" \
             "$__EGL_VENDOR_LIBRARY_DIRS/10_nvidia.json" \
             "$NVGPU_SHARE/lib/libGLX_nvidia.so.0"; do
        [ -e "$f" ] || { echo "GUEST: MISSING FROM SHARE: $f"; missing=1; }
    done
    [ "$missing" = 0 ] && echo "GUEST: share environment OK ($NVGPU_SHARE)"
    return $missing
}
