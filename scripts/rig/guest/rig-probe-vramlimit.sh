#!/bin/bash
# Runs as init inside the guest: is it held to its video memory limit?
# The limit comes on the kernel command line, GUEST_ARGS="VRAM_LIMIT_MIB=N",
# and must be the one the backend was started with (--vram-limit-mib N).
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null), limit ${VRAM_LIMIT_MIB:-none}"
insmod /opt/nvgpu/virtio_gpu_nv.ko
mkdir -p /mnt/nvidia
mount -t virtiofs nvidia /mnt/nvidia || echo "GUEST: no share at /mnt/nvidia"
. /opt/nvgpu/guest-nvidia-env.sh

# What the tools a customer would look at say.
/mnt/nvidia/bin/nvidia-smi --query-gpu=memory.total,memory.used,memory.free --format=csv,noheader 2>&1 |
    sed 's/^/GUEST: nvidia-smi: /'
vulkaninfo 2>/dev/null | grep -A2 -m2 'memoryHeaps\[0\]' | grep -m1 size | sed 's/^/GUEST: vulkan heap 0: /'

VRAM_LIMIT_MIB=${VRAM_LIMIT_MIB:-0} /opt/nvgpu/vramprobe 2>&1 | sed 's/^/GUEST: /'
echo "GUEST: vramprobe rc=${PIPESTATUS[0]}"
VRAM_LIMIT_MIB=${VRAM_LIMIT_MIB:-0} /opt/nvgpu/vkalloc 2>&1 | sed 's/^/GUEST: vk /'
echo "GUEST: vkalloc rc=${PIPESTATUS[0]}"
echo "GUEST: DONE"
sync; poweroff -f
