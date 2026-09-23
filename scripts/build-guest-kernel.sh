#!/usr/bin/env bash
# Build the guest kernel and the guest module against it.
#
# Why this exists: the first guest kernel was built by hand on one machine and
# its config was never written down. When that machine went offline, a one-line
# module fix could not be tested anywhere, because nothing else had a build tree
# the module would load into. The config below is therefore the artefact that
# matters -- the kernel is reproducible from it, and so is the module.
#
# nesbox boots an ELF vmlinux through the PVH entry point, so CONFIG_PVH is not
# optional; without it the VMM rejects the image.
#
# Usage: scripts/build-guest-kernel.sh <linux-source-dir> [jobs]
set -euo pipefail

SRC="${1:?usage: build-guest-kernel.sh <linux-source-dir> [jobs]}"
JOBS="${2:-$(nproc)}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$SRC"
make -j"$JOBS" defconfig

# Everything the guest actually needs, set explicitly rather than inherited
# from defconfig, so a defconfig change upstream cannot silently drop one.
enable() { scripts/config --enable "$1"; }

# Boot. PVH is how nesbox enters an ELF vmlinux.
enable CONFIG_PVH
enable CONFIG_HYPERVISOR_GUEST
enable CONFIG_PARAVIRT
enable CONFIG_KVM_GUEST

# The bus the forwarder sits on, and the devices a guest is given.
enable CONFIG_VIRTIO
enable CONFIG_VIRTIO_PCI
enable CONFIG_VIRTIO_BLK
enable CONFIG_VIRTIO_CONSOLE
enable CONFIG_VIRTIO_MMIO
enable CONFIG_FUSE_FS
enable CONFIG_VIRTIO_FS

# Root filesystem and the pseudo-filesystems the probes mount.
enable CONFIG_EXT4_FS
enable CONFIG_TMPFS
enable CONFIG_PROC_FS
enable CONFIG_SYSFS
enable CONFIG_DEVTMPFS
enable CONFIG_DEVTMPFS_MOUNT

# The module is loaded with insmod, and registers a real DRM device: the core
# has to be built in, and the render-node path with it.
enable CONFIG_MODULES
enable CONFIG_MODULE_UNLOAD
enable CONFIG_PCI
enable CONFIG_DRM

# Not needed to run, needed to debug. The absence of tracefs cost a whole
# diagnosis cycle once: a silent -EINVAL out of the DRM core had to be found by
# reading the kernel source instead of asking the kernel.
enable CONFIG_FTRACE
enable CONFIG_FUNCTION_TRACER
enable CONFIG_FUNCTION_GRAPH_TRACER
enable CONFIG_DYNAMIC_FTRACE
enable CONFIG_KPROBES
enable CONFIG_DEBUG_FS

# So the next person does not have to guess what this was built from.
enable CONFIG_IKCONFIG
enable CONFIG_IKCONFIG_PROC

make olddefconfig
# `modules` as well as `vmlinux`, not `modules_prepare`: modpost resolves the
# module's symbols against the kernel's Module.symvers, and only a real module
# build writes one. With modules_prepare alone every exported symbol comes back
# "undefined" and the module never links.
make -j"$JOBS" vmlinux modules

echo "== kernel built: $SRC/vmlinux"
echo "== building the guest module against it"
# A Module.symvers carried in from another tree would be consulted first.
rm -f "$HERE/driver/Module.symvers"
make -C "$HERE/driver" KDIR="$SRC" clean
make -C "$HERE/driver" KDIR="$SRC"

cp "$SRC/.config" "$HERE/driver/guest-kernel.config"
echo "== module: $HERE/driver/virtio_gpu_nv.ko"
echo "== config recorded at driver/guest-kernel.config"
