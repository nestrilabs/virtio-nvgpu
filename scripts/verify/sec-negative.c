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
 * T9 is the one positive control: a duplicate between two clients of one
 * process, which must be made (it FAILs when refused), so that T8's refusal of
 * the same duplicate across processes is known to be the backend's. T10
 * carries its own control (a device sharing this process's own client).
 * T8 and T10 need a guest module that says which process makes each call
 * (BCAP_PROC_ID, BCAP_PROC_EUID); without it both are refused too.
 *
 * Build:  see sec-negative.sh (gcc, libdrm headers, no other deps).
 */

#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
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
#define NV_ESC_REGISTER_FD 0xC9 /* NV_IOCTL_BASE + 1; nv_ioctl_register_fd_t {int ctl_fd;} */
#define NV_ESC_RM_DUP_OBJECT 0x34
#define NV_ESC_RM_SHARE 0x35

/* NVIDIA builds its escape numbers as _IOC(dir, 'F', nr, sizeof(params)). */
#define NV_IOWR(nr, size) _IOC(_IOC_READ | _IOC_WRITE, NV_IOCTL_MAGIC, (nr), (size))

/* Classes and functions named by the finding, from the SDK headers. */
#define NV01_ROOT 0x00000000u
#define NV01_MEMORY_SYSTEM_OS_DESCRIPTOR 0x00000071u
#define NVOS32_FUNCTION_ALLOC_OS_DESCRIPTOR 8

/* A device, and a VA space under it: the simplest object RM duplicates
 * (vaspaceapiCanCopy), with parameters that are all zeros by default. */
#define NV01_DEVICE_0 0x00000080u
#define FERMI_VASPACE_A 0x000090f1u
#define NV0080_ALLOC_PARAMETERS_SIZE 56
#define NV_VASPACE_ALLOCATION_PARAMETERS_SIZE 56

/* What RM answers a caller without the right (nvstatuscodes.h), and what the
 * backend answers for it when it refuses a share or a duplicate itself. */
#define NV_ERR_INSUFFICIENT_PERMISSIONS 0x1bu

/* rs_access.h: RS_SHARE_TYPE_ALL, RS_SHARE_ACTION_FLAG_COMPOSE, and the
 * DUP_OBJECT right as a mask bit. */
#define RS_SHARE_TYPE_ALL 1
#define RS_SHARE_ACTION_FLAG_COMPOSE 4
#define RS_ACCESS_DUP_OBJECT_BIT 1u

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

/* NVOS55_PARAMETERS (nvos.h): RM_DUP_OBJECT. */
typedef struct {
	uint32_t hClient;
	uint32_t hParent;
	uint32_t hObject;
	uint32_t hClientSrc;
	uint32_t hObjectSrc;
	uint32_t flags;
	uint32_t status;
} nvos55_t;

/* NVOS57_PARAMETERS (nvos.h): RM_SHARE, with its RS_SHARE_POLICY. */
typedef struct {
	uint32_t hClient;
	uint32_t hObject;
	uint32_t target;
	uint32_t accessMask;
	uint16_t type;
	uint8_t action;
	uint8_t _pad;
	uint32_t status;
} nvos57_t;

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

/* T5: ADDFB2 takes only GEM handles this file made. The backend re-homes each
 * handle from the guest file that owns it and IDENTIFYs it as NVKMS memory on
 * the target file before the host sees the ioctl. A dumb buffer made on this
 * file is NVKMS memory under nvidia-drm (nv_drm_dumb_create), so ADDFB2 of it
 * must work, as it does natively: the positive control. A handle this file
 * never made, and one it has just closed, must be refused. Needs a KMS/lease
 * file. */
static int addfb2_handle(int kms, uint32_t handle, uint32_t pitch, uint32_t *fb_id)
{
	struct drm_mode_fb_cmd2 fb = {0};
	fb.width = 64;
	fb.height = 64;
	fb.pixel_format = DRM_FORMAT_XRGB8888;
	fb.handles[0] = handle;
	fb.pitches[0] = pitch;
	int r = ioctl(kms, DRM_IOCTL_MODE_ADDFB2, &fb);
	*fb_id = fb.fb_id;
	return r;
}

static void t_addfb2_non_nvkms(int kms)
{
	if (kms < 0) {
		skip("ADDFB2 own dumb buffer", "no card/lease fd (--kms)");
		skip("ADDFB2 handle never made", "no card/lease fd (--kms)");
		skip("ADDFB2 closed handle", "no card/lease fd (--kms)");
		return;
	}
	struct drm_mode_create_dumb cd = {0};
	cd.width = 64;
	cd.height = 64;
	cd.bpp = 32;
	if (ioctl(kms, DRM_IOCTL_MODE_CREATE_DUMB, &cd) != 0) {
		skip("ADDFB2 own dumb buffer", "CREATE_DUMB unavailable here");
		return;
	}
	uint32_t fb_id = 0;
	if (addfb2_handle(kms, cd.handle, cd.pitch, &fb_id) == 0) {
		pass("ADDFB2 own dumb buffer", "allowed (positive control)");
		ioctl(kms, DRM_IOCTL_MODE_RMFB, &fb_id);
	} else {
		fail("ADDFB2 own dumb buffer", strerror(errno));
	}

	/* A handle number far past anything this file has made. */
	if (addfb2_handle(kms, cd.handle + 0x10000, cd.pitch, &fb_id) == 0) {
		fail("ADDFB2 handle never made", "accepted");
		ioctl(kms, DRM_IOCTL_MODE_RMFB, &fb_id);
	} else {
		pass("ADDFB2 handle never made", "refused");
	}

	struct drm_mode_destroy_dumb dd = {.handle = cd.handle};
	ioctl(kms, DRM_IOCTL_MODE_DESTROY_DUMB, &dd);
	if (addfb2_handle(kms, cd.handle, cd.pitch, &fb_id) == 0) {
		fail("ADDFB2 closed handle", "accepted");
		ioctl(kms, DRM_IOCTL_MODE_RMFB, &fb_id);
	} else {
		pass("ADDFB2 closed handle", "refused");
	}
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

/* Allocate `class` as `handle` under `parent` of `client`, with `size` bytes
 * of zeroed parameters; RM's status, or -1 for a failed ioctl. */
static int64_t alloc_object(int fd, uint32_t client, uint32_t parent,
			    uint32_t handle, uint32_t class, uint32_t size)
{
	uint8_t params[64] = {0};
	nvos64_t a = {0};
	a.hRoot = client;
	a.hObjectParent = parent;
	a.hObjectNew = handle;
	a.hClass = class;
	a.pAllocParms = (uint64_t)(uintptr_t)params;
	a.paramsSize = size;
	if (ioctl(fd, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(a)), &a) != 0)
		return -1;
	return a.status;
}

#define DEV_HANDLE 0xde700001u
#define VAS_HANDLE 0x7a500001u
#define DUP_HANDLE 0xd0b00001u

/* A client on `fd` with a device and a VA space under it: the VA space is what
 * T8 and T9 duplicate. The client's handle, or 0. */
static uint32_t client_with_vaspace(int fd)
{
	/* RM lets a client allocate a device only through a control file the
	 * GPU's file has been registered with (NV_ESC_REGISTER_FD, as NVIDIA's
	 * own userspace does first); otherwise NV_ERR_INSUFFICIENT_PERMISSIONS. */
	int gpu = open("/dev/nvidia0", O_RDWR | O_CLOEXEC);
	if (gpu < 0 || ioctl(gpu, NV_IOWR(NV_ESC_REGISTER_FD, sizeof(int)), &fd) != 0) {
		fprintf(stderr, "sec-negative: registering the control file with /dev/nvidia0: %s\n",
			strerror(errno));
		return 0;
	}
	uint32_t c = alloc_client(fd);
	if (!c)
		return 0;
	int64_t st = alloc_object(fd, c, c, DEV_HANDLE, NV01_DEVICE_0,
				  NV0080_ALLOC_PARAMETERS_SIZE);
	if (st != 0) {
		fprintf(stderr, "sec-negative: device alloc: status %#llx\n", (long long)st);
		return 0;
	}
	st = alloc_object(fd, c, DEV_HANDLE, VAS_HANDLE, FERMI_VASPACE_A,
			  NV_VASPACE_ALLOCATION_PARAMETERS_SIZE);
	if (st != 0) {
		fprintf(stderr, "sec-negative: VA space alloc: status %#llx\n", (long long)st);
		return 0;
	}
	return c;
}

/* Duplicate the VA space of `src` into `dst`'s device (`dst` made by
 * client_with_vaspace on `fd`); RM's status, or -1 for a failed ioctl. */
static int64_t dup_vaspace(int fd, uint32_t dst, uint32_t src, uint32_t as)
{
	nvos55_t d = {0};
	d.hClient = dst;
	d.hParent = DEV_HANDLE;
	d.hObject = as;
	d.hClientSrc = src;
	d.hObjectSrc = VAS_HANDLE;
	if (ioctl(fd, NV_IOWR(NV_ESC_RM_DUP_OBJECT, sizeof(d)), &d) != 0)
		return -1;
	return d.status;
}

/* T7: RM_SHARE of type ALL. RM would let every client on the host duplicate
 * the object -- other VMs' included, since RM sees them all as processes
 * like the backend; the backend refuses a share that reaches outside the VM
 * before RM sees it, with RM's own status for a caller without the right. */
static void t_share_all(int ctl, uint32_t client)
{
	if (!client) {
		skip("RM_SHARE type ALL", "no RM client (alloc failed)");
		return;
	}
	nvos57_t sh = {0};
	sh.hClient = client;
	sh.hObject = client;
	sh.accessMask = RS_ACCESS_DUP_OBJECT_BIT;
	sh.type = RS_SHARE_TYPE_ALL;
	sh.action = RS_SHARE_ACTION_FLAG_COMPOSE;
	int r = ioctl(ctl, NV_IOWR(NV_ESC_RM_SHARE, sizeof(sh)), &sh);
	if (r == 0 && sh.status == 0)
		fail("RM_SHARE type ALL", "accepted (shared with every host client)");
	else if (r == 0 && sh.status == NV_ERR_INSUFFICIENT_PERMISSIONS)
		pass("RM_SHARE type ALL", "refused (NV_ERR_INSUFFICIENT_PERMISSIONS)");
	else
		pass("RM_SHARE type ALL", "refused");
}

/* T9, the positive control for T8: one process, two files, a client on each.
 * RM lets a process duplicate between its own clients, and so must the
 * backend -- this is what every CUDA/GL/Vulkan interop inside one process
 * rests on. PASS when the duplicate is made; FAIL when it is refused. Returns
 * whether it was made, so T8's refusal can be told from RM failing the
 * duplicate for some other reason. */
static int t_dup_same_process(int ctl, uint32_t dst)
{
	if (!dst) {
		skip("DUP same process", "no client with a VA space here");
		return 0;
	}
	int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
	uint32_t src = fd >= 0 ? client_with_vaspace(fd) : 0;
	if (!src) {
		skip("DUP same process", "no second client with a VA space");
		if (fd >= 0)
			close(fd);
		return 0;
	}
	int64_t st = dup_vaspace(ctl, dst, src, DUP_HANDLE);
	int ok = st == 0;
	if (ok)
		pass("DUP same process", "allowed (positive control)");
	else if (st == NV_ERR_INSUFFICIENT_PERMISSIONS)
		fail("DUP same process", "refused: the backend keeps a process from its own objects");
	else {
		char how[64];
		snprintf(how, sizeof(how), "RM failed it (status 0x%llx)", (long long)st);
		fail("DUP same process", how);
	}
	close(fd);
	return ok;
}

/* T8: a forked child makes a client and a VA space on a file of its own; the
 * parent, knowing the handles, duplicates the child's VA space into its own
 * client. Natively RM refuses (the two clients' processes differ, the default
 * PID share policy); through the backend both clients are the backend's
 * process to RM, so the backend refuses it, knowing from the guest kernel
 * which guest process made each client. */
static void t_dup_other_process(int ctl, uint32_t dst, int control_ok)
{
	if (!dst) {
		skip("DUP other process", "no client with a VA space here");
		return;
	}
	int up[2], down[2];
	if (pipe(up) || pipe(down)) {
		skip("DUP other process", "pipe failed");
		return;
	}
	pid_t pid = fork();
	if (pid < 0) {
		skip("DUP other process", "fork failed");
		return;
	}
	if (pid == 0) {
		int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
		uint32_t c = fd >= 0 ? client_with_vaspace(fd) : 0;
		char go;
		if (write(up[1], &c, sizeof(c)) != sizeof(c))
			_exit(1);
		/* Keep the client alive until the parent has tried. */
		if (read(down[0], &go, 1) < 0)
			_exit(1);
		_exit(0);
	}
	uint32_t child = 0;
	if (read(up[0], &child, sizeof(child)) != sizeof(child))
		child = 0;
	if (!child) {
		skip("DUP other process", "the child could not make a VA space");
	} else {
		int64_t st = dup_vaspace(ctl, dst, child, DUP_HANDLE + 1);
		if (st == 0)
			fail("DUP other process", "accepted (another process's object)");
		else if (st == NV_ERR_INSUFFICIENT_PERMISSIONS)
			pass("DUP other process", "refused (NV_ERR_INSUFFICIENT_PERMISSIONS)");
		else if (!control_ok)
			skip("DUP other process",
			     "refused, but T9 could not duplicate either: inconclusive");
		else
			pass("DUP other process", "refused");
	}
	if (write(down[1], "x", 1) != 1)
		kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
	close(up[0]);
	close(up[1]);
	close(down[0]);
	close(down[1]);
}

/* A device under `client` whose VA space is `share`'s (NV0080's hClientShare,
 * at 4 in NV0080_ALLOC_PARAMETERS); RM's status, or -1 for a failed ioctl. */
static int64_t alloc_device_sharing(int fd, uint32_t client, uint32_t share)
{
	uint8_t params[NV0080_ALLOC_PARAMETERS_SIZE] = {0};
	memcpy(params + 4, &share, sizeof(share));
	nvos64_t a = {0};
	a.hRoot = client;
	a.hObjectParent = client;
	a.hObjectNew = DEV_HANDLE + 2;
	a.hClass = NV01_DEVICE_0;
	a.pAllocParms = (uint64_t)(uintptr_t)params;
	a.paramsSize = sizeof(params);
	if (ioctl(fd, NV_IOWR(NV_ESC_RM_ALLOC, sizeof(a)), &a) != 0)
		return -1;
	return a.status;
}

/* T10: a second client named in parameters, of another guest user. A child
 * drops to uid 65534 and makes a client; the parent makes a device that
 * shares that client's VA space. RM checks hClientShare with clientValidate:
 * natively the caller's file (the default, strict) or its euid -- and to RM
 * every guest process is the backend's. The backend holds the field to RM's
 * rule with the guest's processes and euids, and refuses it before RM sees it
 * (NV_ERR_INSUFFICIENT_PERMISSIONS; RM's own strict refusal would be
 * NV_ERR_INVALID_CLIENT). Control first: a device sharing a client of this
 * process's own, which the backend must not refuse. Needs root, to drop to
 * another uid. */
static void t_share_other_user(int ctl, uint32_t client, uint32_t mine)
{
	if (!client || !mine) {
		skip("second client of another user", "no clients here");
		return;
	}
	if (geteuid() != 0) {
		skip("second client of another user", "not root: cannot run a child as another uid");
		return;
	}
	/* The control is informational: the refusal below is told apart by its
	 * status, whatever RM makes of a shared VA space here. */
	int64_t st = alloc_device_sharing(ctl, client, mine);
	printf("  (control: a device sharing this process's own client: status 0x%llx%s)\n",
	       (long long)st, st == NV_ERR_INSUFFICIENT_PERMISSIONS ? ", the backend's refusal: wrong" : "");
	if (st == NV_ERR_INSUFFICIENT_PERMISSIONS)
		fail("second client of another user", "control refused: the backend keeps a process from its own client");
	int up[2], down[2];
	if (pipe(up) || pipe(down)) {
		skip("second client of another user", "pipe failed");
		return;
	}
	pid_t pid = fork();
	if (pid < 0) {
		skip("second client of another user", "fork failed");
		return;
	}
	if (pid == 0) {
		uint32_t c = 0;
		if (setresgid(65534, 65534, 65534) == 0 && setresuid(65534, 65534, 65534) == 0) {
			int fd = open("/dev/nvidiactl", O_RDWR | O_CLOEXEC);
			c = fd >= 0 ? alloc_client(fd) : 0;
		}
		char go;
		if (write(up[1], &c, sizeof(c)) != sizeof(c))
			_exit(1);
		if (read(down[0], &go, 1) < 0)
			_exit(1);
		_exit(0);
	}
	uint32_t theirs = 0;
	if (read(up[0], &theirs, sizeof(theirs)) != sizeof(theirs))
		theirs = 0;
	uint32_t fresh = theirs ? alloc_client(ctl) : 0;
	if (!theirs || !fresh) {
		skip("second client of another user", "the child (uid 65534) or a fresh client could not be made");
	} else {
		st = alloc_device_sharing(ctl, fresh, theirs);
		if (st == 0)
			fail("second client of another user", "accepted (another user's VA space)");
		else if (st == NV_ERR_INSUFFICIENT_PERMISSIONS)
			pass("second client of another user", "refused by the backend (NV_ERR_INSUFFICIENT_PERMISSIONS)");
		else {
			char how[112];
			snprintf(how, sizeof(how),
				 "refused by RM itself (0x%llx), so the backend let it through", (long long)st);
			fail("second client of another user", how);
		}
	}
	if (write(down[1], "x", 1) != 1)
		kill(pid, SIGKILL);
	waitpid(pid, NULL, 0);
	close(up[0]);
	close(up[1]);
	close(down[0]);
	close(down[1]);
}

/* T11: RM's import from an export descriptor (NV0000 OS_UNIX
 * IMPORT_OBJECTS_FROM_FD, 0x3d0c) naming a number this process has not
 * open, and a file that is not one of the device's. RM resolves the number
 * in the backend, which holds every guest process's control files, so a
 * number the guest driver did not translate would import another process's
 * exported memory into this client. It must fail with EBADF before RM is
 * asked: an ioctl that succeeds (whatever RM's status) reached the host. */
#define NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECTS_FROM_FD 0x3d0c

typedef struct {
	int32_t fd;
	uint32_t hParent;
	uint32_t objects[128];
	uint8_t objectTypes[128];
	uint16_t numObjects;
	uint16_t index;
} import_objects_params_t;

static void t_import_foreign_fd(int ctl, uint32_t client)
{
	if (!client) {
		skip("import from a foreign fd", "no RM client (alloc failed)");
		return;
	}
	int probe = -1;
	for (int n = 3; n < 256; n++)
		if (fcntl(n, F_GETFD) < 0) {
			probe = n;
			break;
		}
	int other = open("/dev/null", O_RDWR | O_CLOEXEC);
	int cands[2] = {probe, other};
	const char *what[2] = {"a number not open here", "a file not the device's"};
	for (int i = 0; i < 2; i++) {
		if (cands[i] < 0)
			continue;
		import_objects_params_t p = {0};
		p.fd = cands[i];
		p.hParent = client;
		p.numObjects = 1;
		nvos54_t c = {0};
		c.hClient = client;
		c.hObject = client;
		c.cmd = NV0000_CTRL_CMD_OS_UNIX_IMPORT_OBJECTS_FROM_FD;
		c.params = (uint64_t)(uintptr_t)&p;
		c.paramsSize = sizeof(p);
		int r = ioctl(ctl, NV_IOWR(NV_ESC_RM_CONTROL, sizeof(c)), &c);
		char how[96];
		snprintf(how, sizeof(how), "%s: %s", what[i],
			 r == 0 ? "reached RM" : strerror(errno));
		if (r == 0 || errno != EBADF)
			fail("import from a foreign fd", how);
		else
			pass("import from a foreign fd", how);
	}
	if (other >= 0)
		close(other);
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

	/* RM objects between clients: T7 on the client above; T8 and T9 on a
	 * second client of this process's with a device and a VA space. */
	t_share_all(ctl, client);
	uint32_t mine = client_with_vaspace(ctl);
	int control_ok = t_dup_same_process(ctl, mine);
	t_dup_other_process(ctl, mine, control_ok);
	/* T10: a second client named in parameters, of another guest user. */
	t_share_other_user(ctl, client, mine);
	/* T11: an export descriptor that is not this process's own file. */
	t_import_foreign_fd(ctl, client);

	printf("\n%d passed, %d failed, %d skipped\n", passes, fails, skips);
	if (fails)
		printf("A FAIL means a dangerous request was ACCEPTED. Capture the "
		       "backend log (RUST_LOG=debug) and host dmesg.\n");
	return fails;
}
