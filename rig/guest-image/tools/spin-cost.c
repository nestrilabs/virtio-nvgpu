// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-spin-cost: what the module's reply spinning (rt_spin_us) costs a
 * guest whose CPUs are busy, and what it buys an idle one.
 *
 *   nvgpu-spin-cost [seconds [hogs [callers [node]]]]
 *                   (default 5 s, one hog per CPU, 1 caller, renderD128)
 *
 * `hogs` threads spin on a counter (a CPU-bound application); `callers`
 * threads make back-to-back SYNCOBJ_QUERY calls on one syncobj (the
 * cheapest IOCTL2 round trip: inline on the host, nothing to translate).
 * Printed, one line: calls per second and their mean latency, and the hogs'
 * iterations per second -- run it with rt_spin_us at 0 and at its default
 * (/sys/module/virtio_gpu_nv/parameters/rt_spin_us, root) to compare. A
 * caller that spins holds its CPU for the round trip; one that sleeps
 * gives it to a hog meanwhile, and pays the interrupt and the wakeup.
 *
 * Exit status 0 unless a call failed. Raw ioctls, no libdrm.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

struct syncobj_create {
  uint32_t handle, flags;
};
struct syncobj_timeline_array {
  uint64_t handles, points;
  uint32_t count_handles, flags;
};

#define D(nr, type) _IOC(_IOC_READ | _IOC_WRITE, 'd', nr, sizeof(type))
#define IOC_CREATE D(0xbf, struct syncobj_create)
#define IOC_QUERY D(0xcb, struct syncobj_timeline_array)

static int dfd;
static uint32_t syncobj;
static atomic_int stop, failed;

struct counter {
  pthread_t t;
  uint64_t n;
  char pad[48];
};

static void *hog(void *arg) {
  struct counter *c = arg;
  volatile uint64_t n = 0;

  while (!atomic_load_explicit(&stop, memory_order_relaxed))
    for (int i = 0; i < 100000; i++)
      n++;
  c->n = n;
  return NULL;
}

static void *caller(void *arg) {
  struct counter *c = arg;
  uint32_t h = syncobj;
  uint64_t pt = 0, n = 0;
  struct syncobj_timeline_array a = {
      .handles = (uintptr_t)&h, .points = (uintptr_t)&pt, .count_handles = 1};

  while (!atomic_load_explicit(&stop, memory_order_relaxed)) {
    if (ioctl(dfd, IOC_QUERY, &a)) {
      atomic_store(&failed, errno);
      break;
    }
    n++;
  }
  c->n = n;
  return NULL;
}

static double now(void) {
  struct timespec ts;

  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec + ts.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
  int secs = argc > 1 ? atoi(argv[1]) : 5;
  long ncpu = sysconf(_SC_NPROCESSORS_ONLN);
  int nhog = argc > 2 ? atoi(argv[2]) : (int)ncpu;
  int ncall = argc > 3 ? atoi(argv[3]) : 1;
  const char *node = argc > 4 ? argv[4] : "/dev/dri/renderD128";
  struct counter *hogs = calloc(nhog ? nhog : 1, sizeof(*hogs));
  struct counter *calls = calloc(ncall ? ncall : 1, sizeof(*calls));
  struct syncobj_create c = {0};
  uint64_t hn = 0, cn = 0;
  double t0, t;
  char spin[32] = "?";
  FILE *f;
  int i;

  dfd = open(node, O_RDWR | O_CLOEXEC);
  if (dfd < 0 || ioctl(dfd, IOC_CREATE, &c)) {
    printf("FAIL %s: %s\n", node, strerror(errno));
    return 1;
  }
  syncobj = c.handle;
  f = fopen("/sys/module/virtio_gpu_nv/parameters/rt_spin_us", "r");
  if (f) {
    if (fscanf(f, "%31s", spin) != 1)
      strcpy(spin, "?");
    fclose(f);
  }

  t0 = now();
  for (i = 0; i < nhog; i++)
    pthread_create(&hogs[i].t, NULL, hog, &hogs[i]);
  for (i = 0; i < ncall; i++)
    pthread_create(&calls[i].t, NULL, caller, &calls[i]);
  sleep(secs);
  atomic_store(&stop, 1);
  for (i = 0; i < nhog; i++) {
    pthread_join(hogs[i].t, NULL);
    hn += hogs[i].n;
  }
  for (i = 0; i < ncall; i++) {
    pthread_join(calls[i].t, NULL);
    cn += calls[i].n;
  }
  t = now() - t0;
  printf("SPINCOST rt_spin_us=%s cpus=%ld hogs=%d callers=%d secs=%.2f "
         "calls_per_s=%.0f call_us=%.2f hog_miter_per_s=%.1f\n",
         spin, ncpu, nhog, ncall, t, cn / t,
         cn ? ncall * t * 1e6 / cn : 0.0, hn / t / 1e6);
  if (atomic_load(&failed)) {
    printf("FAIL SYNCOBJ_QUERY: %s\n", strerror(atomic_load(&failed)));
    return 1;
  }
  return 0;
}
