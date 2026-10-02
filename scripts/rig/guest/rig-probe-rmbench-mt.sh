#!/bin/bash
# Runs as init inside the guest: rmbench-mt at 1, 4 and 8 threads. See
# rmbench-mt.c; any wrong or failed answer is a FAIL line.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
for n in 1 4 8; do
    out=$(/opt/nvgpu/rmbench-mt "$n" 20000 2>&1)
    if [ $? = 0 ]; then echo "GUEST: $out PASS"; else echo "GUEST: $out FAIL"; fi
done
echo "GUEST: DONE"
sync; poweroff -f
