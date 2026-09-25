#!/bin/bash
# The image without the device: what can be checked with no virtio-nvgpu
# device behind the guest (QEMU under TCG, or nesbox without --gpu-forward).
# Every check here expects the device to be ABSENT and passes when things
# fail cleanly: the module loads and registers its virtio driver, no node
# appears, the NVIDIA userspace starts and reports "no driver" instead of
# crashing, the loaders find NVIDIA's files, the daemon runs, the module
# unloads, and the kernel log stays clean.
#
# With a real device behind the guest this probe FAILs on purpose (the nodes
# appear); use stage1.sh there.
#   nvgpu_decoy=0   skip binding the module to a virtio-rng device (virtio id
#                   4) to exercise its probe-failure path (default: bind when
#                   one is present; QEMU: -device virtio-rng-pci)
. /opt/nvgpu/probe-common.sh
probe_init nodev 150

# A command's exit status says it failed cleanly when it is a plain nonzero
# status, not a signal (>= 128: a crash, or the step's timeout).
clean_fail() { [ "$1" != 0 ] && [ "$1" -lt 124 ]; }

section "image"
if [ -e /run/opengl-driver/lib/libcuda.so.1 ] && [ -e "$VK_DRIVER_FILES" ] &&
    [ -e "$__EGL_VENDOR_LIBRARY_FILENAMES" ]; then
    pass "/run/opengl-driver: libcuda, the Vulkan ICD and the EGL vendor file are there"
else
    fail "/run/opengl-driver incomplete: $(ls /run/opengl-driver/ 2>&1 | tr '\n' ' ')"
fi
img=$(sed -n 's/^nvidia-userspace \([^ ]*\).*/\1/p' /etc/nvgpu/manifest)
if [ "$img" = 595.99.02 ]; then pass "image userspace is NVIDIA $img"; else fail "image userspace is NVIDIA '$img', not 595.99.02"; fi
if command -v nvidia-smi vulkaninfo nvgpu-wl-guest cuda-smoke >/dev/null; then
    pass "tools on PATH (nvidia-smi vulkaninfo nvgpu-wl-guest cuda-smoke)"
else
    fail "tools missing from PATH=$PATH"
fi

# What the guest kernel believes about caching: with MTRRs off, PAT turns a
# write-back request for the device window into UC- (the driver warns at
# probe). nesbox enables them, default write-back; QEMU's firmware covers RAM
# only. Informational: which answer is right depends on the VMM.
say "MTRR: $(dmesg | grep -E 'MTRRs? (disabled|map|default)|x86/PAT' | sed 's/^\[[^]]*\] //' | tr '\n' ';')"

section "module, no device"
ko=/opt/nvgpu/nvgpu.ko
say "modinfo vermagic: $(modinfo -F vermagic "$ko" 2>&1) ; kernel: $(uname -r)"
if insmod "$ko" "${MODARGS[@]}"; then
    pass "insmod nvgpu.ko"
else
    fail "insmod nvgpu.ko"
    dmesg | tail -n 20 | sed 's/^/    /'
fi
if [ -d /sys/bus/virtio/drivers/virtio-gpu-nv ]; then
    pass "virtio driver virtio-gpu-nv registered"
else
    fail "no /sys/bus/virtio/drivers/virtio-gpu-nv"
fi
bound=0
for d in /sys/bus/virtio/drivers/virtio-gpu-nv/virtio*; do [ -e "$d" ] && bound=$((bound + 1)); done
say "virtio devices: $(for d in /sys/bus/virtio/devices/*; do printf '%s(id %s) ' "${d##*/}" "$(cat "$d/device" 2>/dev/null)"; done)"
sleep 1
if [ "$bound" = 0 ] && [ ! -e /dev/nvidiactl ]; then
    pass "no virtio-nvgpu device: nothing bound, no /dev/nvidiactl (expected here)"
else
    fail "a device is bound ($bound) or /dev/nvidiactl exists: this probe is for a guest without one; use stage1"
fi
taint=$(cat /proc/sys/kernel/tainted)
# 4096 is TAINT_OOT_MODULE, the only taint an out-of-tree module should add.
if [ $((taint & ~4096)) = 0 ]; then
    pass "kernel taint $taint (out-of-tree module only)"
else
    fail "kernel taint $taint has bits besides out-of-tree (4096)"
fi

section "NVIDIA userspace without a driver"
out=$(timeout -k 5 30 nvidia-smi 2>&1)
rc=$?
printf '%s\n' "$out" | sed 's/^/    /'
if clean_fail "$rc" && printf '%s\n' "$out" | grep -qi "couldn't communicate with the NVIDIA driver"; then
    pass "nvidia-smi fails cleanly (exit $rc): no driver"
else
    fail "nvidia-smi: exit $rc, not the expected 'couldn't communicate with the NVIDIA driver'"
fi

out=$(VK_LOADER_DEBUG=driver timeout -k 5 60 vulkaninfo --summary 2>&1)
rc=$?
printf '%s\n' "$out" | grep -Ei 'nvidia|ERROR|devices|GPU' | head -n 30 | sed 's/^/    /'
if printf '%s\n' "$out" | grep -q 'nvidia_icd.json' && [ "$rc" -lt 124 ]; then
    if [ "$rc" = 0 ] && printf '%s\n' "$out" | grep -q 'DRIVER_ID_NVIDIA_PROPRIETARY'; then
        fail "vulkaninfo found an NVIDIA device with no device present?"
    else
        pass "vulkaninfo: the loader found NVIDIA's ICD and fails cleanly without a device (exit $rc)"
    fi
else
    fail "vulkaninfo: exit $rc, or the loader never looked at nvidia_icd.json"
fi

out=$(timeout -k 5 60 cuda-smoke 2>&1)
rc=$?
printf '%s\n' "$out" | head -n 10 | sed 's/^/    /'
if clean_fail "$rc"; then
    pass "cuda-smoke fails cleanly without a device (exit $rc)"
else
    fail "cuda-smoke: exit $rc (a crash, a hang, or a device it cannot have)"
fi

out=$(timeout -k 5 60 eglinfo -B -p surfaceless 2>&1)
rc=$?
printf '%s\n' "$out" | head -n 12 | sed 's/^/    [surfaceless] /'
if [ "$rc" -lt 124 ]; then
    say "eglinfo -p surfaceless: exit $rc (informational)"
else
    say "eglinfo -p surfaceless: exit $rc -- crashed or hung without a device (informational; known for -p gbm)"
fi

section "nvgpu-wl-guest"
out=$(timeout -k 5 10 nvgpu-wl-guest --help 2>&1)
rc=$?
printf '%s\n' "$out" | head -n 5 | sed 's/^/    /'
if [ "$rc" -lt 124 ] && printf '%s\n' "$out" | grep -q 'usage: nvgpu-wl-guest'; then
    pass "nvgpu-wl-guest --help prints its usage (exit $rc)"
else
    fail "nvgpu-wl-guest --help: exit $rc, no usage"
fi
out=$(timeout -k 5 10 nvgpu-wl-guest --socket nodev-test 2>&1)
rc=$?
printf '%s\n' "$out" | head -n 5 | sed 's/^/    /'
if clean_fail "$rc"; then
    pass "nvgpu-wl-guest without /dev/nvgpu-wl exits cleanly (exit $rc)"
else
    fail "nvgpu-wl-guest without /dev/nvgpu-wl: exit $rc"
fi

say "nodes after the userspace ran (it mknods its own as root): $(ls -l /dev/nvidia* 2>/dev/null | awk '{print $NF"("$5$6")"}' | tr '\n' ' ')"

section "module unload"
if rmmod virtio_gpu_nv; then pass "rmmod virtio_gpu_nv"; else fail "rmmod virtio_gpu_nv"; fi

# The probe's failure path: bind the driver to a device that is not ours.
# virtio-rng has one queue; the driver asks for two, so virtio_find_vqs fails
# and probe() has to unwind without leaking or oopsing.
decoy=
for d in /sys/bus/virtio/devices/*; do
    [ "$(cat "$d/device" 2>/dev/null)" = 0x0004 ] && [ ! -e "$d/driver" ] && decoy=$d && break
done
if [ "$(arg decoy 1)" = 1 ] && [ -n "$decoy" ]; then
    section "probe failure path (virtio-rng decoy ${decoy##*/})"
    if insmod "$ko" virtio_id=4; then
        pass "insmod nvgpu.ko virtio_id=4"
        sleep 1
        dmesg | grep -E 'virtio-gpu-nv|virtio_gpu_nv|probe of virtio' | tail -n 5 | sed 's/^/    /'
        # By sysfs, not /dev: run as root, nvidia-smi and libcuda mknod
        # /dev/nvidiactl themselves when it is missing, so a node there now
        # may be theirs rather than the driver's.
        say "decoy: driver=$(readlink "$decoy/driver" 2>/dev/null || echo none); class nvidia: $(ls /sys/class/nvidia 2>/dev/null | tr '\n' ' ')"
        if [ ! -e "$decoy/driver" ] && [ ! -e /sys/class/nvidia ]; then
            pass "probe on a one-queue device failed and unwound (not bound, no nodes)"
        else
            fail "the driver bound a virtio-rng device, or made nodes for it"
        fi
        if rmmod virtio_gpu_nv; then pass "rmmod after the failed probe"; else fail "rmmod after the failed probe"; fi
    else
        fail "insmod nvgpu.ko virtio_id=4"
    fi
else
    skip "probe failure path (no unbound virtio-rng device, or nvgpu_decoy=0)"
fi
finish
