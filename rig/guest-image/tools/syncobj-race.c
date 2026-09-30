// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-syncobj-race: many threads of one process creating, importing,
 * signalling, subscribing to and destroying syncobjs on one DRM file at
 * once, and checking every answer against what the kernel's own syncobj
 * code guarantees.
 *
 *   nvgpu-syncobj-race [seconds per phase [threads [node]]]
 *                       (default 5 x 8, /dev/dri/renderD128)
 *
 * The guest module posts a SYNCOBJ_DESTROY it can prove will succeed
 * (driver/nvgpu_syncobj.c, "SYNCOBJ_DESTROY, posted") -- answered 0 before
 * the host has run it -- on a per-file map of the handles the file holds.
 * A map that ever held a handle the host did not would answer 0 for a
 * destroy the host refused, or destroy another thread's object under a
 * reused number. So:
 *
 *  1. owners: each thread, in a loop, CREATEs a syncobj, TIMELINE_SIGNALs a
 *     point unique to it, QUERYs it back (another object under the number
 *     would show another point), subscribes an eventfd to a later point,
 *     signals that point and waits for the eventfd, exports and re-imports
 *     it (a second handle, FD_TO_HANDLE), and DESTROYs both. Nothing else
 *     touches its handles, so every call must succeed, natively and here.
 *  2. owners and guessers: the same, while other threads DESTROY numbers at
 *     random. An owner's call on a handle a guesser took may then fail --
 *     with the kernel's own errno -- but every object is destroyed exactly
 *     once: the successful DESTROYs of all threads add up to the handles
 *     made, and afterwards no number is live.
 *
 * With the module's pacing counters readable (root), the posted DESTROYs
 * the host refused ("posted_failed") must not have grown.
 *
 * Exit status: 0 if every check held, 1 otherwise. Raw ioctls, no libdrm.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

struct syncobj_create {
  uint32_t handle, flags;
};
struct syncobj_destroy {
  uint32_t handle, pad;
};
struct syncobj_handle {
  uint32_t handle, flags;
  int32_t fd;
  uint32_t pad;
  uint64_t point;
};
struct syncobj_timeline_array {
  uint64_t handles, points;
  uint32_t count_handles, flags;
};
struct syncobj_eventfd {
  uint32_t handle, flags;
  uint64_t point;
  int32_t fd;
  uint32_t pad;
};

_Static_assert(sizeof(struct syncobj_handle) == 24, "syncobj_handle");
_Static_assert(sizeof(struct syncobj_timeline_array) == 24, "timeline_array");
_Static_assert(sizeof(struct syncobj_eventfd) == 24, "syncobj_eventfd");

#define D(nr, type) _IOC(_IOC_READ | _IOC_WRITE, 'd', nr, sizeof(type))
#define IOC_CREATE D(0xbf, struct syncobj_create)
#define IOC_DESTROY D(0xc0, struct syncobj_destroy)
#define IOC_H2FD D(0xc1, struct syncobj_handle)
#define IOC_FD2H D(0xc2, struct syncobj_handle)
#define IOC_QUERY D(0xcb, struct syncobj_timeline_array)
#define IOC_TLSIGNAL D(0xcd, struct syncobj_timeline_array)
#define IOC_EVENTFD D(0xcf, struct syncobj_eventfd)

static int dfd;
static atomic_int fails;
static atomic_int stop;
static atomic_long created, destroyed, stolen;
/* The highest handle number seen, for the guessers and the final sweep. */
static atomic_uint max_handle;

static void fail(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void fail(const char *fmt, ...) {
  va_list ap;

  /* The first few only: a broken invariant tends to break it every time. */
  if (atomic_fetch_add(&fails, 1) >= 20)
    return;
  printf("FAIL ");
  va_start(ap, fmt);
  vprintf(fmt, ap);
  va_end(ap);
  putchar('\n');
  fflush(stdout);
}

static int io(unsigned long cmd, void *arg) {
  int r;

  do
    r = ioctl(dfd, cmd, arg);
  while (r && (errno == EINTR || errno == EAGAIN));
  return r ? -errno : 0;
}

static void seen(uint32_t h) {
  unsigned int m = atomic_load(&max_handle);

  while (h > m && !atomic_compare_exchange_weak(&max_handle, &m, h))
    ;
}

static int create(uint32_t *h) {
  struct syncobj_create c = {0};
  int r = io(IOC_CREATE, &c);

  *h = c.handle;
  if (!r) {
    atomic_fetch_add(&created, 1);
    seen(c.handle);
  }
  return r;
}

/* 0 or -errno; a success is counted, whoever's handle it was. */
static int destroy(uint32_t h) {
  struct syncobj_destroy d = {.handle = h};
  int r = io(IOC_DESTROY, &d);

  if (!r)
    atomic_fetch_add(&destroyed, 1);
  return r;
}

static int tl(unsigned long cmd, uint32_t h, uint64_t *point) {
  struct syncobj_timeline_array a = {
      .handles = (uintptr_t)&h, .points = (uintptr_t)point, .count_handles = 1};

  return io(cmd, &a);
}

/* The eventfd subscribed to (h, point) fires within a second. */
static int eventfd_fires(int efd) {
  struct pollfd p = {.fd = efd, .events = POLLIN};
  uint64_t v;
  int r;

  do
    r = poll(&p, 1, 1000);
  while (r < 0 && errno == EINTR);
  if (r != 1)
    return 0;
  return read(efd, &v, sizeof(v)) == sizeof(v) && v >= 1;
}

struct owner {
  pthread_t t;
  unsigned int id;
  int strict; /* no guessers: every call must succeed */
  long iters;
};

/* One round of an owner. In a strict phase every failure is one; with
 * guessers about, a handle may be taken from under us, and then the call
 * must fail with the kernel's own errno, EINVAL or ENOENT -- never answer
 * for another object. */
static void owner_round(struct owner *o, uint64_t seq) {
  const uint64_t base = ((uint64_t)o->id << 40) | (seq << 2);
  uint64_t pt = base + 1, got = 0;
  uint32_t h = 0, h2 = 0;
  int efd = -1, r;

  r = create(&h);
  if (r) {
    fail("owner %u: SYNCOBJ_CREATE: %d", o->id, r);
    return;
  }

  r = tl(IOC_TLSIGNAL, h, &pt);
  if (!r)
    r = tl(IOC_QUERY, h, &got);
  if (r) {
    if (o->strict || (r != -EINVAL && r != -ENOENT))
      fail("owner %u: signal/query of its handle %u: %d", o->id, h, r);
    goto out;
  }
  /* Another object under our number would carry another point. A guesser
   * could have destroyed ours and another owner made its own under the
   * number meanwhile -- which answers with that owner's point, not ours. */
  if (got != pt && (o->strict || (got >> 40) == o->id))
    fail("owner %u: handle %u queried point %#llx, signalled %#llx", o->id, h,
         (unsigned long long)got, (unsigned long long)pt);

  /* A subscriber to the next point, which is then signalled. */
  efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (efd >= 0) {
    struct syncobj_eventfd e = {.handle = h, .point = base + 2, .fd = efd};

    r = io(IOC_EVENTFD, &e);
    /*
     * With guessers about, a subscription whose syncobj a guesser destroyed
     * before its point was signalled never fires; the syncobj lives on
     * (exported, so imported: the backend cannot tell it is unreachable),
     * and so does its host registration, charged to this process until the
     * file closes. After 256 of those the process's share is spent and
     * SYNCOBJ_EVENTFD says -ENOMEM, the kernel's own answer when it cannot
     * make an entry -- this process's doing, and no other's.
     */
    if (r == -ENOMEM && !o->strict)
      goto out;
    if (r) {
      if (o->strict || (r != -EINVAL && r != -ENOENT))
        fail("owner %u: SYNCOBJ_EVENTFD on its handle %u: %d", o->id, h, r);
      goto out;
    }
    pt = base + 2;
    r = tl(IOC_TLSIGNAL, h, &pt);
    if (r) {
      if (o->strict || (r != -EINVAL && r != -ENOENT))
        fail("owner %u: signal of its handle %u: %d", o->id, h, r);
      goto out;
    }
    if (o->strict && !eventfd_fires(efd))
      fail("owner %u: eventfd on handle %u point %#llx never fired", o->id, h,
           (unsigned long long)pt);
  }

  /* Export and import: a second handle for the same object. */
  {
    struct syncobj_handle x = {.handle = h, .fd = -1};

    r = io(IOC_H2FD, &x);
    if (r) {
      if (o->strict || (r != -EINVAL && r != -ENOENT))
        fail("owner %u: HANDLE_TO_FD of its handle %u: %d", o->id, h, r);
      goto out;
    }
    x.handle = 0;
    r = io(IOC_FD2H, &x);
    close(x.fd);
    if (r) {
      fail("owner %u: FD_TO_HANDLE of its own syncobj file: %d", o->id, r);
      goto out;
    }
    h2 = x.handle;
    atomic_fetch_add(&created, 1);
    seen(h2);
    got = 0;
    r = tl(IOC_QUERY, h2, &got);
    if (o->strict && (r || got != base + 2))
      fail("owner %u: imported handle %u: query %d, point %#llx not %#llx",
           o->id, h2, r, (unsigned long long)got,
           (unsigned long long)(base + 2));
  }

out:
  if (efd >= 0)
    close(efd);
  if (h2) {
    r = destroy(h2);
    if (r) {
      atomic_fetch_add(&stolen, 1);
      if (o->strict || r != -EINVAL)
        fail("owner %u: DESTROY of its imported handle %u: %d", o->id, h2, r);
    }
  }
  r = destroy(h);
  if (r) {
    atomic_fetch_add(&stolen, 1);
    if (o->strict || r != -EINVAL)
      fail("owner %u: DESTROY of its handle %u: %d", o->id, h, r);
  }
}

static void *owner_main(void *arg) {
  struct owner *o = arg;
  uint64_t seq = 0;

  while (!atomic_load(&stop)) {
    owner_round(o, ++seq);
    o->iters++;
  }
  return NULL;
}

static void *guesser_main(void *arg) {
  unsigned int seed = (unsigned int)(uintptr_t)arg;

  while (!atomic_load(&stop)) {
    unsigned int m = atomic_load(&max_handle) + 4;

    destroy(1 + rand_r(&seed) % m);
  }
  return NULL;
}

/* The module's "posted_failed" counter, or -1 without the pacing report. */
static long posted_failed(void) {
  FILE *f = fopen("/sys/module/virtio_gpu_nv/parameters/pacing", "r");
  char line[256];
  long v = -1;

  if (!f)
    return -1;
  while (fgets(line, sizeof(line), f))
    if (sscanf(line, "posted_failed %ld", &v) == 1)
      break;
  fclose(f);
  return v;
}

static long posted(void) {
  FILE *f = fopen("/sys/module/virtio_gpu_nv/parameters/pacing", "r");
  char line[256];
  long v = -1;

  if (!f)
    return -1;
  while (fgets(line, sizeof(line), f))
    if (sscanf(line, "posted %ld", &v) == 1)
      break;
  fclose(f);
  return v;
}

static void phase(const char *name, int secs, int nowners, int nguessers) {
  struct owner *o = calloc(nowners, sizeof(*o));
  pthread_t *g = calloc(nguessers ? nguessers : 1, sizeof(*g));
  long p0 = posted(), f0 = posted_failed(), iters = 0;
  int i, live = 0;

  atomic_store(&stop, 0);
  atomic_store(&created, 0);
  atomic_store(&destroyed, 0);
  atomic_store(&stolen, 0);
  for (i = 0; i < nowners; i++) {
    o[i].id = i + 1;
    o[i].strict = !nguessers;
    pthread_create(&o[i].t, NULL, owner_main, &o[i]);
  }
  for (i = 0; i < nguessers; i++)
    pthread_create(&g[i], NULL, guesser_main, (void *)(uintptr_t)(i * 7919 + 1));
  sleep(secs);
  atomic_store(&stop, 1);
  for (i = 0; i < nowners; i++) {
    pthread_join(o[i].t, NULL);
    iters += o[i].iters;
  }
  for (i = 0; i < nguessers; i++)
    pthread_join(g[i], NULL);

  /* Every number that could have been handed out is dead now. */
  for (unsigned int h = 1; h <= atomic_load(&max_handle) + 64; h++) {
    uint64_t pt = 0;

    if (tl(IOC_QUERY, h, &pt) == 0) {
      live++;
      destroy(h);
    }
  }
  if (live)
    fail("%s: %d syncobj(s) still live after every thread destroyed its own",
         name, live);
  if (atomic_load(&destroyed) - live != atomic_load(&created))
    fail("%s: %ld handles made, %ld destroyed successfully (%d by the sweep): "
         "not each exactly once",
         name, atomic_load(&created), atomic_load(&destroyed) - live, live);
  if (f0 >= 0 && posted_failed() != f0)
    fail("%s: %ld posted DESTROY(s) the host refused", name,
         posted_failed() - f0);
  printf("%s %s: %d owners, %d guessers, %ld rounds, %ld handles, %ld taken "
         "by guessers, %ld DESTROYs posted\n",
         atomic_load(&fails) ? "FAIL" : "PASS", name, nowners, nguessers,
         iters, atomic_load(&created), atomic_load(&stolen),
         p0 >= 0 ? posted() - p0 : -1L);
  fflush(stdout);
  free(o);
  free(g);
}

int main(int argc, char **argv) {
  int secs = argc > 1 ? atoi(argv[1]) : 5;
  int threads = argc > 2 ? atoi(argv[2]) : 8;
  const char *node = argc > 3 ? argv[3] : "/dev/dri/renderD128";

  setvbuf(stdout, NULL, _IOLBF, 0);
  dfd = open(node, O_RDWR | O_CLOEXEC);
  if (dfd < 0) {
    printf("FAIL open %s: %s\n", node, strerror(errno));
    return 1;
  }
  if (threads < 1)
    threads = 1;
  {
    struct syncobj_create c = {0};
    struct syncobj_destroy d = {0};
    int r = io(IOC_CREATE, &c);

    if (r == -EOPNOTSUPP || r == -ENODEV || r == -ENOTTY) {
      printf("SKIP no syncobjs on %s: %s\n", node, strerror(-r));
      return 77;
    }
    d.handle = c.handle;
    if (r || io(IOC_DESTROY, &d)) {
      printf("FAIL a first SYNCOBJ_CREATE/DESTROY: %d\n", r);
      return 1;
    }
  }
  phase("owners", secs, threads, 0);
  phase("owners+guessers", secs, threads, threads / 2 ? threads / 2 : 1);
  printf("%s nvgpu-syncobj-race: %d failure(s)\n",
         atomic_load(&fails) ? "FAIL" : "PASS", atomic_load(&fails));
  return atomic_load(&fails) ? 1 : 0;
}
