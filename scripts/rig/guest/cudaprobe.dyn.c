/*
 * Does CUDA work in here?
 *
 * This is the probe M6 was built for. `cuCtxCreate` registers about 2 MiB of
 * the caller's own memory with RM by CPU address -- the thing a guest could
 * not do until the guest driver pinned its pages and the backend stitched
 * them into a host address of its own. Everything after it is ordinary GPU
 * work: allocate device memory, copy a pattern in, copy it back, compare.
 *
 * A round trip through device memory is the real question. A context that
 * builds but a copy that comes back wrong would mean the registered memory is
 * not the memory the guest thinks it is, and nothing short of reading the
 * bytes back says otherwise.
 *
 * Dynamic, and dlopen rather than linked: it needs no CUDA toolkit on the
 * build box, only the driver's own libcuda, which the guest already has over
 * the share. The driver API's ABI is stable and versioned in the symbol names
 * (`_v2`), so declaring the dozen entry points here is safe and spares the
 * probe a header it would otherwise have to carry.
 *
 *   cc -O2 -o cudaprobe cudaprobe.dyn.c -ldl
 *
 * Prints one PASS/FAIL line per check and exits non-zero if any failed.
 */
#include <dlfcn.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef unsigned long long CUdeviceptr;

#define CUDA_SUCCESS 0

static CUresult (*cuInit)(unsigned int);
static CUresult (*cuDeviceGetCount)(int *);
static CUresult (*cuDeviceGet)(CUdevice *, int);
static CUresult (*cuDeviceGetName)(char *, int, CUdevice);
static CUresult (*cuDeviceTotalMem)(size_t *, CUdevice);
static CUresult (*cuCtxCreate)(CUcontext *, unsigned int, CUdevice);
static CUresult (*cuCtxDestroy)(CUcontext);
static CUresult (*cuMemAlloc)(CUdeviceptr *, size_t);
static CUresult (*cuMemFree)(CUdeviceptr);
static CUresult (*cuMemcpyHtoD)(CUdeviceptr, const void *, size_t);
static CUresult (*cuMemcpyDtoH)(void *, CUdeviceptr, size_t);
static CUresult (*cuGetErrorName)(CUresult, const char **);

static int failures;

static void check(int ok, const char *what, const char *fmt, ...) {
    va_list ap;
    printf("%-6s %s", ok ? "PASS" : "FAIL", what);
    if (fmt && *fmt) {
        va_start(ap, fmt);
        printf("  ");
        vprintf(fmt, ap);
        va_end(ap);
    }
    putchar('\n');
    if (!ok)
        failures++;
}

/* CUDA's own name for a result, so a failure says something searchable. */
static const char *cu(CUresult r) {
    const char *name = NULL;

    if (r == CUDA_SUCCESS)
        return "CUDA_SUCCESS";
    if (cuGetErrorName && cuGetErrorName(r, &name) == CUDA_SUCCESS && name)
        return name;
    return "?";
}

#define BUF_BYTES (1024 * 1024)
static unsigned char out_buf[BUF_BYTES];
static unsigned char in_buf[BUF_BYTES];

int main(void) {
    void *lib;
    CUdevice dev = 0;
    CUcontext ctx = NULL;
    CUdeviceptr mem = 0;
    char name[256] = "";
    size_t total = 0;
    int count = 0;
    CUresult r;
    size_t i, bad;

    lib = dlopen("libcuda.so.1", RTLD_NOW);
    if (!lib) {
        printf("FAIL   libcuda.so.1 loads  %s\n", dlerror());
        return 1;
    }
    printf("PASS   libcuda.so.1 loads\n");

#define BIND(sym, as)                                                                              \
    do {                                                                                           \
        *(void **)(&(sym)) = dlsym(lib, as);                                                       \
        if (!(sym)) {                                                                              \
            printf("FAIL   %s is there\n", as);                                                    \
            return 1;                                                                              \
        }                                                                                          \
    } while (0)

    BIND(cuInit, "cuInit");
    BIND(cuDeviceGetCount, "cuDeviceGetCount");
    BIND(cuDeviceGet, "cuDeviceGet");
    BIND(cuDeviceGetName, "cuDeviceGetName");
    BIND(cuDeviceTotalMem, "cuDeviceTotalMem_v2");
    BIND(cuCtxCreate, "cuCtxCreate_v2");
    BIND(cuCtxDestroy, "cuCtxDestroy_v2");
    BIND(cuMemAlloc, "cuMemAlloc_v2");
    BIND(cuMemFree, "cuMemFree_v2");
    BIND(cuMemcpyHtoD, "cuMemcpyHtoD_v2");
    BIND(cuMemcpyDtoH, "cuMemcpyDtoH_v2");
    /* Not fatal: it only makes the other lines readable. */
    *(void **)(&cuGetErrorName) = dlsym(lib, "cuGetErrorName");

    r = cuInit(0);
    check(r == CUDA_SUCCESS, "cuInit", "%s", cu(r));
    if (r != CUDA_SUCCESS)
        goto out;

    r = cuDeviceGetCount(&count);
    check(r == CUDA_SUCCESS && count > 0, "a device is there", "%s count=%d", cu(r), count);
    if (r != CUDA_SUCCESS || count < 1)
        goto out;

    r = cuDeviceGet(&dev, 0);
    if (r == CUDA_SUCCESS)
        r = cuDeviceGetName(name, sizeof name, dev);
    check(r == CUDA_SUCCESS && name[0], "the device has a name", "%s %s", cu(r), name);

    r = cuDeviceTotalMem(&total, dev);
    check(r == CUDA_SUCCESS && total > 0, "the device has memory", "%s %zu MiB", cu(r),
          total >> 20);

    /*
     * The one M6 exists for. CUDA registers its own memory with RM by CPU
     * address here, and before M6 this is where a guest stopped.
     */
    r = cuCtxCreate(&ctx, 0, dev);
    check(r == CUDA_SUCCESS, "cuCtxCreate", "%s", cu(r));
    if (r != CUDA_SUCCESS)
        goto out;

    r = cuMemAlloc(&mem, BUF_BYTES);
    check(r == CUDA_SUCCESS && mem, "device memory is allocated", "%s %d KiB", cu(r),
          BUF_BYTES / 1024);
    if (r != CUDA_SUCCESS)
        goto out;

    for (i = 0; i < BUF_BYTES; i++)
        out_buf[i] = (unsigned char)(i * 7 + (i >> 11));
    memset(in_buf, 0, sizeof in_buf);

    r = cuMemcpyHtoD(mem, out_buf, BUF_BYTES);
    check(r == CUDA_SUCCESS, "a copy to the device", "%s", cu(r));
    if (r != CUDA_SUCCESS)
        goto out;

    r = cuMemcpyDtoH(in_buf, mem, BUF_BYTES);
    check(r == CUDA_SUCCESS, "a copy back from the device", "%s", cu(r));
    if (r != CUDA_SUCCESS)
        goto out;

    for (i = 0, bad = 0; i < BUF_BYTES; i++)
        if (in_buf[i] != out_buf[i])
            bad++;
    check(bad == 0, "what came back is what went out", "%zu of %d byte(s) differ", bad,
          BUF_BYTES);

out:
    if (mem)
        cuMemFree(mem);
    if (ctx)
        cuCtxDestroy(ctx);
    printf("cudaprobe: %d failed\n", failures);
    return failures ? 1 : 0;
}
