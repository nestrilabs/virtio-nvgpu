#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build the rig's host-side helpers into .rig/bin:
#   vptr               a persistent virtual pointer for the headless compositor
#   nvgpu-inject-test  the capture test's helper (GBM + EGL, against a
#                      backend's --inject-socket; run through inject-hook.sh)
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
    rm -rf "$t"

    # NVIDIA userspace is not linked: glvnd and the GBM loader find it at run
    # time (inject-hook.sh points them at the guest image'"'"'s copy).
    glvnd=$(nix build --no-link --print-out-paths nixpkgs#libglvnd.dev)
    glvndl=$(nix build --no-link --print-out-paths nixpkgs#libglvnd)
    gbm=$(nix build --no-link --print-out-paths nixpkgs#libgbm)
    gcc -O2 -g -Wall -Wextra -Wno-unused-parameter -I"$glvnd/include" -I"$gbm/include" \
        "'"$here"'/nvgpu-inject-test.c" -o "'"$out"'/nvgpu-inject-test" \
        -L"$glvndl/lib" -L"$gbm/lib" -Wl,-rpath,"$glvndl/lib:$gbm/lib" -lEGL -lGLESv2 -lgbm'
echo "built $out/vptr $out/nvgpu-inject-test"
