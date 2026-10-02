#!/bin/bash
# Runs as init inside the guest: what the backend lets through to
# /dev/nvidia-uvm, and what it does not. See uvmprobe.c.
#
# This is the only probe that exercises UVM at all. The draw, vulkan and
# encode probes are graphics and encode: they open /dev/nvidia-uvm in the
# sense that the module creates the node, and then never send it an ioctl, so
# every check in the UVM path is dead code as far as they are concerned.
#
# Compute is off in the backend's default capability set, so /dev/nvidia-uvm
# is not served without it:
#
#   BACKEND_ARGS='--caps graphics,compute' rig.sh probe rig-probe-uvm.sh
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
dmesg | grep -i "UVM call" | sed 's/^/GUEST: /'
/opt/nvgpu/uvmprobe 2>&1 | sed 's/^/GUEST: /'
# The pipe makes $? sed's, and sed always succeeds.
echo "GUEST: uvmprobe rc=${PIPESTATUS[0]}"
echo "GUEST: DONE"
sync; poweroff -f
