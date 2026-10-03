#!/bin/bash
# Runs as init inside the guest. Install at /opt/nvgpu/ in the guest image and
# name it in the kernel command line -- run-guest.sh does both.
#
# Answers one question: does the GPU work in here? It prints what the driver and
# the Vulkan loader say, because when this component is broken every ioctl still
# returns 0 and only they will say why.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

insmod /opt/nvgpu/virtio_gpu_nv.ko

# The host's own driver userspace, exported read-only. A guest image must never
# carry its own copy: the forwarded ioctls are a private contract between one
# build of these libraries and one build of the host kernel module.
mkdir -p /mnt/nvidia && mount -t virtiofs nvidia /mnt/nvidia 2>/dev/null
export LD_LIBRARY_PATH=/mnt/nvidia/lib
export VK_DRIVER_FILES=/mnt/nvidia/share/vulkan/icd.d/nvidia_icd.json

echo "GUEST: ---- nvidia-smi ----"
/mnt/nvidia/bin/nvidia-smi 2>&1 | head -12

echo "GUEST: ---- vulkaninfo ----"
# VK_LOADER_DEBUG is the whole point. Without it vulkaninfo prints its instance
# extensions and stops, and the ICD's own reason -- "Failed to allocate
# semaphore event", say -- is never shown.
VK_LOADER_DEBUG=error,warn /usr/bin/vulkaninfo --summary > /vk.txt 2>/vk.err
echo "GUEST: vulkaninfo rc=$?"
sed -n '/Devices:/,$p' /vk.txt | head -20
echo "GUEST: ---- what the loader and driver said ----"
grep -iE 'error|warn' /vk.err | head -10

echo "GUEST: DONE"
sync; poweroff -f
