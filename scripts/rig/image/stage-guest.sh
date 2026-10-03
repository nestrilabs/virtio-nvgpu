#!/usr/bin/env bash
# Put the module, the probes and (optionally) the sniffer into the guest image,
# and collect a trace back out of it afterwards.
#
#   ./stage-guest.sh install [path/to/libnvidia_sniffer.so]
#   ./stage-guest.sh check
#   ./stage-guest.sh collect  <destination.jsonl>
#   ./stage-guest.sh pull     <path-in-guest> <destination>
#
# The image is an ext4 file, so this is a loop mount. It must not be mounted
# while a guest is running on it.
set -euo pipefail

ACTION=${1:?usage: stage-guest.sh install|collect [arg]}
ROOTFS=${ROOTFS:-/root/guest/rootfs.ext4}
MNT=${MNT:-/mnt/guestfs}
DRIVER_DIR=${DRIVER_DIR:-/root/driver}
HERE=$(cd "$(dirname "$0")" && pwd)

# Libraries and data the NVIDIA userspace opens by name at runtime. None of
# them is in any NEEDED entry, so nothing links against them and nothing in the
# image pulls them in as a dependency. A guest
# missing one does not fail: it silently takes a narrower branch.
#
#   libdrm.so.2  libnvidia-glcore/eglcore dlopen it to enumerate /dev/dri and
#                ask DRM_NVIDIA_DMABUF_SUPPORTED. Without it the ICD never
#                opens /dev/dri, never advertises
#                VK_EXT_external_memory_dma_buf, and reports no surface support
#                for a Wayland surface -- so a client "cannot find both
#                graphics and present queues" while rendering offscreen fine.
#                This cost most of a day on box 1.
#   xkeyboard-config-2  libxkbcommon panics without the keymap data, and the
#                panic names none of it.
GUEST_DLOPEN_LIBS=(libdrm.so.2)
GUEST_DLOPEN_DATA=(/usr/share/xkeyboard-config-2 /usr/share/libinput)

# Copy a host library into the guest, following symlinks, and leave the soname
# link behind it. Harmless when the guest already has its own copy.
stage_host_lib() {
    local soname=$1 src
    src=$(ldconfig -p 2>/dev/null | awk -v s="$soname" '$1 == s { print $NF; exit }')
    [ -n "$src" ] || { echo "stage-guest: host has no $soname" >&2; return 0; }
    [ -e "$MNT/usr/lib/$soname" ] && return 0
    install -m 755 -D "$(readlink -f "$src")" "$MNT/usr/lib/$(basename "$(readlink -f "$src")")"
    ln -sf "$(basename "$(readlink -f "$src")")" "$MNT/usr/lib/$soname"
    echo "stage-guest: staged $soname from $src"
}

mount_image() {
    mkdir -p "$MNT"
    mountpoint -q "$MNT" || mount -o loop "$ROOTFS" "$MNT"
}

case "$ACTION" in
install)
    SNIFFER=${2:-}
    mount_image
    mkdir -p "$MNT/opt/nvgpu"
    install -m 644 "$DRIVER_DIR/virtio_gpu_nv.ko" "$MNT/opt/nvgpu/"
    # Sourced by every probe. Install it before them: a probe without it
    # dies at its first line, which is a clearer failure than a missing EGL
    # vendor path.
    install -m 644 "$HERE/guest-nvidia-env.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-vulkan.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-trace.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-caps.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-draw.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-draw-trace.sh" "$MNT/opt/nvgpu/"
    install -m 755 "$HERE/guest-probe-gemmap.sh" "$MNT/opt/nvgpu/"
    # Plain C against the DRM node, no Vulkan and no libraries, so it builds on
    # the test box with cc and runs in the guest as it is:
    #   cc -O2 -o gemmap gemmap.c
    [ -x "$HERE/gemmap" ] && install -m 755 "$HERE/gemmap" "$MNT/opt/nvgpu/"
    # Built by build-offscreen.sh on the host; absent until that has been run.
    [ -x "$HERE/offscreen-draw" ] && install -m 755 "$HERE/offscreen-draw" "$MNT/opt/nvgpu/"
    [ -n "$SNIFFER" ] && install -m 755 "$SNIFFER" "$MNT/opt/nvgpu/libnvidia_sniffer.so"
    for lib in "${GUEST_DLOPEN_LIBS[@]}"; do stage_host_lib "$lib"; done
    for d in "${GUEST_DLOPEN_DATA[@]}"; do
        [ -e "$MNT$d" ] || echo "stage-guest: guest is missing $d" >&2
    done
    rm -f "$MNT/guest-params.jsonl"
    umount -l "$MNT"
    echo "staged: virtio_gpu_nv.ko, probes${SNIFFER:+, sniffer} -> $ROOTFS:/opt/nvgpu/"
    ;;
check)
    # Answer "is this image able to present?" without booting it. Every line
    # here is something a guest has actually been missing.
    mount_image
    rc=0
    for lib in "${GUEST_DLOPEN_LIBS[@]}"; do
        if [ -e "$MNT/usr/lib/$lib" ]; then echo "ok      $lib"
        else echo "MISSING $lib"; rc=1; fi
    done
    for d in "${GUEST_DLOPEN_DATA[@]}" /usr/share/glvnd/egl_vendor.d/10_nvidia.json; do
        if [ -e "$MNT$d" ]; then echo "ok      $d"
        else echo "MISSING $d"; rc=1; fi
    done
    umount -l "$MNT"
    exit $rc
    ;;
collect)
    DEST=${2:?usage: stage-guest.sh collect <destination.jsonl>}
    mount_image
    cp "$MNT/guest-params.jsonl" "$DEST"
    cp "$MNT/vk.err" "${DEST%.jsonl}.stderr" 2>/dev/null || true
    umount -l "$MNT"
    echo "collected: $DEST ($(wc -l < "$DEST") records)"
    ;;
pull)
    SRC=${2:?usage: stage-guest.sh pull <path-in-guest> <destination>}
    DEST=${3:?usage: stage-guest.sh pull <path-in-guest> <destination>}
    mount_image
    cp "$MNT/${SRC#/}" "$DEST"
    umount -l "$MNT"
    echo "pulled: $SRC -> $DEST ($(wc -l < "$DEST") lines)"
    ;;
*)
    echo "unknown action: $ACTION" >&2
    exit 1
    ;;
esac
