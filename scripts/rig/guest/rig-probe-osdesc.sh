#!/bin/bash
# Runs as init inside the guest: whether a guest can hand RM an address of its
# own, on each of the three ioctls that carry one. See osdescprobe.c.
#
# The other side of it -- that an ordinary heap allocation is untouched -- is
# the draw probe's business: it makes 180 VID_HEAP_CONTROL calls and has none
# refused, which is the number to watch if this check ever grows too wide.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
/opt/nvgpu/osdescprobe 2>&1 | sed 's/^/GUEST: /'
# The pipe makes $? sed's, and sed always succeeds.
echo "GUEST: osdescprobe rc=${PIPESTATUS[0]}"
echo "GUEST: DONE"
sync; poweroff -f
