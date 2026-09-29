# SPDX-License-Identifier: Apache-2.0
# 32-bit clients, for the compat probe (probes/compat.sh): what a 32-bit
# process (Steam's client, a 32-bit game's GL and Vulkan driver) sends the
# guest module goes through its compat_ioctl, and these are the smallest ways
# to send it.
#
#   bin/nvgpu-drm-compat-32  tools/drm-compat.c, built i686
#   bin/nvgpu-rm-smoke-32    tools/rm-smoke.c, built i686
#   bin/vulkaninfo-32        pkgsi686Linux.vulkan-tools' vulkaninfo
#   bin/eglinfo-32           pkgsi686Linux.mesa-demos' eglinfo
#
# The two viewers run with the loaders pinned to NVIDIA's 32-bit files under
# /run/opengl-driver-32 (nvidia.nix's driversEnv32, which the image links
# there as NixOS's hardware.graphics.enable32Bit does), as env.sh pins the
# 64-bit ones: a missing 32-bit ICD is a failure, never a fallback.
{ pkgs, toolsSrc }:
let
  p32 = pkgs.pkgsi686Linux;
  d = "/run/opengl-driver-32";

  bins = p32.stdenv.mkDerivation {
    pname = "nvgpu-guest-tools-32";
    version = "0.1.0";
    src = toolsSrc;
    buildPhase = ''
      runHook preBuild
      CFLAGS="-O2 -g -Wall -Wextra -Wno-unused-parameter"
      $CC $CFLAGS drm-compat.c -o nvgpu-drm-compat-32
      $CC $CFLAGS rm-smoke.c -o nvgpu-rm-smoke-32
      runHook postBuild
    '';
    installPhase = ''
      runHook preInstall
      install -Dm755 -t $out/bin nvgpu-drm-compat-32 nvgpu-rm-smoke-32
      runHook postInstall
    '';
    meta.description = "virtio-nvgpu guest test helpers, 32-bit";
  };

  wrap =
    name: exe:
    pkgs.writeShellScriptBin name ''
      export VK_DRIVER_FILES=${d}/share/vulkan/icd.d/nvidia_icd.json
      export __EGL_VENDOR_LIBRARY_FILENAMES=${d}/share/glvnd/egl_vendor.d/10_nvidia.json
      export __GLX_VENDOR_LIBRARY_NAME=nvidia
      export GBM_BACKENDS_PATH=${d}/lib/gbm
      export __EGL_EXTERNAL_PLATFORM_CONFIG_DIRS=${d}/share/egl/egl_external_platform.d
      exec ${exe} "$@"
    '';
in
pkgs.symlinkJoin {
  name = "nvgpu-guest-32";
  paths = [
    bins
    (wrap "vulkaninfo-32" "${p32.vulkan-tools}/bin/vulkaninfo")
    (wrap "eglinfo-32" "${p32.mesa-demos}/bin/eglinfo")
  ];
}
