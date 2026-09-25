/*
 * lease-flip.c -- light up a leased connector and measure flip pacing.
 *
 * Point it at a guest DRM file that is master of at least one connected
 * connector -- an adopted DRM lease fd (mode 3/4), or a guest card node in
 * compositor-VM mode (--kms-card, mode 5) -- and it does a full modeset onto the
 * first connected output and then page-flips between two dumb buffers a fixed
 * number of times, reporting the interval between flips. That is the smallest
 * end-to-end proof that the guest owns the output: a modeset the host honours,
 * and flips that complete at the monitor's refresh rate rather than stalling or
 * returning -EBUSY forever.
 *
 * It is deliberately libdrm-legacy (SetCrtc + PageFlip with a flip event), not
 * atomic: SetCrtc is itself a full modeset, both paths reach the same NVKMS
 * commit, and the legacy path has far fewer moving parts to get wrong. For an
 * atomic modeset and an explicit KMS-lease flip, TESTING.md §"Lease and
 * VK_KHR_display modes" gives the equivalent `modetest` invocations.
 *
 * The fd can be a device path (--device /dev/dri/cardN) or a number already open
 * in this process (--fd N), e.g. a lease fd a wp_drm_lease client handed down.
 *
 * Build: see lease-flip.sh (gcc + libdrm).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <drm_fourcc.h>
#include <xf86drm.h>
#include <xf86drmMode.h>

static uint64_t now_ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return (uint64_t)t.tv_sec * 1000000000ull + t.tv_nsec;
}

/* One dumb-buffer framebuffer, painted a flat colour so a flip is visible. */
struct fb {
	uint32_t handle;
	uint32_t fb_id;
	uint32_t pitch;
	uint64_t size;
	uint8_t *map;
};

static int make_fb(int fd, uint32_t w, uint32_t h, uint32_t colour, struct fb *out)
{
	struct drm_mode_create_dumb cd = {.width = w, .height = h, .bpp = 32};
	if (drmIoctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) {
		fprintf(stderr, "CREATE_DUMB: %s\n", strerror(errno));
		return -1;
	}
	out->handle = cd.handle;
	out->pitch = cd.pitch;
	out->size = cd.size;

	uint32_t handles[4] = {cd.handle, 0, 0, 0};
	uint32_t pitches[4] = {cd.pitch, 0, 0, 0};
	uint32_t offsets[4] = {0, 0, 0, 0};
	if (drmModeAddFB2(fd, w, h, DRM_FORMAT_XRGB8888, handles, pitches, offsets,
			  &out->fb_id, 0) != 0) {
		fprintf(stderr, "ADDFB2: %s\n", strerror(errno));
		return -1;
	}

	struct drm_mode_map_dumb md = {.handle = cd.handle};
	if (drmIoctl(fd, DRM_IOCTL_MODE_MAP_DUMB, &md) != 0) {
		fprintf(stderr, "MAP_DUMB: %s\n", strerror(errno));
		return -1;
	}
	out->map = mmap(NULL, out->size, PROT_READ | PROT_WRITE, MAP_SHARED, fd,
			(off_t)md.offset);
	if (out->map == MAP_FAILED) {
		fprintf(stderr, "mmap fb: %s\n", strerror(errno));
		return -1;
	}
	for (uint64_t i = 0; i + 4 <= out->size; i += 4)
		*(uint32_t *)(out->map + i) = colour;
	return 0;
}

static void flip_handler(int fd, unsigned int seq, unsigned int tv_sec,
			 unsigned int tv_usec, void *data)
{
	(void)fd;
	(void)seq;
	(void)tv_sec;
	(void)tv_usec;
	*(int *)data = 1; /* flip completed */
}

int main(int argc, char **argv)
{
	const char *device = NULL;
	int fd = -1;
	int frames = 120;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--device") && i + 1 < argc)
			device = argv[++i];
		else if (!strcmp(argv[i], "--fd") && i + 1 < argc)
			fd = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--frames") && i + 1 < argc)
			frames = atoi(argv[++i]);
		else {
			fprintf(stderr,
				"usage: %s (--device /dev/dri/cardN | --fd N) [--frames N]\n",
				argv[0]);
			return 2;
		}
	}
	if (fd < 0 && device)
		fd = open(device, O_RDWR | O_CLOEXEC);
	if (fd < 0) {
		fprintf(stderr, "no DRM fd: pass --device or --fd\n");
		return 2;
	}

	drmModeRes *res = drmModeGetResources(fd);
	if (!res) {
		fprintf(stderr, "drmModeGetResources: %s (is this a KMS/lease fd?)\n",
			strerror(errno));
		return 1;
	}

	/* First connected connector with a mode, and a CRTC that can drive it. */
	drmModeConnector *conn = NULL;
	for (int i = 0; i < res->count_connectors; i++) {
		drmModeConnector *c = drmModeGetConnector(fd, res->connectors[i]);
		if (c && c->connection == DRM_MODE_CONNECTED && c->count_modes > 0) {
			conn = c;
			break;
		}
		if (c)
			drmModeFreeConnector(c);
	}
	if (!conn) {
		fprintf(stderr, "no connected connector with a mode on this fd\n");
		return 1;
	}
	drmModeModeInfo mode = conn->modes[0];

	drmModeEncoder *enc = conn->encoder_id
				      ? drmModeGetEncoder(fd, conn->encoder_id)
				      : NULL;
	uint32_t crtc_id = 0;
	if (enc && enc->crtc_id)
		crtc_id = enc->crtc_id;
	else if (res->count_crtcs > 0)
		crtc_id = res->crtcs[0];
	if (!crtc_id) {
		fprintf(stderr, "no usable CRTC\n");
		return 1;
	}

	printf("connector %u, mode %ux%u@%uHz, crtc %u\n", conn->connector_id,
	       mode.hdisplay, mode.vdisplay, mode.vrefresh, crtc_id);

	struct fb a = {0}, b = {0};
	if (make_fb(fd, mode.hdisplay, mode.vdisplay, 0x00202020, &a) ||
	    make_fb(fd, mode.hdisplay, mode.vdisplay, 0x00404040, &b))
		return 1;

	/* The modeset. A commit that never returns, or ADDFB/SETCRTC failing
	 * where bare metal succeeds, is the failure this catches. */
	uint64_t t0 = now_ns();
	if (drmModeSetCrtc(fd, crtc_id, a.fb_id, 0, 0, &conn->connector_id, 1,
			   &mode) != 0) {
		fprintf(stderr, "SETCRTC (modeset): %s\n", strerror(errno));
		return 1;
	}
	printf("modeset ok in %.1f ms\n", (now_ns() - t0) / 1e6);

	drmEventContext ev = {
		.version = DRM_EVENT_CONTEXT_VERSION,
		.page_flip_handler = flip_handler,
	};

	uint64_t last = now_ns();
	double sum = 0, min = 1e30, max = 0;
	int measured = 0;
	struct fb *front = &a, *back = &b;
	for (int i = 0; i < frames; i++) {
		int done = 0;
		if (drmModePageFlip(fd, crtc_id, back->fb_id,
				    DRM_MODE_PAGE_FLIP_EVENT, &done) != 0) {
			/* -EBUSY is legitimate only while a flip is pending; a
			 * persistent one is the failure. */
			fprintf(stderr, "PAGE_FLIP %d: %s\n", i, strerror(errno));
			return 1;
		}
		/* Wait for the flip event, with a generous cap so a stalled
		 * output fails rather than hangs. */
		uint64_t deadline = now_ns() + 3ull * 1000000000ull;
		while (!done) {
			struct timespec ts = {.tv_sec = 0, .tv_nsec = 2000000};
			nanosleep(&ts, NULL);
			drmHandleEvent(fd, &ev);
			if (now_ns() > deadline) {
				fprintf(stderr, "flip %d never completed (3 s)\n", i);
				return 1;
			}
		}
		uint64_t t = now_ns();
		double dt = (t - last) / 1e6;
		last = t;
		struct fb *tmp = front;
		front = back;
		back = tmp;
		if (i > 4) { /* discard the first few while the pipe warms */
			sum += dt;
			if (dt < min)
				min = dt;
			if (dt > max)
				max = dt;
			measured++;
		}
	}

	if (measured)
		printf("flips: %d measured, mean %.3f ms, min %.3f, max %.3f "
		       "(refresh is %.3f ms)\n",
		       measured, sum / measured, min, max,
		       mode.vrefresh ? 1000.0 / mode.vrefresh : 0.0);
	printf("PASS: modeset and %d flips completed\n", frames);

	drmModeFreeConnector(conn);
	if (enc)
		drmModeFreeEncoder(enc);
	drmModeFreeResources(res);
	return 0;
}
