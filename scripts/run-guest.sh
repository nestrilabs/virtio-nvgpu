#!/bin/bash
# Boot a guest against the vhost-user backend and keep both sides' output.
#
# Usage: run-guest.sh <probe-name> [tag]
#   probe-name  a script under /opt/nvgpu in the guest rootfs, e.g. probeQ.sh
#   tag         names the log and config, so runs do not overwrite each other
#
# Leaves:
#   /root/logs/<tag>.backend.log   everything the backend saw
#   /root/logs/<tag>.console.log   the guest console
#   /root/logs/<tag>.json          the config this run used
#
# Run it on the GPU box. It expects the tree laid out as:
#   /root/vhost-user-nvgpu          backend binary
#   /root/nesbox/target/release/nesbox
#   /root/kernel/vmlinux, /root/guest/rootfs.ext4
set -euo pipefail

PROBE=${1:?usage: run-guest.sh <probe-name> [tag]}
TAG=${2:-$(date +%H%M%S)}
LOGS=/root/logs
SOCK=/tmp/nvgpu.sock
mkdir -p "$LOGS"

# A stale socket makes the VMM connect to a backend that is no longer there,
# and the failure surfaces much later as a torn stream.
pkill -f vhost-user-nvgpu || true
pkill -f 'nesbox.*gpu-forward' || true
rm -f "$SOCK"

cat > "$LOGS/$TAG.json" <<JSON
{
  "boot-source": {
    "kernel_image_path": "/root/kernel/vmlinux",
    "boot_args": "console=hvc0 root=/dev/vda rw init=/opt/nvgpu/$PROBE"
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": "/root/guest/rootfs.ext4", "is_root_device": true, "is_read_only": false }
  ],
  "machine-config": { "vcpu_count": 2, "mem_size_mib": 2048 },
  "gpu-forward": { "socket": "$SOCK" },
  "shared-directories": [
    { "tag": "nvidia", "path-on-host": "/var/lib/nvgpu", "read-only": true }
  ]
}
JSON

RUST_LOG=${RUST_LOG:-info} /root/vhost-user-nvgpu --socket "$SOCK" \
    > "$LOGS/$TAG.backend.log" 2>&1 &
BACKEND=$!
trap 'kill $BACKEND 2>/dev/null || true' EXIT

# The backend must be listening before the VMM connects.
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "backend never created $SOCK" >&2; exit 1; }

timeout 180 /root/nesbox/target/release/nesbox --config "$LOGS/$TAG.json" \
    > "$LOGS/$TAG.console.log" 2>&1 || true

sleep 1
echo "backend: $LOGS/$TAG.backend.log"
echo "console: $LOGS/$TAG.console.log"
