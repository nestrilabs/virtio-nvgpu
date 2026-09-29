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
echo "== unprivileged: a diagnostic flag is said on the terminal"
env -i PATH="$PATH" HOME="$HOME" XDG_RUNTIME_DIR="$X" NVGPU_RIG="$R" NVGPU_SKIP_MEM_CHECK=1 NVGPU_TIMEOUT=3 \
    NVGPU_VMM_NETNS=0 bash "$L" probe usrdiag -- --permissive-abi 2>&1 | grep 'diagnostic flag' | sed 's/^/    | /'
rm -rf "$X"
