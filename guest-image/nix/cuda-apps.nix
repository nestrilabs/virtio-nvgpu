# SPDX-License-Identifier: Apache-2.0
# nvgpu-nbody (../apps/cuda): a CUDA runtime program for the application
# pass, built with nvcc for the rig's card (sm_120, plus PTX). Only nvcc and
# the CUDA runtime: NVIDIA's samples and saxpy need cuBLAS, cuFFT, cuSPARSE and
# more, about 2 GB of downloads, for no path this one does not already take.
{ pkgs, src }:
let
  cp = pkgs.cudaPackages;
in
cp.backendStdenv.mkDerivation {
  name = "nvgpu-cuda-apps";
  inherit src;
  nativeBuildInputs = [
    cp.cuda_nvcc
    pkgs.autoAddDriverRunpath
  ];
  buildInputs = [ cp.cuda_cudart ];
  buildPhase = ''
    nvcc -O2 -gencode arch=compute_120,code=sm_120 -gencode arch=compute_120,code=compute_120 \
      nvgpu-nbody.cu -o nvgpu-nbody
  '';
  installPhase = ''
    install -Dm755 nvgpu-nbody $out/bin/nvgpu-nbody
  '';
}
