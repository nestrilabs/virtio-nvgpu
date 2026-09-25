#!/usr/bin/env bash
# Boot a guest against the vhost-user backend and keep both sides' output.
#
# Usage: run-guest.sh [display options] <probe-name> [tag] [-- backend-args...]
#   probe-name  a script under /opt/nvgpu in the guest rootfs, e.g. probeQ.sh
#   tag         names the log and config, so runs do not overwrite each other
#   backend-args  anything else for the backend, as is (--permissive-abi,
#               --keep-guest-coherency, ...; see vhost-user-nvgpu --help)
#
# Display options, passed through to the backend (TESTING.md has which mode
# wants which):
#   --kms-card                  compositor-VM: offer the host's card nodes
#   --wayland-socket PATH       the host compositor's socket, for the proxy
#   --wayland-lease             offer the compositor's DRM lease device
#   --wayland-export PATH       accept host Wayland clients at PATH
#   --wayland-max-conns N       Wayland channels per VM (backend default 64)
#   --wayland-shm-budget MIB    wl_shm memory per VM (default 1024)
#   --wayland-queue-budget MIB  unread compositor output per VM (default 256)
#   --wayland-lease-interval S  seconds between lease requests (default 5)
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
# (device/src/posture.rs). It runs as $NVGPU_USER, with the groups of the
# device nodes it opens and nothing else:
#
#   useradd --system --no-create-home --shell /usr/sbin/nologin nvgpu
#
# (/dev/nvidia* is 0666; render and card nodes are group render and video;
# /dev/udmabuf is group kvm.) setpriv hands it those groups, no capabilities
# and no_new_privs; the backend drops whatever it is given anyway.
#
# Which user: "nvgpu" by default. With --wayland-socket it is the socket's
# owner instead -- the backend is then a client of that user's compositor
# like any of the user's own programs, and the socket's directory
# ($XDG_RUNTIME_DIR, 0700) lets no one else in anyway. With --wayland-export
# (and no --wayland-socket) it is the owner of the directory the export
# socket goes in, since the backend accepts only clients of its own uid
# (device/src/wl/export.rs). NVGPU_USER overrides all of these.
# NVGPU_USER=root runs it as root, which it refuses unless
# NVGPU_ALLOW_ROOT_UNSAFE=1 as well: that passes --allow-root-unsafe, and
# every guest process is then an RM administrator. Only for ruling the
# credentials out while chasing something, never for a guest you do not
# trust.
set -euo pipefail

usage() {
    sed -n '4,20p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

BACKEND_ARGS=()
WL_SOCK=
WL_EXPORT=
POSITIONAL=()
while [ $# -gt 0 ]; do
    case $1 in
        --kms-card | --wayland-lease)
            BACKEND_ARGS+=("$1")
            shift
            ;;
        --wayland-socket)
            [ $# -ge 2 ] || usage
            WL_SOCK=$2
            BACKEND_ARGS+=("$1" "$2")
            shift 2
            ;;
        --wayland-export)
            [ $# -ge 2 ] || usage
            WL_EXPORT=$2
            BACKEND_ARGS+=("$1" "$2")
            shift 2
            ;;
        --wayland-max-conns | --wayland-shm-budget | \
            --wayland-queue-budget | --wayland-lease-interval)
            [ $# -ge 2 ] || usage
            BACKEND_ARGS+=("$1" "$2")
            shift 2
            ;;
        --)
            shift
            BACKEND_ARGS+=("$@")
            break
            ;;
        -h | --help) usage ;;
        -*)
            echo "unknown option $1 (backend flags go after --)" >&2
            usage
            ;;
        *)
            POSITIONAL+=("$1")
            shift
            ;;
    esac
done
[ ${#POSITIONAL[@]} -ge 1 ] && [ ${#POSITIONAL[@]} -le 2 ] || usage

PROBE=${POSITIONAL[0]}
TAG=${POSITIONAL[1]:-$(date +%H%M%S)}
LOGS=/root/logs
mkdir -p "$LOGS"

if [ -n "${NVGPU_USER:-}" ]; then
    :
elif [ -n "$WL_SOCK" ]; then
    NVGPU_USER=$(stat -c %U "$WL_SOCK") || {
        echo "--wayland-socket $WL_SOCK: no such socket" >&2
        exit 1
    }
elif [ -n "$WL_EXPORT" ]; then
    NVGPU_USER=$(stat -c %U "$(dirname "$WL_EXPORT")") || {
        echo "--wayland-export $WL_EXPORT: no such directory" >&2
        exit 1
    }
else
    NVGPU_USER=nvgpu
fi
id "$NVGPU_USER" >/dev/null 2>&1 || {
    echo "no user $NVGPU_USER to run the backend as; see the top of $0" >&2
    exit 1
}

# The groups the device nodes want, those of them this host has.
GROUPS_WANTED=()
for g in video render kvm; do
    getent group "$g" >/dev/null && GROUPS_WANTED+=("$g")
done
if [ ${#GROUPS_WANTED[@]} -gt 0 ]; then
    GROUP_OPT=--groups=$(IFS=,; echo "${GROUPS_WANTED[*]}")
else
    GROUP_OPT=--clear-groups
fi

# How the backend is started: as $NVGPU_USER, with only those groups, no
# capabilities to inherit or regain, and no way to gain privilege by exec.
AS_BACKEND=(setpriv --reuid="$(id -u "$NVGPU_USER")" --regid="$(id -g "$NVGPU_USER")"
    "$GROUP_OPT" --inh-caps=-all --bounding-set=-all --no-new-privs)
if [ "$(id -u "$NVGPU_USER")" = 0 ]; then
    [ "${NVGPU_ALLOW_ROOT_UNSAFE:-}" = 1 ] || {
        echo "refusing to run the backend as root: every guest process would be an" \
            "RM administrator. NVGPU_ALLOW_ROOT_UNSAFE=1 if you mean it." >&2
        exit 1
    }
    echo "WARNING: backend runs as root (NVGPU_ALLOW_ROOT_UNSAFE=1)" >&2
    AS_BACKEND=()
    BACKEND_ARGS+=(--allow-root-unsafe)
fi

# The compositor's socket has to be one the backend's user can connect to;
# better to say so here than as a failed CONNECT from inside the guest.
if [ -n "$WL_SOCK" ] && [ ${#AS_BACKEND[@]} -gt 0 ] &&
    ! "${AS_BACKEND[@]}" test -w "$WL_SOCK"; then
    echo "$NVGPU_USER cannot connect to $WL_SOCK; run as its owner" \
        "(NVGPU_USER=$(stat -c %U "$WL_SOCK"))" >&2
    exit 1
fi

# A stale backend or VMM holds the GPU and confuses the logs.
pkill -f vhost-user-nvgpu || true
pkill -f 'nesbox.*gpu-forward' || true

# The socket lives in a fresh directory only the backend's user can enter:
# whoever listens at the path the VMM connects to receives the guest's
# memory, so it must not be a fixed name in a shared directory like /tmp,
# where another user could bind it first. The VMM, as root, still reaches
# it. The binary goes in the same directory, as /root is not the backend
# user's to read. (Without --socket the backend would pick
# $XDG_RUNTIME_DIR/nvgpu/nvgpu.sock, which root's environment does not
# describe for another user; a directory per run also keeps two runs apart.)
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
echo "backend: as $NVGPU_USER${BACKEND_ARGS[*]:+, with ${BACKEND_ARGS[*]}}" >&2
RUST_LOG=${RUST_LOG:-info} "${AS_BACKEND[@]}" \
    "$RUN/vhost-user-nvgpu" --socket "$SOCK" "${BACKEND_ARGS[@]}" \
    > "$LOGS/$TAG.backend.log" 2>&1 &
BACKEND=$!
trap 'kill $BACKEND 2>/dev/null || true; rm -rf "$RUN"' EXIT

# The backend must be listening before the VMM connects, and the socket must
# be the backend's: anything else at that path is not ours to connect to.
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done
[ -S "$SOCK" ] || { echo "backend never created $SOCK; see $LOGS/$TAG.backend.log" >&2; exit 1; }
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
