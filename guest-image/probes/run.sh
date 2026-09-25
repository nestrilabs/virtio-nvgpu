#!/bin/bash
# Run one command line, given base64-encoded on the kernel command line, with
# the environment the probes use and the module loaded; then power off. For
# chasing a failure without a probe of its own:
#   NVGPU_CMDLINE_EXTRA="nvgpu_cmd=$(printf %s 'strace -f cuda-smoke' | base64 -w0)"
#   nvgpu_timeout=S   budget in seconds (default 150)
#   nvgpu_wl=1        start nvgpu-wl-guest first (WAYLAND_DISPLAY=wayland-0)
. /opt/nvgpu/probe-common.sh
probe_init run "$(arg timeout 150)"

load_module || { fail "module did not load"; finish; }
if [ "$(arg wl 0)" = 1 ]; then
    start_wl_daemon wayland-0 || { fail "nvgpu-wl-guest did not start"; finish; }
    export WAYLAND_DISPLAY=wayland-0
fi
cmd=$(printf '%s' "$(arg cmd)" | base64 -d 2>/dev/null) || cmd=
[ -n "$cmd" ] || { fail "no nvgpu_cmd (base64) on the command line"; finish; }
section "run"
say "\$ $cmd"
if bash -c "$cmd"; then pass "command exited 0"; else fail "command exited $?"; fi
finish
