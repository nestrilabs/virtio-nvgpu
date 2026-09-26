#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# An interactive shell on the console, with the environment the probes use
# and the module loaded. Exit the shell to power off.
#   nvgpu_wl=1        also start nvgpu-wl-guest (Wayland modes)
#   nvgpu_timeout=S   power off after S seconds regardless (default 0: never;
#                     the launcher's own timeout still applies)
. /opt/nvgpu/probe-common.sh
probe_init shell 0

load_module || say "module did not load; the shell is yours anyway"
[ "$(arg wl 0)" = 1 ] && start_wl_daemon wayland-0
section "shell"
say "PATH=$PATH"
say "probes: /opt/nvgpu/*.sh  verify: $NVGPU_VERIFY  tools: /opt/nvgpu/bin  logs: /var/log/nvgpu"
say "exit the shell to power the VM off"
setsid -c bash -l <>/dev/hvc0 >&0 2>&1 || bash -l </dev/console >/dev/console 2>&1
finish
