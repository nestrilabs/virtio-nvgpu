#!/usr/bin/env bash
# Boot a guest against the vhost-user backend and keep both sides' output.
#
# Usage: run-guest.sh [display options] <probe-name> [tag] [-- backend-args...]
#   probe-name  a script under /opt/nvgpu in the guest rootfs, e.g. probeQ.sh;
#               in the rig layout a bare name gets .sh added (stage1 ->
#               /opt/nvgpu/stage1.sh)
#   tag         names the log and config, so runs do not overwrite each other
#   backend-args  anything else for the backend, as is (--permissive-abi,
#               --keep-guest-coherency, ...; see vhost-user-nvgpu --help)
#
# Display options, passed through to the backend (TESTING.md has which mode
# wants which, TESTING-RIG.md which of them are safe next to a live desktop):
#   --kms-card                  compositor-VM: offer the host's card nodes
#   --wayland-socket PATH       the host compositor's socket, for the proxy
#   --wayland-lease             offer the compositor's DRM lease device
#   --wayland-export PATH       accept host Wayland clients at PATH
#   --wayland-max-conns N       Wayland channels per VM (backend default 64)
#   --wayland-shm-budget MIB    wl_shm memory per VM (default 1024)
#   --wayland-queue-budget MIB  unread compositor output per VM (default 256)
#   --wayland-lease-interval S  seconds between lease requests (default 5)
#
# Compute (off by default; SECURITY.md, "Compute"):
#   --allow-compute             serve CUDA: UVM, the UVM aperture and memory
#                               registered by its pages. Without it the guest
#                               has no /dev/nvidia-uvm and CUDA finds no
#                               device. The probe is told which, as
#                               nvgpu_compute=0|1 on the kernel command line.
#
# Environment (all optional):
#   NVGPU_COMPUTE=1     the same as --allow-compute
#   NVGPU_RIG           the rig directory (bin/ kernel/ guest/ logs/); default
#                       the repo's .rig when not root
#   NVGPU_BACKEND NVGPU_VMM NVGPU_KERNEL NVGPU_ROOTFS NVGPU_LOGS
#                       override one path of whichever layout is in use
#   NVGPU_VCPUS NVGPU_MEM_MIB   guest size (rig 4 / 4096, root 2 / 2048)
#   NVGPU_TIMEOUT       seconds before the VMM is killed (180; 3600 interactive)
#   NVGPU_CMDLINE_EXTRA appended to the guest kernel command line
#   NVGPU_NVIDIA_SHARE  host directory offered to the guest as virtiofs tag
#                       "nvidia" (rig: none; root: /var/lib/nvgpu; set it
#                       empty to drop it there too)
#   NVGPU_COPY_ROOTFS   1 boots a per-run copy of the rootfs (rig default), 0
#                       the image itself (root default)
#   NVGPU_KEEP_ROOTFS=1 keep the per-run copy afterwards, for inspection
#   NVGPU_INTERACTIVE   1 gives the guest console this terminal (typing goes
#                       to the guest, output is shown and logged); default: on
#                       for the shell probe and nvgpu_hold=1 when stdin is a
#                       terminal, off otherwise. NVGPU_TIMEOUT then defaults
#                       to 3600.
#   NVGPU_USER          root only: who the backend runs as (see below)
#
# Safety knobs, for a GPU that also drives the desktop (.rig/SAFETY-NOTES.md):
#   NVGPU_MEMORY_MAX    e.g. 12G: run the launcher, backend and VMM in a
#                       transient systemd scope with that MemoryMax and no
#                       swap (systemd-run --user --scope; fails, running
#                       nothing, where there is no systemd user manager, as in
#                       the Claude sandbox)
#   NVGPU_MEM_HEADROOM_MIB  host memory to leave free beyond the guest and the
#                       1 GiB window, or the run is refused (default 4096)
#   NVGPU_SKIP_MEM_CHECK=1  run even so
#   NVGPU_OOM_SCORE_ADJ oom_score_adj for the launcher, backend and VMM
#                       (default 1000: the OOM killer takes the VM before the
#                       compositor; empty leaves it alone)
#   NVGPU_KMS_CARD_FORCE=1  allow --kms-card although a Wayland compositor's
#                       socket is still there (see "--kms-card" below)
#
# Exit status: the probe's NVGPU_PROBE_DONE verdict when it printed one (0
# PASS, 1 FAIL); otherwise 0 when the console shows a PASS and no FAIL, 1 on
# any FAIL line, 2 when it shows neither. 124 when the guest did not power
# off in time.
# ---
#
# Leaves, in the logs directory ($NVGPU_RIG/logs, or /root/logs as root):
#   <tag>.backend.log   everything the backend saw
#   <tag>.console.log   the guest console
#   <tag>.json          the config this run used
#   <tag>.rootfs.ext4   the run's copy of the rootfs, only with
#                       NVGPU_KEEP_ROOTFS=1
#
# Two ways to run it, picked by who runs it.
#
# As root, on the GPU box -- the original layout. It expects the tree laid
# out as:
#   /root/vhost-user-nvgpu          backend binary
#   /root/nesbox/target/release/nesbox
#   /root/kernel/vmlinux, /root/guest/rootfs.ext4
# (NVGPU_RIG, or the single-path overrides, point it elsewhere.)
#
# As an ordinary user, with the rig layout (TESTING-RIG.md): the user needs
# /dev/kvm and the NVIDIA nodes, and nothing here needs root. Paths come from
# $NVGPU_RIG, laid out as
#   bin/vhost-user-nvgpu  bin/nesbox  [bin/virtiofsd]
#   kernel/vmlinux        guest/rootfs.ext4        logs/
# The rootfs there is the golden image: each run boots a copy of it
# (cp --reflink, so on btrfs or XFS the copy costs nothing until the guest
# writes), and deletes the copy afterwards, so a run can never leave the next
# one a dirty or half-written filesystem. scripts/rig-preflight.sh says
# whether this host is ready for it.
#
# The backend does NOT run as root. RM, DRM and NVKMS take a guest's
# privilege from the backend's credentials, so a root backend makes every
# guest process an RM administrator with all of BAR0 mappable -- the host
# kernel, one DMA away -- and the backend refuses to start that way
# (device/src/posture.rs).
#
# Run as root, the script starts it as $NVGPU_USER, with the groups of the
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
#
# Run as an ordinary user, the backend is that user: nobody else is
# available without privilege, and it is who owns the compositor socket and
# the export directory anyway. setpriv still gives it no_new_privs and empty
# inheritable and ambient capability sets. It cannot change the uid, the
# groups or the bounding set -- all three need privilege -- so the backend
# keeps the user's own supplementary groups: a group that is root in all but
# name (docker, libvirt, disk) is then within reach of anything that takes
# the backend over, and rig-preflight.sh warns about those.
set -euo pipefail

usage() {
    awk 'NR >= 4 { if (/^# ---/) exit; sub(/^# ?/, ""); print }' "$0" >&2
    exit 2
}

die() {
    echo "run-guest: $*" >&2
    exit 1
}

# ── A memory ceiling, when asked for ─────────────────────────────────────────
#
# Guest RAM is a memfd the VMM commits in full at boot, and the backend's
# window memfd fills as the guest uses it; both are charged to the cgroup of
# whoever faults them in, so a scope around this whole launcher bounds both.
# (Pinned GPU system memory that nvidia.ko allocates for the guest is not
# charged to any cgroup: the scope does not bound that.) Re-executed inside
# the scope before anything starts; if systemd-run cannot make the scope,
# nothing runs.
if [ -n "${NVGPU_MEMORY_MAX:-}" ] && [ -z "${NVGPU_IN_SCOPE:-}" ]; then
    [[ $NVGPU_MEMORY_MAX =~ ^[0-9]+[KMGT]?$ ]] || die "NVGPU_MEMORY_MAX=$NVGPU_MEMORY_MAX: a size such as 12G"
    command -v systemd-run >/dev/null || die "NVGPU_MEMORY_MAX needs systemd-run"
    SCOPE=(systemd-run --scope --quiet --collect
        -p "MemoryMax=$NVGPU_MEMORY_MAX" -p MemorySwapMax=0)
    [ "$(id -u)" = 0 ] || SCOPE=("${SCOPE[0]}" --user "${SCOPE[@]:1}")
    echo "run-guest: in a scope with MemoryMax=$NVGPU_MEMORY_MAX, no swap" >&2
    export NVGPU_IN_SCOPE=1
    exec "${SCOPE[@]}" -- "$BASH" "$0" "$@"
fi

# A string as a JSON string literal. Paths and the command line are the only
# things that go into the config from outside, and a quote in either would
# otherwise make it say something else.
json_str() {
    local s=$1
    case $s in *[[:cntrl:]]*) die "control character in \"$s\"" ;; esac
    s=${s//\\/\\\\}
    s=${s//\"/\\\"}
    printf '"%s"' "$s"
}

# A literal string as an extended regular expression, for pkill -f.
re() {
    printf '%s' "$1" | sed 's/[][\\.*^$+?(){}|]/\\&/g'
}

BACKEND_ARGS=()
WL_SOCK=
WL_EXPORT=
KMS_CARD=0
COMPUTE=0
[ "${NVGPU_COMPUTE:-0}" = 1 ] && COMPUTE=1
POSITIONAL=()
while [ $# -gt 0 ]; do
    case $1 in
        --allow-compute)
            COMPUTE=1
            shift
            ;;
        --kms-card | --wayland-lease)
            [ "$1" = --kms-card ] && KMS_CARD=1
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
            for a in "$@"; do
                case $a in
                    --allow-compute) COMPUTE=1 ;;
                    *)
                        [ "$a" = --kms-card ] && KMS_CARD=1
                        BACKEND_ARGS+=("$a")
                        ;;
                esac
            done
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
# Compute is the backend's to serve and the probe's to expect: one switch for
# both (guest-image/probes/render.sh reads nvgpu_compute).
[ "$COMPUTE" = 1 ] && BACKEND_ARGS+=(--allow-compute)

PROBE=${POSITIONAL[0]}
TAG=${POSITIONAL[1]:-$(date +%H%M%S)}
# Both end up in file names, the kernel command line and a pkill pattern.
[[ $PROBE =~ ^[A-Za-z0-9._-]+$ ]] || die "probe name \"$PROBE\": letters, digits, . _ - only"
[[ $TAG =~ ^[A-Za-z0-9._-]+$ ]] || die "tag \"$TAG\": letters, digits, . _ - only"

# ── Which layout, and who we are ─────────────────────────────────────────────
#
# Privilege and layout are separate questions. Root keeps the /root layout it
# always had unless NVGPU_RIG says otherwise; anyone else has only the rig.
if [ "$(id -u)" = 0 ]; then PRIV=root; else PRIV=user; fi
if [ -n "${NVGPU_RIG:-}" ] || [ $PRIV = user ]; then
    LAYOUT=rig
    RIG=$(realpath -s -m -- "${NVGPU_RIG:-$(dirname -- "$0")/../.rig}")
    BACKEND_BIN=${NVGPU_BACKEND:-$RIG/bin/vhost-user-nvgpu}
    VMM=${NVGPU_VMM:-$RIG/bin/nesbox}
    KERNEL=${NVGPU_KERNEL:-$RIG/kernel/vmlinux}
    ROOTFS=${NVGPU_ROOTFS:-$RIG/guest/rootfs.ext4}
    LOGS=${NVGPU_LOGS:-$RIG/logs}
    VCPUS=${NVGPU_VCPUS:-4}
    MEM_MIB=${NVGPU_MEM_MIB:-4096}
    COPY_ROOTFS=${NVGPU_COPY_ROOTFS:-1}
    # The rig's image carries the NVIDIA userspace itself.
    NVIDIA_SHARE=${NVGPU_NVIDIA_SHARE:-}
    # The rig's init convention is /opt/nvgpu/<name>.sh.
    case $PROBE in *.*) ;; *) PROBE=$PROBE.sh ;; esac
else
    LAYOUT=root
    RIG=
    BACKEND_BIN=${NVGPU_BACKEND:-/root/vhost-user-nvgpu}
    VMM=${NVGPU_VMM:-/root/nesbox/target/release/nesbox}
    KERNEL=${NVGPU_KERNEL:-/root/kernel/vmlinux}
    ROOTFS=${NVGPU_ROOTFS:-/root/guest/rootfs.ext4}
    LOGS=${NVGPU_LOGS:-/root/logs}
    VCPUS=${NVGPU_VCPUS:-2}
    MEM_MIB=${NVGPU_MEM_MIB:-2048}
    COPY_ROOTFS=${NVGPU_COPY_ROOTFS:-0}
    # Unset means the share this layout always had; set but empty means none.
    NVIDIA_SHARE=${NVGPU_NVIDIA_SHARE-/var/lib/nvgpu}
fi
CMDLINE_EXTRA=${NVGPU_CMDLINE_EXTRA:-}
case " $CMDLINE_EXTRA " in
    *" nvgpu_compute="*) ;;
    *) CMDLINE_EXTRA="${CMDLINE_EXTRA:+$CMDLINE_EXTRA }nvgpu_compute=$COMPUTE" ;;
esac
# An interactive run (the shell probe, nvgpu_hold=1) needs the guest console
# on this terminal: with stdin at /dev/null the guest's shell reads EOF at
# once and the VM powers off.
case " $PROBE $CMDLINE_EXTRA " in
    *" shell "* | *" shell.sh "* | *" nvgpu_hold=1 "*) INTERACTIVE_DEFAULT=1 ;;
    *) INTERACTIVE_DEFAULT=0 ;;
esac
INTERACTIVE=${NVGPU_INTERACTIVE:-$INTERACTIVE_DEFAULT}
if [ "$INTERACTIVE" = 1 ] && [ ! -t 0 ]; then
    echo "run-guest: stdin is not a terminal; the guest console is not interactive" >&2
    INTERACTIVE=0
fi
if [ "$INTERACTIVE" = 1 ]; then
    TIMEOUT=${NVGPU_TIMEOUT:-3600}
else
    TIMEOUT=${NVGPU_TIMEOUT:-180}
fi

# Absolute and tidy, but not through symlinks: the stale-process patterns
# below match the paths exactly as they are executed.
BACKEND_BIN=$(realpath -s -m -- "$BACKEND_BIN")
VMM=$(realpath -s -m -- "$VMM")
KERNEL=$(realpath -s -m -- "$KERNEL")
ROOTFS=$(realpath -s -m -- "$ROOTFS")
LOGS=$(realpath -s -m -- "$LOGS")

[[ $VCPUS =~ ^[0-9]+$ ]] && [ "$VCPUS" -ge 1 ] && [ "$VCPUS" -le 255 ] ||
    die "NVGPU_VCPUS=$VCPUS: 1 to 255"
[[ $MEM_MIB =~ ^[0-9]+$ ]] && [ "$MEM_MIB" -ge 256 ] || die "NVGPU_MEM_MIB=$MEM_MIB: at least 256"
[[ $TIMEOUT =~ ^[0-9]+$ ]] && [ "$TIMEOUT" -ge 1 ] || die "NVGPU_TIMEOUT=$TIMEOUT: whole seconds"
# As JSON numbers: "04" is not one.
VCPUS=$((10#$VCPUS))
MEM_MIB=$((10#$MEM_MIB))
[ -x "$BACKEND_BIN" ] || die "no backend at $BACKEND_BIN (NVGPU_BACKEND)"
[ -x "$VMM" ] || die "no VMM at $VMM (NVGPU_VMM)"
[ -r "$KERNEL" ] || die "no guest kernel at $KERNEL (NVGPU_KERNEL)"
[ -r "$ROOTFS" ] || die "no rootfs at $ROOTFS (NVGPU_ROOTFS)"
if [ -n "$NVIDIA_SHARE" ]; then
    [ -d "$NVIDIA_SHARE" ] || die "NVGPU_NVIDIA_SHARE=$NVIDIA_SHARE is not a directory"
    # nesbox finds virtiofsd on PATH or in NESBOX_VIRTIOFSD; the rig may carry
    # its own.
    if [ -z "${NESBOX_VIRTIOFSD:-}" ] && [ -n "$RIG" ] && [ -x "$RIG/bin/virtiofsd" ]; then
        export NESBOX_VIRTIOFSD=$RIG/bin/virtiofsd
    fi
fi
mkdir -p "$LOGS"

# ── The desktop shares this GPU and this memory ──────────────────────────────
#
# The VMM commits all of guest RAM at boot and the window can take up to
# 1 GiB more; refuse a run that would leave the desktop less than the
# headroom, rather than have the OOM killer choose. When it does have to
# choose, it should take this run: the launcher, backend and VMM all inherit
# this oom_score_adj (raising one's own needs no privilege).
HEADROOM_MIB=${NVGPU_MEM_HEADROOM_MIB:-4096}
[[ $HEADROOM_MIB =~ ^[0-9]+$ ]] || die "NVGPU_MEM_HEADROOM_MIB=$HEADROOM_MIB: whole MiB"
AVAIL_KIB=$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo 2>/dev/null || true)
if [[ $AVAIL_KIB =~ ^[0-9]+$ ]]; then
    NEED_MIB=$((MEM_MIB + 1024 + 10#$HEADROOM_MIB))
    if [ $((AVAIL_KIB / 1024)) -lt "$NEED_MIB" ] && [ "${NVGPU_SKIP_MEM_CHECK:-}" != 1 ]; then
        die "$((AVAIL_KIB / 1024)) MiB available; a ${MEM_MIB} MiB guest, its 1 GiB window and" \
            "${HEADROOM_MIB} MiB left for the desktop want ${NEED_MIB} (NVGPU_MEM_MIB smaller," \
            "or NVGPU_SKIP_MEM_CHECK=1)"
    fi
fi
OOM_ADJ=${NVGPU_OOM_SCORE_ADJ-1000}
if [ -n "$OOM_ADJ" ]; then
    [[ $OOM_ADJ =~ ^-?[0-9]+$ ]] || die "NVGPU_OOM_SCORE_ADJ=$OOM_ADJ: -1000 to 1000"
    { echo "$OOM_ADJ" > /proc/self/oom_score_adj; } 2>/dev/null ||
        echo "run-guest: note: could not set oom_score_adj $OOM_ADJ" >&2
fi

# --kms-card hands the guest the host's card nodes, and the backend opens one
# for it as an ordinary process would: whoever opens a card node while no one
# is DRM master becomes master. A compositor that is only switched away (a
# VT switch, or another session in front) has dropped master, so the VM would
# take the card and the desktop could not get it back until the VM ends.
# Group C in TESTING-RIG.md stops the compositor first; a socket still in the
# runtime directory says it has not been. (Inside the sandbox the host's
# runtime directory is hidden and this sees nothing: stop it anyway.)
if [ "$KMS_CARD" = 1 ] && [ "${NVGPU_KMS_CARD_FORCE:-}" != 1 ]; then
    RT_DIRS=("${XDG_RUNTIME_DIR:-/nonexistent}")
    [ $PRIV = root ] && RT_DIRS+=(/run/user/*)
    LIVE=()
    for d in "${RT_DIRS[@]}"; do
        for s in "$d"/wayland-* "$d"/hypr/*/.socket.sock; do
            [ -S "$s" ] && LIVE+=("$s")
        done
    done
    [ ${#LIVE[@]} -eq 0 ] ||
        die "--kms-card while a compositor's socket exists (${LIVE[*]}): stop the desktop first" \
            "(TESTING-RIG.md group C); NVGPU_KMS_CARD_FORCE=1 if that socket is stale"
fi

# ── Who the backend runs as ──────────────────────────────────────────────────
if [ $PRIV = root ]; then
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
    BACKEND_UID=$(id -u "$NVGPU_USER")
    AS_BACKEND=(setpriv --reuid="$BACKEND_UID" --regid="$(id -g "$NVGPU_USER")"
        "$GROUP_OPT" --inh-caps=-all --bounding-set=-all --no-new-privs)
    if [ "$BACKEND_UID" = 0 ]; then
        [ "${NVGPU_ALLOW_ROOT_UNSAFE:-}" = 1 ] || {
            echo "refusing to run the backend as root: every guest process would be an" \
                "RM administrator. NVGPU_ALLOW_ROOT_UNSAFE=1 if you mean it." >&2
            exit 1
        }
        echo "WARNING: backend runs as root (NVGPU_ALLOW_ROOT_UNSAFE=1)" >&2
        AS_BACKEND=()
        BACKEND_ARGS+=(--allow-root-unsafe)
    fi
else
    # Without privilege there is exactly one user to be. Saying so beats
    # silently ignoring a NVGPU_USER that asks for another.
    # (A uid with no passwd entry, as in a sandbox, is still a user.)
    BACKEND_UID=$(id -u)
    ME=$(id -un 2>/dev/null) || ME=$BACKEND_UID
    if [ -n "${NVGPU_USER:-}" ] && [ "$NVGPU_USER" != "$ME" ] && [ "$NVGPU_USER" != "$BACKEND_UID" ]; then
        die "NVGPU_USER=$NVGPU_USER needs root; unprivileged, the backend runs as $ME"
    fi
    NVGPU_USER=$ME
    # What an unprivileged setpriv can still do (util-linux 2.42 checked):
    # set no_new_privs, and empty the inheritable and ambient sets.
    # --reuid, --groups/--clear-groups and --bounding-set all fail with EPERM.
    AS_BACKEND=(setpriv --no-new-privs --inh-caps=-all --ambient-caps=-all)
fi

# The compositor's socket has to be one the backend's user can connect to;
# better to say so here than as a failed CONNECT from inside the guest.
if [ -n "$WL_SOCK" ] && [ ${#AS_BACKEND[@]} -gt 0 ] &&
    ! "${AS_BACKEND[@]}" test -S "$WL_SOCK" -a -w "$WL_SOCK"; then
    echo "$NVGPU_USER cannot connect to $WL_SOCK; run as its owner" \
        "(NVGPU_USER=$(stat -c %U "$WL_SOCK" 2>/dev/null || echo '?'))" >&2
    exit 1
fi
# The live desktop's own compositor is a legitimate target (the lease stages
# need it), but not an accidental one: everything the proxy lets through, it
# lets through to the session the user is sitting in.
if [ -n "$WL_SOCK" ] && [ -n "${WAYLAND_DISPLAY:-}" ] && [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    live=$WAYLAND_DISPLAY
    case $live in /*) ;; *) live=$XDG_RUNTIME_DIR/$live ;; esac
    if [ "$(realpath -m -- "$WL_SOCK")" = "$(realpath -m -- "$live")" ]; then
        echo "WARNING: $WL_SOCK is this session's own compositor; the guest's clients" \
            "appear on the live desktop (TESTING-RIG.md says which stages want that)" >&2
    fi
fi
if [ $PRIV = user ] && [ -n "$WL_EXPORT" ]; then
    [ -d "$(dirname -- "$WL_EXPORT")" ] && [ -w "$(dirname -- "$WL_EXPORT")" ] ||
        die "--wayland-export $WL_EXPORT: its directory must exist and be $NVGPU_USER's"
fi

# ── A stale backend or VMM holds the GPU and confuses the logs ───────────────
#
# Only ours: processes of the backend's user started from exactly this
# launcher's backend binary and socket directory, and VMMs of the invoking
# user started from this VMM binary with a config in this logs directory.
# The patterns are anchored at the start of the command line, so an editor
# or a grep with the path in its arguments is not one of them.
if [ $PRIV = root ]; then
    pkill -u "$NVGPU_USER" -f '^/run/nvgpu\.[^/ ]+/vhost-user-nvgpu --socket /run/nvgpu\.' || true
else
    RUN_PARENT=${XDG_RUNTIME_DIR:-$RIG/run}
    pkill -u "$(id -u)" -f "^$(re "$BACKEND_BIN") --socket $(re "$RUN_PARENT")/nvgpu-run\." || true
fi
pkill -u "$(id -u)" -f "^$(re "$VMM") $(re "$LOGS")/[^/ ]+\.json" || true

# ── Cleanup, however the run ends ────────────────────────────────────────────
BACKEND=
VMM_PID=
RUN=
DISK_COPY=
TTY_STATE=
# Whether $1 is still this run's process: its command line names $2 (the
# run's config for the VMM, its socket for the backend). A child that exited
# during the run has been reaped, and its pid may since be someone else's.
still_ours() {
    local cmd
    [ -n "$2" ] || return 1
    cmd=$(tr '\0' ' ' < "/proc/$1/cmdline" 2>/dev/null) || return 1
    [[ $cmd == *"$2"* ]]
}
cleanup() {
    if [ -n "$VMM_PID" ] && still_ours "$VMM_PID" "${CFG:-}"; then
        kill "$VMM_PID" 2>/dev/null || true
    fi
    # nesbox puts the terminal in raw mode; one killed never restores it.
    [ -z "$TTY_STATE" ] || stty "$TTY_STATE" 2>/dev/null || true
    if [ -n "$BACKEND" ] && still_ours "$BACKEND" "${SOCK:-}"; then
        kill "$BACKEND" 2>/dev/null || true
    fi
    [ -z "$RUN" ] || rm -rf "$RUN"
    if [ -n "$DISK_COPY" ]; then
        if [ "${NVGPU_KEEP_ROOTFS:-0}" = 1 ]; then
            echo "rootfs:  $DISK_COPY (kept)"
        else
            rm -f "$DISK_COPY"
        fi
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# ── The socket's directory ───────────────────────────────────────────────────
#
# The socket lives in a fresh directory only the backend's user can enter:
# whoever listens at the path the VMM connects to receives the guest's
# memory, so it must not be a fixed name in a shared directory like /tmp,
# where another user could bind it first. (Without --socket the backend would
# pick $XDG_RUNTIME_DIR/nvgpu/nvgpu.sock; a directory per run also keeps two
# runs apart.)
if [ $PRIV = root ]; then
    # The VMM, as root, still reaches it. The binary goes in the same
    # directory, as /root is not the backend user's to read. (root's
    # environment does not describe another user's $XDG_RUNTIME_DIR.)
    RUN=$(mktemp -d /run/nvgpu.XXXXXX)
    install -m 0755 "$BACKEND_BIN" "$RUN/vhost-user-nvgpu"
    BACKEND_EXE=$RUN/vhost-user-nvgpu
    chown "$NVGPU_USER:" "$RUN"
    chmod 0700 "$RUN"
else
    # Our own runtime directory is already 0700 and ours; without one, the
    # rig's run/ is made so. mktemp -d makes the run's own directory 0700.
    if [ -z "${XDG_RUNTIME_DIR:-}" ]; then
        (umask 077 && mkdir -p "$RUN_PARENT")
        chmod 0700 "$RUN_PARENT"
    fi
    RUN=$(mktemp -d "$RUN_PARENT/nvgpu-run.XXXXXX")
    chmod 0700 "$RUN"
    BACKEND_EXE=$BACKEND_BIN
fi
SOCK=$RUN/nvgpu.sock
# sun_path holds 107 bytes and a NUL; a longer path fails deep in bind().
[ ${#SOCK} -le 107 ] || die "socket path $SOCK is too long for a unix socket; set XDG_RUNTIME_DIR shorter"

# ── The disk ─────────────────────────────────────────────────────────────────
#
# The guest mounts its root read-write, so booting the image itself leaves
# whatever the probe wrote -- and, after a timeout, an unclean filesystem --
# for the next run to boot. A copy per run keeps the golden image what it was
# built as. --reflink shares the blocks where the filesystem can (btrfs, XFS);
# elsewhere it is a real copy, sparse so a mostly empty image stays small.
if [ "$COPY_ROOTFS" = 1 ]; then
    DISK_COPY=$LOGS/$TAG.rootfs.ext4
    cp --reflink=auto --sparse=always -- "$ROOTFS" "$DISK_COPY"
    DISK=$DISK_COPY
else
    DISK=$ROOTFS
fi

# ── The VMM's config ─────────────────────────────────────────────────────────
#
# The schema is nesbox's vmm/src/config.rs, which refuses unknown keys. The
# mixed casing is its own: kebab-case sections, snake_case fields inside
# boot-source, drives and machine-config, kebab-case inside gpu-forward and
# shared-directories.
CFG=$LOGS/$TAG.json
# earlyprintk: nesbox's COM1 exists for this, and goes to the same console
# log -- a guest that dies before virtio-console is up says why there, instead
# of leaving an empty log. panic=-1: a panic (a probe that exits as PID 1, an
# oops in the module at probe) reboots, which ends nesbox at once rather than
# at the timeout.
BOOT_ARGS="console=hvc0 earlyprintk=serial,ttyS0 panic=-1 root=/dev/vda rw init=/opt/nvgpu/$PROBE${CMDLINE_EXTRA:+ $CMDLINE_EXTRA}"
SHARES=
if [ -n "$NVIDIA_SHARE" ]; then
    SHARES=",
  \"shared-directories\": [
    { \"tag\": \"nvidia\", \"path-on-host\": $(json_str "$NVIDIA_SHARE"), \"read-only\": true }
  ]"
fi
cat > "$CFG" <<JSON
{
  "boot-source": {
    "kernel_image_path": $(json_str "$KERNEL"),
    "boot_args": $(json_str "$BOOT_ARGS")
  },
  "drives": [
    { "drive_id": "rootfs", "path_on_host": $(json_str "$DISK"), "is_root_device": true, "is_read_only": false }
  ],
  "machine-config": { "vcpu_count": $VCPUS, "mem_size_mib": $MEM_MIB },
  "gpu-forward": { "socket": $(json_str "$SOCK") }$SHARES
}
JSON

# ── The backend ──────────────────────────────────────────────────────────────
#
# The log file is opened here, by whoever runs this; the backend meters every
# log call site (device/src/ratelimit.rs), so a guest cannot grow it at will.
BLOG=$LOGS/$TAG.backend.log
CONSOLE=$LOGS/$TAG.console.log
echo "backend: as $NVGPU_USER ($PRIV, $LAYOUT layout)${BACKEND_ARGS[*]:+, with ${BACKEND_ARGS[*]}}" >&2
RUST_LOG=${RUST_LOG:-info} "${AS_BACKEND[@]}" \
    "$BACKEND_EXE" --socket "$SOCK" "${BACKEND_ARGS[@]}" \
    > "$BLOG" 2>&1 &
BACKEND=$!

# The backend must be listening before the VMM connects, and the socket must
# be the backend's: anything else at that path is not ours to connect to.
for _ in $(seq 1 50); do
    [ -S "$SOCK" ] && break
    kill -0 "$BACKEND" 2>/dev/null || break
    sleep 0.1
done
kill -0 "$BACKEND" 2>/dev/null || { echo "backend exited; see $BLOG" >&2; tail -n 5 "$BLOG" >&2; exit 1; }
[ -S "$SOCK" ] || { echo "backend never created $SOCK; see $BLOG" >&2; exit 1; }
[ "$(stat -c %u "$SOCK")" = "$BACKEND_UID" ] || {
    echo "$SOCK is not owned by $NVGPU_USER; refusing to connect" >&2
    exit 1
}

# ── The guest ────────────────────────────────────────────────────────────────
#
# The config path is nesbox's one positional argument. stdin is /dev/null:
# nesbox puts a terminal on stdin into raw mode for the guest console, and a
# VMM killed by the timeout never puts it back. It runs in the background and
# is waited for, so that an interrupt here reaches the cleanup, which stops
# it, instead of leaving it to run out its timeout.
echo "guest:   $PROBE on $(basename -- "$DISK"), $VCPUS vCPU / $MEM_MIB MiB, ${TIMEOUT}s" >&2
if [ "$INTERACTIVE" = 1 ]; then
    # The terminal is the guest's console; the log still gets everything.
    # --foreground: timeout otherwise moves itself and nesbox into a process
    # group of their own, which the terminal stops (SIGTTIN/SIGTTOU) as soon
    # as nesbox reads it or makes it raw. An explicit <&0 because a
    # background job of a script otherwise reads /dev/null.
    echo "guest console on this terminal; exit the guest's shell to power off" >&2
    TTY_STATE=$(stty -g 2>/dev/null) || TTY_STATE=
    : > "$CONSOLE"
    timeout --foreground -k 10 "$TIMEOUT" "$VMM" "$CFG" <&0 > "$CONSOLE" 2>&1 &
    VMM_PID=$!
    tail -n +1 -f --pid="$VMM_PID" "$CONSOLE" &
    TAIL_PID=$!
else
    timeout -k 10 "$TIMEOUT" "$VMM" "$CFG" < /dev/null > "$CONSOLE" 2>&1 &
    VMM_PID=$!
    TAIL_PID=
fi
RC=0
wait "$VMM_PID" || RC=$?
VMM_PID=
[ -z "$TAIL_PID" ] || wait "$TAIL_PID" 2>/dev/null || true
if [ -n "$TTY_STATE" ]; then
    stty "$TTY_STATE" 2>/dev/null || true
    TTY_STATE=
    echo
fi

sleep 1
BACKEND_ALIVE=1
kill -0 "$BACKEND" 2>/dev/null || BACKEND_ALIVE=0
echo "backend: $BLOG"
echo "console: $CONSOLE"
echo "config:  $CFG"

# ── What the probe said ──────────────────────────────────────────────────────
#
# The rig's probes (guest-image/probes) end with one verdict line,
#   NVGPU_PROBE_DONE probe=<name> result=PASS|FAIL pass=N fail=N skip=N
# and that line is the result: the tools they run print PASS/FAIL lines of
# their own, some of them expected (cuda-smoke says FAIL where a probe wants
# it to fail). The console comes through a terminal, so lines end in \r.
DONE=$(grep -a -E '^NVGPU_PROBE_DONE ' "$CONSOLE" | tail -n 1 | tr -d '\r' || true)
if [ -n "$DONE" ]; then
    grep -a -E '^\[[A-Za-z0-9_-]+\] FAIL ' "$CONSOLE" | tr -d '\r' | head -n 20 | sed 's/^/  /' || true
    [ "$BACKEND_ALIVE" = 1 ] || echo "note: the backend had exited by the end of the run; see $BLOG"
    echo "probe:   ${DONE#NVGPU_PROBE_DONE }"
    if [ "$RC" = 124 ] || [ "$RC" = 137 ]; then
        echo "result: TIMEOUT -- the probe finished but the guest did not power off within ${TIMEOUT}s"
        exit 124
    fi
    [ "$RC" = 0 ] || echo "note: the VMM exited with status $RC"
    case $DONE in
        *" result=PASS"*) echo "result: PASS"; exit 0 ;;
        *"reason=timeout"*) echo "result: FAIL (the probe's own watchdog)"; exit 1 ;;
        *) echo "result: FAIL"; exit 1 ;;
    esac
fi
# Older probes (the root layout's) print only PASS and FAIL lines; a word
# match, so "FAILED" in a kernel message is not counted, but the probe's own
# "FAIL: ..." is.
NPASS=$(grep -c -E '(^|[^[:alnum:]_])PASS([^[:alnum:]_]|$)' "$CONSOLE" || true)
NFAIL=$(grep -c -E '(^|[^[:alnum:]_])FAIL([^[:alnum:]_]|$)' "$CONSOLE" || true)
grep -E '(^|[^[:alnum:]_])FAIL([^[:alnum:]_]|$)' "$CONSOLE" | head -n 20 | sed 's/^/  /' || true
[ "$BACKEND_ALIVE" = 1 ] || echo "note: the backend had exited by the end of the run; see $BLOG"
if [ "$RC" = 124 ] || [ "$RC" = 137 ]; then
    echo "result: TIMEOUT -- the guest did not power off within ${TIMEOUT}s ($NPASS PASS, $NFAIL FAIL)"
    exit 124
fi
[ "$RC" = 0 ] || echo "note: the VMM exited with status $RC"
if [ "$NFAIL" -gt 0 ]; then
    echo "result: FAIL ($NPASS PASS, $NFAIL FAIL)"
    exit 1
elif [ "$NPASS" -eq 0 ]; then
    echo "result: NO RESULT -- no PASS or FAIL line on the console"
    exit 2
fi
echo "result: PASS ($NPASS PASS)"
