# SPDX-License-Identifier: Apache-2.0
# The small C programs the probes need that nixpkgs does not have, and the
# repo's two verify helpers (sec-negative, lease-flip) built against nix's
# libdrm so they match the guest's glibc.
#
#   bin/nvgpu-lease          wp_drm_lease_device_v1 client; runs a KMS tool on the lease fd
#   lib/libnvgpu-shim.so     LD_PRELOAD: /dev/dri/lease -> the lease fd; modetest -D <path>
#   bin/vk-acquire-display   vkGetDrmDisplayEXT + vkAcquireDrmDisplayEXT + present (stage 5)
#   bin/cuda-smoke           CUDA driver API round trip + a PTX kernel, dlopen()s libcuda
#   bin/nvgpu-poweroff       reboot(RB_POWER_OFF) for PID-1 probe scripts
#   bin/egl-fence            EGL_ANDROID_native_fence_sync: create, export, wait, import
#   bin/gl-then-vk           a GL context survives a Vulkan device in the same process
#   libexec/nvgpu/verify/{sec-negative,lease-flip}   (only with nvgpuSrc)
{
  pkgs,
  nvgpuSrc,
  toolsSrc,
}:
let
  lib = pkgs.lib;
in
pkgs.stdenv.mkDerivation {
  pname = "nvgpu-guest-tools";
  version = "0.1.0";
  src = toolsSrc;
  nativeBuildInputs = with pkgs; [
    pkg-config
    wayland-scanner
  ];
  buildInputs = with pkgs; [
    libdrm
    wayland
    vulkan-headers
    vulkan-loader
    libglvnd
  ];
  verifySrc = if nvgpuSrc == null then "" else "${nvgpuSrc}/rig/verify";
  buildPhase = ''
    runHook preBuild
    CFLAGS="-O2 -g -Wall -Wextra -Wno-unused-parameter"
    xml=${pkgs.wayland-protocols}/share/wayland-protocols/staging/drm-lease/drm-lease-v1.xml
    wayland-scanner client-header $xml drm-lease-v1-client-protocol.h
    wayland-scanner private-code  $xml drm-lease-v1-protocol.c

    $CC $CFLAGS -I. nvgpu-lease.c drm-lease-v1-protocol.c -o nvgpu-lease \
      $(pkg-config --cflags --libs wayland-client)
    $CC $CFLAGS -shared -fPIC nvgpu-shim.c -o libnvgpu-shim.so -ldl
    $CC $CFLAGS vk-acquire-display.c -o vk-acquire-display \
      $(pkg-config --cflags --libs libdrm vulkan)
    $CC $CFLAGS cuda-smoke.c -o cuda-smoke -ldl
    $CC $CFLAGS nvgpu-poweroff.c -o nvgpu-poweroff
    $CC $CFLAGS egl-fence.c -o egl-fence -lEGL -lGLESv2
    $CC $CFLAGS gl-then-vk.c -o gl-then-vk -lEGL -lGLESv2 $(pkg-config --cflags --libs vulkan)

    if [ -n "$verifySrc" ]; then
      $CC -O2 -Wall -Wextra $(pkg-config --cflags libdrm) $verifySrc/sec-negative.c \
        -o sec-negative $(pkg-config --libs libdrm)
      $CC -O2 -Wall -Wextra $(pkg-config --cflags libdrm) $verifySrc/lease-flip.c \
        -o lease-flip $(pkg-config --libs libdrm)
    fi
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    install -Dm755 -t $out/bin nvgpu-lease vk-acquire-display cuda-smoke nvgpu-poweroff egl-fence gl-then-vk
    install -Dm755 -t $out/lib libnvgpu-shim.so
    if [ -n "$verifySrc" ]; then
      install -Dm755 -t $out/libexec/nvgpu/verify sec-negative lease-flip
    fi
    runHook postInstall
  '';
  # libcuda comes from the driver tree at run time, as for any nix program
  # (addDriverRunpath's convention). After fixup, which would drop it, and the
  # linker wrapper, which does not pass an rpath outside the store.
  postFixup = ''
    patchelf --add-rpath /run/opengl-driver/lib $out/bin/cuda-smoke
  '';
  meta.description = "virtio-nvgpu guest test helpers";
}
