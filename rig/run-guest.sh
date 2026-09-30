#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Boot a guest against the vhost-user backend and keep both sides' output.
#
# Usage: run-guest.sh [--vmm nesbox|crosvm] [display options] <probe-name> [tag]
#                     [-- backend-args...]
#   --vmm       which VMM boots the guest (default nesbox, or NVGPU_VMM_KIND);
#               crosvm is unprivileged (rig layout), and takes --allow-compute
#               only if it has the UVM aperture (patches/crosvm 0007-0009),
#               see launcher/crosvm.sh
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
# Capture injection (SECURITY.md, "Capture injection"; unprivileged runs only):
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
#   NVGPU_WINDOW_MIB    the shared window in MiB (the backend's --window-size;
#                       default 1024, a multiple of 64); the VMM takes the size
#                       from the backend -- nesbox from virtio-nvgpu-v4, which
#                       is checked for when it is not 1024
#   NVGPU_WINDOW_SHARE  the percent of each window zone one guest process may
#                       hold (the backend's --window-owner-share; default 50)
#   NVGPU_RIG           the rig directory (bin/ kernel/ guest/ logs/); default
#                       the repo's .rig when not root
#   NVGPU_BACKEND NVGPU_VMM NVGPU_KERNEL NVGPU_ROOTFS NVGPU_LOGS
#                       override one path of whichever layout is in use
#   NVGPU_VCPUS NVGPU_MEM_MIB   guest size (rig 4 / 4096, root 2 / 2048)
#
# Placement (DEPLOY.md, "vCPU placement"; unset: the host scheduler places
# everything and the guest is told its VMM's default topology, as before;
# launcher/placement.sh):
#   NVGPU_PIN           a layout worked out from this host's topology (SMT
#                       cores, L3 domains, the scheduler's preferred cores,
#                       read from sysfs): cores (one thread of each of
#                       NVGPU_VCPUS whole cores, in one L3 domain where they
#                       fit), smt (both threads of NVGPU_VCPUS/2 cores, the
#                       guest told so), spread (cores, alternating L3
#                       domains), l3 (one set, a whole L3 domain) or
#                       core-sets (vCPU i on either thread of core i); then
#                       :l3=CPU (that CPU's domain first), :avoid=LIST (CPUs
#                       whose cores no vCPU takes; default 0, avoid=none),
#                       :io=none|siblings|other|LIST (the VMM's other threads
#                       and the backend). The pieces below, given too, win
#                       over what it says for each. rig/pin-layout.sh prints
#                       a layout without running anything
#   NVGPU_CPU_AFFINITY  host CPUs the vCPU threads may run on, a list such as
#                       8-15 (nesbox cpu_affinity; crosvm --cpu-affinity)
#   NVGPU_VCPU_PINS     one host CPU per vCPU, in order, such as 8,9,10,11
#                       (nesbox vcpu_pins; crosvm --cpu-affinity 0=8:1=9:..),
#                       or a CPU list per vCPU, colon-separated, such as
#                       8,24:9,25 (nesbox needs patches/nesbox/0002): only
#                       for CPUs nothing else is scheduled on
#   NVGPU_IO_AFFINITY   host CPUs for the VMM's other threads (nesbox
#                       io_affinity; crosvm started under taskset, its
#                       vCPUs then placing themselves); the backend's too
#                       unless NVGPU_BACKEND_CPUS says otherwise
#   NVGPU_BACKEND_CPUS  host CPUs for every backend thread (taskset)
#   NVGPU_GUEST_SMT     1 or 2: the guest is told each core has that many
#                       threads (nesbox threads_per_core; crosvm --no-smt for
#                       1). 2 needs one CPU per vCPU, vCPUs 2k and 2k+1 on
#                       one host core's two threads (checked), and a
#                       core-scheduling cookie shared by the VM's vCPUs.
#                       Unset: nesbox tells one, crosvm two for an even count
#   NVGPU_HUGEPAGES     nesbox: transparent (its default: prefaulted and
#                       collapsed into THP), 2m or 1g (the hugetlb pool, which
#                       must be reserved); crosvm: transparent adds
#                       --hugepages (MADV_HUGEPAGE)
#   NVGPU_PREFAULT=0    fault guest RAM in on first touch instead (nesbox;
#                       crosvm with patches/crosvm 0011, --prefault-memory)
#   NVGPU_SLICE_US      the EEVDF slice of every VMM and backend thread, in
#                       microseconds (chrt --other --sched-runtime; 100 to
#                       100000, no privilege needed): a shorter slice than
#                       the host's default (about 3 ms) gets a vCPU or the
#                       queue thread back onto a busy CPU sooner after it
#                       wakes, at the same share of the CPU. Default 100;
#                       0 leaves the host's default
#   NVGPU_CROSVM_CORE_SCHED=0  crosvm: --core-scheduling=false (its default
#                       gives each vCPU a core-scheduling cookie of its own,
#                       which idles the SMT sibling while a vCPU runs)
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
#   NVGPU_USER          root only: who the backend runs as (launcher/root.sh)
#   NVGPU_SANDBOX       on (default) or off: the backend's process sandbox
#                       (device/src/sandbox.rs; off is for diagnosis only, and
#                       as root needs NVGPU_DIAGNOSTIC=1)
#   NVGPU_DIAGNOSTIC=1  root only: allow what is for diagnosis alone --
#                       NVGPU_SANDBOX=off, NVGPU_ALLOW_ROOT_UNSAFE=1 and every
#                       diagnostic backend flag (vhost-user-nvgpu --diagnostic
#                       --help) -- which a root run otherwise refuses. Each
#                       one in effect is said on the terminal, as root or not
#   NVGPU_VM_SLOTS      root only: how many pool slots to look through (64)
#   NVGPU_VMM_JAIL      root only: on (default), auto or off -- the VMM under
#                       nesbox's jailer, as its slot's uid (launcher/root.sh)
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
#                       window (NVGPU_WINDOW_MIB), or the run is refused
#                       (default 4096)
#   NVGPU_SKIP_MEM_CHECK=1  run even so
#   NVGPU_LOG_MAX_MIB   the most of each log (<tag>.backend.log,
#                       <tag>.console.log) kept, in MiB (default 64): the guest
#                       writes the console, and a guest that writes it in a
#                       loop would otherwise fill the logs' filesystem. Past
#                       it, the backend's log drops the rest; the console's
#                       stops the VM (it is written directly, not through a
#                       pipe, which stalled guests)
#   NVGPU_OOM_SCORE_ADJ oom_score_adj for the launcher, backend and VMM
#                       (default 1000: the OOM killer takes the VM before the
#                       compositor; empty leaves it alone)
#   NVGPU_KMS_CARD_FORCE=1  allow --kms-card although a Wayland compositor's
#                       socket is still there (launcher/settings.sh)
#
# Exit status: the probe's NVGPU_PROBE_DONE verdict when it printed one (0
# PASS, 1 FAIL); otherwise 0 when the console shows a PASS and no FAIL, 1 on
# any FAIL line, 2 when it shows neither. 124 when the guest did not power
# off in time.
# ---
#
# Leaves, in the logs directory ($NVGPU_RIG/logs, or /root/logs as root);
# as root, with a pool slot, <tag> is <tag>.vmN, so VMs in two slots never
# share a name; and a run whose <tag>.json another live run holds is refused:
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
# of it and of its pieces beside it --
#   sudo install -D -o root -g root -m 0755 -t /root/bin rig/run-guest.sh
#   sudo install -D -o root -g root -m 0644 -t /root/bin/launcher rig/launcher/*.sh
# -- and a root-owned tree for it.
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
# The rest is in rig/launcher/, beside this file, and is sourced from there
# before anything else runs -- as root, each piece only once it is root's, as
# this file is (root_owned, below):
#   common.sh        helpers: JSON strings, text fit for a terminal, the
#                    capped log writer, CPU lists, still_ours
#   settings.sh      the command line and the environment, checked
#   placement.sh     where the threads run and the guest's CPU topology:
#                    NVGPU_PIN's layouts from the host's topology
#   root.sh          what a root run alone does -- root's files, the slot,
#                    the backend's and the VMM's users, the run's directory,
#                    the VMM's jail, the socket opened to it -- and, in its
#                    header, why
#   unprivileged.sh  what an unprivileged run alone does, and what it gives up
#   nesbox.sh        nesbox's window check, config and command line
#   crosvm.sh        crosvm's: what it is refused, the host checks, the
#                    command line
#   backend.sh       the compositor check, the backend started, its socket
#                    checked
#   guest.sh         the hook, and the guest run with its console capped
#   verdict.sh       the result, from the console
# This file calls them, in the order they run.
# shellcheck source-path=SCRIPTDIR
set -euo pipefail

# ── As root, a clean environment ─────────────────────────────────────────────
#
# What root runs is named by paths it checks (root_owned, below), but a
# helper found on a user's PATH, or a library LD_PRELOAD or LD_LIBRARY_PATH
# names, would be run or loaded all the same (sudo -E, an env_keep rule).
# So root starts again with only this launcher's own variables, RUST_LOG,
# TERM, NESBOX_VIRTIOFSD (checked below like any path) and the two the
# live-desktop warnings read, and a PATH of root's directories. What bash
# takes from the environment before this line runs -- the `bash` the #!
# line finds on PATH, BASH_ENV, SHELLOPTS, functions exported as
# BASH_FUNC_* -- no line here can undo: start it as root only through
# something that drops those (sudo's default env_reset does; `sudo -E`
# with a rule that keeps them does not).
CLEAN_PATH=/usr/sbin:/usr/bin:/sbin:/bin:/run/current-system/sw/bin
if [ "$EUID" = 0 ]; then
    KEEP=(HOME=/root "PATH=$CLEAN_PATH")
    DIRTY=0
    [ "${PATH:-}" = "$CLEAN_PATH" ] || DIRTY=1
    for v in $(compgen -e); do
        case $v in
            NVGPU_* | RUST_LOG | TERM | NESBOX_VIRTIOFSD | XDG_RUNTIME_DIR | WAYLAND_DISPLAY)
                KEEP+=("$v=${!v}") ;;
            # What bash itself exports, and what the list above sets.
            PATH | HOME | PWD | OLDPWD | SHLVL | _) ;;
            *) DIRTY=1 ;;
        esac
    done
    if [ "$DIRTY" = 1 ]; then
        [ -x /usr/bin/env ] || { echo "run-guest: no /usr/bin/env to start again with" >&2; exit 1; }
        exec /usr/bin/env -i "${KEEP[@]}" "$BASH" "$0" "$@"
    fi
fi

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

# ── The launcher's pieces ────────────────────────────────────────────────────
#
# Found beside this file, through its resolved path. As root, this file and
# each piece are root_owned first: whoever could change one could have root
# run what they liked. Each is read by the path the check resolved it to:
# a link on the way there may be in a directory someone else can write, and
# be pointed elsewhere once the check has passed.
if [ "$(id -u)" = 0 ]; then PRIV=root; else PRIV=user; fi
if [ $PRIV = root ]; then
    # What root creates -- the VMM's config, the logs, the backend's socket
    # until it is opened to the VMM alone -- is written by no one else,
    # whatever umask root was started with: a config another user could
    # rewrite before the VMM reads it names what the VMM opens.
    umask "$(printf '%04o' $((8#$(umask) | 8#022)))"
    SELF=$(root_owned "$0" "the launcher")
else
    SELF=$(realpath -e -- "$0")
fi
LIB=${SELF%/*}/launcher
declare -A PIECE
for piece in common settings placement root unprivileged nesbox crosvm backend guest verdict; do
    if [ $PRIV = root ]; then
        PIECE[$piece]=$(root_owned "$LIB/$piece.sh" "the launcher's $piece.sh")
    else
        PIECE[$piece]=$LIB/$piece.sh
    fi
done
# shellcheck source=launcher/common.sh
. "${PIECE[common]}"
# shellcheck source=launcher/settings.sh
. "${PIECE[settings]}"
# shellcheck source=launcher/placement.sh
. "${PIECE[placement]}"
# shellcheck source=launcher/root.sh
. "${PIECE[root]}"
# shellcheck source=launcher/unprivileged.sh
. "${PIECE[unprivileged]}"
# shellcheck source=launcher/nesbox.sh
. "${PIECE[nesbox]}"
# shellcheck source=launcher/crosvm.sh
. "${PIECE[crosvm]}"
# shellcheck source=launcher/backend.sh
. "${PIECE[backend]}"
# shellcheck source=launcher/guest.sh
. "${PIECE[guest]}"
# shellcheck source=launcher/verdict.sh
. "${PIECE[verdict]}"

# ── What is asked for ────────────────────────────────────────────────────────
parse_args "$@"
backend_settings
name_run
choose_layout
gate_diagnostics
if [ "$VMM_KIND" = crosvm ]; then
    crosvm_allowed
fi
run_settings
guest_settings
check_inputs
placement_core_sched_check
if [ $PRIV = root ]; then
    root_only_files
fi

# ── The VMM, and this host ───────────────────────────────────────────────────
CROSVM_NO_HP=0
if [ "$VMM_KIND" = crosvm ]; then
    crosvm_check_host
else
    nesbox_check_window
fi
mkdir -p "$LOGS"
host_limits
kms_card_check

# ── A slot of the pool: this VM's own host users ────────────────────────────
#
# As root (take_slot), first, so that nothing below can mistake another VM's
# processes for this one's.
SLOT=
VMM_USER=
SLOT_GROUP=
exec {SLOT_FD}</dev/null
if [ $PRIV = root ]; then
    take_slot
fi

# ── This run's name ──────────────────────────────────────────────────────────
#
# Two runs with one tag would overwrite each other's disk copy, config and
# logs, and the later one's cleanup would delete the other's. As root, VMs in
# different slots run at once, so the slot goes into the name; and whatever
# the mode, a run whose config another live run holds (flock, for the run's
# length; children are started with it closed) is refused.
if [ -n "$SLOT" ]; then
    TAG=$TAG.vm$SLOT
fi
exec {TAG_FD}>>"$LOGS/$TAG.json"
flock -n "$TAG_FD" ||
    die "another run with the tag $TAG is running (it holds $LOGS/$TAG.json); give this one another tag"

# ── Who the backend runs as ──────────────────────────────────────────────────
BACKEND_NETNS=()
if [ $PRIV = root ]; then
    root_backend_user
else
    user_backend_user
fi

# ── Who the VMM runs as ──────────────────────────────────────────────────────
VMM_JAIL=off
VMM_NETNS=()
VMM_OWN_NETNS=false
if [ $PRIV = root ]; then
    root_vmm_user
elif [ "${NVGPU_VMM_NETNS:-1}" = 1 ]; then
    VMM_OWN_NETNS=true
fi
if [ "$COPY_ROOTFS" = auto ]; then
    if [ "$VMM_JAIL" = on ]; then COPY_ROOTFS=1; else COPY_ROOTFS=0; fi
fi

check_wayland

# ── A stale backend or VMM holds the GPU and confuses the logs ───────────────
#
# Unprivileged, the rig's own are killed (kill_stale).
#
# As root, nothing is killed by pattern. Other VMs run beside this one: their
# backends may be the same user as this one's (the compositor's owner with
# --wayland-socket, the export directory's with --wayland-export, NVGPU_USER),
# and their VMMs root (NVGPU_VMM_JAIL=off), so any pattern for "a backend" or
# "a VMM" reaches them too. And there is nothing of this run's own to find: a
# slot is taken only when its users run nothing (take_slot).
RUN_PARENT=${XDG_RUNTIME_DIR:-$RIG/run}
if [ $PRIV = user ]; then
    kill_stale
fi

# ── Cleanup, however the run ends ────────────────────────────────────────────
BACKEND=
BACKEND_MARK=
VMM_PID=
VMM_MARK=
RUN=
DISK_COPY=
TTY_STATE=
JAIL_BUILT=
cleanup() {
    if [ -n "$VMM_PID" ] && still_ours "$VMM_PID" "${VMM_MARK:-}"; then
        kill "$VMM_PID" 2>/dev/null || true
    fi
    # nesbox puts the terminal in raw mode; one killed never restores it.
    [ -z "$TTY_STATE" ] || stty "$TTY_STATE" 2>/dev/null || true
    if [ -n "$BACKEND" ] && still_ours "$BACKEND" "${BACKEND_MARK:-}"; then
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
# The socket lives in a fresh directory no one else can change: whoever
# listens at the path the VMM connects to receives the guest's memory, so it
# must not be a fixed name in a shared directory like /tmp, where another
# user could bind it first. (Without --socket the backend would pick
# $XDG_RUNTIME_DIR/nvgpu/nvgpu.sock; a directory per run also keeps two runs
# apart.)
if [ $PRIV = root ]; then
    root_run_dir
else
    user_run_dir
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
    # The rig is one user for everything, so the helper is the backend's own
    # uid, which the backend accepts only as a diagnostic.
    BACKEND_ARGS+=(--inject-socket "$INJECT_SOCK" --inject-uid "$BACKEND_UID" --allow-inject-self)
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
    give_disk_to_vmm
fi

# ── The VMM's config ─────────────────────────────────────────────────────────
#
# nesbox's is a config file (nesbox_config); crosvm's, a command line that
# <tag>.json records (crosvm_config).
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
CROSVM_NOTE=
if [ "$VMM_KIND" = crosvm ]; then
    crosvm_config
else
    nesbox_config
fi

# ── The VMM's command line ───────────────────────────────────────────────────
JAIL_ROOT=
if [ "$VMM_JAIL" = on ]; then
    jail_vmm
elif [ "$VMM_KIND" = crosvm ]; then
    crosvm_cmd
else
    nesbox_cmd
fi
VMM_CMD=("${SLICE[@]}" "${VMM_CMD[@]}")

# ── The backend ──────────────────────────────────────────────────────────────
BLOG=$LOGS/$TAG.backend.log
CONSOLE=$LOGS/$TAG.console.log
start_backend
await_socket
if [ $PRIV = root ]; then
    socket_to_vmm
fi

# ── The hook ─────────────────────────────────────────────────────────────────
if [ -n "${NVGPU_BEFORE_VMM:-}" ]; then
    run_hook
fi

# ── The guest ────────────────────────────────────────────────────────────────
run_guest

sleep 1
BACKEND_ALIVE=1
kill -0 "$BACKEND" 2>/dev/null || BACKEND_ALIVE=0
echo "backend: $BLOG"
echo "console: $CONSOLE"
echo "config:  $CFG"

# ── What the probe said ──────────────────────────────────────────────────────
verdict
