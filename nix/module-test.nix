# SPDX-License-Identifier: Apache-2.0
#
# nix/module.nix, evaluated (the root flake's checks.module-eval; scripts/ci.sh
# fast builds it):
#
# - a configuration that uses every option passes the module's assertions;
#   the units it installs say what contrib/systemd's say (nix/unit-diff.py),
#   and its drop-ins set only what is per slot, no hardening key;
# - each configuration the module must refuse is refused, by the assertion
#   meant to (the case names what its message must say).
#
# Nothing is built but the comparison: the package is a placeholder whose
# path alone reaches the units.
{
  nixpkgs,
  system,
  module,
}:
let
  pkgs = nixpkgs.legacyPackages.${system};
  inherit (nixpkgs) lib;

  placeholder = pkgs.runCommand "vhost-user-nvgpu-placeholder" { } "mkdir -p $out/bin";
  base = {
    services.virtio-nvgpu = {
      enable = true;
      package = placeholder;
      slots = 4;
    };
    system.stateVersion = "25.11";
    boot.loader.grub.enable = false;
    fileSystems."/" = {
      device = "none";
      fsType = "tmpfs";
    };
  };
  system' =
    extra:
    lib.nixosSystem {
    inherit system;
    modules = [
      module
      base
      extra
    ];
  };
  eval = extra: (system' extra).config;
  # The module's own assertions that do not hold: those it defines, taken
  # from the option's definitions (the other NixOS modules' assertions are
  # not this test's business, and some do not evaluate in a configuration
  # this bare).
  failing =
    config':
    let
      ours = lib.concatMap (d: d.value) (
        lib.filter (d: lib.hasSuffix "nix/module.nix" (toString d.file)) (
          config'.options.assertions.definitionsWithLocations
        )
      );
    in
    map (a: a.message) (lib.filter (a: !a.assertion) ours);

  # A capture helper of its own, a login user, and a VM using every option.
  helper = {
    users.groups.nvgpu-cap0 = { };
    users.users.nvgpu-cap0 = {
      isSystemUser = true;
      uid = 950;
      group = "nvgpu-cap0";
    };
    users.users.alice = {
      isNormalUser = true;
      uid = 1000;
    };
  };
  vm0 = extra: {
    services.virtio-nvgpu.vms."0" = lib.recursiveUpdate {
      extraArgs = [ "--allow-compute" ];
      windowMiB = 16384;
      windowOwnerShare = 90;
      backendCpus = "0-7,16-23";
      wayland = {
        socket = "/run/user/1000/wayland-1";
        lease = true;
      };
      inject = {
        enable = true;
        helperUid = 950;
        helperGroup = "nvgpu-cap0";
      };
    } extra;
  };
  goodSystem = system' {
    imports = [
      helper
      (vm0 { })
      {
        services.virtio-nvgpu.extraArgs = [
          "--queue-poll-us"
          "50"
        ];
      }
    ];
  };
  good = goodSystem.config;

  refused = {
    "helper is VM 0's VMM" = {
      config = [
        helper
        (vm0 { inject.helperUid = 64001; })
      ];
      says = "not a uid of the pool";
    };
    "helper is another VM's backend" = {
      config = [
        helper
        (vm0 { inject.helperUid = 64002; })
      ];
      says = "not a uid of the pool";
    };
    "helper is the desktop user" = {
      config = [
        helper
        (vm0 { inject.helperUid = 1000; })
      ];
      says = "not a login user's";
    };
    "helper group missing" = {
      config = [
        helper
        (vm0 { inject.helperGroup = "nvgpu-cap9"; })
      ];
      says = "a group of this configuration's own";
    };
    "helper group is a pool group" = {
      config = [
        helper
        (vm0 { inject.helperGroup = "nvgpu-vm1"; })
      ];
      says = "not a pool group";
    };
    "helper group is wheel" = {
      config = [
        helper
        (vm0 { inject.helperGroup = "wheel"; })
      ];
      says = "not a shared one";
    };
    "helper group has another member" = {
      config = [
        helper
        (vm0 { })
        { users.users.alice.extraGroups = [ "nvgpu-cap0" ]; }
      ];
      says = "every member of the helper's group must be the helper";
    };
    "two VMs, one helper" = {
      config = [
        helper
        (vm0 { })
        {
          services.virtio-nvgpu.vms."1".inject = {
            enable = true;
            helperUid = 950;
            helperGroup = "nvgpu-cap0";
          };
        }
      ];
      says = "a capture helper user, and group, of its own";
    };
    "a stranger in a slot's group" = {
      config = [
        helper
        (vm0 { })
        { users.users.alice.extraGroups = [ "nvgpu-vm0" ]; }
      ];
      says = "is in group nvgpu-vmN";
    };
    "a pool user with extra groups" = {
      config = [
        helper
        (vm0 { })
        { users.users.nvgpu-vmm0.extraGroups = [ "video" ]; }
      ];
      says = "must have no extraGroups";
    };
    "a uid of the pool taken" = {
      config = [
        helper
        (vm0 { })
        {
          users.users.other = {
            isSystemUser = true;
            uid = 64003;
            group = "nogroup";
          };
        }
      ];
      says = "another user has a uid of the pool";
    };
    "a slot past slots" = {
      config = [ { services.virtio-nvgpu.vms."7".extraArgs = [ ]; } ];
      says = "names a slot at or above";
    };
    "a flag with a space" = {
      config = [ { services.virtio-nvgpu.vms."0".extraArgs = [ "--window-size 2048" ]; } ];
      says = "no whitespace in it";
    };
    "a global flag with a space" = {
      config = [ { services.virtio-nvgpu.extraArgs = [ "--queue-poll-us 50" ]; } ];
      says = "no whitespace in it";
    };
    "a flag with a quote" = {
      config = [ { services.virtio-nvgpu.vms."0".extraArgs = [ "\"--allow-compute" ]; } ];
      says = "no quote, backslash or %";
    };
    "a flag with a specifier" = {
      config = [ { services.virtio-nvgpu.extraArgs = [ "--queue-poll-us=%i" ]; } ];
      says = "no quote, backslash or %";
    };
    "a Wayland socket with a colon" = {
      config = [
        helper
        (vm0 { wayland.socket = "/run/user/1000/wayland-1:/etc"; })
      ];
      says = "no quote, backslash, % or colon";
    };
    "a window not of 64 MiB steps" = {
      config = [ { services.virtio-nvgpu.vms."0".windowMiB = 1000; } ];
      says = "a multiple of 64 MiB";
    };
    "backend CPUs with a space" = {
      config = [ { services.virtio-nvgpu.vms."0".backendCpus = "8 9"; } ];
      says = "digits, commas and ranges";
    };
    "backend CPUs as all" = {
      config = [ { services.virtio-nvgpu.vms."0".backendCpus = "all"; } ];
      says = "digits, commas and ranges";
    };
  };
  check =
    name: c:
    let
      msgs = failing (system' { imports = c.config; });
    in
    lib.any (m: lib.hasInfix c.says m) msgs
    || throw "module-eval: \"${name}\" was not refused by an assertion saying \"${c.says}\" (the module said: ${
      if msgs == [ ] then "nothing" else lib.concatStringsSep "; " msgs
    })";
  goodFails = failing goodSystem;

  unit = n: builtins.unsafeDiscardStringContext good.systemd.units.${n}.text;
  # The units the module installs (nix/units.nix).
  units = lib.findFirst (
    p: (p.name or "") == "virtio-nvgpu-units"
  ) (throw "module-eval: the module installs no virtio-nvgpu-units") good.systemd.packages;
  # What a backend drop-in may set (unit-diff.py --dropin).
  dropinKeys = lib.concatStringsSep "," [
    "Unit:Requires=vhost-user-nvgpu-inject@0.socket"
    "Service:EnvironmentFile="
    "Service:MemoryMax"
    "Service:TasksMax"
    "Service:CPUAffinity"
    "Service:ProtectHome=tmpfs"
    "Service:BindReadOnlyPaths"
    "env:NVGPU_BACKEND_ARGS"
    "env:NVGPU_WAYLAND_ARGS"
  ];
in
assert
  goodFails == [ ]
  || throw "module-eval: the full configuration fails: ${lib.concatStringsSep "; " goodFails}";
assert lib.all (x: x) (lib.mapAttrsToList check refused);
pkgs.runCommand "virtio-nvgpu-module-eval"
  {
    nativeBuildInputs = [ pkgs.python3 ];
    dropin = unit "vhost-user-nvgpu@0.service";
    # A slot with no vms.<n>: the global settings reach it too.
    dropin1 = unit "vhost-user-nvgpu@1.service";
    inject = unit "vhost-user-nvgpu-inject@0.socket";
    passAsFile = [
      "dropin"
      "dropin1"
      "inject"
    ];
  }
  ''
    d=${../contrib/systemd}
    u=${units}/lib/systemd/system
    # The drop-in check refuses what it must (a second assignment on an
    # Environment= line, a continuation, a key or value off the list).
    python3 ${./unit-diff.py} --self-test
    for f in vhost-user-nvgpu@.service vhost-user-nvgpu@.socket vhost-user-nvgpu-inject@.socket; do
      python3 ${./unit-diff.py} $d/$f $u/$f
    done
    grep -q '^ExecStart=${placeholder}/bin/vhost-user-nvgpu ' $u/vhost-user-nvgpu@.service
    grep -q '^ExecStartPre=+${units}/libexec/virtio-nvgpu/nvgpu-pci-snapshot ' $u/vhost-user-nvgpu@.service
    test -x ${units}/libexec/virtio-nvgpu/nvgpu-pci-snapshot
    python3 ${./unit-diff.py} --dropin "$dropinPath" ${dropinKeys}
    python3 ${./unit-diff.py} --dropin "$dropin1Path" ${dropinKeys}
    python3 ${./unit-diff.py} --dropin "$injectPath" Socket:SocketGroup=nvgpu-cap0
    # The per-VM drop-in carries what the options asked for.
    grep -q -- 'NVGPU_BACKEND_ARGS=--queue-poll-us 50 --allow-compute --window-size 16384' "$dropinPath"
    grep -q -- 'NVGPU_BACKEND_ARGS=--queue-poll-us 50"' "$dropin1Path"
    grep -q -- '--inject-uid 950' "$dropinPath"
    grep -q -- 'NVGPU_WAYLAND_ARGS=--wayland-socket /run/user/1000/wayland-1 --wayland-lease' "$dropinPath"
    grep -q 'ProtectHome=tmpfs' "$dropinPath"
    grep -q 'BindReadOnlyPaths=/run/user/1000/wayland-1' "$dropinPath"
    grep -q 'Requires=vhost-user-nvgpu-inject@0.socket' "$dropinPath"
    grep -q '^MemoryMax=2G' "$dropin1Path"
    # Placement only where asked: slot 0's backend on its CPUs, slot 1's
    # wherever the host puts it.
    grep -q '^CPUAffinity=0-7,16-23$' "$dropinPath"
    ! grep -q 'CPUAffinity' "$dropin1Path"
    touch $out
  ''
