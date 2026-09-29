# SPDX-License-Identifier: GPL-2.0-only
#
# Two ways to build this module: in-tree (how libkrunfw builds it), selected
# by CONFIG_VIRTIO_GPU_NV, and out-of-tree through the front end at the end.

ifneq ($(KERNELRELEASE),)

obj-$(CONFIG_VIRTIO_GPU_NV) += virtio_gpu_nv.o

# One module from several objects. None of them may be called virtio_gpu_nv.o,
# which is the module's own name, so the core is nvgpu_main.c.
virtio_gpu_nv-y := nvgpu_main.o nvgpu_procfs.o nvgpu_pci.o nvgpu_schema.o \
                   nvgpu_drm.o nvgpu_gem.o nvgpu_hostfile.o nvgpu_kms.o \
                   nvgpu_fence.o \
                   nvgpu_syncobj.o nvgpu_semsurf.o \
                   nvgpu_nvkms.o nvgpu_wl.o nvgpu_capture.o nvgpu_misc.o \
                   nvgpu_osdesc.o nvgpu_xfer.o nvgpu_tbuf.o nvgpu_clock.o \
                   nvgpu_events.o nvgpu_v1.o

# Which implementation parses what a guest process hands the module -- the
# IOCTL2 schema walk, RM escapes' parameter blocks and deep segments, OS
# descriptor registrations, ATOMIC commits:
#
#   the Rust  nvgpu_rs.rs, which compiles driver/rust/core (no unsafe, no
#             kernel dependency) into the module, and nvgpu_rs_glue.c, the C
#             it calls. Needs a kernel built with CONFIG_RUST=y. The default
#             wherever the kernel has Rust: it has passed the hardware
#             regression, and it is the stronger boundary (SECURITY.md §6).
#   the C     nvgpu_i2.c, nvgpu_rmio.c, nvgpu_atomic.c: the fallback for a
#             kernel without Rust, and the difftest's oracle. Fixes only.
#
# Chosen, unless NVGPU_RUST says (on the command line or in the environment):
#   in a kernel tree   by Kconfig, CONFIG_VIRTIO_GPU_NV_RUST (default y
#                      when CONFIG_RUST=y);
#   out of tree        by the target kernel: the Rust if it has CONFIG_RUST=y
#                      and the rustc at hand is the one it was built with
#                      (CONFIG_RUSTC_VERSION_TEXT -- a Rust object links only
#                      against its own compiler's kernel crates), else the C,
#                      with a warning saying why when the kernel has Rust.
# NVGPU_RUST=1 insists on the Rust (an error without CONFIG_RUST, or without
# the kernel's rustc), and NVGPU_RUST=0 on the C, which the module then says
# at load on a kernel that has Rust. Both export the same functions
# (nvgpu.h), so nothing else changes, and `modinfo -F parsers` says which a
# module has.
# (The kernel's own RUSTC_VERSION_TEXT is exported only to config targets.)
NVGPU_RUSTC_TEXT := $(if $(filter y,$(CONFIG_RUST)),$(subst $(pound),,$(shell $(RUSTC) --version 2>/dev/null)))
NVGPU_RUSTC_OK := $(and $(NVGPU_RUSTC_TEXT),$(if $(subst $(CONFIG_RUSTC_VERSION_TEXT),,$(NVGPU_RUSTC_TEXT)),,1))
ifeq ($(origin NVGPU_RUST),undefined)
ifneq ($(KBUILD_EXTMOD),)
ifneq ($(NVGPU_RUSTC_OK),)
NVGPU_RUST := 1
else
NVGPU_RUST := 0
ifeq ($(CONFIG_RUST),y)
$(warning this kernel has Rust, but the rustc at hand ('$(NVGPU_RUSTC_TEXT)') is not the one it was built with ('$(CONFIG_RUSTC_VERSION_TEXT)'): building the C parsers. Build in the kernel's Rust toolchain (scripts/guest-toolchain-rust) for the Rust ones, the default)
endif
endif
else
NVGPU_RUST := $(if $(filter y,$(CONFIG_VIRTIO_GPU_NV_RUST)),1,0)
endif
endif

ifeq ($(NVGPU_RUST),1)
ifneq ($(CONFIG_RUST),y)
$(error NVGPU_RUST=1 needs a kernel built with CONFIG_RUST=y)
endif
# A different rustc fails in the kernel's own Makefiles, on crates it cannot
# read (E0514); said here instead.
ifeq ($(NVGPU_RUSTC_OK),)
$(error NVGPU_RUST=1, and the rustc at hand ('$(NVGPU_RUSTC_TEXT)') is not the one this kernel was built with ('$(CONFIG_RUSTC_VERSION_TEXT)'): build in the kernel's Rust toolchain (scripts/guest-toolchain-rust))
endif
virtio_gpu_nv-y += nvgpu_rs.o nvgpu_rs_glue.o
ccflags-y += -DNVGPU_RUST

# No path in the Rust parsers reaches a Rust panic -- an overflow or bounds
# check, an unwrap -- which in the kernel is a BUG(): the object names no
# panic symbol, or the module is not linked.
quiet_cmd_nopanic = NOPANIC $<
      cmd_nopanic = if $(NM) -u $< | grep -i panic; then \
                      echo "$<: a panic path (above)" >&2; exit 1; fi; touch $@
$(obj)/nvgpu_rs.nopanic: $(obj)/nvgpu_rs.o
	$(call cmd,nopanic)
$(obj)/virtio_gpu_nv.o: $(obj)/nvgpu_rs.nopanic
targets += nvgpu_rs.nopanic
clean-files += nvgpu_rs.nopanic
else
virtio_gpu_nv-y += nvgpu_i2.o nvgpu_rmio.o nvgpu_atomic.o
endif

else

# Out-of-tree, for loading into a test guest without rebuilding its kernel:
#
#	make -C driver KDIR=/lib/modules/$(uname -r)/build [NVGPU_RUST=0|1]
#
# (the Rust parsers when that kernel has CONFIG_RUST=y, unless NVGPU_RUST=0;
# see above).
#
# KDIR must be the build tree of the kernel the module will be *loaded* into,
# which for a guest test is the guest's kernel, not the host's.
KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

.PHONY: module clean
module:
	$(MAKE) -C $(KDIR) M=$(PWD) CONFIG_VIRTIO_GPU_NV=m \
		$(if $(NVGPU_RUST),NVGPU_RUST=$(NVGPU_RUST)) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

endif
