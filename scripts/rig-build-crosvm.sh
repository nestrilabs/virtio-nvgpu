#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Build .rig/bin/crosvm: crosvm with patches/crosvm applied (the virtio-nvgpu
# branch of .rig/src/crosvm), static like .rig/bin/nesbox, and without the
# default features (no virtio-gpu, virgl, virtio-wl, audio, usb or net):
# the guest's GPU is the vhost-user nvgpu device, and nothing else is needed.
#
# Usage: scripts/rig-build-crosvm.sh [extra cargo args]
#   NVGPU_RIG      the rig (default: the repo's .rig)
#   CROSVM_SRC     the crosvm checkout (default: $NVGPU_RIG/src/crosvm)
#   CROSVM_OUT     where the binary goes (default: $NVGPU_RIG/bin/crosvm)
#
# First time:
#   git clone https://chromium.googlesource.com/crosvm/crosvm .rig/src/crosvm
#   cd .rig/src/crosvm && git checkout -b virtio-nvgpu <base> &&
#     git am ../../../patches/crosvm/*.patch
# (the series was made against c0474109d64d, 2026-09-25).
#
# minijail is built from crosvm's submodule and linked in statically; its
# Makefile runs /bin/echo, which a NixOS host does not have, so the build
# rewrites those two lines in the submodule's working tree first.
set -euo pipefail
REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RIG=${NVGPU_RIG:-$REPO/.rig}
SRC=${CROSVM_SRC:-$RIG/src/crosvm}
OUT=${CROSVM_OUT:-$RIG/bin/crosvm}
[ -e "$SRC/.git" ] || { echo "no crosvm checkout at $SRC (see the top of $0)" >&2; exit 1; }

git -C "$SRC" submodule update --init --depth 1 third_party/minijail
sed -i 's#@/bin/echo -e#@printf "%s\\n"#' "$SRC/third_party/minijail/Makefile"
sed -i 's#^ECHO = /bin/echo -e#ECHO = printf "%s\\n"#' "$SRC/third_party/minijail/common.mk"

export NIX_CONFIG="experimental-features = nix-command flakes"
export CARGO_HOME=${CARGO_HOME:-$RIG/cache/cargo-home-crosvm}
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$RIG/target-crosvm}
cd "$SRC"
# Inside the shell, because the store paths exist only there (a sandboxed
# nix keeps its store elsewhere and mounts it for the shell).
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#gcc nixpkgs#pkg-config nixpkgs#protobuf \
    nixpkgs#python3 nixpkgs#gnumake nixpkgs#which nixpkgs#glibc.static \
    nixpkgs#libcap.dev nixpkgs#libcap.lib nixpkgs#pkgsStatic.libcap \
    nixpkgs#llvmPackages.libclang.lib -c bash -c '
    set -e
    first() { ls -d $1 2>/dev/null | head -n 1; }
    GS=$(dirname "$(first "/nix/store/*-glibc-*-static/lib/libc.a")")
    CAPA=$(dirname "$(first "/nix/store/*-libcap-*/lib/libcap.a")")
    CAPH=$(first "/nix/store/*-libcap-*-dev/include")
    export LIBCLANG_PATH=$(first "/nix/store/*-clang-*-lib/lib")
    export C_INCLUDE_PATH=$CAPH
    export LIBRARY_PATH=$(dirname "$(first "/nix/store/*-libcap-*-lib/lib/libcap.so")"):$CAPA
    export BINDGEN_EXTRA_CLANG_ARGS="-I$CAPH -I$(first "/nix/store/*-glibc-*-dev/include")"
    export RUSTFLAGS="-C target-feature=+crt-static -L $GS -L $CAPA"
    cargo build --release --target x86_64-unknown-linux-gnu --no-default-features "$@"
' bash "$@"
install -m 0755 "$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/crosvm" "$OUT"
echo "installed $OUT"
