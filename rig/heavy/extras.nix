# SPDX-License-Identifier: Apache-2.0
# What rig/heavy's Proton-like and CPU-heavy workloads need beyond the guest
# image, from the image's own nixpkgs (rig/guest-image/flake.lock), so the
# same store paths run natively and in a guest: Wine (staging, WoW64) with
# DXVK, 0 A.D., Xonotic, stress-ng, nvgpu-gameloop (gameloop.c),
# nvgpu-rtprobe (rtprobe.c) and nvgpu-wakecost (wakecost.c).
# rig/heavy/mkimage-heavy.sh --closure puts it into an image:
#
#   nix build --no-link --print-out-paths -f rig/heavy/extras.nix
{
  system ? "x86_64-linux",
}:
let
  lock = builtins.fromJSON (builtins.readFile ../guest-image/flake.lock);
  n = lock.nodes.nixpkgs.locked;
  nixpkgs = builtins.getFlake "${n.type}:${n.owner}/${n.repo}/${n.rev}";
  pkgs = import nixpkgs {
    inherit system;
    config.allowUnfree = true;
  };
  gameloop = pkgs.stdenv.mkDerivation {
    pname = "nvgpu-gameloop";
    version = "0.1.0";
    src = pkgs.lib.fileset.toSource {
      root = ../.;
      fileset = pkgs.lib.fileset.unions [
        ./gameloop.c
        ./rtprobe.c
        ./rtprobe.comp
        ./wakecost.c
        ../guest-image/tools/nvgpu-bench.vert
        ../guest-image/tools/nvgpu-bench.frag
      ];
    };
    nativeBuildInputs = with pkgs; [
      pkg-config
      wayland-scanner
      glslang
    ];
    buildInputs = with pkgs; [
      wayland
      vulkan-headers
      vulkan-loader
      wayland-protocols
    ];
    buildPhase = ''
      runHook preBuild
      glslangValidator -V --vn gl_vs guest-image/tools/nvgpu-bench.vert -o vs.h
      glslangValidator -V --vn gl_fs guest-image/tools/nvgpu-bench.frag -o fs.h
      cat vs.h fs.h > gameloop-spv.h
      xs=${pkgs.wayland-protocols}/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml
      wayland-scanner client-header $xs xdg-shell-client-protocol.h
      wayland-scanner private-code $xs xdg-shell-protocol.c
      $CC -O2 -g -Wall -Wextra -I. heavy/gameloop.c xdg-shell-protocol.c -o nvgpu-gameloop \
        -lpthread -lm $(pkg-config --cflags --libs vulkan wayland-client)
      glslangValidator -V --target-env vulkan1.2 --vn rt_cs heavy/rtprobe.comp -o rtprobe-spv.h
      $CC -O2 -g -Wall -Wextra -I. heavy/rtprobe.c -o nvgpu-rtprobe $(pkg-config --cflags --libs vulkan)
      $CC -O2 -g -Wall -Wextra heavy/wakecost.c -o nvgpu-wakecost -lpthread
      runHook postBuild
    '';
    installPhase = ''
      install -Dm755 -t $out/bin nvgpu-gameloop nvgpu-rtprobe nvgpu-wakecost
    '';
  };
in
pkgs.buildEnv {
  name = "nvgpu-heavy-extras";
  paths = [
    gameloop
    pkgs.wineWow64Packages.stagingFull
    pkgs.zeroad
    pkgs.xonotic
    pkgs.stress-ng
  ];
  pathsToLink = [
    "/bin"
    "/share"
    "/lib"
  ];
  ignoreCollisions = true;
  # DXVK's DLLs are not under /bin; heavy-run.sh finds them here.
  postBuild = ''
    ln -s ${pkgs.dxvk} $out/dxvk
  '';
  # The guest image's VK_LAYER_NVGPU_no_uvm, to try in an image made before
  # it (`-f rig/heavy/extras.nix vkLayer`).
  passthru.vkLayer = import ../guest-image/nix/vk-layer.nix {
    inherit pkgs;
    src = ../../nvgpu-vk-layer;
  };
}
