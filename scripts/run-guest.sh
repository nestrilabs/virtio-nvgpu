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
# Run it on the GPU box, as root (the VMM needs /dev/kvm and the rootfs). It
# expects the tree laid out as:
#   /root/vhost-user-nvgpu          backend binary
#   /root/nesbox/target/release/nesbox
#   /root/kernel/vmlinux, /root/guest/rootfs.ext4
#
# The backend does NOT run as root. RM, DRM and NVKMS take a guest's
# privilege from the backend's credentials, so a root backend makes every
# guest process an RM administrator with all of BAR0 mappable -- the host
# kernel, one DMA away -- and the backend refuses to start that way
# (device/src/posture.rs). It runs as $NVGPU_USER (default "nvgpu"), which
# needs nothing but the groups of the device nodes it opens:
#
#   useradd --system --no-create-home --shell /usr/sbin/nologin \
#       --groups video,render,kvm nvgpu
#
# (/dev/nvidia* is 0666; render and card nodes are group render and video;
# /dev/udmabuf is group kvm.) setpriv hands it no capabilities and sets
# no_new_privs; the backend drops whatever it is given anyway.
set -euo pipefail

PROBE=${1:?usage: run-guest.sh <probe-name> [tag]}
TAG=${2:-$(date +%H%M%S)}
LOGS=/root/logs
NVGPU_USER=${NVGPU_USER:-nvgpu}
mkdir -p "$LOGS"

id "$NVGPU_USER" >/dev/null 2>&1 || {
    echo "no user $NVGPU_USER to run the backend as; see the top of $0" >&2
    exit 1
}

# A stale backend or VMM holds the GPU and confuses the logs.
pkill -f vhost-user-nvgpu || true
pkill -f 'nesbox.*gpu-forward' || true

# The socket lives in a fresh directory only the backend's user can enter:
# whoever listens at the path the VMM connects to receives the guest's
# memory, so it must not be a fixed name in a shared directory like /tmp,
# where another user could bind it first. The VMM, as root, still reaches
# it. The binary goes in the same directory, as /root is not the backend
# user's to read.
RUN=$(mktemp -d /run/nvgpu.XXXXXX)
SOCK=$RUN/nvgpu.sock
install -m 0755 /root/vhost-user-nvgpu "$RUN/vhost-user-nvgpu"
chown "$NVGPU_USER:" "$RUN"
chmod 0700 "$RUN"

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

# The log file is opened here, by root; the backend meters every log call
# site (device/src/ratelimit.rs), so a guest cannot grow it at will.
RUST_LOG=${RUST_LOG:-info} setpriv --reuid="$(id -u "$NVGPU_USER")" --regid="$(id -g "$NVGPU_USER")" \
    --init-groups --inh-caps=-all --bounding-set=-all --no-new-privs \
    "$RUN/vhost-user-nvgpu" --socket "$SOCK" \
    > "$LOGS/$TAG.backend.log" 2>&1 &
BACKEND=$!
trap 'kill $BACKEND 2>/dev/null || true; rm -rf "$RUN"' EXIT

# The backend must be listening before the VMM connects, and the socket must
# be the backend's: anything else at that path is not ours to connect to.
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "backend never created $SOCK" >&2; exit 1; }
kill -0 "$BACKEND" 2>/dev/null || { echo "backend exited; see $LOGS/$TAG.backend.log" >&2; exit 1; }
[ "$(stat -c %u "$SOCK")" = "$(id -u "$NVGPU_USER")" ] || {
    echo "$SOCK is not owned by $NVGPU_USER; refusing to connect" >&2
    exit 1
}

timeout 180 /root/nesbox/target/release/nesbox --config "$LOGS/$TAG.json" \
    > "$LOGS/$TAG.console.log" 2>&1 || true

sleep 1
echo "backend: $LOGS/$TAG.backend.log"
echo "console: $LOGS/$TAG.console.log"
