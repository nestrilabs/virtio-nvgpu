#!/usr/bin/env bash
# Capture one side of the bare-metal-vs-guest differential (NVK_VERIFICATION.md
# §5.4): run a workload under envyhooks, keeping the RM ioctl trace and the
# pushbuffer dumps. Run it once on bare metal and once in the guest, with the
# SAME driver version and the SAME binary, then compare with envy-diff.sh.
#
# envyhooks and the driver versions must match (§5.0). Build envyhooks against
# this repo's nvidia-driver tree and point $EHKS at the result:
#   export EHKS=/opt/ehks/libenvyhooks.so
#
# Usage: envy-capture.sh <bare|guest> <workload-label> [outdir] -- <cmd...>
#   e.g. envy-capture.sh guest W2 ~/ehks -- vkcube --wsi wayland --c 300
# Without an outdir, a fresh one (mktemp -d) is made and named: a fixed name
# in a shared /tmp is one another user can take first, as a symlink to
# somewhere of theirs.
#
# Leaves, under <outdir>/<side>/<workload>/:
#   rm.log            the AFTER/BEFORE IOCTL trace (envyhooks on stderr)
#   pushbuf_*.bin     dumped pushbuffers and MME macros
set -euo pipefail

if [ "$#" -lt 4 ]; then
    echo "usage: envy-capture.sh <bare|guest> <label> [outdir] -- <cmd...>" >&2
    exit 2
fi

side="$1"; shift
label="$1"; shift
outdir=
if [ "$1" != "--" ]; then outdir="$1"; shift; fi
[ "$1" = "--" ] || { echo "expected -- before the command" >&2; exit 2; }
shift
[ -n "$outdir" ] || outdir=$(mktemp -d "${TMPDIR:-/tmp}/ehks.XXXXXX")

case "$side" in bare|guest) ;; *) echo "side must be bare or guest" >&2; exit 2;; esac
: "${EHKS:?point EHKS at libenvyhooks.so built for this driver version}"
[ -e "$EHKS" ] || { echo "EHKS=$EHKS does not exist" >&2; exit 2; }

dir="$outdir/$side/$label"
rm -rf "$dir"
mkdir -p "$dir"

echo "envy-capture: $side/$label -> $dir"
echo "envy-capture: $*"

# EHKS_LOG_RM_IOCTL dumps every RM ioctl; PUSHBUF_DUMP_DIR the command streams.
# __GL_SHADER_DISK_CACHE=0 keeps a warm cache from hiding allocations. Keep the
# backend at RUST_LOG=warn so its own log is small; ptrace is NOT combined with
# this (both use SIGTRAP) -- run the strace pass separately, as the doc says.
LD_PRELOAD="$EHKS" \
    EHKS_LOG_RM_IOCTL=1 \
    EHKS_PUSHBUF_DUMP_DIR="$dir" \
    __GL_SHADER_DISK_CACHE=0 \
    "$@" 2>"$dir/rm.log" || echo "envy-capture: command exited $? (trace still saved)"

echo "envy-capture: $(grep -c 'AFTER IOCTL' "$dir/rm.log" 2>/dev/null || echo 0) AFTER-IOCTL lines,"\
     "$(find "$dir" -name 'pushbuf_*.bin' | wc -l) pushbuffer(s)"
