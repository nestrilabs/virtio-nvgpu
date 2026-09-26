// SPDX-License-Identifier: GPL-2.0-only
/*
 * The C around the Rust parsers (NVGPU_RUST=1): nvgpu.h's IOCTL2 API on top
 * of nvgpu_rs.rs, and the services the Rust calls back (nvgpu_rs.h). Nothing
 * here reads what a guest process wrote: it copies bytes, allocates, sends,
 * looks up the module's own tables, and calls the hooks -- the decisions are
 * the Rust's.
 */

#include <linux/compat.h>
#include <linux/mm.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/time.h>
#include <linux/uaccess.h>

#include "gen/nvgpu_rm_deep.h"
#include "gen/nvgpu_rmalloc_classes.h"
#include "gen/nvgpu_v1v2_rewrites.h"
#include "nvgpu.h"
#include "nvgpu_rs.h"

/* ───────── memory ───────── */

int nvgpu_rs_copy_from(bool kernel, void *dst, u64 src, size_t len) {
  if (!len)
    return 0;
  /*
   * A call the driver makes itself, on memory it built: the caller wrote
   * every address in it. One in the user range is a user's pointer copied
   * along and not replaced, which a memcpy would follow: refused.
   */
  if (kernel) {
    if (access_ok(u64_to_user_ptr(src), len))
      return -EFAULT;
    memcpy(dst, (const void *)(uintptr_t)src, len);
    return 0;
  }
  return copy_from_user(dst, u64_to_user_ptr(src), len) ? -EFAULT : 0;
}

int nvgpu_rs_copy_to(bool kernel, u64 dst, const void *src, size_t len) {
  if (!len)
    return 0;
  if (kernel) {
    if (access_ok(u64_to_user_ptr(dst), len))
      return -EFAULT;
    memcpy((void *)(uintptr_t)dst, src, len);
    return 0;
  }
  return copy_to_user(u64_to_user_ptr(dst), src, len) ? -EFAULT : 0;
}

void *nvgpu_rs_kvzalloc(size_t len) { return kvzalloc(len, GFP_KERNEL); }

void nvgpu_rs_kvfree(void *p) { kvfree(p); }

/* ───────── the protocol-v1 paths ───────── */

u32 nvgpu_rs_caps(struct nvgpu_fd *nfd) {
  struct nvgpu_device *dev = nfd->dev;
  u32 caps = 0;

  if (dev->v2 && (dev->backend_caps & NVGPU_BCAP_DEEP_SEGS))
    caps |= NVGPU_RS_CAP_DEEP_SEGS;
  if (nvgpu_proc_ids(dev))
    caps |= NVGPU_RS_CAP_PROC_IDS;
  if (nvgpu_proc_euid(dev))
    caps |= NVGPU_RS_CAP_PROC_EUID;
  if (nvgpu_osdesc_ok(dev))
    caps |= NVGPU_RS_CAP_OS_DESC;
  return caps;
}

u32 nvgpu_rs_fd_handle(struct nvgpu_fd *nfd) { return nfd->handle; }

int nvgpu_rs_handle_for_fd(struct nvgpu_fd *nfd, int fd, u32 *handle) {
  return nvgpu_handle_for_fd(nfd->dev, fd, handle);
}

int nvgpu_rs_send_recv(struct nvgpu_fd *nfd, const void *req, size_t req_len,
                       void *resp, size_t resp_len, u32 *used) {
  if (req_len > INT_MAX || resp_len > INT_MAX)
    return -E2BIG;
  return nvgpu_send_recv_used(nfd->dev, (void *)req, (int)req_len, resp,
                              (int)resp_len, used);
}

void nvgpu_rs_proc_id(struct nvgpu_fd *nfd, void *dst) {
  nvgpu_proc_id_fill(nfd->dev, dst);
}

const char *nvgpu_rs_driver_version(struct nvgpu_fd *nfd, size_t *len) {
  *len = strnlen(nfd->dev->driver_version, sizeof(nfd->dev->driver_version));
  return nfd->dev->driver_version;
}

bool nvgpu_rs_clock_to_guest(struct nvgpu_fd *nfd, u32 raw, s64 host_ns,
                             s64 *guest_ns) {
  return nvgpu_host_clock_to_guest(nfd->dev,
                                   raw ? CLOCK_MONOTONIC_RAW : CLOCK_REALTIME,
                                   host_ns, guest_ns);
}

static void nvgpu_rs_deep_copy(const struct nvgpu_rm_deep_control *c,
                               struct nvgpu_rs_deep_control *out) {
  unsigned int i, j;

  memset(out, 0, sizeof(*out));
  out->cmd = c->cmd;
  out->nptrs = c->nptrs;
  for (i = 0; i < NVGPU_RM_DEEP_PTRS_MAX; i++) {
    const struct nvgpu_rm_deep_ptr *p = &c->ptrs[i];

    out->ptrs[i].ptr = p->ptr;
    out->ptrs[i].flags = p->flags;
    out->ptrs[i].ncounts = p->ncounts;
    for (j = 0; j < NVGPU_RM_DEEP_COUNTS_MAX; j++) {
      out->ptrs[i].count_off[j] = p->counts[j].offset;
      out->ptrs[i].count_width[j] = p->counts[j].width;
    }
    out->ptrs[i].scale = p->scale;
    out->ptrs[i].elem = p->elem;
  }
}

bool nvgpu_rs_rm_deep(u32 cmd, bool idle_channels,
                      struct nvgpu_rs_deep_control *out) {
  const struct nvgpu_rm_deep_control *c =
      idle_channels ? &nvgpu_rm_deep_idle_channels : nvgpu_rm_deep_find(cmd);

  if (!c)
    return false;
  nvgpu_rs_deep_copy(c, out);
  return true;
}

bool nvgpu_rs_v1v2(u32 cmd, u32 *userptr_offset, bool *info_style) {
  const struct nvgpu_v1v2_entry *rw = nvgpu_find_v1v2_rewrite(cmd);

  if (!rw)
    return false;
  *userptr_offset = rw->v1_userptr_offset;
  *info_style = rw->info_style;
  return true;
}

u32 nvgpu_rs_rmalloc_size(u32 hclass) {
  return nvgpu_rmalloc_class_param_size(hclass);
}

bool nvgpu_rs_fd_translation(struct nvgpu_fd *nfd, u32 key, u32 *payload) {
  struct nvgpu_device *dev = nfd->dev;
  u32 i;

  for (i = 0; i < dev->num_fd_translations; i++) {
    if (le32_to_cpu(dev->fd_translations[i].nr) == key) {
      *payload = le32_to_cpu(dev->fd_translations[i].payload_offset);
      return true;
    }
  }
  return false;
}

int nvgpu_rs_uvm_size(struct nvgpu_fd *nfd, u32 cmd) {
  const struct nvgpu_uvm_table *t = nfd->dev->uvm;
  u32 i;

  for (i = 0; t && i < t->ncmds; i++)
    if (t->cmds[i].cmd == cmd)
      return t->cmds[i].size;
  return -1;
}

void nvgpu_rs_warn(struct nvgpu_fd *nfd, u32 code, u64 a, u64 b) {
  struct device *d = &nfd->dev->vdev->dev;

  switch (code) {
  case NVGPU_RS_WARN_CONTROL_FD:
    dev_dbg_ratelimited(d,
                        "virtio-gpu-nv: RM control 0x%x names fd %d, "
                        "which is not one of our devices\n",
                        (u32)a, (s32)b);
    break;
  case NVGPU_RS_WARN_CONTROL_OS_EVENT:
    dev_dbg_ratelimited(d,
                        "virtio-gpu-nv: RM control 0x%x names OS event "
                        "0x%llx, which is not one of our devices\n",
                        (u32)a, b);
    break;
  case NVGPU_RS_WARN_ALLOC_EVENT_FD:
    dev_dbg_ratelimited(d,
                        "virtio-gpu-nv: RM_ALLOC of event class 0x%x "
                        "names fd %d, which is not one of our "
                        "devices\n",
                        (u32)a, (s32)b);
    break;
  case NVGPU_RS_WARN_EVENT_BUFFER:
    dev_dbg_ratelimited(d,
                        "virtio-gpu-nv: NV_EVENT_BUFFER names OS event "
                        "0x%llx, which is not one of our devices\n",
                        b);
    break;
  case NVGPU_RS_WARN_SURFACE_FD:
    dev_dbg_ratelimited(
        d,
        "virtio-gpu-nv: REGISTER_SURFACE names fd %d, which is not one "
        "of ours\n",
        (s32)b);
    break;
  }
}

void nvgpu_rs_osdesc_reap(struct nvgpu_fd *nfd) { nvgpu_osdesc_reap(nfd->dev); }

int nvgpu_rs_osdesc_pin(u64 start, u64 npages, bool write, void **out) {
  struct page **pages = kvmalloc_array(npages, sizeof(*pages), GFP_KERNEL);

  if (!pages)
    return NVGPU_RS_PIN_NOMEM;
  if (nvgpu_osdesc_pin(start, npages, write, pages)) {
    kvfree(pages);
    return NVGPU_RS_PIN_FAILED;
  }
  *out = pages;
  return NVGPU_RS_PIN_OK;
}

u64 nvgpu_rs_page_phys(void *pages, u64 i) {
  return page_to_phys(((struct page **)pages)[i]);
}

void nvgpu_rs_osdesc_keep(struct nvgpu_fd *nfd, u64 id, void *pages,
                          u64 npages, bool write) {
  nvgpu_osdesc_keep(nfd->dev, id, pages, npages, write);
}

void nvgpu_rs_osdesc_unpin(void *pages, u64 npages, bool write) {
  nvgpu_osdesc_unpin(pages, npages, write);
}

int nvgpu_rs_osdesc_send(struct nvgpu_fd *nfd, const void *req,
                         size_t req_len, void *resp, size_t resp_len, u32 *used,
                         void *pages, u64 npages, bool write) {
  if (req_len > INT_MAX || resp_len > INT_MAX)
    return -E2BIG;
  return nvgpu_osdesc_send(nfd->dev, (void *)req, (int)req_len, resp,
                           (int)resp_len, used, pages, npages, write);
}

/* ───────── IOCTL2 ───────── */

/* What nvgpu_i2_hold() was given, released with the request (S-25). */
struct nvgpu_rs_held {
  u32 n;
  struct {
    void (*put)(void *obj);
    void *obj;
  } e[NVGPU_I2_MAX_RECS];
};

void nvgpu_rs_held_release(void *arg) {
  struct nvgpu_rs_held *h = arg;
  u32 i;

  if (!h)
    return;
  for (i = 0; i < h->n; i++)
    h->e[i].put(h->e[i].obj);
  kfree(h);
}

int nvgpu_i2_hold(struct nvgpu_i2_call *call, void (*put)(void *obj),
                  void *obj) {
  void **slot = call->st ? nvgpu_rs_i2_held(call->st) : NULL;
  struct nvgpu_rs_held *h;

  if (!slot) {
    put(obj);
    return -EINVAL;
  }
  h = *slot;
  if (!h)
    h = *slot = kzalloc(sizeof(*h), GFP_KERNEL);
  if (!h || h->n >= NVGPU_I2_MAX_RECS) {
    put(obj);
    return h ? -E2BIG : -ENOMEM;
  }
  h->e[h->n].put = put;
  h->e[h->n].obj = obj;
  h->n++;
  return 0;
}

void nvgpu_rs_i2_hand_over(void *req, void **held) {
  if (!*held)
    return;
  nvgpu_tbuf_on_free(req, nvgpu_rs_held_release, *held);
  *held = NULL;
}

static void nvgpu_rs_tables_fill(struct nvgpu_device *dev,
                                 struct nvgpu_rs_tables *t) {
  const struct nvgpu_schema_set *set =
      dev->schema ? dev->schema : nvgpu_schema_default();
  const struct nvgpu_stable *m = set->modeset;

  memset(t, 0, sizeof(*t));
  t->drm = (struct nvgpu_rs_table){
      .ioctls = set->drm->ioctls, .nioctls = set->drm->nioctls,
      .fields = set->drm->fields, .nfields = set->drm->nfields,
      .planes = set->drm->planes, .nplanes = set->drm->nplanes};
  if (m) {
    t->modeset = (struct nvgpu_rs_table){
        .ioctls = m->ioctls, .nioctls = m->nioctls,
        .fields = m->fields, .nfields = m->nfields,
        .planes = m->planes, .nplanes = m->nplanes};
    t->has_modeset = 1;
  }
}

bool nvgpu_i2_has_schema(struct nvgpu_device *dev, u32 sclass,
                         unsigned int cmd, const void *arg_prefix,
                         size_t prefix_len) {
  struct nvgpu_rs_tables t;

  nvgpu_rs_tables_fill(dev, &t);
  return nvgpu_rs_i2_has_schema(&t, sclass, cmd, arg_prefix,
                                arg_prefix ? prefix_len : 0);
}

long nvgpu_i2_ioctl(struct nvgpu_i2_call *call) {
  struct nvgpu_device *dev = call->dev;
  struct nvgpu_rs_i2_args a = {
      .uarg = (u64)(uintptr_t)call->uarg,
      .max_req = dev->max_req,
      .max_resp = dev->max_resp,
      .sclass = call->sclass,
      .cmd = call->cmd,
      .handle = call->handle,
      .render = call->render,
      .xflags = call->xflags,
      .compat = in_compat_syscall(),
      .kernel = call->kernel,
  };
  struct nvgpu_rs_tables t;
  s32 ret_out = 0;
  long ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  nvgpu_rs_tables_fill(dev, &t);
  call->ret = 0;
  ret = nvgpu_rs_i2_ioctl(call, &a, &t, &ret_out);
  call->ret = ret_out;
  call->st = NULL;
  return ret;
}

int nvgpu_i2_add_dyn(struct nvgpu_i2_call *call, u32 kind, u32 buf, u32 off,
                     u32 len) {
  return nvgpu_rs_i2_add_dyn(call->st, kind, buf, off, len);
}

int nvgpu_i2_add_fd(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
                    u32 flags) {
  return nvgpu_rs_i2_add_fd(call->st, buf, off, handle, flags);
}

void *nvgpu_i2_buf(struct nvgpu_i2_call *call, u32 buf, u32 *len) {
  return nvgpu_rs_i2_buf(call->st, buf, len);
}

/*
 * The hooks. Each sees call->st as the state it is handed (the pointer is
 * only valid while it runs) and call->ret as the interpreter has it; what
 * the hook leaves in call->ret goes back.
 */
#define NVGPU_RS_HOOK(c_, s_, r_, expr)                                        \
  ({                                                                           \
    int __r;                                                                   \
    (c_)->st = (s_);                                                           \
    (c_)->ret = *(r_);                                                         \
    __r = (expr);                                                              \
    *(r_) = (c_)->ret;                                                         \
    __r;                                                                       \
  })

int nvgpu_rs_i2_fd_in(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                      u32 off, s64 value, u32 kinds, u32 *handle, u32 *flags) {
  if (!call->ops || !call->ops->fd_in)
    return -EINVAL;
  return NVGPU_RS_HOOK(call, st, ret,
                       call->ops->fd_in(call, buf, off, value, kinds, handle,
                                        flags));
}

int nvgpu_rs_i2_gem_in(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                       u32 off, u32 guest, u32 *owner, u32 *gem) {
  if (!call->ops || !call->ops->gem_in)
    return -EINVAL;
  return NVGPU_RS_HOOK(call, st, ret,
                       call->ops->gem_in(call, buf, off, guest, owner, gem));
}

int nvgpu_rs_i2_fd_out(struct nvgpu_i2_call *call, void *st, s32 *ret, u32 buf,
                       u32 off, u32 handle, u32 kind, s64 *value) {
  if (!call->ops || !call->ops->fd_out)
    return -EINVAL;
  return NVGPU_RS_HOOK(call, st, ret,
                       call->ops->fd_out(call, buf, off, handle, kind, value));
}

int nvgpu_rs_i2_gem_out(struct nvgpu_i2_call *call, void *st, s32 *ret,
                        u32 buf, u32 off, u32 gem, u64 size, u32 *guest) {
  if (!call->ops || !call->ops->gem_out)
    return -EINVAL;
  return NVGPU_RS_HOOK(call, st, ret,
                       call->ops->gem_out(call, buf, off, gem, size, guest));
}

int nvgpu_rs_i2_special(struct nvgpu_i2_call *call, void *st, s32 *ret,
                        u32 id, int phase) {
  if (!call->ops || !call->ops->special)
    return 0;
  return NVGPU_RS_HOOK(call, st, ret, call->ops->special(call, id, phase));
}

int nvgpu_rs_i2_phase(struct nvgpu_i2_call *call, void *st, s32 *ret,
                      int phase) {
  if (!call->ops || !call->ops->phase)
    return 0;
  return NVGPU_RS_HOOK(call, st, ret, call->ops->phase(call, phase));
}

void nvgpu_rs_i2_close(struct nvgpu_i2_call *call, u32 handle) {
  nvgpu_close_handle_async(call->dev, handle);
}

void nvgpu_rs_i2_gem_close(struct nvgpu_i2_call *call, u32 gem) {
  nvgpu_gem_close_async(call->dev, call->render, gem);
}

void nvgpu_rs_i2_warn(struct nvgpu_i2_call *call, u32 code, const char *name,
                      u32 a, u32 b, u32 c) {
  struct device *d = &call->dev->vdev->dev;

  switch (code) {
  case NVGPU_RS_WARN_I2_CMD:
    dev_warn_ratelimited(d,
                         "virtio-gpu-nv: IOCTL2 %s: caller's ioctl 0x%08x "
                         "is not 0x%08x (size %u)\n",
                         name, a, b, c);
    break;
  case NVGPU_RS_WARN_I2_MALFORMED:
    dev_warn_ratelimited(d,
                         "virtio-gpu-nv: IOCTL2 %s: malformed reply "
                         "(%u bytes, %u fds, %u GEM handles)\n",
                         name, a, b, c);
    break;
  case NVGPU_RS_WARN_I2_UNNAMED:
    dev_warn_ratelimited(d,
                         "virtio-gpu-nv: IOCTL2 %s: the reply names a "
                         "descriptor or GEM handle where the schema has none\n",
                         name);
    break;
  }
}

void *nvgpu_rs_tbuf_alloc(size_t len) {
  return nvgpu_tbuf_alloc(len, GFP_KERNEL);
}

void nvgpu_rs_tbuf_free(void *tb) { nvgpu_tbuf_free(tb); }

int nvgpu_rs_tbuf_write(void *tb, size_t off, const void *src, size_t len) {
  return nvgpu_tbuf_write(tb, off, src, len);
}

int nvgpu_rs_tbuf_read(const void *tb, size_t off, void *dst, size_t len) {
  return nvgpu_tbuf_read(tb, off, dst, len);
}

int nvgpu_rs_i2_xfer(struct nvgpu_i2_call *call, void *req, void *resp,
                     u32 flags, u32 *used) {
  return nvgpu_xfer(call->dev, req, resp, flags, used);
}

/* ───────── ATOMIC ───────── */

static_assert(sizeof(struct nvgpu_atomic_out) == 8, "nvgpu_atomic_out");
static_assert(offsetof(struct nvgpu_atomic_out, values_buf) == 4,
              "nvgpu_atomic_out.values_buf");
static_assert(sizeof(struct nvgpu_atomic_ops) == 6 * sizeof(void *),
              "nvgpu_atomic_ops");

int nvgpu_atomic_parse(struct nvgpu_i2_call *call, bool fences,
                       const struct nvgpu_atomic_ops *ops, void *ctx,
                       struct nvgpu_atomic_out *out) {
  return nvgpu_rs_atomic_parse(call->st, fences, ops, ctx, out);
}
