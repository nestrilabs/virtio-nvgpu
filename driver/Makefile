# Two ways to build this module.
#
# In-tree (how libkrunfw builds it), selected by CONFIG_VIRTIO_GPU_NV:
obj-$(CONFIG_VIRTIO_GPU_NV) += virtio_gpu_nv.o
#
# One module from several objects. None of them may be called virtio_gpu_nv.o,
# which is the module's own name, so the core is nvgpu_main.c.
virtio_gpu_nv-y := nvgpu_main.o nvgpu_xfer.o nvgpu_i2.o nvgpu_drm.o nvgpu_hostfile.o \
                   nvgpu_kms.o nvgpu_fence.o nvgpu_nvkms.o nvgpu_wl.o

# Out-of-tree, for loading into a test guest without rebuilding its kernel:
#
#	make -C driver KDIR=/lib/modules/$(uname -r)/build
#
# KDIR must be the build tree of the kernel the module will be *loaded* into,
# which for a guest test is the guest's kernel, not the host's.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

.PHONY: module clean
module:
	$(MAKE) -C $(KDIR) M=$(PWD) CONFIG_VIRTIO_GPU_NV=m modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
