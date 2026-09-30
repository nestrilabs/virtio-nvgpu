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
# Slot N is two system users with fixed ids: nvgpu-vmN (uid uidBase+2N, the
# backend, vhost-user-nvgpu@N.service) and nvgpu-vmmN (uid uidBase+2N+1, the
# VMM), both in group nvgpu-vmN (gid uidBase+2N) and no other. The VMM
# connects to /run/nvgpu/vmN/nvgpu.sock, which vhost-user-nvgpu@N.socket
# binds, root's and open to that group, and hands to the backend (socket
# activation): the backend's user owns neither the socket nor its directory.
# The units are contrib/systemd's own (nix/units.nix), with what is this
# configuration's in a drop-in per slot; nix/module-test.nix checks that a
# drop-in sets no key but those. Nothing here starts a VMM: give its unit
#   bindsTo = [ "vhost-user-nvgpu@N.service" ]; after = [ same ];
# (bindsTo, not requires: a backend that dies on its own must take the VMM
# with it) and User = "nvgpu-vmmN", or use contrib/systemd's
# nvgpu-vmm-nesbox@.service and nvgpu-vmm-crosvm@.service. For a compute VM
# (--allow-compute), that unit also needs the mincore syscall (not in
# @system-service) and, under a device policy, DeviceAllow "/dev/nvidia-uvm
# w": the VMM checks each UVM pool with mincore, which the kernel answers only
# for a file it may write. A device policy names major 195 "char-nvidia"
# (DEPLOY.md, "Per-VM users").
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
    concatStringsSep
    ;

  cfg = config.services.virtio-nvgpu;
  ids = genList toString cfg.slots;

  # contrib/systemd's units and their helper, for this backend.
  units = pkgs.callPackage ./units.nix { backend = cfg.package; };

  # Slot n's ids: the backend's uid and the group's gid are base+2n, the
  # VMM's uid base+2n+1.
  backendUid = n: cfg.uidBase + 2 * lib.toInt n;
  vmmUid = n: backendUid n + 1;
  poolUids = lib.concatMap (n: [
    (backendUid n)
    (vmmUid n)
  ]) ids;
  poolUsers = lib.concatMap (n: [
    "nvgpu-vm${n}"
    "nvgpu-vmm${n}"
  ]) ids;
  poolGroups = map (n: "nvgpu-vm${n}") ids;

  users = config.users.users;
  groups = config.users.groups;
  # Who is in group g: its primary members and those that add it.
  membersOf =
    g:
    lib.attrNames (lib.filterAttrs (_: u: u.group == g || lib.elem g u.extraGroups) users)
    ++ (groups.${g}.members or [ ]);
  # Groups every login user, or the whole system, may be in: never a
  # helper's.
  sharedGroups = [
    "root"
    "wheel"
    "users"
    "nogroup"
    "video"
    "render"
    "kvm"
    "audio"
    "input"
    "systemd-journal"
    "nixbld"
  ];
  injecting = lib.filterAttrs (_: vm: vm.inject.enable) cfg.vms;
  # Slot n's settings: its vms.<n>, or every option's default.
  vmDefaults = (lib.evalModules { modules = [ vmOptions ]; }).config;
  vmOf = n: cfg.vms.${n} or vmDefaults;
  # A word systemd passes on as it is: an unbraced $VAR in ExecStart= is
  # split at whitespace and each word unquoted (quotes and backslashes taken
  # as quoting), and an Environment= value has its % specifiers expanded.
  plainWord =
    a:
    builtins.match ".*[[:space:]].*" a == null
    && !(lib.any (c: lib.hasInfix c a) [
      "\""
      "'"
      "\\"
      "%"
    ]);
  loginUids = lib.filter (u: u != null) (
    lib.mapAttrsToList (_: u: if u.isNormalUser then u.uid else null) users
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
      vramLimitMiB = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        example = 8192;
        description = ''
          The video memory this VM may allocate, in MiB (`--vram-limit`;
          null, the default, is no limit): an allocation past it is refused
          with RM's own out-of-memory status, and nvidia-smi, NVML, Vulkan
          and CUDA in the guest are told it as the GPU's size. One guest
          process holds at most `windowOwnerShare` percent of it. At least
          64. What RM allocates on the VM's behalf (channels, page tables),
          what nvidia-uvm migrates for CUDA managed memory and nvidia-drm's
          own allocations are not counted: a bound on the VM's ordinary
          workload, not a hard partition (DEPLOY.md, "Video memory limit";
          SECURITY.md, "Video memory limit").
        '';
      };
      wayland = {
        socket = mkOption {
          type = types.nullOr (types.strMatching "^/[^[:space:]]+$");
          default = null;
          example = "/run/user/1000/wayland-1";
          description = ''
            The host compositor's socket, for the Wayland proxy
            (`--wayland-socket`; DEPLOY.md, "The Wayland modes"). The unit
            then hides home directories behind an empty tmpfs and binds this
            one socket read-only. The VM's user (nvgpu-vmN) still needs an
            ACL on the socket and search on its directory, set from the
            desktop session's start-up: the compositor makes the socket anew
            each session.
          '';
        };
        lease = mkOption {
          type = types.bool;
          default = false;
          description = ''
            Offer the compositor's DRM lease device (`--wayland-lease`; needs
            the patched Hyprland of patches/ and a monitor marked leasable).
          '';
        };
      };
      inject = {
        enable = mkEnableOption ''
          capture injection for this VM (SECURITY.md, "Capture injection"): the socket unit
          vhost-user-nvgpu-inject@N.socket binds /run/nvgpu/vmN/inject.sock,
          root's and open to `helperGroup`, and hands it to the backend, which
          serves only `helperUid` there. Give every VM a helper user of its
          own: a helper can inject into any VM whose socket admits its uid'';
        helperUid = mkOption {
          type = types.ints.positive;
          description = ''
            The uid of this VM's capture helper (`--inject-uid`), the one
            process that may hand the backend screen-share buffers for it.
            Not a uid of the pool (any slot's backend or VMM), and not a
            login user's.
          '';
        };
        helperGroup = mkOption {
          # A group name and nothing else.
          type = types.strMatching "^[a-z_][a-z0-9_-]*$";
          description = ''
            The group the inject socket is opened to: the helper's own, a
            group of this configuration's (users.groups) whose only members
            have `helperUid`. Not a pool group and not a shared one (wheel,
            users, video, ...).
          '';
        };
      };
      autoStart = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start this VM's backend at boot (multi-user.target). Usually the
          VMM's unit pulls it in instead, with `bindsTo` and `after`.
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

    uidBase = mkOption {
      # With at most 256 slots, the last id is at most 65511: below nobody.
      type = types.ints.between 1000 65000;
      default = 64000;
      description = ''
        The pool's ids: slot N's backend user and group are uidBase+2N, its
        VMM user uidBase+2N+1. Fixed, so that the assertions that keep a
        capture helper apart from every VM's users can compare them; move it
        if another user of this host has one of those ids (an assertion
        says so).
      '';
    };

    extraArgs = mkOption {
      type = types.listOf types.str;
      default = [ ];
      description = ''
        Backend flags for every VM, before each VM's own
        (`vms.<n>.extraArgs`); one word per flag or value, with no
        whitespace in any.
      '';
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
        # The pool's ids are the pool's alone.
        assertion = lib.all (
          name:
          let
            u = users.${name};
          in
          lib.elem name poolUsers || u.uid == null || !(lib.elem u.uid poolUids)
        ) (lib.attrNames users);
        message = "services.virtio-nvgpu.uidBase: another user has a uid of the pool (${toString cfg.uidBase} to ${toString (cfg.uidBase + 2 * cfg.slots - 1)}); move uidBase";
      }
      {
        # A slot's group holds its two users and no one else: whoever else
        # were in it could connect to the VM's backend socket.
        assertion = lib.all (
          n:
          lib.all (m: m == "nvgpu-vm${n}" || m == "nvgpu-vmm${n}") (membersOf "nvgpu-vm${n}")
        ) ids;
        message = "services.virtio-nvgpu: a user other than nvgpu-vmN and nvgpu-vmmN is in group nvgpu-vmN, which opens VM N's backend socket to it";
      }
      {
        # And they are in no other group (the backend's device groups come
        # from its unit, not its account).
        assertion = lib.all (name: users.${name}.extraGroups == [ ]) poolUsers;
        message = "services.virtio-nvgpu: the pool's users (nvgpu-vmN, nvgpu-vmmN) must have no extraGroups";
      }
      {
        assertion = lib.all plainWord (
          cfg.extraArgs ++ lib.concatMap (vm: vm.extraArgs) (lib.attrValues cfg.vms)
        );
        message = "services.virtio-nvgpu.extraArgs, vms.<n>.extraArgs: give each flag and value as a word of its own, with no whitespace in it, and no quote, backslash or % (systemd would unquote or expand them)";
      }
      {
        # The socket's path goes into the same variable, and into
        # BindReadOnlyPaths=, where a colon would name a second path.
        assertion = lib.all (
          vm:
          vm.wayland.socket == null
          || (plainWord vm.wayland.socket && !(lib.hasInfix ":" vm.wayland.socket))
        ) (lib.attrValues cfg.vms);
        message = "services.virtio-nvgpu.vms.<n>.wayland.socket: a path with no quote, backslash, % or colon";
      }
      {
        assertion =
          let
            uids = map (vm: vm.inject.helperUid) (lib.attrValues injecting);
            gs = map (vm: vm.inject.helperGroup) (lib.attrValues injecting);
          in
          lib.length uids == lib.length (lib.unique uids) && lib.length gs == lib.length (lib.unique gs);
        message = "services.virtio-nvgpu.vms.<n>.inject: each VM needs a capture helper user, and group, of its own";
      }
      {
        # Not any VM's backend or VMM user (the backend refuses its own uid
        # itself, at start), nor a login user's.
        assertion = lib.all (
          vm: !(lib.elem vm.inject.helperUid poolUids) && !(lib.elem vm.inject.helperUid loginUids)
        ) (lib.attrValues injecting);
        message = "services.virtio-nvgpu.vms.<n>.inject.helperUid: the capture helper must be a user of its own: not a uid of the pool (any VM's backend or VMM user) and not a login user's";
      }
      {
        assertion = lib.all (
          vm:
          let
            g = vm.inject.helperGroup;
          in
          groups ? ${g}
          && builtins.match "nvgpu-vmm?[0-9]+" g == null
          && !(lib.elem g sharedGroups)
        ) (lib.attrValues injecting);
        message = "services.virtio-nvgpu.vms.<n>.inject.helperGroup: a group of this configuration's own (users.groups), not a pool group (nvgpu-vmN) and not a shared one (${concatStringsSep ", " sharedGroups})";
      }
      {
        # Whoever is in the helper's group can connect to the inject socket;
        # only the helper should.
        assertion = lib.all (
          vm:
          let
            g = vm.inject.helperGroup;
          in
          !(groups ? ${g})
          || lib.all (m: users ? ${m} && users.${m}.uid == vm.inject.helperUid) (membersOf g)
        ) (lib.attrValues injecting);
        message = "services.virtio-nvgpu.vms.<n>.inject.helperGroup: every member of the helper's group must be the helper (a user with uid = helperUid)";
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
        # The backend refuses a smaller limit at start.
        assertion = lib.all (vm: vm.vramLimitMiB == null || vm.vramLimitMiB >= 64) (
          lib.attrValues cfg.vms
        );
        message = "services.virtio-nvgpu.vms.<n>.vramLimitMiB: at least 64 MiB";
      }
      {
        # The display paths and the semaphore-surface fences need NVKMS.
        assertion =
          !(lib.elem "nvidia" config.services.xserver.videoDrivers)
          || config.hardware.nvidia.modesetting.enable;
        message = "virtio-nvgpu needs nvidia_drm.modeset=1: hardware.nvidia.modesetting.enable must not be false";
      }
    ];

    users.groups = listToAttrs (map (i: nameValuePair "nvgpu-vm${i}" { gid = backendUid i; }) ids);
    users.users =
      listToAttrs (
        map (
          i:
          nameValuePair "nvgpu-vm${i}" {
            isSystemUser = true;
            uid = backendUid i;
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
            uid = vmmUid i;
            group = "nvgpu-vm${i}";
            description = "virtio-nvgpu VMM, VM slot ${i}";
          }
        ) ids
      );

    # The units are contrib/systemd's (nix/units.nix): the backend's
    # template, its vhost-user socket (root's, open to the slot's group,
    # handed to the backend) and the capture helper's socket. What is this
    # configuration's is said per slot below, in drop-ins, and only in the
    # keys nix/module-test.nix allows a drop-in (no hardening key).
    systemd.packages = [ units ];

    systemd.services = listToAttrs (
      map (
        n:
        let
          vm = vmOf n;
        in
        nameValuePair "vhost-user-nvgpu@${n}" (
          {
            overrideStrategy = "asDropin";
            # systemd splits an unbraced $VAR at whitespace and unquotes each
            # word, and expands % specifiers in the value: hence one word per
            # flag, and no space, quote, backslash or % in any (plainWord).
            environment = {
              NVGPU_BACKEND_ARGS = concatStringsSep " " (
                cfg.extraArgs
                ++ vm.extraArgs
                ++ lib.optionals (vm.windowMiB != null) [
                  "--window-size"
                  (toString vm.windowMiB)
                ]
                ++ lib.optionals (vm.windowOwnerShare != null) [
                  "--window-owner-share"
                  (toString vm.windowOwnerShare)
                ]
                ++ lib.optionals (vm.vramLimitMiB != null) [
                  "--vram-limit"
                  (toString vm.vramLimitMiB)
                ]
                ++ lib.optionals vm.inject.enable [
                  "--inject-uid"
                  (toString vm.inject.helperUid)
                ]
              );
              NVGPU_WAYLAND_ARGS = concatStringsSep " " (
                lib.optionals (vm.wayland.socket != null) [
                  "--wayland-socket"
                  vm.wayland.socket
                ]
                ++ lib.optional vm.wayland.lease "--wayland-lease"
              );
            };
            serviceConfig = {
              # The flags are this configuration's alone: the unit's
              # /etc/virtio-nvgpu/vmN.env is not read.
              EnvironmentFile = "";
              MemoryMax = cfg.memoryMax;
              TasksMax = cfg.tasksMax;
            }
            // lib.optionalAttrs (vm.wayland.socket != null) {
              # The one socket, and no other file of any home or runtime
              # directory under /home.
              ProtectHome = "tmpfs";
              BindReadOnlyPaths = [ vm.wayland.socket ];
            };
            wantedBy = lib.optional vm.autoStart "multi-user.target";
          }
          // lib.optionalAttrs vm.inject.enable {
            requires = [ "vhost-user-nvgpu-inject@${n}.socket" ];
          }
        )
      ) ids
    );

    # The capture helper's socket, per VM that has one, opened to the
    # helper's group.
    systemd.sockets = mapAttrs' (
      n: vm:
      nameValuePair "vhost-user-nvgpu-inject@${n}" {
        overrideStrategy = "asDropin";
        socketConfig.SocketGroup = vm.inject.helperGroup;
      }
    ) injecting;
  };
}
