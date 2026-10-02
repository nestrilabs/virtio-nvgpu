#!/usr/bin/env bash
# Put the guest module, the rig's probes and a build stamp into a test
# rootfs, on the GPU host. Called by rig.sh; runs on the GPU host.
#
#   GPU_DIR          the synced tree (module at driver/virtio_gpu_nv.ko)
#   GPU_ROOTFS       the image to write; copied from GPU_ROOTFS_BASE if absent
#   GPU_ROOT         1: loop-mount as root. 0: write with debugfs, no root
#
# Files land in /opt/nvgpu, where the probes in the base image already are.
# The image must not be in use by a guest.
set -euo pipefail

: "${GPU_DIR:?}" "${GPU_ROOTFS:?}"
GPU_ROOT=${GPU_ROOT:-1}

if [ ! -f "$GPU_ROOTFS" ]; then
    : "${GPU_ROOTFS_BASE:?no $GPU_ROOTFS and no GPU_ROOTFS_BASE to copy}"
    cp --sparse=always "$GPU_ROOTFS_BASE" "$GPU_ROOTFS"
fi
if pgrep -f "nesbox.*$TAG-" >/dev/null; then
    echo "box-stage: a rig guest is running" >&2
    exit 4
fi

# C probes are built here, static, so the guest needs no libraries for them.
for c in "$GPU_DIR"/scripts/rig/guest/*.c; do
    [ -f "$c" ] || continue
    cc -O2 -static -o "${c%.c}" "$c" || { echo "box-stage: $c did not build" >&2; exit 5; }
done
files=("$GPU_DIR/driver/virtio_gpu_nv.ko")
for f in "$GPU_DIR"/scripts/rig/guest/*; do
    [ -f "$f" ] && [ "${f%.c}" = "$f" ] && files+=("$f")
done
[ -f "${files[0]}" ] || { echo "box-stage: no module at ${files[0]}" >&2; exit 3; }
stamp=$(cat "$GPU_DIR/scripts/rig/.stamp" 2>/dev/null || echo unknown)

# A guest that was killed leaves the journal dirty. Writing into it without
# a check first gave "EXT4-fs error ... checksum invalid" and an init panic.
/usr/sbin/e2fsck -fy "$GPU_ROOTFS" >/dev/null 2>&1 || true

if [ "$GPU_ROOT" = 1 ]; then
    mnt=$(mktemp -d)
    mount -o loop "$GPU_ROOTFS" "$mnt"
    trap 'umount -l "$mnt"; rmdir "$mnt"' EXIT
    for f in "${files[@]}"; do install -m 755 "$f" "$mnt/opt/nvgpu/"; done
    echo "$stamp" > "$mnt/opt/nvgpu/STAMP"
else
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    echo "$stamp" > "$tmp/STAMP"
    files+=("$tmp/STAMP")
    for f in "${files[@]}"; do
        n=$(basename "$f")
        printf 'rm /opt/nvgpu/%s\nwrite %s /opt/nvgpu/%s\nsif /opt/nvgpu/%s mode 0100755\n' \
            "$n" "$f" "$n" "$n"
    done > "$tmp/cmds"
    # `rm` of a file that is not there yet is reported and harmless.
    PAGER=cat /usr/sbin/debugfs -w -f "$tmp/cmds" "$GPU_ROOTFS" 2>&1 |
        grep -vE '^debugfs|not found|^$' || true
    /usr/sbin/e2fsck -fy "$GPU_ROOTFS" >/dev/null 2>&1 || true
fi
echo "box-stage: $stamp -> $GPU_ROOTFS:/opt/nvgpu (${#files[@]} files)"
