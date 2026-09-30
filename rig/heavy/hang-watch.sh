#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A loop of one heavy workload (heavy-run.sh) that says which runs stopped
# making progress, and what the stopped program waits on: in a guest
# (/opt/heavy/hang-watch.sh, through the run probe) or natively.
#
# A run has stalled when its app.log has gained no HEAVY_ line for
# HANG_STALL seconds (default 30; Blender prints one a frame, Godot one at
# the start and one at the end, so give Godot its measured window and more).
# The stalled program's threads are then dumped -- each one's state, kernel
# stack and blocking syscall, with a poll's descriptors and a futex's word
# decoded (waits.py) -- and the guest module's counters, then it is killed
# and the next run starts.
#
# Usage: hang-watch.sh <workload> [runs]
# Output: HANGWATCH lines (a run's start and end, STALL, the total) and one
# HANGDUMP-BEGIN ... HANGDUMP-END block a stall.
set -uo pipefail
WL=${1:?workload}
RUNS=${2:-1}
STALL=${HANG_STALL:-30}
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
OUT=${HEAVY_OUT:-/tmp/heavy}
# The guest image has python3 in its store but not on PATH.
PY=$(command -v python3 || ls /nix/store/*-python3-3*/bin/python3 2>/dev/null | head -n 1)

dump() {
    local p=$1 t
    echo "HANGDUMP-BEGIN pid=$p exe=$(readlink "/proc/$p/exe")"
    echo "--- last lines"
    grep -a HEAVY_ "$OUT/app.log" | tail -n 3
    echo "--- descriptors"
    ls -l "/proc/$p/fd" 2>/dev/null | awk 'NR > 1 { print "  " $(NF-2), $NF }' | grep -v 'socket:\|pipe:'
    for t in "/proc/$p/task"/*; do
        echo "== tid ${t##*/} $(cat "$t/comm") state=$(awk '{ sub(/^.*\) /, ""); print $1 }' "$t/stat") wchan=$(cat "$t/wchan")"
        head -n 14 "$t/stack" 2>/dev/null | sed 's/^/    /'
    done
    echo "--- waits"
    [ -n "$PY" ] && "$PY" "$DIR/waits.py" "$p"
    if [ -r /sys/module/virtio_gpu_nv/parameters/pacing ]; then
        echo "--- guest module counters"
        grep -v '^type\|_log2' /sys/module/virtio_gpu_nv/parameters/pacing
    fi
    echo "HANGDUMP-END"
}

# The workload's own process, not the shells around it.
prog_pid() {
    local q
    for q in $(pgrep -f "$1"); do
        case $(cat "/proc/$q/comm" 2>/dev/null) in bash | sh | timeout) ;; *) echo "$q"; return ;; esac
    done
}

hangs=0
for i in $(seq 1 "$RUNS"); do
    echo "HANGWATCH run $i start $(date +%s)"
    bash "$DIR/heavy-run.sh" "$WL" "$OUT" >"$OUT.run.log" 2>&1 &
    rp=$!
    last=-1 since=$SECONDS
    while kill -0 "$rp" 2>/dev/null; do
        sleep 2
        cur=$(grep -ac HEAVY_ "$OUT/app.log" 2>/dev/null)
        if [ "${cur:-0}" != "$last" ]; then
            last=${cur:-0} since=$SECONDS
            continue
        fi
        [ $((SECONDS - since)) -ge "$STALL" ] || continue
        p=$(prog_pid blender)
        [ -n "$p" ] || p=$(prog_pid godot)
        [ -n "$p" ] || p=$(prog_pid supertuxkart)
        echo "HANGWATCH run $i STALL: no progress for $((SECONDS - since)) s, pid ${p:-none}"
        hangs=$((hangs + 1))
        if [ -n "$p" ]; then
            dump "$p"
            kill -KILL "$p"
        fi
        pkill -KILL -f "heavy-run.sh $WL" 2>/dev/null
        break
    done
    wait "$rp" 2>/dev/null
    echo "HANGWATCH run $i end $(date +%s) stalls=$hangs"
done
echo "HANGWATCH total runs=$RUNS stalls=$hangs"
