#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# nvgpu-bench-suite -- the benchmark matrix of BENCHMARKS.md, one group at a
# time, as "BENCH <name> <value> <unit>" lines. The same script runs natively
# (rig/rig-bench.sh native: rig-native-run.sh, the guest image's programs and
# NVIDIA userspace on the host) and in a guest (rig/rig-bench.sh vm), so the
# two differ only by what is between the programs and the host driver.
#
# Usage: nvgpu-bench-suite [group...]      (default: micro gpu video startup wl)
#   micro    nvgpu-bench: Vulkan, GL and RM microbenchmarks (no window)
#   cuda     nvgpu-cubench, nvgpu-nbody, clpeak, Blender Cycles (CUDA and
#            OptiX); a guest needs --allow-compute
#   gpu      nvgpu-bench vk-cost and glmark2 (--off-screen, needs Wayland):
#            rendering with nothing presented, from submission-bound to
#            GPU-bound
#   video    ffmpeg: NVDEC (-hwaccel cuda; compute), Vulkan Video decode,
#            and NVENC transcodes kept on the GPU (compute)
#   startup  time to a device, a context and a first presented frame
#   wl       Wayland throughput: a wl_shm client, an EGL client unsynced,
#            a Vulkan client in mailbox
#
# Each group's lines are bracketed by "BENCH-SECTION begin|end <group>", which
# the host side uses to charge the backend's and the VMM's CPU to it.
set -u
A=/opt/nvgpu/apps
[ -d "$A" ] || A=${NVGPU_APPS:-$A}
T=${TMPDIR:-/tmp}/nvgpu-bench.$$
mkdir -p "$T"
trap 'rm -rf "$T"' EXIT

groups=("$@")
[ ${#groups[@]} -gt 0 ] || groups=(micro gpu video startup wl)

say() { echo "BENCH $1 $2 $3"; }
sect() { echo "BENCH-SECTION $1 $2 $(date +%s.%N)"; }
t_now() { date +%s.%N; }
elapsed() { awk -v a="$1" -v b="$(t_now)" 'BEGIN { printf "%.4g", (b - a) * 1000 }'; }
has_wl() { [ -n "${WAYLAND_DISPLAY:-}" ]; }

g_micro() {
    nvgpu-bench vk gl rm
}

g_cuda() {
    nvgpu-cubench
    nvgpu-nbody > "$T/nbody.log" 2>&1
    # "nbody N bodies: X GFLOP/s ..." and the copy bandwidths it prints
    awk '/GFLOP\/s/ { for (i = 1; i < NF; i++) if ($(i+1) ~ /^GFLOP\/s/) print "BENCH nbody.gflops", $i, "GFLOP/s" }
         /GB\/s/ { n = split($0, w, " "); for (i = 1; i < n; i++) if (w[i+1] ~ /^GB\/s/) { k = w[1]; gsub(/[^A-Za-z0-9_]/, "", k); print "BENCH nbody." k "_" (++c), w[i], "GB/s" } }' "$T/nbody.log"
    timeout 300 clpeak --opencl --global-memory-bandwidth --single-precision-compute --transfer-bandwidth \
        --kernel-launch-latency --csv-file "$T/clpeak.csv" > "$T/clpeak.log" 2>&1
    # format_version,backend,platform,device,driver,category,test,metric,unit,status,value,reason
    awk -F, 'NR > 1 && $10 == "\"ok\"" {
            t = $7 "_" $8; gsub(/"/, "", t); u = $9; gsub(/"/, "", u)
            if (t ~ /^(single_precision_compute_float|global_memory_bandwidth_float4|transfer_bandwidth_|kernel_launch_latency_)/)
                print "BENCH clpeak." t, $11, u
        }' "$T/clpeak.csv"
    for dev in CUDA OPTIX; do
        timeout 300 blender -b --factory-startup --python "$A/blender/cycles.py" -- "$dev" > "$T/cycles.log" 2>&1
        awk -v d="$dev" '/NVGPU_CYCLES .* frame rendered in/ { for (i = 1; i <= NF; i++) if ($i == "in") { v = $(i+1); sub(/s$/, "", v); print "BENCH cycles." tolower(d), v, "s" } }' "$T/cycles.log"
    done
}

g_gpu() {
    # The frame time against its GPU load, nothing presented (vkmark's
    # headless window system needs VK_EXT_headless_surface, which NVIDIA's
    # driver does not have).
    nvgpu-bench vk-cost
    # glmark2's heavier scenes, rendered off screen at 1920x1080, one
    # glFinish a frame: its light scenes run at 50,000+ fps natively, where
    # the per-frame wait is most of the frame.
    if has_wl; then
        local gs=() s
        for s in build:use-vbo=true texture shading:shading=phong bump:bump-render=normals effect2d \
            pulsar desktop:effect=blur buffer:update-method=map ideas jellyfish terrain refract shadow; do
            gs+=(-b "$s:duration=5")
        done
        timeout 200 glmark2-wayland --off-screen -s 1920x1080 "${gs[@]}" > "$T/glmark2.log" 2>&1
        awk '/^\[/ && /FPS:/ { s = $1; gsub(/[\[\]]/, "", s); for (i = 1; i < NF; i++) if ($i == "FPS:") print "BENCH glmark2." s, $(i+1), "fps" }
             /glmark2 Score/ { print "BENCH glmark2.score", $NF, "points" }' "$T/glmark2.log"
    fi
}

# ffmpeg over a clip looped ten times (3,000 frames): frames per second of
# wall time, process start included.
ff() {
    local name=$1; shift
    local t0; t0=$(t_now)
    timeout 120 ffmpeg -hide_banner -nostdin -y "$@" > "$T/$name.log" 2>&1
    local rc=$? ms; ms=$(elapsed "$t0")
    local fr; fr=$(tr '\r' '\n' < "$T/$name.log" | grep -aoE '^frame= *[0-9]+' | tail -n 1 | tr -dc 0-9)
    if [ "$rc" = 0 ] && [ -n "$fr" ] && [ "$fr" -gt 0 ]; then
        say "video.$name" "$(awk -v f="$fr" -v m="$ms" 'BEGIN { printf "%.4g", f / (m / 1000) }')" fps
    else
        echo "BENCH-FAIL video.$name rc=$rc frames=${fr:-0}"
    fi
}

g_video() {
    local c f
    for c in h264 hevc av1; do
        f=$A/www/$c.mp4
        if [ -e /dev/nvidia-uvm ]; then
            ff "nvdec_$c" -stream_loop 9 -hwaccel cuda -hwaccel_output_format cuda -i "$f" -f null -
        fi
        # H.264 Vulkan decode stalls natively with this ffmpeg and driver.
        [ "$c" = h264 ] || ff "vkdec_$c" -stream_loop 9 -init_hw_device vulkan=vk -hwaccel vulkan \
            -hwaccel_device vk -hwaccel_output_format vulkan -i "$f" -f null -
    done
    if [ -e /dev/nvidia-uvm ]; then
        for e in h264_nvenc hevc_nvenc av1_nvenc; do
            ff "nvenc_$e" -stream_loop 9 -hwaccel cuda -hwaccel_output_format cuda -i "$A/www/h264.mp4" \
                -c:v "$e" -preset p1 -b:v 12M -f null -
        done
    fi
}

tm() {
    local name=$1; shift
    local t0; t0=$(t_now)
    if timeout 60 "$@" > "$T/$name.log" 2>&1; then
        say "startup.$name" "$(elapsed "$t0")" ms
    else
        echo "BENCH-FAIL startup.$name"
    fi
}

g_startup() {
    local i
    for i in 1 2 3; do
        tm "nvidia_smi_L_$i" nvidia-smi -L
        tm "vulkaninfo_summary_$i" vulkaninfo --summary
        if has_wl; then
            tm "vkcube_1frame_$i" vkcube --wsi wayland --c 1
        fi
    done
    nvgpu-bench vk-init gl-init
}

g_wl() {
    has_wl || { echo "BENCH-FAIL wl: no WAYLAND_DISPLAY"; return; }
    nvgpu-bench wl-shm
    # SIGINT: it stops cleanly and its buffered lines reach the log.
    timeout -s INT -k 3 14 weston-simple-egl -b > "$T/simple-egl.log" 2>&1
    awk '/frames in .* seconds/ { v = $(NF-1) } END { if (v) print "BENCH wl.simple_egl_unsynced", v, "fps" }' "$T/simple-egl.log"
    timeout 60 vkmark --winsys wayland -s 1920x1080 --present-mode mailbox -b clear:duration=8 -b cube:duration=8 > "$T/vkmark-wl.log" 2>&1
    awk '/^\[/ && /FPS:/ { s = $1; gsub(/[\[\]]/, "", s); for (i = 1; i < NF; i++) if ($i == "FPS:") print "BENCH wl.vkmark_mailbox_" s, $(i+1), "fps" }' "$T/vkmark-wl.log"
}

for g in "${groups[@]}"; do
    case $g in
        micro | cuda | gpu | video | startup | wl)
            sect begin "$g"
            "g_$g"
            sect end "$g"
            ;;
        *) echo "BENCH-FAIL unknown group $g" ;;
    esac
done
