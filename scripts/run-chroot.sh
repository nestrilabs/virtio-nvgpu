#!/bin/bash
# Run a command against the real driver, inside the guest's own rootfs.
#
# This is the control for every guest experiment: same userspace, same
# libraries, same ICD -- only the driver underneath differs. When it works
# here and fails in the guest, the difference is ours.
#
# Usage: run-chroot.sh <command...>
#   NVSNIFF_OUT=/path  also records every NVIDIA ioctl as JSON lines
set -euo pipefail

ROOT=/mnt/guestfs
IMG=/root/guest/rootfs.ext4

cleanup() {
  for m in dev/shm dev/pts dev proc sys mnt/nvidia; do
    umount -l "$ROOT/$m" 2>/dev/null || true
  done
  umount -l "$ROOT" 2>/dev/null || true
}
trap cleanup EXIT

mkdir -p "$ROOT"
mountpoint -q "$ROOT" || mount -o loop "$IMG" "$ROOT"
mkdir -p "$ROOT/mnt/nvidia"
mount --bind /var/lib/nvgpu "$ROOT/mnt/nvidia"
mount -t proc proc "$ROOT/proc"
mount -t sysfs sys "$ROOT/sys"
mount --rbind /dev "$ROOT/dev"

SNIFF=""
if [ -n "${NVSNIFF_OUT:-}" ]; then
  cp /root/nvidia_sniffer/target/release/libnvidia_sniffer.so "$ROOT/tmp/"
  SNIFF="LD_PRELOAD=/tmp/libnvidia_sniffer.so NVSNIFF_OUT=$NVSNIFF_OUT"
fi

chroot "$ROOT" /usr/bin/env -i \
  PATH=/usr/bin:/usr/sbin:/bin:/sbin \
  LD_LIBRARY_PATH=/mnt/nvidia/lib \
  VK_DRIVER_FILES=/mnt/nvidia/share/vulkan/icd.d/nvidia_icd.json \
  $SNIFF \
  "$@"
