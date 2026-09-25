#!/usr/bin/env bash
# Build the guest kernel and the guest module against it.
#
# Why this exists: the first guest kernel was built by hand on one machine and
# its config was never written down. When that machine went offline, a one-line
# module fix could not be tested anywhere, because nothing else had a build tree
# the module would load into. The config below is therefore the artefact that
# matters -- the kernel is reproducible from it, and so is the module.
#
# nesbox loads an ELF vmlinux and enters it at its 64-bit entry point with a
# boot_params page (the Linux 64-bit boot protocol). QEMU's -kernel boots the
# same ELF only through the PVH entry point, so CONFIG_PVH is kept as well: it
# is what lets one image serve both.
#
# Usage: scripts/build-guest-kernel.sh <linux-source-dir> [jobs] [build-dir]
#   build-dir  where the kernel's objects go (make O=), so the source tree
#              stays clean and can serve more than one build. Without it the
#              kernel is built inside the source tree, as before.
#
# MODULE_DIR=<dir> builds the module from a copy of driver/ kept in <dir>
# instead of in driver/ itself, so the working tree gets no build products.
#
# NVGPU_RUST=1 builds a kernel with CONFIG_RUST and the module with its
# parsers in Rust (driver/rust/, NVGPU_RUST=1 in driver/Makefile) instead of
# C. It needs rustc, bindgen and RUST_LIB_SRC in the environment: run it in
# scripts/guest-toolchain-rust (the rig does: .rig/build-kernel-rust.sh).
set -euo pipefail

usage="usage: build-guest-kernel.sh <linux-source-dir> [jobs] [build-dir]"
SRC="$(cd "${1:?$usage}" && pwd)"
JOBS="${2:-$(nproc)}"
OUT="${3:-}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

KMAKE=(make -C "$SRC")
if [ -n "$OUT" ]; then
    mkdir -p "$OUT"
    OUT="$(cd "$OUT" && pwd)"
    KMAKE+=(O="$OUT")
fi
KTREE="${OUT:-$SRC}"

"${KMAKE[@]}" -j"$JOBS" defconfig

# Everything the guest actually needs, set explicitly rather than inherited
# from defconfig, so a defconfig change upstream cannot silently drop one.
enable() { "$SRC/scripts/config" --file "$KTREE/.config" --enable "$1"; }
disable() { "$SRC/scripts/config" --file "$KTREE/.config" --disable "$1"; }

# Boot. PVH is how QEMU enters an ELF vmlinux.
enable CONFIG_PVH
enable CONFIG_HYPERVISOR_GUEST
enable CONFIG_PARAVIRT
enable CONFIG_KVM_GUEST
# nesbox describes its PCI root, ECAM window and power-off register in
# hardware-reduced ACPI tables; without ACPI there is no PCI and no poweroff.
enable CONFIG_ACPI
enable CONFIG_PCI_MMCONFIG
enable CONFIG_PCI_MSI
# COM1, for when the guest dies before virtio-console is up (earlyprintk,
# console=ttyS0 under QEMU too).
enable CONFIG_SERIAL_8250
enable CONFIG_SERIAL_8250_CONSOLE
# The probes end with poweroff -f, or with sysrq o when that is not there.
enable CONFIG_MAGIC_SYSRQ

# The bus the forwarder sits on, and the devices a guest is given.
enable CONFIG_VIRTIO
enable CONFIG_VIRTIO_PCI
enable CONFIG_VIRTIO_BLK
enable CONFIG_VIRTIO_CONSOLE
enable CONFIG_VIRTIO_MMIO
enable CONFIG_FUSE_FS
enable CONFIG_VIRTIO_FS
enable CONFIG_VIRTIO_NET
enable CONFIG_VSOCKETS
enable CONFIG_VIRTIO_VSOCKETS

# Root filesystem and the pseudo-filesystems the probes mount.
enable CONFIG_EXT4_FS
enable CONFIG_TMPFS
enable CONFIG_PROC_FS
enable CONFIG_SYSFS
enable CONFIG_DEVTMPFS
enable CONFIG_DEVTMPFS_MOUNT

# What Wayland clients, the proxy daemon and a guest compositor lean on:
# wl_shm pools are memfds, descriptors cross unix sockets as SCM_RIGHTS, and
# nvgpu-wl-guest is one epoll loop woken by an eventfd (the module signals
# eventfds too, for DRM_IOCTL_SYNCOBJ_EVENTFD).
enable CONFIG_SHMEM
enable CONFIG_UNIX
enable CONFIG_EPOLL
enable CONFIG_EVENTFD
enable CONFIG_SIGNALFD
enable CONFIG_TIMERFD
enable CONFIG_INOTIFY_USER
enable CONFIG_FHANDLE
# A compositor in the guest (compositor-VM mode) reads input from evdev.
# nesbox has no virtio-input device, so under it the only input is what
# uinput synthesises; virtio-input is for QEMU runs.
enable CONFIG_INPUT_EVDEV
enable CONFIG_INPUT_UINPUT
enable CONFIG_VIRTIO_INPUT

# The module is loaded with insmod, and registers a real DRM device: the core
# has to be built in, and the render-node path with it.
enable CONFIG_MODULES
enable CONFIG_MODULE_UNLOAD
enable CONFIG_PCI
enable CONFIG_DRM
# DRM selects these today, but the module calls them directly -- host fences
# are sync_files behind dma_fence proxies, GEM proxies export dma-bufs -- and
# modpost resolves them only if they are built in, so they are named here.
enable CONFIG_DMA_SHARED_BUFFER
enable CONFIG_SYNC_FILE
# Mappings of the host window are WC or UC per placement; without PAT
# ioremap_wc and pgprot_writecombine quietly fall back to UC.
enable CONFIG_X86_PAT

# Not needed to run, needed to debug. The absence of tracefs cost a whole
# diagnosis cycle once: a silent -EINVAL out of the DRM core had to be found by
# reading the kernel source instead of asking the kernel.
# Namespaces, for the sandboxes guest applications bring: Chromium's and
# Firefox's own (user and PID namespaces with seccomp), and Flatpak/bubblewrap
# around whole apps. A guest meant for sandboxing apps needs them; without
# them browsers run with their sandbox off.
enable CONFIG_NAMESPACES
enable CONFIG_USER_NS
enable CONFIG_PID_NS
enable CONFIG_NET_NS
enable CONFIG_UTS_NS
enable CONFIG_IPC_NS
enable CONFIG_SECCOMP
enable CONFIG_SECCOMP_FILTER

enable CONFIG_FTRACE
enable CONFIG_FUNCTION_TRACER
enable CONFIG_FUNCTION_GRAPH_TRACER
enable CONFIG_DYNAMIC_FTRACE
enable CONFIG_KPROBES
enable CONFIG_DEBUG_FS
enable CONFIG_KALLSYMS
enable CONFIG_KALLSYMS_ALL
enable CONFIG_DYNAMIC_DEBUG
# DWARF, so an oops in the module decodes to a line (scripts/faddr2line on
# the .ko, decode_stacktrace.sh on vmlinux). No BTF: it needs a pahole in
# step with the compiler, and nothing in the guest runs BPF.
enable CONFIG_DEBUG_KERNEL
enable CONFIG_DEBUG_INFO_DWARF_TOOLCHAIN_DEFAULT
disable CONFIG_DEBUG_INFO_NONE
disable CONFIG_DEBUG_INFO_REDUCED
disable CONFIG_DEBUG_INFO_BTF

# So the next person does not have to guess what this was built from.
enable CONFIG_IKCONFIG
enable CONFIG_IKCONFIG_PROC

# The module's untrusted-input parsers in Rust (driver/rust/). Rust needs no
# MODVERSIONS (unset above by defconfig) and no BTF (disabled above).
if [ "${NVGPU_RUST:-0}" = 1 ]; then
    enable CONFIG_RUST
fi

"${KMAKE[@]}" olddefconfig
if [ "${NVGPU_RUST:-0}" = 1 ] && ! grep -q '^CONFIG_RUST=y' "$KTREE/.config"; then
    echo "CONFIG_RUST did not stick: no usable Rust toolchain?" >&2
    "${KMAKE[@]}" rustavailable >&2 || true
    exit 1
fi
# `modules` as well as `vmlinux`, not `modules_prepare`: modpost resolves the
# module's symbols against the kernel's Module.symvers, and only a real module
# build writes one. With modules_prepare alone every exported symbol comes back
# "undefined" and the module never links.
"${KMAKE[@]}" -j"$JOBS" vmlinux modules

echo "== kernel built: $KTREE/vmlinux"
echo "== building the guest module against it"
MOD="${MODULE_DIR:-$HERE/driver}"
if [ "$MOD" != "$HERE/driver" ]; then
    mkdir -p "$MOD"
    # Sources only: the copy mirrors driver/, and is cleaned below like
    # driver/ itself would be.
    rsync -a --delete --exclude='*.o' --exclude='*.ko' --exclude='*.mod' \
        --exclude='*.mod.c' --exclude='.*.cmd' --exclude='modules.order' \
        --exclude='Module.symvers' --exclude='.tmp*' --exclude='guest-kernel.config' \
        "$HERE/driver/" "$MOD/"
fi
# A Module.symvers carried in from another tree would be consulted first.
rm -f "$MOD/Module.symvers"
make -C "$MOD" KDIR="$KTREE" clean
make -C "$MOD" KDIR="$KTREE" NVGPU_RUST="${NVGPU_RUST:-0}"

# The recorded config is the C build's; a Rust build's stays in its tree.
if [ "${NVGPU_RUST:-0}" != 1 ]; then
    cp "$KTREE/.config" "$HERE/driver/guest-kernel.config"
fi
echo "== module: $MOD/virtio_gpu_nv.ko"
[ "${NVGPU_RUST:-0}" = 1 ] || echo "== config recorded at driver/guest-kernel.config"
