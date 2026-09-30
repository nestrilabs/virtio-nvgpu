# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's settings: the command line and the environment (the
# launcher's header lists both), each checked and turned into what the rest
# of the launcher uses. Sourced by the launcher; each function sets its
# globals, in the order the launcher calls them.

# The command line: the backend's flags (BACKEND_ARGS), the display,
# compute and inject switches, the VMM, the probe and the tag.
parse_args() {
    BACKEND_ARGS=()
    WL_SOCK=
    WL_EXPORT=
    KMS_CARD=0
    COMPUTE=0
    INJECT=0
    [ "${NVGPU_COMPUTE:-0}" = 1 ] && COMPUTE=1
    DMABUF_EXPORT=${NVGPU_DMABUF_EXPORT:-0}
    case $DMABUF_EXPORT in 0 | 1) ;; *) die "NVGPU_DMABUF_EXPORT=$DMABUF_EXPORT: 0 or 1" ;; esac
    VMM_KIND=${NVGPU_VMM_KIND:-nesbox}
    POSITIONAL=()
    while [ $# -gt 0 ]; do
        case $1 in
            --allow-compute)
                COMPUTE=1
                shift
                ;;
            --allow-dmabuf-export)
                DMABUF_EXPORT=1
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
                        --allow-dmabuf-export) DMABUF_EXPORT=1 ;;
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
}

# What the backend is given besides the command line's own flags.
backend_settings() {
    # Compute is the backend's to serve and the probe's to expect: one switch
    # for both (rig/guest-image/probes/render.sh reads nvgpu_compute).
    [ "$COMPUTE" = 1 ] && BACKEND_ARGS+=(--allow-compute)
    # RM's dma-buf export, the same way: one switch for the backend and for
    # the render probe (nvgpu_dmabuf_export).
    [ "$DMABUF_EXPORT" = 1 ] && BACKEND_ARGS+=(--allow-dmabuf-export)
    # Opt-in RM allowlist groups (the backend's --rm-allow-group; names it
    # knows, comma-separated; debug and profiling need compute, which the
    # backend checks) and the populate before an OS-descriptor pin (off
    # unless 1). Passed only when set, so an older backend still starts.
    if [ -n "${NVGPU_RM_ALLOW_GROUP:-}" ]; then
        [[ $NVGPU_RM_ALLOW_GROUP =~ ^[a-z]+(,[a-z]+)*$ ]] ||
            die "NVGPU_RM_ALLOW_GROUP=$NVGPU_RM_ALLOW_GROUP: group names, comma-separated"
        BACKEND_ARGS+=(--rm-allow-group "$NVGPU_RM_ALLOW_GROUP")
    fi
    case ${NVGPU_OSDESC_POPULATE:-} in
        '' | 0) ;;
        1) BACKEND_ARGS+=(--osdesc-populate on) ;;
        *) die "NVGPU_OSDESC_POPULATE=$NVGPU_OSDESC_POPULATE: 0 or 1" ;;
    esac
    # The window's size and per-process share: the backend's to take and to
    # check (it refuses what cannot be had), and the VMM's to follow. Passed
    # only when set, so an older backend named by NVGPU_BACKEND still starts.
    WINDOW_MIB=${NVGPU_WINDOW_MIB:-1024}
    [[ $WINDOW_MIB =~ ^[0-9]+$ ]] || die "NVGPU_WINDOW_MIB=$WINDOW_MIB: whole MiB"
    WINDOW_MIB=$((10#$WINDOW_MIB))
    [ -n "${NVGPU_WINDOW_MIB:-}" ] && BACKEND_ARGS+=(--window-size "$WINDOW_MIB")
    if [ -n "${NVGPU_WINDOW_SHARE:-}" ]; then
        [[ $NVGPU_WINDOW_SHARE =~ ^[0-9]+$ ]] || die "NVGPU_WINDOW_SHARE=$NVGPU_WINDOW_SHARE: a percent"
        BACKEND_ARGS+=(--window-owner-share "$NVGPU_WINDOW_SHARE")
    fi
    # The VM's video memory (the backend's --vram-limit; off unless set).
    if [ -n "${NVGPU_VRAM_LIMIT:-}" ]; then
        [[ $NVGPU_VRAM_LIMIT =~ ^[0-9]+$ ]] || die "NVGPU_VRAM_LIMIT=$NVGPU_VRAM_LIMIT: whole MiB"
        BACKEND_ARGS+=(--vram-limit "$((10#$NVGPU_VRAM_LIMIT))")
    fi
    SANDBOX=${NVGPU_SANDBOX:-on}
    case $SANDBOX in on | off) ;; *) die "NVGPU_SANDBOX=$SANDBOX: on or off" ;; esac
    SANDBOX_GIVEN=0
    ALLOW_ROOT_ARG=0
    prev=
    for a in ${BACKEND_ARGS[@]+"${BACKEND_ARGS[@]}"}; do
        case $a in
            --sandbox=*) SANDBOX=${a#--sandbox=} SANDBOX_GIVEN=1 ;;
            --allow-root-unsafe) ALLOW_ROOT_ARG=1 ;;
        esac
        [ "$prev" = --sandbox ] && SANDBOX=$a SANDBOX_GIVEN=1
        prev=$a
    done
    if [ "$SANDBOX" = off ] && [ "$SANDBOX_GIVEN" = 0 ]; then
        BACKEND_ARGS+=(--sandbox=off)
    fi
}

# The backend's diagnostic flags among BACKEND_ARGS (vhost-user-nvgpu
# --diagnostic --help), one per line with what each takes away. The backend
# refuses each without --diagnostic, and the launcher supplies that
# (start_backend): so the launcher is where each must be asked for as what
# it is.
diag_flags() {
    local prev= a
    for a in ${BACKEND_ARGS[@]+"${BACKEND_ARGS[@]}"}; do
        case $prev/$a in
            */--allow-root-unsafe) echo "--allow-root-unsafe: a root backend; every guest process is an RM administrator" ;;
            */--proc-nvidia | */--proc-nvidia=*) echo "--proc-nvidia: a /proc/driver/nvidia other than the host driver's" ;;
            */--permissive-abi) echo "--permissive-abi: ioctls with no ABI profile are forwarded unchecked" ;;
            */--keep-guest-coherency) echo "--keep-guest-coherency: guest system memory is not made GPU-coherent (the Intel PAT cover)" ;;
            */--allow-unmeasured-release) echo "--allow-unmeasured-release: the host driver may be a release the tables were not measured at" ;;
            */--allow-inject-self) echo "--allow-inject-self: every process of the backend's user may inject into this VM" ;;
            */--rm-allowlist=log | --rm-allowlist/log) echo "--rm-allowlist=log: RM calls off the allowlist reach the host's RM, logged instead of refused" ;;
            */--sandbox=off | --sandbox/off) echo "--sandbox=off: no network namespace, Landlock or seccomp for the backend" ;;
            */--sandbox=best-effort | --sandbox/best-effort) echo "--sandbox=best-effort: the backend runs with whatever sandbox layers this host lacks missing" ;;
        esac
        prev=$a
    done
}

# The probe and the tag, which name the run.
name_run() {
    PROBE=${POSITIONAL[0]}
    TAG=${POSITIONAL[1]:-$(date +%H%M%S)}
    # Both end up in file names, the kernel command line and a pkill pattern.
    [[ $PROBE =~ ^[A-Za-z0-9._-]+$ ]] || die "probe name \"$PROBE\": letters, digits, . _ - only"
    [[ $TAG =~ ^[A-Za-z0-9._-]+$ ]] || die "tag \"$TAG\": letters, digits, . _ - only"
}

# Which layout (the header's two). Privilege and layout are separate
# questions: root keeps the /root layout it always had unless NVGPU_RIG says
# otherwise; anyone else has only the rig.
choose_layout() {
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
        # Booted in place, unless the VMM is jailed (root_vmm_user): its user
        # must be able to write the disk, and a copy is made its own.
        COPY_ROOTFS=${NVGPU_COPY_ROOTFS:-auto}
        # Unset means the share this layout always had; set but empty means
        # none.
        NVIDIA_SHARE=${NVGPU_NVIDIA_SHARE-/var/lib/nvgpu}
    fi
}

# What is for diagnosis alone, as root. A root run is the one other
# people's guests get, so the switches that take a layer of confinement away
# are refused there unless asked for as what they are. (An option meaning the
# same sets NVGPU_DIAGNOSTIC=1 before this.)
gate_diagnostics() {
    DIAGNOSTIC=${NVGPU_DIAGNOSTIC:-0}
    case $DIAGNOSTIC in 0 | 1) ;; *) die "NVGPU_DIAGNOSTIC=$DIAGNOSTIC: 0 or 1" ;; esac
    if [ $PRIV = root ] && [ "$DIAGNOSTIC" != 1 ]; then
        [ "$SANDBOX" = on ] ||
            die "the backend's sandbox off (NVGPU_SANDBOX=off, --sandbox=off) as root is for" \
                "diagnosis only: NVGPU_DIAGNOSTIC=1 as well"
        [ "${NVGPU_ALLOW_ROOT_UNSAFE:-}" != 1 ] && [ "$ALLOW_ROOT_ARG" = 0 ] ||
            die "a root backend (NVGPU_ALLOW_ROOT_UNSAFE=1, --allow-root-unsafe) is for" \
                "diagnosis only: NVGPU_DIAGNOSTIC=1 as well"
        DIAG_ASKED=$(diag_flags | cut -d: -f1 | tr '\n' ' ')
        [ -z "$DIAG_ASKED" ] ||
            die "diagnostic backend flags as root (${DIAG_ASKED% }) are for diagnosis only:" \
                "NVGPU_DIAGNOSTIC=1 as well"
    fi
    [ "$DIAGNOSTIC" = 0 ] ||
        echo "run-guest: WARNING: NVGPU_DIAGNOSTIC=1: a diagnostic run, not one to keep" >&2
}

# The guest's command line, whether the run is interactive, and its
# timeout.
run_settings() {
    CMDLINE_EXTRA=${NVGPU_CMDLINE_EXTRA:-}
    case " $CMDLINE_EXTRA " in
        *" nvgpu_compute="*) ;;
        *) CMDLINE_EXTRA="${CMDLINE_EXTRA:+$CMDLINE_EXTRA }nvgpu_compute=$COMPUTE" ;;
    esac
    # The groups the backend serves, for the probes that test them
    # (rig/verify/sec-negative.c).
    case " $CMDLINE_EXTRA " in
        *" nvgpu_rm_groups="*) ;;
        *) [ -z "${NVGPU_RM_ALLOW_GROUP:-}" ] ||
            CMDLINE_EXTRA="${CMDLINE_EXTRA:+$CMDLINE_EXTRA }nvgpu_rm_groups=$NVGPU_RM_ALLOW_GROUP" ;;
    esac
    case " $CMDLINE_EXTRA " in
        *" nvgpu_dmabuf_export="*) ;;
        *) CMDLINE_EXTRA="$CMDLINE_EXTRA nvgpu_dmabuf_export=$DMABUF_EXPORT" ;;
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
}

# The paths made absolute, the guest's size, where its threads run
# (the header's "Placement") and the scheduler slice.
guest_settings() {
    # Absolute and tidy, but not through symlinks: the stale-process
    # patterns (kill_stale) match the paths exactly as they are executed.
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

    # Where the threads run, and the guest's topology (launcher/placement.sh).
    placement_settings
    HUGEPAGES=${NVGPU_HUGEPAGES:-}
    PREFAULT=${NVGPU_PREFAULT:-}
    case $HUGEPAGES in '' | transparent | 2m | 1g) ;; *) die "NVGPU_HUGEPAGES=$HUGEPAGES: transparent, 2m or 1g" ;; esac
    case $PREFAULT in '' | 0 | 1) ;; *) die "NVGPU_PREFAULT=$PREFAULT: 0 or 1" ;; esac
    SLICE_US=${NVGPU_SLICE_US:-100}
    [[ $SLICE_US =~ ^[0-9]+$ ]] && { [ "$SLICE_US" = 0 ] || { [ "$SLICE_US" -ge 100 ] && [ "$SLICE_US" -le 100000 ]; }; } ||
        die "NVGPU_SLICE_US=$SLICE_US: 0, or 100 to 100000"
    # The backend sets its own threads' slice too (--sched-slice-us, 0 keeps
    # what it inherits); both say the same.
    BACKEND_ARGS+=(--sched-slice-us "$SLICE_US")
    SLICE=()
    if [ "$SLICE_US" != 0 ]; then
        # Inherited by every thread either process makes, and kept across the
        # exec of what each runs (the jailer, unshare, setpriv).
        if chrt --other --sched-runtime $((SLICE_US * 1000)) 0 true 2>/dev/null; then
            SLICE=(chrt --other --sched-runtime $((SLICE_US * 1000)) 0)
        else
            echo "run-guest: this host takes no custom EEVDF slice (chrt --sched-runtime for SCHED_OTHER); the default slice stays" >&2
        fi
    fi
}

# What the run boots and serves is there.
check_inputs() {
    [ -x "$BACKEND_BIN" ] || die "no backend at $BACKEND_BIN (NVGPU_BACKEND)"
    [ -x "$VMM" ] || die "no VMM at $VMM (NVGPU_VMM)"
    [ -r "$KERNEL" ] || die "no guest kernel at $KERNEL (NVGPU_KERNEL)"
    [ -r "$ROOTFS" ] || die "no rootfs at $ROOTFS (NVGPU_ROOTFS)"
    if [ -n "$NVIDIA_SHARE" ]; then
        [ -d "$NVIDIA_SHARE" ] || die "NVGPU_NVIDIA_SHARE=$NVIDIA_SHARE is not a directory"
        # nesbox finds virtiofsd on PATH or in NESBOX_VIRTIOFSD; the rig may
        # carry its own.
        if [ -z "${NESBOX_VIRTIOFSD:-}" ] && [ -n "$RIG" ] && [ -x "$RIG/bin/virtiofsd" ]; then
            export NESBOX_VIRTIOFSD=$RIG/bin/virtiofsd
        fi
    fi
}

# The desktop shares this GPU and this memory. The VMM commits all of guest
# RAM at boot and the window can take up to its size more (under crosvm,
# pages of it the guest touches with nothing placed there are the VMM's
# shared memory; nesbox leaves them PROT_NONE, so this is the worst case);
# refuse a run that would leave the desktop less than the headroom, rather
# than have the OOM killer choose. When it does have to
# choose, it should take this run: the launcher, backend and VMM all inherit
# this oom_score_adj (raising one's own needs no privilege).
host_limits() {
    HEADROOM_MIB=${NVGPU_MEM_HEADROOM_MIB:-4096}
    [[ $HEADROOM_MIB =~ ^[0-9]+$ ]] || die "NVGPU_MEM_HEADROOM_MIB=$HEADROOM_MIB: whole MiB"
    AVAIL_KIB=$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo 2>/dev/null || true)
    if [[ $AVAIL_KIB =~ ^[0-9]+$ ]]; then
        NEED_MIB=$((MEM_MIB + WINDOW_MIB + 10#$HEADROOM_MIB))
        if [ $((AVAIL_KIB / 1024)) -lt "$NEED_MIB" ] && [ "${NVGPU_SKIP_MEM_CHECK:-}" != 1 ]; then
            die "$((AVAIL_KIB / 1024)) MiB available; a ${MEM_MIB} MiB guest, its ${WINDOW_MIB} MiB window and" \
                "${HEADROOM_MIB} MiB left for the desktop want ${NEED_MIB} (NVGPU_MEM_MIB smaller," \
                "or NVGPU_SKIP_MEM_CHECK=1)"
        fi
    fi
    LOG_MAX_MIB=${NVGPU_LOG_MAX_MIB:-64}
    [[ $LOG_MAX_MIB =~ ^[0-9]+$ ]] && [ "$((10#$LOG_MAX_MIB))" -ge 1 ] ||
        die "NVGPU_LOG_MAX_MIB=$LOG_MAX_MIB: whole MiB, at least 1"
    LOG_MAX=$((10#$LOG_MAX_MIB * 1024 * 1024))
    OOM_ADJ=${NVGPU_OOM_SCORE_ADJ-1000}
    if [ -n "$OOM_ADJ" ]; then
        [[ $OOM_ADJ =~ ^-?[0-9]+$ ]] || die "NVGPU_OOM_SCORE_ADJ=$OOM_ADJ: -1000 to 1000"
        { echo "$OOM_ADJ" > /proc/self/oom_score_adj; } 2>/dev/null ||
            echo "run-guest: note: could not set oom_score_adj $OOM_ADJ" >&2
    fi
}

# --kms-card hands the guest the host's card nodes, and the backend opens one
# for it as an ordinary process would: whoever opens a card node while no one
# is DRM master becomes master. A compositor that is only switched away (a
# VT switch, or another session in front) has dropped master, so the VM would
# take the card and the desktop could not get it back until the VM ends.
# Group C in rig/TESTING-RIG.md stops the compositor first; a socket still in
# the runtime directory says it has not been. (Inside the sandbox the host's
# runtime directory is hidden and this sees nothing: stop it anyway.)
kms_card_check() {
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
}
