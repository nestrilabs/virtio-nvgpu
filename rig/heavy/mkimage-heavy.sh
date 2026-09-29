#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A guest image for rig/rig-heavy.sh: a reflink copy of a built image with
# rig/heavy at /opt/heavy (and, optionally, another guest module or extra
# files), so the heavy workloads need no image rebuild.
#
# Usage: rig/heavy/mkimage-heavy.sh <base.ext4> <out.ext4> [--module KO]
#            [--file LOCAL:/abs/path/in/image[:MODE]]...
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
BASE=${1:?base image} OUT=${2:?out image}
shift 2
export NIX_CONFIG=${NIX_CONFIG:-experimental-features = nix-command flakes}
E2=$(nix build --no-link --print-out-paths 'nixpkgs#e2fsprogs.bin')/bin
cp --reflink=auto "$BASE" "$OUT"
CMDS=$(mktemp)
trap 'rm -f "$CMDS"' EXIT
put() { # <local> <dir> <name> <mode>
    {
        echo "cd $2"
        echo "rm $3"
        echo "write $1 $3"
        echo "sif $3 uid 0"
        echo "sif $3 gid 0"
        printf "sif %s mode 0%o\n" "$3" $((8#100000 | 8#$4))
    } >>"$CMDS"
}
{
    echo "mkdir /opt/heavy"
    echo "mkdir /opt/heavy/godot"
} >>"$CMDS"
put "$HERE/heavy-run.sh" /opt/heavy heavy-run.sh 755
put "$HERE/blender-eevee.py" /opt/heavy blender-eevee.py 644
for f in "$HERE"/godot/*; do put "$f" /opt/heavy/godot "$(basename "$f")" 644; done
while [ $# -gt 0 ]; do
    case $1 in
        --module) put "$2" /opt/nvgpu nvgpu.ko 644; shift 2 ;;
        --file)
            IFS=: read -r src dst mode <<<"$2"
            put "$src" "$(dirname "$dst")" "$(basename "$dst")" "${mode:-755}"
            shift 2
            ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
"$E2/debugfs" -w -f "$CMDS" "$OUT" 2>&1 | grep -v -e '^debugfs' -e 'File not found by ext2_lookup' \
    -e '^Allocated inode' -e '^$' -e 'already exists' >&2 || true
"$E2/debugfs" -R 'ls -l /opt/heavy' "$OUT" 2>/dev/null | sed 's/^/  /'
