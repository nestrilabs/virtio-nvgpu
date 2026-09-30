#!/bin/bash
# SPDX-License-Identifier: Apache-2.0
# Callers the native kernel serves that are not the module's usual ones: DRM
# ioctls with an older or newer struct size than the kernel's, and 32-bit
# processes. Any display mode; no display needed.
#
#   sizes   nvgpu-drm-compat: SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE with the
#           16-byte drm_syncobj_handle (the Steam runtime's libdrm) and a
#           32-byte one, SYNCOBJ_WAIT / TIMELINE_WAIT from before
#           deadline_nsec; each against the native size (the module
#           normalises the argument as drm_ioctl() does)
#   threads nvgpu-syncobj-race: eight threads of one file creating,
#           importing, signalling, subscribing to and destroying syncobjs,
#           then the same with guessers destroying numbers at random; every
#           object destroyed exactly once, no answer for another's object
#   32-bit  the same tool built i686; nvgpu-rm-smoke-32 (an RM client on
#           /dev/nvidiactl: alloc, two controls, free; and a DRM ioctl),
#           whose RESULT line must be the 64-bit build's; vulkaninfo-32
#           --summary and eglinfo-32 -B on NVIDIA's 32-bit userspace
#           (/run/opengl-driver-32), through the module's compat_ioctl
#
# Afterwards, on the HOST: dmesg clean, backend still serving.
. /opt/nvgpu/probe-common.sh
probe_init compat 200

load_module || finish
check_versions
NODE=$(ls /dev/dri/renderD* 2>/dev/null | head -n 1)
say "render node: ${NODE:-none}"

# A tool that exits 77 found nothing to test (no syncobjs: a backend without
# fences), which is a skip, not a failure.
tool() {
    local name=$1
    shift
    say "---- $name: $*"
    timeout -k 5 60 "$@"
    STEP_RC=$?
    case $STEP_RC in
        0) pass "$name" ;;
        77) skip "$name (no syncobjs on this node)" ;;
        124 | 137) fail "$name (timed out)" ;;
        *) fail "$name (exit $STEP_RC)" ;;
    esac
}

section "DRM ioctl sizes (64-bit)"
tool "nvgpu-drm-compat" nvgpu-drm-compat "$NODE"

# Posted SYNCOBJ_DESTROYs (driver/nvgpu_syncobj.c) against creates, imports,
# eventfds and other threads' destroys of the same numbers.
section "syncobjs from many threads of one file"
if command -v nvgpu-syncobj-race >/dev/null; then
    tool "nvgpu-syncobj-race" nvgpu-syncobj-race 5 8 "$NODE"
else
    fail "no nvgpu-syncobj-race in this image (rebuild it: mkimage.sh)"
fi

section "32-bit processes"
if ! command -v nvgpu-rm-smoke-32 >/dev/null; then
    fail "no 32-bit tools in this image (rebuild it: mkimage.sh)"
    finish
fi
if [ -d /run/opengl-driver-32/lib ]; then
    pass "NVIDIA 32-bit userspace at /run/opengl-driver-32 ($(readlink /run/opengl-driver-32))"
else
    fail "no /run/opengl-driver-32"
fi
tool "nvgpu-drm-compat-32" nvgpu-drm-compat-32 "$NODE"

r64=$(timeout -k 5 60 nvgpu-rm-smoke "$NODE" 2>&1)
rc64=$?
printf '%s\n' "$r64" | sed 's/^/    [64] /'
r32=$(timeout -k 5 60 nvgpu-rm-smoke-32 "$NODE" 2>&1)
rc32=$?
printf '%s\n' "$r32" | sed 's/^/    [32] /'
[ "$rc64" = 0 ] && pass "nvgpu-rm-smoke (64-bit)" || fail "nvgpu-rm-smoke (64-bit): exit $rc64"
[ "$rc32" = 0 ] && pass "nvgpu-rm-smoke-32: RM client and DRM ioctl from a 32-bit process" ||
    fail "nvgpu-rm-smoke-32: exit $rc32"
res64=$(printf '%s\n' "$r64" | grep '^RESULT')
res32=$(printf '%s\n' "$r32" | grep '^RESULT')
if [ -n "$res32" ] && [ "$res32" = "$res64" ]; then
    pass "32-bit and 64-bit clients see the same: $res32"
else
    fail "32-bit and 64-bit clients differ: [$res32] vs [$res64]"
fi

out=$(timeout -k 5 60 vulkaninfo-32 --summary 2>&1)
rc=$?
printf '%s\n' "$out" | sed 's/^/    [vk32] /'
if [ "$rc" = 0 ] && printf '%s\n' "$out" | grep -q 'driverID *= *DRIVER_ID_NVIDIA_PROPRIETARY'; then
    pass "vulkaninfo-32 --summary (NVIDIA proprietary driver, 32-bit)"
else
    fail "vulkaninfo-32 --summary (exit $rc, or no NVIDIA proprietary device)"
fi

egl_ok=0
for plat in surfaceless gbm; do
    out=$(timeout -k 5 60 eglinfo-32 -B -p "$plat" 2>&1)
    rc=$?
    printf '%s\n' "$out" | sed "s/^/    [egl32 $plat] /"
    if printf '%s\n' "$out" | grep -q 'EGL vendor string: NVIDIA'; then
        say "eglinfo-32 -p $plat: NVIDIA EGL display up"
        egl_ok=1
    else
        say "eglinfo-32 -p $plat: no NVIDIA EGL display (exit $rc)"
    fi
done
if [ "$egl_ok" = 1 ]; then pass "eglinfo-32: an NVIDIA EGL display initialised (32-bit)"; else fail "eglinfo-32: no platform initialised the NVIDIA EGL vendor"; fi

section "guest dmesg"
dmesg_nvgpu | tail -n 30 | sed 's/^/    /'
if dmesg | grep -Eq 'BUG:|WARNING:|Oops|general protection'; then
    fail "the guest kernel logged a BUG/WARNING/Oops"
else
    pass "no BUG/WARNING/Oops in the guest kernel log"
fi
finish
