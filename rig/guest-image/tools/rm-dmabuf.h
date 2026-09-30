// SPDX-License-Identifier: Apache-2.0
/*
 * rm-dmabuf.h -- the RM calls nvgpu-rm-dmabuf makes: a client with a device,
 * a subdevice and video memory, and EXPORT_TO_DMABUF_FD on it. Layouts from
 * nvos.h, cl0080.h, cl2080.h and nv-ioctl.h (535.129.03 through 610.57.04).
 */
#ifndef RM_DMABUF_H
#define RM_DMABUF_H

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#define RMD_NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, 'F', (nr), (size))
#define RMD_ESC_RM_FREE 0x29
#define RMD_ESC_RM_ALLOC 0x2b
#define RMD_ESC_REGISTER_FD 0xc9
#define RMD_ESC_EXPORT_TO_DMABUF_FD 0xd9

#define RMD_NV01_ROOT 0x0u
#define RMD_NV01_DEVICE_0 0x80u
#define RMD_NV20_SUBDEVICE_0 0x2080u
#define RMD_NV01_MEMORY_LOCAL_USER 0x40u

/* NVOS32_ATTR: PAGE_SIZE (24:23) BIG, LOCATION (26:25) VIDMEM, PHYSICALITY
 * (28:27) CONTIGUOUS; a whole 2 MiB, which RM's dma-buf checks want page
 * aligned. */
#define RMD_ATTR ((2u << 23) | (0u << 25) | (2u << 27))
#define RMD_SIZE (2u << 20)

#define RMD_DEVICE 0xde700d01u
#define RMD_SUBDEVICE 0x5ddd0d01u
#define RMD_MEMORY 0x3e300d01u

struct rmd_nvos64 {
	uint32_t hRoot, hObjectParent, hObjectNew, hClass;
	uint64_t pAllocParms, pRightsRequested;
	uint32_t paramsSize, flags, status, pad;
};
struct rmd_nvos00 {
	uint32_t hRoot, hObjectParent, hObjectOld, status;
};

struct rmd_ctx {
	int ctl, gpu;
	uint32_t client, mem;
	char why[160];
};

struct rmd_export {
	int fd;
	uint32_t status;
};

static int64_t rmd_alloc(int fd, uint32_t client, uint32_t parent, uint32_t h, uint32_t class,
			 void *params, uint32_t size, uint32_t *made)
{
	struct rmd_nvos64 a = {.hRoot = client, .hObjectParent = parent, .hObjectNew = h,
			       .hClass = class, .pAllocParms = (uintptr_t)params,
			       .paramsSize = size};

	if (ioctl(fd, RMD_NV_IOWR(RMD_ESC_RM_ALLOC, sizeof(a)), &a))
		return -errno;
	if (made)
		*made = a.hObjectNew;
	return a.status;
}

static uint32_t rmd_free(int fd, uint32_t client, uint32_t h)
{
	struct rmd_nvos00 f = {.hRoot = client, .hObjectParent = client, .hObjectOld = h};

	if (ioctl(fd, RMD_NV_IOWR(RMD_ESC_RM_FREE, sizeof(f)), &f))
		return (uint32_t)-1;
	return f.status;
}

/* Video memory `h` of RMD_SIZE under the client's device. */
static int64_t rmd_alloc_vidmem(int fd, uint32_t client, uint32_t h)
{
	uint8_t p[128] = {0};
	uint32_t attr = RMD_ATTR, owner = h, size;
	uint64_t bytes = RMD_SIZE;
	int64_t st;

	/* owner: any non-zero id (NV_ERR_INVALID_OWNER otherwise). */
	memcpy(p + 0, &owner, 4);
	memcpy(p + 24, &attr, 4);
	memcpy(p + 64, &bytes, 8);
	/* NV_MEMORY_ALLOCATION_PARAMS: 128 bytes from 550 on (numaNode), 120
	 * before. */
	for (size = 128; size >= 120; size -= 8) {
		st = rmd_alloc(fd, client, RMD_DEVICE, h, RMD_NV01_MEMORY_LOCAL_USER, p, size,
			       NULL);
		if (st == 0)
			return 0;
	}
	return st;
}

/* A client on its own control file with a device, a subdevice and video
 * memory. */
static int rmd_open(struct rmd_ctx *c)
{
	uint8_t dev[56] = {0}, sub[4] = {0};
	int64_t st;

	memset(c, 0, sizeof(*c));
	c->ctl = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
	c->gpu = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
	if (c->ctl < 0 || c->gpu < 0) {
		snprintf(c->why, sizeof(c->why), "open: %s", strerror(errno));
		return -1;
	}
	/* RM lets a client make a device only through a control file a GPU
	 * file was registered with, as NVIDIA's userspace does first. */
	if (ioctl(c->gpu, RMD_NV_IOWR(RMD_ESC_REGISTER_FD, sizeof(int)), &c->ctl)) {
		snprintf(c->why, sizeof(c->why), "REGISTER_FD: %s", strerror(errno));
		return -1;
	}
	st = rmd_alloc(c->ctl, 0, 0, 0, RMD_NV01_ROOT, NULL, 0, &c->client);
	if (st == 0)
		st = rmd_alloc(c->ctl, c->client, c->client, RMD_DEVICE, RMD_NV01_DEVICE_0, dev,
			       sizeof(dev), NULL);
	if (st == 0)
		st = rmd_alloc(c->ctl, c->client, RMD_DEVICE, RMD_SUBDEVICE, RMD_NV20_SUBDEVICE_0,
			       sub, sizeof(sub), NULL);
	if (st == 0)
		st = rmd_alloc_vidmem(c->ctl, c->client, RMD_MEMORY);
	if (st) {
		snprintf(c->why, sizeof(c->why), "RM_ALLOC: %s %#llx", st < 0 ? "errno" : "status",
			 (long long)(st < 0 ? -st : st));
		return -1;
	}
	c->mem = RMD_MEMORY;
	return 0;
}

static void rmd_close(struct rmd_ctx *c)
{
	if (c->client)
		rmd_free(c->ctl, c->client, c->client);
	if (c->gpu >= 0)
		close(c->gpu);
	if (c->ctl >= 0)
		close(c->ctl);
}

/*
 * EXPORT_TO_DMABUF_FD of all of `mem` of `client` on /dev/nvidia0, with `fd`
 * (-1: a new dma-buf; else the append form). The block is 2608 bytes from
 * 570 on (mappingType, bAllowMmap), 2600 before; the host takes one. 0 and
 * the export in *e, or -errno.
 */
static int rmd_export_on(int gpu, uint32_t client, uint32_t mem, int fd, struct rmd_export *e)
{
	uint8_t b[2608];
	uint32_t one = 1, size = RMD_SIZE;
	uint64_t total = RMD_SIZE, zero = 0;
	int wide, r = -EINVAL;

	e->fd = -1;
	e->status = 0;
	for (wide = 1; wide >= 0; wide--) {
		size_t len = wide ? 2608 : 2600;
		size_t handles = wide ? 36 : 32, offsets = wide ? 552 : 544,
		       sizes = wide ? 1576 : 1568, status = wide ? 2600 : 2592;
		uint64_t sz = size;

		memset(b, 0, sizeof(b));
		memcpy(b + 0, &fd, 4);
		memcpy(b + 4, &client, 4);
		memcpy(b + 8, &one, 4);
		memcpy(b + 12, &one, 4);
		memcpy(b + 24, &total, 8);
		memcpy(b + handles, &mem, 4);
		memcpy(b + offsets, &zero, 8);
		memcpy(b + sizes, &sz, 8);
		if (ioctl(gpu, RMD_NV_IOWR(RMD_ESC_EXPORT_TO_DMABUF_FD, len), b)) {
			r = -errno;
			if (r == -EINVAL)
				continue;
			return r;
		}
		memcpy(&e->status, b + status, 4);
		memcpy(&e->fd, b, 4);
		if (e->status)
			e->fd = -1;
		return 0;
	}
	return r;
}

static int rmd_export(struct rmd_ctx *c, uint32_t client, uint32_t mem, int fd,
		      struct rmd_export *e)
{
	return rmd_export_on(c->gpu, client, mem, fd, e);
}

#endif
