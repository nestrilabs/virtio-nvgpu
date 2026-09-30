# SPDX-License-Identifier: Apache-2.0
#
# The backend's systemd units, as contrib/systemd has them, for a backend in
# the Nix store: vhost-user-nvgpu@.service, vhost-user-nvgpu@.socket and
# vhost-user-nvgpu-inject@.socket in lib/systemd/system, and the PCI
# snapshot helper their ExecStartPre= runs in libexec/virtio-nvgpu. With
# `crosvm` or `nesbox` (a package with bin/jailer), the VMM templates too,
# nvgpu-vmm-crosvm@.service and nvgpu-vmm-nesbox@.service, with the helper
# they exec through (nvgpu-vmm-exec); and always the C-state cap's
# template, nvgpu-cpu-latency@.service, and its helper. The only changes are
# the paths: the binaries', the helpers', and a PATH for each helper.
# nix/module.nix installs these (systemd.packages) and says what is per VM
# in drop-ins; the flake's `units` package is the same for its own backend.
# contrib/systemd stays the one text of the units.
#
#   pkgs.callPackage ./units.nix { backend = <a package with bin/vhost-user-nvgpu>; }
{
  lib,
  runCommand,
  coreutils,
  util-linux,
  getent,
  systemd,
  runtimeShell,
  backend,
  crosvm ? null,
  nesbox ? null,
}:
let
  # A helper's own interpreter and tools: the units run each with systemd's
  # PATH.
  helper = name: path: ''
    install -Dm0755 ${../contrib/systemd}/${name} $lx/${name}
    substituteInPlace $lx/${name} \
      --replace-fail '#!/bin/sh' '#!${runtimeShell}' \
      --replace-fail 'set -eu' 'set -eu
    export PATH=${lib.makeBinPath path}'
  '';
in
runCommand "virtio-nvgpu-units" { } (
  ''
    u=$out/lib/systemd/system
    lx=$out/libexec/virtio-nvgpu
    for f in vhost-user-nvgpu@.service vhost-user-nvgpu@.socket vhost-user-nvgpu-inject@.socket nvgpu-cpu-latency@.service; do
      install -Dm0644 ${../contrib/systemd}/$f $u/$f
    done
    ${helper "nvgpu-pci-snapshot" [ coreutils ]}
    ${helper "nvgpu-cpu-latency" [ coreutils ]}
    substituteInPlace $u/vhost-user-nvgpu@.service \
      --replace-fail /usr/bin/vhost-user-nvgpu ${lib.getExe' backend "vhost-user-nvgpu"} \
      --replace-fail /usr/libexec/virtio-nvgpu/nvgpu-pci-snapshot $lx/nvgpu-pci-snapshot
    substituteInPlace $u/nvgpu-cpu-latency@.service \
      --replace-fail /usr/libexec/virtio-nvgpu/nvgpu-cpu-latency $lx/nvgpu-cpu-latency
  ''
  + lib.optionalString (crosvm != null || nesbox != null) ''
    ${helper "nvgpu-vmm-exec" (
      [
        coreutils
        util-linux
        systemd
      ]
      ++ lib.optional (nesbox != null) nesbox
    )}
  ''
  + lib.optionalString (crosvm != null) ''
    install -Dm0644 ${../contrib/systemd}/nvgpu-vmm-crosvm@.service $u/nvgpu-vmm-crosvm@.service
    substituteInPlace $u/nvgpu-vmm-crosvm@.service \
      --replace-fail /usr/bin/crosvm ${lib.getExe' crosvm "crosvm"} \
      --replace-fail /usr/libexec/virtio-nvgpu/nvgpu-vmm-exec $lx/nvgpu-vmm-exec
  ''
  + lib.optionalString (nesbox != null) ''
    install -Dm0644 ${../contrib/systemd}/nvgpu-vmm-nesbox@.service $u/nvgpu-vmm-nesbox@.service
    substituteInPlace $u/nvgpu-vmm-nesbox@.service \
      --replace-fail /bin/sh ${runtimeShell} \
      --replace-fail 'exec /usr/libexec/virtio-nvgpu/nvgpu-vmm-exec nesbox jailer' \
        'exec '$lx'/nvgpu-vmm-exec nesbox ${lib.getExe' nesbox "jailer"}' \
      --replace-fail '$(id -u' '$(${coreutils}/bin/id -u' \
      --replace-fail '$(getent group' '$(${getent}/bin/getent group' \
      --replace-fail '| cut -d' '| ${coreutils}/bin/cut -d' \
      --replace-fail '+/usr/libexec/virtio-nvgpu/nvgpu-vmm-exec' "+$lx/nvgpu-vmm-exec"
  ''
)
