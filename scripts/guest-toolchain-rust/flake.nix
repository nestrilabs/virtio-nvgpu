# SPDX-License-Identifier: Apache-2.0
{
  # The guest kernel's toolchain with Rust: the pinned C toolchain the rig's
  # guest kernel is built with (.rig/kernel/toolchain, the same nixpkgs), plus
  # the rustc, rust-src and bindgen that CONFIG_RUST needs. Linux 7.2 asks for
  # rustc >= 1.85 and bindgen >= 0.71.1 (scripts/min-tool-version.sh); this
  # nixpkgs has rustc 1.98.1 and bindgen 0.72.1. The kernel and every Rust
  # object linked into a module must come from one rustc: bump the rev on
  # purpose, then rebuild clean.
  description = "virtio-nvgpu guest kernel toolchain, with Rust";
  inputs.nixpkgs.url = "https://releases.nixos.org/nixpkgs/nixpkgs-26.11pre1078969.34ca302a9572/nixexprs.tar.zst";
  outputs = { self, nixpkgs }:
    let pkgs = nixpkgs.legacyPackages.x86_64-linux;
    in {
      devShells.x86_64-linux.default = pkgs.mkShell {
        hardeningDisable = [ "all" ];
        nativeBuildInputs = with pkgs; [
          gnumake gcc binutils flex bison bc perl python3 pkg-config
          kmod cpio zstd xz rsync util-linux which
          rustc rust-bindgen rustfmt clippy
        ];
        buildInputs = with pkgs; [ elfutils openssl ncurses ];
        # The kernel builds `core` itself, from the toolchain's own sources.
        RUST_LIB_SRC = "${pkgs.rustPlatform.rustLibSrc}";
      };
    };
}
