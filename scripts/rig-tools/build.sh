#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build the rig's host-side helpers into .rig/bin (vptr: a persistent virtual
# pointer for the headless compositor).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=$(cd "${1:-$here/../../.rig/bin}" && pwd)
export NIX_CONFIG="experimental-features = nix-command flakes"
# A chroot store's paths are only visible inside nix's own namespace, so the
# lookups happen in there too.
nix shell nixpkgs#gcc nixpkgs#wayland-scanner nixpkgs#pkg-config -c bash -c '
    set -euo pipefail
    proto=$(nix build --no-link --print-out-paths nixpkgs#wlr-protocols)
    wl=$(nix build --no-link --print-out-paths nixpkgs#wayland.dev)
    export PKG_CONFIG_PATH=$wl/lib/pkgconfig
    x=$proto/share/wlr-protocols/unstable/wlr-virtual-pointer-unstable-v1.xml
    t=$(mktemp -d)
    wayland-scanner client-header "$x" "$t/wlr-virtual-pointer-unstable-v1-client-protocol.h"
    wayland-scanner private-code "$x" "$t/proto.c"
    gcc -O2 -Wall -Wextra -I"$t" "'"$here"'/vptr.c" "$t/proto.c" -o "'"$out"'/vptr" \
        $(pkg-config --cflags --libs wayland-client)
    rm -rf "$t"'
echo "built $out/vptr"
