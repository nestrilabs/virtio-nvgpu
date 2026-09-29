# SPDX-License-Identifier: Apache-2.0
#
# NixOS module: the virtio-nvgpu backend, one systemd unit per VM, each VM a
# host user of its own (DEPLOY.md, "Per-VM users").
#
#   imports = [ ./nix/module.nix ];          # or the flake's nixosModules.default
#   services.virtio-nvgpu = {
#     enable = true;
#     package = <a package with bin/vhost-user-nvgpu>;   # the flake's default
#     slots = 4;                              # VMs that may run at once
#     vms."0".extraArgs = [ "--allow-compute" ];
#     vms."0".inject = { enable = true; helperUid = 950; helperGroup = "nvgpu-cap0"; };
#   };
#
# Slot N is two system users: nvgpu-vmN runs the backend
# (vhost-user-nvgpu@N.service), and nvgpu-vmmN, in group nvgpu-vmN and no
# other, is for the VMM, which connects to /run/nvgpu/vmN/nvgpu.sock. The unit
# is contrib/systemd/vhost-user-nvgpu@.service, said in Nix; keep the two in
# step. Nothing here starts a VMM: give its unit
#   requires = [ "vhost-user-nvgpu@N.service" ]; after = [ same ];
# and User = "nvgpu-vmmN". For a compute VM (--allow-compute), that unit also
# needs the mincore syscall (not in @system-service) and, under a device
# policy, DeviceAllow "/dev/nvidia-uvm w": the VMM checks each UVM pool with
# mincore, which the kernel answers only for a file it may write. A device
# policy names major 195 "char-nvidia" (DEPLOY.md, "Per-VM users").
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkIf
    mkOption
    types
    genList
    listToAttrs
    nameValuePair
    mapAttrs'
    escapeShellArgs
    concatStringsSep
    ;

  cfg = config.services.virtio-nvgpu;
  ids = genList toString cfg.slots;

  socketOpen = pkgs.writeShellScript "nvgpu-socket-open" (
    builtins.readFile ../contrib/systemd/nvgpu-socket-open
  );

  vmOptions = {
    options = {
      extraArgs = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [
          "--allow-compute"
        ];
        description = ''
          Backend flags for this VM only, after `services.virtio-nvgpu.extraArgs`
          (DEPLOY.md, "Backend flags"). The diagnostic flags are refused
          without `--diagnostic`; do not ship a configuration that needs it.
        '';
      };
      windowMiB = mkOption {
        type = types.nullOr (types.ints.between 256 65536);
        default = null;
        example = 16384;
        description = ''
          The VM's shared window in MiB (`--window-size`; null is the
          backend's 1024): how much GPU memory the VM's processes can have
          CPU-mapped at once. A multiple of 64; with `--allow-compute` at
          most 64512 (window and UVM aperture share crosvm's 64 GiB region
          cap), under nesbox at most 32768. The VMM takes the size from the
          backend. Under crosvm, pages of the window the guest touches with
          nothing placed there are the VMM's shared memory: size the VMM's
          MemoryMax as guest RAM plus this (DEPLOY.md, "Sizing the window").
        '';
      };
      windowOwnerShare = mkOption {
        type = types.nullOr (types.ints.between 1 95);
        default = null;
        example = 90;
        description = ''
          The percent of each window zone one guest process may hold
          (`--window-owner-share`; null is the backend's 50). From 88 one
          process can take a zone down to its reserve, leaving the VM's
          other processes the reserve alone: availability within this VM
          only (SECURITY.md, "The window's size and share").
        '';
      };
      inject = {
        enable = mkEnableOption ''
          capture injection for this VM (SECURITY.md §18): the backend listens
          at /run/nvgpu/vmN/inject.sock for this VM's capture helper, opened
          to `helperGroup`, and serves only `helperUid` there. Give every VM
          a helper user of its own: a helper can inject into any VM whose
          socket admits its uid'';
        helperUid = mkOption {
          type = types.ints.positive;
          description = ''
            The uid of this VM's capture helper (`--inject-uid`), the one
            process that may hand the backend screen-share buffers for it.
            Not the backend's, the VMM's or the desktop user's uid.
          '';
        };
        helperGroup = mkOption {
          # A group name and nothing else: it reaches the socket helper's
          # command line through the unit's environment.
          type = types.strMatching "^[a-z_][a-z0-9_-]*$";
          description = "The group the inject socket is opened to: the helper's own.";
        };
      };
      autoStart = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start this VM's backend at boot (multi-user.target). Usually the
          VMM's unit pulls it in instead, with `requires` and `after`.
        '';
      };
    };
  };
in
{
  options.services.virtio-nvgpu = {
    enable = mkEnableOption "the virtio-nvgpu vhost-user backend, one unit and one host user per VM";

    package = mkOption {
      type = types.package;
      description = "The package whose bin/vhost-user-nvgpu is run.";
    };

    slots = mkOption {
      type = types.ints.between 1 256;
      default = 4;
      description = ''
        How many VMs may run at once: slot N gets the users nvgpu-vmN (the
        backend) and nvgpu-vmmN (the VMM), in the group nvgpu-vmN.
      '';
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = "Backend flags for every VM.";
    };

    vms = mkOption {
      type = types.attrsOf (types.submodule vmOptions);
      default = { };
      example = {
        "0".extraArgs = [ "--allow-compute" ];
      };
      description = "Per-slot settings, keyed by slot number.";
    };

    memoryMax = mkOption {
      type = types.str;
      default = "2G";
      description = ''
        MemoryMax of each backend's cgroup. What the backend holds itself is
        mostly Wayland shm (`--wayland-shm-budget`, 1 GiB by default) and
        queued compositor output (`--wayland-queue-budget`, 256 MiB); raise
        this with them. Guest RAM is the VMM's.
      '';
    };

    tasksMax = mkOption {
      type = types.ints.positive;
      default = 256;
      description = "TasksMax of each backend's cgroup.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.all (n: lib.elem n ids) (lib.attrNames cfg.vms);
        message = "services.virtio-nvgpu.vms names a slot at or above services.virtio-nvgpu.slots (${toString cfg.slots})";
      }
      {
        assertion = lib.all (vm: lib.all (a: builtins.match ".*[[:space:]].*" a == null) vm.extraArgs) (
          lib.attrValues cfg.vms
        );
        message = "services.virtio-nvgpu.vms.<n>.extraArgs: give each flag and value as a word of its own, with no whitespace in it";
      }
      {
        assertion =
          let
            uids = map (vm: vm.inject.helperUid) (lib.filter (vm: vm.inject.enable) (lib.attrValues cfg.vms));
          in
          lib.length uids == lib.length (lib.unique uids);
        message = "services.virtio-nvgpu.vms.<n>.inject.helperUid: each VM needs a capture helper user of its own";
      }
      {
        # Not the VM's backend or VMM user, where their uids are fixed
        # (the backend refuses its own uid itself, at start).
        assertion = lib.all (
          n:
          let
            vm = cfg.vms.${n};
            fixed = u: if config.users.users ? ${u} then config.users.users.${u}.uid else null;
          in
          !vm.inject.enable
          || !(lib.elem vm.inject.helperUid [
            (fixed "nvgpu-vm${n}")
            (fixed "nvgpu-vmm${n}")
          ])
        ) (lib.attrNames cfg.vms);
        message = "services.virtio-nvgpu.vms.<n>.inject.helperUid: the capture helper must not be the VM's backend or VMM user";
      }
      {
        # The backend refuses any other size at start; say it here, where
        # the option was set.
        assertion = lib.all (vm: vm.windowMiB == null || lib.mod vm.windowMiB 64 == 0) (
          lib.attrValues cfg.vms
        );
        message = "services.virtio-nvgpu.vms.<n>.windowMiB: a multiple of 64 MiB";
      }
      {
        # The display paths and the semaphore-surface fences need NVKMS.
        assertion =
          !(lib.elem "nvidia" config.services.xserver.videoDrivers)
          || config.hardware.nvidia.modesetting.enable;
        message = "virtio-nvgpu needs nvidia_drm.modeset=1: hardware.nvidia.modesetting.enable must not be false";
      }
    ];

    users.groups = listToAttrs (map (i: nameValuePair "nvgpu-vm${i}" { }) ids);
    users.users =
      listToAttrs (
        map (
          i:
          nameValuePair "nvgpu-vm${i}" {
            isSystemUser = true;
            group = "nvgpu-vm${i}";
            description = "virtio-nvgpu backend, VM slot ${i}";
          }
        ) ids
      )
      // listToAttrs (
        map (
          i:
          nameValuePair "nvgpu-vmm${i}" {
            isSystemUser = true;
            group = "nvgpu-vm${i}";
            description = "virtio-nvgpu VMM, VM slot ${i}";
          }
        ) ids
      );

    systemd.services = {
      "vhost-user-nvgpu@" = {
        description = "virtio-nvgpu vhost-user backend for VM %i";
        after = [ "systemd-modules-load.service" ];
        environment.RUST_LOG = lib.mkDefault "warn";
        serviceConfig = {
          Type = "exec";
          User = "nvgpu-vm%i";
          Group = "nvgpu-vm%i";
          SupplementaryGroups = [
            "video"
            "render"
            "kvm"
          ];
          ExecStart = "${lib.getExe' cfg.package "vhost-user-nvgpu"} --socket /run/nvgpu/vm%i/nvgpu.sock ${escapeShellArgs cfg.extraArgs} $NVGPU_BACKEND_ARGS";
          # $NVGPU_INJECT_GROUP, unset, is no argument at all.
          ExecStartPost = "+${socketOpen} /run/nvgpu/vm%i nvgpu-vm%i $NVGPU_INJECT_GROUP";
          RuntimeDirectory = "nvgpu/vm%i";
          RuntimeDirectoryMode = "0700";
          UMask = "0077";

          Restart = "no";
          TimeoutStopSec = 10;

          MemoryMax = cfg.memoryMax;
          MemorySwapMax = 0;
          TasksMax = cfg.tasksMax;
          OOMScoreAdjust = 500;
          LimitCORE = 0;

          NoNewPrivileges = true;
          CapabilityBoundingSet = "";
          AmbientCapabilities = "";
          RestrictSUIDSGID = true;
          LockPersonality = true;
          RestrictRealtime = true;
          SystemCallArchitectures = "native";
          MemoryDenyWriteExecute = true;
          RestrictAddressFamilies = [
            "AF_UNIX"
            "AF_NETLINK"
          ];

          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          PrivateIPC = true;
          PrivateNetwork = true;
          ProtectKernelTunables = true;
          ProtectKernelModules = true;
          ProtectControlGroups = true;
          ProtectProc = "invisible";
        };
      };
    }
    // mapAttrs' (
      n: vm:
      nameValuePair "vhost-user-nvgpu@${n}" {
        overrideStrategy = "asDropin";
        # systemd splits an unbraced $VAR at whitespace and takes quotes in
        # it literally: hence one word per flag, and no spaces in any.
        environment = {
          NVGPU_BACKEND_ARGS = concatStringsSep " " (
            vm.extraArgs
            ++ lib.optionals (vm.windowMiB != null) [
              "--window-size"
              (toString vm.windowMiB)
            ]
            ++ lib.optionals (vm.windowOwnerShare != null) [
              "--window-owner-share"
              (toString vm.windowOwnerShare)
            ]
            ++ lib.optionals vm.inject.enable [
              "--inject-socket"
              "/run/nvgpu/vm${n}/inject.sock"
              "--inject-uid"
              (toString vm.inject.helperUid)
            ]
          );
        }
        // lib.optionalAttrs vm.inject.enable {
          # A name `helperGroup`'s type holds to [a-z0-9_-]: one word.
          NVGPU_INJECT_GROUP = vm.inject.helperGroup;
        };
        wantedBy = lib.optional vm.autoStart "multi-user.target";
      }
    ) cfg.vms;
  };
}
