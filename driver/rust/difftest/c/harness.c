// SPDX-License-Identifier: GPL-2.0
/*
 * The kernel around the module's C parsers, for the differential test: every
 * service nvgpu_i2.c and nvgpu_rmio.c call (the caller's memory, allocation,
 * the transport, the descriptor table, pinning, the IOCTL2 hooks) forwards to
 * the Rust test's world (src/lib.rs, the dt_* functions), which serves the
 * Rust core the same way. Allocations carry a canary, checked on free, so a C
 * write past the end of what it allocated fails the test.
 */

#include <stdlib.h>
#include <string.h>

#include "nvgpu.h"

#include "gen/nvgpu_rm_deep.h"
#include "gen/nvgpu_rmalloc_classes.h"
#include "gen/nvgpu_schema.h"
#include "gen/nvgpu_v1v2_rewrites.h"

/* ── the Rust world (src/lib.rs) ── */
int dt_copy_from_user(void *to, u64 from, size_t n);
int dt_copy_to_user(u64 to, const void *from, size_t n);
int dt_send_recv(const void *req, size_t req_len, void *resp, size_t resp_len,
                 u32 *used);
int dt_handle_for_fd(int fd, u32 *handle);
void dt_proc_id(void *dst);
bool dt_clock(bool raw, s64 host_ns, s64 *guest_ns);
void dt_reap(void);
int dt_pin(u64 start, u64 npages, bool write, u64 *phys);
void dt_keep(u64 id, u64 npages, bool write);
void dt_unpin(u64 npages, bool write);
void dt_warn(const char *fmt);
bool dt_compat(void);
int dt_xfer(const void *req, size_t req_len, void *resp, size_t resp_len,
            u32 flags, u32 *used);
void dt_close(u32 handle);
void dt_gem_close(u32 file, u32 gem);
void dt_canary(void);
int dt_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off, s64 v, u32 kinds,
             u32 *handle, u32 *flags);
int dt_gem_in(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 guest,
              u32 *owner, u32 *gem);
int dt_fd_out(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
              u32 kind, s64 *v);
int dt_gem_out(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 gem,
               u64 size, u32 *guest);
int dt_special(struct nvgpu_i2_call *call, u32 id, int phase);
int dt_phase(struct nvgpu_i2_call *call, int phase);

/* ── allocation, with a canary behind every block ── */

#define CANARY 64
struct hdr {
  size_t n;
  u64 pad;
};

static void *alloc_fill(size_t n, int fill) {
  struct hdr *h = malloc(sizeof(*h) + n + CANARY);

  if (!h)
    return NULL;
  h->n = n;
  memset(h + 1, fill, n);
  memset((u8 *)(h + 1) + n, 0x5c, CANARY);
  return h + 1;
}

/*
 * What kmalloc()'d memory holds: zero by default, as the Rust side's
 * buffers do, so that the comparison is of what each writes; a test that
 * wants to see what the C sends without having written it sets 0xaa.
 */
static __thread int kmalloc_fill;

void harness_set_kmalloc_fill(int fill) { kmalloc_fill = fill; }

void *harness_kmalloc(size_t n) { return alloc_fill(n, kmalloc_fill); }

void *harness_kzalloc(size_t n) { return alloc_fill(n, 0); }

void *harness_kvmalloc_array(size_t n, size_t size) {
  size_t t;

  if (__builtin_mul_overflow(n, size, &t))
    return NULL;
  return alloc_fill(t, 0xaa);
}

void harness_kfree(const void *p) {
  struct hdr *h;
  size_t i;

  if (!p)
    return;
  h = (struct hdr *)p - 1;
  for (i = 0; i < CANARY; i++)
    if (((const u8 *)p)[h->n + i] != 0x5c) {
      dt_canary();
      break;
    }
  free(h);
}

/* ── the caller's memory ── */

unsigned long harness_copy_from_user(void *to, u64 from, unsigned long n) {
  if (!n)
    return 0;
  return dt_copy_from_user(to, from, n) ? n : 0;
}

unsigned long harness_copy_to_user(u64 to, const void *from, unsigned long n) {
  if (!n)
    return 0;
  return dt_copy_to_user(to, from, n) ? n : 0;
}

void harness_warn(const char *fmt) { dt_warn(fmt); }

bool in_compat_syscall(void) { return dt_compat(); }

/* ── nvgpu_main.c ── */

int nvgpu_handle_for_fd(int guest_fd, u32 *handle) {
  if (guest_fd < 0)
    return -EBADF;
  return dt_handle_for_fd(guest_fd, handle);
}

bool nvgpu_proc_ids(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_PROC_ID);
}

bool nvgpu_proc_euid(const struct nvgpu_device *dev) {
  return nvgpu_proc_ids(dev) && (dev->backend_caps & NVGPU_BCAP_PROC_EUID);
}

void nvgpu_proc_id_fill(const struct nvgpu_device *dev, void *dst) {
  (void)dev;
  dt_proc_id(dst);
}

bool nvgpu_host_clock_to_guest(struct nvgpu_device *dev, clockid_t clk,
                               s64 host_ns, s64 *guest_ns) {
  (void)dev;
  return dt_clock(clk == CLOCK_MONOTONIC_RAW, host_ns, guest_ns);
}

/* ── the transport ── */

int nvgpu_send_recv_used(struct nvgpu_device *dev, void *req, int req_len,
                         void *resp, int resp_len, u32 *used) {
  (void)dev;
  /* nvgpu_send_recv_used(): the reply reads as zero past what the device
   * wrote. */
  memset(resp, 0, resp_len);
  return dt_send_recv(req, req_len, resp, resp_len, used);
}

struct nvgpu_tbuf {
  size_t len;
  u8 *d;
  void (*release)(void *arg);
  void *arg;
};

struct nvgpu_tbuf *nvgpu_tbuf_alloc(size_t len, gfp_t gfp) {
  struct nvgpu_tbuf *tb;

  (void)gfp;
  if (!len)
    return NULL;
  tb = harness_kzalloc(sizeof(*tb));
  if (!tb)
    return NULL;
  tb->d = harness_kzalloc(len);
  if (!tb->d) {
    harness_kfree(tb);
    return NULL;
  }
  tb->len = len;
  return tb;
}

void nvgpu_tbuf_free(struct nvgpu_tbuf *tb) {
  if (!tb)
    return;
  if (tb->release)
    tb->release(tb->arg);
  harness_kfree(tb->d);
  harness_kfree(tb);
}

void nvgpu_tbuf_on_free(struct nvgpu_tbuf *tb, void (*fn)(void *arg),
                        void *arg) {
  tb->release = fn;
  tb->arg = arg;
}

size_t nvgpu_tbuf_len(const struct nvgpu_tbuf *tb) { return tb->len; }

int nvgpu_tbuf_write(struct nvgpu_tbuf *tb, size_t off, const void *src,
                     size_t len) {
  if (len > tb->len || off > tb->len - len)
    return -EINVAL;
  if (len)
    memcpy(tb->d + off, src, len);
  return 0;
}

int nvgpu_tbuf_read(const struct nvgpu_tbuf *tb, size_t off, void *dst,
                    size_t len) {
  if (len > tb->len || off > tb->len - len)
    return -EINVAL;
  if (len)
    memcpy(dst, tb->d + off, len);
  return 0;
}

int nvgpu_xfer(struct nvgpu_device *dev, struct nvgpu_tbuf *req,
               struct nvgpu_tbuf *resp, u32 flags, u32 *used) {
  int r;

  (void)dev;
  memset(resp->d, 0, resp->len);
  r = dt_xfer(req->d, req->len, resp->d, resp->len, flags, used);
  if (r == -ETIMEDOUT || r == -EINTR) {
    /* The transport's now; the late reply is not the test's business. */
    nvgpu_tbuf_free(req);
    nvgpu_tbuf_free(resp);
  }
  return r;
}

void nvgpu_close_handle_async(struct nvgpu_device *dev, u32 handle) {
  (void)dev;
  dt_close(handle);
}

void nvgpu_gem_close_async(struct nvgpu_device *dev, u32 file_handle,
                           u32 gem) {
  (void)dev;
  dt_gem_close(file_handle, gem);
}

/* ── nvgpu_osdesc.c ── */

bool nvgpu_osdesc_ok(const struct nvgpu_device *dev) {
  return dev->v2 && (dev->backend_caps & NVGPU_BCAP_OS_DESC);
}

void nvgpu_osdesc_reap(struct nvgpu_device *dev) {
  (void)dev;
  dt_reap();
}

int nvgpu_osdesc_pin(unsigned long start, unsigned long npages, bool write,
                     struct page **pages) {
  return dt_pin(start, npages, write, (u64 *)pages);
}

void nvgpu_osdesc_keep(struct nvgpu_device *dev, u64 id, struct page **pages,
                       unsigned long npages, bool write) {
  (void)dev;
  dt_keep(id, npages, write);
  harness_kfree(pages);
}

void nvgpu_osdesc_unpin(struct page **pages, unsigned long n, bool write) {
  dt_unpin(n, write);
  harness_kfree(pages);
}

/* ── devices and calls, for the test ── */

struct nvgpu_device *harness_dev(const char *version, bool v2, u32 caps,
                                 u32 max_req, u32 max_resp,
                                 const u32 *fdt_nr, const u32 *fdt_payload,
                                 u32 nfdt) {
  struct nvgpu_device *dev = calloc(1, sizeof(*dev));
  u32 i;

  strncpy(dev->driver_version, version, sizeof(dev->driver_version) - 1);
  dev->v2 = v2;
  dev->backend_caps = caps;
  dev->max_req = max_req;
  dev->max_resp = max_resp;
  dev->schema = nvgpu_schema_select(dev->driver_version);
  dev->uvm = nvgpu_uvm_select(dev->driver_version);
  for (i = 0; i < nfdt && i < 16; i++) {
    dev->fd_translations[i].nr = fdt_nr[i];
    dev->fd_translations[i].payload_offset = fdt_payload[i];
  }
  dev->num_fd_translations = nfdt < 16 ? nfdt : 16;
  return dev;
}

void harness_dev_free(struct nvgpu_device *dev) { free(dev); }

struct nvgpu_fd *harness_fd(struct nvgpu_device *dev, u32 handle) {
  struct nvgpu_fd *nfd = calloc(1, sizeof(*nfd));

  nfd->dev = dev;
  nfd->handle = handle;
  return nfd;
}

void harness_fd_free(struct nvgpu_fd *nfd) { free(nfd); }

/* The tables, as struct nvgpu_rs_tables lays them out (nvgpu_rs.h). */
struct harness_table {
  const struct nvgpu_sioctl *ioctls;
  u64 nioctls;
  const struct nvgpu_sfield *fields;
  u64 nfields;
  const u8 *planes;
  u64 nplanes;
};

struct harness_tables {
  struct harness_table drm, modeset;
  u32 has_modeset, reserved;
};

void harness_tables(const struct nvgpu_device *dev, struct harness_tables *t) {
  const struct nvgpu_schema_set *set =
      dev->schema ? dev->schema : nvgpu_schema_default();
  const struct nvgpu_stable *m = set->modeset;

  memset(t, 0, sizeof(*t));
  t->drm = (struct harness_table){set->drm->ioctls, set->drm->nioctls,
                                  set->drm->fields, set->drm->nfields,
                                  set->drm->planes, set->drm->nplanes};
  if (m) {
    t->modeset = (struct harness_table){m->ioctls, m->nioctls, m->fields,
                                        m->nfields, m->planes, m->nplanes};
    t->has_modeset = 1;
  }
}

/* The RM tables, for the Rust world, as nvgpu_rs_glue.c hands them over. */
struct harness_deep_ptr {
  u32 ptr, flags, ncounts, count_off[2], count_width[2], scale, elem;
};
struct harness_deep {
  u32 cmd, nptrs;
  struct harness_deep_ptr ptrs[4];
};

bool harness_rm_deep(u32 cmd, bool idle, struct harness_deep *out) {
  const struct nvgpu_rm_deep_control *c =
      idle ? &nvgpu_rm_deep_idle_channels : nvgpu_rm_deep_find(cmd);
  unsigned int i, j;

  if (!c)
    return false;
  memset(out, 0, sizeof(*out));
  out->cmd = c->cmd;
  out->nptrs = c->nptrs;
  for (i = 0; i < NVGPU_RM_DEEP_PTRS_MAX; i++) {
    out->ptrs[i].ptr = c->ptrs[i].ptr;
    out->ptrs[i].flags = c->ptrs[i].flags;
    out->ptrs[i].ncounts = c->ptrs[i].ncounts;
    for (j = 0; j < NVGPU_RM_DEEP_COUNTS_MAX; j++) {
      out->ptrs[i].count_off[j] = c->ptrs[i].counts[j].offset;
      out->ptrs[i].count_width[j] = c->ptrs[i].counts[j].width;
    }
    out->ptrs[i].scale = c->ptrs[i].scale;
    out->ptrs[i].elem = c->ptrs[i].elem;
  }
  return true;
}

bool harness_v1v2(u32 cmd, u32 *off, bool *info) {
  const struct nvgpu_v1v2_entry *rw = nvgpu_find_v1v2_rewrite(cmd);

  if (!rw)
    return false;
  *off = rw->v1_userptr_offset;
  *info = rw->info_style;
  return true;
}

u32 harness_rmalloc_size(u32 hclass) {
  return nvgpu_rmalloc_class_param_size(hclass);
}

s32 harness_call_ret(struct nvgpu_i2_call *call) { return call->ret; }

void harness_set_call_ret(struct nvgpu_i2_call *call, s32 ret) {
  call->ret = ret;
}

int harness_uvm_size(const struct nvgpu_device *dev, u32 cmd) {
  const struct nvgpu_uvm_table *t = dev->uvm;
  u32 i;

  for (i = 0; t && i < t->ncmds; i++)
    if (t->cmds[i].cmd == cmd)
      return t->cmds[i].size;
  return -1;
}

/* ── the IOCTL2 hooks, and a call ── */

static int h_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off, s64 v,
                   u32 kinds, u32 *handle, u32 *flags) {
  return dt_fd_in(call, buf, off, v, kinds, handle, flags);
}

static int h_gem_in(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 guest,
                    u32 *owner, u32 *gem) {
  return dt_gem_in(call, buf, off, guest, owner, gem);
}

static int h_fd_out(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
                    u32 kind, s64 *v) {
  return dt_fd_out(call, buf, off, handle, kind, v);
}

static int h_gem_out(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 gem,
                     u64 size, u32 *guest) {
  return dt_gem_out(call, buf, off, gem, size, guest);
}

static int h_special(struct nvgpu_i2_call *call, u32 id, int phase) {
  return dt_special(call, id, phase);
}

static int h_phase(struct nvgpu_i2_call *call, int phase) {
  return dt_phase(call, phase);
}

/* Bit n of `mask` gives the call hook n: fd_in, gem_in, fd_out, gem_out,
 * special, phase. */
long harness_i2(struct nvgpu_device *dev, u32 sclass, u32 cmd, u64 uarg,
                u32 handle, u32 render, u32 xflags, bool kernel, u32 mask,
                s32 *ret_out) {
  struct nvgpu_i2_ops ops = {
      .fd_in = mask & 1 ? h_fd_in : NULL,
      .gem_in = mask & 2 ? h_gem_in : NULL,
      .fd_out = mask & 4 ? h_fd_out : NULL,
      .gem_out = mask & 8 ? h_gem_out : NULL,
      .special = mask & 16 ? h_special : NULL,
      .phase = mask & 32 ? h_phase : NULL,
  };
  struct nvgpu_i2_call call = {
      .dev = dev,
      .handle = handle,
      .render = render,
      .sclass = sclass,
      .cmd = cmd,
      .uarg = (void __user *)(uintptr_t)uarg,
      .xflags = xflags,
      .kernel = kernel,
      .ops = &ops,
  };
  long r = nvgpu_i2_ioctl(&call);

  *ret_out = call.ret;
  return r;
}

/*
 * A table the generator would refuse, for the tests: _IOWR('d', 0xf0, 16) on
 * a KMS file, with a descriptor field 2 bytes wide at 0 (the generator allows
 * 4 or 8), and _IOWR('d', 0xf1, 16) with a GEM handle 2 bytes wide.
 */
static const struct nvgpu_sfield harness_bad_fields[] = {
    {.off = 0, .kind = NVGPU_SF_FD_IN, .width = 2, .kinds = 0xff,
     .none_value = -1},
    {.off = 4, .kind = NVGPU_SF_GEM_OUT, .width = 2},
};
static const struct nvgpu_sioctl harness_bad_ioctls[] = {
    {.name = "BAD_FD", .cmd = 0xc01064f0u, .size = 16,
     .sclass = NVGPU_SCLASS_KMS, .field = 0, .nfield = 1},
    {.name = "BAD_GEM", .cmd = 0xc01064f1u, .size = 16,
     .sclass = NVGPU_SCLASS_KMS, .field = 1, .nfield = 1},
};
static const struct nvgpu_stable harness_bad_table = {
    .name = "bad", .ioctls = harness_bad_ioctls, .nioctls = 2,
    .fields = harness_bad_fields, .nfields = 2};
static const struct nvgpu_schema_set harness_bad_set = {
    .drm = &harness_bad_table};

void harness_dev_bad_schema(struct nvgpu_device *dev) {
  dev->schema = &harness_bad_set;
}

/* ── ATOMIC: the parse's hooks, forwarded to the test's world ── */

int dt_a_obj(void *ctx, u32 obj, u32 *crtc);
int dt_a_prop(void *ctx, u32 id);
int dt_a_in_fence(void *ctx, void *st, u32 buf, u32 off, s64 fd);
int dt_a_out_fence(void *ctx, void *st, u32 buf, u32 off, u64 uptr);
void dt_a_learn(void *ctx, u32 obj, u32 crtc);
int dt_a_reserve(void *ctx, u32 crtc, u64 user_data);

static const struct nvgpu_atomic_ops harness_atomic_ops = {
    .obj_class = dt_a_obj,
    .prop_class = dt_a_prop,
    .in_fence = dt_a_in_fence,
    .out_fence = dt_a_out_fence,
    .learn = dt_a_learn,
    .reserve = dt_a_reserve,
};

int harness_atomic(struct nvgpu_i2_call *call, bool fences, void *ctx,
                   bool *commit, u32 *values_buf) {
  struct nvgpu_atomic_out out = {};
  int r = nvgpu_atomic_parse(call, fences, &harness_atomic_ops, ctx, &out);

  *commit = out.commit;
  *values_buf = out.values_buf;
  return r;
}
