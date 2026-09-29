// SPDX-License-Identifier: Apache-2.0
/*
 * nvgpu-drm-compat [render node]: DRM ioctls whose struct grew, issued with
 * the caller's older (shorter) and a newer (longer) size, and checked against
 * the same call at the native size. The guest module normalises every DRM
 * ioctl it serves as drm_ioctl() does (drm_ioctl.c:848-915): the caller's
 * bytes in, zero-extended to the native struct, the caller's size back.
 *
 *   SYNCOBJ_HANDLE_TO_FD / FD_TO_HANDLE with the 16-byte drm_syncobj_handle
 *     (before `point`: the Steam runtime's libdrm) and a 32-byte one;
 *   SYNCOBJ_WAIT (32 bytes) and SYNCOBJ_TIMELINE_WAIT (40), before
 *     `deadline_nsec`.
 *
 * Built 64- and 32-bit (nvgpu-drm-compat-32): the 32-bit one goes through
 * the node's compat_ioctl too. Prints "PASS|FAIL <what>" per check and exits
 * with the number of failures; 77 when the node serves no syncobjs (a
 * backend without fences), which is not this test's to judge.
 */
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/* The structs as the kernel's uapi has them now (drm.h), by hand: the point
 * is their sizes, not whatever the build's headers say. */
struct syncobj_create {
  uint32_t handle;
  uint32_t flags;
};
struct syncobj_handle { /* 24 bytes; 16 before `point` */
  uint32_t handle;
  uint32_t flags;
  int32_t fd;
  uint32_t pad;
  int64_t point;
};
struct syncobj_wait { /* 40 bytes; 32 before `deadline_nsec` */
  uint64_t handles;
  int64_t timeout_nsec;
  uint32_t count_handles;
  uint32_t flags;
  uint32_t first_signaled;
  uint32_t pad;
  uint64_t deadline_nsec;
};
struct syncobj_timeline_wait { /* 48 bytes; 40 before `deadline_nsec` */
  uint64_t handles;
  uint64_t points;
  int64_t timeout_nsec;
  uint32_t count_handles;
  uint32_t flags;
  uint32_t first_signaled;
  uint32_t pad;
  uint64_t deadline_nsec;
};
struct syncobj_array {
  uint64_t handles;
  uint32_t count_handles;
  uint32_t pad;
};
struct drm_version_native {
  int version_major, version_minor, version_patchlevel;
  size_t name_len;
  char *name;
  size_t date_len;
  char *date;
  size_t desc_len;
  char *desc;
};

_Static_assert(sizeof(struct syncobj_handle) == 24, "syncobj_handle");
_Static_assert(sizeof(struct syncobj_wait) == 40, "syncobj_wait");
_Static_assert(sizeof(struct syncobj_timeline_wait) == 48, "timeline_wait");

#define D(dir, nr, size) _IOC(dir, 'd', nr, size)
#define RW (_IOC_READ | _IOC_WRITE)
#define NR_CREATE 0xbf
#define NR_DESTROY 0xc0
#define NR_H2FD 0xc1
#define NR_FD2H 0xc2
#define NR_WAIT 0xc3
#define NR_RESET 0xc4
#define NR_SIGNAL 0xc5
#define NR_TLWAIT 0xca

#define CREATE_SIGNALED 1u
#define EXPORT_SYNC_FILE 1u
#define IMPORT_SYNC_FILE 1u

static int fails;
static void check(int ok, const char *fmt, ...) __attribute__((format(printf, 2, 3)));
static void check(int ok, const char *fmt, ...) {
  va_list ap;

  printf("%s ", ok ? "PASS" : "FAIL");
  va_start(ap, fmt);
  vprintf(fmt, ap);
  va_end(ap);
  putchar('\n');
  fflush(stdout);
  fails += !ok;
}

static int dfd;

/* ioctl with the argument `arg` of `size` bytes: 0 or -errno. */
static int io(unsigned int dir, unsigned int nr, unsigned int size, void *arg) {
  return ioctl(dfd, D(dir, nr, size), arg) ? -errno : 0;
}

static int create(uint32_t flags, uint32_t *h) {
  struct syncobj_create c = {.flags = flags};
  int r = io(RW, NR_CREATE, sizeof(c), &c);

  *h = c.handle;
  return r;
}

static void destroy(uint32_t h) {
  uint32_t d[2] = {h, 0};

  io(RW, NR_DESTROY, sizeof(d), d);
}

static int arr(unsigned int nr, uint32_t h) {
  struct syncobj_array a = {.handles = (uintptr_t)&h, .count_handles = 1};

  return io(RW, nr, sizeof(a), &a);
}

/* DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT: without it the kernel answers
 * -EINVAL for a syncobj that holds no fence yet (a reset one), not -ETIME
 * (drm_syncobj_array_wait_timeout); with it, "not signalled" is -ETIME. */
#define WAIT_FOR_SUBMIT (1u << 1)

/* WAIT with timeout 0 on one handle, at `size` bytes: 0, -ETIME, ... */
static int poll_wait(uint32_t h, unsigned int size, uint32_t *first) {
  struct {
    struct syncobj_wait w;
    uint8_t canary[8];
  } b;
  int r;

  memset(&b, 0xa5, sizeof(b));
  memset(&b.w, 0, size < sizeof(b.w) ? size : sizeof(b.w));
  b.w.handles = (uintptr_t)&h;
  b.w.count_handles = 1;
  b.w.flags = WAIT_FOR_SUBMIT;
  b.w.first_signaled = 0x55;
  r = io(RW, NR_WAIT, size, &b);
  if (size <= sizeof(b.w) - 8 && b.w.deadline_nsec != 0xa5a5a5a5a5a5a5a5ull)
    check(0, "SYNCOBJ_WAIT(%u) wrote past its struct", size);
  *first = b.w.first_signaled;
  return r;
}

static int poll_tlwait(uint32_t h, unsigned int size) {
  uint64_t point = 0;
  struct {
    struct syncobj_timeline_wait w;
    uint8_t canary[8];
  } b;
  int r;

  memset(&b, 0xa5, sizeof(b));
  memset(&b.w, 0, size < sizeof(b.w) ? size : sizeof(b.w));
  b.w.handles = (uintptr_t)&h;
  b.w.points = (uintptr_t)&point;
  b.w.count_handles = 1;
  b.w.flags = WAIT_FOR_SUBMIT;
  r = io(RW, NR_TLWAIT, size, &b);
  if (size <= sizeof(b.w) - 8 && b.w.deadline_nsec != 0xa5a5a5a5a5a5a5a5ull)
    check(0, "SYNCOBJ_TIMELINE_WAIT(%u) wrote past its struct", size);
  return r;
}

/* HANDLE_TO_FD at `size`: the fd, or -errno. The bytes past `size` (to 32)
 * are a canary that must come back as it was; so must a longer struct's
 * tail, which the kernel neither reads nor writes. */
static int h2fd(uint32_t h, uint32_t flags, unsigned int size) {
  uint8_t b[32];
  struct syncobj_handle *a = (void *)b;
  int r;

  memset(b, 0xa5, sizeof(b));
  memset(a, 0, size < sizeof(*a) ? size : sizeof(*a));
  a->handle = h;
  a->flags = flags;
  a->fd = -1;
  r = io(RW, NR_H2FD, size, b);
  for (unsigned int i = size < 24 ? size : 24; i < sizeof(b); i++) {
    if (b[i] != 0xa5) {
      check(0, "SYNCOBJ_HANDLE_TO_FD(%u) changed byte %u past its struct", size, i);
      break;
    }
  }
  return r ? r : a->fd;
}

static int fd2h(int fd, uint32_t h, uint32_t flags, unsigned int size,
                uint32_t *out) {
  uint8_t b[32];
  struct syncobj_handle *a = (void *)b;
  int r;

  memset(b, 0xa5, sizeof(b));
  memset(a, 0, size < sizeof(*a) ? size : sizeof(*a));
  a->handle = h;
  a->flags = flags;
  a->fd = fd;
  r = io(RW, NR_FD2H, size, b);
  for (unsigned int i = size < 24 ? size : 24; i < sizeof(b); i++) {
    if (b[i] != 0xa5) {
      check(0, "SYNCOBJ_FD_TO_HANDLE(%u) changed byte %u past its struct", size, i);
      break;
    }
  }
  *out = a->handle;
  return r;
}

static void kind(int fd, char *out, size_t n) {
  char p[64];
  ssize_t l;

  snprintf(p, sizeof(p), "/proc/self/fd/%d", fd);
  l = readlink(p, out, n - 1);
  out[l < 0 ? 0 : l] = 0;
}

int main(int argc, char **argv) {
  const char *node = argc > 1 ? argv[1] : "/dev/dri/renderD128";
  static const unsigned int sizes[] = {24, 16, 32};
  char name[64] = "", k0[128], k1[128];
  struct drm_version_native v = {.name = name, .name_len = sizeof(name) - 1};
  uint32_t h, x, first;
  int fds[3], r, r2;

  printf("nvgpu-drm-compat: %zu-bit, %s\n", sizeof(void *) * 8, node);
  dfd = open(node, O_RDWR | O_CLOEXEC);
  if (dfd < 0) {
    printf("FAIL open %s: %s\n", node, strerror(errno));
    return 1;
  }
  /* The core's own: a 32-bit caller's layout differs, drm_compat_ioctl()
   * converts it. */
  r = ioctl(dfd, _IOWR('d', 0x00, struct drm_version_native), &v);
  check(r == 0 && !strcmp(name, "nvidia-drm"), "DRM_IOCTL_VERSION: \"%s\" (%s)",
        name, r ? strerror(errno) : "ok");

  r = create(CREATE_SIGNALED, &h);
  if (r == -EOPNOTSUPP || r == -ENODEV || r == -ENOTTY) {
    printf("SKIP no syncobjs on %s: %s\n", node, strerror(-r));
    return 77;
  }
  check(r == 0 && h, "SYNCOBJ_CREATE (signalled): handle %u", h);

  /* HANDLE_TO_FD: native, older, longer -- all a syncobj file, alike. */
  for (int i = 0; i < 3; i++) {
    fds[i] = h2fd(h, 0, sizes[i]);
    check(fds[i] >= 0, "SYNCOBJ_HANDLE_TO_FD(%u bytes): fd %d%s%s", sizes[i],
          fds[i], fds[i] < 0 ? " " : "", fds[i] < 0 ? strerror(-fds[i]) : "");
  }
  if (fds[0] >= 0) {
    kind(fds[0], k0, sizeof(k0));
    for (int i = 1; i < 3; i++) {
      if (fds[i] < 0)
        continue;
      kind(fds[i], k1, sizeof(k1));
      check(!strcmp(k0, k1), "HANDLE_TO_FD(%u) gives what (24) gives: %s / %s",
            sizes[i], k1, k0);
    }
  }

  /* FD_TO_HANDLE: each file back as a handle to the same syncobj, which
   * RESET and SIGNAL on the original show. */
  for (int i = 0; i < 3; i++) {
    uint32_t hi = 0;

    if (fds[i] < 0)
      continue;
    r = fd2h(fds[i], 0, 0, sizes[i], &hi);
    check(r == 0 && hi && hi != h, "SYNCOBJ_FD_TO_HANDLE(%u bytes): handle %u (%s)",
          sizes[i], hi, r ? strerror(-r) : "ok");
    if (r)
      continue;
    arr(NR_RESET, h);
    r = poll_wait(hi, 40, &first);
    r2 = poll_wait(hi, 32, &first);
    check(r == -ETIME && r2 == -ETIME,
          "  imported handle is the same syncobj: unsignalled after RESET "
          "(WAIT 40: %d, WAIT 32: %d)", r, r2);
    arr(NR_SIGNAL, h);
    r = poll_wait(hi, 40, &first);
    r2 = poll_wait(hi, 32, &first);
    check(r == 0 && r2 == 0, "  ... and signalled after SIGNAL (WAIT 40: %d, WAIT 32: %d)",
          r, r2);
    destroy(hi);
    close(fds[i]);
  }

  /* EXPORT_SYNC_FILE / IMPORT_SYNC_FILE through the older struct. */
  arr(NR_SIGNAL, h);
  for (int i = 0; i < 3; i++) {
    int sf = h2fd(h, EXPORT_SYNC_FILE, sizes[i]);
    struct pollfd p = {.fd = sf, .events = POLLIN};

    check(sf >= 0 && poll(&p, 1, 1000) == 1,
          "HANDLE_TO_FD(%u bytes, EXPORT_SYNC_FILE): a signalled sync_file (fd %d)",
          sizes[i], sf);
    if (sf < 0)
      continue;
    if (create(0, &x) == 0) {
      r = fd2h(sf, x, IMPORT_SYNC_FILE, sizes[i], &x);
      check(r == 0 && poll_wait(x, 40, &first) == 0,
            "FD_TO_HANDLE(%u bytes, IMPORT_SYNC_FILE): the syncobj is signalled (%d)",
            sizes[i], r);
      destroy(x);
    }
    close(sf);
  }

  /* The same refusal at every size. */
  r = h2fd(0xdead, 0, 24);
  for (int i = 1; i < 3; i++) {
    r2 = h2fd(0xdead, 0, sizes[i]);
    check(r < 0 && r2 == r, "HANDLE_TO_FD of no handle: %d at %u bytes, %d at 24",
          r2, sizes[i], r);
  }

  /* SYNCOBJ_WAIT and TIMELINE_WAIT from before deadline_nsec. */
  arr(NR_RESET, h);
  check(poll_wait(h, 40, &first) == -ETIME && poll_wait(h, 32, &first) == -ETIME,
        "SYNCOBJ_WAIT (40 and 32 bytes), unsignalled: -ETIME both");
  check(poll_tlwait(h, 48) == -ETIME && poll_tlwait(h, 40) == -ETIME,
        "SYNCOBJ_TIMELINE_WAIT (48 and 40 bytes), unsignalled: -ETIME both");
  arr(NR_SIGNAL, h);
  r = poll_wait(h, 40, &first);
  check(r == 0 && first == 0, "SYNCOBJ_WAIT (40 bytes), signalled: %d, first %u", r, first);
  r = poll_wait(h, 32, &first);
  check(r == 0 && first == 0, "SYNCOBJ_WAIT (32 bytes), signalled: %d, first %u", r, first);
  check(poll_tlwait(h, 48) == 0 && poll_tlwait(h, 40) == 0,
        "SYNCOBJ_TIMELINE_WAIT (48 and 40 bytes), signalled: 0 both");

  destroy(h);
  printf("nvgpu-drm-compat: %d failed\n", fails);
  return fails > 76 ? 76 : fails;
}
