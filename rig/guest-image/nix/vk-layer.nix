# SPDX-License-Identifier: Apache-2.0
# VK_LAYER_NVGPU_no_uvm (nvgpu-vk-layer/): an implicit Vulkan layer that hides
# the NVIDIA device extensions a guest without compute cannot create a device
# with. `arch` is the manifest's library_arch: the i686 build's manifest is
# named apart, so both can sit in one implicit_layer.d and each loader takes
# the one of its own class.
{
  pkgs,
  src,
  arch ? "64",
}:
pkgs.stdenv.mkDerivation {
  pname = "nvgpu-vk-layer";
  version = "0.1.0";
  inherit src;
  buildInputs = [ pkgs.vulkan-headers ];
  makeFlags = [
    "PREFIX=${placeholder "out"}"
    "LIBARCH=${arch}"
    "MANIFEST=VK_LAYER_NVGPU_no_uvm${if arch == "32" then "_32" else ""}.json"
  ];
  meta.description = "virtio-nvgpu guest: hide UVM-only Vulkan extensions without compute";
}
