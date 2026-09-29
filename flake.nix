# SPDX-License-Identifier: Apache-2.0
{
  description = "virtio-nvgpu: NVIDIA GPU access in KVM guests at the driver ABI level";

  # The nixpkgs the guest image and the rig are built with (rig/guest-image/flake.nix).
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/34ca302a9572963c02e385c056be37c85ff51b77";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = pkgs.lib;

      # The tracked tree, less what never goes into a build.
      src = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: _type:
          !(lib.elem (baseNameOf path) [
            "target"
            ".rig"
            "hyprland"
            "nvidia-driver"
            "audit"
            "result"
          ]);
      };

      cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };

      # One step of scripts/ci.sh's fast tier, in the build sandbox.
      rustCheck =
        name: script:
        pkgs.stdenv.mkDerivation {
          name = "virtio-nvgpu-${name}";
          inherit src cargoDeps;
          nativeBuildInputs = [
            pkgs.rustPlatform.cargoSetupHook
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.python3
          ];
          dontConfigure = true;
          # The difftest compiles the guest's C at -O0 with -Werror, where
          # the wrapper's _FORTIFY_SOURCE is a warning.
          hardeningDisable = [ "fortify" ];
          buildPhase = ''
            runHook preBuild
            export CARGO_TARGET_DIR=$PWD/target
            ${script}
            runHook postBuild
          '';
          installPhase = "touch $out";
        };

      backend = pkgs.rustPlatform.buildRustPackage {
        pname = "vhost-user-nvgpu";
        version = "0.1.0";
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        buildFeatures = [ "vhost-user" ];
        cargoBuildFlags = [
          "-p"
          "device"
          "--bin"
          "vhost-user-nvgpu"
          "--bin"
          "nvgpu-userspace"
        ];
        # The tests run in checks.test, with the whole workspace.
        doCheck = false;
        meta = {
          description = "vhost-user backend of the virtio-nvgpu device";
          license = lib.licenses.asl20;
          mainProgram = "vhost-user-nvgpu";
          platforms = [ system ];
        };
      };

      wlGuest = pkgs.rustPlatform.buildRustPackage {
        pname = "nvgpu-wl-guest";
        version = "0.1.0";
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [
          "-p"
          "nvgpu-wl-guest"
          "--bin"
          "nvgpu-wl-guest"
        ];
        doCheck = false;
        meta = {
          description = "Guest-side Wayland proxy daemon of virtio-nvgpu";
          license = lib.licenses.asl20;
          mainProgram = "nvgpu-wl-guest";
          platforms = [ system ];
        };
      };
    in
    {
      packages.${system} = {
        vhost-user-nvgpu = backend;
        nvgpu-wl-guest = wlGuest;
        # The backend's units and their helper, contrib/systemd's for this
        # backend (nix/units.nix; the NixOS module installs the same). The
        # VMM templates are not among them: they name a VMM this flake does
        # not build.
        units = pkgs.callPackage ./nix/units.nix { inherit backend; };
        default = backend;
      };

      # nix/module.nix, with this flake's backend as its default package.
      nixosModules.default =
        { lib, ... }:
        {
          imports = [ ./nix/module.nix ];
          services.virtio-nvgpu.package = lib.mkDefault self.packages.${system}.vhost-user-nvgpu;
        };

      # The Rust half of scripts/ci.sh's fast tier, and the NixOS module's
      # evaluation (the rest -- the patch checks, shell syntax, the units --
      # is ci.sh's own).
      checks.${system} = {
        fmt = rustCheck "fmt" ''
          cargo fmt --all -- --check
          cargo fmt --manifest-path fuzz/Cargo.toml --all -- --check
          cargo fmt --manifest-path driver/rust/fuzz/Cargo.toml --all -- --check
        '';
        clippy = rustCheck "clippy" ''
          cargo clippy --offline --workspace --all-targets --features device/vhost-user -- -D warnings
          cargo clippy --offline -p device --all-targets --features vhost-user,test-bins -- -D warnings
        '';
        test = rustCheck "test" ''
          cargo test --offline --workspace --features device/vhost-user
        '';
        unsafe = rustCheck "unsafe" ''
          patchShebangs scripts/check-unsafe.sh
          scripts/check-unsafe.sh
        '';
        inherit (self.packages.${system}) vhost-user-nvgpu nvgpu-wl-guest units;
        # nix/module.nix evaluated: its assertions refuse what they must, its
        # units are contrib/systemd's, and its drop-ins set only what is per
        # slot.
        module-eval = import ./nix/module-test.nix {
          inherit nixpkgs system;
          module = self.nixosModules.default;
        };
      };
    };
}
