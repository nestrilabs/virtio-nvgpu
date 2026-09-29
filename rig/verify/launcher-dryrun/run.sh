#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# A dry run of rig/run-guest.sh, root and unprivileged, with stub binaries:
# no KVM, no GPU, no root. The root half runs in a user namespace (one uid,
# mapped to 0), chrooted into a tmpfs holding a root-owned rig, and compares
# the launcher against an older one on two things a root run must not do --
# kill another VM's backend or VMM by pattern, and open a root-owned socket to
# the slot's group through a symlink the backend's user put where its socket
# should be -- then checks the root-only refusals (no layout named, a
# diagnostic switch without NVGPU_DIAGNOSTIC=1, a path someone else can
# write or does not belong to root -- the launcher's pieces, rig/launcher,
# included -- no jailer) and what the 2026-09-29 review
# asked of it (SECURITY.md §22): root binds the backend's socket and hands it
# over, and never gives the run's directory to the backend's user (H1); a
# diagnostic backend flag needs NVGPU_DIAGNOSTIC=1 and is said on the
# terminal (H3); two runs cannot share a tag (H7); a guest flooding its
# console fills at most NVGPU_LOG_MAX_MIB (H2); guest text reaches the
# terminal without control characters (H5); and root starts again with a
# clean environment, and puts no library that is not root's in the VMM's jail
# (H6). The unprivileged half checks a rig run still gets to the VMM, still
# kills a stale backend of its own, and says a diagnostic flag aloud.
#
# Usage: rig/verify/launcher-dryrun/run.sh [OLD_LAUNCHER]
#   OLD_LAUNCHER  a run-guest.sh to compare with (default: e513ba9's)
set -euo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
W=$(mktemp -d "${TMPDIR:-/tmp}/launcher-dryrun.XXXXXX")
trap 'rm -rf "$W"' EXIT
cp "$HERE"/{setup.sh,inside.sh,backend.sh,jailer.sh,user.sh,stubvmm.c} "$W/"
if [ $# -ge 1 ]; then
    cp "$1" "$W/old.sh"
else
    git -C "$REPO" show e513ba9:scripts/run-guest.sh > "$W/old.sh"
fi
CC=(cc)
command -v cc >/dev/null ||
    CC=(env NIX_CONFIG="experimental-features = nix-command flakes" nix shell nixpkgs#gcc -c gcc)
"${CC[@]}" -O2 -static -nostdlib -fno-stack-protector -o "$W/stubvmm" "$W/stubvmm.c"
"${CC[@]}" -O2 -DWITH_LIBC -o "$W/stubvmm-dyn" "$W/stubvmm.c"
echo "### as root (in a user namespace)"
unshare --user --map-root-user --mount --pid --fork bash "$W/setup.sh" "$W/old.sh" "$REPO/rig/run-guest.sh"
echo "### unprivileged"
bash "$W/user.sh" "$REPO/rig/run-guest.sh"
