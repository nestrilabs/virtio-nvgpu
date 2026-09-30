#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A loop of one heavy workload (heavy-run.sh) that says which runs stopped
# making progress, and what the stopped program waits on: in a guest
# (/opt/heavy/hang-watch.sh, through the run probe) or natively.
#
# A run has stalled when its app.log has gained no HEAVY_ line for
# HANG_STALL seconds (default 30; Blender prints one a frame, Godot one at
# the start and one at the end, so give Godot its measured window and more).
# Then, in one HANGDUMP block:
#   - the kernel's blocked-task report (sysrq w, as root): every task in D,
#     kernel threads too, with its stack -- a journal or writeback thread
#     there says the disk, not the program, is what stopped;
#   - the stalled program's threads: state, kernel stack and blocking
#     syscall, with a poll's descriptors and a futex's word decoded
#     (waits.py), and the guest module's counters;
#   - in a guest, whether one read on each CPU's block queue unblocks the
#     D tasks (a completion the device never interrupted for, or a request
#     it was never told of, is picked up by the next one on its queue);
# then it is killed, and a program still there HANG_AFTER s later (default
# 5) is dumped again: that is an uninterruptible wait.
#
# A stalled guest may have stopped reading its disk: every command run from
# here once it stalls is read in first, and every read of another process's
# /proc files that can block (its stack, its memory, its command line) is
# bounded, so the report still comes out.
#
# Usage: hang-watch.sh <workload> [runs]
# Output: HANGWATCH lines (a run's start and end, STALL, the total) and one
# HANGDUMP-BEGIN ... HANGDUMP-END block a stall.
set -uo pipefail
WL=${1:?workload}
RUNS=${2:-1}
STALL=${HANG_STALL:-30}
AFTER=${HANG_AFTER:-5}
DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
OUT=${HEAVY_OUT:-/tmp/heavy}
# The guest image has python3 in its store but not on PATH.
PY=$(command -v python3 || ls /nix/store/*-python3-3*/bin/python3 2>/dev/null | head -n 1)
IN_GUEST=0
grep -q '^flags.* hypervisor' /proc/cpuinfo && [ "$(id -u)" = 0 ] && [ -b /dev/vda ] && IN_GUEST=1

# Everything the stall handling runs, into the page cache now.
for b in timeout taskset dd awk sed grep head tail tr ls dmesg readlink sleep cat pgrep pkill nproc seq; do
    b=$(command -v "$b") && cat "$b" >/dev/null
done
[ -n "$PY" ] && timeout 30 "$PY" -c 'import os, struct, sys' 2>/dev/null
cat "$DIR/waits.py" >/dev/null 2>&1

# One task's state, wchan, syscall and kernel stack; the reads that can wait
# on the task are bounded.
task_dump() { # <task dir> <indent>
    local t=$1 st
    st=$(awk '{ sub(/^.*\) /, ""); print $1 }' "$t/stat" 2>/dev/null)
    echo "$2== tid ${t##*/} $(cat "$t/comm" 2>/dev/null) state=$st wchan=$(cat "$t/wchan" 2>/dev/null)"
    timeout -s KILL 2 head -n 14 "$t/stack" 2>/dev/null | sed "s/^/$2    /"
}

d_count() {
    local t n=0
    for t in /proc/[0-9]*/task/*; do
        [ "$(awk '{ sub(/^.*\) /, ""); print $1 }' "$t/stat" 2>/dev/null)" = D ] && n=$((n + 1))
    done
    echo "$n"
}

blocked_tasks() {
    if [ -w /proc/sysrq-trigger ]; then
        echo "--- blocked tasks (sysrq w)"
        echo 1 >/proc/sys/kernel/sysrq 2>/dev/null
        dmesg -c >/dev/null 2>&1
        echo w >/proc/sysrq-trigger
        sleep 1
        dmesg -c 2>/dev/null | grep -a -v 'used greatest stack'
    else
        echo "--- tasks in D"
        local t
        for t in /proc/[0-9]*/task/*; do
            [ "$(awk '{ sub(/^.*\) /, ""); print $1 }' "$t/stat" 2>/dev/null)" = D ] && task_dump "$t" "  "
        done
    fi
}

# A lost notification on a block queue is recovered by the next request on
# that queue; blk-mq gives each CPU its queue, so one direct read from each.
block_poke() {
    [ "$IN_GUEST" = 1 ] || return 0
    local c before after pids=()
    before=$(d_count)
    for c in $(seq 0 $(($(nproc) - 1))); do
        timeout -s KILL 10 taskset -c "$c" dd if=/dev/vda of=/dev/null bs=4096 count=1 \
            skip=$((RANDOM * 64 + c)) iflag=direct 2>/dev/null &
        pids+=($!)
    done
    # Only these: the run itself is a background job of this shell too.
    wait "${pids[@]}"
    sleep 2
    after=$(d_count)
    echo "--- block queues poked: tasks in D before $before, after $after"
}

dump() {
    local p=$1 t
    echo "HANGDUMP-BEGIN pid=$p exe=$(readlink "/proc/$p/exe")"
    echo "--- last lines"
    grep -a HEAVY_ "$OUT/app.log" | tail -n 3
    blocked_tasks
    echo "--- descriptors"
    ls -l "/proc/$p/fd" 2>/dev/null | awk 'NR > 1 { print "  " $(NF-2), $NF }' | grep -v 'socket:\|pipe:'
    for t in "/proc/$p/task"/*; do task_dump "$t" ""; done
    echo "--- waits"
    [ -n "$PY" ] && timeout -s KILL 10 "$PY" "$DIR/waits.py" "$p"
    if [ -r /sys/module/virtio_gpu_nv/parameters/pacing ]; then
        echo "--- guest module counters"
        grep -v '^type\|_log2' /sys/module/virtio_gpu_nv/parameters/pacing
    fi
    block_poke
    echo "HANGDUMP-END"
}

# The workload's own process, not the shells around it. pgrep -f reads each
# process's command line, which waits on that process's memory map.
prog_pid() {
    local q
    for q in $(timeout -s KILL 10 pgrep -f "$1"); do
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
            sleep "$AFTER"
            if [ -d "/proc/$p" ] && [ "$(awk '{ sub(/^.*\) /, ""); print $1 }' "/proc/$p/stat" 2>/dev/null)" != Z ]; then
                echo "HANGWATCH run $i pid $p still there $AFTER s after SIGKILL"
                echo "HANGDUMP-BEGIN pid=$p after SIGKILL"
                for t in "/proc/$p/task"/*; do task_dump "$t" ""; done
                blocked_tasks
                echo "HANGDUMP-END"
            fi
        else
            echo "HANGDUMP-BEGIN pid=none"
            blocked_tasks
            block_poke
            echo "HANGDUMP-END"
        fi
        timeout -s KILL 10 pkill -KILL -f "heavy-run.sh $WL" 2>/dev/null
        break
    done
    wait "$rp" 2>/dev/null
    echo "HANGWATCH run $i end $(date +%s) stalls=$hangs last: $(grep -a HEAVY_ "$OUT/app.log" 2>/dev/null | tail -n 1 | grep -ao 'HEAVY_.*')"
done
echo "HANGWATCH total runs=$RUNS stalls=$hangs"
