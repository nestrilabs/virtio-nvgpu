// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-rm-dmabuf -- video memory exported as a dma-buf through RM's
 * EXPORT_TO_DMABUF_FD, and imported back, with no CUDA.
 *
 * CUDA reaches the same escape through cuMemGetHandleForAddressRange (see
 * cuda-dmabuf.c), but only where it offers dma-buf export at all, which a
 * GeForce card does not (CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED is 0 on the
 * RTX 5090, natively too). This makes the calls NVIDIA's userspace makes,
 * on its own: an RM client, a device and subdevice, NV01_MEMORY_LOCAL_USER
 * video memory, and the export; then
 *
 *   size     the dma-buf's lseek end is the allocation's size
 *   mmap     a CPU mapping of it is refused (nv_dma_buf_mmap: never on a
 *            discrete GPU)
 *   drm      PRIME_FD_TO_HANDLE into an NVIDIA render node, and what
 *            GEM_IDENTIFY_OBJECT says it is
 *   egl      EGL imports it as a linear image (EGL_EXT_image_dma_buf_import),
 *            clears it and reads the clear back
 *   vk       Vulkan imports it (VK_EXT_external_memory_dma_buf), fills it,
 *            and reads the fill back through a copy
 *   append   the append form (fd >= 0) on the complete dma-buf is refused
 *
 * NVIDIA's EGL and Vulkan refuse RM's dma-bufs on a discrete GPU natively
 * (seen on an RTX 5090 with 595.99.02): their refusal is a SKIP unless
 * --expect-import says the host takes them.
 *
 * `--loops N` does it all N times, so a leak shows up in a later round.
 * With `--expect-refused` the export itself must fail (the backend's knob
 * off) and nothing else runs.
 *
 * One "rm-dmabuf: PASS|FAIL|SKIP <step> ..." line per step; the exit status
 * is the number of failed steps.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <GLES3/gl3.h>
#include <vulkan/vulkan.h>

#include "rm-dmabuf.h"

static int failures;
static const char *render_path;

/* Whether NVIDIA's EGL and Vulkan must take the dma-buf (--expect-import).
 * Natively on the RTX 5090 (595.99.02) neither does -- EGL_BAD_ALLOC, and
 * no memory type for it -- so by default a refusal of theirs is a SKIP:
 * what is tested is that the guest refuses where the host does, and that
 * nothing else breaks. */
static int expect_import;
#define REFUSED (expect_import ? "FAIL" : "SKIP")

static void report(const char *verdict, const char *step, const char *fmt, ...)
	__attribute__((format(printf, 3, 4)));
static void report(const char *verdict, const char *step, const char *fmt, ...)
{
	va_list ap;

	printf("rm-dmabuf: %s %s", verdict, step);
	if (fmt && *fmt) {
		putchar(' ');
		va_start(ap, fmt);
		vprintf(fmt, ap);
		va_end(ap);
	}
	putchar('\n');
	fflush(stdout);
	if (!strcmp(verdict, "FAIL"))
		failures++;
}

/* ───────── DRM ───────── */

struct prime_handle {
	uint32_t handle, flags;
	int32_t fd;
};
#define PRIME_FD_TO_HANDLE _IOWR('d', 0x2e, struct prime_handle)
struct gem_close_arg {
	uint32_t handle, pad;
};
#define GEM_CLOSE _IOW('d', 0x09, struct gem_close_arg)
struct identify {
	uint32_t handle, object_type;
};
#define NV_GEM_IDENTIFY _IOWR('d', 0x4e, struct identify)

static void drm_step(int fd)
{
	char path[64];
	struct prime_handle p = {.fd = fd};
	struct identify id;
	struct gem_close_arg c;
	int r = -1, i;

	if (render_path) {
		r = open(render_path, O_RDWR | O_CLOEXEC);
	} else {
		for (i = 128; i < 136 && r < 0; i++) {
			snprintf(path, sizeof(path), "/dev/dri/renderD%d", i);
			r = open(path, O_RDWR | O_CLOEXEC);
		}
	}
	if (r < 0) {
		report("SKIP", "drm", "no render node: %s", strerror(errno));
		return;
	}
	if (ioctl(r, PRIME_FD_TO_HANDLE, &p)) {
		report("FAIL", "drm", "PRIME_FD_TO_HANDLE: %s", strerror(errno));
		close(r);
		return;
	}
	memset(&id, 0, sizeof(id));
	id.handle = p.handle;
	if (ioctl(r, NV_GEM_IDENTIFY, &id))
		report("PASS", "drm", "imported as handle %u; IDENTIFY: %s", p.handle,
		       strerror(errno));
	else
		report("PASS", "drm", "imported as handle %u, object type %u (1: dma-buf)",
		       p.handle, id.object_type);
	c.handle = p.handle;
	c.pad = 0;
	ioctl(r, GEM_CLOSE, &c);
	close(r);
}

/* ───────── EGL ───────── */

/* The dma-buf as a 1024x512 linear ABGR8888 image (RMD_SIZE bytes), made a
 * framebuffer's colour attachment, cleared to a colour and read back. */
static void egl_step(int fd, uint32_t round)
{
	PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy =
		(void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	PFNEGLCREATEIMAGEKHRPROC create_image = (void *)eglGetProcAddress("eglCreateImageKHR");
	PFNEGLDESTROYIMAGEKHRPROC destroy_image = (void *)eglGetProcAddress("eglDestroyImageKHR");
	PFNGLEGLIMAGETARGETTEXTURE2DOESPROC target_tex =
		(void *)eglGetProcAddress("glEGLImageTargetTexture2DOES");
	const EGLint w = 1024, h = (EGLint)(RMD_SIZE / 4 / 1024);
	EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
	EGLint a[] = {EGL_WIDTH, w, EGL_HEIGHT, h,
		      EGL_LINUX_DRM_FOURCC_EXT, 0x34324241 /* AB24 */,
		      EGL_DMA_BUF_PLANE0_FD_EXT, fd,
		      EGL_DMA_BUF_PLANE0_OFFSET_EXT, 0,
		      EGL_DMA_BUF_PLANE0_PITCH_EXT, w * 4,
		      EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, 0,
		      EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, 0,
		      EGL_NONE};
	uint8_t want[4] = {0x11, (uint8_t)round, 0x33, 0xff}, *px = NULL;
	GLuint tex = 0, fbo = 0;
	EGLContext c = EGL_NO_CONTEXT;
	EGLImageKHR img = EGL_NO_IMAGE_KHR;
	EGLDisplay d;
	long i, bad = 0;

	if (!getdpy || !create_image || !destroy_image || !target_tex) {
		report("FAIL", "egl", "no dma-buf import entry points");
		return;
	}
	d = getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
	if (!eglInitialize(d, NULL, NULL)) {
		report("FAIL", "egl", "eglInitialize: %#x", eglGetError());
		return;
	}
	eglBindAPI(EGL_OPENGL_ES_API);
	c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
	if (c == EGL_NO_CONTEXT || !eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c)) {
		report("FAIL", "egl", "a GLES 3 context: %#x", eglGetError());
		goto out;
	}
	img = create_image(d, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, a);
	if (img == EGL_NO_IMAGE_KHR) {
		report(REFUSED, "egl", "eglCreateImageKHR (%s): %#x", eglQueryString(d, EGL_VENDOR),
		       eglGetError());
		goto out;
	}
	glGenTextures(1, &tex);
	glBindTexture(GL_TEXTURE_2D, tex);
	target_tex(GL_TEXTURE_2D, img);
	glGenFramebuffers(1, &fbo);
	glBindFramebuffer(GL_FRAMEBUFFER, fbo);
	glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
	if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
		report("FAIL", "egl", "the image is no complete framebuffer (%#x)", glGetError());
		goto out;
	}
	glClearColor(want[0] / 255.0f, want[1] / 255.0f, want[2] / 255.0f, 1.0f);
	glClear(GL_COLOR_BUFFER_BIT);
	px = malloc((size_t)w * h * 4);
	if (!px)
		goto out;
	glReadPixels(0, 0, w, h, GL_RGBA, GL_UNSIGNED_BYTE, px);
	for (i = 0; i < (long)w * h; i++)
		bad += memcmp(px + i * 4, want, 4) != 0;
	if (glGetError() != GL_NO_ERROR || bad)
		report("FAIL", "egl", "%ld of %ld pixels read back wrong (%#x)", bad, (long)w * h,
		       glGetError());
	else
		report("PASS", "egl", "imported, cleared and read back %dx%d", w, h);
out:
	free(px);
	if (fbo)
		glDeleteFramebuffers(1, &fbo);
	if (tex)
		glDeleteTextures(1, &tex);
	if (img != EGL_NO_IMAGE_KHR)
		destroy_image(d, img);
	eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
	if (c != EGL_NO_CONTEXT)
		eglDestroyContext(d, c);
	eglTerminate(d);
}

/* ───────── Vulkan ───────── */

struct vk {
	VkInstance inst;
	VkPhysicalDevice pd;
	VkDevice dev;
	VkQueue q;
	uint32_t qf;
	VkCommandPool pool;
	PFN_vkGetMemoryFdPropertiesKHR fd_props;
};

static int vk_init(struct vk *v)
{
	VkApplicationInfo app = {.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
				 .apiVersion = VK_API_VERSION_1_2};
	VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
				    .pApplicationInfo = &app};
	const char *exts[] = {VK_KHR_EXTERNAL_MEMORY_FD_EXTENSION_NAME,
			      VK_EXT_EXTERNAL_MEMORY_DMA_BUF_EXTENSION_NAME};
	VkQueueFamilyProperties qp[16];
	uint32_t n = 1, nq = 16, i;
	float prio = 1;

	if (vkCreateInstance(&ici, NULL, &v->inst) != VK_SUCCESS)
		return -1;
	if (vkEnumeratePhysicalDevices(v->inst, &n, &v->pd) < 0 || n == 0)
		return -1;
	vkGetPhysicalDeviceQueueFamilyProperties(v->pd, &nq, qp);
	for (i = 0; i < nq; i++)
		if (qp[i].queueFlags & (VK_QUEUE_TRANSFER_BIT | VK_QUEUE_GRAPHICS_BIT))
			break;
	if (i == nq)
		return -1;
	v->qf = i;
	VkDeviceQueueCreateInfo q = {.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
				     .queueFamilyIndex = v->qf,
				     .queueCount = 1,
				     .pQueuePriorities = &prio};
	VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
				  .queueCreateInfoCount = 1,
				  .pQueueCreateInfos = &q,
				  .enabledExtensionCount = 2,
				  .ppEnabledExtensionNames = exts};
	if (vkCreateDevice(v->pd, &dci, NULL, &v->dev) != VK_SUCCESS)
		return -1;
	vkGetDeviceQueue(v->dev, v->qf, 0, &v->q);
	v->fd_props = (PFN_vkGetMemoryFdPropertiesKHR)vkGetDeviceProcAddr(
		v->dev, "vkGetMemoryFdPropertiesKHR");
	VkCommandPoolCreateInfo pci = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
				       .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
				       .queueFamilyIndex = v->qf};
	return vkCreateCommandPool(v->dev, &pci, NULL, &v->pool) == VK_SUCCESS ? 0 : -1;
}

static int mem_type(struct vk *v, uint32_t bits, VkMemoryPropertyFlags want)
{
	VkPhysicalDeviceMemoryProperties mp;
	uint32_t i;

	vkGetPhysicalDeviceMemoryProperties(v->pd, &mp);
	for (i = 0; i < mp.memoryTypeCount; i++)
		if ((bits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & want) == want)
			return (int)i;
	return -1;
}

/* Fill `dst` with `fill`, then copy it to `host`; submitted and waited for. */
static int vk_fill_copy(struct vk *v, VkBuffer dst, VkBuffer host, uint32_t fill)
{
	VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
					   .commandPool = v->pool,
					   .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
					   .commandBufferCount = 1};
	VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
	VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
	VkBufferCopy copy = {.size = RMD_SIZE};
	VkMemoryBarrier mb = {.sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
			      .srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
			      .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT};
	VkCommandBuffer cb;
	VkFence fence;
	int ret = -1;

	if (vkAllocateCommandBuffers(v->dev, &cai, &cb) != VK_SUCCESS)
		return -1;
	vkBeginCommandBuffer(cb, &bi);
	vkCmdFillBuffer(cb, dst, 0, RMD_SIZE, fill);
	vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0,
			     1, &mb, 0, NULL, 0, NULL);
	vkCmdCopyBuffer(cb, dst, host, 1, &copy);
	vkEndCommandBuffer(cb);
	VkSubmitInfo si = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
			   .commandBufferCount = 1,
			   .pCommandBuffers = &cb};
	if (vkCreateFence(v->dev, &fci, NULL, &fence) == VK_SUCCESS) {
		if (vkQueueSubmit(v->q, 1, &si, fence) == VK_SUCCESS &&
		    vkWaitForFences(v->dev, 1, &fence, VK_TRUE, 10ull * 1000 * 1000 * 1000) ==
			    VK_SUCCESS)
			ret = 0;
		vkDestroyFence(v->dev, fence, NULL);
	}
	vkFreeCommandBuffers(v->dev, v->pool, 1, &cb);
	return ret;
}

static VkBuffer vk_buffer(struct vk *v, int external)
{
	VkExternalMemoryBufferCreateInfo ext = {
		.sType = VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_BUFFER_CREATE_INFO,
		.handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
	VkBufferCreateInfo bci = {.sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO,
				  .pNext = external ? &ext : NULL,
				  .size = RMD_SIZE,
				  .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
					   VK_BUFFER_USAGE_TRANSFER_DST_BIT,
				  .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
	VkBuffer b = VK_NULL_HANDLE;

	vkCreateBuffer(v->dev, &bci, NULL, &b);
	return b;
}

static void vk_step(struct vk *v, int fd, uint32_t round)
{
	VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
	VkBuffer ext = VK_NULL_HANDLE, host = VK_NULL_HANDLE;
	VkDeviceMemory emem = VK_NULL_HANDLE, hmem = VK_NULL_HANDLE;
	VkMemoryRequirements req, hreq;
	uint32_t *p = NULL, i, bad = 0, fill = 0xc0ffee00u ^ round;
	int t, dupfd = -1;
	VkResult r;

	r = v->fd_props(v->dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fp);
	if (r != VK_SUCCESS || !fp.memoryTypeBits) {
		report(REFUSED, "vk", "vkGetMemoryFdPropertiesKHR: %d, memoryTypeBits %#x", r,
		       fp.memoryTypeBits);
		return;
	}
	ext = vk_buffer(v, 1);
	host = vk_buffer(v, 0);
	if (!ext || !host) {
		report("FAIL", "vk", "vkCreateBuffer");
		goto out;
	}
	vkGetBufferMemoryRequirements(v->dev, ext, &req);
	t = mem_type(v, req.memoryTypeBits & fp.memoryTypeBits, 0);
	dupfd = dup(fd);
	if (t < 0 || dupfd < 0) {
		report("FAIL", "vk", "no memory type in %#x & %#x", req.memoryTypeBits,
		       fp.memoryTypeBits);
		goto out;
	}
	VkMemoryDedicatedAllocateInfo ded = {.sType = VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
					     .buffer = ext};
	VkImportMemoryFdInfoKHR imp = {.sType = VK_STRUCTURE_TYPE_IMPORT_MEMORY_FD_INFO_KHR,
				       .pNext = &ded,
				       .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
				       .fd = dupfd};
	VkMemoryAllocateInfo mai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				    .pNext = &imp,
				    .allocationSize = req.size,
				    .memoryTypeIndex = (uint32_t)t};
	r = vkAllocateMemory(v->dev, &mai, NULL, &emem);
	if (r != VK_SUCCESS) {
		report(REFUSED, "vk", "vkAllocateMemory(import): %d", r);
		goto out;
	}
	dupfd = -1; /* Vulkan's now */
	vkBindBufferMemory(v->dev, ext, emem, 0);
	vkGetBufferMemoryRequirements(v->dev, host, &hreq);
	t = mem_type(v, hreq.memoryTypeBits,
		     VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT);
	VkMemoryAllocateInfo hai = {.sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
				    .allocationSize = hreq.size,
				    .memoryTypeIndex = (uint32_t)t};
	if (t < 0 || vkAllocateMemory(v->dev, &hai, NULL, &hmem) != VK_SUCCESS) {
		report("FAIL", "vk", "host-visible memory");
		goto out;
	}
	vkBindBufferMemory(v->dev, host, hmem, 0);
	if (vkMapMemory(v->dev, hmem, 0, RMD_SIZE, 0, (void **)&p) != VK_SUCCESS) {
		report("FAIL", "vk", "vkMapMemory");
		p = NULL;
		goto out;
	}
	if (vk_fill_copy(v, ext, host, fill)) {
		report("FAIL", "vk", "fill and copy of the import");
		goto out;
	}
	for (i = 0; i < RMD_SIZE / 4; i++)
		bad += p[i] != fill;
	if (bad)
		report("FAIL", "vk", "%u of %u words read back wrong", bad, RMD_SIZE / 4);
	else
		report("PASS", "vk", "imported, filled and read back %u bytes", RMD_SIZE);
out:
	if (p)
		vkUnmapMemory(v->dev, hmem);
	if (dupfd >= 0)
		close(dupfd);
	if (ext)
		vkDestroyBuffer(v->dev, ext, NULL);
	if (host)
		vkDestroyBuffer(v->dev, host, NULL);
	if (emem)
		vkFreeMemory(v->dev, emem, NULL);
	if (hmem)
		vkFreeMemory(v->dev, hmem, NULL);
}

static void fdinfo_name(int fd, char *out, size_t n)
{
	char path[64], line[256];
	FILE *f;

	snprintf(out, n, "?");
	snprintf(path, sizeof(path), "/proc/self/fdinfo/%d", fd);
	f = fopen(path, "r");
	if (!f)
		return;
	while (fgets(line, sizeof(line), f))
		if (!strncmp(line, "exp_name:", 9)) {
			snprintf(out, n, "%s", line + 9 + strspn(line + 9, " \t"));
			out[strcspn(out, "\n")] = 0;
		}
	fclose(f);
}

int main(int argc, char **argv)
{
	int loops = 1, expect_refused = 0, skip_vk = 0, skip_drm = 0, skip_egl = 0, a;
	struct rmd_ctx c;
	struct vk v = {0};
	int have_vk = 0;
	uint32_t round;

	for (a = 1; a < argc; a++) {
		if (!strcmp(argv[a], "--loops") && a + 1 < argc)
			loops = atoi(argv[++a]);
		else if (!strcmp(argv[a], "--render") && a + 1 < argc)
			render_path = argv[++a];
		else if (!strcmp(argv[a], "--no-vk"))
			skip_vk = 1;
		else if (!strcmp(argv[a], "--no-drm"))
			skip_drm = 1;
		else if (!strcmp(argv[a], "--no-egl"))
			skip_egl = 1;
		else if (!strcmp(argv[a], "--expect-import"))
			expect_import = 1;
		else if (!strcmp(argv[a], "--expect-refused"))
			expect_refused = 1;
		else {
			fprintf(stderr,
				"usage: %s [--loops N] [--render PATH] [--no-vk] [--no-drm] "
				"[--no-egl] [--expect-import] [--expect-refused]\n",
				argv[0]);
			return 2;
		}
	}
	if (rmd_open(&c)) {
		report("FAIL", "rm", "%s", c.why);
		return 1;
	}
	report("PASS", "rm", "client %#x, video memory %#x (%u bytes)", c.client, c.mem, RMD_SIZE);
	if (!skip_vk && !expect_refused) {
		have_vk = !vk_init(&v);
		if (!have_vk)
			report("FAIL", "vk-init", "no Vulkan device with dma-buf import");
	}

	for (round = 0; round < (uint32_t)loops; round++) {
		struct rmd_export e;
		char name[64];
		off_t end;
		void *m;
		int r = rmd_export(&c, c.client, c.mem, -1, &e);

		if (expect_refused) {
			if (r == 0 && e.status == 0 && e.fd >= 0) {
				report("FAIL", "export", "made fd %d with the export refused", e.fd);
				close(e.fd);
			} else {
				report("PASS", "export", "refused: ioctl %s, status %#x",
				       r ? strerror(-r) : "ok", e.status);
			}
			break;
		}
		if (r || e.status || e.fd < 0) {
			report("FAIL", "export", "ioctl %s, status %#x, fd %d",
			       r ? strerror(-r) : "ok", e.status, e.fd);
			break;
		}
		fdinfo_name(e.fd, name, sizeof(name));
		report("PASS", "export", "round %u: fd %d, exporter %s", round, e.fd, name);
		end = lseek(e.fd, 0, SEEK_END);
		if (end != (off_t)RMD_SIZE)
			report("FAIL", "size", "lseek end %lld, want %u", (long long)end, RMD_SIZE);
		else
			report("PASS", "size", "%lld", (long long)end);
		m = mmap(NULL, RMD_SIZE, PROT_READ, MAP_SHARED, e.fd, 0);
		if (m == MAP_FAILED) {
			report("PASS", "mmap", "refused: %s", strerror(errno));
		} else {
			report("FAIL", "mmap", "a CPU mapping of video memory was made");
			munmap(m, RMD_SIZE);
		}
		if (!skip_drm)
			drm_step(e.fd);
		if (!skip_egl)
			egl_step(e.fd, round);
		if (have_vk)
			vk_step(&v, e.fd, round);
		/* The append form on a dma-buf that is already whole. */
		{
			struct rmd_export again;

			r = rmd_export(&c, c.client, c.mem, e.fd, &again);
			if (r == 0 && again.status == 0)
				report("FAIL", "append", "a second export into a whole dma-buf "
							 "was taken");
			else
				report("PASS", "append", "refused: ioctl %s, status %#x",
				       r ? strerror(-r) : "ok", again.status);
		}
		if (close(e.fd))
			report("FAIL", "close", "%s", strerror(errno));
	}

	if (have_vk) {
		vkDestroyCommandPool(v.dev, v.pool, NULL);
		vkDestroyDevice(v.dev, NULL);
		vkDestroyInstance(v.inst, NULL);
	}
	rmd_close(&c);
	printf("rm-dmabuf: %s (%d failed)\n", failures ? "FAIL" : "PASS", failures);
	return failures;
}
