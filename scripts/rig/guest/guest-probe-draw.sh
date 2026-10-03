#!/bin/bash
# Runs as init inside the guest. Install at /opt/nvgpu/ in the guest image and
# name it in the kernel command line -- run-guest.sh does both.
#
# The first thing that actually draws. vkcube cannot run on either GPU box (the
# NVIDIA card's outputs are empty, so there is no WSI surface), so this renders
# a triangle into a VkImage with no swapchain and checks the pixels that come
# back. See exporting-a-frame.md.
#
# The binary is built on the host by build-offscreen.sh and staged into the
# image by stage-guest.sh -- the guest has no compiler and no Vulkan headers.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

insmod /opt/nvgpu/virtio_gpu_nv.ko

# The host's own driver userspace. See guest-probe-vulkan.sh for why the guest
# image must never carry its own copy.
mkdir -p /mnt/nvidia && mount -t virtiofs nvidia /mnt/nvidia 2>/dev/null
. /opt/nvgpu/guest-nvidia-env.sh
nvgpu_env_check || echo "GUEST: continuing anyway so the failure is visible"

echo "GUEST: ---- offscreen draw ----"
# The probe prints its own GUEST:-prefixed lines, which is what run-guest.sh
# greps for at the end. The rendered image is left on the image for
# `stage-guest.sh pull /draw.ppm ...` -- a wrong-looking pass is worth eyeing.
VK_LOADER_DEBUG=error,warn /opt/nvgpu/offscreen-draw /draw.ppm 2>/draw.err
echo "GUEST: offscreen-draw rc=$?"

echo "GUEST: ---- what the loader and driver said ----"
grep -iE 'error|warn' /draw.err | head -10 | sed 's/^/GUEST: /'

echo "GUEST: DONE"
sync; poweroff -f
