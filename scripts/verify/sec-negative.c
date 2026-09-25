/*
 * sec-negative.c -- the guest half of the display-passthrough security tests.
 *
 * Every check here fires one ioctl a malicious guest would use to reach past
 * its VM, and asserts the backend turns it away. The point is not that the
 * guest gets an error -- an error is easy -- but that the *host* is never made
 * to act on the guest's request: no host-kernel out-of-bounds read, no foreign
 * client dup'd, no VMM pointer followed, no other tenant's pixels scanned out.
 * The strong proof of each refusal is a backend unit test that asserts the host
 * ioctl was never issued (verification NVK_VERIFICATION.md §5.1, security
 * FINDINGS.md); this program is the end-to-end confirmation that the same
 * refusal is wired all the way from a guest process. sec-negative.sh runs it
 * inside the guest and prints the guest dmesg tail after (the guest must
 * survive); the operator checks the host side -- host dmesg clean and the
 * backend still serving -- as TESTING.md §"Security negative tests" sets out.
 *
 * It uses only public UAPI: the NVIDIA escape numbers from
 * nvidia-driver/.../nv_escape.h and the NVOS parameter layouts from
 * nvidia-driver/.../nvos.h (both mirrored below so the program builds anywhere),
 * and libdrm's own drm.h / drm_mode.h. The nvidia-drm command numbers are the
 * repo's own (gen/schema/nvidia_drm.py: DRM_COMMAND_BASE + n).
 *
 * A test is PASS when the dangerous action is refused (the ioctl fails, or in
 * GETFB's case hands back a zero handle). It is FAIL only when the action is
 * accepted. SKIP means the mode this test needs was not offered (no card/lease
 * fd). Exit status is the number of FAILs, so the wrapper can gate on it.
 *
 * Build:  see sec-negative.sh (gcc, libdrm headers, no other deps).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <drm.h>
#include <drm_fourcc.h>
#include <drm_mode.h>

/* ---- NVIDIA escape ABI (nv-ioctl-numbers.h, nv_escape.h) ---------------- */

#define NV_IOCTL_MAGIC 'F'
#define NV_ESC_RM_ALLOC_MEMORY 0x27
#define NV_ESC_RM_CONTROL 0x2A
#define NV_ESC_RM_ALLOC 0x2B
#define NV_ESC_RM_VID_HEAP_CONTROL 0x4A

/* NVIDIA builds its escape numbers as _IOC(dir, 'F', nr, sizeof(params)). */
#define NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, (nr), (size))

/* Classes and functions named by the finding, from the SDK headers. */
#define NV01_ROOT 0x00000000u
#define NV01_MEMORY_SYSTEM_OS_DESCRIPTOR 0x00000071u
#define NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR 8

/* A non-privileged control that carries embedded pointers and runs on the
 * client root object, so it needs no device/subdevice to reach the embedded-
 * pointer copy path (NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION, 0x00000101). */
#define NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION 0x00000101u

/* NVOS64_PARAMETERS (nvos.h): the modern RM_ALLOC arg. */
typedef struct {
	uint32_t hRoot;
	uint32_t hObjectParent;
	uint32_t hObjectNew;
	uint32_t hClass;
	uint64_t pAllocParms;
	uint64_t pRightsRequested;
	uint32_t paramsSize;
	uint32_t flags;
	uint32_t status;
	uint32_t _pad;
} nvos64_t;

/* NVOS54_PARAMETERS (nvos.h): RM_CONTROL. */
typedef struct {
	uint32_t hClient;
	uint32_t hObject;
	uint32_t cmd;
	uint32_t flags;
	uint64_t params;
	uint32_t paramsSize;
	uint32_t status;
} nvos54_t;

/* NVOS32_PARAMETERS is a big union; we only need the header far enough to set
 * `function`, so the backend can refuse ALLOC_OS_DESCRIPTOR by function alone.
 * A 1 KiB tail covers every arm without spelling the union out. */
typedef struct {
	uint32_t hRoot;
	uint32_t hObjectParent;
	uint32_t function;
	uint32_t hVASpace;
	uint16_t ivcHeapNumber;
	uint16_t _pad;
	uint32_t status;
	uint64_t total;
	uint64_t free;
	uint8_t data[1024];
} nvos32_t;

/* NV0000_CTRL_SYSTEM_GET_BUILD_VERSION_PARAMS: three embedded char* buffers.
 * We hand the guest-chosen pointers a value the host must never follow. */
typedef struct {
	uint32_t sizeOfStrings;
	uint32_t _pad;
	uint64_t pDriverVersionBuffer;
	uint64_t pVersionBuffer;
	uint64_t pTitleBuffer;
	uint32_t changelistNumber;
	uint32_t officialChangelistNumber;
} build_version_params_t;

/* ---- nvidia-drm command numbers (gen/schema/nvidia_drm.py) -------------- */

#define DRM_COMMAND_BASE 0x40
#define DRM_NVIDIA_GRANT_PERMISSIONS (DRM_COMMAND_BASE + 0x12)
#define DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE (DRM_COMMAND_BASE + 0x14)
#define NV_DRM_PERMISSIONS_TYPE_SUB_OWNER 3

struct grant_permissions_params { /* drm_nvidia_grant_permissions_params */
	int32_t fd;
	uint32_t dpyId;
	uint32_t type;
};

struct semsurf_ctx_create_params { /* drm_nvidia_semsurf_fence_ctx_create_params */
	uint64_t index;
	uint64_t nvkms_params_ptr;
	uint64_t nvkms_params_size;
	uint32_t handle;
	uint32_t __pad;
};

#define DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS \
	DRM_IOWR(DRM_NVIDIA_GRANT_PERMISSIONS, struct grant_permissions_params)
#define DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE \
	DRM_IOWR(DRM_NVIDIA_SEMSURF_FENCE_CTX_CREATE, struct semsurf_ctx_create_params)

/* A guest virtual address the backend must never make the host follow. Picked
 * to be non-canonical-ish and certainly not a buffer we own. */
#define POISON_PTR 0xdeadbeef00000000ull

/* ---- test bookkeeping --------------------------------------------------- */

static int fails;
static int passes;
static int skips;

static void pass(const char *name, const char *how)
{
	printf("  PASS  %-28s %s\n", name, how);
	passes++;
}

static void fail(const char *name, const char *how)
{
	printf("  FAIL  %-28s %s\n", name, how);
	fails++;
}

static void skip(const char *name, const char *why)
{
	printf("  SKIP  %-28s %s\n", name, why);
	skips++;
}

/* Allocate an RM client (NV01_ROOT) on /dev/nvidiactl and return its handle,
 * or 0. hObjectNew is an in/out field: RM fills it with the assigned handle. */
static uint32_t alloc_client(int ctl)
{
	nvos64_t a = {0};
	a.hClass = NV01_ROOT;
	a.hObjectNew = 0;
	if (ioctl(ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(a)), &a) != 0)
		return 0;
	if (a.status != 0)
		return 0;
	return a.hObjectNew;
}

/* T1: OS-descriptor allocation. RM would pin memory named by a CPU address in
 * the *backend's* address space; the backend must refuse hClass 0x71 and the
 * VID_HEAP_CONTROL ALLOC_OS_DESCRIPTOR function outright. */
static void t_os_descriptor(int ctl)
{
	nvos64_t a = {0};
	a.hClass = NV01_MEMORY_SYSTEM_OS_DESCRIPTOR;
	a.pAllocParms = POISON_PTR;
	a.paramsSize = 64;
	int r = ioctl(ctl, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(a)), &a);
	if (r == 0 && a.status == 0)
		fail("os-descriptor RM_ALLOC 0x71", "accepted (host may pin VMM memory)");
	else
		pass("os-descriptor RM_ALLOC 0x71", "refused");

	nvos32_t h = {0};
	h.function = NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR;
	r = ioctl(ctl, NV_IOWR(NV_ESC_RM_VID_HEAP_CONTROL, sizeof(h)), &h);
	if (r == 0 && h.status == 0)
		fail("VID_HEAP ALLOC_OS_DESC", "accepted");
	else
		pass("VID_HEAP ALLOC_OS_DESC", "refused");
}

/* T2: an RM_CONTROL whose parameter block carries embedded pointers. The
 * backend must make itself the only authority for every NvP64 -- it relocates
 * each to a buffer of its own, or refuses -- so a guest-chosen pointer is never
 * the address RM copies to. We give it POISON_PTR; the host must not fault. */
static void t_raw_pointer_control(int ctl, uint32_t client)
{
	if (!client) {
		skip("raw-pointer RM_CONTROL", "no RM client (alloc failed)");
		return;
	}
	build_version_params_t p = {0};
	p.sizeOfStrings = 4096;
	p.pDriverVersionBuffer = POISON_PTR;
	p.pVersionBuffer = POISON_PTR + 0x1000;
	p.pTitleBuffer = POISON_PTR + 0x2000;

	nvos54_t c = {0};
	c.hClient = client;
	c.hObject = client; /* the client root answers this control */
	c.cmd = NV0000_CTRL_CMD_SYSTEM_GET_BUILD_VERSION;
	c.params = (uint64_t)(uintptr_t)&p; /* the outer pointer the guest driver relocates */
	c.paramsSize = sizeof(p);

	int r = ioctl(ctl, NV_IOWR(NV_ESC_RM_CONTROL, sizeof(c)), &c);
	/*
	 * PASS in either shape: the call is refused, or it succeeds because the
	 * backend relocated the embedded pointers to its own buffers -- in which
	 * case our POISON_PTR was overwritten before the host saw it, and our
	 * copy of p is left with the backend's pointer values, not ours. A FAIL
	 * is the host having copied to POISON_PTR, which we cannot see directly;
	 * the backend unit test asserts it, and here we assert only that we and
	 * the host survived (checked by the wrapper) and that our poison values
	 * did not reach the host unchanged as a successful deref.
	 */
	if (r == 0 && c.status == 0 &&
	    p.pDriverVersionBuffer == POISON_PTR &&
	    p.pVersionBuffer == POISON_PTR + 0x1000)
		fail("raw-pointer RM_CONTROL", "accepted with guest pointers intact");
	else
		pass("raw-pointer RM_CONTROL",
		     r == 0 ? "accepted, pointers relocated by backend" : "refused");
}

/* T3: SEMSURF_FENCE_CTX_CREATE (nvidia-drm 0x54) with an enormous index and a
 * poisoned nested params block. The host adds index*stride to a kernel mapping
 * with no bound (C-1); the backend must bound the index and refuse a client it
 * did not allocate, before the host ioctl. Runs on the render node. */
static void t_semsurf_huge_index(int render)
{
	if (render < 0) {
		skip("semsurf 0x54 huge index", "no render node");
		return;
	}
	struct semsurf_ctx_create_params p = {0};
	p.index = 0xffffffffffffffffull; /* index*stride overflows */
	p.nvkms_params_ptr = POISON_PTR;
	p.nvkms_params_size = 16;
	int r = ioctl(render, DRM_IOCTL_NVIDIA_SEMSURF_FENCE_CTX_CREATE, &p);
	if (r == 0)
		fail("semsurf 0x54 huge index", "accepted (host OOB read primitive)");
	else
		pass("semsurf 0x54 huge index", "refused");
}

/* T4: GRANT_PERMISSIONS with SUB_OWNER, which blanks every head and hands whole-
 * device NVKMS ownership over. The backend allows only type == MODESET. Needs a
 * KMS/lease file. */
static void t_grant_sub_owner(int kms)
{
	if (kms < 0) {
		skip("GRANT_PERMISSIONS SUB_OWNER", "no card/lease fd (--kms)");
		return;
	}
	struct grant_permissions_params g = {0};
	g.fd = -1;
	g.dpyId = 0;
	g.type = NV_DRM_PERMISSIONS_TYPE_SUB_OWNER;
	int r = ioctl(kms, DRM_IOCTL_NVIDIA_GRANT_PERMISSIONS, &g);
	if (r == 0)
		fail("GRANT_PERMISSIONS SUB_OWNER", "accepted (device-wide ownership)");
	else
		pass("GRANT_PERMISSIONS SUB_OWNER", "refused");
}

/* T5: ADDFB2 of a handle the host does not know as NVKMS memory. The backend
 * IDENTIFYs every scanout handle as NVKMS and refuses otherwise. We use a dumb
 * buffer, which is not an NVKMS surface on this path. Needs a KMS/lease file. */
static void t_addfb2_non_nvkms(int kms)
{
	if (kms < 0) {
		skip("ADDFB2 non-NVKMS handle", "no card/lease fd (--kms)");
		return;
	}
	struct drm_mode_create_dumb cd = {0};
	cd.width = 64;
	cd.height = 64;
	cd.bpp = 32;
	if (ioctl(kms, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) {
		skip("ADDFB2 non-NVKMS handle", "CREATE_DUMB unavailable here");
		return;
	}
	struct drm_mode_fb_cmd2 fb = {0};
	fb.width = 64;
	fb.height = 64;
	fb.pixel_format = DRM_FORMAT_XRGB8888;
	fb.handles[0] = cd.handle;
	fb.pitches[0] = cd.pitch;
	int r = ioctl(kms, DRM_IOCTL_MODE_ADDFB2, &fb);
	if (r == 0) {
		fail("ADDFB2 non-NVKMS handle", "accepted");
		struct drm_mode_fb_cmd2 rm = {0};
		rm.fb_id = fb.fb_id;
		ioctl(kms, DRM_IOCTL_MODE_RMFB, &fb.fb_id);
		(void)rm;
	} else {
		pass("ADDFB2 non-NVKMS handle", "refused");
	}
	struct drm_mode_destroy_dumb dd = {.handle = cd.handle};
	ioctl(kms, DRM_IOCTL_MODE_DESTROY_DUMB, &dd);
}

/* T6: GETFB of framebuffer ids this file did not create. The backend returns a
 * handle only for FBs the same file made, and zeroes the handle otherwise, so a
 * guest cannot obtain a GEM handle for another tenant's scanout buffer. Needs a
 * KMS/lease file. */
static void t_getfb_foreign(int kms)
{
	if (kms < 0) {
		skip("GETFB foreign fb", "no card/lease fd (--kms)");
		return;
	}
	int leaked = 0, probed = 0;
	for (uint32_t id = 1; id <= 64; id++) {
		struct drm_mode_fb_cmd r = {0};
		r.fb_id = id;
		if (ioctl(kms, DRM_IOCTL_MODE_GETFB, &r) != 0)
			continue; /* ENOENT: no such fb, fine */
		probed++;
		if (r.handle != 0) {
			leaked++;
			struct drm_gem_close gc = {.handle = r.handle};
			ioctl(kms, DRM_IOCTL_GEM_CLOSE, &gc);
		}
	}
	if (leaked)
		fail("GETFB foreign fb", "handed back a handle for a foreign fb");
	else
		pass("GETFB foreign fb", probed ? "existing fbs gave handle 0"
					       : "no foreign fbs visible");
}

static int open_first(const char *const *paths)
{
	for (; *paths; paths++) {
		int fd = open(*paths, O_RDWR | O_CLOEXEC);
		if (fd >= 0) {
			fprintf(stderr, "  (using %s)\n", *paths);
			return fd;
		}
	}
	return -1;
}

static void usage(const char *me)
{
	fprintf(stderr,
		"usage: %s [--ctl PATH] [--render PATH] [--kms PATH] [--kms-fd N]\n"
		"  --ctl     /dev/nvidiactl (default)\n"
		"  --render  the nvidia render node (default: first of renderD128..131)\n"
		"  --kms     a guest card or adopted lease DRM file (enables the KMS tests)\n"
		"  --kms-fd  an already-open lease fd number, e.g. inherited from a lease client\n",
		me);
}

int main(int argc, char **argv)
{
	const char *ctl_path = "/dev/nvidiactl";
	const char *render_path = NULL;
	const char *kms_path = NULL;
	int kms_fd = -1;

	for (int i = 1; i < argc; i++) {
		if (!strcmp(argv[i], "--ctl") && i + 1 < argc)
			ctl_path = argv[++i];
		else if (!strcmp(argv[i], "--render") && i + 1 < argc)
			render_path = argv[++i];
		else if (!strcmp(argv[i], "--kms") && i + 1 < argc)
			kms_path = argv[++i];
		else if (!strcmp(argv[i], "--kms-fd") && i + 1 < argc)
			kms_fd = atoi(argv[++i]);
		else {
			usage(argv[0]);
			return 2;
		}
	}

	int ctl = open(ctl_path, O_RDWR | O_CLOEXEC);
	if (ctl < 0) {
		fprintf(stderr, "cannot open %s: %s\n", ctl_path, strerror(errno));
		return 2;
	}

	int render;
	if (render_path)
		render = open(render_path, O_RDWR | O_CLOEXEC);
	else {
		static const char *cands[] = {
			"/dev/dri/renderD128", "/dev/dri/renderD129",
			"/dev/dri/renderD130", "/dev/dri/renderD131", NULL};
		render = open_first(cands);
	}

	int kms = kms_fd;
	if (kms < 0 && kms_path)
		kms = open(kms_path, O_RDWR | O_CLOEXEC);

	printf("display-passthrough security negative tests\n");
	printf("ctl=%s render=%s kms=%s\n", ctl_path,
	       render >= 0 ? "yes" : "no",
	       kms >= 0 ? "yes" : "no");

	uint32_t client = alloc_client(ctl);

	t_os_descriptor(ctl);
	t_raw_pointer_control(ctl, client);
	t_semsurf_huge_index(render);
	t_grant_sub_owner(kms);
	t_addfb2_non_nvkms(kms);
	t_getfb_foreign(kms);

	printf("\n%d passed, %d failed, %d skipped\n", passes, fails, skips);
	if (fails)
		printf("A FAIL means a dangerous request was ACCEPTED. Capture the "
		       "backend log (RUST_LOG=debug) and host dmesg.\n");
	return fails;
}
