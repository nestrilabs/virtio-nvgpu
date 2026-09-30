#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Heavy workloads, native against a guest (BENCHMARKS.md, "Heavy
# workloads"): games, engines and renderers run unpaced against the rig's
# headless sway, their frame times recorded the same way both ways
# (rig/heavy/heavy-run.sh; MangoHud or the program's own), and summed up by
# rig/heavy/heavy-stats.py. Nothing reaches a monitor.
#
# Usage: rig/rig-heavy.sh <workload> <native|vm> <tag> [runs [first]]
#   workload  one of rig/heavy/heavy-run.sh's (stk-ultra, stk-vk,
#             stk-low, godot-gpu, godot-draws, godot-draws-gl, blender-gl,
#             blender-vk, vk-stream, vk-stream-pool; and from
#             rig/heavy/extras.nix and HEAVY_WIN: gameloop,
#             wine-godot-draws-d3d12, wine-godot-draws-vk,
#             wine-godot-gpu-d3d12, wine-heaven, 0ad, probe-vk, wakecost)
#   native    rig/rig-native-run.sh: the guest image's programs and NVIDIA
#             userspace on the host (NVGPU_HEAVY_NATIVE_CPUS=0-3 confines it
#             to as many CPUs as the guest has, with taskset)
#   vm        rig/run-guest.sh --wayland-socket <headless sway> run, every
#             knob of run-guest.sh from the environment (NVGPU_VMM_KIND,
#             NVGPU_BACKEND, NVGPU_ROOTFS, NVGPU_VCPUS, ...); the image must
#             have /opt/heavy (rig/heavy/mkimage-heavy.sh)
#
# Environment:
#   NVGPU_HEAVY_BACKEND_ARGS  words for the backend (run-guest.sh's --)
#   NVGPU_HEAVY_GUEST_PRE     a guest shell command run first
#   NVGPU_HEAVY_ENV           VAR=value,... for heavy-run.sh, natively and in
#                             a guest (HEAVY_THP=always, GL_THREADS=8, ...)
#   NVGPU_HEAVY_MODE          the headless output's mode for the run
#                             (default: the workload's, below)
#   NVGPU_HEAVY_PERF=1        vm: perf record the backend and the VMM while
#                             the workload runs (user space, 20 s, 999 Hz;
#                             needs kernel.perf_event_paranoid <= 2)
#   NVGPU_PACING_STATS        the backend's periodic pacing report (default 5 s)
#   NVGPU_HEAVY_BENCH         native: the nvgpu-bench for vk-stream (a guest
#                             image carries its own, mkimage-heavy.sh --file)
#   NVGPU_HEAVY_EXTRAS        native: rig/heavy/extras.nix's store path (an
#                             image has it at /opt/heavy/extras)
#   NVGPU_HEAVY_WIN           native: the Windows programs' directory
#                             (heavy-run.sh's HEAVY_WIN; /opt/heavy/win)
#   NVGPU_HEAVY_WINE_PREFIX   native: the Wine prefix (/opt/heavy/wine/prefix)
#   NVGPU_HEAVY_KVMSTAT=1     vm: KVM's counters for a window of the run
#                             (rig/heavy/kvmstat.py; .kvmstat), from
#                             NVGPU_HEAVY_KVMSTAT_DELAY s after the VMM
#                             starts (14) for NVGPU_HEAVY_KVMSTAT_SECS (12)
#
# Results: $NVGPU_RIG/logs/heavy/<tag>/<workload>-<mode>-<n>.{frames,meta},
# for a guest also .cpu (backend and VMM CPU over the run), .pacing, and the
# backend's and console's logs; summary.txt has a stats line a run.
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
export NVGPU_RIG=$RIG
[ $# -ge 3 ] || { sed -n '2,49p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
WLD=$1 MODE=$2 TAG=$3 RUNS=${4:-3} FIRST=${5:-1}
OUT=${NVGPU_HEAVY_OUT:-$RIG/logs/heavy}/$TAG
mkdir -p "$OUT"
SOCK=$(cat "$RIG/run/headless-sway.socket" 2>/dev/null)
[ -S "$SOCK" ] || { echo "no headless sway: start rig/rig-headless-sway.sh" >&2; exit 1; }
export NIX_CONFIG=${NIX_CONFIG:-experimental-features = nix-command flakes}

case $WLD in
    stk-ultra | stk-vk | godot-gpu) DMODE=3840x2160 BUDGET=240 ;;
    stk-low | godot-draws | godot-draws-gl) DMODE=1280x720 BUDGET=240 ;;
    blender-gl | blender-vk) DMODE=1920x1080 BUDGET=600 ;;
    vk-stream | vk-stream-pool) DMODE=1920x1080 BUDGET=120 ;;
    gameloop | wine-godot-draws-d3d12 | wine-godot-draws-vk | wine-heaven) DMODE=1280x720 BUDGET=240 ;;
    wine-godot-gpu-d3d12) DMODE=3840x2160 BUDGET=240 ;;
    0ad) DMODE=1280x720 BUDGET=300 ;;
    probe-vk | wakecost) DMODE=1920x1080 BUDGET=120 ;;
    *) echo "unknown workload $WLD" >&2; exit 2 ;;
esac
OMODE=${NVGPU_HEAVY_MODE:-$DMODE}
swaymsg_() {
    SWAYSOCK=$(ls "$(dirname "$SOCK")"/sway-ipc.*.sock | head -n 1) nix shell nixpkgs#sway -c swaymsg "$@"
}
swaymsg_ output HEADLESS-1 mode "$OMODE@60Hz" >/dev/null ||
    { echo "could not set the headless output to $OMODE" >&2; exit 1; }

echo "# $(date -Is) $WLD $MODE output=$OMODE vmm=${NVGPU_VMM_KIND:-nesbox} vcpus=${NVGPU_VCPUS:-4} mem=${NVGPU_MEM_MIB:-8192} backend=${NVGPU_BACKEND:-default} rootfs=${NVGPU_ROOTFS:-default} args=[${NVGPU_HEAVY_BACKEND_ARGS:-}] pre=[${NVGPU_HEAVY_GUEST_PRE:-}] env=[${NVGPU_HEAVY_ENV:-}] cmdline=[${NVGPU_CMDLINE_EXTRA:-}] native-cpus=${NVGPU_HEAVY_NATIVE_CPUS:-all}" >>"$OUT/summary.txt"

stat_ticks() { # utime + stime of processes, all their threads, in clock ticks
    local p t=0 x
    for p in "$@"; do
        # No pid (a run whose backend never started): /proc//stat is
        # /proc/stat, which is not a process's.
        [ -n "$p" ] || continue
        x=$(awk '{ sub(/^.*\) /, ""); print $12 + $13 }' "/proc/$p/stat" 2>/dev/null) && t=$((t + x))
    done
    echo "$t"
}
# Every process of the VMM, whoever it runs as: crosvm's device processes
# run in user namespaces of their own, with no uid mapped (one VM at a time).
vmm_pids() { pgrep -x nesbox; pgrep -x crosvm; }

run_native() { # run_native N
    local d=$OUT/$WLD-native-$1.d pre=()
    [ -n "${NVGPU_HEAVY_NATIVE_CPUS:-}" ] && pre=(taskset -c "$NVGPU_HEAVY_NATIVE_CPUS")
    NVGPU_NATIVE_PKGS=${NVGPU_HEAVY_EXTRAS:-} \
        "$REPO/rig/rig-native-run.sh" --timeout "$BUDGET" -- ${pre[@]+"${pre[@]}"} \
        env ${NVGPU_HEAVY_ENV:+${NVGPU_HEAVY_ENV//,/ }} HEAVY_DIR="$REPO/rig/heavy" HEAVY_BENCH="${NVGPU_HEAVY_BENCH:-}" \
        HEAVY_EXTRAS="${NVGPU_HEAVY_EXTRAS:-}" HEAVY_WIN="${NVGPU_HEAVY_WIN:-}" \
        HEAVY_WINE_PREFIX="${NVGPU_HEAVY_WINE_PREFIX:-}" \
        bash "$REPO/rig/heavy/heavy-run.sh" "$WLD" "$d" \
        >"$OUT/$WLD-native-$1.log" 2>&1
    # Wine's server outlives the program by a few seconds.
    [ -n "${NVGPU_HEAVY_WINE_PREFIX:-}" ] &&
        NVGPU_NATIVE_PKGS=${NVGPU_HEAVY_EXTRAS:-} "$REPO/rig/rig-native-run.sh" --timeout 20 -- \
            env WINEPREFIX="$NVGPU_HEAVY_WINE_PREFIX" "${NVGPU_HEAVY_EXTRAS:-}/bin/wineserver" -k >/dev/null 2>&1
    # A game that aborts can leave a process behind that goes on rendering
    # on the compositor (SuperTuxKart 1.5 on Vulkan can): anything still
    # running from the image's programs is this run's, and would load the
    # next one.
    if pgrep -u "$(id -u)" -f '^/nix/store/[^ ]*-nvgpu-guest-sw/bin/' >/dev/null; then
        echo "HEAVY-META leftover process(es) killed" >>"$OUT/$WLD-native-$1.log"
        pkill -KILL -u "$(id -u)" -f '^/nix/store/[^ ]*-nvgpu-guest-sw/bin/'
    fi
    grep -a '^HEAVY-META' "$OUT/$WLD-native-$1.log" >"$OUT/$WLD-native-$1.meta"
    [ -s "$d/frames.txt" ] && cp "$d/frames.txt" "$OUT/$WLD-native-$1.frames"
    rm -rf "${d:?}/home" "${d:?}/godot"
}

run_vm() { # run_vm N
    local tag=heavy-$TAG-$WLD-$1 gcmd extra=() logs=${NVGPU_LOGS:-$RIG/logs}
    gcmd="${NVGPU_HEAVY_GUEST_PRE:-true}; ${NVGPU_HEAVY_ENV:+export ${NVGPU_HEAVY_ENV//,/ };} bash /opt/heavy/heavy-run.sh $WLD /tmp/heavy; echo HEAVY_FRAMES_BEGIN; gzip -c /tmp/heavy/frames.txt 2>/dev/null | base64 -w 0; echo; echo HEAVY_FRAMES_END; echo HEAVY_PACING_BEGIN; cat /sys/module/virtio_gpu_nv/parameters/pacing 2>/dev/null; echo HEAVY_PACING_END; tail -n 30 /tmp/heavy/app.log; true"
    [ -n "${NVGPU_HEAVY_BACKEND_ARGS:-}" ] && read -r -a extra <<<"$NVGPU_HEAVY_BACKEND_ARGS"
    local console=$logs/$tag.console.log kvm=()
    rm -f "$console"
    [ "${NVGPU_HEAVY_KVMSTAT:-0}" = 1 ] &&
        kvm=(env NESBOX_HOLD_KVM_STATS=1 python3 "$REPO/rig/heavy/kvmstat.py" --out "$OUT/$WLD-vm-$1.kvmstat"
            --delay "${NVGPU_HEAVY_KVMSTAT_DELAY:-14}" --secs "${NVGPU_HEAVY_KVMSTAT_SECS:-12}" --)
    NVGPU_MEM_MIB=${NVGPU_MEM_MIB:-8192} NVGPU_PACING_STATS=${NVGPU_PACING_STATS:-5} \
        NVGPU_TIMEOUT=${NVGPU_TIMEOUT:-$((BUDGET + 120))} \
        NVGPU_CMDLINE_EXTRA="nvgpu_wl=1 nvgpu_timeout=$((BUDGET + 90)) nvgpu_cmd=$(printf %s "$gcmd" | base64 -w0) ${NVGPU_CMDLINE_EXTRA:-}" \
        ${kvm[@]+"${kvm[@]}"} "$REPO/rig/run-guest.sh" --wayland-socket "$SOCK" run "$tag" ${extra[@]+-- "${extra[@]}"} \
        >"$OUT/$WLD-vm-$1.launcher.log" 2>&1 &
    local launcher=$! be='' vmm=''
    for _ in $(seq 1 150); do
        be=$(pgrep -n -u "$(id -u)" -f '^[^ ]*/vhost-user-nvgpu --socket ') &&
            vmm=$(pgrep -n -u "$(id -u)" -x nesbox || pgrep -n -u "$(id -u)" -x crosvm) && break
        sleep 0.2
    done
    # The backend's and the VMM's CPU over the whole run, and (optionally) a
    # user-space profile of both while the workload runs.
    local be0 vmm0 t0
    # shellcheck disable=SC2046 # the pids are words
    be0=$(stat_ticks "$be") vmm0=$(stat_ticks $(vmm_pids)) t0=$(date +%s.%N)
    local perfpid=
    if [ "${NVGPU_HEAVY_PERF:-0}" = 1 ] && [ -n "$be" ]; then
        (
            sleep "${NVGPU_HEAVY_PERF_DELAY:-35}"
            nix shell nixpkgs#perf -c perf record -F 999 -g --call-graph dwarf,16384 -e cpu-clock:u \
                -o "$OUT/$WLD-vm-$1.backend.perf" -p "$be" -- sleep 20 >/dev/null 2>&1 &
            nix shell nixpkgs#perf -c perf record -F 999 -g -e cpu-clock:u \
                -o "$OUT/$WLD-vm-$1.vmm.perf" -p "$vmm" -- sleep 20 >/dev/null 2>&1 &
            wait
        ) &
        perfpid=$!
    fi
    local last_be=$be0 last_vmm=$vmm0 t1=$t0
    while kill -0 "$launcher" 2>/dev/null; do
        local b v
        # shellcheck disable=SC2046
        b=$(stat_ticks "$be") v=$(stat_ticks $(vmm_pids))
        # Only while both are alive (a read of a dead process is 0).
        [ "$b" != 0 ] && last_be=$b && last_vmm=$v && t1=$(date +%s.%N)
        sleep 0.5
    done
    wait "$launcher"
    local rc=$?
    [ -n "$perfpid" ] && wait "$perfpid" 2>/dev/null
    local tck
    tck=$(getconf CLK_TCK)
    awk -v b=$((last_be - be0)) -v v=$((last_vmm - vmm0)) -v t0="$t0" -v t1="$t1" -v hz="$tck" \
        'BEGIN { w = t1 - t0; printf "HEAVY-CPU wall=%.1fs backend=%.1fs (%.0f%%) vmm=%.1fs (%.0f%%)\n", w, b/hz, 100*b/hz/w, v/hz, 100*v/hz/w }' \
        >"$OUT/$WLD-vm-$1.cpu"
    tr -d '\r' <"$console" | grep -a '^HEAVY-META' >"$OUT/$WLD-vm-$1.meta"
    echo "launcher-rc=$rc" >>"$OUT/$WLD-vm-$1.meta"
    tr -d '\r' <"$console" | sed -n '/^HEAVY_FRAMES_BEGIN/,/^HEAVY_FRAMES_END/p' | sed '1d;$d' |
        tr -d '\n' | base64 -d 2>/dev/null | gzip -dc >"$OUT/$WLD-vm-$1.frames" 2>/dev/null
    [ -s "$OUT/$WLD-vm-$1.frames" ] || rm -f "$OUT/$WLD-vm-$1.frames"
    {
        tr -d '\r' <"$console" | sed -n '/^HEAVY_PACING_BEGIN/,/^HEAVY_PACING_END/p' | sed '1d;$d' | sed 's/^/guest: /'
        grep -a 'pacing:\|window use' "$logs/$tag.backend.log" | sed 's/^.*\] //'
    } >"$OUT/$WLD-vm-$1.pacing"
    cp "$logs/$tag.backend.log" "$OUT/$WLD-vm-$1.backend.log" 2>/dev/null
    cp "$console" "$OUT/$WLD-vm-$1.console.log" 2>/dev/null
}

for n in $(seq "$FIRST" $((FIRST + RUNS - 1))); do
    case $MODE in
        native) run_native "$n" ;;
        vm) run_vm "$n" ;;
        *) echo "mode: native or vm" >&2; exit 2 ;;
    esac
    f=$OUT/$WLD-$MODE-$n.frames
    if [ -s "$f" ]; then
        python3 "$REPO/rig/heavy/heavy-stats.py" "$WLD-$MODE-$n=$f" | tail -n 1 | tee -a "$OUT/summary.txt"
    else
        echo "$WLD $MODE run $n: no frames" | tee -a "$OUT/summary.txt"
    fi
    grep -a 'stk:\|done\|first frame\|BENCH' "$OUT/$WLD-$MODE-$n.meta" 2>/dev/null | sed 's/^/    /' | tee -a "$OUT/summary.txt"
    [ -f "$OUT/$WLD-$MODE-$n.cpu" ] && sed 's/^/    /' "$OUT/$WLD-$MODE-$n.cpu" | tee -a "$OUT/summary.txt"
    grep -a 'guest-rates\|HEAVY_GAMELOOP done\|wine-\|HEAVY_RT\|HEAVY_WAKE' "$OUT/$WLD-$MODE-$n.meta" 2>/dev/null | sed 's/^/    /' | tee -a "$OUT/summary.txt"
    [ -f "$OUT/$WLD-$MODE-$n.kvmstat" ] &&
        grep -E 'window=| (exits|halt_exits|irq_exits|io_exits|mmio_exits|halt_wakeup|halt_successful_poll|pf_taken) ' \
            "$OUT/$WLD-$MODE-$n.kvmstat" | sed 's/^/    /' | tee -a "$OUT/summary.txt"
    sleep 2
done
swaymsg_ output HEADLESS-1 mode 1920x1080@60Hz >/dev/null
exit 0
