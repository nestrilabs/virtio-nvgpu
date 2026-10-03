/*
 * Is a guest held to its video memory limit?
 *
 * Three questions, and the limit (VRAM_LIMIT_MIB, 0 for none) says what each
 * answer must be:
 *   - what CUDA says the device holds, and how much of it is free;
 *   - how much can actually be allocated, 64 MiB at a time, until refused;
 *   - what the refusal was.
 * With a limit, the total must be the limit, and allocation must stop at it
 * with CUDA_ERROR_OUT_OF_MEMORY -- not later, and not much earlier: the
 * context and RM's own buffers take some, which is the margin below.
 *
 *   cc -O2 -o vramprobe vramprobe.dyn.c -ldl
 */
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef unsigned long long CUdeviceptr;

static CUresult (*cuInit)(unsigned int);
static CUresult (*cuDeviceGet)(CUdevice *, int);
static CUresult (*cuDeviceTotalMem)(size_t *, CUdevice);
static CUresult (*cuCtxCreate)(CUcontext *, unsigned int, CUdevice);
static CUresult (*cuMemGetInfo)(size_t *, size_t *);
static CUresult (*cuMemAlloc)(CUdeviceptr *, size_t);
static CUresult (*cuGetErrorName)(CUresult, const char **);

#define STEP (64ull << 20)
#define MAX_STEPS 4096
/* What a context and RM's own buffers may take out of the limit. */
#define MARGIN_MIB 384

static int failures;

static const char *cu(CUresult r) {
    const char *n = NULL;
    if (!r)
        return "CUDA_SUCCESS";
    return cuGetErrorName && !cuGetErrorName(r, &n) && n ? n : "?";
}

static void check(int ok, const char *what, const char *detail) {
    printf("%-6s %s  %s\n", ok ? "PASS" : "FAIL", what, detail);
    if (!ok)
        failures++;
}

int main(void) {
    const char *env = getenv("VRAM_LIMIT_MIB");
    unsigned long long limit = env ? strtoull(env, NULL, 10) : 0;
    void *lib = dlopen("libcuda.so.1", RTLD_NOW);
    CUdevice dev;
    CUcontext ctx;
    size_t total = 0, free_b = 0, total2 = 0;
    static CUdeviceptr ptrs[MAX_STEPS];
    unsigned long long got = 0;
    CUresult r = 0;
    char line[256];
    int i;

    if (!lib) {
        printf("FAIL   libcuda.so.1 loads  %s\n", dlerror());
        return 1;
    }
#define BIND(s, n)                                                                                 \
    if (!(*(void **)(&s) = dlsym(lib, n))) {                                                       \
        printf("FAIL   %s is there\n", n);                                                         \
        return 1;                                                                                  \
    }
    BIND(cuInit, "cuInit");
    BIND(cuDeviceGet, "cuDeviceGet");
    BIND(cuDeviceTotalMem, "cuDeviceTotalMem_v2");
    BIND(cuCtxCreate, "cuCtxCreate_v2");
    BIND(cuMemGetInfo, "cuMemGetInfo_v2");
    BIND(cuMemAlloc, "cuMemAlloc_v2");
    *(void **)(&cuGetErrorName) = dlsym(lib, "cuGetErrorName");

    if ((r = cuInit(0)) || (r = cuDeviceGet(&dev, 0)) || (r = cuDeviceTotalMem(&total, dev)) ||
        (r = cuCtxCreate(&ctx, 0, dev)) || (r = cuMemGetInfo(&free_b, &total2))) {
        printf("FAIL   CUDA up  %s\n", cu(r));
        return 1;
    }
    printf("INFO   limit %llu MiB; device %zu MiB; context sees %zu free of %zu MiB\n", limit,
           total >> 20, free_b >> 20, total2 >> 20);

    for (i = 0; i < MAX_STEPS; i++) {
        r = cuMemAlloc(&ptrs[i], STEP);
        if (r)
            break;
        got += STEP;
    }
    snprintf(line, sizeof line, "%llu MiB, then %s", got >> 20, cu(r));
    printf("INFO   allocated %s\n", line);

    if (!limit) {
        check(r != 0, "allocation stops somewhere", line);
        return failures != 0;
    }
    snprintf(line, sizeof line, "%zu MiB against %llu", total >> 20, limit);
    check((total >> 20) <= limit, "the device reports at most the limit", line);
    snprintf(line, sizeof line, "%zu MiB free against %llu", free_b >> 20, limit);
    check((free_b >> 20) <= limit, "free is at most the limit", line);
    snprintf(line, sizeof line, "%llu MiB against %llu", got >> 20, limit);
    check((got >> 20) <= limit, "allocation stops at the limit", line);
    check((got >> 20) + MARGIN_MIB >= limit, "and not far short of it", line);
    check(r == 2 /* CUDA_ERROR_OUT_OF_MEMORY */, "the refusal is out of memory", cu(r));
    return failures != 0;
}
