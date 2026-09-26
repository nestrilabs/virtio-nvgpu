# SPDX-License-Identifier: Apache-2.0
# shellcheck shell=bash
# Shared by every probe. Source it; do not run it.
#
# A probe runs as PID 1 (the kernel command line says init=/opt/nvgpu/<probe>.sh),
# so there is no init system: this file mounts what a program expects, loads
# the guest module, and -- whatever happens -- powers the VM off at the end.
# If PID 1 exits the kernel panics, so a probe never just exits: `finish`
# does, and a watchdog does it for a probe that hangs.
#
# Test arguments come from the kernel command line (run-guest.sh passes them
# through as boot args), as nvgpu_<key>=<value>; read them with `arg`:
#   nvgpu_timeout=SECS   the whole probe's budget (default per probe)
#   nvgpu_secs=SECS      how long a timed step runs (a compositor, a client)
#   nvgpu_frames=N       frames for the frame-counted steps
#   nvgpu_hold=1         drop to a shell on the console instead of powering off
#   nvgpu_loglevel=N     console printk level while the probe runs (default 4)
#   nvgpu_dryrun=1       do not load the module (exercise a probe's own logic
#                        where there is no device; everything after fails)
# NVGPU_CMDLINE in the environment stands in for /proc/cmdline (dry runs).
# and virtio_gpu_nv.<param>=<value> tokens become insmod arguments for the
# module (insmod does not read the command line itself).
#
# Output: each check prints one line "[<probe>] PASS|FAIL|SKIP <what>", and
# the last line of a run is
#   NVGPU_PROBE_DONE probe=<name> result=PASS|FAIL pass=N fail=N skip=N
# The console log is also kept in the guest at /var/log/nvgpu/<probe>.log.

. /opt/nvgpu/env.sh

PROBE=${PROBE:-$(basename "$0" .sh)}
NPASS=0
NFAIL=0
NSKIP=0
declare -A NVGPU_ARGS=()
MODARGS=()
BG_PIDS=()
FINISHING=0

say() { printf '[%s] %s\n' "$PROBE" "$*"; }
pass() { NPASS=$((NPASS + 1)); say "PASS $*"; }
fail() { NFAIL=$((NFAIL + 1)); say "FAIL $*"; }
skip() { NSKIP=$((NSKIP + 1)); say "SKIP $*"; }
section() { printf '\n[%s] ==== %s ====\n' "$PROBE" "$*"; }

# arg KEY [DEFAULT]: the value of nvgpu_KEY= on the command line.
arg() { printf '%s' "${NVGPU_ARGS[$1]:-${2:-}}"; }

# step NAME SECS CMD...: run CMD bounded by SECS; PASS on exit 0, else FAIL.
# The exit status is returned, and kept in $STEP_RC.
step() {
    local name=$1 secs=$2
    shift 2
    say "---- $name: $*"
    timeout -k 5 "$secs" "$@"
    STEP_RC=$?
    if [ "$STEP_RC" = 0 ]; then
        pass "$name"
    elif [ "$STEP_RC" = 124 ] || [ "$STEP_RC" = 137 ]; then
        fail "$name (timed out after ${secs}s)"
    else
        fail "$name (exit $STEP_RC)"
    fi
    return "$STEP_RC"
}

# run_for NAME SECS CMD...: for programs that run until stopped (a client, a
# compositor). PASS when it is still running after SECS and is then stopped;
# FAIL if it exits (or crashes) before that.
run_for() {
    local name=$1 secs=$2
    shift 2
    say "---- $name (for ${secs}s): $*"
    timeout -s TERM -k 5 "$secs" "$@"
    STEP_RC=$?
    if [ "$STEP_RC" = 124 ]; then
        pass "$name (ran ${secs}s)"
        STEP_RC=0
    else
        fail "$name (exited early, status $STEP_RC)"
    fi
    return "$STEP_RC"
}

# bg CMD...: start a helper in the background; finish() stops it.
bg() {
    "$@" &
    BG_PIDS+=($!)
}

# The driver's lines; not the command line or init's path, which say nvgpu too.
dmesg_nvgpu() {
    dmesg 2>/dev/null | grep -E 'virtio-gpu-nv|virtio_gpu_nv|nvgpu' |
        grep -Ev 'ommand line:|as init process|^\[[^]]*\] +/opt/nvgpu/' || true
}

# Mount what programs expect, and give the tmpfs-backed names their links.
setup_fs() {
    mountpoint -q /proc || mount -t proc proc /proc
    mountpoint -q /sys || mount -t sysfs sysfs /sys
    mountpoint -q /dev || mount -t devtmpfs devtmpfs /dev
    mkdir -p /dev/pts /dev/shm
    mountpoint -q /dev/pts || mount -t devpts -o gid=5,mode=620,ptmxmode=666 devpts /dev/pts
    mountpoint -q /dev/shm || mount -t tmpfs -o mode=1777 shm /dev/shm
    mountpoint -q /run || mount -t tmpfs -o mode=0755 run /run
    mountpoint -q /tmp || mount -t tmpfs -o mode=1777 tmp /tmp
    mountpoint -q /sys/kernel/debug 2>/dev/null || mount -t debugfs debugfs /sys/kernel/debug 2>/dev/null || true
    [ -e /dev/fd ] || ln -s /proc/self/fd /dev/fd
    [ -e /dev/stdin ] || ln -s /proc/self/fd/0 /dev/stdin
    [ -e /dev/stdout ] || ln -s /proc/self/fd/1 /dev/stdout
    [ -e /dev/stderr ] || ln -s /proc/self/fd/2 /dev/stderr

    # /run is a fresh tmpfs: the names the loaders and PATH use live there.
    ln -sfn "$(readlink /etc/nvgpu/opengl-driver)" /run/opengl-driver
    mkdir -p /run/current-system
    ln -sfn "$(readlink /etc/nvgpu/sw)" /run/current-system/sw
    mkdir -p "$(dirname "$XDG_RUNTIME_DIR")"
    mkdir -m 0700 "$XDG_RUNTIME_DIR" 2>/dev/null || chmod 0700 "$XDG_RUNTIME_DIR"
    mkdir -p /var/log/nvgpu
    echo nvgpu-guest > /proc/sys/kernel/hostname 2>/dev/null || true
}

parse_cmdline() {
    local tok k
    # xargs honours the kernel's double quotes (nvgpu_x="a b").
    while IFS= read -r tok; do
        case $tok in
            nvgpu_*=*)
                k=${tok%%=*}
                NVGPU_ARGS[${k#nvgpu_}]=${tok#*=}
                ;;
            virtio_gpu_nv.*=*)
                MODARGS+=("${tok#virtio_gpu_nv.}")
                ;;
        esac
    done < <(printf '%s\n' "${NVGPU_CMDLINE:-$(cat /proc/cmdline 2>/dev/null)}" | xargs -n1 printf '%s\n' 2>/dev/null)
}

# Load the guest module and wait for its nodes. Returns nonzero on failure.
load_module() {
    local ko=/opt/nvgpu/nvgpu.ko
    section "module"
    if [ "$(arg dryrun 0)" = 1 ]; then
        skip "module load (nvgpu_dryrun=1)"
        return 0
    elif grep -q '^virtio_gpu_nv ' /proc/modules 2>/dev/null; then
        say "virtio_gpu_nv already loaded"
    elif [ ! -f "$ko" ]; then
        fail "module present ($ko missing; mkimage.sh --module-only)"
        return 1
    else
        say "modinfo: $(modinfo -F vermagic "$ko" 2>/dev/null) ; kernel: $(uname -r)"
        if insmod "$ko" "${MODARGS[@]}"; then
            pass "insmod nvgpu.ko ${MODARGS[*]}"
        else
            fail "insmod nvgpu.ko ${MODARGS[*]}"
            dmesg | tail -n 20
            return 1
        fi
    fi
    for _ in $(seq 1 100); do
        [ -e /dev/nvidiactl ] && break
        sleep 0.1
    done
    if [ -e /dev/nvidiactl ]; then
        pass "/dev/nvidiactl appeared"
    else
        fail "/dev/nvidiactl never appeared (10 s)"
    fi
    say "nodes: $(ls /dev/nvidia* /dev/nvgpu* /dev/dri/* /dev/udmabuf 2>/dev/null | tr '\n' ' ')"
    say "guest dmesg (driver lines):"
    dmesg_nvgpu | sed 's/^/    /'
    [ -e /dev/nvidiactl ]
}

# The driver version the userspace in this image was built for, and the one
# the module reports, if it reports one. They must be the host's (RM checks).
check_versions() {
    local img v
    img=$(sed -n 's/^nvidia-userspace \([^ ]*\).*/\1/p' /etc/nvgpu/manifest)
    say "image userspace: NVIDIA $img"
    if [ -r /proc/driver/nvidia/version ]; then
        v=$(head -n 1 /proc/driver/nvidia/version)
        say "/proc/driver/nvidia/version: $v"
        case $v in
            *"$img"*) pass "module-reported driver version matches the userspace ($img)" ;;
            *) fail "driver version mismatch: userspace $img, $v" ;;
        esac
    else
        say "no /proc/driver/nvidia/version in the guest (the version check is RM's, at the first client)"
    fi
}

# The backend's capability bits, from the driver's HELLO line (0 without one).
# A guest always has /dev/dri/card0 -- the DRM core gives the render device a
# primary node too -- so a card node alone does not mean compositor-VM mode;
# NVGPU_BCAP_KMS_CARD (bit 0, --kms-card) does.
backend_caps() {
    local c
    c=$(dmesg 2>/dev/null | sed -n 's/.*virtio-gpu-nv: protocol v2, backend caps \(0x[0-9a-fA-F]*\).*/\1/p' | tail -n 1)
    printf '%d' "${c:-0}"
}
kms_card_offered() { [ $(($(backend_caps) & 1)) -ne 0 ]; }

# Start the guest Wayland daemon (normal mode) and wait for its socket.
# Exports WAYLAND_DISPLAY. Returns nonzero if it never listens.
start_wl_daemon() {
    local sock=${1:-wayland-0}
    shift || true
    if [ ! -e /dev/nvgpu-wl ]; then
        fail "nvgpu-wl-guest: /dev/nvgpu-wl missing (backend not in a Wayland mode?)"
        return 1
    fi
    say "starting nvgpu-wl-guest --socket $sock $*"
    bg nvgpu-wl-guest --socket "$sock" "$@" 2> >(tee /var/log/nvgpu/nvgpu-wl-guest.log | sed 's/^/    [wl-guest] /' >&2)
    WL_PID=${BG_PIDS[-1]}
    for _ in $(seq 1 100); do
        [ -S "$XDG_RUNTIME_DIR/$sock" ] && break
        kill -0 "$WL_PID" 2>/dev/null || break
        sleep 0.1
    done
    if [ -S "$XDG_RUNTIME_DIR/$sock" ]; then
        pass "nvgpu-wl-guest serving $XDG_RUNTIME_DIR/$sock"
        export WAYLAND_DISPLAY=$sock
        return 0
    fi
    fail "nvgpu-wl-guest never listened on $sock"
    return 1
}

wl_daemon_alive() {
    if [ -n "${WL_PID:-}" ] && kill -0 "$WL_PID" 2>/dev/null; then
        pass "nvgpu-wl-guest still running"
    else
        fail "nvgpu-wl-guest died during the probe"
    fi
}

# Power off. Never returns.
poweroff_now() {
    sync
    /opt/nvgpu/bin/nvgpu-poweroff 2>/dev/null
    echo o > /proc/sysrq-trigger 2>/dev/null
    sleep 2
    /opt/nvgpu/bin/nvgpu-poweroff reboot 2>/dev/null
    echo b > /proc/sysrq-trigger 2>/dev/null
    while :; do sleep 1; done
}

finish() {
    local result p
    [ "$FINISHING" = 1 ] && return
    FINISHING=1
    trap - EXIT
    for p in "${BG_PIDS[@]}"; do kill -TERM "$p" 2>/dev/null; done
    sleep 0.5
    for p in "${BG_PIDS[@]}"; do kill -KILL "$p" 2>/dev/null; done
    [ -n "${WATCHDOG_PID:-}" ] && kill "$WATCHDOG_PID" 2>/dev/null

    section "guest kernel log check"
    if dmesg 2>/dev/null | grep -Eiq 'Oops|BUG:|general protection|Call Trace|WARNING:'; then
        fail "guest kernel reported an oops/WARN:"
        dmesg | grep -Ei -B2 -A12 'Oops|BUG:|general protection|Call Trace|WARNING:' | head -n 80 | sed 's/^/    /'
    else
        say "no oops/WARN in guest dmesg"
    fi
    dmesg > "/var/log/nvgpu/$PROBE.dmesg" 2>/dev/null

    if [ "$NFAIL" = 0 ] && [ "$NPASS" -gt 0 ]; then result=PASS; else result=FAIL; fi
    printf '\nNVGPU_PROBE_DONE probe=%s result=%s pass=%d fail=%d skip=%d\n' \
        "$PROBE" "$result" "$NPASS" "$NFAIL" "$NSKIP"

    if [ "$(arg hold 0)" = 1 ]; then
        say "nvgpu_hold=1: shell on the console; exit it to power off"
        setsid -c bash -l <>/dev/hvc0 >&0 2>&1 || bash -l
    fi
    # Let the log tee drain before the disk goes away.
    if [ -c /dev/console ]; then exec >/dev/console 2>&1; else exec >/dev/null 2>&1; fi
    sleep 0.3
    poweroff_now
}

# probe_init NAME DEFAULT_BUDGET_SECS: the first thing every probe does.
probe_init() {
    PROBE=$1
    local budget
    unset WAYLAND_DISPLAY DISPLAY WAYLAND_SOCKET
    setup_fs
    parse_cmdline
    dmesg -n "$(arg loglevel 4)" 2>/dev/null || true
    exec > >(tee -a "/var/log/nvgpu/$PROBE.log") 2>&1
    trap finish EXIT
    trap 'say "signal received"; finish' INT TERM

    budget=$(arg timeout "$2")
    if [ "$budget" != 0 ]; then
        # The watchdog outlives a hung step: it powers off itself.
        (
            sleep "$budget"
            printf '\n[%s] FAIL probe budget of %ss exhausted -- powering off\n' "$PROBE" "$budget" >/dev/console
            printf 'NVGPU_PROBE_DONE probe=%s result=FAIL reason=timeout\n' "$PROBE" >/dev/console
            poweroff_now
        ) &
        WATCHDOG_PID=$!
    fi
    say "virtio-nvgpu guest probe '$PROBE' starting; kernel $(uname -r), budget ${budget}s"
    say "cmdline: ${NVGPU_CMDLINE:-$(cat /proc/cmdline)}"
    [ ${#NVGPU_ARGS[@]} -gt 0 ] && say "args: $(for k in "${!NVGPU_ARGS[@]}"; do printf '%s=%s ' "$k" "${NVGPU_ARGS[$k]}"; done)"
    say "image: $(tr '\n' ';' < /etc/nvgpu/manifest)"
}
