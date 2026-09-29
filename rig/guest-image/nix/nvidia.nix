# SPDX-License-Identifier: Apache-2.0
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
        # The 32-bit libraries too (the `lib32` output), for the guest's
        # 32-bit clients -- Steam's, a 32-bit game's GL and Vulkan -- as
        # NixOS's hardware.graphics.enable32Bit gives them.
        disable32Bit = false;
      };

  # The EGL external platforms NVIDIA's libEGL loads for Wayland and GBM
  # surfaces (and X11, which nothing here uses but costs little). The same set,
  # and the same way of joining them, as nixos/modules/hardware/video/nvidia.nix
  # for a >= 595 driver (no priority remediation) -- for each width.
  eglPlatformsFor =
    p:
    p.symlinkJoin {
      name = "nvidia-egl-external-platforms${lib.optionalString p.stdenv.hostPlatform.is32bit "-x32"}";
      paths = with p; [
        egl-wayland
        egl-gbm
        egl-wayland2
        egl-x11
      ];
    };
  eglPlatforms = eglPlatformsFor pkgs;

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
  # What /run/opengl-driver-32 points at: NixOS's driversEnv32 for
  # hardware.graphics.enable32Bit with the NVIDIA module (extraPackages32:
  # nvidia_x11.lib32 and the i686 EGL platforms), again without mesa. The
  # i686 loaders nixpkgs builds look there (addDriverRunpath's driverLink is
  # /run/opengl-driver-32 on i686).
  driversEnv32 = pkgs.buildEnv {
    name = "nvgpu-opengl-driver-32-${version}";
    paths = [
      driver.lib32
      (eglPlatformsFor pkgs.pkgsi686Linux)
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
  inherit driver driversEnv driversEnv32 eglPlatforms;
  bin = driver.bin;
}
