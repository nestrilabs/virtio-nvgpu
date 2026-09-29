#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Frame pacing, native against a guest: the same workload, the same MangoHud
# build and config, the same monitor, one after the other. What it measures is
# the application's own frame times (MangoHud's `frametime`, the interval
# between successive presents), per frame.
#
# Usage: rig/rig-framepace.sh <workload> <native|vm> <tag> [runs [first]]
#   workload  vkcube        vkcube, FIFO (vsync), native Wayland
#             vkcube-mbox   vkcube --present_mode 1 (mailbox)
#             vkmark        vkmark's `shading` scene, FIFO
#             stk           SuperTuxKart's profile race (GL, SDL2, Wayland)
#             gamescope     vkcube inside a nested gamescope (Xwayland)
#   native    rig/rig-native-run.sh --live: the guest image's own programs
#             and NVIDIA userspace, on the host
#   vm        rig/run-guest.sh --wayland-socket <session> run (the `run`
#             probe), every one of its knobs (NVGPU_VCPUS, NVGPU_VMM_KIND,
#             NVGPU_MEM_MIB, ...) as the environment has them
#   runs      how many times, one after another (default 3), numbered from
#             first (default 1): interleaving native and vm runs of one tag
#
# Environment:
#   NVGPU_FP_MON     the monitor (default DP-3): its active workspace takes
#                    the window, fullscreen, silently and without focus, by a
#                    window rule on the workloads' classes; it is powered on
#                    (DPMS) only while a run measures, and off afterwards
#                    (DP-3 is an OLED). NVGPU_FP_DPMS=0 leaves DPMS alone
#   NVGPU_FP_WARM    seconds before logging starts (default 6)
#   NVGPU_FP_SECS    seconds logged (default 20)
#   NVGPU_FP_BACKEND_ARGS  words for the backend (run-guest.sh's -- args)
#   NVGPU_FP_FULLSCREEN=0  leave the window as the app made it
#   NVGPU_FP_GUEST_PRE  a guest shell command run before the workload (such
#                    as a module parameter written through /sys/module)
#   NVGPU_FP_LOAD    stress-ng arguments for a host load that runs through
#                    each run (a busy desktop, reproducibly), e.g.
#                    "--cpu 32 --cpu-load 50"
#
# Nothing is typed or clicked into the session. Results: .rig/logs/fp/<tag>/
# <workload>-<mode>-<n>.csv, the guest's console and backend logs beside them,
# and a stats line per run (rig/framepace-stats.py) in summary.txt.
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
# shellcheck source=rig/lib.sh
. "$REPO/rig/lib.sh"
RIG=${NVGPU_RIG:-$REPO/.rig}
export NVGPU_RIG=$RIG
[ $# -ge 3 ] || { sed -n '2,40p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
WL=$1 MODE=$2 TAG=$3 RUNS=${4:-3} FIRST=${5:-1}
MON=${NVGPU_FP_MON:-DP-3}
WARM=${NVGPU_FP_WARM:-6}
SECS=${NVGPU_FP_SECS:-20}
OUT=$RIG/logs/fp/$TAG
mkdir -p "$OUT"
SESSION=${XDG_RUNTIME_DIR:-}/${WAYLAND_DISPLAY:-}
[ -S "$SESSION" ] || { echo "no session socket at $SESSION" >&2; exit 1; }
hyprctl -j version >/dev/null 2>&1 || { echo "hyprctl cannot reach Hyprland" >&2; exit 1; }

TARGET=
case $WL in
    vkcube) CMD='mangohud vkcube --wsi wayland' ;;
    vkcube-mbox) CMD='mangohud vkcube --wsi wayland --present_mode 1' ;;
    vkmark) CMD='mangohud vkmark --winsys wayland --run-forever -b shading' ;;
    stk) CMD="mangohud supertuxkart --no-start-screen --track=lighthouse --numkarts=4 --laps=9 --profile-time=$((WARM + SECS + 20)) --windowed" ;;
    # MangoHud in the game only. gamescope without CAP_SYS_NICE, as a user
    # runs it: as the guest's root it asks for a realtime queue, whose RM
    # control (FIFO_RUNLIST_SET_SCHED_POLICY) the backend refuses, and
    # vkCreateDevice fails. (Natively it cannot start in the Claude sandbox:
    # rig/TESTING-RIG.md.)
    gamescope) CMD='setpriv --bounding-set=-sys_nice --inh-caps=-sys_nice -- gamescope -W 1920 -H 1080 -- mangohud vkcube' ;;
    *) echo "unknown workload $WL" >&2; exit 2 ;;
esac
# MangoHud: every frame, the smallest overlay (fps only: with no overlay at
# all, no_display, 0.8.4 never starts the log), a log that starts after the
# warm-up and lasts SECS. The wrapper preloads its GL shim and enables the
# Vulkan layer for the process and its children.
MH="fps_only,log_interval=0,autostart_log=$WARM,log_duration=$SECS"
TOTAL=$((WARM + SECS + 6))

# ---- the monitor ------------------------------------------------------------
lua() { hyprctl eval "$1" >>"$OUT/hyprctl.log" 2>&1; }
dpms() { [ "${NVGPU_FP_DPMS:-1}" = 1 ] && lua "hl.dispatch(hl.dsp.dpms({ action = \"$1\", monitor = \"$MON\" }))"; }
WS=$(hyprctl -j monitors | jq -r --arg m "$MON" '.[] | select(.name == $m) | .activeWorkspace.id')
HZ=$(hyprctl -j monitors | jq -r --arg m "$MON" '.[] | select(.name == $m) | .refreshRate')
DS=$(hyprctl -j getoption render:direct_scanout | jq -r '.int')
[ -n "$WS" ] || { echo "no monitor $MON" >&2; exit 1; }
RULE=nvgpu-framepace
FS=true
[ "${NVGPU_FP_FULLSCREEN:-1}" = 1 ] || FS=false
# vkcube and vkmark set no app_id: they are matched by title.
lua "hl.window_rule({ name = \"$RULE\", enabled = true, match = { class = [[^(vkcube|vkmark|\.?supertuxkart.*|SuperTuxKart|gamescope)\$]] }, workspace = \"$WS silent\", no_initial_focus = true })"
lua "hl.window_rule({ name = \"$RULE-t\", enabled = true, match = { title = [[^(vkcube|vkmark.*|SuperTuxKart.*)\$]] }, workspace = \"$WS silent\", no_initial_focus = true })"
cleanup() {
    lua "hl.window_rule({ name = \"$RULE\", enabled = false })"
    lua "hl.window_rule({ name = \"$RULE-t\", enabled = false })"
    dpms off
}
trap cleanup EXIT
trap 'exit 130' INT TERM
{
    echo "# $(date -Is) $WL $MODE on $MON (${HZ} Hz, workspace $WS), render:direct_scanout=$DS"
    echo "# warm ${WARM}s, logged ${SECS}s; VM: vcpus=${NVGPU_VCPUS:-4} mem=${NVGPU_MEM_MIB:-4096} vmm=${NVGPU_VMM_KIND:-nesbox} pins=${NVGPU_VCPU_PINS:-} affinity=${NVGPU_CPU_AFFINITY:-} io=${NVGPU_IO_AFFINITY:-} hugepages=${NVGPU_HUGEPAGES:-} backend=${NVGPU_FP_BACKEND_ARGS:-} load=${NVGPU_FP_LOAD:-} guest-pre=${NVGPU_FP_GUEST_PRE:-} slice=${NVGPU_SLICE_US:-default}"
} >>"$OUT/summary.txt"

run_native() { # run_native N
    local d=$OUT/$WL-native-$1.d home
    rm -rf "$d"
    mkdir -p "$d"
    home=$(mktemp -d "${TMPDIR:-/tmp}/nvgpu-fp-home.XXXXXX")
    # A home of its own for the app: SuperTuxKart and MangoHud read and write
    # there. (Not nix's: a chroot store lives under the caller's HOME.)
    MANGOHUD_CONFIG="$MH,output_folder=$d" \
        "$REPO/rig/rig-native-run.sh" --live --timeout "$TOTAL" -- env HOME="$home" sh -c "$CMD" \
        >"$d/app.log" 2>&1
    rm -rf -- "$home"
    local f
    # (A program whose name starts with a dot, as a nix wrapper's does, makes
    # a hidden file.)
    f=$(find "$d" -maxdepth 1 -name '*.csv' ! -name '*_summary.csv' | head -n 1)
    [ -n "$f" ] && cp "$f" "$OUT/$WL-native-$1.csv"
}

run_vm() { # run_vm N
    local tag=fp-$TAG-$WL-$1 gcmd
    gcmd="mkdir -p /tmp/fp; ${NVGPU_FP_GUEST_PRE:-true}; HOME=/root MANGOHUD_CONFIG='$MH,output_folder=/tmp/fp' timeout -s TERM -k 3 $TOTAL sh -c '$CMD' >/tmp/fp/app.log 2>&1; f=\$(find /tmp/fp -maxdepth 1 -name \"*.csv\" ! -name \"*_summary.csv\" | head -n 1); echo FP_CSV_BEGIN; cat \"\$f\"; echo FP_CSV_END; echo FP_PACING_BEGIN; cat /sys/module/virtio_gpu_nv/parameters/pacing 2>/dev/null; echo FP_PACING_END; tail -n 20 /tmp/fp/app.log; true"
    local extra=()
    [ -n "${NVGPU_FP_BACKEND_ARGS:-}" ] && read -r -a extra <<<"$NVGPU_FP_BACKEND_ARGS"
    # How the host scheduled the VMM's and the backend's threads while the
    # log runs (framepace-sched.py): the guest boots in about 10 s.
    (
        sleep $((WARM + 12))
        pids=$(rig_vmm_pid)
        pids="$pids $(rig_backend_pid)"
        python3 "$REPO/rig/framepace-sched.py" $((SECS > 6 ? SECS - 4 : 2)) $pids >"$OUT/$WL-vm-$1.sched.txt" 2>&1
    ) &
    rig_run_vm "$tag" "$SESSION" "$TOTAL" "$gcmd" ${extra[@]+"${extra[@]}"} >"$OUT/$WL-vm-$1.launcher.log" 2>&1
    tr -d '\r' <"$RIG/logs/$tag.console.log" | sed -n '/^FP_CSV_BEGIN/,/^FP_CSV_END/p' | sed '1d;$d' >"$OUT/$WL-vm-$1.csv"
    # The two sides' pacing counters: the guest driver's, printed after the
    # log, and the backend's teardown report.
    rig_pacing "$RIG/logs/$tag.console.log" "$RIG/logs/$tag.backend.log" FP_PACING >"$OUT/$WL-vm-$1.pacing.txt"
    cp "$RIG/logs/$tag.backend.log" "$OUT/$WL-vm-$1.backend.log" 2>/dev/null
    cp "$RIG/logs/$tag.console.log" "$OUT/$WL-vm-$1.console.log" 2>/dev/null
    [ -s "$OUT/$WL-vm-$1.csv" ] || rm -f "$OUT/$WL-vm-$1.csv"
}

# The workload's window, once it maps on the monitor's workspace: made
# fullscreen by its address (the rule's own `fullscreen` does not take
# with a silent workspace), then where it is once logging runs -- monitor,
# workspace, fullscreen, and whether the monitor scans it out directly.
PAT='vkcube|vkmark|supertuxkart|gamescope'
place() {
    local a= i
    for i in $(seq 1 120); do
        a=$(hyprctl -j clients | jq -r --arg ws "$WS" --arg p "$PAT" '.[] | select(.workspace.name == $ws)
            | select((.class + " " + .title) | test($p; "i")) | .address' | head -n 1)
        [ -n "$a" ] && break
        sleep 0.5
    done
    [ -n "$a" ] || {
        echo "no window on workspace $WS; the clients:" >>"$OUT/placement.log"
        hyprctl -j clients | jq -c '.[] | {class, title, workspace: .workspace.name}' >>"$OUT/placement.log"
        return
    }
    [ "$FS" = true ] && lua "hl.dispatch(hl.dsp.window.fullscreen_state({ internal = 2, client = 2, window = \"address:$a\" }))"
    sleep $((WARM + 2))
    {
        hyprctl -j clients | jq -c --arg a "$a" '.[] | select(.address == $a)
            | {class, title, monitor, workspace: .workspace.name, fullscreen, size}'
        hyprctl -j monitors | jq -c --arg m "$MON" '.[] | select(.name == $m)
            | {name, refreshRate, directScanoutTo, directScanoutBlockedBy, solitary, solitaryBlockedBy}'
    } >>"$OUT/placement.log" 2>&1
}

files=()
for n in $(seq "$FIRST" $((FIRST + RUNS - 1))); do
    dpms on
    sleep 1
    echo "# run $n" >>"$OUT/placement.log"
    place &
    load=
    if [ -n "${NVGPU_FP_LOAD:-}" ]; then
        # shellcheck disable=SC2086 # words for stress-ng
        nix shell nixpkgs#stress-ng -c stress-ng $NVGPU_FP_LOAD --quiet >/dev/null 2>&1 &
        load=$!
    fi
    case $MODE in
        native) run_native "$n" ;;
        vm) run_vm "$n" ;;
        *) echo "mode: native or vm" >&2; exit 2 ;;
    esac
    [ -z "$load" ] || { pkill -TERM -P "$load" 2>/dev/null; kill "$load" 2>/dev/null; pkill -x stress-ng 2>/dev/null; }
    wait
    dpms off
    f=$OUT/$WL-$MODE-$n.csv
    if [ -s "$f" ]; then
        files+=("$f")
        python3 "$REPO/rig/framepace-stats.py" --hz "${TARGET:-$HZ}" "$f" | tee -a "$OUT/summary.txt"
    else
        echo "$WL $MODE run $n: no frame log" | tee -a "$OUT/summary.txt"
    fi
    sleep 2
done
[ ${#files[@]} -gt 1 ] &&
    python3 "$REPO/rig/framepace-stats.py" --hz "${TARGET:-$HZ}" --label "$WL-$MODE" "${files[@]}" | tail -n 2 | tee -a "$OUT/summary.txt"
exit 0
