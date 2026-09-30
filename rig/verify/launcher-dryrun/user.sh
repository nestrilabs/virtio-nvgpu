#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# The unprivileged path, as this user, with the same stubs: nothing of the
# root-only checks applies, a stale backend of the rig's own is still killed,
# and a diagnostic backend flag is said on the terminal.
set -u
D=$(cd "$(dirname "$0")" && pwd)
L=$1
R=$D/userrig
rm -rf "$R"
mkdir -p "$R"/{bin,kernel,guest,logs,run}
cp "$D/stubvmm" "$R/bin/nesbox"
cp "$D/backend.sh" "$R/bin/vhost-user-nvgpu"
chmod 0755 "$R/bin/"*
echo kernel > "$R/kernel/vmlinux"
truncate -s 1M "$R/guest/rootfs.ext4"
# A stale backend of this rig's own, as the pattern sees it. X is short: a
# socket path must fit in 107 bytes.
X=$(mktemp -d /tmp/rg.XXXXXX)
chmod 0700 "$X"
mkdir -p "$X/nvgpu-run.stale"
cp "$D/stubvmm" "$R/run/stale-backend"
(exec -a "$R/bin/vhost-user-nvgpu" "$R/run/stale-backend" --socket "$X/nvgpu-run.stale/nvgpu.sock" other) &
STALE=$!
sleep 0.3
env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
    NVGPU_VMM_NETNS=0 bash "$L" probe usr 2>&1 | sed 's/^/    | /'
echo "  rc ${PIPESTATUS[0]}; the stale backend: $(kill -0 $STALE 2>/dev/null && echo alive || echo killed)"
kill "$STALE" 2>/dev/null
# Nothing placed unless asked: the config carries no placement key.
echo "  placement keys in a default run's config: $(grep -co 'vcpu_pins\|threads_per_core\|cpu_affinity\|io_affinity' "$R/logs/usr.json")"
echo "== unprivileged: NVGPU_PIN=smt:io=other on a fake host (two SMT cores, two L3 domains)"
T=$R/sysfs
mkdir -p "$T"
echo 0-3 >"$T/online"
for c in 0 1 2 3; do
    mkdir -p "$T/cpu$c/topology" "$T/cpu$c/cache/index3"
    echo "$((c % 2)),$((c % 2 + 2))" >"$T/cpu$c/topology/thread_siblings_list"
    echo 3 >"$T/cpu$c/cache/index3/level"
    echo "$((c % 2)),$((c % 2 + 2))" >"$T/cpu$c/cache/index3/shared_cpu_list"
done
env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
    NVGPU_VMM_NETNS=0 NVGPU_VCPUS=2 NVGPU_PIN=smt:io=other NVGPU_SYSFS_CPU="$T" bash "$L" probe usrpin 2>&1 |
    grep 'placement:' | sed 's/^/    | /'
echo "  the config's machine-config: $(grep -o '"machine-config": {[^}]*}' "$R/logs/usrpin.json")"
# Pins alone: the workers vCPU threads start go to the launcher's own CPUs,
# not to a vCPU's.
env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
    NVGPU_VMM_NETNS=0 NVGPU_VCPUS=2 NVGPU_VCPU_PINS=0,1 bash "$L" probe usrpins >/dev/null 2>&1
echo "  pins alone, the I/O set is the launcher's CPUs: $(grep -o '"io_affinity": \[[0-9, ]*\]' "$R/logs/usrpins.json" |
    tr -d ' ' | grep -qx "\"io_affinity\":\[$(taskset -pc $$ | sed 's/.*: //' | tr -d ' ' |
        awk -F, '{ for (i = 1; i <= NF; i++) { n = split($i, r, "-"); a = r[1]; b = (n > 1 ? r[2] : r[1]); for (j = a; j <= b; j++) printf "%s%d", (o++ ? "," : ""), j } }')\]" && echo yes || echo no)"
echo "== unprivileged: a diagnostic flag is said on the terminal"
env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
    NVGPU_VMM_NETNS=0 bash "$L" probe usrdiag -- --permissive-abi 2>&1 | grep 'diagnostic flag' | sed 's/^/    | /'
rm -rf "$X"
