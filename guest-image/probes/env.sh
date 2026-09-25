# shellcheck shell=bash
# The guest's environment: sourced by probe-common.sh and by /etc/profile.
#
# Everything is a nix path behind a fixed name: /etc/nvgpu/sw is the tools'
# buildEnv, /run/opengl-driver the NVIDIA 595.99.02 userspace (re-created at
# boot by probe-common.sh, since /run is a tmpfs).
export PATH=/opt/nvgpu/bin:/etc/nvgpu/sw/bin:/bin:/usr/bin
export HOME=/root
export USER=root
export SHELL=/bin/bash
export LANG=C.UTF-8
export TERM=${TERM:-vt100}
export XDG_RUNTIME_DIR=/run/user/0
export FONTCONFIG_FILE=/etc/fonts/fonts.conf

# Pin every loader to NVIDIA's files, so a missing or broken NVIDIA ICD is a
# failure rather than a silent fallback. (The default search paths would find
# the same files: nothing else is under /run/opengl-driver.)
export VK_DRIVER_FILES=/run/opengl-driver/share/vulkan/icd.d/nvidia_icd.json
export __EGL_VENDOR_LIBRARY_FILENAMES=/run/opengl-driver/share/glvnd/egl_vendor.d/10_nvidia.json
export __GLX_VENDOR_LIBRARY_NAME=nvidia
export GBM_BACKENDS_PATH=/run/opengl-driver/lib/gbm
export __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=/run/opengl-driver/share/egl/egl_external_platform.d

# The helpers the verify scripts and probes share.
export NVGPU_SHIM=/opt/nvgpu/libnvgpu-shim.so
export NVGPU_VERIFY=/opt/nvgpu/verify
