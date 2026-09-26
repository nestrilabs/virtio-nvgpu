#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build the two C helpers (sec-negative, lease-flip) into scripts/verify/bin/.
#
# Run it wherever the binary will run: on the dev box it finds a toolchain
# through nix; inside the guest it uses the guest's own cc so the binary matches
# the guest glibc. The tests link only libc and libdrm, nothing NVIDIA, so a
# stock toolchain is enough.
#
# Usage: build.sh [outdir]   (default: the directory this script lives in, /bin)
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/verify/common.sh
. "$here/common.sh"

out="${1:-$here/bin}"
mkdir -p "$out"

cc="$(verify_cc)"
mapfile -t flags < <(verify_libdrm_flags)
cflags="${flags[0]}"
libs="${flags[1]}"

echo "verify: cc=$cc"
echo "verify: libdrm cflags=$cflags"

# Word-splitting on $cflags/$libs is intended: they are compiler flags.
# shellcheck disable=SC2086
"$cc" -O2 -Wall -Wextra $cflags "$here/sec-negative.c" -o "$out/sec-negative" $libs
# shellcheck disable=SC2086
"$cc" -O2 -Wall -Wextra $cflags "$here/lease-flip.c" -o "$out/lease-flip" $libs

echo "verify: built $out/sec-negative and $out/lease-flip"
