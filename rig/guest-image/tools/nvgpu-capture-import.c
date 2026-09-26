// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-capture-import -- open a buffer the host's capture helper injected,
 * as a guest capture daemon would, and prove it is the host's memory:
 *
 *   1. /dev/nvgpu-capture's OPEN with the id and token: a read-only dma-buf
 *      and the buffer's description (fourcc, modifier, offsets, strides);
 *   2. the CPU: a writable mapping of the dma-buf, and of the object through
 *      the render node, is refused; a read-only one works;
 *   3. EGL: EGL_EXT_image_dma_buf_import(_modifiers) into a texture, drawn
 *      into a linear framebuffer and read back;
 *   4. Vulkan: VK_EXT_image_drm_format_modifier + VK_EXT_external_memory_
 *      dma_buf, the image copied into a buffer and read back;
 *
 * and every pixel of 3 and 4 is checked against the pattern the host painted
 * for the given frame (capture-pattern.h), with the checksum the host
 * printed. It is also the reference for what a guest daemon does with an id
 * and a token: steps 1, 3 and 4, and nothing else.
 *
 * TOKENS NEVER GO ON A COMMAND LINE. A process's arguments are readable by
 * every user of its system (/proc/PID/cmdline), and so is the kernel's
 * command line; a token there is a token every guest process knows, which
 * is no token at all. A daemon keeps the helper's tokens in its own memory,
 * from its own vsock channel. This tool takes them on a descriptor
 * (--token-fd: one token, 32 hex digits, per line; the syncobj's second).
 *
 * Usage: nvgpu-capture-import --id N --token-fd FD [--frame F] [--fnv HEX]
 *          [--render PATH] [--node PATH] [--no-egl] [--no-vk]
 *          [--expect-errno E] [--watch MS] [--no-cpu]
 *          [--sync ID --pingpong N]
 *   --token-fd FD  read the buffer's token (and, with --sync, the syncobj's
 *                  on the next line) from descriptor FD; 0 is stdin
 *   --token HEX32  the token on the command line: for poking at the node by
 *                  hand in a throwaway guest, NEVER in a daemon or a script
 *                  that runs where other users are (see above)
 *   --frame F      the frame the host painted last into this buffer; without
 *                  it the frame is read from pixel (0,0) and the rest of the
 *                  image checked against it
 *   --fnv HEX      the host's checksum of that frame (nvgpu-inject-test)
 *   --expect-errno the OPEN must fail with this errno (e.g. 2, ENOENT, for a
 *                  wrong token or a released id); nothing else is done
 *   --no-cpu       skip step 2: no CPU mapping at all, so what 3 and 4 cost
 *                  the shared window is what a GPU-only consumer costs it
 *   --watch MS     read the buffer through Vulkan again after MS ms (the host
 *                  keeps painting it): the frame must have moved on, without
 *                  a new OPEN; a read of a frame half painted is counted as
 *                  a tear (there is no sync in this first cut)
 *   --sync ID --pingpong N
 *                  explicit sync with the helper's injected syncobj
 *                  (OPEN_SYNCOBJ): N frames, each waited for at its acquire
 *                  point (2k-1), read, and released (2k); see pingpong();
 *                  with --expect-sync-errno E, OPEN_SYNCOBJ must fail so
 * Output: "capture-import: ok|FAIL <what>" lines, then "ALL PASS" or
 * "FAILED"; exit 0 only when everything passed.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES3/gl3.h>
#include <GLES2/gl2ext.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <drm/drm.h>
#include <vulkan/vulkan.h>

#include "capture-pattern.h"

/* driver/uapi/nvgpu_capture.h, mirrored (a daemon need not build against the
 * kernel tree); the size is asserted. */
struct nvgpu_capture_open {
	int32_t render_fd;
	uint32_t id;
	uint8_t token[16];
	uint32_t flags;
	int32_t dmabuf_fd;
	uint32_t width, height, fourcc, nplanes;
	uint64_t modifier;
	uint32_t offsets[4], strides[4];
	uint32_t buf_flags, pad;
	uint64_t size;
};
_Static_assert(sizeof(struct nvgpu_capture_open) == 104, "uapi size");
#define NVGPU_CAPTURE_IOC_OPEN _IOWR('C', 0x40, struct nvgpu_capture_open)
struct nvgpu_capture_open_syncobj {
	int32_t render_fd;
	uint32_t id;
	uint8_t token[16];
	uint32_t flags;
	uint32_t handle;
};
_Static_assert(sizeof(struct nvgpu_capture_open_syncobj) == 32, "uapi size");
#define NVGPU_CAPTURE_IOC_OPEN_SYNCOBJ _IOWR('C', 0x41, struct nvgpu_capture_open_syncobj)

/* drm_nvidia_gem_map_offset_params, answered by the guest module. */
struct gem_map_offset {
	uint32_t handle, pad;
	uint64_t offset;
};
#define DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET _IOWR('d', 0x4a, struct gem_map_offset)
/* PRIME_FD_TO_HANDLE, GEM_CLOSE and the syncobj ioctls: <drm/drm.h>. */

#define FOURCC(a, b, c, d) ((uint32_t)(a) | (uint32_t)(b) << 8 | (uint32_t)(c) << 16 | (uint32_t)(d) << 24)

static int failures;
#define OK(cond, ...)                                                                              \
	do {                                                                                       \
		int ok_ = !!(cond);                                                                \
		printf("capture-import: %s ", ok_ ? "ok  " : "FAIL");                              \
		printf(__VA_ARGS__);                                                               \
		printf("\n");                                                                      \
		fflush(stdout);                                                                    \
		if (!ok_)                                                                          \
			failures++;                                                                \
	} while (0)

static double now_us(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec * 1e6 + t.tv_nsec / 1e3;
}

/* The frame an RGBA image's pixel (0,0) was painted in, mod 256 (3 has
 * the inverse 171 mod 256), or -1 if its green disagrees. */
static int frame_of(const uint8_t *px)
{
	uint32_t f = (171u * px[0]) & 255u;
	return ((5u * f) & 255u) == px[1] ? (int)f : -1;
}

/* Check an RGBA image against frame `frame` (or the frame of its first
 * pixel, when -1). Returns the mismatching pixels; *f gets the frame. */
static uint64_t check_image(const char *who, const uint8_t *rgba, uint32_t w, uint32_t h, int frame,
			    uint32_t fnv_want, int *f)
{
	int fr = frame >= 0 ? frame : frame_of(rgba);
	if (fr < 0) {
		OK(0, "%s: pixel (0,0) %u,%u,%u is no frame of the pattern", who, rgba[0], rgba[1], rgba[2]);
		*f = -1;
		return (uint64_t)w * h;
	}
	*f = fr;
	uint64_t bad = cap_mismatches(rgba, w, h, w * 4, (uint32_t)fr);
	uint32_t fnv = cap_fnv(rgba, w, h, w * 4);
	OK(bad == 0, "%s: %ux%u pixels are frame %d's pattern (%llu differ)", who, w, h, fr,
	   (unsigned long long)bad);
	if (bad == 0 && frame >= 0)
		OK(fnv == cap_expected_fnv(w, h, (uint32_t)fr), "%s: checksum %08x", who, fnv);
	if (fnv_want)
		OK(fnv == fnv_want, "%s: checksum %08x is the host's %08x", who, fnv, fnv_want);
	return bad;
}

/* ───────────────────────────── the CPU ───────────────────────────── */

static void cpu_checks(int render, const struct nvgpu_capture_open *o)
{
	size_t len = (size_t)o->size;
	void *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, o->dmabuf_fd, 0);
	OK(p == MAP_FAILED, "mmap of the dma-buf PROT_WRITE refused (%s)",
	   p == MAP_FAILED ? strerror(errno) : "it was mapped");
	if (p != MAP_FAILED)
		munmap(p, len);

	p = mmap(NULL, len, PROT_READ, MAP_SHARED, o->dmabuf_fd, 0);
	OK(p != MAP_FAILED, "mmap of the dma-buf PROT_READ (%s)", p == MAP_FAILED ? strerror(errno) : "mapped");
	if (p != MAP_FAILED) {
		volatile uint8_t b = ((volatile uint8_t *)p)[0];
		(void)b;
		OK(mprotect(p, len, PROT_READ | PROT_WRITE) != 0, "mprotect of it to PROT_WRITE refused (%s)",
		   strerror(errno));
		munmap(p, len);
	}

	/* The same object through the render node: a handle for it in this
	 * file, the node's offset for it, and a writable mapping of that. */
	struct drm_prime_handle ph = {.fd = o->dmabuf_fd};
	if (ioctl(render, DRM_IOCTL_PRIME_FD_TO_HANDLE, &ph) != 0) {
		OK(0, "PRIME_FD_TO_HANDLE of the dma-buf on the render node (%s)", strerror(errno));
		return;
	}
	struct gem_map_offset mo = {.handle = ph.handle};
	if (ioctl(render, DRM_IOCTL_NVIDIA_GEM_MAP_OFFSET, &mo) != 0) {
		OK(0, "GEM_MAP_OFFSET on the render node (%s)", strerror(errno));
	} else {
		p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, render, (off_t)mo.offset);
		OK(p == MAP_FAILED, "mmap through the render node PROT_WRITE refused (%s)",
		   p == MAP_FAILED ? strerror(errno) : "it was mapped");
		if (p != MAP_FAILED)
			munmap(p, len);
		p = mmap(NULL, len, PROT_READ, MAP_SHARED, render, (off_t)mo.offset);
		OK(p != MAP_FAILED, "mmap through the render node PROT_READ (%s)",
		   p == MAP_FAILED ? strerror(errno) : "mapped");
		if (p != MAP_FAILED)
			munmap(p, len);
	}
	struct drm_gem_close gc = {.handle = ph.handle};
	ioctl(render, DRM_IOCTL_GEM_CLOSE, &gc);
}

/* ───────────────────────────── EGL ───────────────────────────── */

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
		fprintf(stderr, "shader: %s\n", log);
	}
	return s;
}

static void egl_check(const struct nvgpu_capture_open *o, int frame, uint32_t fnv)
{
	PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	PFNEGLCREATEIMAGEKHRPROC create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
	PFNEGLDESTROYIMAGEKHRPROC destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
	PFNEGLQUERYDMABUFMODIFIERSEXTPROC query_mods = (void *)eglGetProcAddress("eglQueryDmaBufModifiersEXT");
	PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_tex = (void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
	if (!getdpy || !create_image || !target_tex) {
		OK(0, "EGL: the dma-buf import entry points");
		return;
	}
	EGLDisplay d = getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
	if (!eglInitialize(d, NULL, NULL)) {
		OK(0, "EGL: eglInitialize (0x%x)", eglGetError());
		return;
	}
	const char *exts = eglQueryString(d, EGL_EXTENSIONS);
	OK(exts && strstr(exts, "EGL_EXT_image_dma_buf_import_modifiers"), "EGL: %s, with dma-buf import modifiers",
	   eglQueryString(d, EGL_VENDOR));
	eglBindAPI(EGL_OPENGL_ES_API);
	EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
	EGLContext c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
	if (c == EGL_NO_CONTEXT || !eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c)) {
		OK(0, "EGL: a GLES 3 context (0x%x)", eglGetError());
		return;
	}

	/* Whether the driver takes this modifier only as an external image. */
	EGLBoolean external = EGL_FALSE, listed = EGL_FALSE;
	if (query_mods) {
		EGLuint64KHR mods[128];
		EGLBoolean ext[128];
		EGLint n = 0;
		query_mods(d, (EGLint)o->fourcc, 128, mods, ext, &n);
		for (EGLint i = 0; i < n; i++)
			if (mods[i] == o->modifier) {
				listed = EGL_TRUE;
				external = ext[i];
			}
	}
	OK(listed, "EGL: modifier 0x%016llx is one EGL lists for this format%s", (unsigned long long)o->modifier,
	   external ? " (external only)" : "");

	double t0 = now_us();
	EGLint a[64], n = 0;
	a[n++] = EGL_WIDTH, a[n++] = (EGLint)o->width;
	a[n++] = EGL_HEIGHT, a[n++] = (EGLint)o->height;
	a[n++] = EGL_LINUX_DRM_FOURCC_EXT, a[n++] = (EGLint)o->fourcc;
	static const EGLint pl[4][5] = {
		{EGL_DMA_BUF_PLANE0_FD_EXT, EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGL_DMA_BUF_PLANE0_PITCH_EXT,
		 EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT},
		{EGL_DMA_BUF_PLANE1_FD_EXT, EGL_DMA_BUF_PLANE1_OFFSET_EXT, EGL_DMA_BUF_PLANE1_PITCH_EXT,
		 EGL_DMA_BUF_PLANE1_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE1_MODIFIER_HI_EXT},
		{EGL_DMA_BUF_PLANE2_FD_EXT, EGL_DMA_BUF_PLANE2_OFFSET_EXT, EGL_DMA_BUF_PLANE2_PITCH_EXT,
		 EGL_DMA_BUF_PLANE2_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE2_MODIFIER_HI_EXT},
		{EGL_DMA_BUF_PLANE3_FD_EXT, EGL_DMA_BUF_PLANE3_OFFSET_EXT, EGL_DMA_BUF_PLANE3_PITCH_EXT,
		 EGL_DMA_BUF_PLANE3_MODIFIER_LO_EXT, EGL_DMA_BUF_PLANE3_MODIFIER_HI_EXT},
	};
	for (uint32_t p = 0; p < o->nplanes && p < 4; p++) {
		a[n++] = pl[p][0], a[n++] = o->dmabuf_fd;
		a[n++] = pl[p][1], a[n++] = (EGLint)o->offsets[p];
		a[n++] = pl[p][2], a[n++] = (EGLint)o->strides[p];
		a[n++] = pl[p][3], a[n++] = (EGLint)(o->modifier & 0xffffffff);
		a[n++] = pl[p][4], a[n++] = (EGLint)(o->modifier >> 32);
	}
	a[n++] = EGL_NONE;
	EGLImageKHR img = create_image(d, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
	if (img == EGL_NO_IMAGE_KHR) {
		OK(0, "EGL: eglCreateImageKHR from the dma-buf (0x%x)", eglGetError());
		return;
	}
	GLenum target = external ? GL_TEXTURE_EXTERNAL_OES : GL_TEXTURE_2D;
	GLuint tex;
	glGenTextures(1, &tex);
	glBindTexture(target, tex);
	glTexParameteri(target, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
	glTexParameteri(target, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
	target_tex(target, img);
	OK(glGetError() == GL_NO_ERROR, "EGL: the image bound to a %s texture",
	   external ? "GL_TEXTURE_EXTERNAL_OES" : "GL_TEXTURE_2D");

	/* Drawn, pixel for pixel, into a linear RGBA8 framebuffer. */
	static const char *vs = "#version 300 es\n"
				"void main() {\n"
				"  vec2 v = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));\n"
				"  gl_Position = vec4(v * 2.0 - 1.0, 0.0, 1.0);\n"
				"}\n";
	static const char *fs2d = "#version 300 es\n"
				  "precision highp float;\n"
				  "uniform highp sampler2D t;\n"
				  "out vec4 o;\n"
				  "void main() { o = texelFetch(t, ivec2(gl_FragCoord.xy), 0); }\n";
	static const char *fsext = "#version 300 es\n"
				   "#extension GL_OES_EGL_image_external_essl3 : require\n"
				   "precision highp float;\n"
				   "uniform highp samplerExternalOES t;\n"
				   "uniform vec2 size;\n"
				   "out vec4 o;\n"
				   "void main() { o = texture(t, gl_FragCoord.xy / size); }\n";
	GLuint prog = glCreateProgram();
	glAttachShader(prog, shader(GL_VERTEX_SHADER, vs));
	glAttachShader(prog, shader(GL_FRAGMENT_SHADER, external ? fsext : fs2d));
	glLinkProgram(prog);
	glUseProgram(prog);
	glUniform1i(glGetUniformLocation(prog, "t"), 0);
	if (external)
		glUniform2f(glGetUniformLocation(prog, "size"), (float)o->width, (float)o->height);
	GLuint rb, fb, vao;
	glGenRenderbuffers(1, &rb);
	glBindRenderbuffer(GL_RENDERBUFFER, rb);
	glRenderbufferStorage(GL_RENDERBUFFER, GL_RGBA8, (GLsizei)o->width, (GLsizei)o->height);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferRenderbuffer(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_RENDERBUFFER, rb);
	glGenVertexArrays(1, &vao);
	glBindVertexArray(vao);
	glViewport(0, 0, (GLsizei)o->width, (GLsizei)o->height);
	glActiveTexture(GL_TEXTURE0);
	glBindTexture(target, tex);
	glDrawArrays(GL_TRIANGLES, 0, 3);
	uint8_t *px = malloc((size_t)o->width * o->height * 4);
	glReadPixels(0, 0, (GLsizei)o->width, (GLsizei)o->height, GL_RGBA, GL_UNSIGNED_BYTE, px);
	GLenum err = glGetError();
	double t1 = now_us();
	OK(err == GL_NO_ERROR, "EGL: drawn and read back in %.0f us (glGetError 0x%x)", t1 - t0, err);
	int f;
	check_image("EGL", px, o->width, o->height, frame, fnv, &f);
	free(px);
	destroy_image(d, img);
	eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
	eglDestroyContext(d, c);
	eglTerminate(d);
}

/* ───────────────────────────── Vulkan ───────────────────────────── */

struct vk {
	VkInstance inst;
	VkPhysicalDevice pd;
	VkDevice dev;
	VkQueue q;
	uint32_t qf;
	VkCommandPool pool;
	PFN_vkGetMemoryFdPropertiesKHR fd_props;
};

static VkFormat vk_format(uint32_t fourcc, int *swap_rb)
{
	*swap_rb = 0;
	switch (fourcc) {
	case FOURCC('X', 'R', '2', '4'):
	case FOURCC('A', 'R', '2', '4'):
		*swap_rb = 1;
		return VK_FORMAT_B8G8R8A8_UNORM;
	case FOURCC('X', 'B', '2', '4'):
	case FOURCC('A', 'B', '2', '4'):
		return VK_FORMAT_R8G8B8A8_UNORM;
	default:
		return VK_FORMAT_UNDEFINED;
	}
}

static int vk_init(struct vk *v)
{
	VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .apiVersion = VK_API_VERSION_1_2};
	VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app};
	if (vkCreateInstance(&ici, NULL, &v->inst) != VK_SUCCESS)
		return -1;
	uint32_t n = 1;
	if (vkEnumeratePhysicalDevices(v->inst, &n, &v->pd) < 0 || n == 0)
		return -1;
	uint32_t nq = 0;
	vkGetPhysicalDeviceQueueFamilyProperties(v->pd, &nq, NULL);
	VkQueueFamilyProperties qp[16];
	if (nq > 16)
		nq = 16;
	vkGetPhysicalDeviceQueueFamilyProperties(v->pd, &nq, qp);
	v->qf = UINT32_MAX;
	for (uint32_t i = 0; i < nq && v->qf == UINT32_MAX; i++)
		if (qp[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_TRANSFER_BIT))
			v->qf = i;
	float prio = 1.0f;
	VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
				     .queueFamilyIndex = v->qf,
				     .queueCount = 1,
				     .pQueuePriorities = &prio};
	const char *exts[] = {"VK_KHR_external_memory_fd", "VK_EXT_external_memory_dma_buf",
			      "VK_EXT_image_drm_format_modifier"};
	VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
				  .queueCreateInfoCount = 1,
				  .pQueueCreateInfos = &q,
				  .enabledExtensionCount = 3,
				  .ppEnabledExtensionNames = exts};
	if (vkCreateDevice(v->pd, &dci, NULL, &v->dev) != VK_SUCCESS)
		return -2;
	vkGetDeviceQueue(v->dev, v->qf, 0, &v->q);
	v->fd_props = (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(v->dev, "vkGetMemoryFdPropertiesKHR");
	VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				       .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
				       .queueFamilyIndex = v->qf};
	vkCreateCommandPool(v->dev, &pci, NULL, &v->pool);
	return 0;
}

static uint32_t mem_type(struct vk *v, uint32_t bits, VkMemoryPropertyFlags want)
{
	VkPhysicalDeviceMemoryProperties mp;
	vkGetPhysicalDeviceMemoryProperties(v->pd, &mp);
	for (uint32_t i = 0; i < mp.memoryTypeCount; i++)
		if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
			return i;
	return UINT32_MAX;
}

/* An imported image and what copies it into a host-visible buffer. */
struct vkimg {
	VkImage image;
	VkDeviceMemory mem, bmem;
	VkBuffer buf;
	VkCommandBuffer cb;
	VkFence fence;
	VkDeviceSize bytes;
	int swap;
};

static void vk_free(struct vk *v, struct vkimg *m)
{
	if (m->fence)
		vkDestroyFence(v->dev, m->fence, NULL);
	if (m->cb)
		vkFreeCommandBuffers(v->dev, v->pool, 1, &m->cb);
	if (m->buf)
		vkDestroyBuffer(v->dev, m->buf, NULL);
	if (m->bmem)
		vkFreeMemory(v->dev, m->bmem, NULL);
	if (m->image)
		vkDestroyImage(v->dev, m->image, NULL);
	if (m->mem)
		vkFreeMemory(v->dev, m->mem, NULL);
	memset(m, 0, sizeof(*m));
}

/* Import the dma-buf as an image with its modifier, and record the copy of
 * it into a host-visible buffer, once. 0, or -1 having said why. */
static int vk_import(struct vk *v, const struct nvgpu_capture_open *o, struct vkimg *m)
{
	memset(m, 0, sizeof(*m));
	VkFormat fmt = vk_format(o->fourcc, &m->swap);
	if (fmt == VK_FORMAT_UNDEFINED) {
		OK(0, "Vulkan: fourcc 0x%08x has no format here", o->fourcc);
		return -1;
	}
	VkSubresourceLayout layout = {.offset = o->offsets[0], .rowPitch = o->strides[0]};
	VkImageDrmFormatModifierExplicitCreateInfoEXT mod = {
		.sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_EXPLICIT_CREATE_INFO_EXT,
		.drmFormatModifier = o->modifier,
		.drmFormatModifierPlaneCount = 1,
		.pPlaneLayouts = &layout};
	VkExternalMemoryImageCreateInfo ext = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
					       .pNext = &mod,
					       .handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
	VkImageCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
				 .pNext = &ext,
				 .imageType = VK_IMAGE_TYPE_2D,
				 .format = fmt,
				 .extent = {o->width, o->height, 1},
				 .mipLevels = 1,
				 .arrayLayers = 1,
				 .samples = VK_SAMPLE_COUNT_1_BIT,
				 .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT,
				 .usage = VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT,
				 .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED};
	VkResult r = vkCreateImage(v->dev, &ici, NULL, &m->image);
	if (r != VK_SUCCESS) {
		OK(0, "Vulkan: an image of modifier 0x%016llx (%d)", (unsigned long long)o->modifier, r);
		m->image = VK_NULL_HANDLE;
		return -1;
	}
	VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
	r = v->fd_props(v->dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, o->dmabuf_fd, &fp);
	VkMemoryRequirements req;
	vkGetImageMemoryRequirements(v->dev, m->image, &req);
	uint32_t mt = mem_type(v, req.memoryTypeBits & fp.memoryTypeBits, 0);
	if (r != VK_SUCCESS || mt == UINT32_MAX) {
		OK(0, "Vulkan: a memory type for the dma-buf (%d, fd bits 0x%x, image bits 0x%x)", r,
		   fp.memoryTypeBits, req.memoryTypeBits);
		vk_free(v, m);
		return -1;
	}
	VkMemoryDedicatedAllocateInfo ded = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
					     .image = m->image};
	VkImportMemoryFdInfoKHR imp = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
				       .pNext = &ded,
				       .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
				       .fd = dup(o->dmabuf_fd)};
	VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				    .pNext = &imp,
				    .allocationSize = o->size > req.size ? o->size : req.size,
				    .memoryTypeIndex = mt};
	r = vkAllocateMemory(v->dev, &mai, NULL, &m->mem);
	if (r != VK_SUCCESS) {
		OK(0, "Vulkan: import of the dma-buf (%d)", r);
		close(imp.fd);
		m->mem = VK_NULL_HANDLE;
		vk_free(v, m);
		return -1;
	}
	vkBindImageMemory(v->dev, m->image, m->mem, 0);

	m->bytes = (VkDeviceSize)o->width * o->height * 4;
	VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
				  .size = m->bytes,
				  .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT};
	vkCreateBuffer(v->dev, &bci, NULL, &m->buf);
	VkMemoryRequirements breq;
	vkGetBufferMemoryRequirements(v->dev, m->buf, &breq);
	VkMemoryAllocateInfo bai = {
		.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
		.allocationSize = breq.size,
		.memoryTypeIndex = mem_type(v, breq.memoryTypeBits,
					    VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT)};
	vkAllocateMemory(v->dev, &bai, NULL, &m->bmem);
	vkBindBufferMemory(v->dev, m->buf, m->bmem, 0);

	VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
					   .commandPool = v->pool,
					   .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
					   .commandBufferCount = 1};
	vkAllocateCommandBuffers(v->dev, &cai, &m->cb);
	VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
	vkBeginCommandBuffer(m->cb, &bi);
	/* The contents are the host's: from GENERAL, which keeps them, not from
	 * UNDEFINED, which may discard them; and back to GENERAL after. */
	VkImageMemoryBarrier bar = {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
				    .srcAccessMask = 0,
				    .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
				    .oldLayout = VK_IMAGE_LAYOUT_GENERAL,
				    .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL,
				    .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
				    .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
				    .image = m->image,
				    .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}};
	vkCmdPipelineBarrier(m->cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL,
			     0, NULL, 1, &bar);
	VkBufferImageCopy copy = {.imageSubresource = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1},
				  .imageExtent = {o->width, o->height, 1}};
	vkCmdCopyImageToBuffer(m->cb, m->image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, m->buf, 1, &copy);
	bar.srcAccessMask = VK_ACCESS_TRANSFER_READ_BIT;
	bar.dstAccessMask = 0;
	bar.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL;
	bar.newLayout = VK_IMAGE_LAYOUT_GENERAL;
	vkCmdPipelineBarrier(m->cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT, 0, 0,
			     NULL, 0, NULL, 1, &bar);
	vkEndCommandBuffer(m->cb);
	VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
	vkCreateFence(v->dev, &fci, NULL, &m->fence);
	return 0;
}

/* One copy of the image: its RGBA bytes (malloc'd), or NULL. */
static uint8_t *vk_copy(struct vk *v, struct vkimg *m)
{
	vkResetFences(v->dev, 1, &m->fence);
	VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &m->cb};
	VkResult r = vkQueueSubmit(v->q, 1, &si, m->fence);
	if (r == VK_SUCCESS)
		r = vkWaitForFences(v->dev, 1, &m->fence, VK_TRUE, 5000000000ull);
	if (r != VK_SUCCESS) {
		OK(0, "Vulkan: the copy (%d)", r);
		return NULL;
	}
	void *p;
	vkMapMemory(v->dev, m->bmem, 0, m->bytes, 0, &p);
	uint8_t *out = malloc(m->bytes);
	memcpy(out, p, m->bytes);
	vkUnmapMemory(v->dev, m->bmem);
	if (m->swap)
		for (VkDeviceSize i = 0; i < m->bytes; i += 4) {
			uint8_t t = out[i];
			out[i] = out[i + 2];
			out[i + 2] = t;
		}
	return out;
}

/* The imported image, copied once into a host-visible buffer: RGBA bytes. */
static uint8_t *vk_read(struct vk *v, const struct nvgpu_capture_open *o, double *us)
{
	struct vkimg m;
	double t0 = now_us();
	if (vk_import(v, o, &m))
		return NULL;
	uint8_t *out = vk_copy(v, &m);
	*us = now_us() - t0;
	vk_free(v, &m);
	return out;
}

/* ─────────────────── explicit sync: the helper's syncobj ─────────────────── */

/*
 * The first cut's second step, as a guest capture daemon would run it: the
 * helper signals point 2k-1 when frame k is in the buffer (acquire), the
 * guest waits for it, reads the frame, and signals 2k (release); the helper
 * paints frame k+1 only after 2k. Every read must then be one whole frame,
 * the right one. The first half of the frames is checked pixel for pixel;
 * the second only waits and signals, which is what the notification itself
 * costs (the helper times both).
 */
static void pingpong(int cap, int render, const struct nvgpu_capture_open *o, uint32_t sync_id,
		     const uint8_t sync_token[16], int n, int expect_errno)
{
	struct nvgpu_capture_open_syncobj so = {.render_fd = render, .id = sync_id};
	memcpy(so.token, sync_token, 16);
	int r = ioctl(cap, NVGPU_CAPTURE_IOC_OPEN_SYNCOBJ, &so);
	if (expect_errno) {
		OK(r && errno == expect_errno, "OPEN_SYNCOBJ of id %u refused with errno %d, as expected %d",
		   sync_id, r ? errno : 0, expect_errno);
		return;
	}
	if (r) {
		OK(0, "OPEN_SYNCOBJ of id %u (%s)", sync_id, strerror(errno));
		return;
	}
	OK(so.handle != 0, "OPEN_SYNCOBJ of id %u: syncobj handle %u in the render file", sync_id, so.handle);
	struct vk v = {0};
	struct vkimg m;
	if (vk_init(&v) || vk_import(&v, o, &m)) {
		OK(0, "Vulkan: the sync buffer's import");
		return;
	}
	int bad_frames = 0, torn = 0, checked = 0;
	double wait_us = 0, check_us = 0, signal_us = 0;
	int k;
	for (k = 1; k <= n; k++) {
		uint64_t acquire = 2ull * k - 1, release = 2ull * k;
		struct timespec now;
		clock_gettime(CLOCK_MONOTONIC, &now);
		/* The first frame waits for the helper to start; the rest are
		 * a frame apart. */
		int64_t deadline = (int64_t)now.tv_sec * 1000000000 + now.tv_nsec + (k == 1 ? 60 : 5) * 1000000000ll;
		struct drm_syncobj_timeline_wait w = {.handles = (uintptr_t)&so.handle,
						      .points = (uintptr_t)&acquire,
						      .timeout_nsec = deadline,
						      .count_handles = 1,
						      .flags = DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT};
		double t0 = now_us();
		if (ioctl(render, DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, &w)) {
			OK(0, "explicit sync: waiting for acquire point %llu (%s)", (unsigned long long)acquire,
			   strerror(errno));
			break;
		}
		double t1 = now_us();
		if (k > 1)
			wait_us += t1 - t0;
		if (k <= n / 2) {
			uint8_t *px = vk_copy(&v, &m);
			if (!px)
				break;
			int f = frame_of(px);
			if (f != (k & 255))
				bad_frames++;
			else if (cap_mismatches(px, o->width, o->height, o->width * 4, (uint32_t)f))
				torn++;
			checked++;
			free(px);
		}
		double t2 = now_us();
		if (k <= n / 2)
			check_us += t2 - t1;
		struct drm_syncobj_timeline_array sig = {.handles = (uintptr_t)&so.handle,
							 .points = (uintptr_t)&release,
							 .count_handles = 1};
		if (ioctl(render, DRM_IOCTL_SYNCOBJ_TIMELINE_SIGNAL, &sig)) {
			OK(0, "explicit sync: signalling release point %llu (%s)", (unsigned long long)release,
			   strerror(errno));
			break;
		}
		signal_us += now_us() - t2;
	}
	int done = k - 1;
	OK(done == n, "explicit sync: %d of %d frames acquired and released through the helper's syncobj", done, n);
	OK(checked > 0 && bad_frames == 0 && torn == 0,
	   "explicit sync: %d frames read after their acquire point: %d not the frame signalled, %d torn", checked,
	   bad_frames, torn);
	if (done > 1)
		printf("capture-import: note explicit sync: per frame, wait for acquire %.0f us (the helper's "
		       "paint included), frame check %.0f us, signal release %.0f us\n",
		       wait_us / (done - 1), checked ? check_us / checked : 0.0, signal_us / done);
	struct drm_syncobj_destroy d = {.handle = so.handle};
	ioctl(render, DRM_IOCTL_SYNCOBJ_DESTROY, &d);
	vk_free(&v, &m);
	vkDestroyCommandPool(v.dev, v.pool, NULL);
	vkDestroyDevice(v.dev, NULL);
	vkDestroyInstance(v.inst, NULL);
}

static void vk_check(const struct nvgpu_capture_open *o, int frame, uint32_t fnv, int watch_ms)
{
	struct vk v = {0};
	int e = vk_init(&v);
	OK(e == 0, "Vulkan: a device with VK_EXT_image_drm_format_modifier and VK_EXT_external_memory_dma_buf (%d)",
	   e);
	if (e)
		return;
	double us = 0;
	uint8_t *px = vk_read(&v, o, &us);
	if (!px)
		return;
	OK(1, "Vulkan: imported, copied and read back in %.0f us", us);
	int f1;
	if (watch_ms > 0) {
		/* The host is painting it now: the frame is read from its first
		 * pixel, and a read that caught a frame half done is a tear,
		 * which this first cut, with no sync, allows. */
		f1 = frame_of(px);
		uint64_t torn =
			f1 < 0 ? 1 : cap_mismatches(px, o->width, o->height, o->width * 4, (uint32_t)f1);
		OK(f1 >= 0, "Vulkan: the live buffer shows frame %d%s", f1, torn ? " (torn)" : "");
	} else {
		check_image("Vulkan", px, o->width, o->height, frame, fnv, &f1);
	}
	free(px);
	if (watch_ms > 0) {
		usleep((useconds_t)watch_ms * 1000);
		px = vk_read(&v, o, &us);
		if (!px)
			return;
		int f2 = frame_of(px);
		uint64_t torn = f2 < 0 ? 1 : cap_mismatches(px, o->width, o->height, o->width * 4, (uint32_t)f2);
		OK(f2 >= 0 && f2 != f1, "Vulkan: %d ms later the same open shows frame %d (was %d): the host's "
		   "writes are seen without a copy", watch_ms, f2, f1);
		printf("capture-import: note %s (%llu pixels of another frame: no sync in this first cut)\n",
		       torn ? "the read was torn" : "the read was whole", (unsigned long long)torn);
		free(px);
	}
	vkDestroyCommandPool(v.dev, v.pool, NULL);
	vkDestroyDevice(v.dev, NULL);
	vkDestroyInstance(v.inst, NULL);
}

/* ───────────────────────────── main ───────────────────────────── */

static int hex_token(const char *s, uint8_t t[16])
{
	if (strlen(s) < 32 || (s[32] && s[32] != '\n'))
		return -1;
	for (int i = 0; i < 16; i++) {
		unsigned b;
		if (sscanf(s + 2 * i, "%2x", &b) != 1)
			return -1;
		t[i] = (uint8_t)b;
	}
	return 0;
}

int main(int argc, char **argv)
{
	const char *node = "/dev/nvgpu-capture", *render_path = "/dev/dri/renderD128";
	struct nvgpu_capture_open o = {0};
	int frame = -1, no_egl = 0, no_vk = 0, no_cpu = 0, expect_errno = 0, watch = 0, have_token = 0;
	int pp = 0, expect_sync_errno = 0;
	uint32_t fnv = 0, sync_id = 0;
	uint8_t sync_token[16] = {0};
	int token_fd = -1;
	for (int i = 1; i < argc; i++) {
		const char *v = i + 1 < argc ? argv[i + 1] : NULL;
		if (!strcmp(argv[i], "--id") && v)
			o.id = (uint32_t)strtoul(v, NULL, 0), i++;
		else if (!strcmp(argv[i], "--token-fd") && v)
			token_fd = atoi(v), i++;
		/* On the command line: every user can read it (see the head). */
		else if (!strcmp(argv[i], "--token") && v)
			have_token = hex_token(v, o.token) == 0, i++;
		else if (!strcmp(argv[i], "--frame") && v)
			frame = atoi(v), i++;
		else if (!strcmp(argv[i], "--fnv") && v)
			fnv = (uint32_t)strtoul(v, NULL, 16), i++;
		else if (!strcmp(argv[i], "--render") && v)
			render_path = v, i++;
		else if (!strcmp(argv[i], "--node") && v)
			node = v, i++;
		else if (!strcmp(argv[i], "--expect-errno") && v)
			expect_errno = atoi(v), i++;
		else if (!strcmp(argv[i], "--watch") && v)
			watch = atoi(v), i++;
		else if (!strcmp(argv[i], "--no-egl"))
			no_egl = 1;
		else if (!strcmp(argv[i], "--no-vk"))
			no_vk = 1;
		else if (!strcmp(argv[i], "--no-cpu"))
			no_cpu = 1;
		else if (!strcmp(argv[i], "--sync") && v) {
			sync_id = (uint32_t)strtoul(v, NULL, 0);
			/* ID:TOKEN, the command line again: by hand only. */
			if (strchr(v, ':') && hex_token(strchr(v, ':') + 1, sync_token))
				return 2;
			i++;
		} else if (!strcmp(argv[i], "--pingpong") && v)
			pp = atoi(v), i++;
		else if (!strcmp(argv[i], "--expect-sync-errno") && v)
			expect_sync_errno = atoi(v), i++;
		else {
			fprintf(stderr, "usage: see the head of %s's source\n", argv[0]);
			return 2;
		}
	}
	if (token_fd >= 0) {
		FILE *f = fdopen(token_fd, "r");
		char line[80];
		have_token = f && fgets(line, sizeof line, f) && hex_token(line, o.token) == 0;
		if (have_token && sync_id && !(fgets(line, sizeof line, f) && hex_token(line, sync_token) == 0)) {
			fprintf(stderr, "--token-fd: no syncobj token on the second line\n");
			return 2;
		}
	}
	if (!have_token) {
		fprintf(stderr, "a token: --token-fd FD, 32 hex digits a line\n");
		return 2;
	}
	int cap = open(node, O_RDWR | O_CLOEXEC);
	int render = open(render_path, O_RDWR | O_CLOEXEC);
	if (cap < 0 || render < 0) {
		OK(0, "open %s and %s (%s)", node, render_path, strerror(errno));
		goto out;
	}
	o.render_fd = render;
	double t0 = now_us();
	int r = ioctl(cap, NVGPU_CAPTURE_IOC_OPEN, &o);
	double t1 = now_us();
	int err = r ? errno : 0;
	if (expect_errno) {
		OK(err == expect_errno, "OPEN of id %u refused with errno %d (%s), as expected %d", o.id, err,
		   strerror(err), expect_errno);
		goto out;
	}
	if (r) {
		OK(0, "OPEN of id %u (%s)", o.id, strerror(err));
		goto out;
	}
	OK(o.dmabuf_fd >= 0, "OPEN of id %u in %.0f us: dma-buf %d, %ux%u fourcc %.4s modifier 0x%016llx, %u "
	   "plane(s), offset %u stride %u, %llu bytes", o.id, t1 - t0, o.dmabuf_fd, o.width, o.height,
	   (const char *)&o.fourcc, (unsigned long long)o.modifier, o.nplanes, o.offsets[0], o.strides[0],
	   (unsigned long long)o.size);
	OK((fcntl(o.dmabuf_fd, F_GETFL) & O_ACCMODE) == O_RDONLY, "the dma-buf is open read-only");
	if (pp > 0) {
		pingpong(cap, render, &o, sync_id, sync_token, pp, expect_sync_errno);
		goto out;
	}
	if (!no_cpu)
		cpu_checks(render, &o);
	if (!no_egl)
		egl_check(&o, frame, fnv);
	if (!no_vk)
		vk_check(&o, frame, fnv, watch);
out:
	printf("capture-import: %s\n", failures ? "FAILED" : "ALL PASS");
	return failures ? 1 : 0;
}
