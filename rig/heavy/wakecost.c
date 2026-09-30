// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-wakecost -- what a thread waking another costs, natively and in a
 * guest: the round trip of two threads on two CPUs handing a token back and
 * forth through a futex (each hand-off a FUTEX_WAKE of a sleeping thread:
 * natively an IPI to a CPU that may be idle, in a guest an IPI to a vCPU
 * that may have halted to the host), the same hand-off with the waiter
 * spinning instead of sleeping (the cache line alone), and the cost of one
 * CPUID (a VM exit and entry in a guest, nothing natively).
 *
 *   nvgpu-wakecost [pairs]   pairs of CPUs to try (default 0,1 and 0,2)
 *
 * Output: HEAVY_WAKE lines, microseconds, p50 and p99 of 20,000 round trips.
 */
#define _GNU_SOURCE
#include <cpuid.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

enum { N = 20000, WARM = 2000 };

static double now_us(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1e6 + ts.tv_nsec * 1e-3;
}

static int cmp(const void *a, const void *b) {
  double x = *(const double *)a, y = *(const double *)b;
  return x < y ? -1 : x > y;
}

static _Atomic uint32_t tok;
static int spin_mode, cpu_b;

static void pin(int cpu) {
  cpu_set_t s;
  CPU_ZERO(&s);
  CPU_SET(cpu, &s);
  sched_setaffinity(0, sizeof(s), &s);
}

static void wait_for(uint32_t v) {
  uint32_t cur;
  while ((cur = atomic_load(&tok)) != v)
    if (spin_mode)
      __builtin_ia32_pause();
    else
      syscall(SYS_futex, &tok, FUTEX_WAIT_PRIVATE, cur, NULL, NULL, 0);
}

static void give(uint32_t v) {
  atomic_store(&tok, v);
  if (!spin_mode)
    syscall(SYS_futex, &tok, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
}

static void *peer(void *arg) {
  (void)arg;
  pin(cpu_b);
  for (uint32_t i = 0; i < WARM + N; i++) {
    wait_for(2 * i + 1);
    give(2 * i + 2);
  }
  return NULL;
}

static void pingpong(int a, int b, int spin) {
  static double rt[N];
  pthread_t t;
  spin_mode = spin;
  cpu_b = b;
  atomic_store(&tok, 0);
  pin(a);
  pthread_create(&t, NULL, peer, NULL);
  for (uint32_t i = 0; i < WARM + N; i++) {
    double t0 = now_us();
    give(2 * i + 1);
    wait_for(2 * i + 2);
    if (i >= WARM)
      rt[i - WARM] = now_us() - t0;
  }
  pthread_join(t, NULL);
  qsort(rt, N, sizeof(rt[0]), cmp);
  printf("HEAVY_WAKE %s cpus=%d,%d rt_p50_us=%.2f rt_p99_us=%.2f\n", spin ? "spin" : "futex", a, b,
         rt[N / 2], rt[N * 99 / 100]);
}

int main(int argc, char **argv) {
  unsigned eax, ebx, ecx, edx;
  double t0 = now_us();
  for (int i = 0; i < 100000; i++)
    __cpuid(0, eax, ebx, ecx, edx);
  printf("HEAVY_WAKE cpuid_us=%.3f\n", (now_us() - t0) / 100000);
  int pairs[4][2] = {{0, 1}, {0, 2}}, np = 2;
  if (argc > 1) {
    np = 0;
    for (int i = 1; i < argc && np < 4; i++)
      if (sscanf(argv[i], "%d,%d", &pairs[np][0], &pairs[np][1]) == 2)
        np++;
  }
  for (int p = 0; p < np; p++) {
    pingpong(pairs[p][0], pairs[p][1], 0);
    pingpong(pairs[p][0], pairs[p][1], 1);
  }
  return 0;
}
