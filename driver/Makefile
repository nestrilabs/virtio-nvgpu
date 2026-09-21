# Two ways to build this module.
#
# In-tree (how libkrunfw builds it), selected by CONFIG_VIRTIO_GPU_NV:
obj-$(CONFIG_VIRTIO_GPU_NV) += virtio_gpu_nv.o

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
