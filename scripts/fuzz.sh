#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Fuzz every place the host parses what a guest (or a Wayland peer) sent, and
# run Miri over the unsafe-heavy unit tests. device/README.md, "Fuzzing".
#
# Usage: scripts/fuzz.sh <command> [args]
#   list                      the targets
#   build                     build every target (nightly, AddressSanitizer)
#   seeds                     record seeds from the unit tests into the
#                             backend corpora, and lay them out as control
#                             queues for the vring corpus
#   stats [TARGET...]         how far the backend corpora reach: host calls,
#                             IOCTL2, OS-descriptor registrations, placements
#   run [SECONDS] [TARGET...] run targets side by side (default: all, 600 s),
#                             each in a sandbox; FUZZ_SCALE multiplies the
#                             worker processes per target (default 1: 18 in all)
#   repro TARGET FILE         replay one input, symbolised
#   triage TARGET             replay every artifact of TARGET, one line each,
#                             counted by cause
#   miri                      Miri over the unit tests of device and wlwire,
#                             one per process (see miri() below for how each
#                             result is counted)
#
# Corpora are fuzz/corpus/<target>, findings fuzz/artifacts/<target>, logs
# fuzz/artifacts/<target>.log; none of them is checked in. A finding that
# is fixed becomes a unit test beside the code it was in.
#
# The toolchain is a pinned nightly (fenix) with cargo-fuzz, bubblewrap and
# llvm-symbolizer from nixpkgs; the script re-runs itself inside it.
#
# Every target runs in a bubblewrap sandbox with a /dev of its own: no GPU,
# no /dev/dri, no udmabuf, no network. The harness (device/src/fuzzing)
# replaces every host call with a fake kernel and every device path with
# /dev/null, and refuses to start where /dev/nvidiactl exists; the sandbox is
# the second lock on the same door. Never run the fuzzers on the GPU rig
# outside it.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
FUZZ="$ROOT/fuzz"
TARGETS=(backend backend_v2 vring deepseg osdesc rmshare nvkms guestptr misc wl_engine wl_codec inject)
# Worker processes per target: the stateful targets are slow and deep.
declare -A WEIGHT=([backend]=4 [backend_v2]=4 [vring]=2 [wl_engine]=3)

FENIX=github:nix-community/fenix/529de6f9a0c6721ae2a69d5719bbb803390e7b86
ENV_EXPR='
let
  fenix = (builtins.getFlake "'"$FENIX"'").packages.x86_64-linux;
  pkgs = (builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux;
in pkgs.buildEnv {
  name = "nvgpu-fuzz-env";
  paths = [
    (fenix.latest.withComponents [ "cargo" "rustc" "rust-src" "miri" "llvm-tools" ])
    pkgs.cargo-fuzz pkgs.bubblewrap pkgs.llvm pkgs.gcc pkgs.python3
  ];
  ignoreCollisions = true;
}'

if [[ -z "${NVGPU_FUZZ_ENV:-}" ]]; then
    export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
    exec nix shell --impure --expr "$ENV_EXPR" -c env NVGPU_FUZZ_ENV=1 "$0" "$@"
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$FUZZ/target}"
BIN="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release"
# libFuzzer is C++: the sandboxed binary needs to find libstdc++.
LIBSTDCXX=$(dirname "$(g++ -print-file-name=libstdc++.so.6)")

sandbox() {
    local t=$1
    shift
    mkdir -p "$FUZZ/corpus/$t" "$FUZZ/artifacts/$t"
    bwrap --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --bind "$FUZZ/corpus" "$FUZZ/corpus" --bind "$FUZZ/artifacts" "$FUZZ/artifacts" \
        --unshare-all --die-with-parent \
        --setenv LD_LIBRARY_PATH "$LIBSTDCXX" \
        --setenv ASAN_SYMBOLIZER_PATH "$(command -v llvm-symbolizer)" \
        --setenv RUST_BACKTRACE 1 \
        ${NVGPU_FUZZ_STATS:+--setenv NVGPU_FUZZ_STATS 1} \
        "$BIN/$t" "$@"
}

build() {
    (cd "$FUZZ" && cargo fuzz build -O)
}

seeds() {
    local tmp="$FUZZ/artifacts/.seeds"
    rm -rf "$tmp"
    mkdir -p "$tmp/recorded" "$tmp/remapped" "$FUZZ/corpus/vring"
    (cd "$ROOT" && NVGPU_FUZZ_SEEDS="$tmp/recorded" CARGO_TARGET_DIR="$ROOT/target" \
        cargo test -q -p device --features vhost-user --lib >/dev/null)
    echo "$(ls "$tmp/recorded" | wc -l) sessions recorded from the unit tests"
    # A test opens its own files, so its handle 1 is whatever it opened first;
    # the harness has one file of every kind open, at 1..12
    # (fuzzing/backend.rs, KINDS). Each session again with its handle 1 as
    # each of those, so the UVM file's calls reach the UVM file and so on.
    # And the same messages as a guest's control queue (fuzzing/vring.rs): a
    # configuration byte, the queue size (2^8), then guest memory from 0 --
    # descriptor table, available ring at 0x2000, each request from 0x4000
    # with a 4 KiB writable buffer after it.
    python3 - "$tmp/recorded" "$tmp/remapped" "$FUZZ/corpus/vring" <<'EOF'
import glob, os, struct, sys
src, remapped, vring = sys.argv[1:]
nv = 0
for f in sorted(glob.glob(src + "/*")):
    d = bytearray(open(f, "rb").read())
    at, i = [], 17
    while i + 2 <= len(d):
        (l,) = struct.unpack_from("<H", d, i)
        at.append((i + 2, l))
        i += 4 + l
    for k in range(1, 13):
        v = bytearray(d)
        for a, _ in at:
            if a + 8 <= len(v) and struct.unpack_from("<I", v, a + 4)[0] == 1:
                struct.pack_into("<I", v, a + 4, k)
        open(os.path.join(remapped, "%d-%s" % (k, os.path.basename(f))), "wb").write(v)
    msgs = [bytes(d[a : a + l]) for a, l in at if l <= 0x1800][:4]
    if not msgs:
        continue
    img, heads, p = bytearray(0x10000), [], 0x4000
    for k, m in enumerate(msgs):
        struct.pack_into("<QIHH", img, 32 * k, p, len(m), 1, 2 * k + 1)
        img[p : p + len(m)] = m
        p += (len(m) + 15) & ~15
        struct.pack_into("<QIHH", img, 32 * k + 16, 0x8000 + k * 0x1000, 0x1000, 2, 0)
        heads.append(2 * k)
        if p > 0x7F00:
            break
    struct.pack_into("<HH", img, 0x2000, 0, len(heads))
    for k, h in enumerate(heads):
        struct.pack_into("<H", img, 0x2004 + 2 * k, h)
    end = max(p, 0x2004 + 2 * len(heads))
    open(os.path.join(vring, "seed-%d" % nv), "wb").write(bytes([d[0] & 0x3F, 8]) + bytes(img[:end]))
    nv += 1
print("%d control-queue seeds" % nv)
EOF
    for t in backend backend_v2; do
        sandbox "$t" -merge=1 "$FUZZ/corpus/$t" "$tmp/remapped" 2>&1 | grep 'MERGE-OUTER: .* new files' || true
    done
    rm -rf "$tmp"
}

# How far the backend corpora get (NVGPU_FUZZ_STATS, fuzzing/host.rs
# `report`): of the inputs, how many reach the host at all, IOCTL2, a
# checked OS-descriptor registration, a window or a UVM placement.
stats() {
    local targets=("$@")
    [[ ${#targets[@]} -eq 0 ]] && targets=(backend backend_v2 vring)
    for t in "${targets[@]}"; do
        NVGPU_FUZZ_STATS=1 sandbox "$t" "$FUZZ/corpus/$t" -runs=0 2>&1 |
            awk -v t="$t" '/^NVGPU_FUZZ_STATS/ {
                for (i = 2; i <= NF; i++) { split($i, a, "="); if (a[2] > 0) n[a[1]]++ }
                runs++
            } END {
                printf "%-11s %d runs:", t, runs
                for (k in n) printf " %s %d", k, n[k]
                print ""
            }'
    done
}

run() {
    local secs=${1:-600}
    shift || true
    local targets=("$@")
    [[ ${#targets[@]} -eq 0 ]] && targets=("${TARGETS[@]}")
    local scale=${FUZZ_SCALE:-1}
    for t in "${targets[@]}"; do
        local jobs=$(( ${WEIGHT[$t]:-1} * scale ))
        mkdir -p "$FUZZ/artifacts/$t"
        sandbox "$t" "$FUZZ/corpus/$t" -artifact_prefix="$FUZZ/artifacts/$t/" \
            -max_total_time="$secs" -rss_limit_mb=3072 -timeout=20 -fork="$jobs" \
            -ignore_crashes=1 -ignore_timeouts=1 -ignore_ooms=1 \
            >"$FUZZ/artifacts/$t.log" 2>&1 &
    done
    wait
    for t in "${targets[@]}"; do
        printf '%-11s %s findings  %s\n' "$t" "$(find "$FUZZ/artifacts/$t" -type f | wc -l)" \
            "$(grep -E '^#[0-9]+' "$FUZZ/artifacts/$t.log" | tail -1 | cut -c1-90)"
    done
}

repro() {
    sandbox "$1" "$2"
}

triage() {
    local t=$1
    for f in "$FUZZ/artifacts/$t"/*; do
        [[ -f $f ]] || continue
        local out why
        out=$(sandbox "$t" "$f" 2>&1 || true)
        why=$(grep -A1 'panicked at' <<<"$out" | tail -1 || true)
        [[ -z $why ]] && why=$(grep -m1 -E 'SUMMARY|ERROR:' <<<"$out" || true)
        [[ -z $why ]] && why='(no crash on replay)'
        printf '%s\t%s\n' "$why" "$(basename "$f")"
    done | tee "$FUZZ/artifacts/$t.triage" | cut -f1 | sed 's/0x[0-9a-f]*/X/g' | sort | uniq -c | sort -rn
}

# Miri, one test per process: Miri stops a whole run at the first system
# call it cannot model (memfd_create, file-backed mmap, Unix sockets, the
# NVIDIA ioctls), and most of the backend's tests make one. Each test is
# PASS, UNSUPPORTED (what Miri cannot model; not a finding), IGNORED (a
# cfg_attr(miri, ignore) says why), TIMEOUT, or FAIL -- Undefined Behavior or
# a panic, which fails the command. MIRI_JOBS in parallel (default 6), each
# at most MIRI_TIMEOUT seconds (default 600).
miri() {
    cd "$ROOT"
    export MIRIFLAGS="${MIRIFLAGS:--Zmiri-disable-isolation}"
    export CARGO_TARGET_DIR="$ROOT/target/miri-run"
    local out="$ROOT/target/miri-run/results.tsv"
    mkdir -p "$(dirname "$out")"
    : >"$out"
    for pkg in "-p wlwire" "-p device --features vhost-user"; do
        # shellcheck disable=SC2086
        cargo miri test $pkg --lib -- --list 2>/dev/null | sed -n 's/: test$//p' |
            xargs -P "${MIRI_JOBS:-6}" -I{} sh -c '
                o=$(timeout "${MIRI_TIMEOUT:-600}" cargo miri test '"$pkg"' --lib -- --exact "$1" 2>&1)
                rc=$?
                why=$(printf "%s\n" "$o" | grep -m1 -oE "(unsupported operation: .*|Undefined Behavior: .*|panicked at .*)" | cut -c1-160)
                if [ $rc = 124 ]; then st=TIMEOUT
                elif printf "%s" "$o" | grep -q "1 passed"; then st=PASS
                elif printf "%s" "$o" | grep -q "1 ignored"; then st=IGNORED
                elif printf "%s" "$why" | grep -q "^unsupported"; then st=UNSUPPORTED
                else st=FAIL; fi
                printf "%s\t%s\t%s\n" "$st" "$1" "$why"
            ' _ {} >>"$out"
    done
    cut -f1 "$out" | sort | uniq -c
    if grep -q '^FAIL' "$out"; then
        grep '^FAIL' "$out"
        return 1
    fi
}

cmd=${1:-}
shift || true
case $cmd in
    list) printf '%s\n' "${TARGETS[@]}" ;;
    build) build ;;
    seeds) build && seeds ;;
    stats) stats "$@" ;;
    run) build && run "$@" ;;
    repro) repro "$@" ;;
    triage) triage "$@" ;;
    miri) miri ;;
    *) sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
