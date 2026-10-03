#!/usr/bin/env bash
# Does this host's driver cap concurrent encodes, and through which API?
# Runs on the GPU host itself, no guest involved.
#
#   host-encode-cap.sh [N...]      (default: 1 4 8 9 10 12 16)
#
# For each N, starts N ffmpeg encodes of a 720p60 test pattern at once, first
# through NVENC (h264_nvenc), then through Vulkan Video (h264_vulkan), the API
# the guest's capture layer uses. Reports how many finished, the first error
# of any that did not, and the driver's own session count mid-run.
#
# A consumer card's session cap shows as NVENC encodes past the cap failing
# with an explicit error. If NVENC stops at a cap and Vulkan Video does not,
# the cap does not apply to the guest's path.
set -uo pipefail

SECS=${SECS:-20}
[ $# -gt 0 ] || set -- 1 4 8 9 10 12 16
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
nvidia-smi --query-gpu=name,driver_version --format=csv,noheader

run() {
    local enc=$1 i
    case $enc in
    h264_nvenc) args=(-re -f lavfi -i "testsrc2=size=1280x720:rate=60" -t "$SECS" -c:v h264_nvenc -b:v 20M) ;;
    h264_vulkan) args=(-init_hw_device vulkan=vk -filter_hw_device vk -re
        -f lavfi -i "testsrc2=size=1280x720:rate=60" -t "$SECS"
        -vf "format=nv12,hwupload" -c:v h264_vulkan -b:v 20M) ;;
    esac
    for i in $(seq 1 "$n"); do
        ffmpeg -hide_banner -nostdin -y "${args[@]}" -f null - > "$work/$enc-$i.log" 2>&1 &
        echo $! > "$work/$enc-$i.pid"
    done
    sleep $((SECS / 2))
    local sess
    sess=$(nvidia-smi --query-gpu=encoder.stats.sessionCount --format=csv,noheader,nounits | tr -d ' ')
    local ok=0 first=""
    for i in $(seq 1 "$n"); do
        if wait "$(cat "$work/$enc-$i.pid")"; then
            ok=$((ok + 1))
        elif [ -z "$first" ]; then
            first=$(grep -aiE 'error|fail|cannot|out of|session' "$work/$enc-$i.log" | head -1 | cut -c1-110)
        fi
    done
    printf '  %-11s N=%-3s finished %s of %s, driver sessions mid-run %s%s\n' \
        "$enc" "$n" "$ok" "$n" "$sess" "${first:+, first failure: $first}"
}

for n in "$@"; do
    run h264_nvenc
    run h264_vulkan
done
