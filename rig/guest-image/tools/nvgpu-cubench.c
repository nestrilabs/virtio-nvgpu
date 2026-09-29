// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-cubench -- CUDA driver-API microbenchmarks, native against a guest
 * (--allow-compute). libcuda is dlopen()ed as in cuda-smoke, and the one
 * kernel is PTX the driver JIT-compiles, so nothing but a C compiler is
 * needed to build it.
 *
 *   nvgpu-cubench [test...]     (default: all)
 *
 *   init      cuInit, cuCtxCreate, cuCtxDestroy, and a second context (ms)
 *   memcpy    256 MiB host<->device, pageable (malloc) and pinned
 *             (cuMemAllocHost), and device to device (GB/s)
 *   hostreg   cuMemHostRegister + cuMemHostUnregister of 64 MiB (ms)
 *   launch    an empty kernel: launch + cuCtxSynchronize latency (us) with
 *             the default (spin) and blocking-sync schedules; launches per
 *             second, synchronised once
 *   malloc    cuMemAlloc + cuMemFree, 1 MiB and 64 MiB (us)
 *
 * Output: "BENCH cu-<test>.<figure> <value> <unit>" lines.
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
typedef unsigned long long CUdeviceptr;
typedef struct CUmod_st *CUmodule;
typedef struct CUfunc_st *CUfunction;
typedef struct CUstream_st *CUstream;

#define CU_CTX_SCHED_BLOCKING_SYNC 0x04

static struct {
  CUresult (*Init)(unsigned);
  CUresult (*DeviceGet)(CUdevice *, int);
  CUresult (*CtxCreate)(CUcontext *, unsigned, CUdevice);
  CUresult (*CtxDestroy)(CUcontext);
  CUresult (*CtxSynchronize)(void);
  CUresult (*MemAlloc)(CUdeviceptr *, size_t);
  CUresult (*MemFree)(CUdeviceptr);
  CUresult (*MemAllocHost)(void **, size_t);
  CUresult (*MemFreeHost)(void *);
  CUresult (*MemHostRegister)(void *, size_t, unsigned);
  CUresult (*MemHostUnregister)(void *);
  CUresult (*MemcpyHtoD)(CUdeviceptr, const void *, size_t);
  CUresult (*MemcpyDtoH)(void *, CUdeviceptr, size_t);
  CUresult (*MemcpyDtoD)(CUdeviceptr, CUdeviceptr, size_t);
  CUresult (*ModuleLoadData)(CUmodule *, const void *);
  CUresult (*ModuleGetFunction)(CUfunction *, CUmodule, const char *);
  CUresult (*LaunchKernel)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned,
                           unsigned, unsigned, CUstream, void **, void **);
} cu;

static const char ptx[] = ".version 7.0\n"
                          ".target sm_52\n"
                          ".address_size 64\n"
                          ".visible .entry nothing()\n"
                          "{\n"
                          "  ret;\n"
                          "}\n";

static double now_s(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec * 1e-9;
}

static void bench(const char *test, const char *fig, double v, const char *unit) {
  printf("BENCH cu-%s.%s %.4g %s\n", test, fig, v, unit);
  fflush(stdout);
}

static int cmp_d(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return x < y ? -1 : x > y;
}

static void stats(const char *test, const char *fig, double *v, int n, double scale,
                  const char *unit) {
  double sum = 0;
  char name[96];

  qsort(v, n, sizeof(*v), cmp_d);
  for (int i = 0; i < n; i++)
    sum += v[i];
  snprintf(name, sizeof(name), "%s_mean", fig);
  bench(test, name, sum / n * scale, unit);
  snprintf(name, sizeof(name), "%s_p50", fig);
  bench(test, name, v[n / 2] * scale, unit);
  snprintf(name, sizeof(name), "%s_p99", fig);
  bench(test, name, v[(int)(n * 0.99)] * scale, unit);
}

#define CK(x)                                                                   \
  do {                                                                          \
    CUresult r_ = (x);                                                          \
    if (r_) {                                                                   \
      fprintf(stderr, "nvgpu-cubench: %s = %d (line %d)\n", #x, r_, __LINE__); \
      return -1;                                                                \
    }                                                                           \
  } while (0)

static CUdevice dev;
static CUcontext ctx;

static int t_init(void) {
  double t0 = now_s();
  CK(cu.Init(0));
  double t1 = now_s();
  CK(cu.DeviceGet(&dev, 0));
  CK(cu.CtxCreate(&ctx, 0, dev));
  double t2 = now_s();
  CK(cu.CtxDestroy(ctx));
  double t3 = now_s();
  CK(cu.CtxCreate(&ctx, 0, dev));
  double t4 = now_s();
  bench("init", "cuInit", (t1 - t0) * 1e3, "ms");
  bench("init", "ctx_create_first", (t2 - t1) * 1e3, "ms");
  bench("init", "ctx_destroy", (t3 - t2) * 1e3, "ms");
  bench("init", "ctx_create_second", (t4 - t3) * 1e3, "ms");
  return 0;
}

static int t_memcpy(void) {
  const size_t SZ = 256u << 20;
  enum { REPS = 8 };
  CUdeviceptr d, d2;
  void *pinned;
  char *pageable = aligned_alloc(4096, SZ);

  memset(pageable, 0x5a, SZ);
  CK(cu.MemAlloc(&d, SZ));
  CK(cu.MemAlloc(&d2, SZ));
  CK(cu.MemAllocHost(&pinned, SZ));
  memset(pinned, 0x5a, SZ);
  struct {
    const char *name;
    void *host;
  } kinds[] = {{"pageable", pageable}, {"pinned", pinned}};
  for (int k = 0; k < 2; k++) {
    char fig[64];
    CK(cu.MemcpyHtoD(d, kinds[k].host, SZ));
    double t0 = now_s();
    for (int r = 0; r < REPS; r++)
      CK(cu.MemcpyHtoD(d, kinds[k].host, SZ));
    snprintf(fig, sizeof(fig), "h2d_%s", kinds[k].name);
    bench("memcpy", fig, (double)SZ * REPS / (now_s() - t0) / 1e9, "GB/s");
    CK(cu.MemcpyDtoH(kinds[k].host, d, SZ));
    t0 = now_s();
    for (int r = 0; r < REPS; r++)
      CK(cu.MemcpyDtoH(kinds[k].host, d, SZ));
    snprintf(fig, sizeof(fig), "d2h_%s", kinds[k].name);
    bench("memcpy", fig, (double)SZ * REPS / (now_s() - t0) / 1e9, "GB/s");
  }
  CK(cu.MemcpyDtoD(d2, d, SZ));
  CK(cu.CtxSynchronize());
  double t0 = now_s();
  for (int r = 0; r < REPS; r++)
    CK(cu.MemcpyDtoD(d2, d, SZ));
  CK(cu.CtxSynchronize());
  bench("memcpy", "d2d", (double)SZ * REPS / (now_s() - t0) / 1e9, "GB/s");
  CK(cu.MemFreeHost(pinned));
  CK(cu.MemFree(d));
  CK(cu.MemFree(d2));
  free(pageable);
  return 0;
}

static int t_hostreg(void) {
  const size_t SZ = 64u << 20;
  enum { N = 20 };
  static double lat[N], ulat[N];
  char *p = aligned_alloc(4096, SZ);
  CUdeviceptr d;

  memset(p, 1, SZ);
  CK(cu.MemAlloc(&d, SZ));
  for (int i = 0; i < N; i++) {
    double t0 = now_s();
    CK(cu.MemHostRegister(p, SZ, 0));
    double t1 = now_s();
    if (i == 0) {
      /* the registered pages as a copy source: pinned speed or not */
      double t2 = now_s();
      for (int r = 0; r < 4; r++)
        CK(cu.MemcpyHtoD(d, p, SZ));
      bench("hostreg", "h2d_registered", (double)SZ * 4 / (now_s() - t2) / 1e9, "GB/s");
    }
    double t3 = now_s();
    CK(cu.MemHostUnregister(p));
    lat[i] = t1 - t0;
    ulat[i] = now_s() - t3;
  }
  stats("hostreg", "register_64m", lat, N, 1e3, "ms");
  stats("hostreg", "unregister_64m", ulat, N, 1e3, "ms");
  CK(cu.MemFree(d));
  free(p);
  return 0;
}

static int launch_lat(const char *fig, CUfunction fn) {
  enum { WARM = 200, N = 5000, RATE = 50000 };
  static double lat[N];

  for (int i = 0; i < WARM + N; i++) {
    double t0 = now_s();
    CK(cu.LaunchKernel(fn, 1, 1, 1, 32, 1, 1, 0, NULL, NULL, NULL));
    CK(cu.CtxSynchronize());
    if (i >= WARM)
      lat[i - WARM] = now_s() - t0;
  }
  stats("launch", fig, lat, N, 1e6, "us");
  double t0 = now_s();
  for (int i = 0; i < RATE; i++)
    CK(cu.LaunchKernel(fn, 1, 1, 1, 32, 1, 1, 0, NULL, NULL, NULL));
  CK(cu.CtxSynchronize());
  char r[64];
  snprintf(r, sizeof(r), "%s_rate", fig);
  bench("launch", r, RATE / (now_s() - t0), "launches/s");
  return 0;
}

static int t_launch(void) {
  CUmodule mod;
  CUfunction fn;
  CUcontext b;

  CK(cu.ModuleLoadData(&mod, ptx));
  CK(cu.ModuleGetFunction(&fn, mod, "nothing"));
  if (launch_lat("spin", fn))
    return -1;
  /* a context that sleeps in the kernel for completion, as a GPU-bound
   * program with CUDA_DEVICE_SCHEDULE_BLOCKING_SYNC does */
  CK(cu.CtxCreate(&b, CU_CTX_SCHED_BLOCKING_SYNC, dev));
  CK(cu.ModuleLoadData(&mod, ptx));
  CK(cu.ModuleGetFunction(&fn, mod, "nothing"));
  if (launch_lat("blocking", fn))
    return -1;
  CK(cu.CtxDestroy(b));
  return 0;
}

static int t_malloc(void) {
  enum { N = 1000 };
  static double lat[N];
  const size_t sizes[] = {1u << 20, 64u << 20};
  const char *names[] = {"1m", "64m"};

  for (int s = 0; s < 2; s++) {
    int n = s ? 200 : N;
    for (int i = 0; i < n; i++) {
      CUdeviceptr d;
      double t0 = now_s();
      CK(cu.MemAlloc(&d, sizes[s]));
      CK(cu.MemFree(d));
      lat[i] = now_s() - t0;
    }
    stats("malloc", names[s], lat, n, 1e6, "us");
  }
  return 0;
}

static void *sym(void *h, const char *a, const char *b) {
  void *p = dlsym(h, a);
  return !p && b ? dlsym(h, b) : p;
}

int main(int argc, char **argv) {
  void *h = dlopen("libcuda.so.1", RTLD_NOW);
  static const struct {
    const char *name;
    int (*fn)(void);
  } tests[] = {{"memcpy", t_memcpy}, {"hostreg", t_hostreg}, {"launch", t_launch},
               {"malloc", t_malloc}};
  int failed = 0;

  if (!h) {
    printf("BENCH-FAIL cu: dlopen libcuda.so.1: %s\n", dlerror());
    return 1;
  }
  cu.Init = sym(h, "cuInit", NULL);
  cu.DeviceGet = sym(h, "cuDeviceGet", NULL);
  cu.CtxCreate = sym(h, "cuCtxCreate_v2", "cuCtxCreate");
  cu.CtxDestroy = sym(h, "cuCtxDestroy_v2", "cuCtxDestroy");
  cu.CtxSynchronize = sym(h, "cuCtxSynchronize", NULL);
  cu.MemAlloc = sym(h, "cuMemAlloc_v2", "cuMemAlloc");
  cu.MemFree = sym(h, "cuMemFree_v2", "cuMemFree");
  cu.MemAllocHost = sym(h, "cuMemAllocHost_v2", "cuMemAllocHost");
  cu.MemFreeHost = sym(h, "cuMemFreeHost", NULL);
  cu.MemHostRegister = sym(h, "cuMemHostRegister_v2", "cuMemHostRegister");
  cu.MemHostUnregister = sym(h, "cuMemHostUnregister", NULL);
  cu.MemcpyHtoD = sym(h, "cuMemcpyHtoD_v2", "cuMemcpyHtoD");
  cu.MemcpyDtoH = sym(h, "cuMemcpyDtoH_v2", "cuMemcpyDtoH");
  cu.MemcpyDtoD = sym(h, "cuMemcpyDtoD_v2", "cuMemcpyDtoD");
  cu.ModuleLoadData = sym(h, "cuModuleLoadData", NULL);
  cu.ModuleGetFunction = sym(h, "cuModuleGetFunction", NULL);
  cu.LaunchKernel = sym(h, "cuLaunchKernel", NULL);
  if (t_init()) {
    printf("BENCH-FAIL cu-init\n");
    return 1;
  }
  for (unsigned i = 0; i < sizeof(tests) / sizeof(tests[0]); i++) {
    int want = argc < 2;
    for (int a = 1; a < argc; a++)
      want |= !strcmp(argv[a], tests[i].name);
    if (want && tests[i].fn()) {
      printf("BENCH-FAIL cu-%s\n", tests[i].name);
      failed++;
    }
  }
  cu.CtxDestroy(ctx);
  return failed;
}
