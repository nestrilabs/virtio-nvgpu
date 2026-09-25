#!/usr/bin/env bash
# Diff a bare-metal capture against a guest capture (NVK_VERIFICATION.md §5.4a/b).
# The two runs differ legitimately only in address-like values, so before diffing
# we normalise those away:
#   - keep only AFTER IOCTL lines (the state after each call);
#   - rename each RM handle value (a hex after an `h<Name>:` field) to H0, H1, ...
#     by first appearance, so equal handles read equal and the numbering lines up
#     between the two sides that allocate in the same order;
#   - collapse remaining long hex (CPU pointers, pLinearAddress) to PTR;
#   - collapse fd numbers to FD, and drop timer/correlation values.
# What is left should be identical. A surviving difference -- a class or control
# added or missing or reordered, a different length or caching flag, a nonzero
# status on one side only -- is the finding.
#
# It also compares the pushbuffer sets by content hash (§5.4b): a signature on
# only one side is a divergence to decode with nv_push_dump.
#
# Usage: envy-diff.sh <bare-dir> <guest-dir>
#   the two <side>/<label> directories envy-capture.sh produced.
set -euo pipefail

if [ "$#" -ne 2 ]; then
    echo "usage: envy-diff.sh <bare-dir> <guest-dir>" >&2
    exit 2
fi
bare="$1"; guest="$2"
for d in "$bare" "$guest"; do
    [ -f "$d/rm.log" ] || { echo "no $d/rm.log (run envy-capture.sh first)" >&2; exit 2; }
done

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The normaliser. gawk keeps a per-file map of handle value -> Hn. It scrubs
# handles, pointer fields, fds and timers, but deliberately KEEPS control command
# ids (cmd:) and alloc class ids (hClass is renamed as a handle, so class stays in
# the ioctl name / cmd), which are exactly what a real divergence shows up in.
normalise() {
    gawk '
        BEGIN { n = 0 }
        /AFTER IOCTL/ {
            line = $0
            # Handle fields: h<Name>: 0xVALUE  ->  h<Name>: H<n>, stable per file.
            out = ""
            rest = line
            while (match(rest, /h[A-Za-z][A-Za-z0-9_]*:[ ]*0x[0-9a-fA-F]+/)) {
                pre = substr(rest, 1, RSTART - 1)
                tok = substr(rest, RSTART, RLENGTH)
                rest = substr(rest, RSTART + RLENGTH)
                split(tok, kv, ":")
                val = kv[2]; gsub(/[ ]/, "", val)
                if (!(val in seen)) { seen[val] = "H" n; n++ }
                out = out pre kv[1] ": " seen[val]
            }
            line = out rest
            # Timer / correlation values: blank them, whatever the width. Before
            # the pointer rule, so a *Timer field is not mistaken for a pointer.
            gsub(/[A-Za-z0-9_]*[Tt]imer[A-Za-z0-9_]*:[ ]*[-0-9a-fx]+/, "TIMER", line)
            gsub(/[A-Za-z0-9_]*[Cc]orrelation[A-Za-z0-9_]*:[ ]*[-0-9a-fx]+/, "CORR", line)
            # Pointer fields (p<Name>: 0x...) -> PTR, keeping the field name.
            gsub(/p[A-Za-z][A-Za-z0-9_]*:[ ]*0x[0-9a-fA-F]+/, "PTRFIELD", line)
            # fd numbers -> FD.
            gsub(/fd:[ ]*-?[0-9]+/, "fd: FD", line)
            # Any remaining long hex (>=9 digits) is a CPU address, not a 32-bit
            # handle or an 8-digit command/class id, so collapse it too.
            gsub(/0x[0-9a-fA-F]{9,}/, "PTR", line)
            print line
        }
    ' "$1"
}

normalise "$bare/rm.log"  > "$tmp/bare.seq"
normalise "$guest/rm.log" > "$tmp/guest.seq"

echo "== RM sequence diff (bare < , guest > ) =="
if diff -u "$tmp/bare.seq" "$tmp/guest.seq" > "$tmp/seq.diff"; then
    echo "  identical after normalisation"
else
    # Trim the header, show the body.
    tail -n +3 "$tmp/seq.diff"
    echo "  --- $(grep -c '^[<>]' "$tmp/seq.diff") differing line(s) ---"
    echo "  Expected: handle values, PTR, FD, times. A class/control/status"
    echo "  difference is a real divergence (NVK_VERIFICATION.md §5.4a)."
fi

echo
echo "== pushbuffer sets (content hash) =="
hashes() { (cd "$1" && find . -name 'pushbuf_*.bin' -o -name 'mme_*.bin' \
    | sort | xargs -r sha256sum | awk '{print $1}' | sort -u); }
hashes "$bare"  > "$tmp/bare.h"
hashes "$guest" > "$tmp/guest.h"
only_bare=$(comm -23 "$tmp/bare.h" "$tmp/guest.h" | wc -l)
only_guest=$(comm -13 "$tmp/bare.h" "$tmp/guest.h" | wc -l)
shared=$(comm -12 "$tmp/bare.h" "$tmp/guest.h" | wc -l)
echo "  $shared shared, $only_bare bare-only, $only_guest guest-only"
if [ "$only_bare" != 0 ] || [ "$only_guest" != 0 ]; then
    echo "  Decode the unmatched buffers with nv_push_dump (§5.2, §5.4b) and pair"
    echo "  by (subch, mthd); a signature on only one side, especially extra host"
    echo "  c56f SEM_EXECUTE/WFI in the guest, is a FAIL."
fi
