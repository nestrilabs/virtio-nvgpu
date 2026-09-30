#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A guest image for rig/rig-heavy.sh: a reflink copy of a built image with
# rig/heavy at /opt/heavy (and, optionally, another guest module, extra
# files, directories and nix closures), so the heavy workloads need no image
# rebuild.
#
# Usage: rig/heavy/mkimage-heavy.sh <base.ext4> <out.ext4> [--module KO]
#            [--file LOCAL:/abs/path/in/image[:MODE]]...
#            [--tree LOCALDIR:/abs/dir/in/image]...
#            [--closure STOREPATH[:/abs/link/in/image]]...
#            [--grow MIB]
#   --tree     a directory, copied whole (files, directories, symlinks)
#   --closure  a store path and everything it refers to, into /nix/store
#              (paths the image already has are skipped), and a symlink to
#              it (rig/heavy/extras.nix's goes to /opt/heavy/extras)
#   --grow     make the image that much larger first (a closure can be
#              gigabytes; the base has about 1 GiB free)
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# shellcheck source=rig/lib.sh
. "$HERE/../lib.sh"
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
# put_tree <localdir> <imagedir>: debugfs commands that make <imagedir> a
# copy of <localdir>, owned by root, modes kept. debugfs's write and mkdir
# name an entry in its working directory, so files are written directory by
# directory.
put_tree() {
    local src=$1 dst=$2
    (
        cd "$src"
        # Directories first (parents before children), then the rest.
        find . -mindepth 1 -type d -printf '%P\t%m\n' | LC_ALL=C sort | while IFS=$'\t' read -r p m; do
            printf 'mkdir "%s/%s"\nsif "%s/%s" mode 040%s\nsif "%s/%s" uid 0\nsif "%s/%s" gid 0\n' \
                "$dst" "$p" "$dst" "$p" "$m" "$dst" "$p" "$dst" "$p"
        done
        find . -mindepth 1 -type l -printf '%P\t%l\n' | while IFS=$'\t' read -r p l; do
            printf 'symlink "%s/%s" "%s"\n' "$dst" "$p" "$l"
        done
        find . -mindepth 1 -type f -printf '%h\t%f\t%m\n' | LC_ALL=C sort | awk -F'\t' -v dst="$dst" -v src="$src" '
            BEGIN { cur = "\001" }   # so the top directory is changed into too
            { d = ($1 == ".") ? "" : "/" substr($1, 3)
              if (d != cur) { printf "cd \"%s%s\"\n", dst, d; cur = d }
              printf "write \"%s%s/%s\" \"%s\"\n", src, d, $2, $2
              printf "sif \"%s\" mode 0100%s\nsif \"%s\" uid 0\nsif \"%s\" gid 0\n", $2, $3, $2, $2 }'
    ) >>"$CMDS"
}
{
    echo "mkdir /opt/heavy"
    echo "mkdir /opt/heavy/godot"
} >>"$CMDS"
put "$HERE/heavy-run.sh" /opt/heavy heavy-run.sh 755
put "$HERE/blender-eevee.py" /opt/heavy blender-eevee.py 644
put "$HERE/hang-watch.sh" /opt/heavy hang-watch.sh 755
put "$HERE/waits.py" /opt/heavy waits.py 644
for f in "$HERE"/godot/*; do put "$f" /opt/heavy/godot "$(basename "$f")" 644; done
GROW=0
CLOSURES=()
while [ $# -gt 0 ]; do
    case $1 in
        --module) put "$2" /opt/nvgpu nvgpu.ko 644; shift 2 ;;
        --file)
            IFS=: read -r src dst mode <<<"$2"
            put "$src" "$(dirname "$dst")" "$(basename "$dst")" "${mode:-755}"
            shift 2
            ;;
        --tree)
            IFS=: read -r src dst <<<"$2"
            # Its parents first, as mkdir -p would; from /, since debugfs
            # makes "/x" in its working directory.
            echo "cd /" >>"$CMDS"
            a=
            IFS=/ read -r -a parts <<<"${dst#/}"
            for c in "${parts[@]}"; do a+=/$c; echo "mkdir \"$a\"" >>"$CMDS"; done
            put_tree "$(cd "$src" && pwd)" "$dst"
            shift 2
            ;;
        --closure) CLOSURES+=("$2"); shift 2 ;;
        --grow) GROW=$2; shift 2 ;;
        *) echo "unknown option $1" >&2; exit 2 ;;
    esac
done
if [ "$GROW" -gt 0 ]; then
    truncate -s "+${GROW}M" "$OUT"
    "$E2/e2fsck" -f -y "$OUT" >/dev/null 2>&1 || [ $? -le 1 ]
    "$E2/resize2fs" "$OUT" >/dev/null
fi
if [ ${#CLOSURES[@]} -gt 0 ]; then
    have=$("$E2/debugfs" -R 'ls -p /nix/store' "$OUT" 2>/dev/null | awk -F/ '{ print $6 }')
    for c in "${CLOSURES[@]}"; do
        IFS=: read -r sp link <<<"$c"
        for p in $(nix path-info -r "$sp"); do
            grep -qxF "$(basename "$p")" <<<"$have" && continue
            ph=$(rig_phys "$p")
            if [ -d "$ph" ] && [ ! -L "$ph" ]; then
                echo "mkdir \"$p\"" >>"$CMDS"
                put_tree "$ph" "$p"
            elif [ -L "$ph" ]; then
                echo "symlink \"$p\" \"$(readlink "$ph")\"" >>"$CMDS"
            else
                put "$ph" /nix/store "$(basename "$p")" "$(stat -c %a "$ph")"
            fi
            have+=$'\n'$(basename "$p")
        done
        [ -n "$link" ] && printf 'rm "%s"\nsymlink "%s" "%s"\n' "$link" "$link" "$sp" >>"$CMDS"
    done
fi
"$E2/debugfs" -w -f "$CMDS" "$OUT" >"$OUT.debugfs.log" 2>&1
grep -v -e '^debugfs' -e 'File not found by ext2_lookup' -e '^Allocated inode' -e '^$' \
    -e 'already exists' "$OUT.debugfs.log" | head -n 20 >&2 || true
"$E2/debugfs" -R 'ls -l /opt/heavy' "$OUT" 2>/dev/null | sed 's/^/  /'
