#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Print a vCPU layout for this host (rig/run-guest.sh's NVGPU_PIN;
# DEPLOY.md, "vCPU placement") as each place that takes one says it: the
# launcher's variables, crosvm's options, nesbox's machine-config keys, and
# the systemd CPUAffinity= of the backend's unit (nix/module.nix's
# vms.<n>.backendCpus) and of the VMM's. Runs nothing, needs no privilege.
#
# Usage: rig/pin-layout.sh <vcpus> <layout>[:l3=CPU][:avoid=LIST][:io=WHERE]
#   e.g. rig/pin-layout.sh 8 cores:io=siblings
# NVGPU_SYSFS_CPU reads another sysfs CPU tree (the tests' fake hosts).
set -euo pipefail
[ $# = 2 ] || { sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
die() {
    echo "pin-layout: $*" >&2
    exit 1
}
LIB=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/launcher
# PRIV, VCPUS and NVGPU_PIN are placement.sh's to read.
# shellcheck disable=SC2034
PRIV=user
# shellcheck disable=SC2034
[ "$(id -u)" != 0 ] || PRIV=root
# shellcheck source=launcher/common.sh
. "$LIB/common.sh"
# shellcheck source=launcher/placement.sh
. "$LIB/placement.sh"
[[ $1 =~ ^[0-9]+$ ]] && [ "$1" -ge 1 ] && [ "$1" -le 255 ] || die "vcpus: 1 to 255"
# shellcheck disable=SC2034
VCPUS=$((10#$1))
# shellcheck disable=SC2034
NVGPU_PIN=$2
unset NVGPU_CPU_AFFINITY NVGPU_VCPU_PINS NVGPU_IO_AFFINITY NVGPU_BACKEND_CPUS NVGPU_GUEST_SMT
placement_settings

pins= xpins= jpins= i=0
for s in ${VCPU_SETS_J[@]+"${VCPU_SETS_J[@]}"}; do
    s=${s// /}
    pins+="${pins:+$([ "$VCPU_SINGLE" = 1 ] && echo , || echo :)}$s"
    xpins+="${xpins:+:}$i=$s"
    if [ "$VCPU_SINGLE" = 1 ]; then jpins+="${jpins:+, }$s"; else jpins+="${jpins:+, }[${s//,/, }]"; fi
    i=$((i + 1))
done
echo "# $PLACEMENT"
echo "launcher:"
[ -z "$pins" ] || echo "  NVGPU_VCPU_PINS=$pins"
[ -z "$CPU_AFFINITY" ] || echo "  NVGPU_CPU_AFFINITY=$CPU_AFFINITY"
[ -z "$IO_AFFINITY" ] || echo "  NVGPU_IO_AFFINITY=$IO_AFFINITY"
[ -z "$GUEST_SMT" ] || echo "  NVGPU_GUEST_SMT=$GUEST_SMT"
echo "crosvm (NVGPU_CROSVM_ARGS):"
if [ -n "$xpins" ]; then
    echo "  --cpu-affinity $xpins"
elif [ -n "$CPU_AFFINITY" ]; then
    echo "  --cpu-affinity $CPU_AFFINITY"
fi
[ "$GUEST_SMT" != 1 ] || echo "  --no-smt"
[ "$GUEST_SMT" != 2 ] || echo "  --per-vm-core-scheduling   (vCPUs 2k and 2k+1 share a core: one cookie for the VM)"
echo "nesbox (machine-config):"
[ -z "$jpins" ] || echo "  \"vcpu_pins\": [$jpins],"
[ -z "$CPU_AFFINITY_J" ] || echo "  \"cpu_affinity\": [$CPU_AFFINITY_J],"
[ -z "$GUEST_SMT" ] || echo "  \"threads_per_core\": $GUEST_SMT,"
if [ -n "$IO_AFFINITY_J" ]; then
    echo "  \"io_affinity\": [$IO_AFFINITY_J],"
elif [ -n "$jpins" ]; then
    # As the launcher does: else a worker a vCPU starts stays on its CPU.
    echo "  \"io_affinity\": [$(echo "$TOPO_CPUS" | sed 's/ /, /g')],   (every CPU: the workers vCPU threads start off their pins)"
fi
[ "$VCPU_SINGLE" = 1 ] || [ -z "$jpins" ] || echo "  (a list per vCPU: nesbox with patches/nesbox/0002)"
echo "systemd:"
[ -z "$BACKEND_CPUS" ] || echo "  vhost-user-nvgpu@N.service: CPUAffinity=$BACKEND_CPUS   (nix: vms.\"N\".backendCpus = \"$BACKEND_CPUS\")"
[ -z "$IO_AFFINITY" ] || echo "  nvgpu-vmm-crosvm@N.service: CPUAffinity=$IO_AFFINITY   (its threads but the vCPUs)"
exit 0
