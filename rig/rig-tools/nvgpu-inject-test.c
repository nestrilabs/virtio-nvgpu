// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-inject-test -- the capture helper's half of the rig's capture test,
 * run on the host against a backend's --inject-socket.
 *
 * It allocates buffers the way xdg-desktop-portal-hyprland does for a
 * screencast -- gbm_bo_create_with_modifiers() on the render node, with the
 * modifiers EGL can render to -- paints a pattern that changes every frame
 * (capture-pattern.h) with GLES into each, glFinish()es, and hands them to
 * the backend: HELLO, then IMPORT with the dma-buf. What it prints is what
 * the guest needs to open and check them, as kernel command-line words for
 * the capture probe (rig/guest-image/probes/capture.sh):
 *
 *   nvgpu_cap=ID:TOKEN:FRAME:FNV,...     buffers, each with the frame it
 *                                        holds and the pattern's checksum
 *   nvgpu_cap_live=ID:TOKEN              a buffer repainted every frame while
 *                                        this runs (--hold)
 *   nvgpu_cap_released=ID:TOKEN          an id imported and then RELEASEd
 *   nvgpu_cap_sync=ID:TOKEN:SID:STOKEN:N a buffer and a syncobj (IMPORT_SYNCOBJ)
 *                                        for N frames of explicit sync
 *
 * The ids and tokens it prints are for the rig, which has no vsock and puts
 * them on the guest's kernel command line: every guest process can read
 * that, and every host user the VMM's. A real helper sends each token over
 * its own channel to the guest's daemon alone, and never writes one to a
 * command line, a log or a world-readable file (DEPLOY.md, "Capture
 * injection").
 *
 * It also checks the refusals it can cause itself: a memfd (not a dma-buf,
 * EBADF), a udmabuf (another device's memory, ENODEV) when /dev/udmabuf can
 * be opened, and a layout larger than the buffer (EINVAL).
 *
 * Usage: nvgpu-inject-test --socket PATH [--render PATH] [--size WxH]
 *          [--buffers N] [--frames N] [--hold SECS] [--out FILE]
 *          [--pingpong N]
 *
 *   --buffers N  buffers to inject (default 4), plus the live one
 *   --frames N   frames painted round-robin into them (default 240), timed
 *   --hold SECS  keep the connection (and so the ids) for SECS, painting the
 *                live buffer at 60 Hz, or until the backend hangs up
 *   --out FILE   write the command-line words there (a line), after the
 *                buffers are imported; the rig's hook waits for it
 *   --pingpong N while holding, N frames of explicit sync with the guest: a
 *                syncobj is injected with one more buffer; frame k is
 *                painted, glFinish()ed and announced at timeline point 2k-1,
 *                and frame k+1 painted only once the guest has signalled 2k.
 *                The time from announcing a frame to its release is printed
 *                (the round trip of the notification, and the guest's read)
 *
 * Needs NVIDIA's EGL and GBM (rig/rig-tools/inject-hook.sh runs it with the
 * guest image's copy of the host's userspace).
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <errno.h>
#include <fcntl.h>
#include <gbm.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#include <drm/drm.h>

#include "../guest-image/tools/capture-pattern.h"

#define FOURCC(a, b, c, d) ((uint32_t)(a) | (uint32_t)(b) << 8 | (uint32_t)(c) << 16 | (uint32_t)(d) << 24)
#define XR24 FOURCC('X', 'R', '2', '4')

/* protocol/src/inject.rs, little-endian on a little-endian host. */
enum { INJ_VERSION = 1, INJ_OP_HELLO = 1, INJ_OP_IMPORT = 2, INJ_OP_RELEASE = 3, INJ_OP_IMPORT_SYNCOBJ = 4 };
struct inj_hello {
	uint32_t op, version, flags, reserved;
};
struct inj_import {
	uint32_t op, nplanes, width, height, fourcc, flags;
	uint64_t modifier;
	uint32_t offsets[4], strides[4];
};
struct inj_release {
	uint32_t op, id;
};
struct inj_import_syncobj {
	uint32_t op, flags;
};
struct inj_reply {
	uint32_t op;
	int32_t status;
	uint32_t id, version;
	uint8_t token[16];
	uint32_t max_buffers, max_syncobjs;
	uint64_t max_bytes;
};
_Static_assert(sizeof(struct inj_hello) == 16, "hello");
_Static_assert(sizeof(struct inj_import) == 64, "import");
_Static_assert(sizeof(struct inj_release) == 8, "release");
_Static_assert(sizeof(struct inj_reply) == 48, "reply");

static double now_us(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

static int failures;
#define CHECK(cond, ...)                                                                           \
	do {                                                                                       \
		int ok_ = !!(cond);                                                                \
		fprintf(stderr, "inject-test: %s ", ok_ ? "ok  " : "FAIL");                        \
		fprintf(stderr, __VA_ARGS__);                                                      \
		fprintf(stderr, "\n");                                                             \
		if (!ok_)                                                                          \
			failures++;                                                                \
	} while (0)

/* ───────────────────────── the socket ───────────────────────── */

static int sock_connect(const char *path)
{
	int s = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_CLOEXEC, 0);
	struct sockaddr_un a = {.sun_family = AF_UNIX};
	snprintf(a.sun_path, sizeof a.sun_path, "%s", path);
	if (s < 0 || connect(s, (struct sockaddr *)&a, sizeof a) < 0) {
		fprintf(stderr, "inject-test: connect %s: %s\n", path, strerror(errno));
		return -1;
	}
	return s;
}

/* One request (with `fd` if >= 0) and its reply; -1 if the connection
 * broke. */
static int roundtrip(int s, const void *req, size_t len, int fd, struct inj_reply *r, double *us)
{
	struct iovec iov = {.iov_base = (void *)req, .iov_len = len};
	union {
		char buf[CMSG_SPACE(sizeof(int))];
		struct cmsghdr align;
	} c;
	struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1};
	if (fd >= 0) {
		m.msg_control = c.buf;
		m.msg_controllen = sizeof c.buf;
		struct cmsghdr *h = CMSG_FIRSTHDR(&m);
		h->cmsg_level = SOL_SOCKET;
		h->cmsg_type = SCM_RIGHTS;
		h->cmsg_len = CMSG_LEN(sizeof(int));
		memcpy(CMSG_DATA(h), &fd, sizeof(int));
	}
	double t0 = now_us();
	if (sendmsg(s, &m, MSG_NOSIGNAL) != (ssize_t)len)
		return -1;
	ssize_t n = recv(s, r, sizeof *r, 0);
	if (us)
		*us = now_us() - t0;
	return n == (ssize_t)sizeof *r ? 0 : -1;
}

static void hex(const uint8_t t[16], char out[33])
{
	for (int i = 0; i < 16; i++)
		sprintf(out + 2 * i, "%02x", t[i]);
}

/* ───────────────────────── GL ───────────────────────── */

struct buf {
	struct gbm_bo *bo;
	int fd;
	uint32_t stride, offset;
	uint64_t modifier;
	EGLImageKHR img;
	GLuint tex, fb;
	uint32_t id;
	uint8_t token[16];
	int frame;
	uint32_t fnv;
};

static GLuint prog;
static GLint frame_loc;

static GLuint shader(GLenum type, const char *src)
{
	GLuint s = glCreateShader(type);
	glShaderSource(s, 1, &src, NULL);
	glCompileShader(s);
	GLint ok = 0;
	glGetShaderiv(s, GL_COMPILE_STATUS, &ok);
	if (!ok) {
		char log[1024];
		glGetShaderInfoLog(s, sizeof log, NULL, log);
		fprintf(stderr, "inject-test: shader: %s\n", log);
	}
	return s;
}

static void gl_setup(void)
{
	static const char *vs = "#version 300 es\n"
				"void main() {\n"
				"  vec2 v = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
				"  gl_Position = vec4(v * 2.0 - 1.0, 0.0, 1.0);\n"
				"}\n";
	static const char *fs = "#version 300 es\n"
				"precision highp float;\n"
				"precision highp int;\n"
				"uniform uint frame;\n"
				"out vec4 o;\n"
				"void main() {\n" CAP_PATTERN_GLSL "}\n";
	prog = glCreateProgram();
	glAttachShader(prog, shader(GL_VERTEX_SHADER, vs));
	glAttachShader(prog, shader(GL_FRAGMENT_SHADER, fs));
	glLinkProgram(prog);
	frame_loc = glGetUniformLocation(prog, "frame");
	GLuint vao;
	glGenVertexArrays(1, &vao);
	glBindVertexArray(vao);
}

static void paint(struct buf *b, uint32_t w, uint32_t h, int frame)
{
	glBindFramebuffer(GL_FRAMEBUFFER, b->fb);
	glViewport(0, 0, (GLsizei)w, (GLsizei)h);
	glUseProgram(prog);
	glUniform1ui(frame_loc, (GLuint)frame);
	glDrawArrays(GL_TRIANGLES, 0, 3);
	b->frame = frame;
}

static int cmp_d(const void *a, const void *b)
{
	double x = *(const double *)a, y = *(const double *)b;
	return x < y ? -1 : x > y;
}

int main(int argc, char **argv)
{
	const char *sock_path = NULL, *render_path = "/dev/dri/renderD128", *out_path = NULL;
	uint32_t w = 1280, h = 720;
	int nbuf = 4, frames = 240, hold = 0, pingpong = 0;
	for (int i = 1; i < argc; i++) {
		const char *v = i + 1 < argc ? argv[i + 1] : NULL;
		if (!strcmp(argv[i], "--socket") && v)
			sock_path = v, i++;
		else if (!strcmp(argv[i], "--render") && v)
			render_path = v, i++;
		else if (!strcmp(argv[i], "--size") && v && sscanf(v, "%ux%u", &w, &h) == 2)
			i++;
		else if (!strcmp(argv[i], "--buffers") && v)
			nbuf = atoi(v), i++;
		else if (!strcmp(argv[i], "--frames") && v)
			frames = atoi(v), i++;
		else if (!strcmp(argv[i], "--hold") && v)
			hold = atoi(v), i++;
		else if (!strcmp(argv[i], "--out") && v)
			out_path = v, i++;
		else if (!strcmp(argv[i], "--pingpong") && v)
			pingpong = atoi(v), i++;
		else {
			fprintf(stderr, "usage: see the head of %s's source\n", argv[0]);
			return 2;
		}
	}
	if (!sock_path || nbuf < 1 || nbuf > 16 || frames < 1) {
		fprintf(stderr, "inject-test: --socket PATH; 1..16 buffers\n");
		return 2;
	}

	/* ── allocate as xdph does ── */
	int drm = open(render_path, O_RDWR | O_CLOEXEC);
	struct gbm_device *gbm = drm >= 0 ? gbm_create_device(drm) : NULL;
	if (!gbm) {
		fprintf(stderr, "inject-test: GBM on %s: %s\n", render_path, strerror(errno));
		return 1;
	}
	fprintf(stderr, "inject-test: GBM backend %s on %s\n", gbm_device_get_backend_name(gbm), render_path);
	PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	PFNEGLCREATEIMAGEKHRPROC create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
	PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_mods = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
	PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_tex = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
	EGLDisplay d = getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
	if (!eglInitialize(d, NULL, NULL)) {
		fprintf(stderr, "inject-test: eglInitialize 0x%x\n", eglGetError());
		return 1;
	}
	eglBindAPI(EGL_OPENGL_ES_API);
	EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
	EGLContext ctx = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
	eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, ctx);
	fprintf(stderr, "inject-test: EGL %s, GL %s\n", eglQueryString(d, EGL_VENDOR), glGetString(GL_RENDERER));

	/* The modifiers EGL renders XRGB8888 to, as the compositor would
	 * advertise them to xdph. */
	EGLuint64KHR mods[128];
	EGLBoolean ext[128];
	EGLint nm = 0, nr = 0;
	uint64_t render_mods[128];
	query_mods(d, (EGLint)XR24, 128, mods, ext, &nm);
	for (EGLint i = 0; i < nm; i++)
		if (!ext[i])
			render_mods[nr++] = mods[i];
	CHECK(nr > 0, "EGL renders XRGB8888 with %d modifier(s)", nr);

	/* + the live buffer, + the explicit-sync one, + the one released */
	int total = nbuf + 3;
	struct buf *bufs = calloc((size_t)total, sizeof *bufs);
	gl_setup();
	for (int i = 0; i < total; i++) {
		struct buf *b = &bufs[i];
		b->bo = gbm_bo_create_with_modifiers(gbm, w, h, XR24, render_mods, (unsigned)nr);
		if (!b->bo) {
			CHECK(0, "gbm_bo_create_with_modifiers %ux%u (%s)", w, h, strerror(errno));
			return 1;
		}
		b->fd = gbm_bo_get_fd(b->bo);
		b->stride = gbm_bo_get_stride_for_plane(b->bo, 0);
		b->offset = gbm_bo_get_offset(b->bo, 0);
		b->modifier = gbm_bo_get_modifier(b->bo);
		EGLint a[] = {EGL_WIDTH, (EGLint)w, EGL_HEIGHT, (EGLint)h, EGL_LINUX_DRM_FOURCC_EXT, (EGLint)XR24,
			      EGL_DMA_BUF_PLANE0_FD_EXT, b->fd, EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)b->offset,
			      EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)b->stride, EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
			      (EGLint)(b->modifier & 0xffffffff), EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
			      (EGLint)(b->modifier >> 32), EGL_NONE};
		b->img = create_image(d, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
		glGenTextures(1, &b->tex);
		glBindTexture(GL_TEXTURE_2D, b->tex);
		target_tex(GL_TEXTURE_2D, b->img);
		glGenFramebuffers(1, &b->fb);
		glBindFramebuffer(GL_FRAMEBUFFER, b->fb);
		glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, b->tex, 0);
		if (b->img == EGL_NO_IMAGE_KHR || glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
			CHECK(0, "buffer %d as a GL framebuffer (EGL 0x%x)", i, eglGetError());
			return 1;
		}
	}
	fprintf(stderr, "inject-test: %d GBM buffers %ux%u XR24, modifier 0x%016llx, stride %u, offset %u\n", total,
		w, h, (unsigned long long)bufs[0].modifier, bufs[0].stride, bufs[0].offset);

	/* ── hand them to the backend ── */
	int s = sock_connect(sock_path);
	if (s < 0)
		return 1;
	struct inj_reply r;
	struct inj_hello hello = {.op = INJ_OP_HELLO, .version = INJ_VERSION};
	if (roundtrip(s, &hello, sizeof hello, -1, &r, NULL) || r.status) {
		CHECK(0, "HELLO (status %d)", r.status);
		return 1;
	}
	CHECK(1, "HELLO: version %u, at most %u buffers and %llu MiB per VM", r.version, r.max_buffers,
	      (unsigned long long)(r.max_bytes >> 20));
	double import_us[32];
	for (int i = 0; i < total; i++) {
		struct buf *b = &bufs[i];
		struct inj_import imp = {.op = INJ_OP_IMPORT, .nplanes = 1, .width = w, .height = h, .fourcc = XR24,
					 .modifier = b->modifier};
		imp.offsets[0] = b->offset;
		imp.strides[0] = b->stride;
		if (roundtrip(s, &imp, sizeof imp, b->fd, &r, &import_us[i]) || r.status) {
			CHECK(0, "IMPORT of buffer %d (status %d): the backend refused a GBM buffer", i,
			      r.status);
			return 1;
		}
		b->id = r.id;
		memcpy(b->token, r.token, 16);
	}
	qsort(import_us, (size_t)total, sizeof(double), cmp_d);
	CHECK(1, "IMPORT of %d GBM buffers: all NVKMS memory of this GPU; round trip median %.0f us, max %.0f us",
	      total, import_us[total / 2], import_us[total - 1]);

	/* The refusals a helper can cause. */
	int mfd = memfd_create("not-a-dmabuf", MFD_CLOEXEC | MFD_ALLOW_SEALING);
	ftruncate(mfd, (off_t)bufs[0].stride * h);
	struct inj_import bad = {.op = INJ_OP_IMPORT, .nplanes = 1, .width = w, .height = h, .fourcc = XR24,
				 .modifier = bufs[0].modifier};
	bad.strides[0] = bufs[0].stride;
	CHECK(roundtrip(s, &bad, sizeof bad, mfd, &r, NULL) == 0 && r.status == -EBADF,
	      "a memfd is refused as no dma-buf (status %d)", r.status);
	int ud = open("/dev/udmabuf", O_RDWR | O_CLOEXEC);
	if (ud >= 0) {
		struct {
			uint32_t memfd, flags;
			uint64_t offset, size;
		} uc = {.memfd = (uint32_t)mfd, .flags = 1, .offset = 0,
			.size = ((uint64_t)bufs[0].stride * h + 4095) & ~4095ull};
		ftruncate(mfd, (off_t)uc.size);
		fcntl(mfd, F_ADD_SEALS, F_SEAL_SHRINK);
		int ubuf = ioctl(ud, _IOW('u', 0x42, uc), &uc);
		if (ubuf >= 0) {
			CHECK(roundtrip(s, &bad, sizeof bad, ubuf, &r, NULL) == 0 && r.status == -ENODEV,
			      "a udmabuf (another device's memory) is refused (status %d)", r.status);
			close(ubuf);
		} else {
			fprintf(stderr, "inject-test: skip udmabuf: UDMABUF_CREATE: %s\n", strerror(errno));
		}
		close(ud);
	} else {
		fprintf(stderr, "inject-test: skip udmabuf: /dev/udmabuf: %s\n", strerror(errno));
	}
	close(mfd);
	struct inj_import big = bad;
	big.height = h * 4;
	CHECK(roundtrip(s, &big, sizeof big, bufs[0].fd, &r, NULL) == 0 && r.status == -EINVAL,
	      "a layout larger than the buffer is refused (status %d)", r.status);

	/* One released before the guest can open it. */
	struct buf *released = &bufs[total - 1];
	struct inj_release rel = {.op = INJ_OP_RELEASE, .id = released->id};
	CHECK(roundtrip(s, &rel, sizeof rel, -1, &r, NULL) == 0 && r.status == 0, "RELEASE of id %u", released->id);

	/* The explicit-sync buffer's syncobj: a timeline on the helper's own
	 * render file, handed to the backend as a syncobj file. */
	struct buf *syncbuf = &bufs[nbuf + 1];
	uint32_t sync_handle = 0, sync_id = 0;
	uint8_t sync_token[16] = {0};
	if (pingpong > 0) {
		struct drm_syncobj_create sc = {0};
		struct drm_syncobj_handle sh = {0};
		if (ioctl(drm, DRM_IOCTL_SYNCOBJ_CREATE, &sc) == 0) {
			sh.handle = sc.handle;
			if (ioctl(drm, DRM_IOCTL_SYNCOBJ_HANDLE_TO_FD, &sh) == 0) {
				struct inj_import_syncobj is = {.op = INJ_OP_IMPORT_SYNCOBJ};
				if (roundtrip(s, &is, sizeof is, sh.fd, &r, NULL) == 0 && r.status == 0) {
					sync_handle = sc.handle;
					sync_id = r.id;
					memcpy(sync_token, r.token, 16);
				}
				close(sh.fd);
			}
		}
		CHECK(sync_id != 0, "IMPORT_SYNCOBJ of a timeline syncobj (status %d)", r.status);
		/* A memfd is no syncobj. */
		int nf = memfd_create("not-a-syncobj", MFD_CLOEXEC);
		struct inj_import_syncobj is = {.op = INJ_OP_IMPORT_SYNCOBJ};
		CHECK(roundtrip(s, &is, sizeof is, nf, &r, NULL) == 0 && r.status == -EBADF,
		      "a memfd is refused as no syncobj (status %d)", r.status);
		close(nf);
	}

	/* ── paint: frames round-robin, timed to glFinish ── */
	double *lat = calloc((size_t)frames, sizeof(double));
	double t_all = now_us();
	for (int f = 0; f < frames; f++) {
		double t0 = now_us();
		paint(&bufs[f % nbuf], w, h, f);
		glFinish();
		lat[f] = now_us() - t0;
	}
	t_all = now_us() - t_all;
	paint(&bufs[nbuf], w, h, 0); /* the live buffer */
	glFinish();
	qsort(lat, (size_t)frames, sizeof(double), cmp_d);
	CHECK(1, "%d frames painted into %d injected buffers in %.1f ms: paint+glFinish median %.0f us, p99 %.0f us",
	      frames, nbuf, t_all / 1000, lat[frames / 2], lat[frames * 99 / 100]);

	/* The host's own read of each: the pattern, and its checksum. */
	uint8_t *px = malloc((size_t)w * h * 4);
	for (int i = 0; i < nbuf; i++) {
		struct buf *b = &bufs[i];
		glBindFramebuffer(GL_FRAMEBUFFER, b->fb);
		glReadPixels(0, 0, (GLsizei)w, (GLsizei)h, GL_RGBA, GL_UNSIGNED_BYTE, px);
		b->fnv = cap_fnv(px, w, h, w * 4);
		uint64_t mis = cap_mismatches(px, w, h, w * 4, (uint32_t)b->frame);
		CHECK(mis == 0 && b->fnv == cap_expected_fnv(w, h, (uint32_t)b->frame),
		      "buffer id %u holds frame %d, checksum %08x (%llu pixels off)", b->id, b->frame, b->fnv,
		      (unsigned long long)mis);
	}
	free(px);

	/* ── what the guest needs ── */
	char line[4096], tok[33];
	int n = snprintf(line, sizeof line, "nvgpu_cap=");
	for (int i = 0; i < nbuf; i++) {
		hex(bufs[i].token, tok);
		n += snprintf(line + n, sizeof line - (size_t)n, "%s%u:%s:%d:%08x", i ? "," : "", bufs[i].id, tok,
			      bufs[i].frame, bufs[i].fnv);
	}
	hex(bufs[nbuf].token, tok);
	n += snprintf(line + n, sizeof line - (size_t)n, " nvgpu_cap_live=%u:%s", bufs[nbuf].id, tok);
	hex(released->token, tok);
	n += snprintf(line + n, sizeof line - (size_t)n, " nvgpu_cap_released=%u:%s nvgpu_cap_size=%ux%u",
		      released->id, tok, w, h);
	if (sync_id) {
		char stok[33];
		hex(syncbuf->token, tok);
		hex(sync_token, stok);
		n += snprintf(line + n, sizeof line - (size_t)n, " nvgpu_cap_sync=%u:%s:%u:%s:%d", syncbuf->id, tok,
			      sync_id, stok, pingpong);
	}
	printf("%s\n", line);
	fflush(stdout);
	if (out_path) {
		char tmp[4096];
		snprintf(tmp, sizeof tmp, "%s.tmp", out_path);
		FILE *f = fopen(tmp, "w");
		if (f) {
			fprintf(f, "%s\n", line);
			fclose(f);
			rename(tmp, out_path);
		}
	}
	fprintf(stderr, "inject-test: %s\n", failures ? "FAILED" : "ALL PASS (host side)");

	/* ── hold: keep the ids, repaint the live buffer ── */
	double end = now_us() + hold * 1e6;
	int live = 1;
	/* Explicit sync: frame k announced at 2k-1, released at 2k. */
	int pk = 0;
	double announced = 0, *rtt = pingpong > 0 ? calloc((size_t)pingpong, sizeof(double)) : NULL;
	double last_live = 0;
	if (sync_id) {
		pk = 1;
		paint(syncbuf, w, h, pk);
		glFinish();
		uint64_t pt = 1;
		struct drm_syncobj_timeline_array sig = {.handles = (uintptr_t)&sync_handle,
							 .points = (uintptr_t)&pt,
							 .count_handles = 1};
		ioctl(drm, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &sig);
		announced = now_us();
	}
	while (now_us() < end) {
		struct pollfd p = {.fd = s, .events = POLLIN};
		if (poll(&p, 1, 0) > 0 && (p.revents & (POLLHUP | POLLERR | POLLIN))) {
			fprintf(stderr, "inject-test: the backend hung up\n");
			break;
		}
		if (pk > 0 && pk <= pingpong) {
			/* Sleep in the release wait itself (16 ms at most), so its
			 * signal is seen when it comes. */
			uint64_t pt = 2ull * (uint64_t)pk;
			struct timespec now;
			clock_gettime(CLOCK_MONOTONIC, &now);
			struct drm_syncobj_timeline_wait wt = {
				.handles = (uintptr_t)&sync_handle,
				.points = (uintptr_t)&pt,
				.timeout_nsec = (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + 16000000,
				.count_handles = 1,
				.flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT};
			if (ioctl(drm, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &wt) == 0) {
				rtt[pk - 1] = now_us() - announced;
				if (++pk <= pingpong) {
					paint(syncbuf, w, h, pk);
					glFinish();
					uint64_t ap = 2ull * (uint64_t)pk - 1;
					struct drm_syncobj_timeline_array sig = {.handles = (uintptr_t)&sync_handle,
										 .points = (uintptr_t)&ap,
										 .count_handles = 1};
					ioctl(drm, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &sig);
					announced = now_us();
				} else {
					/* The first frame waited for the guest to boot. */
					int half = pingpong / 2, m = pingpong - half - 1;
					if (m > 0) {
						qsort(rtt + 1, (size_t)(half > 1 ? half - 1 : 0), sizeof(double), cmp_d);
						qsort(rtt + half, (size_t)(pingpong - half), sizeof(double), cmp_d);
						fprintf(stderr,
							"inject-test: ok   explicit sync: %d frames announced and released; "
							"announce to release, guest reading each frame: median %.0f us; "
							"guest only waiting and signalling: median %.0f us, p99 %.0f us\n",
							pingpong, half > 1 ? rtt[1 + (half - 1) / 2] : 0.0,
							rtt[half + (pingpong - half) / 2],
							rtt[half + (pingpong - half) * 99 / 100]);
					}
				}
			}
		} else {
			usleep(1000);
		}
		if (now_us() - last_live >= 16000) {
			paint(&bufs[nbuf], w, h, live++);
			glFinish();
			last_live = now_us();
		}
	}
	if (pk > 0 && pk <= pingpong)
		fprintf(stderr, "inject-test: explicit sync: %d of %d frames released\n", pk - 1, pingpong);
	fprintf(stderr, "inject-test: done after %d live frames\n", live - 1);
	close(s);
	return failures ? 1 : 0;
}
