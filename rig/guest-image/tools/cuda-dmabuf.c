// SPDX-License-Identifier: Apache-2.0
/*
 * cuda-dmabuf -- CUDA memory exported as a dma-buf through RM, and imported
 * back.
 *
 * cuMemGetHandleForAddressRange(CU_MEM_RANGE_HANDLE_TYPE_DMA_BUF_FD) is RM's
 * EXPORT_TO_DMABUF_FD, which a guest reaches only with the backend's
 * --allow-dmabuf-export (DEPLOY.md, "dma-buf export through RM"). This
 * exports a CUDA allocation holding a known pattern and then, step by step:
 *
 *   size     the dma-buf's lseek end is the allocation's size
 *   mmap     a CPU mapping of it is refused, as on a discrete GPU natively
 *            (nv_dma_buf_mmap: can_mmap only on an iGPU, and never asked)
 *   drm      PRIME_FD_TO_HANDLE into an NVIDIA render node, and what
 *            GEM_IDENTIFY_OBJECT says the object is
 *   vk       Vulkan imports it (VK_EXT_external_memory_dma_buf) and reads
 *            the pattern CUDA wrote; then fills it, and CUDA reads that
 *
 * and then does it all again `--loops` times, so a leak on either side
 * shows up as a failure in a later round (or in the backend's handle count,
 * which the probe that runs this compares).
 *
 * libcuda is dlopen()ed, as in cuda-smoke.c; Vulkan is linked. With --expect-
 * refused the export itself is expected to fail (the knob is off), and
 * nothing else runs.
 *
 * Prints one "cuda-dmabuf: PASS|FAIL|SKIP <step> ..." line per step; the
 * exit status is the number of failed steps.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>
#include <vulkan/vulkan.h>

typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
typedef unsigned long long CUdeviceptr;

#define CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED 124
#define CU_MEM_RANGE_HANDLE_TYPE_DMA_BUF_FD 1

static struct {
	CUresult (*Init)(unsigned);
	CUresult (*DeviceGet)(CUdevice *, int);
	CUresult (*DeviceGetAttribute)(int *, int, CUdevice);
	CUresult (*CtxCreate)(CUcontext *, unsigned, CUdevice);
	CUresult (*CtxDestroy)(CUcontext);
	CUresult (*CtxSynchronize)(void);
	CUresult (*MemAlloc)(CUdeviceptr *, size_t);
	CUresult (*MemFree)(CUdeviceptr);
	CUresult (*MemcpyHtoD)(CUdeviceptr, const void *, size_t);
	CUresult (*MemcpyDtoH)(void *, CUdeviceptr, size_t);
	CUresult (*MemGetHandleForAddressRange)(void *, CUdeviceptr, size_t, int,
						unsigned long long);
	CUresult (*GetErrorName)(CUresult, const char **);
} cu;

static int failures;
static const char *render_path;
static int skip_vk, skip_drm;

#define SIZE (4u << 20)

static void report(const char *verdict, const char *step, const char *fmt, ...)
	__attribute__((format(printf, 3, 4)));
static void report(const char *verdict, const char *step, const char *fmt, ...)
{
	va_list ap;

	printf("cuda-dmabuf: %s %s", verdict, step);
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

static const char *cuerr(CUresult r)
{
	const char *s = NULL;

	if (cu.GetErrorName && cu.GetErrorName(r, &s) == 0 && s)
		return s;
	return "?";
}

static int load_cuda(void)
{
	void *h = dlopen("libcuda.so.1", RTLD_NOW);

	if (!h) {
		report("FAIL", "dlopen", "%s", dlerror());
		return -1;
	}
#define SYM(field, name)                                                       \
	do {                                                                   \
		*(void **)&cu.field = dlsym(h, name);                          \
		if (!cu.field) {                                               \
			report("FAIL", "dlsym", "%s", name);                   \
			return -1;                                             \
		}                                                              \
	} while (0)
	SYM(Init, "cuInit");
	SYM(DeviceGet, "cuDeviceGet");
	SYM(DeviceGetAttribute, "cuDeviceGetAttribute");
	SYM(CtxCreate, "cuCtxCreate_v2");
	SYM(CtxDestroy, "cuCtxDestroy_v2");
	SYM(CtxSynchronize, "cuCtxSynchronize");
	SYM(MemAlloc, "cuMemAlloc_v2");
	SYM(MemFree, "cuMemFree_v2");
	SYM(MemcpyHtoD, "cuMemcpyHtoD_v2");
	SYM(MemcpyDtoH, "cuMemcpyDtoH_v2");
	SYM(MemGetHandleForAddressRange, "cuMemGetHandleForAddressRange");
	SYM(GetErrorName, "cuGetErrorName");
#undef SYM
	return 0;
}

static uint32_t pattern(uint32_t i, uint32_t round) { return 0x5eed0000u ^ (i * 2654435761u) ^ round; }

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
		if (qp[i].queueFlags & VK_QUEUE_TRANSFER_BIT ||
		    qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)
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

/* One command buffer: copy src to dst, or fill dst with `fill` when src is
 * NULL; submitted and waited for. */
static int vk_run(struct vk *v, VkBuffer src, VkBuffer dst, uint32_t fill)
{
	VkCommandBufferAllocateInfo cai = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
					   .commandPool = v->pool,
					   .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
					   .commandBufferCount = 1};
	VkCommandBufferBeginInfo bi = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO};
	VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
	VkBufferCopy copy = {.size = SIZE};
	VkCommandBuffer cb;
	VkFence fence;
	int ret = -1;

	if (vkAllocateCommandBuffers(v->dev, &cai, &cb) != VK_SUCCESS)
		return -1;
	vkBeginCommandBuffer(cb, &bi);
	if (src)
		vkCmdCopyBuffer(cb, src, dst, 1, &copy);
	else
		vkCmdFillBuffer(cb, dst, 0, SIZE, fill);
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
				  .size = SIZE,
				  .usage = VK_BUFFER_USAGE_TRANSFER_SRC_BIT |
					   VK_BUFFER_USAGE_TRANSFER_DST_BIT,
				  .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
	VkBuffer b = VK_NULL_HANDLE;

	vkCreateBuffer(v->dev, &bci, NULL, &b);
	return b;
}

static void vk_step(struct vk *v, int fd, CUdeviceptr d, uint32_t round)
{
	VkMemoryFdPropertiesKHR fp = {.sType = VK_STRUCTURE_TYPE_MEMORY_FD_PROPERTIES_KHR};
	VkBuffer ext = VK_NULL_HANDLE, host = VK_NULL_HANDLE;
	VkDeviceMemory emem = VK_NULL_HANDLE, hmem = VK_NULL_HANDLE;
	VkMemoryRequirements req, hreq;
	uint32_t *back = NULL, *p = NULL, i, bad = 0;
	int t, dupfd = -1;
	VkResult r;

	r = v->fd_props(v->dev, VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT, fd, &fp);
	if (r != VK_SUCCESS || !fp.memoryTypeBits) {
		report("FAIL", "vk", "vkGetMemoryFdPropertiesKHR: %d, memoryTypeBits %#x", r,
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
		report("FAIL", "vk", "vkAllocateMemory(import): %d", r);
		goto out;
	}
	dupfd = -1; /* Vulkan owns it now */
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
	if (vkMapMemory(v->dev, hmem, 0, SIZE, 0, (void **)&p) != VK_SUCCESS) {
		report("FAIL", "vk", "vkMapMemory");
		goto out;
	}

	/* CUDA wrote the pattern: Vulkan reads it. */
	if (vk_run(v, ext, host, 0)) {
		report("FAIL", "vk", "copy from the import");
		goto out;
	}
	for (i = 0; i < SIZE / 4; i++)
		bad += p[i] != pattern(i, round);
	if (bad) {
		report("FAIL", "vk", "read %u of %u words wrong (first: %#x, want %#x)", bad,
		       SIZE / 4, p[0], pattern(0, round));
		goto out;
	}
	/* Vulkan writes: CUDA reads it. */
	if (vk_run(v, VK_NULL_HANDLE, ext, 0xc0ffee00u ^ round)) {
		report("FAIL", "vk", "fill of the import");
		goto out;
	}
	back = malloc(SIZE);
	if (!back || cu.CtxSynchronize() || cu.MemcpyDtoH(back, d, SIZE)) {
		report("FAIL", "vk", "cuMemcpyDtoH after the fill");
		goto out;
	}
	for (i = 0; i < SIZE / 4; i++)
		bad += back[i] != (0xc0ffee00u ^ round);
	if (bad)
		report("FAIL", "vk", "CUDA read %u of %u words of the fill wrong", bad, SIZE / 4);
	else
		report("PASS", "vk", "Vulkan read CUDA's %u bytes, and CUDA Vulkan's", SIZE);
out:
	free(back);
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
	int loops = 1, expect_refused = 0, i, a, fd;
	uint32_t *host, round;
	struct vk v = {0};
	int have_vk = 0;
	CUdevice dev;
	CUcontext ctx;
	CUdeviceptr d;
	CUresult r;

	for (a = 1; a < argc; a++) {
		if (!strcmp(argv[a], "--loops") && a + 1 < argc)
			loops = atoi(argv[++a]);
		else if (!strcmp(argv[a], "--render") && a + 1 < argc)
			render_path = argv[++a];
		else if (!strcmp(argv[a], "--no-vk"))
			skip_vk = 1;
		else if (!strcmp(argv[a], "--no-drm"))
			skip_drm = 1;
		else if (!strcmp(argv[a], "--expect-refused"))
			expect_refused = 1;
		else {
			fprintf(stderr,
				"usage: %s [--loops N] [--render PATH] [--no-vk] [--no-drm] "
				"[--expect-refused]\n",
				argv[0]);
			return 2;
		}
	}
	if (load_cuda())
		return 1;
	if ((r = cu.Init(0)) || (r = cu.DeviceGet(&dev, 0)) || (r = cu.CtxCreate(&ctx, 0, dev))) {
		report("FAIL", "init", "%s", cuerr(r));
		return 1;
	}
	cu.DeviceGetAttribute(&a, CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED, dev);
	if (!a && !expect_refused) {
		/* CUDA's own choice, natively too (a GeForce card): nothing to
		 * test here. nvgpu-rm-dmabuf makes the same export without it. */
		report("SKIP", "attribute",
		       "CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED=0: CUDA offers no dma-buf "
		       "export on this device");
		cu.CtxDestroy(ctx);
		printf("cuda-dmabuf: SKIP (0 failed)\n");
		return 0;
	}
	report("PASS", "attribute", "CU_DEVICE_ATTRIBUTE_DMA_BUF_SUPPORTED=%d", a);
	host = malloc(SIZE);
	if (!host || (r = cu.MemAlloc(&d, SIZE))) {
		report("FAIL", "alloc", "%s", cuerr(r));
		return 1;
	}
	if (!skip_vk) {
		have_vk = !vk_init(&v);
		if (!have_vk)
			report("FAIL", "vk-init", "no Vulkan device with dma-buf import");
	}

	for (round = 0; round < (uint32_t)loops; round++) {
		struct stat st;
		char name[64];
		off_t end;
		void *m;

		for (i = 0; i < (int)(SIZE / 4); i++)
			host[i] = pattern((uint32_t)i, round);
		if ((r = cu.MemcpyHtoD(d, host, SIZE)) || (r = cu.CtxSynchronize())) {
			report("FAIL", "fill", "%s", cuerr(r));
			break;
		}
		fd = -1;
		r = cu.MemGetHandleForAddressRange(&fd, d, SIZE, CU_MEM_RANGE_HANDLE_TYPE_DMA_BUF_FD, 0);
		if (expect_refused) {
			if (r || fd < 0)
				report("PASS", "export", "refused: %s", cuerr(r));
			else
				report("FAIL", "export", "made fd %d with the export refused", fd);
			if (fd >= 0)
				close(fd);
			break;
		}
		if (r || fd < 0) {
			report("FAIL", "export", "cuMemGetHandleForAddressRange: %s", cuerr(r));
			break;
		}
		fdinfo_name(fd, name, sizeof(name));
		report("PASS", "export", "round %u: fd %d, exporter %s", round, fd, name);
		end = lseek(fd, 0, SEEK_END);
		if (fstat(fd, &st) || end != (off_t)SIZE)
			report("FAIL", "size", "lseek end %lld, want %u", (long long)end, SIZE);
		else
			report("PASS", "size", "%lld", (long long)end);
		m = mmap(NULL, SIZE, PROT_READ, MAP_SHARED, fd, 0);
		if (m == MAP_FAILED) {
			report("PASS", "mmap", "refused: %s", strerror(errno));
		} else {
			report("FAIL", "mmap", "a CPU mapping of GPU memory was made");
			munmap(m, SIZE);
		}
		if (!skip_drm)
			drm_step(fd);
		if (have_vk)
			vk_step(&v, fd, d, round);
		if (close(fd))
			report("FAIL", "close", "%s", strerror(errno));
	}

	if (have_vk) {
		vkDestroyCommandPool(v.dev, v.pool, NULL);
		vkDestroyDevice(v.dev, NULL);
		vkDestroyInstance(v.inst, NULL);
	}
	cu.MemFree(d);
	cu.CtxDestroy(ctx);
	free(host);
	printf("cuda-dmabuf: %s (%d failed)\n", failures ? "FAIL" : "PASS", failures);
	return failures;
}
