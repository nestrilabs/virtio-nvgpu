# SPDX-License-Identifier: Apache-2.0
#
# nix/module.nix, evaluated (the root flake's checks.module-eval; scripts/ci.sh
# fast builds it):
#
# - a configuration that uses every option passes the module's assertions;
#   the units it installs say what contrib/systemd's say (nix/unit-diff.py),
#   and its drop-ins set only what is per slot, no hardening key;
# - each configuration the module must refuse is refused, by the assertion
#   meant to (the case names what its message must say), and each value an
#   option's type must refuse is (typeRefused);
# - the tuning knobs reach the backend's and the VMMs' drop-ins as they
#   should, and a slot left at its defaults has the drop-in it had.
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
  crosvmPkg = pkgs.runCommand "crosvm-placeholder" { } "mkdir -p $out/bin";
  nesboxPkg = pkgs.runCommand "nesbox-placeholder" { } "mkdir -p $out/bin";
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
      vramLimitMiB = 8192;
      rmAllowGroups = [
        "thermal"
        "debug"
      ];
      osdescPopulate = true;
      dmabufExport = true;
      wayland = {
        socket = "/run/user/1000/wayland-1";
        lease = true;
      };
      inject = {
        enable = true;
        helperUid = 950;
        helperGroup = "nvgpu-cap0";
      };
      queuePollUs = 10;
      sliceUs = 200;
      cpuQuota = "50%";
      fifoDisable = {
        procRate = 10;
        procBurst = 20;
        vmRate = 400;
        vmBurst = 320;
      };
      vmm = {
        kind = "crosvm";
        crosvmArgs = [
          "--cpus"
          "4"
          "--mem"
          "4096"
          "/var/lib/virtio-nvgpu/vmlinux"
        ];
        coreScheduling = "shared";
        prefaultMemory = false;
        guestMemMiB = 4096;
        diskMiB = 8192;
        limitFSizeMiB = 16384;
        cpuLatencyUs = 50;
      };
    } extra;
  };
  # A nesbox VM with a preset window and its own cookie.
  vm2 = {
    services.virtio-nvgpu.vms."2" = {
      windowPreset = "creative";
      vmm = {
        kind = "nesbox";
        nesboxConfig = "/etc/virtio-nvgpu/vm2.json";
        jailRoot = "/var/lib/virtio-nvgpu/jail";
        coreScheduling = "vm";
      };
    };
  };
  vmmPackages = {
    services.virtio-nvgpu.vmmPackages = {
      crosvm = crosvmPkg;
      nesbox = nesboxPkg;
    };
  };
  goodSystem = system' {
    imports = [
      helper
      (vm0 { })
      vm2
      vmmPackages
      {
        services.virtio-nvgpu.extraArgs = [
          "--wayland-max-conns"
          "32"
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
    "a debug group without compute" = {
      config = [ { services.virtio-nvgpu.vms."1".rmAllowGroups = [ "debug" ]; } ];
      says = "debug and profiling need --allow-compute";
    };
    "a window not of 64 MiB steps" = {
      config = [ { services.virtio-nvgpu.vms."0".windowMiB = 1000; } ];
      says = "a multiple of 64 MiB";
    };
    "a video memory limit below the backend's" = {
      config = [ { services.virtio-nvgpu.vms."0".vramLimitMiB = 32; } ];
      says = "at least 64 MiB";
    };
    "a flag an option renders, in extraArgs too" = {
      config = [
        {
          services.virtio-nvgpu.extraArgs = [
            "--queue-poll-us"
            "50"
          ];
          services.virtio-nvgpu.vms."0".queuePollUs = 10;
        }
      ];
      says = "the backend refuses a flag given twice";
    };
    "a window flag in a VM's extraArgs and its option" = {
      config = [
        {
          services.virtio-nvgpu.vms."0" = {
            extraArgs = [ "--window-size=2048" ];
            windowPreset = "creative";
          };
        }
      ];
      says = "the backend refuses a flag given twice";
    };
    "the video memory limit in extraArgs and its option" = {
      config = [
        {
          services.virtio-nvgpu.extraArgs = [ "--vram-limit=4096" ];
          services.virtio-nvgpu.vms."0".vramLimitMiB = 8192;
        }
      ];
      says = "the backend refuses a flag given twice";
    };
    "dma-buf export in a VM's extraArgs and its option" = {
      config = [
        {
          services.virtio-nvgpu.vms."0" = {
            extraArgs = [ "--allow-dmabuf-export" ];
            dmabufExport = true;
          };
        }
      ];
      says = "the backend refuses a flag given twice";
    };
    "RM groups in extraArgs and the option" = {
      config = [
        {
          services.virtio-nvgpu.vms."0" = {
            extraArgs = [
              "--rm-allow-group"
              "health"
            ];
            rmAllowGroups = [ "thermal" ];
          };
        }
      ];
      says = "the backend refuses a flag given twice";
    };
    "a preset and a size" = {
      config = [
        {
          services.virtio-nvgpu.vms."0" = {
            windowMiB = 2048;
            windowPreset = "creative";
          };
        }
      ];
      says = "a preset or windowMiB, not both";
    };
    "a process's disable rate at the VM's" = {
      config = [ { services.virtio-nvgpu.vms."0".fifoDisable.procRate = 200; } ];
      says = "procRate below vmRate";
    };
    "a process's disable burst past the VM's less its reserve" = {
      config = [ { services.virtio-nvgpu.vms."0".fifoDisable.procBurst = 121; } ];
      says = "procRate below vmRate";
    };
    "a process's disable burst within the reserve's floor" = {
      config = [ { services.virtio-nvgpu.vms."0".fifoDisable.procBurst = 8; } ];
      says = "procRate below vmRate";
    };
    "a VMM knob with no VMM" = {
      config = [ { services.virtio-nvgpu.vms."0".vmm.coreScheduling = "off"; } ];
      says = "set vmm.kind";
    };
    "a C-state cap with no VMM" = {
      config = [ { services.virtio-nvgpu.vms."0".vmm.cpuLatencyUs = 50; } ];
      says = "set vmm.kind";
    };
    "crosvm with no package" = {
      config = [
        {
          services.virtio-nvgpu.vms."0".vmm = {
            kind = "crosvm";
            crosvmArgs = [ "/k" ];
          };
        }
      ];
      says = "vmmPackages.crosvm, vmmPackages.nesbox) is not set";
    };
    "crosvm with no arguments" = {
      config = [
        vmmPackages
        { services.virtio-nvgpu.vms."0".vmm.kind = "crosvm"; }
      ];
      says = "crosvm takes crosvmArgs";
    };
    "nesbox with no config" = {
      config = [
        vmmPackages
        { services.virtio-nvgpu.vms."0".vmm.kind = "nesbox"; }
      ];
      says = "nesbox nesboxConfig and jailRoot";
    };
    "nesbox per vCPU" = {
      config = [
        vmmPackages
        vm2
        { services.virtio-nvgpu.vms."2".vmm.coreScheduling = lib.mkForce "per-vcpu"; }
      ];
      says = "nesbox makes no core-scheduling cookie per vCPU";
    };
    "nesbox without prefault" = {
      config = [
        vmmPackages
        vm2
        { services.virtio-nvgpu.vms."2".vmm.prefaultMemory = false; }
      ];
      says = "its prefault is its config's";
    };
    "a crosvm argument with a space" = {
      config = [
        vmmPackages
        {
          services.virtio-nvgpu.vms."0".vmm = {
            kind = "crosvm";
            crosvmArgs = [ "--mem 4096" ];
          };
        }
      ];
      says = "one word each";
    };
    "a file size limit with nothing to hold it to" = {
      config = [
        vmmPackages
        vm2
        { services.virtio-nvgpu.vms."2".vmm.limitFSizeMiB = 65536; }
      ];
      says = "give guestMemMiB and diskMiB too";
    };
    "a file size limit below the disk" = {
      config = [
        vmmPackages
        vm2
        {
          services.virtio-nvgpu.vms."2".vmm = {
            limitFSizeMiB = 8192;
            guestMemMiB = 4096;
            diskMiB = 10000;
          };
        }
      ];
      says = "at least the largest of the disk";
    };
    "a file size limit below guest RAM" = {
      config = [
        vmmPackages
        vm2
        {
          services.virtio-nvgpu.vms."2".vmm = {
            limitFSizeMiB = 9000;
            guestMemMiB = 16384;
            diskMiB = 4096;
          };
        }
      ];
      says = "at least the largest of the disk";
    };
    "a file size limit below the preset's window" = {
      config = [
        vmmPackages
        vm2
        {
          services.virtio-nvgpu.vms."2".vmm = {
            limitFSizeMiB = 6000;
            guestMemMiB = 4096;
            diskMiB = 4096;
          };
        }
      ];
      says = "at least the largest of the disk";
    };
  };
  # Values an option's type refuses, before any assertion: that option of
  # vms."0", read, must throw, and read with the value beside it (a good
  # one) must not -- so that the case fails for its value, not for
  # something else of the configuration.
  typeRefused = {
    "a poll past 1000 us" = [ [ "queuePollUs" ] 1001 1000 ];
    "a slice below 100 us" = [ [ "sliceUs" ] 50 0 ];
    "a slice past 100 ms" = [ [ "sliceUs" ] 100001 100000 ];
    "a CPU quota that is not a percentage" = [ [ "cpuQuota" ] "50" "50%" ];
    "a disable rate past 1000" = [ [ "fifoDisable" "vmRate" ] 1001 1000 ];
    "a disable rate of 0" = [ [ "fifoDisable" "procRate" ] 0 1 ];
    "a preset it does not know" = [ [ "windowPreset" ] "huge" "creative" ];
    "a core-scheduling mode it does not know" = [ [ "vmm" "coreScheduling" ] "sometimes" "shared" ];
    "a C-state cap past 100 ms" = [ [ "vmm" "cpuLatencyUs" ] 100001 0 ];
    "a VMM it does not know" = [ [ "vmm" "kind" ] "qemu" "nesbox" ];
  };
  checkType =
    name: c:
    let
      path = lib.elemAt c 0;
      read =
        v:
        (builtins.tryEval (
          builtins.deepSeq (lib.getAttrFromPath path
            (eval {
              services.virtio-nvgpu.vms."0" = lib.setAttrByPath path v;
            }).services.virtio-nvgpu.vms."0"
          ) true
        )).success;
    in
    (!(read (lib.elemAt c 1)) || throw "module-eval: \"${name}\" was accepted; its option's type must refuse it")
    && (read (lib.elemAt c 2) || throw "module-eval: \"${name}\": the good value beside it was refused too");
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
    "Service:CPUQuota"
    "Service:ProtectHome=tmpfs"
    "Service:BindReadOnlyPaths"
    "env:NVGPU_BACKEND_ARGS"
    "env:NVGPU_WAYLAND_ARGS"
  ];
  # What a VMM drop-in may set: the VMM's arguments, its knobs, its file
  # size limit and the C-state cap it wants (vm 0's, 50 us).
  vmmDropinKeys = lib.concatStringsSep "," [
    "Unit:Wants=nvgpu-cpu-latency@50.service"
    "Unit:After=nvgpu-cpu-latency@50.service"
    "Service:EnvironmentFile="
    "Service:LimitFSIZE"
    "env:NVGPU_SLICE_US"
    "env:NVGPU_CORE_SCHED"
    "env:NVGPU_PREFAULT"
    "env:NVGPU_CROSVM_ARGS"
    "env:NVGPU_VMM_CONFIG"
    "env:NVGPU_JAIL_ROOT"
  ];
in
assert
  goodFails == [ ]
  || throw "module-eval: the full configuration fails: ${lib.concatStringsSep "; " goodFails}";
assert lib.all (x: x) (lib.mapAttrsToList check refused);
assert lib.all (x: x) (lib.mapAttrsToList checkType typeRefused);
pkgs.runCommand "virtio-nvgpu-module-eval"
  {
    nativeBuildInputs = [ pkgs.python3 ];
    dropin = unit "vhost-user-nvgpu@0.service";
    # A slot with no vms.<n>: the global settings reach it too.
    dropin1 = unit "vhost-user-nvgpu@1.service";
    inject = unit "vhost-user-nvgpu-inject@0.socket";
    vmm0 = unit "nvgpu-vmm-crosvm@0.service";
    vmm2 = unit "nvgpu-vmm-nesbox@2.service";
    dropin2 = unit "vhost-user-nvgpu@2.service";
    passAsFile = [
      "dropin"
      "dropin1"
      "dropin2"
      "inject"
      "vmm0"
      "vmm2"
    ];
  }
  ''
    d=${../contrib/systemd}
    u=${units}/lib/systemd/system
    # The drop-in check refuses what it must (a second assignment on an
    # Environment= line, a continuation, a key or value off the list).
    python3 ${./unit-diff.py} --self-test
    for f in vhost-user-nvgpu@.service vhost-user-nvgpu@.socket vhost-user-nvgpu-inject@.socket \
      nvgpu-vmm-crosvm@.service nvgpu-vmm-nesbox@.service nvgpu-cpu-latency@.service; do
      python3 ${./unit-diff.py} $d/$f $u/$f
    done
    # The helpers the VMM units and the C-state cap run, from the store.
    grep -q '^ExecStart=${units}/libexec/virtio-nvgpu/nvgpu-vmm-exec crosvm ${crosvmPkg}/bin/crosvm run ' $u/nvgpu-vmm-crosvm@.service
    grep -q '^ExecStartPost=+${units}/libexec/virtio-nvgpu/nvgpu-vmm-exec join ' $u/nvgpu-vmm-crosvm@.service
    grep -q 'nvgpu-vmm-exec nesbox ${nesboxPkg}/bin/jailer ' $u/nvgpu-vmm-nesbox@.service
    grep -q '^ExecStart=${units}/libexec/virtio-nvgpu/nvgpu-cpu-latency %i' $u/nvgpu-cpu-latency@.service
    test -x ${units}/libexec/virtio-nvgpu/nvgpu-vmm-exec
    test -x ${units}/libexec/virtio-nvgpu/nvgpu-cpu-latency
    grep -q '^ExecStart=${placeholder}/bin/vhost-user-nvgpu ' $u/vhost-user-nvgpu@.service
    grep -q '^ExecStartPre=+${units}/libexec/virtio-nvgpu/nvgpu-pci-snapshot ' $u/vhost-user-nvgpu@.service
    test -x ${units}/libexec/virtio-nvgpu/nvgpu-pci-snapshot
    python3 ${./unit-diff.py} --dropin "$dropinPath" ${dropinKeys}
    python3 ${./unit-diff.py} --dropin "$dropin1Path" ${dropinKeys}
    python3 ${./unit-diff.py} --dropin "$dropin2Path" ${dropinKeys}
    python3 ${./unit-diff.py} --dropin "$injectPath" Socket:SocketGroup=nvgpu-cap0
    python3 ${./unit-diff.py} --dropin "$vmm0Path" ${vmmDropinKeys}
    python3 ${./unit-diff.py} --dropin "$vmm2Path" ${vmmDropinKeys}
    # The per-VM drop-in carries what the options asked for.
    grep -q -- 'NVGPU_BACKEND_ARGS=--wayland-max-conns 32 --allow-compute --window-size 16384' "$dropinPath"
    grep -q -- '--rm-allow-group thermal,debug --osdesc-populate on' "$dropinPath"
    if grep -q -- '--rm-allow-group\|--osdesc-populate' "$dropin1Path"; then exit 1; fi
    grep -q -- 'NVGPU_BACKEND_ARGS=--wayland-max-conns 32"' "$dropin1Path"
    grep -q -- '--inject-uid 950' "$dropinPath"
    grep -q -- '--window-owner-share 90 --vram-limit 8192' "$dropinPath"
    grep -q -- '--osdesc-populate on --allow-dmabuf-export' "$dropinPath"
    if grep -q -- '--allow-dmabuf-export' "$dropin1Path"; then exit 1; fi
    grep -q -- 'NVGPU_WAYLAND_ARGS=--wayland-socket /run/user/1000/wayland-1 --wayland-lease' "$dropinPath"
    grep -q 'ProtectHome=tmpfs' "$dropinPath"
    grep -q 'BindReadOnlyPaths=/run/user/1000/wayland-1' "$dropinPath"
    grep -q 'Requires=vhost-user-nvgpu-inject@0.socket' "$dropinPath"
    grep -q '^MemoryMax=2G' "$dropin1Path"
    # The tuning knobs: each where it goes, and none on a slot with the
    # defaults.
    grep -q -- '--queue-poll-us 10 --sched-slice-us 200 --fifo-disable-proc-rate 10 --fifo-disable-proc-burst 20 --fifo-disable-vm-rate 400 --fifo-disable-vm-burst 320' "$dropinPath"
    grep -q '^CPUQuota=50%' "$dropinPath"
    ! grep -q 'CPUQuota\|--sched-slice-us\|--fifo-disable' "$dropin1Path"
    grep -q -- 'NVGPU_BACKEND_ARGS=--wayland-max-conns 32 --window-size 8192"' "$dropin2Path"
    grep -q 'NVGPU_CORE_SCHED=shared' "$vmm0Path"
    grep -q 'NVGPU_PREFAULT=0' "$vmm0Path"
    grep -q 'NVGPU_SLICE_US=200' "$vmm0Path"
    grep -q 'NVGPU_CROSVM_ARGS=--cpus 4 --mem 4096 /var/lib/virtio-nvgpu/vmlinux' "$vmm0Path"
    grep -q '^LimitFSIZE=16384M' "$vmm0Path"
    grep -q '^Wants=nvgpu-cpu-latency@50.service' "$vmm0Path"
    grep -q 'NVGPU_CORE_SCHED=vm' "$vmm2Path"
    grep -q 'NVGPU_SLICE_US=100' "$vmm2Path"
    grep -q 'NVGPU_VMM_CONFIG=/etc/virtio-nvgpu/vm2.json' "$vmm2Path"
    ! grep -q 'LimitFSIZE\|cpu-latency\|NVGPU_PREFAULT' "$vmm2Path"
    touch $out
  ''
