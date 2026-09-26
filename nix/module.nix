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
#   };
#
# Slot N is two system users: nvgpu-vmN runs the backend
# (vhost-user-nvgpu@N.service), and nvgpu-vmmN, in group nvgpu-vmN and no
# other, is for the VMM, which connects to /run/nvgpu/vmN/nvgpu.sock. The unit
# is contrib/systemd/vhost-user-nvgpu@.service, said in Nix; keep the two in
# step. Nothing here starts a VMM: give its unit
#   requires = [ "vhost-user-nvgpu@N.service" ]; after = [ same ];
# and User = "nvgpu-vmmN".
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
          ExecStartPost = "+${socketOpen} /run/nvgpu/vm%i nvgpu-vm%i";
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
        environment.NVGPU_BACKEND_ARGS = concatStringsSep " " vm.extraArgs;
        wantedBy = lib.optional vm.autoStart "multi-user.target";
      }
    ) cfg.vms;
  };
}
