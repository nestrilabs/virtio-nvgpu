// SPDX-License-Identifier: Apache-2.0
/*
 * vk-acquire-display -- the VK_EXT_acquire_drm_display path, end to end.
 *
 * TESTING.md stage 5: "a minimal vkAcquireDrmDisplayEXT -> vkGetDrmDisplayEXT
 * -> vkCreateDisplayPlaneSurfaceKHR program does the same [as vkcube --wsi
 * display]; the acquire path is what is being checked." This is it:
 *
 *   vkGetDrmDisplayEXT(drm fd, connector)   the leased connector as a VkDisplayKHR
 *   vkAcquireDrmDisplayEXT(drm fd, display) nvidia-drm GRANT_PERMISSIONS(MODESET)
 *   display mode + plane -> VkSurfaceKHR -> swapchain (FIFO)
 *   N frames of vkCmdClearColorImage, a colour ramp, presented
 *   vkReleaseDisplayEXT
 *
 * The DRM fd: --fd N, or NVGPU_LEASE_FD (what nvgpu-lease hands a child), or
 * --device PATH (a guest card node in compositor-VM mode). The connector:
 * --connector ID, or NVGPU_LEASE_CONNECTOR_ID, or the first connected one the
 * fd shows.
 *
 * Prints "vk-acquire-display: PASS|FAIL <step>" lines; exit 0 only if every
 * frame was presented.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <xf86drm.h>
#include <xf86drmMode.h>

#include <vulkan/vulkan.h>

#define TAG "vk-acquire-display: "

static double now_s(void)
{
	struct timespec t;

	clock_gettime(CLOCK_MONOTONIC, &t);
	return t.tv_sec + t.tv_nsec / 1e9;
}

#define CHECK(what, call)                                                         \
	do {                                                                       \
		VkResult r_ = (call);                                              \
		if (r_ != VK_SUCCESS) {                                            \
			printf(TAG "FAIL %s: VkResult %d\n", what, r_);            \
			return 1;                                                  \
		}                                                                  \
	} while (0)

static uint32_t first_connected(int fd)
{
	drmModeRes *res = drmModeGetResources(fd);
	uint32_t id = 0;

	if (!res) {
		printf(TAG "FAIL drmModeGetResources on the DRM fd\n");
		return 0;
	}
	for (int i = 0; i < res->count_connectors && !id; i++) {
		drmModeConnector *c = drmModeGetConnector(fd, res->connectors[i]);

		if (c && c->connection == DRM_MODE_CONNECTED)
			id = c->connector_id;
		drmModeFreeConnector(c);
	}
	drmModeFreeResources(res);
	return id;
}

int main(int argc, char **argv)
{
	int fd = -1, frames = 300;
	uint32_t conn = 0;
	const char *e;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--fd") && i + 1 < argc)
			fd = atoi(argv[++i]);
		else if (!strcmp(argv[i], "--device") && i + 1 < argc) {
			fd = open(argv[++i], O_RDWR | O_CLOEXEC);
			if (fd < 0) {
				perror(argv[i]);
				return 1;
			}
		} else if (!strcmp(argv[i], "--connector") && i + 1 < argc)
			conn = (uint32_t)strtoul(argv[++i], NULL, 0);
		else if (!strcmp(argv[i], "--frames") && i + 1 < argc)
			frames = atoi(argv[++i]);
		else {
			fprintf(stderr, "usage: %s [--fd N | --device PATH] [--connector ID] [--frames N]\n",
				argv[0]);
			return 2;
		}
	}
	if (fd < 0 && (e = getenv("NVGPU_LEASE_FD")))
		fd = atoi(e);
	if (!conn && (e = getenv("NVGPU_LEASE_CONNECTOR_ID")))
		conn = (uint32_t)strtoul(e, NULL, 0);
	if (fd < 0) {
		printf(TAG "FAIL no DRM fd (--fd, --device or NVGPU_LEASE_FD)\n");
		return 2;
	}
	if (!conn)
		conn = first_connected(fd);
	if (!conn) {
		printf(TAG "FAIL no connected connector on the DRM fd\n");
		return 1;
	}
	printf(TAG "drm fd %d, connector %u\n", fd, conn);

	/* Instance. */
	const char *iexts[] = {
		VK_KHR_SURFACE_EXTENSION_NAME,
		VK_KHR_DISPLAY_EXTENSION_NAME,
		VK_EXT_DIRECT_MODE_DISPLAY_EXTENSION_NAME,
		VK_EXT_ACQUIRE_DRM_DISPLAY_EXTENSION_NAME,
		VK_KHR_GET_PHYSICAL_DEVICE_PROPERTIES_2_EXTENSION_NAME,
	};
	VkApplicationInfo app = {
		.sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.pApplicationName = "vk-acquire-display",
		.apiVersion = VK_API_VERSION_1_1,
	};
	VkInstanceCreateInfo ici = {
		.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
		.pApplicationInfo = &app,
		.enabledExtensionCount = sizeof iexts / sizeof *iexts,
		.ppEnabledExtensionNames = iexts,
	};
	VkInstance inst;
	CHECK("vkCreateInstance (KHR_display + EXT_acquire_drm_display)", vkCreateInstance(&ici, NULL, &inst));

	PFN_vkGetDrmDisplayEXT getDrmDisplay =
		(PFN_vkGetDrmDisplayEXT)vkGetInstanceProcAddr(inst, "vkGetDrmDisplayEXT");
	PFN_vkAcquireDrmDisplayEXT acquireDrmDisplay =
		(PFN_vkAcquireDrmDisplayEXT)vkGetInstanceProcAddr(inst, "vkAcquireDrmDisplayEXT");
	PFN_vkReleaseDisplayEXT releaseDisplay =
		(PFN_vkReleaseDisplayEXT)vkGetInstanceProcAddr(inst, "vkReleaseDisplayEXT");
	if (!getDrmDisplay || !acquireDrmDisplay || !releaseDisplay) {
		printf(TAG "FAIL instance lacks the acquire_drm_display entry points\n");
		return 1;
	}

	uint32_t npd = 8;
	VkPhysicalDevice pds[8], pd = VK_NULL_HANDLE;
	VkDisplayKHR display = VK_NULL_HANDLE;
	CHECK("vkEnumeratePhysicalDevices", vkEnumeratePhysicalDevices(inst, &npd, pds));
	for (uint32_t i = 0; i < npd && !pd; i++) {
		VkPhysicalDeviceProperties p;

		vkGetPhysicalDeviceProperties(pds[i], &p);
		VkResult r = getDrmDisplay(pds[i], fd, conn, &display);
		printf(TAG "vkGetDrmDisplayEXT on %s: %d\n", p.deviceName, r);
		if (r == VK_SUCCESS && display != VK_NULL_HANDLE)
			pd = pds[i];
	}
	if (!pd) {
		printf(TAG "FAIL vkGetDrmDisplayEXT: no physical device knows connector %u\n", conn);
		return 1;
	}
	printf(TAG "PASS vkGetDrmDisplayEXT\n");

	CHECK("vkAcquireDrmDisplayEXT", acquireDrmDisplay(pd, fd, display));
	printf(TAG "PASS vkAcquireDrmDisplayEXT\n");

	/* A mode: the first the display lists (NVIDIA puts the preferred first). */
	uint32_t nmodes = 0;
	CHECK("vkGetDisplayModePropertiesKHR", vkGetDisplayModePropertiesKHR(pd, display, &nmodes, NULL));
	if (!nmodes) {
		printf(TAG "FAIL the display lists no modes\n");
		return 1;
	}
	VkDisplayModePropertiesKHR *modes = calloc(nmodes, sizeof *modes);
	CHECK("vkGetDisplayModePropertiesKHR", vkGetDisplayModePropertiesKHR(pd, display, &nmodes, modes));
	VkDisplayModePropertiesKHR mode = modes[0];
	printf(TAG "mode %ux%u @ %.3f Hz (%u modes)\n", mode.parameters.visibleRegion.width,
	       mode.parameters.visibleRegion.height, mode.parameters.refreshRate / 1000.0, nmodes);

	/* A plane that can show this display. */
	uint32_t nplanes = 0, plane = UINT32_MAX;
	CHECK("vkGetPhysicalDeviceDisplayPlanePropertiesKHR",
	      vkGetPhysicalDeviceDisplayPlanePropertiesKHR(pd, &nplanes, NULL));
	VkDisplayPlanePropertiesKHR *planes = calloc(nplanes ? nplanes : 1, sizeof *planes);
	CHECK("vkGetPhysicalDeviceDisplayPlanePropertiesKHR",
	      vkGetPhysicalDeviceDisplayPlanePropertiesKHR(pd, &nplanes, planes));
	for (uint32_t i = 0; i < nplanes && plane == UINT32_MAX; i++) {
		uint32_t nd = 0;

		if (planes[i].currentDisplay != VK_NULL_HANDLE && planes[i].currentDisplay != display)
			continue;
		if (vkGetDisplayPlaneSupportedDisplaysKHR(pd, i, &nd, NULL) != VK_SUCCESS || !nd)
			continue;
		VkDisplayKHR *ds = calloc(nd, sizeof *ds);
		vkGetDisplayPlaneSupportedDisplaysKHR(pd, i, &nd, ds);
		for (uint32_t j = 0; j < nd; j++)
			if (ds[j] == display)
				plane = i;
		free(ds);
	}
	if (plane == UINT32_MAX) {
		printf(TAG "FAIL no plane supports the display (%u planes)\n", nplanes);
		return 1;
	}
	VkDisplayPlaneCapabilitiesKHR pcaps;
	CHECK("vkGetDisplayPlaneCapabilitiesKHR",
	      vkGetDisplayPlaneCapabilitiesKHR(pd, mode.displayMode, plane, &pcaps));
	VkDisplayPlaneAlphaFlagBitsKHR alpha = VK_DISPLAY_PLANE_ALPHA_OPAQUE_BIT_KHR;
	if (!(pcaps.supportedAlpha & alpha))
		alpha = (VkDisplayPlaneAlphaFlagBitsKHR)(pcaps.supportedAlpha & -pcaps.supportedAlpha);

	VkDisplaySurfaceCreateInfoKHR sci = {
		.sType = VK_STRUCTURE_TYPE_DISPLAY_SURFACE_CREATE_INFO_KHR,
		.displayMode = mode.displayMode,
		.planeIndex = plane,
		.planeStackIndex = planes[plane].currentStackIndex,
		.transform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
		.globalAlpha = 1.0f,
		.alphaMode = alpha,
		.imageExtent = mode.parameters.visibleRegion,
	};
	VkSurfaceKHR surf;
	CHECK("vkCreateDisplayPlaneSurfaceKHR", vkCreateDisplayPlaneSurfaceKHR(inst, &sci, NULL, &surf));
	printf(TAG "PASS vkCreateDisplayPlaneSurfaceKHR (plane %u)\n", plane);

	/* Device and queue. */
	uint32_t nq = 0, qf = UINT32_MAX;
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, NULL);
	VkQueueFamilyProperties *qp = calloc(nq, sizeof *qp);
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qp);
	for (uint32_t i = 0; i < nq && qf == UINT32_MAX; i++) {
		VkBool32 ok = VK_FALSE;

		vkGetPhysicalDeviceSurfaceSupportKHR(pd, i, surf, &ok);
		if (ok && (qp[i].queueFlags & VK_QUEUE_GRAPHICS_BIT))
			qf = i;
	}
	if (qf == UINT32_MAX) {
		printf(TAG "FAIL no graphics queue can present to the display surface\n");
		return 1;
	}
	float prio = 1.0f;
	VkDeviceQueueCreateInfo qci = {
		.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
		.queueFamilyIndex = qf,
		.queueCount = 1,
		.pQueuePriorities = &prio,
	};
	const char *dexts[] = {VK_KHR_SWAPCHAIN_EXTENSION_NAME};
	VkDeviceCreateInfo dci = {
		.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
		.queueCreateInfoCount = 1,
		.pQueueCreateInfos = &qci,
		.enabledExtensionCount = 1,
		.ppEnabledExtensionNames = dexts,
	};
	VkDevice dev;
	CHECK("vkCreateDevice", vkCreateDevice(pd, &dci, NULL, &dev));
	VkQueue q;
	vkGetDeviceQueue(dev, qf, 0, &q);

	/* Swapchain. */
	VkSurfaceCapabilitiesKHR caps;
	CHECK("vkGetPhysicalDeviceSurfaceCapabilitiesKHR",
	      vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pd, surf, &caps));
	uint32_t nf = 0;
	vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surf, &nf, NULL);
	VkSurfaceFormatKHR *fmts = calloc(nf ? nf : 1, sizeof *fmts);
	vkGetPhysicalDeviceSurfaceFormatsKHR(pd, surf, &nf, fmts);
	VkSurfaceFormatKHR fmt = fmts[0];
	for (uint32_t i = 0; i < nf; i++)
		if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM)
			fmt = fmts[i];
	if (!(caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_DST_BIT)) {
		printf(TAG "FAIL swapchain images cannot be transfer destinations\n");
		return 1;
	}
	uint32_t want = caps.minImageCount + 1;
	if (caps.maxImageCount && want > caps.maxImageCount)
		want = caps.maxImageCount;
	VkSwapchainCreateInfoKHR swci = {
		.sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
		.surface = surf,
		.minImageCount = want,
		.imageFormat = fmt.format,
		.imageColorSpace = fmt.colorSpace,
		.imageExtent = caps.currentExtent.width != UINT32_MAX ? caps.currentExtent
								       : mode.parameters.visibleRegion,
		.imageArrayLayers = 1,
		.imageUsage = VK_IMAGE_USAGE_TRANSFER_DST_BIT,
		.imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
		.preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
		.compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
		.presentMode = VK_PRESENT_MODE_FIFO_KHR,
		.clipped = VK_TRUE,
	};
	VkSwapchainKHR sc;
	CHECK("vkCreateSwapchainKHR", vkCreateSwapchainKHR(dev, &swci, NULL, &sc));
	uint32_t nimg = 0;
	vkGetSwapchainImagesKHR(dev, sc, &nimg, NULL);
	VkImage *imgs = calloc(nimg, sizeof *imgs);
	vkGetSwapchainImagesKHR(dev, sc, &nimg, imgs);
	printf(TAG "PASS vkCreateSwapchainKHR (%u images, format %d)\n", nimg, fmt.format);

	VkCommandPoolCreateInfo cpci = {
		.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
		.flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT,
		.queueFamilyIndex = qf,
	};
	VkCommandPool pool;
	CHECK("vkCreateCommandPool", vkCreateCommandPool(dev, &cpci, NULL, &pool));
	VkCommandBufferAllocateInfo cbai = {
		.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
		.commandPool = pool,
		.level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
		.commandBufferCount = 1,
	};
	VkCommandBuffer cb;
	CHECK("vkAllocateCommandBuffers", vkAllocateCommandBuffers(dev, &cbai, &cb));
	VkSemaphoreCreateInfo semi = {.sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO};
	VkSemaphore acq, *done = calloc(nimg, sizeof *done);
	CHECK("vkCreateSemaphore", vkCreateSemaphore(dev, &semi, NULL, &acq));
	for (uint32_t i = 0; i < nimg; i++)
		CHECK("vkCreateSemaphore", vkCreateSemaphore(dev, &semi, NULL, &done[i]));
	VkFenceCreateInfo fci = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
	VkFence fence;
	CHECK("vkCreateFence", vkCreateFence(dev, &fci, NULL, &fence));

	double t0 = now_s(), tfirst = 0;
	int shown = 0;
	for (int f = 0; f < frames; f++) {
		uint32_t idx;
		VkResult r = vkAcquireNextImageKHR(dev, sc, 3000000000ull, acq, VK_NULL_HANDLE, &idx);

		if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) {
			printf(TAG "FAIL vkAcquireNextImageKHR at frame %d: %d\n", f, r);
			break;
		}
		VkCommandBufferBeginInfo bi = {
			.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
			.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
		};
		VkImageSubresourceRange rng = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
		VkImageMemoryBarrier b1 = {
			.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
			.dstAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT,
			.oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
			.newLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
			.srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
			.dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
			.image = imgs[idx],
			.subresourceRange = rng,
		};
		VkImageMemoryBarrier b2 = b1;
		b2.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
		b2.dstAccessMask = 0;
		b2.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
		b2.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
		float t = (float)(f % 120) / 120.0f;
		VkClearColorValue col = {.float32 = {t, 0.3f, 1.0f - t, 1.0f}};

		vkResetCommandBuffer(cb, 0);
		vkBeginCommandBuffer(cb, &bi);
		vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT,
				     0, 0, NULL, 0, NULL, 1, &b1);
		vkCmdClearColorImage(cb, imgs[idx], VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL, &col, 1, &rng);
		vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
				     0, 0, NULL, 0, NULL, 1, &b2);
		vkEndCommandBuffer(cb);
		VkPipelineStageFlags ws = VK_PIPELINE_STAGE_TRANSFER_BIT;
		VkSubmitInfo si = {
			.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
			.waitSemaphoreCount = 1,
			.pWaitSemaphores = &acq,
			.pWaitDstStageMask = &ws,
			.commandBufferCount = 1,
			.pCommandBuffers = &cb,
			.signalSemaphoreCount = 1,
			.pSignalSemaphores = &done[idx],
		};
		if ((r = vkQueueSubmit(q, 1, &si, fence)) != VK_SUCCESS) {
			printf(TAG "FAIL vkQueueSubmit at frame %d: %d\n", f, r);
			break;
		}
		VkPresentInfoKHR pi = {
			.sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
			.waitSemaphoreCount = 1,
			.pWaitSemaphores = &done[idx],
			.swapchainCount = 1,
			.pSwapchains = &sc,
			.pImageIndices = &idx,
		};
		r = vkQueuePresentKHR(q, &pi);
		if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) {
			printf(TAG "FAIL vkQueuePresentKHR at frame %d: %d\n", f, r);
			break;
		}
		if ((r = vkWaitForFences(dev, 1, &fence, VK_TRUE, 3000000000ull)) != VK_SUCCESS) {
			printf(TAG "FAIL frame %d never completed within 3 s: %d\n", f, r);
			break;
		}
		vkResetFences(dev, 1, &fence);
		if (f == 0)
			tfirst = now_s();
		shown++;
	}
	double el = now_s() - tfirst;
	vkDeviceWaitIdle(dev);
	if (shown > 1)
		printf(TAG "%s presented %d/%d frames, %.2f Hz after the first (mode %.3f Hz)\n",
		       shown == frames ? "PASS" : "FAIL", shown, frames, (shown - 1) / el,
		       mode.parameters.refreshRate / 1000.0);
	else
		printf(TAG "FAIL presented %d/%d frames\n", shown, frames);
	(void)t0;

	vkDestroySwapchainKHR(dev, sc, NULL);
	vkDestroyDevice(dev, NULL);
	vkDestroySurfaceKHR(inst, surf, NULL);
	VkResult rr = releaseDisplay(pd, display);
	printf(TAG "%s vkReleaseDisplayEXT: %d\n", rr == VK_SUCCESS ? "PASS" : "FAIL", rr);
	vkDestroyInstance(inst, NULL);
	return shown == frames && rr == VK_SUCCESS ? 0 : 1;
}
