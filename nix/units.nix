# SPDX-License-Identifier: Apache-2.0
#
# The backend's systemd units, as contrib/systemd has them, for a backend in
# the Nix store: vhost-user-nvgpu@.service, vhost-user-nvgpu@.socket and
# vhost-user-nvgpu-inject@.socket in lib/systemd/system, and the PCI
# snapshot helper their ExecStartPre= runs in libexec/virtio-nvgpu. The only
# changes are the paths: the backend's, the helper's, and a PATH for the
# helper. nix/module.nix installs these (systemd.packages) and says what is
# per VM in drop-ins; the flake's `units` package is the same for its own
# backend. contrib/systemd stays the one text of the units.
#
#   pkgs.callPackage ./units.nix { backend = <a package with bin/vhost-user-nvgpu>; }
{
  lib,
  runCommand,
  coreutils,
  runtimeShell,
  backend,
}:
runCommand "virtio-nvgpu-units" { } ''
  u=$out/lib/systemd/system
  for f in vhost-user-nvgpu@.service vhost-user-nvgpu@.socket vhost-user-nvgpu-inject@.socket; do
    install -Dm0644 ${../contrib/systemd}/$f $u/$f
  done
  snap=$out/libexec/virtio-nvgpu/nvgpu-pci-snapshot
  install -Dm0755 ${../contrib/systemd/nvgpu-pci-snapshot} $snap
  # Its own interpreter and tools: the unit runs it with systemd's PATH.
  substituteInPlace $snap \
    --replace-fail '#!/bin/sh' '#!${runtimeShell}' \
    --replace-fail 'set -eu' 'set -eu
  export PATH=${lib.makeBinPath [ coreutils ]}'
  substituteInPlace $u/vhost-user-nvgpu@.service \
    --replace-fail /usr/bin/vhost-user-nvgpu ${lib.getExe' backend "vhost-user-nvgpu"} \
    --replace-fail /usr/libexec/virtio-nvgpu/nvgpu-pci-snapshot $snap
''
