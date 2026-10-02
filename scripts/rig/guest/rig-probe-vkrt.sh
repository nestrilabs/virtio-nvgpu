#!/bin/bash
# Runs as init inside the guest: does Vulkan make a device with each extension
# the driver offers? See vkrt.dyn.c. Run once per --caps set: the question is
# what changes when /dev/nvidia-uvm is absent.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
mkdir -p /mnt/nvidia && mount -t virtiofs nvidia /mnt/nvidia 2>/dev/null
. /opt/nvgpu/guest-nvidia-env.sh
nvgpu_env_check || echo "GUEST: continuing anyway so the failure is visible"
# Twice: as an application sees it with the layer, and without, which is
# what the driver itself does and what tells when the layer can go.
for layer in on off; do
    echo "GUEST: vkrt layer=$layer"
    if [ $layer = on ]; then unset DISABLE_VK_LAYER_NVGPU_NO_UVM
    else export DISABLE_VK_LAYER_NVGPU_NO_UVM=1; fi
    VK_ADD_IMPLICIT_LAYER_PATH=/opt/nvgpu VK_LOADER_DEBUG=error /opt/nvgpu/vkrt 2>&1 | sed '/^GUEST:/!s/^/GUEST: loader: /'
done
echo "GUEST: DONE"
sync; poweroff -f
