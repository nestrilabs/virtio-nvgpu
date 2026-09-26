# SPDX-License-Identifier: Apache-2.0
# Blender with Cycles' CUDA and OptiX devices, for the application pass's
# compute slot (apps.sh blendercuda). nixpkgs' cached blender has neither:
# its cudaSupport switch also rebuilds OpenSubdiv, OpenImageDenoise and
# OpenUSD for CUDA and every architecture, hours of build. This rebuilds
# only blender, with the kernels for the rig's card alone (sm_120, the RTX
# 5090, and its PTX); the libraries it links stay the cached CPU ones.
#
# At run time Cycles loads libcuda and libnvoptix from /run/opengl-driver/lib
# (the RUNPATH addDriverRunpath gives it), as on NixOS.
{ pkgs }:
let
  cp = pkgs.cudaPackages;
  # The OptiX SDK headers blender's package.nix pins (build_files/config/
  # pipeline_config.yaml upstream).
  optix = pkgs.fetchFromGitHub {
    owner = "NVIDIA";
    repo = "optix-dev";
    tag = "v8.0.0";
    hash = "sha256-SXkXZHzQH8JOkXypjjxNvT/lUlWZkCuhh6hNCHE7FkY=";
  };
in
(pkgs.blender.override { stdenv = cp.backendStdenv; }).overrideAttrs (old: {
  pname = "blender-cuda";
  # CMake takes the last -D of a name: these win over the package's own OFFs.
  cmakeFlags = old.cmakeFlags ++ [
    "-DWITH_CYCLES_DEVICE_CUDA=ON"
    "-DWITH_CYCLES_CUDA_BINARIES=ON"
    "-DCYCLES_CUDA_BINARIES_ARCH=sm_120;compute_120"
    "-DWITH_CYCLES_DEVICE_OPTIX=ON"
    "-DOPTIX_ROOT_DIR=${optix}"
  ];
  nativeBuildInputs = old.nativeBuildInputs ++ [
    cp.cuda_nvcc
    pkgs.addDriverRunpath
  ];
  buildInputs = old.buildInputs ++ [ cp.cuda_cudart ];
  postFixup = (old.postFixup or "") + ''
    for program in $out/bin/blender $out/bin/.blender-wrapped; do
      [ -e "$program" ] && addDriverRunpath "$program"
    done
  '';
  doCheck = false;
})
