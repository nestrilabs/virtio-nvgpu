# Two ways to build this module.
#
# In-tree (how libkrunfw builds it), selected by CONFIG_VIRTIO_GPU_NV:
obj-$(CONFIG_VIRTIO_GPU_NV) += virtio_gpu_nv.o
#
# One module from several objects. None of them may be called virtio_gpu_nv.o,
# which is the module's own name, so the core is nvgpu_main.c.
virtio_gpu_nv-y := nvgpu_main.o nvgpu_schema.o nvgpu_drm.o nvgpu_hostfile.o \
                   nvgpu_kms.o nvgpu_fence.o nvgpu_nvkms.o nvgpu_wl.o \
                   nvgpu_osdesc.o nvgpu_xfer.o
#
# Which implementation parses what a guest process hands the module -- the
# IOCTL2 schema walk, RM escapes' parameter blocks and deep segments, OS
# descriptor registrations:
#
#   NVGPU_RUST=0 (default)  the C: nvgpu_i2.c, nvgpu_rmio.c, nvgpu_atomic.c
#   NVGPU_RUST=1            the Rust: nvgpu_rs.rs, which compiles
#                           driver/rust/core (no unsafe, no kernel deps) into
#                           the module, and nvgpu_rs_glue.c, the C it calls.
#                           Needs a kernel built with CONFIG_RUST=y
#                           (scripts/build-guest-kernel.sh with NVGPU_RUST=1).
#
# Both export the same functions (nvgpu.h), so nothing else changes. Once the
# Rust build has passed the hardware regression, the C goes: delete
# nvgpu_i2.c, nvgpu_rmio.c, nvgpu_atomic.c and this switch, and keep the
# Rust lines.
NVGPU_RUST ?= 0
ifeq ($(NVGPU_RUST),1)
virtio_gpu_nv-y += nvgpu_rs.o nvgpu_rs_glue.o
ccflags-y += -DNVGPU_RUST
else
virtio_gpu_nv-y += nvgpu_i2.o nvgpu_rmio.o nvgpu_atomic.o
endif

# Out-of-tree, for loading into a test guest without rebuilding its kernel:
#
#	make -C driver KDIR=/lib/modules/$(uname -r)/build [NVGPU_RUST=1]
#
# KDIR must be the build tree of the kernel the module will be *loaded* into,
# which for a guest test is the guest's kernel, not the host's.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

.PHONY: module clean
module:
	$(MAKE) -C $(KDIR) M=$(PWD) CONFIG_VIRTIO_GPU_NV=m NVGPU_RUST=$(NVGPU_RUST) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
