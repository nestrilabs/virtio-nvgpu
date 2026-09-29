# SPDX-License-Identifier: Apache-2.0
{
  # The guest root filesystem for on-device tests of virtio-nvgpu.
  #
  # `guestRoot` is a root overlay (bin/, etc/, opt/nvgpu/, run/opengl-driver)
  # whose symlinks point into its own closure; mkimage.sh copies the overlay and
  # the closure into a staging tree and makes the ext4 image from it. Read
  # README.md; do not build this directly unless you know you want the pieces.
  #
  # The repo's own sources (nvgpu-wl-guest and the workspace it lives in, and
  # rig/verify) are NOT read from the parent directory: a flake can only
  # see its own tree. mkimage.sh copies them into ./nvgpu-src of a staged copy
  # of this directory (under .rig/guest/flake) and builds that. Without
  # ./nvgpu-src the image is built without nvgpu-wl-guest and the verify
  # helpers, and says so in /etc/nvgpu/manifest.
  description = "virtio-nvgpu guest root filesystem (NVIDIA 595.99.02 userspace, 64- and 32-bit, + display test tools)";

  # Pinned to the nixpkgs the rest of the rig was built with, so what comes
  # from the binary cache is what was checked.
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/34ca302a9572963c02e385c056be37c85ff51b77";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        config = {
          allowUnfree = true;
          # The NVIDIA userspace is the user's own driver release, for their
          # own card; building the package needs this acknowledged.
          nvidia.acceptLicense = true;
        };
      };
      lib = pkgs.lib;

      nvgpuSrc = if builtins.pathExists ./nvgpu-src then ./nvgpu-src else null;

      nvidia = import ./nix/nvidia.nix { inherit pkgs; };

      nvgpu-wl-guest =
        if nvgpuSrc == null then null else import ./nix/nvgpu-wl-guest.nix { inherit pkgs nvgpuSrc; };

      cudaApps = import ./nix/cuda-apps.nix {
        inherit pkgs;
        src = ./apps/cuda;
      };

      appsData = import ./nix/apps-data.nix {
        inherit pkgs;
        appsSrc = ./apps;
      };

      blenderCuda = import ./nix/blender-cuda.nix { inherit pkgs; };

      # Qt Quick's own runtime (the `qml` tool), wrapped like an app so it
      # finds the Wayland platform plugin and QtQuick's QML modules.
      qmlRunner = pkgs.stdenv.mkDerivation {
        name = "nvgpu-qml";
        dontUnpack = true;
        nativeBuildInputs = [ pkgs.qt6.wrapQtAppsHook ];
        buildInputs = with pkgs.qt6; [
          qtbase
          qtdeclarative
          qtwayland
        ];
        installPhase = ''
          mkdir -p $out/bin
          cp ${pkgs.qt6.qtdeclarative}/bin/qml $out/bin/nvgpu-qml
        '';
      };

      tools = import ./nix/tools.nix {
        inherit pkgs nvgpuSrc;
        toolsSrc = ./tools;
      };

      # 32-bit clients and their userspace (probes/compat.sh).
      tools32 = import ./nix/tools32.nix {
        inherit pkgs;
        toolsSrc = ./tools;
      };

      # nixpkgs' weston builds none of its simple clients (-Dsimple-clients=);
      # the Wayland stage uses these (weston-simple-egl, -shm, -damage,
      # -dmabuf-egl, -dmabuf-feedback). weston itself is never started.
      weston-clients =
        (pkgs.weston.override {
          # Only the clients are wanted: no backends, shells or demos (and
          # none of freerdp, neatvnc, xwayland, lua in the closure).
          demoSupport = false;
          rdpSupport = false;
          vncSupport = false;
          xwaylandSupport = false;
          luaSupport = false;
          pipewireSupport = false;
          remotingSupport = false;
        }).overrideAttrs
          (old: {
            pname = "weston-simple-clients";
            mesonFlags =
              (lib.filter (f: !(lib.hasPrefix "-Dsimple-clients" f)) old.mesonFlags)
              ++ [ "-Dsimple-clients=egl,shm,damage,dmabuf-egl,dmabuf-feedback" ];
          });

      fontsConf = pkgs.makeFontsConf { fontDirectories = [ pkgs.dejavu_fonts ]; };

      # Everything on PATH in the guest.
      sw = pkgs.buildEnv {
        name = "nvgpu-guest-sw";
        paths =
          (with pkgs; [
            bashInteractive
            coreutils
            util-linux
            kmod
            procps
            findutils
            gnugrep
            gnused
            gawk
            diffutils
            gnutar
            gzip
            less
            which
            file
            jq
            pciutils
            strace
            gdb
            libdrm.bin # modetest, proptest, ... (-Dinstall-test-programs=true)
            drm_info
            kmscube
            vulkan-tools # vulkaninfo, vkcube
            mesa-demos # eglinfo, eglgears_wayland, es2gears_wayland
            wayland-utils # wayland-info
            sway
            swaybg
            hyprland # TESTING.md stage 6's guest compositor (compositor.sh nvgpu_comp=hyprland)
            seatd
            foot
            wlr-randr
            # Real clients, for the Wayland stage's application pass: X11
            # through a rootful Xwayland, a nested gamescope, toolkits (GTK4,
            # Qt6), a browser, a video player, and GL/Vulkan benchmarks.
            xwayland
            xorg.xeyes
            xterm
            gamescope
            gnome-calculator # GTK4 + libadwaita
            qalculate-qt
            firefox
            chromium
            mpv
            glmark2
            vkmark
            wev # the events a client gets (apps.sh pointer)
            wl-clipboard # wl-copy, wl-paste (apps.sh clipboard)
            dbus # dbus-run-session, for the toolkits
            # The broader application pass (apps.sh; rig/TESTING-RIG.md has the
            # table): games and engines, creative apps, office and toolkits,
            # Electron, video decode and encode, and compute.
            supertuxkart # GL, SDL2, native Wayland
            neverball # SDL2 + GL, through Xwayland and gamescope
            godot_4 # Vulkan (Forward+) and GL (compatibility), native Wayland
            gimp
            inkscape
            krita # Qt6, OpenGL canvas
            libreoffice-fresh # Skia (Vulkan) through Xwayland's gen VCL
            gnome-text-editor # GTK4: GSK's ngl and vulkan renderers
            element-desktop # Electron
            electron
            ffmpeg-full # NVENC, NVDEC (cuda), Vulkan Video; the test clips
            libva-utils # vainfo
            clinfo
            clpeak
            # Per-frame times, the same build natively and in the guest
            # (rig/rig-framepace.sh).
            mangohud
          ])
          ++ [
            weston-clients
            nvidia.bin # nvidia-smi
            tools
            tools32 # nvgpu-drm-compat-32, nvgpu-rm-smoke-32, vulkaninfo-32, eglinfo-32
            qmlRunner
            blenderCuda # the GL/Vulkan UI, and Cycles on CUDA and OptiX
            cudaApps # nvgpu-nbody
          ]
          ++ lib.optional (nvgpu-wl-guest != null) nvgpu-wl-guest;
        pathsToLink = [
          "/bin"
          "/share"
          "/libexec"
        ];
        ignoreCollisions = true;
      };

      # nixpkgs' vulkan-loader, libglvnd and libgbm look for drivers under
      # /run/opengl-driver; our init scripts point that at this.
      openglDriver = nvidia.driversEnv;
      # And /run/opengl-driver-32, for the i686 loaders (32-bit clients).
      openglDriver32 = nvidia.driversEnv32;

      manifest = pkgs.writeText "nvgpu-manifest" ''
        nvidia-userspace ${nvidia.driver.version} ${nvidia.driver.out}
        opengl-driver ${openglDriver}
        opengl-driver-32 ${openglDriver32}
        sw ${sw}
        nvgpu-wl-guest ${if nvgpu-wl-guest == null then "ABSENT (no ./nvgpu-src)" else "${nvgpu-wl-guest}"}
        verify-helpers ${if nvgpuSrc == null then "ABSENT (no ./nvgpu-src)" else "${tools}/libexec/nvgpu/verify"}
        nixpkgs ${nixpkgs.rev or "dirty"}
      '';

      guestRoot = pkgs.runCommand "nvgpu-guest-root"
        {
          inherit sw openglDriver openglDriver32 fontsConf manifest;
          nvidiaBin = nvidia.bin;
          nvidiaOut = nvidia.driver.out;
          probes = ./probes;
          inherit appsData;
          bash = pkgs.bashInteractive;
          coreutils = pkgs.coreutils;
          verifySrc = if nvgpuSrc == null then "" else "${nvgpuSrc}/rig/verify";
          tools = tools;
        }
        ''
          set -eu
          mkdir -p $out
          cd $out
          mkdir -p bin usr/bin sbin usr/sbin etc/nvgpu opt/nvgpu/verify root \
                   proc sys dev run tmp mnt var/tmp var/log var/empty home

          # The canonical names the probes use; /run is a tmpfs at boot, so
          # probe-common.sh re-creates /run/opengl-driver from the /etc link.
          ln -s $sw           etc/nvgpu/sw
          ln -s $openglDriver etc/nvgpu/opengl-driver
          ln -s $openglDriver run/opengl-driver
          ln -s $openglDriver32 etc/nvgpu/opengl-driver-32
          ln -s $openglDriver32 run/opengl-driver-32
          cp $manifest etc/nvgpu/manifest

          ln -s $bash/bin/bash      bin/sh
          ln -s $bash/bin/bash      bin/bash
          for t in $sw/bin/*; do ln -sf "$t" usr/bin/; done
          ln -sf $coreutils/bin/env usr/bin/env

          # Users. Root only; video/render for tools that look the groups up.
          cat > etc/passwd <<EOF
          root:x:0:0:root:/root:/bin/bash
          nobody:x:65534:65534:nobody:/var/empty:/bin/false
          EOF
          cat > etc/group <<EOF
          root:x:0:
          wheel:x:1:root
          kvm:x:26:root
          video:x:27:root
          render:x:28:root
          input:x:29:root
          nvgpu-wl:x:900:root
          nogroup:x:65534:
          EOF
          echo 'root:!:1::::::' > etc/shadow
          chmod 0600 etc/shadow
          echo nvgpu-guest > etc/hostname
          printf '127.0.0.1 localhost nvgpu-guest\n::1 localhost\n' > etc/hosts
          printf 'passwd: files\ngroup: files\nshadow: files\nhosts: files\n' > etc/nsswitch.conf
          cat > etc/os-release <<EOF
          NAME="virtio-nvgpu guest"
          ID=nvgpu-guest
          PRETTY_NAME="virtio-nvgpu test guest (nix)"
          EOF
          : > etc/fstab
          mkdir -p etc/fonts
          ln -s $fontsConf etc/fonts/fonts.conf

          # NVIDIA's libEGL looks for its external platforms (egl-wayland,
          # egl-gbm) here, as on NixOS.
          mkdir -p etc/egl usr/share/egl
          ln -s /run/opengl-driver/share/egl/egl_external_platform.d etc/egl/egl_external_platform.d
          ln -s /run/opengl-driver/share/egl/egl_external_platform.d usr/share/egl/egl_external_platform.d
          if [ -e $nvidiaBin/share/nvidia/nvidia-application-profiles-rc ]; then
            mkdir -p etc/nvidia
            ln -s $nvidiaBin/share/nvidia/nvidia-application-profiles-rc etc/nvidia/
          fi

          # Login shell environment for shell.sh and anything interactive.
          cat > etc/profile <<'EOF'
          [ -r /opt/nvgpu/env.sh ] && . /opt/nvgpu/env.sh
          PS1='[nvgpu-guest \w]# '
          EOF

          # Probes. Executable, and the shared library alongside.
          for p in $probes/*.sh; do
            install -m 0755 "$p" opt/nvgpu/
          done
          chmod 0644 opt/nvgpu/*-common.sh opt/nvgpu/env.sh
          mkdir -p opt/nvgpu/bin
          for t in $tools/bin/*; do ln -s "$t" opt/nvgpu/bin/; done
          ln -s $tools/lib/libnvgpu-shim.so opt/nvgpu/libnvgpu-shim.so

          # What the application pass opens: test pages and clips, the Godot,
          # QML, Electron and Blender pieces (nix/apps-data.nix).
          ln -s $appsData opt/nvgpu/apps
          # OpenCL's ICD loader looks here for NVIDIA's ICD.
          mkdir -p etc/OpenCL
          ln -s /run/opengl-driver/etc/OpenCL/vendors etc/OpenCL/vendors

          # The repo's verify scripts, with the helpers already built (the
          # wrappers only build when bin/<helper> is missing).
          if [ -n "$verifySrc" ]; then
            for s in $verifySrc/*.sh; do install -m 0755 "$s" opt/nvgpu/verify/; done
            mkdir -p opt/nvgpu/verify/bin
            for b in $tools/libexec/nvgpu/verify/*; do ln -s "$b" opt/nvgpu/verify/bin/; done
          fi
        '';
    in
    {
      packages.${system} = {
        default = guestRoot;
        inherit
          guestRoot
          sw
          openglDriver
          openglDriver32
          tools
          tools32
          weston-clients
          appsData
          blenderCuda
          qmlRunner
          cudaApps
          ;
        nvidia-userspace = nvidia.driver;
      }
      // lib.optionalAttrs (nvgpu-wl-guest != null) { inherit nvgpu-wl-guest; };
    };
}
