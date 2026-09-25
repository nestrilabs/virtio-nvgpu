#!/bin/bash
# TESTING.md stage 7, the guest side: a compositor on the guest card (compositor-VM
# mode) and nvgpu-wl-guest --export carrying host clients to it. Launch with
# --kms-card --wayland-export /path/sock, then on the host, while this runs:
#   WAYLAND_DISPLAY=/path/sock foot
# PASS needs a host client's window to appear on the guest compositor.
#   nvgpu_card=PATH   the guest card node (default: the first /dev/dri/card*)
#   nvgpu_secs=S      how long to wait for host clients (default 120)
#   nvgpu_comp=sway|hyprland   the guest compositor (default sway)
. /opt/nvgpu/probe-common.sh
probe_init export 170
SECS=$(arg secs 120)

load_module || finish
CARD=$(arg card "$(ls /dev/dri/card* 2>/dev/null | head -n 1)")
if [ -z "$CARD" ] || ! kms_card_offered; then
    fail "no KMS card node (export mode rides on compositor-VM mode: --kms-card; backend caps $(printf '0x%x' "$(backend_caps)"))"
    finish
fi
if [ ! -e /dev/nvgpu-wl ]; then
    fail "/dev/nvgpu-wl missing (backend not started with --wayland-export)"
    finish
fi

section "guest compositor"
. /opt/nvgpu/compositor-common.sh
start_comp "$CARD" || finish

section "export daemon"
say "starting nvgpu-wl-guest --export $COMP_WL --card $CARD"
bg nvgpu-wl-guest --export "$COMP_WL" --card "$CARD" 2> >(tee /var/log/nvgpu/nvgpu-wl-guest.log | sed 's/^/    [wl-guest] /' >&2)
WL_PID=${BG_PIDS[-1]}
sleep 2
if kill -0 "$WL_PID" 2>/dev/null; then pass "nvgpu-wl-guest --export running"; else fail "nvgpu-wl-guest --export exited"; finish; fi

base=$(comp_windows)
say "EXPORT READY: on the host, run a client against the --wayland-export socket now (waiting ${SECS}s)"
seen=0
end=$((SECONDS + SECS))
while [ "$SECONDS" -lt "$end" ]; do
    n=$(comp_windows)
    if [ "$n" -gt "$base" ] && [ "$seen" = 0 ]; then
        seen=1
        pass "a host client's window appeared on the guest compositor"
        comp_windows_detail | sed 's/^/    /'
    fi
    kill -0 "$WL_PID" 2>/dev/null || { fail "nvgpu-wl-guest --export died"; break; }
    sleep 2
done
[ "$seen" = 1 ] || fail "no host client window appeared in ${SECS}s"
wl_daemon_alive
stop_comp
finish
