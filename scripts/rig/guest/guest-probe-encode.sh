#!/bin/bash
# Runs as init inside the guest. The whole chain, for the first time on NVIDIA:
# nescope (headless Wayland) -> vkcube presents -> nescapture captures on the
# game's own device -> Vulkan Video encodes -> datagrams out to a receiver.
#
# What this is really asking is whether a frame survives past device adoption:
# the ring, the timeline semaphore, the ColorConverter compute shader and the
# encode queue are all untested on this driver.
export PATH=/usr/bin:/usr/sbin:/bin:/sbin
mount -t proc proc /proc 2>/dev/null
mount -t sysfs sys /sys 2>/dev/null
mount -t devtmpfs dev /dev 2>/dev/null
mkdir -p /dev/shm; mount -t tmpfs tmpfs /dev/shm 2>/dev/null
exec >/dev/console 2>&1

insmod /opt/nvgpu/virtio_gpu_nv.ko
mkdir -p /mnt/nvidia && mount -t virtiofs nvidia /mnt/nvidia 2>/dev/null
. /opt/nvgpu/guest-nvidia-env.sh
nvgpu_env_check || echo "GUEST: continuing anyway so the failure is visible"

export XDG_RUNTIME_DIR=/run/user/0
mkdir -p "$XDG_RUNTIME_DIR"; chmod 700 "$XDG_RUNTIME_DIR"
export HOME=/root; mkdir -p /root

SECS=${SECS:-14}
SOCK=/tmp/nestri-video.sock
STREAM=/capture.h264

export NESCAPTURE_ENABLE=1
export VK_ADD_IMPLICIT_LAYER_PATH=/opt/nescapture/implicit_layer.d
export NESCAPTURE_CODEC=h264
export NESCAPTURE_BITRATE=20000
export NESCAPTURE_FPS=60
export NESCAPTURE_IPC_PATH=$SOCK
export RUST_LOG=debug

echo "GUEST: ---- receiver ----"
/opt/nvgpu/nesrecv "$SOCK" "$STREAM" "$SECS" > /recv.txt 2>/recv.err &
RECV=$!
sleep 1

echo "GUEST: ---- nescope + vkcube ----"
timeout $((SECS - 3)) /opt/nescapture/nescope --width 1280 --height 720 --fps 60 \
    -- vkcube --wsi wayland --gpu_number 0 --c 100000 > /run.log 2>&1
echo "GUEST: nescope rc=$?"

wait $RECV 2>/dev/null
echo "GUEST: receiver: $(cat /recv.txt 2>/dev/null) $(cat /recv.err 2>/dev/null | head -2)"
echo "GUEST: stream bytes: $(stat -c %s $STREAM 2>/dev/null || echo 0)"

echo "GUEST: ---- which path ----"
grep -iE "encoder on the game's device|device of its own|cannot host the encoder|no queue for the encoder" /run.log | sed 's/^/GUEST: /'

echo "GUEST: ---- encode/convert ----"
grep -iE "encoder|encode|convert|rgb|nv12|vulkan video|pixelforge" /run.log | head -14 | sed 's/^/GUEST: /'

echo "GUEST: ---- errors ----"
grep -iE "error|panic|fail|refus|unsupported|WARN" /run.log | head -20 | sed 's/^/GUEST: /'

echo "GUEST: ---- vkcube said ----"; grep -iE "vkcube|assert|gpu" /run.log | tail -8 | sed "s/^/GUEST: /"
echo "GUEST: ---- last of the log ----"
tail -12 /run.log | sed 's/^/GUEST: /'

echo "GUEST: DONE"
sync; poweroff -f
