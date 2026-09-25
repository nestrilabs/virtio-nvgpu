/*
 * cuda-smoke -- the CUDA driver API end to end, with no CUDA toolkit.
 *
 * cuInit, a context, a device allocation, a host->device->host round trip
 * compared byte for byte, and one kernel (PTX JIT-compiled by the driver, so
 * the GPFIFO submission and the completion wait are exercised too) whose
 * result is checked. libcuda is dlopen()ed, so the binary needs nothing at
 * build time but a C compiler; at run time it finds libcuda.so.1 through its
 * rpath, /run/opengl-driver/lib.
 *
 * The few driver-API prototypes are declared here: the driver API is a stable
 * C ABI and the versioned symbol names (_v2) are what cuda.h maps to.
 *
 * Prints one "cuda-smoke: PASS ..." / "cuda-smoke: FAIL ..." line per step;
 * exit status is the number of failed steps (a failed step stops the run).
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
typedef unsigned long long CUdeviceptr;
typedef struct CUmod_st *CUmodule;
typedef struct CUfunc_st *CUfunction;
typedef struct CUstream_st *CUstream;

#define CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR 75
#define CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR 76

static struct {
	CUresult (*Init)(unsigned);
	CUresult (*DriverGetVersion)(int *);
	CUresult (*DeviceGetCount)(int *);
	CUresult (*DeviceGet)(CUdevice *, int);
	CUresult (*DeviceGetName)(char *, int, CUdevice);
	CUresult (*DeviceTotalMem)(size_t *, CUdevice);
	CUresult (*DeviceGetAttribute)(int *, int, CUdevice);
	CUresult (*CtxCreate)(CUcontext *, unsigned, CUdevice);
	CUresult (*CtxDestroy)(CUcontext);
	CUresult (*CtxSynchronize)(void);
	CUresult (*MemAlloc)(CUdeviceptr *, size_t);
	CUresult (*MemFree)(CUdeviceptr);
	CUresult (*MemcpyHtoD)(CUdeviceptr, const void *, size_t);
	CUresult (*MemcpyDtoH)(void *, CUdeviceptr, size_t);
	CUresult (*ModuleLoadData)(CUmodule *, const void *);
	CUresult (*ModuleUnload)(CUmodule);
	CUresult (*ModuleGetFunction)(CUfunction *, CUmodule, const char *);
	CUresult (*LaunchKernel)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned,
				 unsigned, unsigned, CUstream, void **, void **);
	CUresult (*GetErrorName)(CUresult, const char **);
} cu;

/* y = 2x + 1, in place, for i < n. PTX 7.0 / sm_52: any driver of the last
 * several years JIT-compiles it for whatever the GPU is. */
static const char ptx[] =
	".version 7.0\n"
	".target sm_52\n"
	".address_size 64\n"
	".visible .entry twice_plus_one(.param .u64 p_buf, .param .u32 p_n)\n"
	"{\n"
	"  .reg .pred %p<2>;\n"
	"  .reg .b32 %r<9>;\n"
	"  .reg .b64 %rd<5>;\n"
	"  ld.param.u64 %rd1, [p_buf];\n"
	"  ld.param.u32 %r1, [p_n];\n"
	"  mov.u32 %r2, %ctaid.x;\n"
	"  mov.u32 %r3, %ntid.x;\n"
	"  mov.u32 %r4, %tid.x;\n"
	"  mad.lo.s32 %r5, %r2, %r3, %r4;\n"
	"  setp.ge.u32 %p1, %r5, %r1;\n"
	"  @%p1 bra DONE;\n"
	"  cvta.to.global.u64 %rd2, %rd1;\n"
	"  mul.wide.u32 %rd3, %r5, 4;\n"
	"  add.s64 %rd4, %rd2, %rd3;\n"
	"  ld.global.u32 %r6, [%rd4];\n"
	"  shl.b32 %r7, %r6, 1;\n"
	"  add.s32 %r8, %r7, 1;\n"
	"  st.global.u32 [%rd4], %r8;\n"
	"DONE:\n"
	"  ret;\n"
	"}\n";

static int fails;

static const char *errname(CUresult r)
{
	const char *s = NULL;

	if (cu.GetErrorName && cu.GetErrorName(r, &s) == 0 && s)
		return s;
	return "?";
}

#define STEP(what, call)                                                              \
	do {                                                                           \
		CUresult r_ = (call);                                                  \
		if (r_ != 0) {                                                         \
			printf("cuda-smoke: FAIL %s: %d (%s)\n", what, r_, errname(r_)); \
			fails++;                                                       \
			goto out;                                                      \
		}                                                                      \
	} while (0)

static void *sym(void *h, const char *a, const char *b)
{
	void *p = dlsym(h, a);

	if (!p && b)
		p = dlsym(h, b);
	return p;
}

int main(int argc, char **argv)
{
	size_t n = argc > 1 ? strtoul(argv[1], NULL, 0) : (16u << 20) / 4;
	void *h = dlopen("libcuda.so.1", RTLD_NOW);
	CUdevice dev;
	CUcontext ctx = NULL;
	CUdeviceptr d = 0;
	CUmodule mod = NULL;
	CUfunction fn;
	uint32_t *a = NULL, *b = NULL;
	int ver = 0, count = 0, major = 0, minor = 0;
	size_t mem = 0;
	char name[128] = "";

	if (!h) {
		printf("cuda-smoke: FAIL dlopen libcuda.so.1: %s\n", dlerror());
		return 1;
	}
	cu.Init = sym(h, "cuInit", NULL);
	cu.DriverGetVersion = sym(h, "cuDriverGetVersion", NULL);
	cu.DeviceGetCount = sym(h, "cuDeviceGetCount", NULL);
	cu.DeviceGet = sym(h, "cuDeviceGet", NULL);
	cu.DeviceGetName = sym(h, "cuDeviceGetName", NULL);
	cu.DeviceTotalMem = sym(h, "cuDeviceTotalMem_v2", "cuDeviceTotalMem");
	cu.DeviceGetAttribute = sym(h, "cuDeviceGetAttribute", NULL);
	cu.CtxCreate = sym(h, "cuCtxCreate_v2", "cuCtxCreate");
	cu.CtxDestroy = sym(h, "cuCtxDestroy_v2", "cuCtxDestroy");
	cu.CtxSynchronize = sym(h, "cuCtxSynchronize", NULL);
	cu.MemAlloc = sym(h, "cuMemAlloc_v2", "cuMemAlloc");
	cu.MemFree = sym(h, "cuMemFree_v2", "cuMemFree");
	cu.MemcpyHtoD = sym(h, "cuMemcpyHtoD_v2", "cuMemcpyHtoD");
	cu.MemcpyDtoH = sym(h, "cuMemcpyDtoH_v2", "cuMemcpyDtoH");
	cu.ModuleLoadData = sym(h, "cuModuleLoadData", NULL);
	cu.ModuleUnload = sym(h, "cuModuleUnload", NULL);
	cu.ModuleGetFunction = sym(h, "cuModuleGetFunction", NULL);
	cu.LaunchKernel = sym(h, "cuLaunchKernel", NULL);
	cu.GetErrorName = sym(h, "cuGetErrorName", NULL);
	for (void **p = (void **)&cu; p < (void **)(&cu + 1); p++)
		if (!*p) {
			printf("cuda-smoke: FAIL libcuda lacks symbol #%td\n", p - (void **)&cu);
			return 1;
		}

	STEP("cuInit", cu.Init(0));
	STEP("cuDriverGetVersion", cu.DriverGetVersion(&ver));
	STEP("cuDeviceGetCount", cu.DeviceGetCount(&count));
	printf("cuda-smoke: PASS cuInit (driver API %d.%d, %d device(s))\n", ver / 1000,
	       (ver % 1000) / 10, count);
	if (count < 1) {
		printf("cuda-smoke: FAIL no CUDA device\n");
		fails++;
		goto out;
	}
	STEP("cuDeviceGet", cu.DeviceGet(&dev, 0));
	STEP("cuDeviceGetName", cu.DeviceGetName(name, sizeof name, dev));
	STEP("cuDeviceTotalMem", cu.DeviceTotalMem(&mem, dev));
	STEP("cuDeviceGetAttribute", cu.DeviceGetAttribute(&major, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MAJOR, dev));
	STEP("cuDeviceGetAttribute", cu.DeviceGetAttribute(&minor, CU_DEVICE_ATTRIBUTE_COMPUTE_CAPABILITY_MINOR, dev));
	printf("cuda-smoke: PASS device 0: %s, sm_%d%d, %zu MiB\n", name, major, minor, mem >> 20);

	STEP("cuCtxCreate", cu.CtxCreate(&ctx, 0, dev));
	printf("cuda-smoke: PASS cuCtxCreate\n");

	a = malloc(n * 4);
	b = malloc(n * 4);
	if (!a || !b) {
		printf("cuda-smoke: FAIL host malloc\n");
		fails++;
		goto out;
	}
	for (size_t i = 0; i < n; i++)
		a[i] = (uint32_t)(i * 2654435761u);
	memset(b, 0, n * 4);
	STEP("cuMemAlloc", cu.MemAlloc(&d, n * 4));
	STEP("cuMemcpyHtoD", cu.MemcpyHtoD(d, a, n * 4));
	STEP("cuMemcpyDtoH", cu.MemcpyDtoH(b, d, n * 4));
	if (memcmp(a, b, n * 4) != 0) {
		printf("cuda-smoke: FAIL round trip of %zu bytes differs\n", n * 4);
		fails++;
		goto out;
	}
	printf("cuda-smoke: PASS HtoD/DtoH round trip, %zu bytes identical\n", n * 4);

	STEP("cuModuleLoadData (PTX JIT)", cu.ModuleLoadData(&mod, ptx));
	STEP("cuModuleGetFunction", cu.ModuleGetFunction(&fn, mod, "twice_plus_one"));
	{
		unsigned nn = (unsigned)n;
		void *params[] = {&d, &nn};
		unsigned blocks = (unsigned)((n + 255) / 256);

		STEP("cuLaunchKernel", cu.LaunchKernel(fn, blocks, 1, 1, 256, 1, 1, 0, NULL, params, NULL));
	}
	STEP("cuCtxSynchronize", cu.CtxSynchronize());
	STEP("cuMemcpyDtoH", cu.MemcpyDtoH(b, d, n * 4));
	for (size_t i = 0; i < n; i++)
		if (b[i] != a[i] * 2u + 1u) {
			printf("cuda-smoke: FAIL kernel result [%zu] = %#x, want %#x\n", i, b[i],
			       a[i] * 2u + 1u);
			fails++;
			goto out;
		}
	printf("cuda-smoke: PASS kernel launch, %zu results correct\n", n);

out:
	if (mod)
		cu.ModuleUnload(mod);
	if (d)
		cu.MemFree(d);
	if (ctx)
		cu.CtxDestroy(ctx);
	free(a);
	free(b);
	printf("cuda-smoke: %s\n", fails ? "FAILED" : "ALL PASS");
	return fails;
}
