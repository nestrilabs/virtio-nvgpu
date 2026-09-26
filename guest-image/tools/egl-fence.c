// SPDX-License-Identifier: Apache-2.0
/*
 * egl-fence -- does EGL_ANDROID_native_fence_sync work? Creates a context on
 * the surfaceless platform (or the device platform), clears, flushes, makes a
 * native fence sync, exports its fd, waits on it, and says at which step it
 * failed. Chromium's GPU process (ANGLE) makes these for every frame.
 */
#define _GNU_SOURCE
#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#define CHECK(c, what) do { if (!(c)) { printf("egl-fence: FAIL %s (eglGetError 0x%x)\n", what, eglGetError()); return 1; } \
	printf("egl-fence: ok   %s\n", what); } while (0)

int main(void)
{
	PFNEGLGETPLATFORMDISPLAYEXTPROC getdpy = (void *)eglGetProcAddress("eglGetPlatformDisplayEXT");
	EGLDisplay d = getdpy ? getdpy(EGL_PLATFORM_SURFACELESS_MESA, EGL_DEFAULT_DISPLAY, NULL) : EGL_NO_DISPLAY;
	if (d == EGL_NO_DISPLAY)
		d = eglGetDisplay(EGL_DEFAULT_DISPLAY);
	CHECK(d != EGL_NO_DISPLAY, "display");
	EGLint maj, min;
	CHECK(eglInitialize(d, &maj, &min), "eglInitialize");
	const char *ext = eglQueryString(d, EGL_EXTENSIONS);
	printf("egl-fence: vendor %s, native_fence_sync %s\n", eglQueryString(d, EGL_VENDOR),
	       strstr(ext, "EGL_ANDROID_native_fence_sync") ? "advertised" : "absent");
	CHECK(eglBindAPI(EGL_OPENGL_ES_API), "bind GLES");
	EGLint cattr[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
	EGLContext c = eglCreateContext(d, EGL_NO_CONFIG_KHR, EGL_NO_CONTEXT, cattr);
	CHECK(c != EGL_NO_CONTEXT, "context");
	CHECK(eglMakeCurrent(d, EGL_NO_SURFACE, EGL_NO_SURFACE, c), "make current (surfaceless)");
	GLuint fb, tex;
	glGenTextures(1, &tex);
	glBindTexture(GL_TEXTURE_2D, tex);
	glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, 64, 64, 0, GL_RGBA, GL_UNSIGNED_BYTE, NULL);
	glGenFramebuffers(1, &fb);
	glBindFramebuffer(GL_FRAMEBUFFER, fb);
	glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
	glClearColor(0.2f, 0.4f, 0.6f, 1);
	glClear(GL_COLOR_BUFFER_BIT);

	PFNEGLCREATESYNCKHRPROC create = (void *)eglGetProcAddress("eglCreateSyncKHR");
	PFNEGLDUPNATIVEFENCEFDANDROIDPROC dup = (void *)eglGetProcAddress("eglDupNativeFenceFDANDROID");
	PFNEGLCLIENTWAITSYNCKHRPROC wait = (void *)eglGetProcAddress("eglClientWaitSyncKHR");
	CHECK(create && dup && wait, "entry points");
	EGLint sattr[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, EGL_NO_NATIVE_FENCE_FD_ANDROID, EGL_NONE};
	EGLSyncKHR s = create(d, EGL_SYNC_NATIVE_FENCE_ANDROID, sattr);
	CHECK(s != EGL_NO_SYNC_KHR, "eglCreateSync(NATIVE_FENCE_ANDROID)");
	glFlush();
	int fd = dup(d, s);
	CHECK(fd >= 0, "eglDupNativeFenceFDANDROID");
	struct pollfd p = {.fd = fd, .events = POLLIN};
	int r = poll(&p, 1, 2000);
	CHECK(r == 1, "sync_file signals within 2 s");
	CHECK(wait(d, s, EGL_SYNC_FLUSH_COMMANDS_BIT_KHR, 1000000000) == EGL_CONDITION_SATISFIED_KHR, "eglClientWaitSync");
	/* And the other direction: a sync from an existing fd. */
	EGLint iattr[] = {EGL_SYNC_NATIVE_FENCE_FD_ANDROID, dup(d, s), EGL_NONE};
	EGLSyncKHR s2 = create(d, EGL_SYNC_NATIVE_FENCE_ANDROID, iattr);
	CHECK(s2 != EGL_NO_SYNC_KHR, "eglCreateSync from an imported fd");
	printf("egl-fence: ALL PASS\n");
	close(fd);
	return 0;
}
