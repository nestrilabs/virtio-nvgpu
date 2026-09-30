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
#   bin/nvgpu-map-churn      RM map / UPDATE_DEVICE_MAPPING_INFO / unmap-by-address, many times
#   bin/egl-fence            EGL_ANDROID_native_fence_sync: create, export, wait, import
#   bin/gl-then-vk           a GL context survives a Vulkan device in the same process
#   bin/nvgpu-capture-import /dev/nvgpu-capture: an injected host buffer into EGL
#                            and Vulkan, pixels and read-only mappings checked
#   bin/nvgpu-drm-compat     DRM ioctls whose struct grew, at older, native and
#                            newer sizes (SYNCOBJ_HANDLE_TO_FD's 16 bytes, ...)
#   bin/nvgpu-syncobj-race   syncobjs made, imported, signalled, subscribed to
#                            and destroyed by many threads of one file at once
#   bin/nvgpu-rm-smoke       an RM client (alloc, controls, free) and a DRM
#                            ioctl; tools32.nix builds both of these 32-bit too
#   bin/nvgpu-bench          Vulkan, GL, RM and wl_shm microbenchmarks (BENCHMARKS.md)
#   bin/nvgpu-cubench        CUDA driver-API microbenchmarks, libcuda dlopen()ed
#   bin/nvgpu-bench-suite    the matrix of BENCHMARKS.md, as BENCH lines
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
    glslang
  ];
  buildInputs = with pkgs; [
    libdrm
    wayland
    vulkan-headers
    vulkan-loader
    libglvnd
    wayland-protocols
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
    $CC $CFLAGS nvgpu-map-churn.c -o nvgpu-map-churn
    $CC $CFLAGS egl-fence.c -o egl-fence -lEGL -lGLESv2
    $CC $CFLAGS gl-then-vk.c -o gl-then-vk -lEGL -lGLESv2 $(pkg-config --cflags --libs vulkan)
    $CC $CFLAGS nvgpu-capture-import.c -o nvgpu-capture-import -lEGL -lGLESv2 \
      $(pkg-config --cflags --libs vulkan)
    $CC $CFLAGS drm-compat.c -o nvgpu-drm-compat
    $CC $CFLAGS -pthread syncobj-race.c -o nvgpu-syncobj-race
    $CC $CFLAGS rm-smoke.c -o nvgpu-rm-smoke

    glslangValidator -V --vn bench_vs nvgpu-bench.vert -o bench-vs.h
    glslangValidator -V --vn bench_fs nvgpu-bench.frag -o bench-fs.h
    glslangValidator -V --vn bench_full_vs nvgpu-bench-full.vert -o bench-full-vs.h
    glslangValidator -V --vn bench_cost_fs nvgpu-bench-cost.frag -o bench-cost-fs.h
    cat bench-vs.h bench-fs.h bench-full-vs.h bench-cost-fs.h > nvgpu-bench-spv.h
    xs=${pkgs.wayland-protocols}/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml
    wayland-scanner client-header $xs xdg-shell-client-protocol.h
    wayland-scanner private-code  $xs xdg-shell-protocol.c
    $CC $CFLAGS -I. nvgpu-bench.c xdg-shell-protocol.c -o nvgpu-bench \
      -lEGL -lGLESv2 $(pkg-config --cflags --libs vulkan wayland-client)
    $CC $CFLAGS nvgpu-cubench.c -o nvgpu-cubench -ldl

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
    install -Dm755 -t $out/bin nvgpu-lease vk-acquire-display cuda-smoke nvgpu-poweroff egl-fence gl-then-vk \
      nvgpu-capture-import nvgpu-map-churn nvgpu-drm-compat nvgpu-syncobj-race nvgpu-rm-smoke nvgpu-bench nvgpu-cubench
    install -Dm755 -t $out/lib libnvgpu-shim.so
    install -Dm755 nvgpu-bench-suite.sh $out/bin/nvgpu-bench-suite
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
    patchelf --add-rpath /run/opengl-driver/lib $out/bin/nvgpu-cubench
  '';
  meta.description = "virtio-nvgpu guest test helpers";
}
