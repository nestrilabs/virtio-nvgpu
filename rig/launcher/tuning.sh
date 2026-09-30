# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
#
# rig/run-guest.sh's tuning knobs (the launcher's header, "Tuning"; DEPLOY.md
# has each one's gain and cost, SECURITY.md, "The tuning knobs", what each
# gives up): core scheduling, the backend's poll and channel-disable rates,
# the window preset, the VMM's file-size limit, a host C-state cap and the
# guest's command-line knobs. Each is off, or today's behaviour, unless its
# variable is set. Sourced by the launcher; the launcher calls each function
# at the point its comment names.

# NVGPU_WINDOW_PRESET, before backend_settings reads NVGPU_WINDOW_MIB: a
# preset is a window size and nothing else, and an explicit size wins.
window_preset() {
    case ${NVGPU_WINDOW_PRESET:-} in
        '' | default) ;;
        creative) : "${NVGPU_WINDOW_MIB:=8192}" ;;
        *) die "NVGPU_WINDOW_PRESET=$NVGPU_WINDOW_PRESET: default or creative" ;;
    esac
}

# The knobs' values, checked. After run_settings and guest_settings: needs
# VMM_KIND, WINDOW_MIB, MEM_MIB, TIMEOUT and CMDLINE_EXTRA.
tuning_settings() {
    # Core scheduling (SECURITY.md, "The tuning knobs"). crosvm's default is a
    # cookie per vCPU; nesbox sets none. NVGPU_CROSVM_CORE_SCHED=0|1 is the
    # older spelling of off and per-vcpu, for crosvm. Another piece of the
    # launcher that needs a mode (a placement whose sibling vCPUs must share
    # a core) sets CORE_SCHED_DEFAULT before this runs; a mode asked for
    # explicitly still wins, and that piece checks CORE_SCHED afterwards.
    local legacy=${NVGPU_CROSVM_CORE_SCHED:-}
    if [ -n "${NVGPU_CORE_SCHED:-}" ]; then
        CORE_SCHED=$NVGPU_CORE_SCHED
    elif [ "$VMM_KIND" = crosvm ] && [ -n "$legacy" ]; then
        case $legacy in
            1) CORE_SCHED=per-vcpu ;;
            0) CORE_SCHED=off ;;
            *) die "NVGPU_CROSVM_CORE_SCHED=$legacy: 0 or 1 (or NVGPU_CORE_SCHED)" ;;
        esac
    elif [ -n "${CORE_SCHED_DEFAULT:-}" ]; then
        CORE_SCHED=$CORE_SCHED_DEFAULT
    elif [ "$VMM_KIND" = crosvm ]; then
        CORE_SCHED=per-vcpu
    else
        CORE_SCHED=off
    fi
    case $CORE_SCHED in
        per-vcpu | shared | vm | off) ;;
        *) die "NVGPU_CORE_SCHED=$CORE_SCHED: per-vcpu, shared, vm or off" ;;
    esac
    [ "$VMM_KIND" = crosvm ] || [ "$CORE_SCHED" != per-vcpu ] ||
        die "NVGPU_CORE_SCHED=per-vcpu: nesbox makes no core-scheduling cookie per vCPU;" \
            "vm (one for the whole VMM) or shared (one with the backend) it can have"
    # What crosvm is told; the cookie shared with the backend, or nesbox's
    # one cookie, is made by tuning_vmm_cmd.
    CROSVM_CORE_ARGS=()
    if [ "$VMM_KIND" = crosvm ]; then
        case $CORE_SCHED in
            vm) CROSVM_CORE_ARGS=(--per-vm-core-scheduling) ;;
            shared | off) CROSVM_CORE_ARGS=(--core-scheduling=false) ;;
        esac
    fi
    CORESCHED=
    if [ "$CORE_SCHED" = shared ] || { [ "$VMM_KIND" = nesbox ] && [ "$CORE_SCHED" = vm ]; }; then
        CORESCHED=$(command -v coresched) ||
            die "NVGPU_CORE_SCHED=$CORE_SCHED needs coresched (util-linux 2.40 or later) on PATH"
        [ $PRIV = user ] || CORESCHED=$(root_owned "$CORESCHED" "coresched")
    fi

    # The backend's poll and its channel-disable budgets: its own flags,
    # which it checks again; given only when set.
    if [ -n "${NVGPU_QUEUE_POLL_US:-}" ]; then
        [[ $NVGPU_QUEUE_POLL_US =~ ^[0-9]+$ ]] && [ "$((10#$NVGPU_QUEUE_POLL_US))" -le 1000 ] ||
            die "NVGPU_QUEUE_POLL_US=$NVGPU_QUEUE_POLL_US: 0 to 1000"
        BACKEND_ARGS+=(--queue-poll-us "$((10#$NVGPU_QUEUE_POLL_US))")
    fi
    if [ -n "${NVGPU_FIFO_DISABLE_RATES:-}" ]; then
        [[ $NVGPU_FIFO_DISABLE_RATES =~ ^([0-9]+),([0-9]+),([0-9]+),([0-9]+)$ ]] ||
            die "NVGPU_FIFO_DISABLE_RATES=$NVGPU_FIFO_DISABLE_RATES: PROC_RATE,PROC_BURST,VM_RATE,VM_BURST" \
                "(the backend's defaults are 50,40,200,160)"
        BACKEND_ARGS+=(--fifo-disable-proc-rate "${BASH_REMATCH[1]}"
            --fifo-disable-proc-burst "${BASH_REMATCH[2]}"
            --fifo-disable-vm-rate "${BASH_REMATCH[3]}"
            --fifo-disable-vm-burst "${BASH_REMATCH[4]}")
        # For sec-negative's rate test, which expects the burst it was given.
        case " $CMDLINE_EXTRA " in
            *" nvgpu_fifo_disable_rates="*) ;;
            *) CMDLINE_EXTRA="${CMDLINE_EXTRA:+$CMDLINE_EXTRA }nvgpu_fifo_disable_rates=$NVGPU_FIFO_DISABLE_RATES" ;;
        esac
    fi

    # The VMM's RLIMIT_FSIZE, in MiB: bounds how far a VMM its guest has
    # taken over can grow a file it can write. Every memfd the VMM sizes is
    # held to it too, so it must cover guest RAM and the window; the disk
    # is checked once there is one (tuning_vmm_cmd).
    VMM_FSIZE_MIB=${NVGPU_VMM_FSIZE_MIB:-}
    if [ -n "$VMM_FSIZE_MIB" ]; then
        [[ $VMM_FSIZE_MIB =~ ^[0-9]+$ ]] && [ "$((10#$VMM_FSIZE_MIB))" -ge 1 ] ||
            die "NVGPU_VMM_FSIZE_MIB=$VMM_FSIZE_MIB: whole MiB"
        VMM_FSIZE_MIB=$((10#$VMM_FSIZE_MIB))
        local m
        for m in "$MEM_MIB:guest RAM (NVGPU_MEM_MIB)" "$WINDOW_MIB:the window (NVGPU_WINDOW_MIB)"; do
            [ "$VMM_FSIZE_MIB" -ge "${m%%:*}" ] ||
                die "NVGPU_VMM_FSIZE_MIB=$VMM_FSIZE_MIB is below ${m#*:}, ${m%%:*} MiB: the VMM" \
                    "could not size it"
        done
        command -v prlimit >/dev/null || die "NVGPU_VMM_FSIZE_MIB needs prlimit (util-linux)"
    fi

    # A host C-state cap while the VM runs: /dev/cpu_dma_latency held at
    # NVGPU_CPU_LATENCY_US (cpu_latency_start).
    CPU_LATENCY_US=${NVGPU_CPU_LATENCY_US:-}
    if [ -n "$CPU_LATENCY_US" ]; then
        [[ $CPU_LATENCY_US =~ ^[0-9]+$ ]] && [ "$((10#$CPU_LATENCY_US))" -le 100000 ] ||
            die "NVGPU_CPU_LATENCY_US=$CPU_LATENCY_US: 0 to 100000 microseconds"
        CPU_LATENCY_US=$((10#$CPU_LATENCY_US))
        [ -w /dev/cpu_dma_latency ] ||
            die "NVGPU_CPU_LATENCY_US: /dev/cpu_dma_latency is root's; run as root, or leave it unset"
    fi

    # The guest's knobs, as words of its kernel command line (DEPLOY.md,
    # "The guest").
    local words=()
    case ${NVGPU_GUEST_HALTPOLL:-} in
        '' | 0) ;;
        1)
            words+=(cpuidle_haltpoll.force=1)
            echo "run-guest: note: NVGPU_GUEST_HALTPOLL=1: guest haltpoll, which cost a D3D11 game" \
                "under Wine 9% (DEPLOY.md, \"The guest\")" >&2
            ;;
        *) die "NVGPU_GUEST_HALTPOLL=$NVGPU_GUEST_HALTPOLL: 0 or 1" ;;
    esac
    if [ -n "${NVGPU_GUEST_RT_SPIN_US:-}" ]; then
        [[ $NVGPU_GUEST_RT_SPIN_US =~ ^[0-9]+$ ]] && [ "$((10#$NVGPU_GUEST_RT_SPIN_US))" -le 1000 ] ||
            die "NVGPU_GUEST_RT_SPIN_US=$NVGPU_GUEST_RT_SPIN_US: 0 to 1000"
        words+=("virtio_gpu_nv.rt_spin_us=$((10#$NVGPU_GUEST_RT_SPIN_US))")
    fi
    case ${NVGPU_GUEST_ASYNC_FENCE_WATCH:-} in
        '') ;;
        1) words+=(virtio_gpu_nv.async_fence_watch=Y) ;;
        0) words+=(virtio_gpu_nv.async_fence_watch=N) ;;
        *) die "NVGPU_GUEST_ASYNC_FENCE_WATCH=$NVGPU_GUEST_ASYNC_FENCE_WATCH: 0 or 1" ;;
    esac
    case ${NVGPU_GUEST_THP:-} in
        '') ;;
        always | madvise | never) words+=("transparent_hugepage=$NVGPU_GUEST_THP") ;;
        *) die "NVGPU_GUEST_THP=$NVGPU_GUEST_THP: always, madvise or never" ;;
    esac
    if [ ${#words[@]} -gt 0 ]; then
        CMDLINE_EXTRA="${CMDLINE_EXTRA:+$CMDLINE_EXTRA }${words[*]}"
    fi
}

# The cookie the backend and the VMM share (NVGPU_CORE_SCHED=shared): made
# by a process of the launcher's own that holds it -- a sleep, started with
# a new cookie -- and copied from it into the backend and the VMM as each is
# started, before either runs a line of its own: both are exec'd with it,
# and every thread and process either makes inherits it. Neither can be
# given it later (the backend is undumpable, so the launcher, unprivileged,
# may not change its cookie once it runs). From tuning_vmm_cmd, before
# either starts.
core_sched_anchor() {
    CS_ANCHOR=
    [ "$CORE_SCHED" = shared ] || return 0
    "$CORESCHED" new -- sleep $((TIMEOUT + 120)) {SLOT_FD}>&- {TAG_FD}>&- </dev/null >/dev/null 2>&1 &
    CS_ANCHOR=$!
    local c=
    for _ in $(seq 1 50); do
        c=$("$CORESCHED" get -s "$CS_ANCHOR" 2>/dev/null) || true
        c=${c##* }
        [ -n "$c" ] && [ "$c" != 0x0 ] && break
        sleep 0.05
    done
    [ -n "$c" ] && [ "$c" != 0x0 ] ||
        die "NVGPU_CORE_SCHED=shared: no core-scheduling cookie could be made (a kernel without" \
            "CONFIG_SCHED_CORE?)"
    echo "run-guest: core scheduling: the backend and the VMM share one cookie ($c)" >&2
    BACKEND_PREFIX=("$CORESCHED" copy -s "$CS_ANCHOR" --)
}

# The VMM's command line, with what the knobs put in front of it: its
# file-size limit and its cookie. After the VMM_CMD is complete (the slice
# included), and once the disk is known.
tuning_vmm_cmd() {
    if [ -n "$VMM_FSIZE_MIB" ]; then
        local disk_mib
        disk_mib=$((($(stat -L -c %s -- "$DISK") + 1048575) / 1048576))
        [ "$VMM_FSIZE_MIB" -ge "$disk_mib" ] ||
            die "NVGPU_VMM_FSIZE_MIB=$VMM_FSIZE_MIB is below the disk, $disk_mib MiB: the guest" \
                "could not write its own disk to its end"
        [ "$VMM_FSIZE_MIB" -ge "$LOG_MAX_MIB" ] ||
            die "NVGPU_VMM_FSIZE_MIB=$VMM_FSIZE_MIB is below NVGPU_LOG_MAX_MIB=$LOG_MAX_MIB: the VMM" \
                "writes the console log"
        VMM_CMD=(prlimit --fsize=$((VMM_FSIZE_MIB * 1048576)) -- "${VMM_CMD[@]}")
    fi
    core_sched_anchor
    case $CORE_SCHED/$VMM_KIND in
        shared/*) VMM_CMD=("$CORESCHED" copy -s "$CS_ANCHOR" -- "${VMM_CMD[@]}") ;;
        vm/nesbox) VMM_CMD=("$CORESCHED" new -- "${VMM_CMD[@]}") ;;
    esac
}

# The C-state cap: /dev/cpu_dma_latency, written and held open for the run
# by a process of its own, so that no other child of the launcher -- the
# backend, the VMM -- inherits a descriptor that could change it. The
# kernel drops the request when the holder exits (tuning_cleanup). Before
# the VMM starts.
cpu_latency_start() {
    CPU_LATENCY_PID=
    [ -n "$CPU_LATENCY_US" ] || return 0
    local fd
    exec {fd}>/dev/cpu_dma_latency || die "NVGPU_CPU_LATENCY_US: cannot open /dev/cpu_dma_latency"
    # Hex text, not four bytes: four bytes would be taken as a binary s32.
    printf '0x%08x' "$CPU_LATENCY_US" >&"$fd" ||
        die "NVGPU_CPU_LATENCY_US=$CPU_LATENCY_US: the kernel refused it"
    sleep $((TIMEOUT + 120)) {SLOT_FD}>&- {TAG_FD}>&- </dev/null >/dev/null 2>&1 &
    CPU_LATENCY_PID=$!
    exec {fd}>&-
    echo "run-guest: host C-states held to ${CPU_LATENCY_US} us exit latency while the VM runs" >&2
}

# The knobs' processes, stopped however the run ends (the launcher's
# cleanup).
tuning_cleanup() {
    [ -z "${CPU_LATENCY_PID:-}" ] || kill "$CPU_LATENCY_PID" 2>/dev/null || true
    [ -z "${CS_ANCHOR:-}" ] || kill "$CS_ANCHOR" 2>/dev/null || true
}
