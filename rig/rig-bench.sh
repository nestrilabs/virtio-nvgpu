#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# The benchmark matrix of BENCHMARKS.md, natively or in a guest: the image's
# nvgpu-bench-suite, the same script and programs both ways, against the
# rig's headless sway (never the desktop: nothing it draws reaches a monitor).
#
# Usage: rig/rig-bench.sh <native|vm> <tag> [runs [first]]
#   native   rig/rig-native-run.sh: the guest image's programs and NVIDIA
#            userspace on the host
#   vm       rig/run-guest.sh --wayland-socket <headless sway> run, with every
#            knob run-guest.sh takes from the environment (NVGPU_VMM_KIND,
#            NVGPU_COMPUTE, NVGPU_VCPUS, NVGPU_MEM_MIB, ...)
#   runs     how many times (default 3), numbered from first (default 1)
#
# Environment:
#   NVGPU_BENCH_GROUPS   the suite's groups (default "micro gpu video startup
#                        wl"; add cuda, which needs NVGPU_COMPUTE=1 in a guest)
#   NVGPU_BENCH_WL       the compositor socket (default: the headless sway's,
#                        $NVGPU_RIG/run/headless-sway.socket; start it with
#                        rig/rig-headless-sway.sh)
#   NVGPU_BENCH_BACKEND_ARGS  words for the backend (run-guest.sh's -- args)
#   NVGPU_BENCH_GUEST_PRE  a guest shell command run first (a module
#                        parameter written through /sys/module, say)
#   NVGPU_BENCH_SUITE    the suite to run (default nvgpu-bench-suite on PATH)
#
# Results: $NVGPU_RIG/logs/bench/<tag>/<mode>-<n>.bench, the BENCH lines;
# for a guest also <mode>-<n>.cpu, the backend's and the VMM's CPU time per
# suite group (sampled from /proc every 100 ms, charged by when each group's
# BENCH-SECTION lines say it began and ended), and
# <mode>-<n>.pacing, both sides' pacing counters. rig/bench-stats.py makes
# the table.
set -uo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
export NVGPU_RIG=$RIG
[ $# -ge 2 ] || { sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
MODE=$1 TAG=$2 RUNS=${3:-3} FIRST=${4:-1}
GROUPS_=${NVGPU_BENCH_GROUPS:-micro gpu video startup wl}
SUITE=${NVGPU_BENCH_SUITE:-nvgpu-bench-suite}
OUT=$RIG/logs/bench/$TAG
mkdir -p "$OUT"
WL=${NVGPU_BENCH_WL:-$(cat "$RIG/run/headless-sway.socket" 2>/dev/null)}
[ -S "$WL" ] || { echo "no compositor socket ($WL): start rig/rig-headless-sway.sh" >&2; exit 1; }
# Budget: the groups' worst cases, generously.
BUDGET=120
for g in $GROUPS_; do
    case $g in
        micro) BUDGET=$((BUDGET + 240)) ;; cuda) BUDGET=$((BUDGET + 600)) ;;
        gpu) BUDGET=$((BUDGET + 300)) ;; video) BUDGET=$((BUDGET + 400)) ;;
        startup) BUDGET=$((BUDGET + 120)) ;; wl) BUDGET=$((BUDGET + 120)) ;;
    esac
done

echo "# $(date -Is) $MODE groups=[$GROUPS_] vmm=${NVGPU_VMM_KIND:-nesbox} compute=${NVGPU_COMPUTE:-0} vcpus=${NVGPU_VCPUS:-4} mem=${NVGPU_MEM_MIB:-4096} backend=[${NVGPU_BENCH_BACKEND_ARGS:-}] guest-pre=[${NVGPU_BENCH_GUEST_PRE:-}] slice=${NVGPU_SLICE_US:-default}" >>"$OUT/summary.txt"

stat_ticks() { # the utime + stime of a process, all its threads, in clock ticks
    [ -n "$1" ] && awk '{ sub(/^.*\) /, ""); print $12 + $13 }' "/proc/$1/stat" 2>/dev/null || echo 0
}

# The image's media and scripts, where the suite finds them natively.
phys() { for b in "$HOME/.local/share/nix/root" ""; do [ -e "$b$1" ] && { echo "$b$1"; return; }; done; echo "$1"; }
APPS=$(readlink "$(phys "$(readlink "$RIG/guest/result")")/opt/nvgpu/apps")

run_native() { # run_native N
    # Without XDG_DATA_DIRS, as the guest's probes run: `nix shell` points it
    # at the image's share/, where MangoHud's and gamescope's implicit Vulkan
    # layers are, which the guest's loader never finds (vulkaninfo took twice
    # as long natively with them).
    # shellcheck disable=SC2086 # the groups are words
    "$REPO/rig/rig-native-run.sh" --timeout "$BUDGET" -- env -u XDG_DATA_DIRS NVGPU_APPS="$APPS" \
        "$SUITE" $GROUPS_ >"$OUT/native-$1.log" 2>&1
    grep -a '^BENCH' "$OUT/native-$1.log" >"$OUT/native-$1.bench"
}

run_vm() { # run_vm N
    local tag=bench-$TAG-$1 gcmd extra=()
    gcmd="${NVGPU_BENCH_GUEST_PRE:-true}; $SUITE $GROUPS_; echo BENCH_PACING_BEGIN; cat /sys/module/virtio_gpu_nv/parameters/pacing 2>/dev/null; echo BENCH_PACING_END; true"
    [ -n "${NVGPU_BENCH_BACKEND_ARGS:-}" ] && read -r -a extra <<<"$NVGPU_BENCH_BACKEND_ARGS"
    local console=$RIG/logs/$tag.console.log
    rm -f "$console"
    NVGPU_TIMEOUT=${NVGPU_TIMEOUT:-$((BUDGET + 90))} \
        NVGPU_CMDLINE_EXTRA="nvgpu_wl=1 nvgpu_timeout=$((BUDGET + 60)) nvgpu_cmd=$(printf %s "$gcmd" | base64 -w0) ${NVGPU_CMDLINE_EXTRA:-}" \
        "$REPO/rig/run-guest.sh" --wayland-socket "$WL" run "$tag" ${extra[@]+-- "${extra[@]}"} \
        >"$OUT/vm-$1.launcher.log" 2>&1 &
    local launcher=$! be= vmm= i
    # The backend's and the VMM's CPU, sampled; the console's section lines,
    # stamped as they arrive.
    for i in $(seq 1 100); do
        # This user's, and the backend as run-guest.sh starts it (the
        # binary, then --socket): not another VM's, nor an editor's.
        be=$(pgrep -n -u "$(id -u)" -f '^[^ ]*/vhost-user-nvgpu --socket ') &&
            vmm=$(pgrep -n -u "$(id -u)" -x nesbox || pgrep -n -u "$(id -u)" -x crosvm) && break
        sleep 0.2
    done
    (
        while kill -0 "$launcher" 2>/dev/null; do
            echo "$(date +%s.%N) $(stat_ticks "$be") $(stat_ticks "$vmm")"
            sleep 0.1
        done
    ) >"$OUT/vm-$1.samples" &
    local sampler=$!
    (
        for i in $(seq 1 100); do [ -e "$console" ] && break; sleep 0.2; done
        tail -s 0.05 -n +1 -F --pid="$launcher" "$console" 2>/dev/null | tr -d '\r' | while IFS= read -r l; do
            case $l in *BENCH-SECTION*) echo "$(date +%s.%N) ${l#*BENCH-SECTION }" ;; esac
        done
    ) >"$OUT/vm-$1.sections" &
    local stamper=$!
    wait "$launcher"
    kill "$sampler" "$stamper" 2>/dev/null
    wait 2>/dev/null
    tr -d '\r' <"$console" | grep -a '^BENCH' | grep -av '^BENCH-SECTION\|^BENCH_PACING' >"$OUT/vm-$1.bench"
    {
        tr -d '\r' <"$console" | sed -n '/^BENCH_PACING_BEGIN/,/^BENCH_PACING_END/p' | sed '1d;$d' | sed 's/^/guest: /'
        grep -a 'pacing:' "$RIG/logs/$tag.backend.log" | sed 's/^.*\] //'
    } >"$OUT/vm-$1.pacing"
    python3 "$REPO/rig/bench-cpu.py" "$OUT/vm-$1.samples" "$OUT/vm-$1.sections" >"$OUT/vm-$1.cpu"
    cat "$OUT/vm-$1.cpu" >>"$OUT/vm-$1.bench"
    cp "$RIG/logs/$tag.backend.log" "$OUT/vm-$1.backend.log" 2>/dev/null
    cp "$console" "$OUT/vm-$1.console.log" 2>/dev/null
}

for n in $(seq "$FIRST" $((FIRST + RUNS - 1))); do
    case $MODE in
        native) run_native "$n" ;;
        vm) run_vm "$n" ;;
        *) echo "mode: native or vm" >&2; exit 2 ;;
    esac
    echo "$MODE-$n: $(grep -c '^BENCH ' "$OUT/$MODE-$n.bench") figures, $(grep -c '^BENCH-FAIL' "$OUT/$MODE-$n.bench") failed" | tee -a "$OUT/summary.txt"
done
