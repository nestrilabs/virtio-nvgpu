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
# Beyond the guest image, from rig/heavy/extras.nix (an image made with
# mkimage-heavy.sh --closure has it at /opt/heavy/extras; HEAVY_EXTRAS
# natively) and Windows programs under HEAVY_WIN (default $HEAVY_DIR/win):
#   gameloop      nvgpu-gameloop (rig/heavy/gameloop.c): a job system's
#                 fork-join phases on every CPU, then 4,000 draws presented
#                 unpaced; its GL_* variables pass through
#   wine-godot-draws-d3d12, wine-godot-draws-vk, wine-godot-gpu-d3d12
#                 Godot's Windows build of the same version under Wine
#                 (staging, WoW64), on vkd3d-proton's D3D12 or winevulkan,
#                 the same scenes as godot-draws and godot-gpu
#   wine-heaven   Unigine Heaven 4.0, its 32-bit D3D11 build under Wine with
#                 DXVK, flying its demo camera, 1280x720, low, no
#                 tessellation (HEAVY_HEAVEN_QUALITY, _TESS, _SIZE)
#   probe-vk      the Vulkan device extensions listed, and nvgpu-rtprobe
#                 (rig/heavy/rtprobe.c): does a ray-query device work
#   wakecost      nvgpu-wakecost (rig/heavy/wakecost.c): a futex hand-off
#                 between two CPUs, and a CPUID, in microseconds
#   0ad           0 A.D. 0.28 on its Vulkan renderer, four Petra AIs against
#                 each other on a generated map, observed (HEAVY_0AD_GL=1
#                 for GL)
# Wine runs on its Wayland driver; HEAVY_WINE_X11=1 runs it on X11 through
# a rootful Xwayland, as a Proton game is. It synchronises through its
# server, as natively where the host has no /dev/ntsync: a guest's is
# removed for the run unless HEAVY_WINE_NTSYNC=1 (HEAVY-META wine-sync=).
# Environment: HEAVY_DIR (where godot/ and blender-eevee.py are; default
# this script's directory), HEAVY_WARM, HEAVY_SECS (Godot), HEAVY_FRAMES
# (Blender), HEAVY_MANGOHUD=0 (no MangoHud), HEAVY_BENCH (the nvgpu-bench
# to run; default $HEAVY_DIR/nvgpu-bench if there is one, else PATH's),
# HEAVY_THP (a guest's transparent huge pages: always, madvise or never).
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
    local t0=$SECONDS lim=()
    # HEAVY_STK_TIMEOUT: kill the game itself after that many seconds (a
    # loop of start-ups; not a measurement).
    [ -n "${HEAVY_STK_TIMEOUT:-}" ] && lim=(timeout -s KILL "$HEAVY_STK_TIMEOUT")
    ${lim[@]+"${lim[@]}"} "${MANGO[@]}" supertuxkart --benchmark >"$OUT/app.log" 2>&1
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

# What the guest's CPUs were asked to do meanwhile: interrupts by kind
# (RES rescheduling and CAL function-call IPIs, LOC timer, TLB shootdowns)
# and context switches, over the whole run.
irq_snap() {
    awk '$1 ~ /^(RES|CAL|LOC|TLB):$/ { s = 0; for (i = 2; i <= NF && $i ~ /^[0-9]+$/; i++) s += $i; printf "%s=%d ", substr($1, 1, 3), s }' /proc/interrupts
    awk '$1 == "ctxt" { printf "ctxt=%d ", $2 }' /proc/stat
    date +%s.%N
}
irq_delta() { # <before> <after>
    awk -v a="$1" -v b="$2" 'BEGIN {
        na = split(a, x, " "); nb = split(b, y, " "); d = y[nb] - x[na]
        printf "HEAVY-META guest-rates secs=%.1f", d
        for (i = 1; i < na; i++) { split(x[i], p, "="); split(y[i], q, "="); printf " %s/s=%.0f", p[1], (q[2] - p[2]) / d }
        printf "\n" }'
}

X=${HEAVY_EXTRAS:-$DIR/extras}
WIN=${HEAVY_WIN:-$DIR/win}

wine_setup() { # <width> <height>
    export PATH=$X/bin:$PATH
    export WINEPREFIX=${HEAVY_WINE_PREFIX:-$DIR/wine/prefix} WINEDEBUG=${WINEDEBUG:--all}
    export WINEDLLOVERRIDES="d3d8,d3d9,d3d10core,d3d11,dxgi,d3d12,d3d12core=n,b;mscoree,mshtml,winemenubuilder.exe="
    export VKD3D_SHADER_CACHE_PATH=$OUT/vkd3d-cache DXVK_STATE_CACHE_PATH=$OUT/dxvk-cache
    mkdir -p "$VKD3D_SHADER_CACHE_PATH" "$DXVK_STATE_CACHE_PATH"
    # What a previous run left in the prefix: Godot's shader cache.
    rm -rf "$WINEPREFIX"/drive_c/users/*/AppData/Roaming/Godot
    if [ -e /dev/ntsync ] && [ "${HEAVY_WINE_NTSYNC:-0}" = 1 ]; then
        meta "wine-sync=ntsync"
    else
        # Only in a guest, as root: the host's is the host's.
        [ -e /dev/ntsync ] && [ "$(id -u)" = 0 ] && rm -f /dev/ntsync
        meta "wine-sync=wineserver"
    fi
    if [ "${HEAVY_WINE_X11:-0}" = 1 ]; then
        Xwayland :9 -geometry "$1x$2" -noreset >"$OUT/xwayland.log" 2>&1 &
        XWL=$!
        for _ in $(seq 1 50); do [ -S /tmp/.X11-unix/X9 ] && break; sleep 0.1; done
        export DISPLAY=:9
        meta "wine-display=x11 (rootful Xwayland ${1}x$2)"
    else
        unset DISPLAY
        meta "wine-display=wayland"
    fi
}
wine_done() {
    # A program that failed can leave a dialog behind that would hold the
    # server up for good.
    wineserver -k 2>/dev/null
    timeout 15 wineserver -w 2>/dev/null || pkill -KILL -f 'wineserver|\.exe' 2>/dev/null
    [ -n "${XWL:-}" ] && kill "$XWL" 2>/dev/null
    true
}

wine_godot() { # <scene> <driver> <godot args...>
    local sc=$1 drv=$2 w=1280 h=720
    shift 2
    [ "$sc" = gpu ] && w=3840 h=2160
    wine_setup "$w" "$h"
    rm -rf "$OUT/godot"
    cp -r "$DIR/godot" "$OUT/godot"
    local t0=$SECONDS
    # The GUI build: the console one's wrapper outlives the engine. In a
    # session of its own, and bounded, since Wine's end can take its
    # process group with it.
    HEAVY_SCENE=$sc HEAVY_OUT="Z:$OUT" timeout -s KILL $((HEAVY_WARM + HEAVY_SECS + 90)) \
        setsid -w wine "$WIN/godot/Godot_v4.7.2-stable_win64.exe" \
        --path "Z:$OUT/godot" --rendering-driver "$drv" --rendering-method forward_plus "$@" \
        </dev/null >"$OUT/app.log" 2>&1
    meta "exit=$? wall_s=$((SECONDS - t0))"
    wine_done
    grep -a '^HEAVY_GODOT' "$OUT/app.log" | tr -d '\r' | sed 's/^/HEAVY-META /'
}

wine_heaven() {
    local q=${HEAVY_HEAVEN_QUALITY:-LOW} tess=${HEAVY_HEAVEN_TESS:-DISABLED}
    local size=${HEAVY_HEAVEN_SIZE:-1280x720}
    local w=${size%x*} h=${size#*x}
    wine_setup "$w" "$h"
    # Its own shader compiler, as on Windows (Wine's cannot compile some of
    # its shaders).
    WINEDLLOVERRIDES="$WINEDLLOVERRIDES;d3dcompiler_42,d3dx11_42,d3dx9_42=n"
    local run=$((HEAVY_WARM + HEAVY_SECS + 20)) t0=$SECONDS
    # Its data is found from the working directory, as its launcher runs it.
    (
        cd "$WIN/heaven/bin" &&
            timeout -s TERM -k 5 "$run" "${MANGO[@]}" wine Heaven.exe -project_name Heaven \
                -data_path ../ -engine_config ../data/heaven_4.0.cfg -system_script heaven/unigine.cpp \
                -sound_app null -video_app direct3d11 -video_multisample 0 -video_fullscreen 0 \
                -video_mode -1 -video_width "$w" -video_height "$h" \
                -extern_define "RELEASE,LANGUAGE_EN,QUALITY_$q,TESSELLATION_$tess"
    ) >"$OUT/app.log" 2>&1
    meta "exit=$? wall_s=$((SECONDS - t0)) quality=$q tessellation=$tess size=$size"
    wine_done
    mangohud_on && mh_tail $((HEAVY_SECS * 1000))
}

zeroad() {
    local be=vulkan run=$((HEAVY_WARM + HEAVY_SECS + 60)) t0=$SECONDS
    [ "${HEAVY_0AD_GL:-0}" = 1 ] && be=gl
    export PATH=$X/bin:$PATH
    # Observed (player -1), four Petra AIs, a generated map; the first
    # turns are loading and the AIs' start-up.
    # MangoHud as a Vulkan layer only: its GL hook (the mangohud wrapper's
    # LD_PRELOAD) crashes 0 A.D. as it starts.
    local m mvk=()
    m=$(dirname "$(dirname "$(readlink -f "$(command -v mangohud)")")")
    mangohud_on && mvk=(env "MANGOHUD_CONFIG=$MH" MANGOHUD=1 "XDG_DATA_DIRS=$m/share"
        "LD_LIBRARY_PATH=$m/lib/mangohud${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}")
    # 0 A.D. refuses to run as root, which the guest's probes are: as
    # nobody there, with the compositor's socket and the GPU's nodes opened
    # to it for the run (a throwaway guest).
    local as=()
    if [ "$(id -u)" = 0 ]; then
        chown -R 65534:65534 "$OUT"
        local rt=${XDG_RUNTIME_DIR:-/run/user/0}
        chmod 0755 "$rt" 2>/dev/null
        chmod 0666 "$rt/${WAYLAND_DISPLAY:-wayland-0}" /dev/dri/* /dev/nvidia* 2>/dev/null
        as=(setpriv --reuid=65534 --regid=65534 --clear-groups)
    fi
    SDL_VIDEODRIVER=wayland timeout -s TERM -k 5 "$run" ${as[@]+"${as[@]}"} "${mvk[@]}" 0ad -quickstart -nosound \
        -conf="rendererbackend:$be" -conf="windowed:true" -conf="xres:1280" -conf="yres:720" \
        -conf="vsync:false" -conf="adaptivefps.session:0" -conf="adaptivefps.menu:0" \
        -autostart="random/mainland" -autostart-size=320 -autostart-seed=4 -autostart-players=4 \
        -autostart-ai=1:petra -autostart-ai=2:petra -autostart-ai=3:petra -autostart-ai=4:petra \
        -autostart-player=-1 >"$OUT/app.log" 2>&1
    meta "exit=$? wall_s=$((SECONDS - t0)) backend=$be"
    mangohud_on && mh_tail $((HEAVY_SECS * 1000))
}

# HEAVY_THP=always|madvise|never: the guest kernel's transparent huge pages
# for the run (a guest's own setting; natively the host's is left alone).
if [ -n "${HEAVY_THP:-}" ] && [ -w /sys/kernel/mm/transparent_hugepage/enabled ] &&
    grep -q hypervisor /proc/cpuinfo; then
    echo "$HEAVY_THP" >/sys/kernel/mm/transparent_hugepage/enabled
fi
[ -r /sys/kernel/mm/transparent_hugepage/enabled ] &&
    meta "thp=$(sed 's/.*\[\(.*\)\].*/\1/' /sys/kernel/mm/transparent_hugepage/enabled)"
IRQ0=$(irq_snap)
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
    gameloop)
        t0=$SECONDS
        GL_FRAMES=$OUT/frames.txt GL_WARM=$HEAVY_WARM GL_SECS=$HEAVY_SECS "$X/bin/nvgpu-gameloop" \
            >"$OUT/app.log" 2>&1
        meta "exit=$? wall_s=$((SECONDS - t0))"
        grep -a '^HEAVY_GAMELOOP' "$OUT/app.log" | sed 's/^/HEAVY-META /'
        ;;
    wine-godot-draws-d3d12) wine_godot draws d3d12 --resolution 1280x720 ;;
    wine-godot-draws-vk) wine_godot draws vulkan --resolution 1280x720 ;;
    wine-godot-gpu-d3d12) wine_godot gpu d3d12 --fullscreen ;;
    wine-heaven) wine_heaven ;;
    0ad) zeroad ;;
    probe-vk)
        # The device extensions the NVIDIA device lists, then whether one
        # with ray queries can be made and how fast it casts rays.
        vulkaninfo 2>/dev/null | awk '/^Device Extensions/ { on = 1; next } on && /^[A-Z]/ { exit }
            on && $1 ~ /^VK_/ { print $1 }' | sort -u >"$OUT/vk-exts.txt"
        meta "vk-exts count=$(wc -l <"$OUT/vk-exts.txt") $(tr '\n' ' ' <"$OUT/vk-exts.txt")"
        "$X/bin/nvgpu-rtprobe" 2>&1 | grep -a '^HEAVY_RT' | sed 's/^/HEAVY-META /'
        ;;
    wakecost) "$X/bin/nvgpu-wakecost" 0,1 0,2 2>&1 | grep -a '^HEAVY_WAKE' | sed 's/^/HEAVY-META /' ;;
    *) echo "unknown workload $WL" >&2; exit 2 ;;
esac
irq_delta "$IRQ0" "$(irq_snap)"
[ -s "$OUT/frames.txt" ] && meta "frames=$(wc -l <"$OUT/frames.txt")"
exit 0
