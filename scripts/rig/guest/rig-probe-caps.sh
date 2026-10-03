#!/bin/bash
# Runs as init inside the guest. Says which device nodes the guest driver
# created for the capabilities the backend served, and whether nvidia-smi and
# a CUDA context get anywhere. Pass/fail is against GUEST_EXPECT, a comma list
# of the caps the backend was started with, read from the kernel command
# line as nvgpu_expect=...; without it the lines only report.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

echo "GUEST: stamp $(cat /opt/nvgpu/STAMP 2>/dev/null)"
insmod /opt/nvgpu/virtio_gpu_nv.ko
dmesg | grep -E 'virtio-gpu-nv: caps' | sed 's/^.*virtio-gpu-nv: /GUEST: /'

expect=$(tr ' ' '\n' < /proc/cmdline | sed -n 's/^nvgpu_expect=//p')
has() { case ",$expect," in *",$1,"*) return 0 ;; *) return 1 ;; esac; }

check() { # check <path> <cap | always | never>
    local present=no want
    [ -e "$1" ] && present=yes
    if [ -z "$expect" ]; then
        echo "GUEST: node $1 present=$present"
        return
    fi
    if [ "$2" = always ]; then want=yes
    elif [ "$2" = never ]; then want=no
    elif has "$2"; then want=yes
    else want=no; fi
    if [ "$present" = "$want" ]; then
        echo "GUEST: node $1 present=$present PASS"
    else
        echo "GUEST: node $1 present=$present want=$want FAIL"
    fi
}

check /dev/nvidiactl always
check /dev/nvidia-uvm compute
check /dev/nvidia-uvm-tools never
check /dev/nvidia-modeset graphics
check /dev/dri/renderD128 graphics

mkdir -p /mnt/nvidia && mount -t virtiofs nvidia /mnt/nvidia 2>/dev/null
. /opt/nvgpu/guest-nvidia-env.sh 2>/dev/null
if [ -x /mnt/nvidia/bin/nvidia-smi ]; then
    /mnt/nvidia/bin/nvidia-smi -L 2>&1 | sed 's/^/GUEST: nvidia-smi: /' | head -3
fi
if [ -x /opt/nvgpu/cuda_probe ]; then
    /opt/nvgpu/cuda_probe 2>&1 | sed 's/^/GUEST: cuda: /' | head -20
fi

echo "GUEST: DONE"
sync; poweroff -f
