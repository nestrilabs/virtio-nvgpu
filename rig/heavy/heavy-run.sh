#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# One heavy workload, the same natively (rig/rig-native-run.sh) and in a
# guest (/opt/heavy/heavy-run.sh, through the run probe): rig/rig-heavy.sh
# drives it. Every workload runs unpaced (vsync off, no fps cap) against the
# compositor it is given, and leaves:
#   $HEAVY_OUT/frames.txt   one frame time a line, in ms, for the measured
#                           window only (warm-up and loading cut off)
#   $HEAVY_OUT/app.log      the application's output
#   HEAVY-META lines on stdout: what ran, and the app's own figures
#
# Usage: heavy-run.sh <workload> [out-dir]
#   stk-ultra     SuperTuxKart --benchmark (its replayed race), GL, 3840x2160,
#                 every effect on, 2048 shadows, HD textures
#   stk-vk        the same settings on STK's Vulkan renderer, which has no
#                 advanced pipeline: a light, CPU-bound Vulkan game
#   stk-low       SuperTuxKart --benchmark, GL, 1280x720, the legacy
#                 pipeline and every effect off: CPU- and driver-bound
#   godot-gpu     Godot 4 Forward+ (Vulkan), rig/heavy/godot's "gpu" scene,
#                 at the output's size
#   godot-draws   the "draws" scene, Vulkan, 1280x720
#   godot-draws-gl  the "draws" scene on GL (compatibility renderer)
#   blender-gl    EEVEE, 1920x1080, 64 samples, from the GL UI
#   blender-vk    the same, Vulkan backend
#   vk-stream     nvgpu-bench vk-stream: a frame of fresh uploads, fences
#                 and semaphores (rig/guest-image/tools/nvgpu-bench.c)
#   vk-stream-pool  the same frame from staging memory kept mapped
# Environment: HEAVY_DIR (where godot/ and blender-eevee.py are; default
# this script's directory), HEAVY_WARM, HEAVY_SECS (Godot), HEAVY_FRAMES
# (Blender), HEAVY_MANGOHUD=0 (no MangoHud), HEAVY_BENCH (the nvgpu-bench
# to run; default $HEAVY_DIR/nvgpu-bench if there is one, else PATH's).
set -uo pipefail
WL=${1:?workload}
OUT=${2:-${HEAVY_OUT:-/tmp/heavy}}
DIR=${HEAVY_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)}
export HEAVY_OUT=$OUT HEAVY_WARM=${HEAVY_WARM:-6} HEAVY_SECS=${HEAVY_SECS:-20}
rm -rf "$OUT"
mkdir -p "$OUT"
H=$OUT/home
mkdir -p "$H"
export HOME=$H XDG_CONFIG_HOME=$H/.config XDG_CACHE_HOME=$H/.cache XDG_DATA_HOME=$H/.local/share
meta() { echo "HEAVY-META $*"; }
meta "workload=$WL host=$(uname -n) kernel=$(uname -r) cpus=$(nproc)"

# MangoHud: every present, from the start (the workload's window is cut out
# of the log afterwards), fps-only overlay (0.8.4 starts no log without one).
MH="fps_only,log_interval=0,autostart_log=1,log_duration=600,output_folder=$OUT/mh"
mkdir -p "$OUT/mh"
mangohud_on() { [ "${HEAVY_MANGOHUD:-1}" = 1 ]; }
MANGO=()
mangohud_on && MANGO=(env "MANGOHUD_CONFIG=$MH" mangohud)

# The last <ms> of MangoHud's frame times, into frames.txt.
mh_tail() { # <ms>
    local f
    f=$(find "$OUT/mh" -maxdepth 1 -name '*.csv' ! -name '*_summary.csv' | head -n 1)
    [ -n "$f" ] || { meta "mangohud=no-log"; return 1; }
    # 0.8.4: two lines of system info, then the header; frametime is ms.
    awk -F, -v want="$1" '
        NR == 3 { for (i = 1; i <= NF; i++) if ($i == "frametime") c = i; next }
        NR > 3 && c { ft[++n] = $c }
        END { s = 0; for (i = n; i >= 1 && s < want; i--) { s += ft[i]; k = i }
              for (i = k; i <= n; i++) print ft[i] }' "$f" >"$OUT/frames.txt"
}

stk_config() { # <preset> <renderer> <w> <h>
    local on=true off=false d=$XDG_CONFIG_HOME/supertuxkart/config-0.10
    mkdir -p "$d"
    if [ "$1" = ultra ]; then
        cat >"$d/config.xml" <<EOF
<?xml version="1.0"?>
<stkconfig version="8" >
    <Video real_width="$3" real_height="$4" width="$3" height="$4" fullscreen="true"
        max_fps="1000" enable_texture_compression="true" enable_high_definition_textures="3"
        enable_glow="$on" enable_bloom="$on" enable_light_shaft="$on" enable_dynamic_lights="$on"
        enable_dof="$on" max_texture_size="2048" ssr="$on" hq_mipmap="$on" render_driver="$2" />
    <GFX particles-effecs="2" animated-characters="true" geometry-level="0" anisotropic="16"
        swap-interval-vsync="0" motionblur_enabled="$on" mlaa="$on" ssao="$on" light_scatter="$on"
        shadows_resolution="2048" pcss="$on" Degraded_IBL="false" />
    <enable_internet value="0" />
</stkconfig>
EOF
    else
        cat >"$d/config.xml" <<EOF
<?xml version="1.0"?>
<stkconfig version="8" >
    <Video real_width="$3" real_height="$4" width="$3" height="$4" fullscreen="true"
        max_fps="1000" enable_texture_compression="true" enable_high_definition_textures="2"
        enable_glow="$off" enable_bloom="$off" enable_light_shaft="$off" enable_dynamic_lights="$off"
        enable_dof="$off" max_texture_size="512" ssr="$off" hq_mipmap="$off" render_driver="$2" />
    <GFX particles-effecs="0" animated-characters="false" geometry-level="2" anisotropic="0"
        swap-interval-vsync="0" motionblur_enabled="$off" mlaa="$off" ssao="$off" light_scatter="$off"
        shadows_resolution="0" pcss="$off" Degraded_IBL="true" />
    <enable_internet value="0" />
</stkconfig>
EOF
    fi
}

stk() { # <preset> <renderer> <w> <h>
    stk_config "$@"
    local t0=$SECONDS
    "${MANGO[@]}" supertuxkart --benchmark >"$OUT/app.log" 2>&1
    meta "exit=$? wall_s=$((SECONDS - t0))"
    local d=$XDG_CONFIG_HOME/supertuxkart/config-0.10 prof ms
    prof=$(grep -a 'Profiler: Frame count' "$OUT/app.log" | tail -n 1)
    meta "stk: ${prof#*Profiler: }"
    ms=$(sed -n "s/.*Time (ms) '\([0-9]*\)'.*/\1/p" <<<"$prof")
    # The graphics STK says it used, and its slow-frame table at 60/144/240.
    local rep
    rep=$(ls "$d"/*perf-report*.csv 2>/dev/null | head -n 1)
    if [ -n "$rep" ]; then
        meta "stk-settings: $(sed -n '/^Values/p' "$rep" | tr -d ' ')"
        awk -F', *' '$1 == 60 || $1 == 144 || $1 == 240 || $1 == 500 { printf "HEAVY-META stk-slow@%s=%s ratio=%s\n", $1, $2, $3 }' "$rep"
        cp "$rep" "$OUT/stk-perf-report.csv"
    fi
    [ -n "$ms" ] && mangohud_on && mh_tail "$ms"
}

godot_run() { # <scene> <renderer...>
    local sc=$1
    shift
    rm -rf "$OUT/godot"
    cp -r "$DIR/godot" "$OUT/godot"
    local t0=$SECONDS
    # Godot times its own frames; MangoHud's GL hook crashes Godot's
    # eglTerminate (0.8.4), so it runs without.
    HEAVY_SCENE=$sc godot --path "$OUT/godot" --display-driver wayland "$@" >"$OUT/app.log" 2>&1
    meta "exit=$? wall_s=$((SECONDS - t0))"
    grep -a '^HEAVY_GODOT' "$OUT/app.log" | sed 's/^/HEAVY-META /'
}

case $WL in
    stk-ultra) stk ultra opengl 3840 2160 ;;
    stk-vk) stk ultra vulkan 3840 2160 ;;
    stk-low) stk low opengl 1280 720 ;;
    godot-gpu) godot_run gpu --rendering-driver vulkan --rendering-method forward_plus --fullscreen ;;
    godot-draws) godot_run draws --rendering-driver vulkan --rendering-method forward_plus --resolution 1280x720 ;;
    godot-draws-gl) godot_run draws --rendering-driver opengl3 --rendering-method gl_compatibility --resolution 1280x720 ;;
    blender-gl | blender-vk)
        be=()
        [ "$WL" = blender-vk ] && be=(--gpu-backend vulkan)
        t0=$SECONDS
        blender --factory-startup "${be[@]}" --python "$DIR/blender-eevee.py" >"$OUT/app.log" 2>&1
        meta "exit=$? wall_s=$((SECONDS - t0))"
        grep -a '^HEAVY_BLENDER' "$OUT/app.log" | sed 's/^/HEAVY-META /'
        ;;
    vk-stream | vk-stream-pool)
        bench=${HEAVY_BENCH:-nvgpu-bench}
        [ -z "${HEAVY_BENCH:-}" ] && [ -x "$DIR/nvgpu-bench" ] && bench=$DIR/nvgpu-bench
        NVGPU_BENCH_FRAMES=$OUT/frames.txt "$bench" "$WL" >"$OUT/app.log" 2>&1
        meta "exit=$?"
        grep -a '^BENCH' "$OUT/app.log" | sed 's/^/HEAVY-META /'
        ;;
    *) echo "unknown workload $WL" >&2; exit 2 ;;
esac
[ -s "$OUT/frames.txt" ] && meta "frames=$(wc -l <"$OUT/frames.txt")"
exit 0
