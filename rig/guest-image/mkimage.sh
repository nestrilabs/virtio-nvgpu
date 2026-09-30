#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build the guest root filesystem image: .rig/guest/rootfs.ext4.
#
# Usage: rig/guest-image/mkimage.sh [options]
#   (none)            nix build the guest root, stage it with its closure, and
#                     make the ext4 image (a few minutes cold, well under one
#                     warm)
#   --module-only     put .rig/kernel/nvgpu.ko (or --module PATH) into the
#                     existing image in place, with debugfs; seconds
#   --probes-only     copy rig/guest-image/probes/*.sh into the existing image's
#                     /opt/nvgpu in place (for iterating on a probe; the next
#                     full build puts the nix-built ones back, identical)
#   --module PATH     the guest module to install (default .rig/kernel/nvgpu.ko)
#   --out PATH        the image (default .rig/guest/rootfs.ext4)
#   NVGPU_RIG=DIR     the rig directory (default the repo's .rig), as for
#                     rig/run-guest.sh
#   --headroom MIB    free space in the image beyond its contents (default 1024)
#   --keep-staging    leave .rig/guest/staging after a full build
#
# What it does, in order (full build):
#   1. stages this directory, plus a filtered copy of the repo's workspace and
#      rig/verify, as .rig/guest/flake/ (a flake can only see its own tree;
#      see flake.nix), and `nix build`s its guestRoot, with the out-link at
#      .rig/guest/result;
#   2. copies guestRoot (the root overlay: /bin, /etc, /opt/nvgpu, ...) and
#      every store path of its closure, from where the store physically keeps
#      them, into .rig/guest/staging;
#   3. adds /opt/nvgpu/nvgpu.ko if the module exists (warns and goes on if not);
#   4. makes the ext4 image from the staging tree with mkfs.ext4 -d, inside a
#      user namespace where the caller is uid 0, so every file in the image is
#      owned by root without fakeroot or sudo.
#
# The image is the golden one: boot a copy of it, never it (the launcher
# copies it per run).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
RIG="${NVGPU_RIG:-$REPO/.rig}"
G="$RIG/guest"
LOGDIR="$RIG/logs/guest-image"

MODE=full
MODULE="$RIG/kernel/nvgpu.ko"
OUT="$G/rootfs.ext4"
HEADROOM_MIB=1024
KEEP_STAGING=0
while [ $# -gt 0 ]; do
    case $1 in
        --module-only) MODE=module; shift ;;
        --probes-only) MODE=probes; shift ;;
        --module) MODULE=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --headroom) HEADROOM_MIB=$2; shift 2 ;;
        --keep-staging) KEEP_STAGING=1; shift ;;
        -h | --help) sed -n '2,33p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "mkimage: unknown option $1" >&2; exit 2 ;;
    esac
done

mkdir -p "$G" "$LOGDIR"
export NIX_CONFIG="${NIX_CONFIG:-experimental-features = nix-command flakes}"
log() { printf 'mkimage: %s\n' "$*" >&2; }

# Where the store keeps a path's bytes: with a chroot store (nix without root),
# under ~/.local/share/nix/root; otherwise /nix/store itself.
# NIX_STORE_PHYS overrides (the directory that holds nix/store).
phys() {
    local p=$1 base
    for base in ${NIX_STORE_PHYS:+"$NIX_STORE_PHYS"} "$HOME/.local/share/nix/root" ""; do
        if [ -e "$base$p" ] || [ -L "$base$p" ]; then echo "$base$p"; return; fi
    done
    echo "mkimage: store path $p is nowhere on disk" >&2
    return 1
}

# debugfs from e2fsprogs; the host's if it has one, else nix's.
E2BIN=
e2init() {
    [ -n "$E2BIN" ] && return
    if command -v debugfs >/dev/null 2>&1 && command -v mkfs.ext4 >/dev/null 2>&1; then
        E2BIN=$(dirname "$(command -v debugfs)")
    else
        E2BIN=$(nix build --no-link --print-out-paths 'nixpkgs#e2fsprogs.bin')/bin
    fi
}
e2tool() { e2init; "$E2BIN/$1" "${@:2}"; }

# Replace one file in the image, owned by root, with the given mode.
put_file() { # <local file> <dir in image> <name> <mode>
    local cmds
    cmds=$(mktemp)
    {
        echo "cd $2"
        echo "rm $3"
        echo "write $1 $3"
        echo "sif $3 uid 0"
        echo "sif $3 gid 0"
        printf "sif %s mode 0%o\n" "$3" $((8#100000 | 8#$4))
    } > "$cmds"
    # debugfs prints the "rm" failure for a file that was not there; harmless.
    e2tool debugfs -w -f "$cmds" "$OUT" 2>&1 | grep -v -e '^debugfs' -e 'File not found by ext2_lookup' -e '^Allocated inode' -e '^$' >&2 || true
    rm -f "$cmds"
}

case $MODE in
    module)
        [ -f "$OUT" ] || { log "no image at $OUT; run a full build first"; exit 1; }
        [ -f "$MODULE" ] || { log "no module at $MODULE"; exit 1; }
        put_file "$MODULE" /opt/nvgpu nvgpu.ko 0644
        log "installed $MODULE as /opt/nvgpu/nvgpu.ko in $OUT"
        e2tool debugfs -R 'ls -l /opt/nvgpu' "$OUT" 2>/dev/null | grep ' nvgpu.ko' | sed 's/^/  /' >&2
        exit 0
        ;;
    probes)
        [ -f "$OUT" ] || { log "no image at $OUT; run a full build first"; exit 1; }
        for p in "$HERE"/probes/*.sh; do
            case $(basename "$p") in
                *-common.sh | env.sh) m=0644 ;;
                *) m=0755 ;;
            esac
            put_file "$p" /opt/nvgpu "$(basename "$p")" "$m"
        done
        log "copied rig/guest-image/probes/*.sh into $OUT:/opt/nvgpu"
        exit 0
        ;;
esac

# ---- 1. the nix build ----------------------------------------------------------
FLAKE="$G/flake"
log "staging the flake at $FLAKE"
if [ -d "$FLAKE" ]; then chmod -R u+w "$FLAKE"; rm -rf "$FLAKE"; fi
mkdir -p "$FLAKE/nvgpu-src/rig/verify"
cp -a "$HERE/flake.nix" "$HERE/nix" "$HERE/tools" "$HERE/probes" "$HERE/apps" "$FLAKE/"
[ -f "$HERE/flake.lock" ] && cp -a "$HERE/flake.lock" "$FLAKE/"
# The workspace, as cargo needs to see it: every member, never a target/.
cp -a "$REPO/Cargo.toml" "$REPO/Cargo.lock" "$FLAKE/nvgpu-src/"
members=$(sed -n '/^members/,/\]/p' "$REPO/Cargo.toml" | grep -o '"[^"]*"' | tr -d '"')
for m in $members; do
    tar -C "$REPO" --exclude=target --exclude='*.rs.bk' -cf - "$m" | tar -C "$FLAKE/nvgpu-src" -xf -
done
cp -a "$REPO"/rig/verify/*.sh "$REPO"/rig/verify/*.c "$FLAKE/nvgpu-src/rig/verify/"
cp -a "$REPO/nvgpu-vk-layer" "$FLAKE/nvgpu-src/"

log "nix build (log: $LOGDIR/build.log)"
if ! nix build "path:$FLAKE#guestRoot" --out-link "$G/result" --print-build-logs \
    > "$LOGDIR/build.log" 2>&1; then
    tail -n 40 "$LOGDIR/build.log" >&2
    log "nix build failed; full log in $LOGDIR/build.log"
    exit 1
fi
# The first build writes the lock; keep it with the sources.
if [ -f "$FLAKE/flake.lock" ] && ! cmp -s "$FLAKE/flake.lock" "$HERE/flake.lock" 2>/dev/null; then
    cp "$FLAKE/flake.lock" "$HERE/flake.lock"
    log "wrote rig/guest-image/flake.lock"
fi
ROOT=$(readlink -f "$G/result")
log "guestRoot: $ROOT"
# RM refuses a client of another release: the guest's userspace must be the
# host module's. Say so now rather than as a failed first ioctl in the guest.
IMGVER=$(sed -n 's/^nvidia-userspace \([^ ]*\).*/\1/p' "$(phys "$ROOT")/etc/nvgpu/manifest" 2>/dev/null || true)
if [ -r /proc/driver/nvidia/version ]; then
    if grep -q " $IMGVER " /proc/driver/nvidia/version; then
        log "NVIDIA userspace $IMGVER matches this host's kernel module"
    else
        log "WARNING: image userspace is $IMGVER but this host runs: $(head -n 1 /proc/driver/nvidia/version)"
    fi
fi

# ---- 2. stage the tree ----------------------------------------------------------
ST="$G/staging"
if [ -d "$ST" ]; then chmod -R u+w "$ST"; rm -rf "$ST"; fi
mkdir -p "$ST/nix/store" "$ST/nix/var/nix"
log "staging the root overlay and its closure in $ST"
cp -a --no-preserve=ownership "$(phys "$ROOT")/." "$ST/"
chmod -R u+w "$ST"
mapfile -t closure < <(nix path-info --recursive "$ROOT")
for p in "${closure[@]}"; do
    [ "$p" = "$ROOT" ] && continue
    cp -a --reflink=auto --no-preserve=ownership "$(phys "$p")" "$ST/nix/store/"
done
log "closure: ${#closure[@]} store paths"
# The overlay itself too, so its own symlinks (and the manifest's path) resolve.
cp -a --reflink=auto --no-preserve=ownership "$(phys "$ROOT")" "$ST/nix/store/"

# ---- 3. the module ----------------------------------------------------------------
if [ -f "$MODULE" ]; then
    install -m 0644 "$MODULE" "$ST/opt/nvgpu/nvgpu.ko"
    log "module: $MODULE"
else
    log "WARNING: no module at $MODULE; the image has no /opt/nvgpu/nvgpu.ko."
    log "         Add it later with: rig/guest-image/mkimage.sh --module-only"
fi

# Directories the running guest writes to must be writable (store paths are
# 0555, which is right for /nix/store and wrong for /tmp).
chmod 1777 "$ST/tmp" "$ST/var/tmp"
chmod 0700 "$ST/root"

# ---- 4. the image -------------------------------------------------------------------
bytes=$(du -sb "$ST" | cut -f1)
files=$(find "$ST" | wc -l)
size_mib=$(( (bytes + 1048575) / 1048576 * 11 / 10 + HEADROOM_MIB ))
inodes=$(( files * 3 / 2 + 65536 ))
log "contents $((bytes / 1048576)) MiB in $files files; image ${size_mib} MiB, $inodes inodes"

TMPIMG="$OUT.tmp"
rm -f "$TMPIMG"
# In a user namespace where we are root, the staging files (ours) read as
# root's, so mkfs.ext4 -d records uid/gid 0 for every one of them.
MKFS=(mkfs.ext4 -q -F -L nvgpu-root -N "$inodes" -E root_owner=0:0 -d "$ST" "$TMPIMG" "${size_mib}M")
e2init
if unshare -r true 2>/dev/null; then
    unshare -r "$E2BIN/${MKFS[0]}" "${MKFS[@]:1}"
else
    log "no user namespaces; using fakeroot for root ownership"
    nix shell nixpkgs#fakeroot nixpkgs#e2fsprogs -c fakeroot "${MKFS[@]}"
fi
mv -f "$TMPIMG" "$OUT"
e2tool e2fsck -fn "$OUT" > "$LOGDIR/e2fsck.log" 2>&1 || { log "e2fsck found problems: $LOGDIR/e2fsck.log"; exit 1; }

if [ "$KEEP_STAGING" = 0 ]; then chmod -R u+w "$ST"; rm -rf "$ST"; fi
log "wrote $OUT ($(du -h --apparent-size "$OUT" | cut -f1) apparent, $(du -h "$OUT" | cut -f1) on disk)"
e2tool debugfs -R 'ls -l /opt/nvgpu' "$OUT" 2>/dev/null | sed 's/^/  /' >&2 || true
