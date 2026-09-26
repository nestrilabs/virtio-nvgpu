# NVIDIA's userspace at exactly the host's release, built by nixpkgs' own
# machinery (nvidiaPackages.mkDriver), and the /run/opengl-driver tree the
# nix-built loaders read.
#
# The guest's userspace must be the host kernel module's release to the digit:
# RM refuses a client whose version string differs. nixpkgs-unstable carries
# 595.104.02, not 595.99.02, so the release is spelled out here. To follow the
# host to another release, change `version` and `sha256_64bit` (get it with
#   nix store prefetch-file --hash-type sha256 \
#     https://download.nvidia.com/XFree86/Linux-x86_64/<v>/NVIDIA-Linux-x86_64-<v>.run
# ), and the backend needs gen/ tables for that release too.
{ pkgs }:
let
  lib = pkgs.lib;

  version = "595.99.02";

  driver =
    (pkgs.linuxPackages.nvidiaPackages.mkDriver {
      inherit version;
      sha256_64bit = "sha256-6HR3lYv3YwcFSTJL1a1slI66btIQ5EAFs+/4SUD24ew=";
      # Userspace only: no kernel module (the guest's /dev/nvidia* come from
      # virtio_gpu_nv), no GSP firmware (openSha256 = null), no settings GUI,
      # no persistenced.
      openSha256 = null;
      useSettings = false;
      usePersistenced = false;
    }).override
      {
        # The 64-bit libraries only; the guest runs no 32-bit clients.
        disable32Bit = true;
      };

  # The EGL external platforms NVIDIA's libEGL loads for Wayland and GBM
  # surfaces (and X11, which nothing here uses but costs little). The same set,
  # and the same way of joining them, as nixos/modules/hardware/video/nvidia.nix
  # for a >= 595 driver (no priority remediation).
  eglPlatforms = pkgs.symlinkJoin {
    name = "nvidia-egl-external-platforms";
    paths = with pkgs; [
      egl-wayland
      egl-gbm
      egl-wayland2
      egl-x11
    ];
  };

  # What /run/opengl-driver points at. NixOS merges mesa in here too; the
  # guest must use the proprietary userspace only (no NVK, no llvmpipe
  # fallback that would hide a broken NVIDIA ICD), so mesa is left out.
  driversEnv = pkgs.buildEnv {
    name = "nvgpu-opengl-driver-${version}";
    paths = [
      driver.out
      eglPlatforms
      # VA-API on NVDEC (through CUDA), for the video slots: libva looks in
      # /run/opengl-driver/lib/dri, as NixOS's hardware.graphics.extraPackages.
      pkgs.nvidia-vaapi-driver
    ];
    pathsToLink = [
      "/lib"
      "/share"
      "/etc"
    ];
    ignoreCollisions = false;
  };
in
{
  inherit driver driversEnv eglPlatforms;
  bin = driver.bin;
}
