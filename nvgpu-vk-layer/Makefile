# SPDX-License-Identifier: Apache-2.0
# VK_LAYER_NVGPU_no_uvm: the layer's library and its implicit-layer manifest
# (DEPLOY.md, "The guest"). `make PREFIX=/usr install` puts the library in
# $(LIBDIR) and the manifest in $(DATADIR)/vulkan/implicit_layer.d, naming
# the library by its absolute path.
PREFIX ?= /usr/local
LIBDIR ?= $(PREFIX)/lib
DATADIR ?= $(PREFIX)/share
CFLAGS ?= -O2 -g
CFLAGS += -Wall -Wextra -fPIC -fvisibility=hidden
LIB = libnvgpu_vk_layer.so
# The manifest's library_arch: 64, or 32 for an i686 build beside it.
LIBARCH ?= 64
MANIFEST ?= VK_LAYER_NVGPU_no_uvm.json

all: $(LIB) $(MANIFEST)

$(LIB): nvgpu_vk_layer.c
	$(CC) $(CFLAGS) -shared -o $@ $< -lpthread $(LDFLAGS)

$(MANIFEST): VK_LAYER_NVGPU_no_uvm.json.in
	sed -e 's|@LIB@|$(LIBDIR)/$(LIB)|' -e 's|@ARCH@|$(LIBARCH)|' $< > $@

install: all
	install -Dm755 $(LIB) $(DESTDIR)$(LIBDIR)/$(LIB)
	install -Dm644 $(MANIFEST) $(DESTDIR)$(DATADIR)/vulkan/implicit_layer.d/$(MANIFEST)

clean:
	rm -f $(LIB) $(MANIFEST)

.PHONY: all install clean
