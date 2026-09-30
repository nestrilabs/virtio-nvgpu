#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# remove() under load: the driver is unbound from its device while threads
# are inside ioctls -- syncobj waits asleep, eventfds subscribed, requests on
# the ring and being notified -- and every file is closed after the device
# has gone. What remove() tears down (the virtqueues, the event buffers, the
# DRM devices) must not be reached by any of them afterwards: the kernel log
# stays clean, which a KASAN kernel makes a strict check. Then the module
# unloads.
#
# The window each race needs is narrow (an enqueuer between its unlock and
# its notify, a callback still running past the reset); this is the load
# that makes them possible, not a reproducer that forces them.
#   nvgpu_threads=N   nvgpu-syncobj-race threads (default 8)
#   nvgpu_before=S    seconds of load before the unbind (default 3)
. /opt/nvgpu/probe-common.sh
probe_init unbind 150

load_module || finish
NODE=$(ls /dev/dri/renderD* 2>/dev/null | head -n 1)
say "render node: ${NODE:-none}"
DRV=/sys/bus/virtio/drivers/virtio-gpu-nv
VDEV=
for d in "$DRV"/virtio*; do [ -e "$d" ] && VDEV=${d##*/}; done
[ -n "$VDEV" ] || { fail "no device bound to virtio-gpu-nv"; finish; }
say "device: $VDEV"

RACE=$(command -v nvgpu-syncobj-race || echo /opt/nvgpu/nvgpu-syncobj-race)
[ -x "$RACE" ] || { fail "no nvgpu-syncobj-race in this image"; finish; }

section "load"
# Long phases: the unbind comes in the middle of the first.
"$RACE" 30 "$(arg threads 8)" "$NODE" >/var/log/nvgpu/unbind-race.log 2>&1 &
RACE_PID=$!
BG_PIDS+=("$RACE_PID")
sleep "$(arg before 3)"
if kill -0 "$RACE_PID" 2>/dev/null; then
    pass "nvgpu-syncobj-race running before the unbind"
else
    fail "nvgpu-syncobj-race ended before the unbind"
fi

section "unbind under load"
echo "$VDEV" > "$DRV/unbind" &
UNBIND_PID=$!
for _ in $(seq 1 300); do
    kill -0 "$UNBIND_PID" 2>/dev/null || break
    sleep 0.1
done
if kill -0 "$UNBIND_PID" 2>/dev/null; then
    fail "remove() still running after 30 s"
    echo w > /proc/sysrq-trigger 2>/dev/null
    finish
fi
wait "$UNBIND_PID"
pass "unbind returned"
[ -e /dev/nvidiactl ] && fail "/dev/nvidiactl still there" || pass "nodes gone"

section "files closed after remove()"
# The race's threads now find the device gone, and go on failing until
# their phase ends; a couple of seconds of that, then they are killed. The
# kill must end them at once: a thread still there is stuck in the kernel.
sleep 2
kill -TERM "$RACE_PID" 2>/dev/null
for _ in $(seq 1 100); do
    kill -0 "$RACE_PID" 2>/dev/null || break
    sleep 0.1
done
if kill -0 "$RACE_PID" 2>/dev/null; then
    fail "nvgpu-syncobj-race still there 10 s after SIGTERM"
    echo w > /proc/sysrq-trigger 2>/dev/null
    kill -KILL "$RACE_PID" 2>/dev/null
else
    wait "$RACE_PID"
    pass "nvgpu-syncobj-race ended on SIGTERM (status $?), its files closed"
fi
tail -n 5 /var/log/nvgpu/unbind-race.log | sed 's/^/    /'
sleep 1
if rmmod virtio_gpu_nv; then pass "rmmod virtio_gpu_nv"; else fail "rmmod virtio_gpu_nv"; fi
say "guest dmesg (driver lines):"
dmesg_nvgpu | tail -n 20 | sed 's/^/    /'
finish
