/*
 * gl-then-vk -- a GL context, then a Vulkan device in the same process, then
 * GL again. Chromium's GPU process does exactly this when chrome://gpu asks
 * it for WebGPU (Dawn on Vulkan) adapter information, and in a guest its GL
 * context stopped working at that moment. Each step says ok or FAIL.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan.h>

static int gl_frame(const char *when)
{
	GLuint fb, tex;
	glGenTextures(1, &tex);
	glBindTexture(GL_TEXTURE_2D, tex);
	glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 256, 256, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
	GLenum st = glCheckFramebufferStatus(GL_FRAMEBUFFER);
	glClearColor(0.1f, 0.2f, 0.3f, 1);
	glClear(GL_COLOR_BUFFER_BIT);
	unsigned char px[4] = {0};
	glReadPixels(0, 0, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
	GLenum err = glGetError();
	int ok = st == GL_FRAMEBUFFER_COMPLETE && err == GL_NO_ERROR && px[2] > 60;
	printf("gl-then-vk: %s GL %s: fb status 0x%x, glGetError 0x%x, pixel %u,%u,%u\n", ok ? "ok  " : "FAIL",
	       when, st, err, px[0], px[1], px[2]);
	glDeleteFramebuffers(1, &fb);
	glDeleteTextures(1, &tex);
	return ok;
}

int main(void)
{
	PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	EGLDisplay d = getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL);
	eglInitialize(d, NULL, NULL);
	eglBindAPI(EGL_OPENGL_ES_API);
	EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 3, EGL_NONE};
	EGLContext c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
	eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c);
	int ok = gl_frame("before Vulkan");

	VkInstanceCreateInfo ici = {.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO};
	VkInstance inst;
	VkResult r = vkCreateInstance(&ici, NULL, &inst);
	printf("gl-then-vk: %s vkCreateInstance (%d)\n", r == VK_SUCCESS ? "ok  " : "FAIL", r);
	uint32_t n = 1;
	VkPhysicalDevice pd;
	vkEnumeratePhysicalDevices(inst, &n, &pd);
	uint32_t nq = 0;
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, NULL);
	VkQueueFamilyProperties qp[32];
	if (nq > 32) nq = 32;
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &nq, qp);
	/* Every family, every queue, as a device that wants everything would. */
	VkDeviceQueueCreateInfo q[32];
	float prio[64];
	for (int i = 0; i < 64; i++) prio[i] = 1.0f;
	for (uint32_t i = 0; i < nq; i++)
		q[i] = (VkDeviceQueueCreateInfo){.sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
			.queueFamilyIndex = i, .queueCount = qp[i].queueCount > 64 ? 64 : qp[i].queueCount,
			.pQueuePriorities = prio};
	VkDeviceCreateInfo dci = {.sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .queueCreateInfoCount = nq,
				  .pQueueCreateInfos = q};
	VkDevice dev;
	r = vkCreateDevice(pd, &dci, NULL, &dev);
	printf("gl-then-vk: %s vkCreateDevice with %u queue families (%d)\n", r == VK_SUCCESS ? "ok  " : "FAIL", nq, r);
	ok &= gl_frame("with a Vulkan device alive");
	if (r == VK_SUCCESS)
		vkDestroyDevice(dev, NULL);
	vkDestroyInstance(inst, NULL);
	ok &= gl_frame("after the Vulkan device is gone");
	printf("gl-then-vk: %s\n", ok ? "ALL PASS" : "FAILED");
	return !ok;
}
