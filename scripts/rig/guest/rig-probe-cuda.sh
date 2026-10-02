#!/bin/bash
# Runs as init inside the guest: does CUDA work in here? See cudaprobe.c.
#
# This is the probe M6 was built for -- cuCtxCreate registers memory with RM by
# CPU address -- and the first thing in the rig that asks for compute rather
# than graphics.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko

mkdir -p /mnt/nvidia
mount -t virtiofs nvidia /mnt/nvidia || echo "GUEST: no share at /mnt/nvidia"
. /opt/nvgpu/guest-nvidia-env.sh

/opt/nvgpu/cudaprobe 2>&1 | sed 's/^/GUEST: /'
# The pipe makes $? sed's, and sed always succeeds.
echo "GUEST: cudaprobe rc=${PIPESTATUS[0]}"
echo "GUEST: DONE"
sync; poweroff -f
