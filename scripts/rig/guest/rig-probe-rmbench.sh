#!/bin/bash
# Runs as init inside the guest: rmbench, three times, so a run-to-run spread
# is visible. See rmbench.c.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
# nvgpu_rpc_spin=N on the kernel command line sets rpc_spin_us, so one image
# can sweep it.
spin=$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^nvgpu_rpc_spin=//p')
if [ -n "$spin" ]; then
    echo "$spin" > /sys/module/virtio_gpu_nv/parameters/rpc_spin_us
    echo "GUEST: rpc_spin_us=$(cat /sys/module/virtio_gpu_nv/parameters/rpc_spin_us)"
fi
for i in 1 2 3; do
    /opt/nvgpu/rmbench 100000 2>&1 | sed 's/^/GUEST: /'
done
echo "GUEST: DONE"
sync; poweroff -f
