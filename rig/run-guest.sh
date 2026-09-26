#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Boot a guest against the vhost-user backend and keep both sides' output.
#
# Usage: run-guest.sh [--vmm nesbox|crosvm] [display options] <probe-name> [tag]
#                     [-- backend-args...]
#   --vmm       which VMM boots the guest (default nesbox, or NVGPU_VMM_KIND);
#               crosvm is unprivileged (rig layout), and takes --allow-compute
#               only if it has the UVM aperture (the virtio-nvgpu-compute
#               branch), see "crosvm" below
#   probe-name  a script under /opt/nvgpu in the guest rootfs, e.g. stage1.sh;
#               in the rig layout a bare name gets .sh added (stage1 ->
#               /opt/nvgpu/stage1.sh)
#   tag         names the log and config, so runs do not overwrite each other
#   backend-args  anything else for the backend, as is (--permissive-abi,
#               --keep-guest-coherency, ...; see vhost-user-nvgpu --help);
#               --permissive-abi and --rm-allowlist=log are said aloud
#
# Display options, passed through to the backend (TESTING.md has which mode
# wants which, rig/TESTING-RIG.md which of them are safe next to a live desktop):
#   --kms-card                  compositor-VM: offer the host's card nodes
#   --wayland-socket PATH       the host compositor's socket, for the proxy
#   --wayland-lease             offer the compositor's DRM lease device
#   --wayland-export PATH       accept host Wayland clients at PATH
#   --wayland-max-conns N       Wayland channels per VM (backend default 64)
#   --wayland-shm-budget MIB    wl_shm memory per VM (default 1024)
#   --wayland-queue-budget MIB  unread compositor output per VM (default 256)
#   --wayland-lease-interval S  seconds between lease requests (default 5)
#
# Capture injection (SECURITY.md §18; unprivileged runs only):
#   --inject                    give the backend --inject-socket in the run's
#                               directory (inject.sock), for peers of this
#                               user's uid; NVGPU_INJECT_SOCKET names it to the
#                               NVGPU_BEFORE_VMM hook (rig/rig-tools/inject-hook.sh)
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
#   NVGPU_BEFORE_VMM    unprivileged only: a command run (bash -c) once the
#                       backend listens and before the VMM starts, with
#                       NVGPU_RUN_DIR, NVGPU_INJECT_SOCKET (with --inject) and
#                       NVGPU_HOOK_LOG (<tag>.hook.log) set; the last line it
#                       prints is added to the guest kernel command line
#                       (letters, digits and . _ , : = - only). The capture
#                       probe's ids and tokens come this way
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
#   NVGPU_PREFIX        root only: where the root layout's tree is (see below);
#                       as root, this or NVGPU_RIG must be given
#   NVGPU_USER          root only: who the backend runs as (see below)
#   NVGPU_SANDBOX       on (default) or off: the backend's process sandbox
#                       (device/src/sandbox.rs; off is for diagnosis only, and
#                       as root needs NVGPU_DIAGNOSTIC=1)
#   NVGPU_DIAGNOSTIC=1  root only: allow what is for diagnosis alone --
#                       NVGPU_SANDBOX=off, NVGPU_ALLOW_ROOT_UNSAFE=1 -- which
#                       a root run otherwise refuses
#   NVGPU_VM_SLOTS      root only: how many pool slots to look through (64)
#   NVGPU_VMM_JAIL      root only: on (default), auto or off -- the VMM under
#                       nesbox's jailer, as its slot's uid (see below)
#   NVGPU_JAILER        root only: the jailer binary (default: beside the VMM)
#   NVGPU_JAIL_ROOT     root only: a jail image to use instead of the one built
#                       for the run (nesbox's build/output/jail); nesbox must be
#                       at /usr/bin/nesbox in it
#   NVGPU_VMM_NETNS     unprivileged only: 1 (default) has nesbox leave the
#                       host's network ("unshare-network"), and starts crosvm
#                       in a user and network namespace of its own; 0 does not
#   NVGPU_VMM_KIND      nesbox (default) or crosvm, the same as --vmm
#   NVGPU_CROSVM_SANDBOX  on (default) or off: crosvm's own sandbox (a minijail
#                       per device process, with its seccomp policy); off runs
#                       crosvm with --disable-sandbox, and says so loudly on
#                       the console log. For diagnosis only
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
# out under NVGPU_PREFIX (/root, as it always was) as:
#   $NVGPU_PREFIX/vhost-user-nvgpu          backend binary
#   $NVGPU_PREFIX/nesbox/target/release/nesbox
#   $NVGPU_PREFIX/kernel/vmlinux, $NVGPU_PREFIX/guest/rootfs.ext4
#   $NVGPU_PREFIX/logs
# (NVGPU_RIG, or the single-path overrides, point it elsewhere.) The layout is
# never assumed: as root, NVGPU_PREFIX or NVGPU_RIG must be set, so that a
# sudo from someone's checkout does not quietly run what is under /root.
#
# As root, this launcher runs, reads and writes only root's files: itself,
# the backend, the VMM, the jailer and virtiofsd, the kernel, the rootfs, the
# share, the jail image and the logs directory must each be root's, and so
# must every directory above them, none writable by group or others (a
# sticky one aside). Anything else is refused: whoever could change one of
# them could have root run, read or overwrite something of their choosing --
# a rig of the user's own, a launcher in a user's checkout. Install a copy
# (sudo install -D -o root -g root -m 0755 rig/run-guest.sh
# /root/bin/run-guest.sh) and a root-owned tree for it.
#
# As an ordinary user, with the rig layout (rig/TESTING-RIG.md): the user needs
# /dev/kvm and the NVIDIA nodes, and nothing here needs root. Paths come from
# $NVGPU_RIG, laid out as
#   bin/vhost-user-nvgpu  bin/nesbox  [bin/virtiofsd]
#   kernel/vmlinux        guest/rootfs.ext4        logs/
# The rootfs there is the golden image: each run boots a copy of it
# (cp --reflink, so on btrfs or XFS the copy costs nothing until the guest
# writes), and deletes the copy afterwards, so a run can never leave the next
# one a dirty or half-written filesystem. rig/rig-preflight.sh says
# whether this host is ready for it.
#
# The backend does NOT run as root. RM, DRM and NVKMS take a guest's
# privilege from the backend's credentials, so a root backend makes every
# guest process an RM administrator with all of BAR0 mappable -- the host
# kernel, one DMA away -- and the backend refuses to start that way
# (device/src/posture.rs).
#
# Run as root, each VM takes a slot of its own from a pool of host users, so
# that no two VMs are one host principal (SECURITY.md, "One uid per VM", says
# what that separates and what it does not). Slot N is two users:
#
#   for i in 0 1 2 3; do
#     useradd --system --no-create-home --shell /usr/sbin/nologin --user-group nvgpu-vm$i
#     useradd --system --no-create-home --shell /usr/sbin/nologin -g nvgpu-vm$i nvgpu-vmm$i
#   done
#
# (NixOS: users.users."nvgpu-vm0" = { isSystemUser = true; group = "nvgpu-vm0"; },
# users.groups."nvgpu-vm0" = {}, users.users."nvgpu-vmm0" = { isSystemUser =
# true; group = "nvgpu-vm0"; }, and so on.)
#
# nvgpu-vmN runs the slot's backend, with the groups of the device nodes it
# opens and nothing else -- setpriv hands them over, so the account needs no
# membership (/dev/nvidia* is 0666; render and card nodes are group render and
# video; /dev/udmabuf is group kvm) -- no capabilities and no_new_privs; the
# backend drops whatever it is given anyway. nvgpu-vmmN runs the slot's VMM
# under nesbox's jailer, in group nvgpu-vmN and no other: the backend's socket
# is made 0660 in that group, which is how the VMM reaches it and all it
# reaches of the backend's. A slot is free when no run holds its lock
# (/run/nvgpu-slots/N.lock) and neither of its users has a live process; the
# first free one is taken, and none free stops the run. Pools are counted from
# nvgpu-vm0 up to the first missing user, at most NVGPU_VM_SLOTS.
#
# Which user the backend is: slot N's nvgpu-vmN. With --wayland-socket it is
# the socket's owner instead -- the backend is then a client of that user's
# compositor like any of the user's own programs, and the socket's directory
# ($XDG_RUNTIME_DIR, 0700) lets no one else in anyway. With --wayland-export
# (and no --wayland-socket) it is the owner of the directory the export
# socket goes in, since the backend accepts only clients of its own uid
# (device/src/wl/export.rs). In those two modes every VM's backend is that
# one user, and what keeps them apart is the backend's sandbox (Landlock's
# signal and socket scoping, ABI 6 and 9) and its being undumpable; the VMM
# still takes a slot. NVGPU_USER overrides all of these, and makes every VM
# started with it one user, which is said. NVGPU_USER=root runs it as root,
# which it refuses unless NVGPU_ALLOW_ROOT_UNSAFE=1 and NVGPU_DIAGNOSTIC=1 as
# well: that passes --allow-root-unsafe, and every guest process is then an
# RM administrator. Only for ruling the credentials out while chasing
# something, never for a guest you do not trust. A host with no pool but the older single user
# "nvgpu" runs every backend as that user, with a warning each time.
#
# The VMM, run as root: under nesbox's jailer (tools/jailer in nesbox; built
# with `cargo build --release -p jailer`, and found beside the VMM binary or
# at NVGPU_JAILER), chrooted into a jail image built for the run -- nesbox
# itself, and virtiofsd with its libraries when there is a share -- with the
# config's kernel, disk, socket and share bound in, as the slot's nvgpu-vmmN.
# The jailer clears supplementary groups, so /dev/kvm must be 0666 (systemd's
# default). The disk must be writable by that user: a per-run copy is made
# for it (NVGPU_COPY_ROOTFS defaults to 1 here), and an image booted in place
# must already be. Without a jailer, a pool or a 0666 /dev/kvm the run is
# refused; NVGPU_VMM_JAIL=off runs the VMM as root, as before, and auto does
# the same, with a warning, where one of them is missing.
#
# The socket's directory is the backend user's while the backend binds its
# socket, and root's from then on: the launcher checks the socket there, and
# opens it to the slot's group, only once nobody but root can rename, replace
# or link anything in it (the backend's user could otherwise swap the socket
# for a symlink to a file of root's, and have root give that to the slot's
# group).
#
# Both halves run as root in network namespaces of their own (unshare --net):
# neither needs a network, the vhost-user socket is a path, and a namespace
# made by root still hears the kernel's uevents. NVGPU_SANDBOX=off leaves the
# backend in the host's.
#
# Run as an ordinary user, one VM's backend and VMM are that user: nobody
# else is available without privilege, and it is who owns the compositor
# socket and the export directory anyway. setpriv still gives the backend
# no_new_privs and empty inheritable and ambient capability sets. It cannot
# change the uid, the groups or the bounding set -- all three need privilege
# -- so the backend keeps the user's own supplementary groups: a group that is
# root in all but name (docker, libvirt, disk) is then within reach of
# anything that takes the backend over, and rig-preflight.sh warns about
# those. The backend's sandbox still applies: it enters a user and network
# namespace of its own (where unprivileged user namespaces are allowed), and
# Landlock and seccomp confine it. nesbox leaves the host's network the same
# way ("unshare-network"). What sharing the uid loses, against a slot of its
# own: the VMM (seccomp, but no Landlock) can open whatever the user can, and
# can signal the user's processes and, on a host with kernel.yama.ptrace_scope
# 0, trace them; RM's security token -- the uid -- is the user's own, the same
# as every GPU program on the desktop; and two VMs started by one user are one
# principal to RM and to the kernel. Keep a rig for one VM at a time.
#
# crosvm (--vmm crosvm, rig layout: bin/crosvm, built from the virtio-nvgpu
# branch in .rig/src/crosvm, patches/crosvm/): the same kernel, disk, console
# log, probe and backend; the guest's device is crosvm's vhost-user frontend
# of type nvgpu. crosvm has no config file, so <tag>.json records the command
# line it was given. Unprivileged, crosvm runs with its sandbox: every device
# it emulates itself (disk, consoles, rng) is a process of its own in a
# minijail -- user, pid, mount and network namespaces, pivoted into an empty
# directory (the rig's run/crosvm-empty), under its seccomp policy -- and so,
# with patches/crosvm 0007-0009, is the nvgpu vhost-user frontend: it checks
# the backend's mapping requests and passes them to the main process, which
# checks each again against the regions it laid out and makes it (window
# mappings; with --allow-compute, UVM pools in the aperture). A crosvm without
# those patches keeps the frontend in its main process, as upstream has it,
# and takes no --allow-compute. With NVGPU_VMM_NETNS=1 the main
# process is started in a user and network namespace of its own (unshare),
# the counterpart of nesbox's "unshare-network". No virtiofs share: crosvm's
# has no read-only mode, so NVGPU_NVIDIA_SHARE must be empty.
#
# Not implemented, as root: what production would use is crosvm as the slot's
# nvgpu-vmmN (setpriv, with only the slot's group and kvm), with its sandbox
# on and /var/empty as the pivot root, in a network namespace made by root,
# and the backend's socket 0660 in the slot's group -- the same slot, the same
# socket arrangement and the same disk copy as nesbox's jailer gets above. The
# launcher refuses --vmm crosvm as root until that has been built and run.
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

# root_owned PATH WHAT: print PATH with every symlink resolved, once it and
# every directory above it are root's and writable by no one else (a sticky
# directory aside: in one, only an entry's owner may rename or remove it).
# Used as root, for everything root runs, reads or writes: then nobody but
# root can change what the path names between this check and its use.
root_owned() {
    local p d st uid mode
    p=$(realpath -e -- "$1" 2>/dev/null) || die "$2 $1 does not exist"
    d=$p
    while :; do
        st=$(stat -c '%u %a' -- "$d") || die "$2: cannot stat $d"
        uid=${st% *} mode=${st#* }
        [ "$uid" = 0 ] ||
            die "$2 $1: $d is not root's (uid $uid); as root, this launcher runs, reads and" \
                "writes only root's files (the header of $0)"
        if [ $((8#$mode & 8#022)) -ne 0 ] && { [ ! -d "$d" ] || [ $((8#$mode & 8#1000)) -eq 0 ]; }; then
            die "$2 $1: $d is writable by others than root (mode $mode)"
        fi
        [ "$d" = / ] && break
        d=$(dirname -- "$d")
    done
    printf '%s\n' "$p"
}

BACKEND_ARGS=()
WL_SOCK=
WL_EXPORT=
KMS_CARD=0
COMPUTE=0
INJECT=0
[ "${NVGPU_COMPUTE:-0}" = 1 ] && COMPUTE=1
VMM_KIND=${NVGPU_VMM_KIND:-nesbox}
POSITIONAL=()
while [ $# -gt 0 ]; do
    case $1 in
        --allow-compute)
            COMPUTE=1
            shift
            ;;
        --inject)
            INJECT=1
            shift
            ;;
        --vmm)
            [ $# -ge 2 ] || usage
            VMM_KIND=$2
            shift 2
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
case $VMM_KIND in nesbox | crosvm) ;; *) die "--vmm $VMM_KIND: nesbox or crosvm" ;; esac
CROSVM_SANDBOX=${NVGPU_CROSVM_SANDBOX:-on}
case $CROSVM_SANDBOX in on | off) ;; *) die "NVGPU_CROSVM_SANDBOX=$CROSVM_SANDBOX: on or off" ;; esac
# Compute is the backend's to serve and the probe's to expect: one switch for
# both (rig/guest-image/probes/render.sh reads nvgpu_compute).
[ "$COMPUTE" = 1 ] && BACKEND_ARGS+=(--allow-compute)
SANDBOX=${NVGPU_SANDBOX:-on}
case $SANDBOX in on | off) ;; *) die "NVGPU_SANDBOX=$SANDBOX: on or off" ;; esac
SANDBOX_GIVEN=0
PERMISSIVE_ABI=0
RM_ALLOWLIST_LOG=0
ALLOW_ROOT_ARG=0
prev=
for a in ${BACKEND_ARGS[@]+"${BACKEND_ARGS[@]}"}; do
    case $a in
        --sandbox=*) SANDBOX=${a#--sandbox=} SANDBOX_GIVEN=1 ;;
        --permissive-abi) PERMISSIVE_ABI=1 ;;
        --rm-allowlist=log) RM_ALLOWLIST_LOG=1 ;;
        --allow-root-unsafe) ALLOW_ROOT_ARG=1 ;;
    esac
    [ "$prev" = --sandbox ] && SANDBOX=$a SANDBOX_GIVEN=1
    [ "$prev" = --rm-allowlist ] && [ "$a" = log ] && RM_ALLOWLIST_LOG=1
    prev=$a
done
if [ "$SANDBOX" = off ]; then
    [ "$SANDBOX_GIVEN" = 1 ] || BACKEND_ARGS+=(--sandbox=off)
    echo "run-guest: WARNING: the backend's sandbox is off (NVGPU_SANDBOX=off): for" \
        "diagnosis only" >&2
fi
# Passed through as asked, but never quietly: each forwards what the backend
# would otherwise refuse.
[ "$PERMISSIVE_ABI" = 0 ] ||
    echo "run-guest: WARNING: --permissive-abi: ioctls with no ABI profile are forwarded" \
        "unchecked; for finding what a new driver needs, never for a guest you do not trust" >&2
[ "$RM_ALLOWLIST_LOG" = 0 ] ||
    echo "run-guest: WARNING: --rm-allowlist=log: RM controls and classes off the allowlist" \
        "reach the host's RM, logged instead of refused; for diagnosis only" >&2

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
    VMM=${NVGPU_VMM:-$RIG/bin/$VMM_KIND}
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
    [ -n "${NVGPU_PREFIX:-}" ] ||
        die "as root, name the tree: NVGPU_PREFIX (the root layout, e.g. /root) or" \
            "NVGPU_RIG (a rig of root's); see the header of $0"
    PREFIX=${NVGPU_PREFIX%/}
    BACKEND_BIN=${NVGPU_BACKEND:-$PREFIX/vhost-user-nvgpu}
    VMM=${NVGPU_VMM:-$PREFIX/nesbox/target/release/nesbox}
    KERNEL=${NVGPU_KERNEL:-$PREFIX/kernel/vmlinux}
    ROOTFS=${NVGPU_ROOTFS:-$PREFIX/guest/rootfs.ext4}
    LOGS=${NVGPU_LOGS:-$PREFIX/logs}
    VCPUS=${NVGPU_VCPUS:-2}
    MEM_MIB=${NVGPU_MEM_MIB:-2048}
    # Booted in place, unless the VMM is jailed (below): its user must be
    # able to write the disk, and a copy is made its own.
    COPY_ROOTFS=${NVGPU_COPY_ROOTFS:-auto}
    # Unset means the share this layout always had; set but empty means none.
    NVIDIA_SHARE=${NVGPU_NVIDIA_SHARE-/var/lib/nvgpu}
fi

# ── What is for diagnosis alone, as root ─────────────────────────────────────
#
# A root run is the one other people's guests get, so the switches that take
# a layer of confinement away are refused there unless asked for as what they
# are. (An option meaning the same sets NVGPU_DIAGNOSTIC=1 before this.)
DIAGNOSTIC=${NVGPU_DIAGNOSTIC:-0}
case $DIAGNOSTIC in 0 | 1) ;; *) die "NVGPU_DIAGNOSTIC=$DIAGNOSTIC: 0 or 1" ;; esac
if [ $PRIV = root ] && [ "$DIAGNOSTIC" != 1 ]; then
    [ "$SANDBOX" = on ] ||
        die "the backend's sandbox off (NVGPU_SANDBOX=off, --sandbox=off) as root is for" \
            "diagnosis only: NVGPU_DIAGNOSTIC=1 as well"
    [ "${NVGPU_ALLOW_ROOT_UNSAFE:-}" != 1 ] && [ "$ALLOW_ROOT_ARG" = 0 ] ||
        die "a root backend (NVGPU_ALLOW_ROOT_UNSAFE=1, --allow-root-unsafe) is for" \
            "diagnosis only: NVGPU_DIAGNOSTIC=1 as well"
fi
[ "$DIAGNOSTIC" = 0 ] ||
    echo "run-guest: WARNING: NVGPU_DIAGNOSTIC=1: a diagnostic run, not one to keep" >&2

if [ "$VMM_KIND" = crosvm ]; then
    [ $PRIV = user ] ||
        die "--vmm crosvm runs unprivileged only for now; the header says what root would need"
    [ -z "$NVIDIA_SHARE" ] ||
        die "--vmm crosvm has no read-only virtiofs share; NVGPU_NVIDIA_SHARE must be empty"
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

# ── As root, only root's files ───────────────────────────────────────────────
#
# The header says why. Each path is replaced by its resolved form, which from
# here on only root can change.
if [ $PRIV = root ]; then
    root_owned "$0" "the launcher" >/dev/null
    BACKEND_BIN=$(root_owned "$BACKEND_BIN" "the backend")
    VMM=$(root_owned "$VMM" "the VMM")
    KERNEL=$(root_owned "$KERNEL" "the guest kernel")
    ROOTFS=$(root_owned "$ROOTFS" "the rootfs")
    if [ -n "$NVIDIA_SHARE" ]; then
        NVIDIA_SHARE=$(root_owned "$NVIDIA_SHARE" "the share (NVGPU_NVIDIA_SHARE)")
        # What nesbox would start for it: its NESBOX_VIRTIOFSD, or the one on
        # PATH, named explicitly from here on.
        VFS=${NESBOX_VIRTIOFSD:-$(command -v virtiofsd || true)}
        if [ -n "$VFS" ]; then
            NESBOX_VIRTIOFSD=$(root_owned "$VFS" "virtiofsd (NESBOX_VIRTIOFSD)")
            export NESBOX_VIRTIOFSD
        fi
    fi
    # The logs directory is made if missing, so its nearest existing
    # ancestor is checked first.
    d=$LOGS
    while [ ! -e "$d" ]; do d=$(dirname -- "$d"); done
    root_owned "$d" "the logs directory's parent" >/dev/null
    mkdir -p -- "$LOGS"
    LOGS=$(root_owned "$LOGS" "the logs directory")
fi

# ── crosvm: the host GPU's PCI address has to be free in the guest ──────────
#
# The guest driver gives each GPU the host's own PCI address, and it cannot
# make a bus the VMM already has: the guest logs "cannot put the GPU at its
# host address" and has no GPU device. crosvm's root bus is bus 0, and it puts
# an empty hot-plug root port on the first free bus, bus 1 -- where most hosts
# have their GPU -- unless told --no-pci-hotplug-port (patches/crosvm/0003).
# The GPUs are the ones the backend serves: those nvidia.ko lists.
CROSVM_NO_HP=0
if [ "$VMM_KIND" = crosvm ]; then
    CROSVM_HELP=$("$VMM" run --help 2>&1 || true)
    case $CROSVM_HELP in *--no-pci-hotplug-port*) CROSVM_NO_HP=1 ;; esac
    for g in /proc/driver/nvidia/gpus/*; do
        [[ ${g##*/} =~ ^0000:([0-9a-fA-F]{2}): ]] || continue
        case $((16#${BASH_REMATCH[1]})) in
            0) die "the host GPU ${g##*/} is on PCI bus 0, crosvm's root bus in the guest:" \
                "the guest driver cannot give it that address" ;;
            1) [ "$CROSVM_NO_HP" = 1 ] ||
                die "the host GPU ${g##*/} is on PCI bus 1, where $VMM puts its hot-plug" \
                    "root port, and it has no --no-pci-hotplug-port to leave it out: build" \
                    "it with patches/crosvm (rig/rig-build-crosvm.sh)" ;;
        esac
    done
    [ "$CROSVM_NO_HP" = 1 ] ||
        echo "run-guest: note: $VMM has no --no-pci-hotplug-port; its hot-plug root port" \
            "takes PCI bus 1" >&2
    # Compute needs the UVM aperture (patches/crosvm 0007-0009), which the
    # help of --vhost-user names. A crosvm without it publishes the window
    # alone: the guest would get a UVM device whose pools it cannot map, and
    # CUDA would fail late.
    case $CROSVM_HELP in *nvgpu-uvm-aperture*) CROSVM_UVM=1 ;; *) CROSVM_UVM=0 ;; esac
    [ "$COMPUTE" = 0 ] || [ "$CROSVM_UVM" = 1 ] ||
        die "--allow-compute: $VMM has no UVM aperture; build the virtio-nvgpu-compute" \
            "branch (patches/crosvm 0007-0009) or drop --allow-compute"
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
# Group C in rig/TESTING-RIG.md stops the compositor first; a socket still in the
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
            "(rig/TESTING-RIG.md group C); NVGPU_KMS_CARD_FORCE=1 if that socket is stale"
fi

# ── A slot of the pool: this VM's own host users ────────────────────────────
#
# Taken first, as root, so that nothing below can mistake another VM's
# processes for this one's. The lock is held by this launcher for the run; a
# launcher killed outright lets go of it while its VM may still run, which is
# why a slot whose users have live processes is not free either. Children
# are started with the lock's descriptor closed ({SLOT_FD}>&- below).
SLOT=
VMM_USER=
SLOT_GROUP=
exec {SLOT_FD}</dev/null
if [ $PRIV = root ]; then
    mkdir -p /run/nvgpu-slots
    chmod 0700 /run/nvgpu-slots
    MAX_SLOTS=${NVGPU_VM_SLOTS:-64}
    [[ $MAX_SLOTS =~ ^[0-9]+$ ]] || die "NVGPU_VM_SLOTS=$MAX_SLOTS: a number"
    for ((i = 0; i < MAX_SLOTS; i++)); do
        id "nvgpu-vm$i" >/dev/null 2>&1 || break
        exec {SLOT_FD}>&-
        exec {SLOT_FD}>>"/run/nvgpu-slots/$i.lock"
        flock -n "$SLOT_FD" || continue
        if pgrep -u "nvgpu-vm$i" >/dev/null ||
            { id "nvgpu-vmm$i" >/dev/null 2>&1 && pgrep -u "nvgpu-vmm$i" >/dev/null; }; then
            echo "run-guest: slot $i is unlocked but its users still run something; skipping it" >&2
            continue
        fi
        SLOT=$i
        break
    done
    if [ -z "$SLOT" ]; then
        exec {SLOT_FD}>&-
        exec {SLOT_FD}</dev/null
        if [ "$i" -gt 0 ]; then
            die "all $i slots of the pool (nvgpu-vm0..nvgpu-vm$((i - 1))) are in use"
        fi
    else
        SLOT_GROUP=nvgpu-vm$SLOT
        getent group "$SLOT_GROUP" >/dev/null ||
            die "no group $SLOT_GROUP: make the pool's users with --user-group (the top of $0)"
        if id "nvgpu-vmm$SLOT" >/dev/null 2>&1; then
            VMM_USER=nvgpu-vmm$SLOT
            [ "$(id -gn "$VMM_USER")" = "$SLOT_GROUP" ] ||
                die "$VMM_USER's group is $(id -gn "$VMM_USER"), not $SLOT_GROUP (the top of $0)"
        fi
        echo "run-guest: slot $SLOT (nvgpu-vm$SLOT${VMM_USER:+, $VMM_USER})" >&2
    fi
fi

# ── Who the backend runs as ──────────────────────────────────────────────────
if [ $PRIV = root ]; then
    if [ -n "${NVGPU_USER:-}" ]; then
        echo "run-guest: NVGPU_USER=$NVGPU_USER: every backend started this way is that one" \
            "user, and those VMs are one principal to the host" >&2
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
    elif [ -n "$SLOT" ]; then
        NVGPU_USER=nvgpu-vm$SLOT
    elif id nvgpu >/dev/null 2>&1; then
        NVGPU_USER=nvgpu
        echo "run-guest: WARNING: no pool of VM users (nvgpu-vm0, ...); every VM's backend" \
            "is the one user nvgpu, and can signal the others, read their files and share" \
            "RM's security token. Make the pool: the top of $0" >&2
    else
        die "no user to run the backend as: make the pool of VM users (the top of $0)"
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
                "RM administrator. NVGPU_ALLOW_ROOT_UNSAFE=1 NVGPU_DIAGNOSTIC=1 if you mean it." >&2
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

# As root, the backend's network namespace is made here, where that needs no
# user namespace and the namespace still hears the kernel's uevents; the
# backend finds only loopback and keeps it (device/src/sandbox.rs).
# Unprivileged, the backend makes its own.
BACKEND_NETNS=()
if [ $PRIV = root ] && [ "$SANDBOX" = on ]; then
    BACKEND_NETNS=(unshare --net --)
fi

# ── Who the VMM runs as ──────────────────────────────────────────────────────
#
# As root: the slot's nvgpu-vmmN, under nesbox's jailer, in a network
# namespace of its own. nesbox's own "unshare-network" cannot be used there:
# the kernel refuses a user namespace to a chrooted process.
VMM_JAIL=off
VMM_NETNS=()
VMM_OWN_NETNS=false
if [ $PRIV = root ]; then
    VMM_JAIL=${NVGPU_VMM_JAIL:-on}
    case $VMM_JAIL in auto | on | off) ;; *) die "NVGPU_VMM_JAIL=$VMM_JAIL: on, auto or off" ;; esac
    JAILER=${NVGPU_JAILER:-$(dirname -- "$VMM")/jailer}
    why=
    if [ "$VMM_JAIL" != off ]; then
        if [ -z "$VMM_USER" ]; then
            why="no pool slot with a VMM user (nvgpu-vmmN; the top of $0)"
        elif [ ! -x "$JAILER" ]; then
            why="no jailer at $JAILER (NVGPU_JAILER; cargo build --release -p jailer in nesbox)"
        elif [ "$(stat -L -c %a /dev/kvm 2>/dev/null)" != 666 ]; then
            why="/dev/kvm is not 0666, and the jailer clears supplementary groups"
        fi
    fi
    if [ "$VMM_JAIL" = off ]; then
        echo "run-guest: WARNING: the VMM runs as root (NVGPU_VMM_JAIL=off)" >&2
    elif [ -n "$why" ]; then
        [ "$VMM_JAIL" = on ] &&
            die "the VMM would run as root, unjailed: $why (NVGPU_VMM_JAIL=auto or off to" \
                "allow that)"
        echo "run-guest: WARNING: the VMM runs as root, unjailed: $why" >&2
        VMM_JAIL=off
    else
        VMM_JAIL=on
        # Run by root, so root's alone.
        JAILER=$(root_owned "$JAILER" "the jailer (NVGPU_JAILER)")
    fi
    VMM_NETNS=(unshare --net --)
elif [ "${NVGPU_VMM_NETNS:-1}" = 1 ]; then
    VMM_OWN_NETNS=true
fi
if [ "$COPY_ROOTFS" = auto ]; then
    if [ "$VMM_JAIL" = on ]; then COPY_ROOTFS=1; else COPY_ROOTFS=0; fi
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
            "appear on the live desktop (rig/TESTING-RIG.md says which stages want that)" >&2
    fi
fi
if [ $PRIV = user ] && [ -n "$WL_EXPORT" ]; then
    [ -d "$(dirname -- "$WL_EXPORT")" ] && [ -w "$(dirname -- "$WL_EXPORT")" ] ||
        die "--wayland-export $WL_EXPORT: its directory must exist and be $NVGPU_USER's"
fi

# ── A stale backend or VMM holds the GPU and confuses the logs ───────────────
#
# Unprivileged, the rig is one VM at a time (the header), and a stale one of
# its own is killed: processes of this user started from exactly this
# launcher's backend binary and socket directory, and VMMs started from this
# VMM binary with a config in this logs directory. The patterns are anchored
# at the start of the command line, so an editor or a grep with the path in
# its arguments is not one of them.
#
# As root, nothing is killed by pattern. Other VMs run beside this one: their
# backends may be the same user as this one's (the compositor's owner with
# --wayland-socket, the export directory's with --wayland-export, NVGPU_USER),
# and their VMMs root (NVGPU_VMM_JAIL=off), so any pattern for "a backend" or
# "a VMM" reaches them too. And there is nothing of this run's own to find: a
# slot is taken only when its users run nothing (above).
RUN_PARENT=${XDG_RUNTIME_DIR:-$RIG/run}
if [ $PRIV = user ]; then
    pkill -u "$(id -u)" -f "^$(re "$BACKEND_BIN") --socket $(re "$RUN_PARENT")/nvgpu-run\." || true
    if [ "$VMM_KIND" = crosvm ]; then
        pkill -u "$(id -u)" -f "^$(re "$VMM") run .*--vhost-user type=nvgpu,socket=$(re "$RUN_PARENT")/nvgpu-run\." || true
    else
        pkill -u "$(id -u)" -f "^$(re "$VMM") $(re "$LOGS")/[^/ ]+\.json" || true
    fi
fi

# ── Cleanup, however the run ends ────────────────────────────────────────────
BACKEND=
VMM_PID=
VMM_MARK=
RUN=
DISK_COPY=
TTY_STATE=
JAIL_BUILT=
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
    if [ -n "$VMM_PID" ] && still_ours "$VMM_PID" "${VMM_MARK:-}"; then
        kill "$VMM_PID" 2>/dev/null || true
    fi
    # nesbox puts the terminal in raw mode; one killed never restores it.
    [ -z "$TTY_STATE" ] || stty "$TTY_STATE" 2>/dev/null || true
    if [ -n "$BACKEND" ] && still_ours "$BACKEND" "${SOCK:-}"; then
        kill "$BACKEND" 2>/dev/null || true
    fi
    [ -z "$RUN" ] || rm -rf "$RUN"
    [ -z "$JAIL_BUILT" ] || rm -rf "$JAIL_BUILT"
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
# The capture helper's socket, for this user's own processes (the rig's
# helper is nvgpu-inject-test, run by the NVGPU_BEFORE_VMM hook).
INJECT_SOCK=
if [ "$INJECT" = 1 ]; then
    [ $PRIV = user ] || die "--inject: unprivileged runs only (the helper would be another user's)"
    INJECT_SOCK=$RUN/inject.sock
    BACKEND_ARGS+=(--inject-socket "$INJECT_SOCK" --inject-uid "$BACKEND_UID")
fi

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
# A jailed VMM opens the disk as its slot's user: the copy is made its own,
# and an image booted in place must be already.
if [ "$VMM_JAIL" = on ]; then
    if [ -n "$DISK_COPY" ]; then
        chown "$VMM_USER:$SLOT_GROUP" "$DISK_COPY"
        chmod 0600 "$DISK_COPY"
    elif [ "$(stat -c %U "$DISK")" != "$VMM_USER" ]; then
        die "$DISK is booted in place (NVGPU_COPY_ROOTFS=0) but is not $VMM_USER's to write"
    fi
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
# crosvm puts console=, earlycon= and panic=-1 on the command line itself,
# for the consoles it is given, and is handed the rest.
CONSOLE_ARGS="console=hvc0 earlyprintk=serial,ttyS0 panic=-1"
BOOT_ARGS="$CONSOLE_ARGS root=/dev/vda rw init=/opt/nvgpu/$PROBE${CMDLINE_EXTRA:+ $CMDLINE_EXTRA}"
SHARES=
if [ -n "$NVIDIA_SHARE" ]; then
    SHARES=",
  \"shared-directories\": [
    { \"tag\": \"nvidia\", \"path-on-host\": $(json_str "$NVIDIA_SHARE"), \"read-only\": true }
  ]"
fi
CROSVM_NOTE=
if [ "$VMM_KIND" = crosvm ]; then
    # The same guest, said as crosvm's command line. Two consoles to the one
    # log, as nesbox has: hvc0 (virtio-console) and COM1 for earlyprintk.
    # The queue size is the backend's: crosvm offers 32768 unless told. No
    # hot-plug root port: crosvm puts it on PCI bus 1, and the guest driver
    # gives the GPU the host's own PCI address, commonly 0000:01:00.0 (a
    # crosvm without the option was checked above to need none).
    CONSOLE_OPTS=type=stdout,hardware=virtio-console,console
    [ "$INTERACTIVE" = 1 ] && CONSOLE_OPTS=$CONSOLE_OPTS,stdin
    VMM_ARGS=(run --cpus "$VCPUS" --mem "$MEM_MIB"
        --block "path=$DISK"
        --serial "$CONSOLE_OPTS"
        --serial "type=stdout,hardware=serial,num=1,earlycon"
        --vhost-user "type=nvgpu,socket=$SOCK,max-queue-size=256")
    [ "$CROSVM_NO_HP" = 0 ] || VMM_ARGS+=(--no-pci-hotplug-port)
    VMM_ARGS+=(-p "${BOOT_ARGS#"$CONSOLE_ARGS "}")
    case $DISK$SOCK in *,*) die "a comma in $DISK or $SOCK would split crosvm's option" ;; esac
    if [ "$CROSVM_SANDBOX" = on ]; then
        # minijail pivots each device process into an empty directory:
        # crosvm's default is /var/empty, which not every host has.
        EMPTY=$RIG/run/crosvm-empty
        mkdir -p "$EMPTY"
        chmod 0755 "$EMPTY"
        [ -z "$(ls -A -- "$EMPTY")" ] || die "$EMPTY must be empty: crosvm pivots its devices into it"
        VMM_ARGS+=(--pivot-root "$EMPTY")
    else
        VMM_ARGS+=(--disable-sandbox)
        CROSVM_NOTE="run-guest: WARNING: crosvm runs with --disable-sandbox (NVGPU_CROSVM_SANDBOX=off): every device it emulates is in its main process, with no minijail and no seccomp policy. For diagnosis only"
        echo "$CROSVM_NOTE" >&2
    fi
    VMM_ARGS+=("$KERNEL")
    VMM_MARK=socket=$SOCK
    {
        printf '{\n  "vmm": "crosvm",\n  "argv": [\n    %s' "$(json_str "$VMM")"
        for a in "${VMM_ARGS[@]}"; do printf ',\n    %s' "$(json_str "$a")"; done
        printf '\n  ]\n}\n'
    } > "$CFG"
else
VMM_MARK=$CFG
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
  "gpu-forward": { "socket": $(json_str "$SOCK") },
  "unshare-network": $VMM_OWN_NETNS$SHARES
}
JSON
fi

# ── The VMM's jail ───────────────────────────────────────────────────────────
#
# The jail image is the jailer's read-only lower layer: here nesbox (at
# /usr/bin/nesbox, where the jailer looks) and, for a share, virtiofsd, each
# with the loader and libraries it names, at their own paths. Everything the
# config names -- kernel, disk, socket, share, the config itself -- the jailer
# binds in at the same path. NVGPU_JAIL_ROOT uses a materialized image instead.
jail_add() {
    local src=$1 dst=$2 out lib
    install -D -m 0755 -- "$src" "$JAIL_ROOT$dst"
    command -v ldd >/dev/null || die "no ldd to find what $src links against; NVGPU_JAIL_ROOT"
    out=$(ldd -- "$src" 2>&1) || true
    case $out in *"not a dynamic executable"* | *"statically linked"*) return 0 ;; esac
    while read -r lib; do
        [ -n "$lib" ] && [ ! -e "$JAIL_ROOT$lib" ] || continue
        install -D -m 0755 -- "$(realpath -- "$lib")" "$JAIL_ROOT$lib"
    done < <(printf '%s\n' "$out" | awk '$2 == "=>" && $3 ~ /^\// { print $3 } $1 ~ /^\// { print $1 }')
    if [ -e /etc/ld.so.cache ] && [ ! -e "$JAIL_ROOT/etc/ld.so.cache" ]; then
        install -D -m 0644 /etc/ld.so.cache "$JAIL_ROOT/etc/ld.so.cache"
    fi
}
JAIL_ROOT=
if [ "$VMM_JAIL" = on ]; then
    if [ -n "${NVGPU_JAIL_ROOT:-}" ]; then
        JAIL_ROOT=$(root_owned "$NVGPU_JAIL_ROOT" "the jail image (NVGPU_JAIL_ROOT)")
        [ -x "$JAIL_ROOT/usr/bin/nesbox" ] || die "NVGPU_JAIL_ROOT=$JAIL_ROOT has no usr/bin/nesbox"
    else
        JAIL_ROOT=$(mktemp -d /run/nvgpu-jail.XXXXXX)
        JAIL_BUILT=$JAIL_ROOT
        chmod 0755 "$JAIL_ROOT"
        jail_add "$VMM" /usr/bin/nesbox
        if [ -n "$NVIDIA_SHARE" ]; then
            VFS=${NESBOX_VIRTIOFSD:-$(command -v virtiofsd || true)}
            [ -n "$VFS" ] || die "a share needs virtiofsd in the jail, and none is on PATH (NESBOX_VIRTIOFSD)"
            VFS=$(realpath -- "$VFS")
            jail_add "$VFS" "$VFS"
            export NESBOX_VIRTIOFSD=$VFS
        fi
    fi
    VMM_CMD=("${VMM_NETNS[@]}" "$JAILER" --config "$CFG" --jail-root "$JAIL_ROOT"
        --uid "$(id -u "$VMM_USER")" --gid "$(getent group "$SLOT_GROUP" | cut -d: -f3)")
elif [ "$VMM_KIND" = crosvm ]; then
    # Unprivileged, crosvm leaves the host's network the only way it can: in
    # a user namespace of its own that maps just this user, where minijail
    # still makes each device's namespaces beneath it.
    if [ "$VMM_OWN_NETNS" = true ]; then
        VMM_CMD=(unshare --user --map-current-user --net -- "$VMM" "${VMM_ARGS[@]}")
    else
        VMM_CMD=("$VMM" "${VMM_ARGS[@]}")
    fi
else
    VMM_CMD=("${VMM_NETNS[@]}" "$VMM" "$CFG")
fi

# ── The backend ──────────────────────────────────────────────────────────────
#
# The log file is opened here, by whoever runs this; the backend meters every
# log call site (device/src/ratelimit.rs), so a guest cannot grow it at will.
BLOG=$LOGS/$TAG.backend.log
CONSOLE=$LOGS/$TAG.console.log
# The backend refuses its diagnostic flags without --diagnostic
# (vhost-user-nvgpu --diagnostic --help). One here was asked for -- after --,
# or by NVGPU_SANDBOX=off or NVGPU_ALLOW_ROOT_UNSAFE=1 above -- so say so.
diag_prev=
for a in ${BACKEND_ARGS[@]+"${BACKEND_ARGS[@]}"}; do
    case $diag_prev/$a in
        */--diagnostic) break ;;
        */--allow-root-unsafe | */--proc-nvidia | */--proc-nvidia=* | */--permissive-abi | \
            */--keep-guest-coherency | */--allow-unmeasured-release | \
            */--rm-allowlist=log | --rm-allowlist/log | \
            */--sandbox=off | */--sandbox=best-effort | --sandbox/off | --sandbox/best-effort)
            BACKEND_ARGS+=(--diagnostic)
            break
            ;;
    esac
    diag_prev=$a
done
echo "backend: as $NVGPU_USER ($PRIV, $LAYOUT layout)${BACKEND_ARGS[*]:+, with ${BACKEND_ARGS[*]}}" >&2
RUST_LOG=${RUST_LOG:-info} "${BACKEND_NETNS[@]}" "${AS_BACKEND[@]}" \
    "$BACKEND_EXE" --socket "$SOCK" "${BACKEND_ARGS[@]}" \
    {SLOT_FD}>&- > "$BLOG" 2>&1 &
BACKEND=$!

# The backend must be listening before the VMM connects, and the socket must
# be the backend's: anything else at that path is not ours to connect to.
for _ in $(seq 1 50); do
    [ -S "$SOCK" ] && break
    kill -0 "$BACKEND" 2>/dev/null || break
    sleep 0.1
done
kill -0 "$BACKEND" 2>/dev/null || { echo "backend exited; see $BLOG" >&2; tail -n 5 "$BLOG" >&2; exit 1; }
# As root: the directory is root's from here on (0711: the backend's user may
# still reach its binary in it, but no longer rename, replace or link
# anything there), so what is checked below is what the VMM connects to and
# what root changes. $RUN itself lies in /run, which only root can change.
if [ $PRIV = root ]; then
    chown root:root -- "$RUN"
    chmod 0711 -- "$RUN"
fi
# The socket must be the backend's, a socket, and not a link to one: anything
# else at that path is not ours to connect to (or, as root, to open to a group).
[ ! -L "$SOCK" ] || { echo "$SOCK is a symbolic link, not the backend's socket; refusing it" >&2; exit 1; }
[ -S "$SOCK" ] || { echo "backend never created $SOCK; see $BLOG" >&2; exit 1; }
[ "$(stat -c %u -- "$SOCK")" = "$BACKEND_UID" ] || {
    echo "$SOCK is not owned by $NVGPU_USER; refusing to connect" >&2
    exit 1
}
# The jailed VMM is another user, in the slot's group: the socket is opened
# to that group, which holds no one else. (The jailer binds the socket file
# alone into the jail.) -h and the checks above: never through a link.
if [ "$VMM_JAIL" = on ]; then
    chgrp -h -- "$SLOT_GROUP" "$SOCK"
    chmod 0660 -- "$SOCK"
fi

# ── The hook ─────────────────────────────────────────────────────────────────
#
# What must happen between the backend and the guest: the capture test's
# helper imports its buffers now, and its ids and tokens reach the guest as
# kernel command-line words, added to the config already written.
if [ -n "${NVGPU_BEFORE_VMM:-}" ]; then
    [ $PRIV = user ] || die "NVGPU_BEFORE_VMM: unprivileged runs only"
    HOOK_WORDS=$(NVGPU_RUN_DIR=$RUN NVGPU_INJECT_SOCKET=$INJECT_SOCK NVGPU_HOOK_LOG=$LOGS/$TAG.hook.log \
        bash -c "$NVGPU_BEFORE_VMM" | tail -n 1) || die "NVGPU_BEFORE_VMM failed; see $LOGS/$TAG.hook.log"
    [[ $HOOK_WORDS =~ ^[A-Za-z0-9_.,:=\ -]*$ ]] || die "NVGPU_BEFORE_VMM printed characters a command line cannot take"
    if [ -n "$HOOK_WORDS" ]; then
        echo "hook:    adds to the guest command line: $HOOK_WORDS" >&2
        # nesbox's config, and crosvm's argv log, carry the boot line whole;
        # crosvm's own argv carries it after -p.
        sed -i "0,/init=\/opt\/nvgpu\/$PROBE/s//init=\/opt\/nvgpu\/$PROBE $HOOK_WORDS/" "$CFG"
        for i in "${!VMM_CMD[@]}"; do
            [ "${VMM_CMD[$i]}" = -p ] && VMM_CMD[i + 1]+=" $HOOK_WORDS"
        done
    fi
fi

# ── The guest ────────────────────────────────────────────────────────────────
#
# The config path is nesbox's one positional argument. stdin is /dev/null:
# nesbox puts a terminal on stdin into raw mode for the guest console, and a
# VMM killed by the timeout never puts it back. It runs in the background and
# is waited for, so that an interrupt here reaches the cleanup, which stops
# it, instead of leaving it to run out its timeout.
echo "guest:   $PROBE on $(basename -- "$DISK"), $VCPUS vCPU / $MEM_MIB MiB, ${TIMEOUT}s" >&2
if [ "$VMM_JAIL" = on ]; then
    echo "vmm:     as $VMM_USER under $JAILER, jail $JAIL_ROOT, own network namespace" >&2
fi
if [ "$VMM_KIND" = crosvm ]; then
    echo "vmm:     crosvm, sandbox $CROSVM_SANDBOX$([ "$CROSVM_SANDBOX" = on ] && [ "$CROSVM_UVM" = 1 ] && echo ', nvgpu frontend jailed')$([ "$VMM_OWN_NETNS" = true ] && echo ', own user and network namespace')" >&2
fi
# The console log starts with what the reader must know before the guest's
# first line: a VMM without its sandbox.
: > "$CONSOLE"
[ -z "$CROSVM_NOTE" ] || printf '%s\n' "$CROSVM_NOTE" >> "$CONSOLE"
if [ "$INTERACTIVE" = 1 ]; then
    # The terminal is the guest's console; the log still gets everything.
    # --foreground: timeout otherwise moves itself and nesbox into a process
    # group of their own, which the terminal stops (SIGTTIN/SIGTTOU) as soon
    # as nesbox reads it or makes it raw. An explicit <&0 because a
    # background job of a script otherwise reads /dev/null.
    echo "guest console on this terminal; exit the guest's shell to power off" >&2
    TTY_STATE=$(stty -g 2>/dev/null) || TTY_STATE=
    timeout --foreground -k 10 "$TIMEOUT" "${VMM_CMD[@]}" {SLOT_FD}>&- <&0 >> "$CONSOLE" 2>&1 &
    VMM_PID=$!
    tail -n +1 -f --pid="$VMM_PID" "$CONSOLE" &
    TAIL_PID=$!
else
    timeout -k 10 "$TIMEOUT" "${VMM_CMD[@]}" {SLOT_FD}>&- < /dev/null >> "$CONSOLE" 2>&1 &
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
# The rig's probes (rig/guest-image/probes) end with one verdict line,
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
