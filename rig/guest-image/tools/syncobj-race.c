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
 *     with the kernel's own errno -- or answer for whatever object has the
 *     number now, but every object is destroyed exactly once: the
 *     successful DESTROYs of all threads add up to the handles made, and
 *     afterwards no number is live.
 *
 *  3. orphans: one process after another (NVGPU_RACE_ORPHAN_RUNS, default
 *     6; 0 skips the phase), each on a DRM file of its own, first
 *     subscribes to a point nobody will signal on syncobjs only it holds,
 *     destroying each at once (let go with it: never refused), then makes
 *     orphans until SYNCOBJ_EVENTFD says -ENOMEM: a syncobj exported and
 *     imported back, a subscription through the first handle to a point
 *     that never comes, the first handle destroyed -- the import keeps the
 *     syncobj, and the subscription's host registration, alive. Then it
 *     exits without cleaning up. Each must get as far as the first did: its
 *     exit let go of everything it made, and its registrations only ever
 *     counted against its own share. A fresh process must then still
 *     subscribe. These orphans once outlived their process, and a few runs
 *     of phase 2 left SYNCOBJ_EVENTFD -ENOMEM for every process of the VM.
 *
 * With the module's pacing counters readable (root), the posted DESTROYs
 * the host refused ("posted_failed") must not have grown.
 *
 * Every owner's eventfd must fire within the patience (NVGPU_RACE_PATIENCE_MS,
 * default 1000). One that has not is waited for on, up to NVGPU_RACE_DIAG_S
 * (default 30) seconds in all, and the failure says which it was: LATE
 * (fired, and when) or LOST (never), with the syncobj's point as the host
 * had it when the patience ran out (SYNCOBJ_QUERY, and a TIMELINE_WAIT poll)
 * -- signalled there and not delivered, or never signalled -- and the event
 * records the guest took meanwhile (the module's ev_records), and how many
 * other owners' eventfds fired while it waited: a stall of the whole event
 * path, or one wakeup lost. Each phase prints the distribution of the time
 * from TIMELINE_SIGNAL to the eventfd firing.
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
#include <sys/wait.h>
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
struct syncobj_timeline_wait {
  uint64_t handles, points;
  int64_t timeout_nsec;
  uint32_t count_handles, flags, first_signaled, pad;
  uint64_t deadline_nsec;
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
_Static_assert(sizeof(struct syncobj_timeline_wait) == 48, "timeline_wait");

#define D(nr, type) _IOC(_IOC_READ | _IOC_WRITE, 'd', nr, sizeof(type))
#define IOC_CREATE D(0xbf, struct syncobj_create)
#define IOC_DESTROY D(0xc0, struct syncobj_destroy)
#define IOC_H2FD D(0xc1, struct syncobj_handle)
#define IOC_FD2H D(0xc2, struct syncobj_handle)
#define IOC_TLWAIT D(0xca, struct syncobj_timeline_wait)
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

static uint64_t now_ns(void) {
  struct timespec t;

  clock_gettime(CLOCK_MONOTONIC, &t);
  return (uint64_t)t.tv_sec * 1000000000u + (uint64_t)t.tv_nsec;
}

/* How long an eventfd may take (NVGPU_RACE_PATIENCE_MS), and how long one
 * that took longer is waited for to tell late from lost (NVGPU_RACE_DIAG_S). */
static int patience_ms = 1000;
static int diag_s = 30;

/* Signal-to-fire times of the eventfds of a phase, in powers of two of a
 * microsecond (bucket b: under 2^(b+1) us), and the slowest. */
#define LAT_BUCKETS 32
static atomic_long lat[LAT_BUCKETS];
static _Atomic uint64_t lat_max_ns;
/* Eventfds fired, all owners: what the others did while one waited. */
static atomic_long fires;

static void lat_record(uint64_t ns) {
  uint64_t us = ns / 1000;
  unsigned int b = 0;
  uint64_t m = atomic_load(&lat_max_ns);

  while (us > 1 && b < LAT_BUCKETS - 1) {
    us >>= 1;
    b++;
  }
  atomic_fetch_add(&lat[b], 1);
  while (ns > m && !atomic_compare_exchange_weak(&lat_max_ns, &m, ns))
    ;
}

/* The upper bound, in us, of the bucket holding quantile q of n. */
static unsigned long long lat_quantile(long n, double q) {
  long want = (long)(q * (double)n), sum = 0;

  for (int b = 0; b < LAT_BUCKETS; b++) {
    sum += atomic_load(&lat[b]);
    if (sum > want)
      return 2ull << b;
  }
  return 2ull << (LAT_BUCKETS - 1);
}

static void lat_report(const char *name) {
  long n = 0, over = 0;

  for (int b = 0; b < LAT_BUCKETS; b++) {
    n += atomic_load(&lat[b]);
    if (b >= 10) /* 1 ms and up */
      over += atomic_load(&lat[b]);
  }
  if (!n)
    return;
  printf("INFO %s: eventfd signal->fire, %ld fired: p50<=%lluus p99<=%lluus "
         "p99.9<=%lluus p99.99<=%lluus max=%.1fms, %ld over 1ms; log2 us:",
         name, n, lat_quantile(n, 0.5), lat_quantile(n, 0.99),
         lat_quantile(n, 0.999), lat_quantile(n, 0.9999),
         (double)atomic_load(&lat_max_ns) / 1e6, over);
  for (int b = 0; b < LAT_BUCKETS; b++)
    if (atomic_load(&lat[b]))
      printf(" [%d]=%ld", b, atomic_load(&lat[b]));
  putchar('\n');
  fflush(stdout);
}

static long pacing_ctr(const char *key);

/* Poll `efd` until `deadline` (CLOCK_MONOTONIC ns): 1 readable, 0 not. */
static int poll_until(int efd, uint64_t deadline) {
  struct pollfd p = {.fd = efd, .events = POLLIN};
  int r;

  for (;;) {
    uint64_t t = now_ns();
    int ms;

    if (t >= deadline)
      return 0;
    ms = (int)((deadline - t + 999999) / 1000000);
    r = poll(&p, 1, ms);
    if (r == 1)
      return 1;
    if (r < 0 && errno != EINTR)
      return 0;
  }
}

/*
 * The eventfd subscribed to (h, point) fires within the patience of `t0`,
 * when the point was signalled. When it does not, it is waited for on, and
 * the failure says what was found: late or lost, and the host's view of the
 * point when the patience ran out. 0 for a failure not yet reported.
 */
static int eventfd_fires(int efd, unsigned int id, uint32_t h, uint64_t point,
                         uint64_t t0) {
  uint64_t v, t1, got = 0;
  struct syncobj_timeline_wait w = {0};
  long rec0, fires0, rec1;
  int q, tw, fired;

  if (poll_until(efd, t0 + (uint64_t)patience_ms * 1000000u)) {
    t1 = now_ns();
    atomic_fetch_add(&fires, 1);
    lat_record(t1 - t0);
    return read(efd, &v, sizeof(v)) == sizeof(v) && v >= 1;
  }

  /* Late or lost. What the host has, now. */
  rec0 = pacing_ctr("ev_records");
  fires0 = atomic_load(&fires);
  q = tl(IOC_QUERY, h, &got);
  w.handles = (uintptr_t)&h;
  w.points = (uintptr_t)&point;
  w.count_handles = 1;
  tw = io(IOC_TLWAIT, &w); /* timeout 0: a poll */
  fired = poll_until(efd, t0 + (uint64_t)diag_s * 1000000000u);
  t1 = now_ns();
  rec1 = pacing_ctr("ev_records");
  if (fired) {
    atomic_fetch_add(&fires, 1);
    lat_record(t1 - t0);
  }
  fail("owner %u: eventfd on handle %u point %#llx %s %.1f ms after the "
       "signal; at %d ms the host's point was %#llx (query %d, timeline "
       "wait %d: %s), and until %s the guest took %ld event record(s) and "
       "%ld other eventfd(s) fired",
       id, h, (unsigned long long)point,
       fired ? "LATE: fired" : "LOST: not fired", (double)(t1 - t0) / 1e6,
       patience_ms, (unsigned long long)got, q, tw,
       tw == 0 ? "signalled" : tw == -ETIME ? "not signalled" : "error",
       fired ? "it fired" : "it gave up",
       rec0 >= 0 && rec1 >= 0 ? rec1 - rec0 : -1L,
       atomic_load(&fires) - fires0);
  if (fired)
    (void)!read(efd, &v, sizeof(v));
  return 1; /* failed already, and said how */
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
  uint64_t t0;
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
  /*
   * Another object under our number would carry another point. With
   * guessers about, a guesser may have destroyed ours and some other object
   * got the number -- another owner's, or one of our own from an earlier
   * round: an owner whose number a guesser took goes on as if the object
   * now under it were its own, exports it and imports it back, and an
   * import lands on the lowest free number, ours among them. Native Linux
   * does exactly that (every run of this phase on the host's own render
   * node shows a few). What no object can carry is a point of ours we have
   * not signalled yet.
   */
  if (o->strict ? got != pt
                : (got >> 40) == o->id && (got & ((1ull << 40) - 1)) > (pt & ((1ull << 40) - 1)))
    fail("owner %u: handle %u queried point %#llx, signalled %#llx", o->id, h,
         (unsigned long long)got, (unsigned long long)pt);

  /* A subscriber to the next point, which is then signalled. */
  efd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (efd >= 0) {
    struct syncobj_eventfd e = {.handle = h, .point = base + 2, .fd = efd};

    r = io(IOC_EVENTFD, &e);
    /*
     * With guessers about, a subscription whose handle a guesser destroyed
     * before its point was signalled never fires. Its host registration
     * lasts as long as something of this process still reaches the
     * syncobj (another handle a guesser has not taken yet), charged to
     * this process; with 256 of those at once its share is spent and
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
    t0 = now_ns();
    r = tl(IOC_TLSIGNAL, h, &pt);
    if (r) {
      if (o->strict || (r != -EINVAL && r != -ENOENT))
        fail("owner %u: signal of its handle %u: %d", o->id, h, r);
      goto out;
    }
    if (o->strict && !eventfd_fires(efd, o->id, h, pt, t0))
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

/* A counter of the module's pacing report, or -1 without it (not root). */
static long pacing_ctr(const char *key) {
  FILE *f = fopen("/sys/module/virtio_gpu_nv/parameters/pacing", "r");
  char line[256], name[64];
  long v = -1, x;

  if (!f)
    return -1;
  while (fgets(line, sizeof(line), f))
    if (sscanf(line, "%63s %ld", name, &x) == 2 && !strcmp(name, key)) {
      v = x;
      break;
    }
  fclose(f);
  return v;
}

/* The module's "posted_failed" counter, or -1 without the pacing report. */
static long posted_failed(void) { return pacing_ctr("posted_failed"); }

static long posted(void) { return pacing_ctr("posted"); }

static void phase(const char *name, int secs, int nowners, int nguessers) {
  struct owner *o = calloc(nowners, sizeof(*o));
  pthread_t *g = calloc(nguessers ? nguessers : 1, sizeof(*g));
  long p0 = posted(), f0 = posted_failed(), iters = 0;
  int i, live = 0;

  atomic_store(&stop, 0);
  for (i = 0; i < LAT_BUCKETS; i++)
    atomic_store(&lat[i], 0);
  atomic_store(&lat_max_ns, 0);
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
  lat_report(name);
  free(o);
  free(g);
}

/* What one orphans child did: subscriptions made on syncobjs let go at
 * once, orphans made, and the errno that stopped each (0: not stopped). */
struct orphan_report {
  int private_made, private_err;
  int orphans_made, orphans_err;
};

/* A point nobody signals. */
#define NEVER_POINT (1ull << 62)

/* SYNCOBJ_EVENTFD on (h, point) with a fresh eventfd, closed after: the
 * subscription lives on in the kernel (the module's, the backend's). */
static int subscribe(uint32_t h, uint64_t point) {
  struct syncobj_eventfd e = {.handle = h, .point = point};
  int r;

  e.fd = eventfd(0, EFD_CLOEXEC | EFD_NONBLOCK);
  if (e.fd < 0)
    return -errno;
  r = io(IOC_EVENTFD, &e);
  close(e.fd);
  return r;
}

/* The child's work, on a DRM file of its own: never returns. */
static void orphan_child(const char *node, int wfd, int private_rounds,
                         int max_orphans) {
  struct orphan_report rep = {0};
  int i, r;

  dfd = open(node, O_RDWR | O_CLOEXEC);
  if (dfd < 0) {
    rep.private_err = rep.orphans_err = -errno;
    goto out;
  }
  /* Private: the handle is the only way to the syncobj, and its DESTROY
   * lets go of the subscription with it. */
  for (i = 0; i < private_rounds; i++) {
    uint32_t h;

    r = create(&h);
    if (!r)
      r = subscribe(h, NEVER_POINT + (uint64_t)i);
    destroy(h);
    if (r) {
      rep.private_err = r;
      break;
    }
    rep.private_made++;
  }
  /* Orphans: exported and imported back, then the handle subscribed
   * through destroyed. */
  for (i = 0; i < max_orphans; i++) {
    struct syncobj_handle x = {.fd = -1};
    uint32_t h;

    r = create(&h);
    if (r) {
      rep.orphans_err = r;
      break;
    }
    x.handle = h;
    r = io(IOC_H2FD, &x);
    if (!r) {
      x.handle = 0;
      r = io(IOC_FD2H, &x);
      close(x.fd);
    }
    if (!r)
      r = subscribe(h, NEVER_POINT + (uint64_t)i);
    destroy(h);
    if (r) {
      rep.orphans_err = r;
      break;
    }
    rep.orphans_made++;
  }
out:
  (void)!write(wfd, &rep, sizeof(rep));
  /* No cleanup: the exit closes the file, the imports with it. */
  _exit(0);
}

/* Whether environment variable `name` is "1". */
static int only(const char *name) {
  const char *v = getenv(name);

  return v && !strcmp(v, "1");
}

/* One child, waited for. 0 and its report, or -1. */
static int orphan_run(const char *node, int private_rounds, int max_orphans,
                      struct orphan_report *rep) {
  int p[2], st;
  pid_t pid;
  ssize_t n;

  if (pipe2(p, O_CLOEXEC))
    return -1;
  pid = fork();
  if (pid < 0) {
    close(p[0]);
    close(p[1]);
    return -1;
  }
  if (!pid) {
    close(p[0]);
    orphan_child(node, p[1], private_rounds, max_orphans);
  }
  close(p[1]);
  n = read(p[0], rep, sizeof(*rep));
  close(p[0]);
  while (waitpid(pid, &st, 0) < 0 && errno == EINTR)
    ;
  /* The guest closes a dead process's files on its own time; give the
   * backend a moment to see them go. */
  usleep(200 * 1000);
  return n == (ssize_t)sizeof(*rep) ? 0 : -1;
}

/*
 * Phase 3 (see the top). A fresh process must get as far as the first:
 * the share is the backend's per process (a quarter of its 1,024), less
 * whatever other processes of the VM hold meanwhile, so a little slack.
 */
static void orphans_phase(const char *node) {
  const char *e = getenv("NVGPU_RACE_ORPHAN_RUNS");
  int runs = e ? atoi(e) : 6, first = -1, i, f0 = atomic_load(&fails);
  struct orphan_report rep;

  if (e && !strcmp(e, "0")) {
    printf("INFO orphans: skipped (NVGPU_RACE_ORPHAN_RUNS=0)\n");
    return;
  }
  if (runs < 2)
    runs = 2;
  for (i = 0; i < runs; i++) {
    if (orphan_run(node, 512, 4096, &rep)) {
      fail("orphans: run %d: no report from its process", i + 1);
      return;
    }
    printf("INFO orphans: run %d: %d let go with their syncobj (%d), %d "
           "orphans before %d\n",
           i + 1, rep.private_made, rep.private_err, rep.orphans_made,
           rep.orphans_err);
    if (rep.private_err)
      fail("orphans: run %d: SYNCOBJ_EVENTFD on a syncobj only it held "
           "failed %d after %d, each destroyed at once",
           i + 1, rep.private_err, rep.private_made);
    /* -ENOMEM: its share spent; 0: none refused (natively, no cap). */
    if (rep.orphans_err && rep.orphans_err != -ENOMEM)
      fail("orphans: run %d: its orphans stopped at %d with %d, not "
           "-ENOMEM (its share spent)",
           i + 1, rep.orphans_made, rep.orphans_err);
    if (first < 0) {
      first = rep.orphans_made;
      if (first < 16)
        fail("orphans: run 1 made only %d", first);
    } else if (rep.orphans_made < first - first / 8) {
      fail("orphans: run %d made %d orphans, the first %d: what the runs "
           "before made outlived them",
           i + 1, rep.orphans_made, first);
    }
  }
  /* A fresh process, subscribing and nothing else. */
  if (orphan_run(node, 64, 0, &rep) || rep.private_err ||
      rep.private_made != 64)
    fail("orphans: a fresh process's SYNCOBJ_EVENTFD failed %d after %d",
         rep.private_err, rep.private_made);
  printf("%s orphans: %d runs of %d orphans each, then a fresh process "
         "subscribed %d times\n",
         atomic_load(&fails) != f0 ? "FAIL" : "PASS", runs, first,
         rep.private_made);
}

int main(int argc, char **argv) {
  int secs = argc > 1 ? atoi(argv[1]) : 5;
  int threads = argc > 2 ? atoi(argv[2]) : 8;
  const char *node = argc > 3 ? argv[3] : "/dev/dri/renderD128";

  setvbuf(stdout, NULL, _IOLBF, 0);
  if (getenv("NVGPU_RACE_PATIENCE_MS"))
    patience_ms = atoi(getenv("NVGPU_RACE_PATIENCE_MS"));
  if (getenv("NVGPU_RACE_DIAG_S"))
    diag_s = atoi(getenv("NVGPU_RACE_DIAG_S"));
  if (patience_ms < 1)
    patience_ms = 1;
  if ((long)diag_s * 1000 < patience_ms)
    diag_s = (patience_ms + 999) / 1000;
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
  /* NVGPU_RACE_ORPHANS_ONLY=1: phase 3 alone. */
  if (only("NVGPU_RACE_ORPHANS_ONLY")) {
    orphans_phase(node);
    goto done;
  }
  phase("owners", secs, threads, 0);
  /* NVGPU_RACE_OWNERS_ONLY=1: the first phase alone. */
  if (!only("NVGPU_RACE_OWNERS_ONLY")) {
    phase("owners+guessers", secs, threads, threads / 2 ? threads / 2 : 1);
    orphans_phase(node);
  }
done:
  printf("%s nvgpu-syncobj-race: %d failure(s)\n",
         atomic_load(&fails) ? "FAIL" : "PASS", atomic_load(&fails));
  return atomic_load(&fails) ? 1 : 0;
}
