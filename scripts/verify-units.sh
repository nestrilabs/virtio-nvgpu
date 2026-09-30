#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# `systemd-analyze verify` over contrib/systemd's units, as instances of
# slot 0, in a scratch copy whose binaries are ones this host has (the units
# name /usr/bin/..., which a build host need not have). Checks their syntax,
# the directives this systemd knows and the socket units' dependencies; not
# what they do at run time.
#
# Usage: scripts/verify-units.sh   (exit 0 clean)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
command -v systemd-analyze >/dev/null || { echo "verify-units: no systemd-analyze" >&2; exit 1; }
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
TRUE=$(type -P true)
CHRT=$(type -P chrt || echo "$TRUE")
# systemd's own units (sysinit.target and the like), where this systemd has
# them: its package's lib/systemd/system, or example/ on NixOS.
SD=$(dirname "$(readlink -f "$(type -P systemd-analyze)")")/..
SYS_UNITS=$SD/lib/systemd/system:$SD/example/systemd/system
for f in "$ROOT"/contrib/systemd/*@.service "$ROOT"/contrib/systemd/*@.socket; do
    sed -e "s|/usr/bin/vhost-user-nvgpu|$TRUE|g" -e "s|/usr/bin/crosvm|$TRUE|g" \
        -e "s|/usr/libexec/virtio-nvgpu/nvgpu-pci-snapshot|$TRUE|g" \
        -e "s|/usr/libexec/virtio-nvgpu/nvgpu-vmm-exec|$TRUE|g" \
        -e "s|/usr/libexec/virtio-nvgpu/nvgpu-cpu-latency|$TRUE|g" \
        -e "s|/usr/bin/chrt|$CHRT|g" "$f" > "$W/$(basename "$f")"
done
units=()
for f in "$W"/*@.*; do
    b=$(basename "$f")
    units+=("$W/${b/@./@0.}")
done
cd "$W"
# Instances by path: systemd-analyze loads the template beside each.
out=$(SYSTEMD_UNIT_PATH="$W:$SYS_UNITS:" systemd-analyze verify --man=no "${units[@]}" 2>&1) || rc=$?
# A build host without the pool's users, groups or files is not a defect of
# the units.
out=$(printf '%s\n' "$out" | grep -v -E "Failed to resolve (user|group)|EnvironmentFile|Unit configured to use KillMode=none|^$" || true)
if [ -n "$out" ] || [ "${rc:-0}" != 0 ]; then
    printf '%s\n' "$out" >&2
    echo "verify-units: systemd-analyze verify found the above (exit ${rc:-0})" >&2
    exit 1
fi
echo "verify-units: ${#units[@]} units verified" >&2
