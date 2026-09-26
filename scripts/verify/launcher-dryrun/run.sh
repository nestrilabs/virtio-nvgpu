#!/usr/bin/env bash
# A dry run of scripts/run-guest.sh, root and unprivileged, with stub binaries:
# no KVM, no GPU, no root. The root half runs in a user namespace (one uid,
# mapped to 0), chrooted into a tmpfs holding a root-owned rig, and compares
# the launcher against an older one on two things a root run must not do --
# kill another VM's backend or VMM by pattern, and open a root-owned socket to
# the slot's group through a symlink the backend's user put where its socket
# should be -- then checks the root-only refusals (no layout named, a
# diagnostic switch without NVGPU_DIAGNOSTIC=1, a path someone else can
# write or does not belong to root, no jailer). The unprivileged half checks
# a rig run still gets to the VMM and still kills a stale backend of its own.
#
# Usage: scripts/verify/launcher-dryrun/run.sh [OLD_LAUNCHER]
#   OLD_LAUNCHER  a run-guest.sh to compare with (default: e513ba9's)
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
W=$(mktemp -d "${TMPDIR:-/tmp}/launcher-dryrun.XXXXXX")
trap 'rm -rf "$W"' EXIT
cp "$HERE"/{setup.sh,inside.sh,backend.sh,user.sh,stubvmm.c} "$W/"
if [ $# -ge 1 ]; then
    cp "$1" "$W/old.sh"
else
    git -C "$REPO" show e513ba9:scripts/run-guest.sh > "$W/old.sh"
fi
if command -v cc >/dev/null; then
    cc -O2 -o "$W/stubvmm" "$W/stubvmm.c"
else
    NIX_CONFIG="experimental-features = nix-command flakes" \
        nix shell nixpkgs#gcc -c gcc -O2 -o "$W/stubvmm" "$W/stubvmm.c"
fi
echo "### as root (in a user namespace)"
unshare --user --map-root-user --mount --pid --fork bash "$W/setup.sh" "$W/old.sh" "$REPO/scripts/run-guest.sh"
echo "### unprivileged"
bash "$W/user.sh" "$REPO/scripts/run-guest.sh"
