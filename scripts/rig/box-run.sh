#!/usr/bin/env bash
# Boot one guest against the vhost-user backend and keep both sides' output.
# Called by rig.sh; runs on the GPU host.
#
#   box-run.sh <probe> <tag>
#
#   probe   a script under /opt/nvgpu in the guest rootfs
#   tag     names the logs, so runs do not overwrite each other
#
# Environment:
#   GPU_BIN, TAG     the backend is $GPU_BIN/vhost-user-nvgpu-$TAG
#   GPU_ROOTFS       guest rootfs image
#   GPU_KERNEL       guest kernel image
#   GPU_VMM          command that starts nesbox; the config path is appended
#   GPU_SHARE        host NVIDIA userspace, shared into the guest as "nvidia"
#   GPU_LOGS         where logs go
#   GPU_ROOT, GPU_USER
#                    as root (GPU_ROOT=1) the backend runs as GPU_USER, an
#                    unprivileged user that can open /dev/nvidia* and the
#                    render node, because it refuses to run as root. Without
#                    root it runs as the login user.
#   BACKEND_ARGS     extra backend arguments, e.g. "--caps graphics,compute"
#   GUEST_ARGS       appended to the guest kernel command line
#   BACKEND_WRAP     a command the backend runs under, e.g.
#                    "strace -f -c -o /tmp/backend.strace"
#   GPU_TIMEOUT      seconds before the guest is killed (default 180)
#   GPU_VCPUS, GPU_MEM_MIB
#                    the guest's size (default 2 and 2048); smaller lets a
#                    density run reach the card's limits before the host's
#
# Prints the guest's result lines and the backend's refusals, and leaves
# $GPU_LOGS/<tag>.{backend,console}.log and <tag>.json.
set -euo pipefail

PROBE=${1:?usage: box-run.sh <probe> <tag>}
RUN_TAG=${2:?usage: box-run.sh <probe> <tag>}
: "${GPU_BIN:?}" "${TAG:?}" "${GPU_ROOTFS:?}" "${GPU_KERNEL:?}" "${GPU_VMM:?}" "${GPU_SHARE:?}" "${GPU_LOGS:?}"
GPU_ROOT=${GPU_ROOT:-1}
BACKEND=$GPU_BIN/vhost-user-nvgpu-$TAG
mkdir -p "$GPU_LOGS"
RUN=$(mktemp -d)
SOCK=$RUN/nvgpu.sock
chmod 755 "$RUN"

if [ "$GPU_ROOT" = 1 ]; then
    : "${GPU_USER:?set GPU_USER: the backend refuses to run as root}"
    chown "$GPU_USER" "$RUN"
    # The userspace stats this node and the ICD gives up without it. box1
    # lost it on a reboot, and vulkaninfo crashed in libnvidia-eglcore.
    [ -e /dev/nvidia-modeset ] || nvidia-modprobe -m || true
    AS=(setpriv --reuid="$GPU_USER" --regid="$GPU_USER" --init-groups)
else
    [ -e /dev/nvidia-modeset ] || echo "box-run: no /dev/nvidia-modeset; Vulkan will fail" >&2
    AS=()
fi

cat > "$GPU_LOGS/$RUN_TAG.json" <<JSON
{
  "boot-source": {
    "kernel_image_path": "$GPU_KERNEL",
    "boot_args": "console=hvc0 root=/dev/vda rw init=/opt/nvgpu/$PROBE ${GUEST_ARGS:-}"
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": "$GPU_ROOTFS", "is_root_device": true, "is_read_only": false }
  ],
  "machine-config": { "vcpu_count": ${GPU_VCPUS:-2}, "mem_size_mib": ${GPU_MEM_MIB:-2048} },
  "gpu-forward": { "socket": "$SOCK" },
  "shared-directories": [
    { "tag": "nvidia", "path-on-host": "$GPU_SHARE", "read-only": true }
  ]
}
JSON

# shellcheck disable=SC2086
RUST_LOG=${RUST_LOG:-info} "${AS[@]}" ${BACKEND_WRAP:-} "$BACKEND" --socket "$SOCK" ${BACKEND_ARGS:-} \
    > "$GPU_LOGS/$RUN_TAG.backend.log" 2>&1 &
BE=$!
trap 'kill $BE 2>/dev/null || true; rm -rf "$RUN"' EXIT

for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
if ! [ -S "$SOCK" ]; then
    echo "== $PROBE ($RUN_TAG): backend never created its socket"
    tail -n 20 "$GPU_LOGS/$RUN_TAG.backend.log" | sed 's/^/  /'
    exit 1
fi

# A stream left by an earlier run would pass the decode check below.
# The guest powers off without unmounting, which leaves the bitmaps stale,
# and debugfs will not write an image in that state: check it first.
if [ "$PROBE" = guest-probe-encode.sh ]; then
    /usr/sbin/e2fsck -fy "$GPU_ROOTFS" >/dev/null 2>&1 || true
    /usr/sbin/debugfs -w -R "rm /capture.h264" "$GPU_ROOTFS" >/dev/null 2>&1 || true
fi

# shellcheck disable=SC2086
timeout "${GPU_TIMEOUT:-180}" $GPU_VMM "$GPU_LOGS/$RUN_TAG.json" \
    > "$GPU_LOGS/$RUN_TAG.console.log" 2>&1 || true
sleep 1
kill $BE 2>/dev/null || true
wait $BE 2>/dev/null || true

echo "== $PROBE ($RUN_TAG) on $(uname -n), stamp $(cat "$(dirname "$0")/.stamp" 2>/dev/null || echo ?)"
grep -aE 'PASS|FAIL|frames=|deviceName|offscreen-draw|nvidia-smi:|rmbench|cuda|CUDA|panic|Oops' \
    "$GPU_LOGS/$RUN_TAG.console.log" | sed 's/^/  /' | head -n 30 || true
grep -aE 'refus|served [0-9]+ message|panicked|error' "$GPU_LOGS/$RUN_TAG.backend.log" |
    sed -E 's/^\[[^]]*\] ?//; s/^/  backend: /' | head -n 20 || true
# The encode probe's receiver only counts what arrived, and a black or
# corrupt stream arrives at full rate. So the stream comes out of the image
# and is decoded here: every frame, no decode error, and a picture that is
# not one flat level.
if [ "$PROBE" = guest-probe-encode.sh ] && command -v ffprobe >/dev/null; then
    out="$GPU_LOGS/$RUN_TAG.h264"
    rm -f "$out"
    # -c: read-only and past the stale bitmaps the guest's poweroff leaves.
    /usr/sbin/debugfs -c -R "dump /capture.h264 $out" "$GPU_ROOTFS" >/dev/null 2>&1 || true
    n=$(ffprobe -v error -count_frames -select_streams v:0 \
        -show_entries stream=nb_read_frames -of csv=p=0 "$out" 2>/dev/null || true)
    # ffprobe says N/A for a file with no stream in it.
    [[ "$n" =~ ^[0-9]+$ ]] || n=0
    if [ "${n:-0}" -gt 0 ] && ffmpeg -v error -xerror -i "$out" -f null - 2>/dev/null; then
        echo "  HOST: PASS   stream decodes  $n frame(s), no error"
    else
        echo "  HOST: FAIL   stream decodes  ${n:-0} frame(s) read from $out"
    fi
    range=$(ffmpeg -i "$out" -vf "select=eq(n\,$((${n:-2} / 2))),signalstats,metadata=print" \
        -f null - 2>&1 | grep -aoE 'YMIN=[0-9]+|YMAX=[0-9]+' | cut -d= -f2 | tr '\n' ' ')
    ymin= ymax=
    read -r ymin ymax <<<"$range" || true
    if [ -n "${ymax:-}" ] && [ $((ymax - ymin)) -gt 32 ]; then
        echo "  HOST: PASS   picture is not flat  Y $ymin..$ymax in frame $((${n:-2} / 2))"
    else
        echo "  HOST: FAIL   picture is not flat  Y ${ymin:-?}..${ymax:-?}"
    fi
fi
echo "  logs: $GPU_LOGS/$RUN_TAG.{backend,console}.log"
