#!/usr/bin/env bash
# How many guests one card carries: N guests at once, each running a probe
# (the encode probe by default), for each N given. Runs on the GPU host;
# called by `rig.sh density`.
#
#   box-density.sh <probe> <N> [<N>...]
#
# Each guest gets its own copy of $GPU_ROOTFS, its own backend and its own
# log tag ($TAG-d<N>-g<i>), through box-run.sh. While they run, the card and
# the host are sampled once a second into $GPU_LOGS/$TAG-d<N>.samples.csv.
#
# Per N it prints one line per guest and one for the card:
#   g<i>  frames=<received>  decode=<PASS|FAIL>  dropped=<capture drops>
#   card  peak vram, peak encoder %, peak encoder sessions, peak host load
# and stops going up once every guest of a step failed.
set -euo pipefail

PROBE=${1:?usage: box-density.sh <probe> <N>...}
shift
[ $# -gt 0 ] || { echo "usage: box-density.sh <probe> <N>..." >&2; exit 2; }
: "${GPU_ROOTFS:?}" "${GPU_LOGS:?}" "${TAG:?}"
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$GPU_LOGS"
copies=$(dirname "$GPU_ROOTFS")/density-$TAG
mkdir -p "$copies"
trap 'rm -rf "$copies"' EXIT

for n in "$@"; do
    step="$TAG-d$n"
    samples="$GPU_LOGS/$step.samples.csv"
    echo "== $n guest(s), $PROBE"
    for i in $(seq 1 "$n"); do
        cp --sparse=always "$GPU_ROOTFS" "$copies/g$i.ext4"
    done

    echo "t,vram_mib,enc_pct,enc_sessions,gpu_pct,load1" > "$samples"
    (
        t=0
        while :; do
            g=$(nvidia-smi --query-gpu=memory.used,utilization.encoder,encoder.stats.sessionCount,utilization.gpu \
                --format=csv,noheader,nounits 2>/dev/null | tr -d ' ')
            echo "$t,$g,$(cut -d' ' -f1 /proc/loadavg)" >> "$samples"
            t=$((t + 1))
            sleep 1
        done
    ) &
    sampler=$!

    pids=()
    for i in $(seq 1 "$n"); do
        GPU_ROOTFS="$copies/g$i.ext4" bash "$HERE/box-run.sh" "$PROBE" "$step-g$i" \
            > "$GPU_LOGS/$step-g$i.summary" 2>&1 &
        pids+=($!)
    done
    for p in "${pids[@]}"; do wait "$p" || true; done
    kill "$sampler" 2>/dev/null || true
    wait "$sampler" 2>/dev/null || true

    ok=0
    for i in $(seq 1 "$n"); do
        s="$GPU_LOGS/$step-g$i.summary"
        c="$GPU_LOGS/$step-g$i.console.log"
        frames=$(grep -aoE 'frames=[0-9]+' "$s" | head -1 | cut -d= -f2 || true)
        dec=$(grep -aq 'PASS   stream decodes' "$s" && echo PASS || echo FAIL)
        drop=$(grep -aoE 'dropped [0-9]+' "$c" 2>/dev/null | awk '{s+=$2} END {print s+0}' || true)
        err=$(grep -aiE 'out of memory|OUT_OF_MEMORY|NV_ERR_INSUFFICIENT|no encoder|encode session|refus|panic' "$c" 2>/dev/null |
            grep -vE 'stats socket|isolation' | head -1 | cut -c1-100 || true)
        printf '  g%-3s frames=%-5s decode=%s dropped=%s %s\n' "$i" "${frames:-0}" "$dec" "$drop" "${err:+ first error: $err}"
        [ "$dec" = PASS ] && ok=$((ok + 1))
    done
    awk -F, 'NR > 1 {
        if ($2 > v) v = $2; if ($3 > e) e = $3; if ($4 > s) s = $4; if ($5 > g) g = $5; if ($6 > l) l = $6
    } END {
        printf "  card peak: vram %d MiB, encoder %d%%, sessions %d, gpu %d%%, host load %.1f\n", v, e, s, g, l
    }' "$samples"
    echo "  $ok of $n decoded; samples: $samples"
    rm -f "$copies"/g*.ext4
    [ "$ok" -gt 0 ] || { echo "== every guest failed at $n; stopping"; break; }
done
