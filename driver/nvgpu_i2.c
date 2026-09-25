// SPDX-License-Identifier: GPL-2.0
/*
 * The schema-driven IOCTL2 interpreter: gathers a caller's buffers per the
 * generated schema (gen/nvgpu_schema.h), sends them, and scatters the reply.
 *
 * An ioctl that carries pointers cannot be forwarded as a block of bytes: the
 * pointers are addresses in this guest, and a host kernel handed them would
 * read and write the backend's own memory at those addresses. So the schema
 * names every pointer, what it reaches and how long that is, and we copy all
 * of it -- the argument and everything it points at, recursively -- into one
 * request, in the order both halves walk it (the canonical traversal,
 * gen/schema/lang.py). The backend walks its own copy of the same schema over
 * what we send and refuses anything that disagrees; nothing here is trusted
 * by it, and nothing it answers is trusted by us beyond what we can check.
 *
 * Descriptors and GEM handles are the other half. A descriptor number means
 * nothing to the host; the caller's hook turns it into the backend handle
 * that stands for it (or refuses). A GEM handle names one of our proxies,
 * which knows the host file and handle it stands for. On the way back the
 * host's new descriptors and GEM handles arrive as backend handles and host
 * handles in the file's render node, and hooks materialise them as guest fds
 * and proxies before the caller sees a number.
 *
 * What reaches the caller's memory is what the host kernel would have written
 * there: each OUT buffer's copy-back rule mirrors the kernel's own fill rule,
 * the argument is copied back even when the call failed (drm_ioctl.c:915 does
 * too), and the caller's own pointers, descriptors and handles are put back
 * where we or the backend replaced them.
 */

#include <linux/compat.h>
#include <linux/err.h>
#include <linux/kernel.h>
#include <linux/minmax.h>
#include <linux/overflow.h>
#include <linux/slab.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include "nvgpu.h"

#define NVGPU_SCHEMA_TABLES
#include "gen/nvgpu_schema.h"

/*
 * Schema positions one call may reach. Far above any real call (ATOMIC with a
 * fence on every plane is a few dozen); a bound so that a count the caller
 * made up sizes nothing but a refusal.
 */
#define NVGPU_I2_MAX_SLOTS 512

/* One buffer of the request: the kernel's copy of the caller's memory. */
struct nvgpu_i2_kbuf {
  u8 *k;   /* NULL when len == 0 */
  u32 len;
  u8 dir;  /* NVGPU_SDIR_* */
  /* The PTR field that reaches it (NULL for the argument), the buffer that
   * pointer sits in and where its struct starts there: the copy-back count
   * is that struct's, read once as sent and once as the host left it. */
  const struct nvgpu_sfield *f;
  u32 parent, pbase;
  u64 sent;
  void __user *uptr;
};

/* A schema position the walk reached, with the caller's value there. */
struct nvgpu_i2_slot {
  const struct nvgpu_sfield *f;
  u32 buf, off;
  u64 orig;
};

/* A descriptor or GEM handle the reply carries. */
struct nvgpu_i2_out {
  u32 buf, off;
  u32 handle; /* backend handle (fd) or host GEM handle in the render file */
  u32 kind;   /* fd: NVGPU_HK_*; GEM: the guest handle, once made */
  u64 size;
};

struct nvgpu_i2_state {
  const struct nvgpu_stable *t;
  const struct nvgpu_sioctl *e;
  bool kernel; /* nvgpu_i2_call.kernel: every address is a kernel one */
  u32 nbuf, nslot, nfd, ngem, ndyn, nfdo, ngemo;
  size_t in_bytes, out_bytes; /* request / reply data, padded */
  struct nvgpu_i2_kbuf buf[NVGPU_I2_MAX_BUFS];
  struct nvgpu_i2_slot slot[NVGPU_I2_MAX_SLOTS];
  struct nvgpu_i2_fd_in fd[NVGPU_I2_MAX_RECS];
  struct nvgpu_i2_gem_in gem[NVGPU_I2_MAX_RECS];
  struct nvgpu_i2_dyn dyn[NVGPU_I2_MAX_RECS];
  struct nvgpu_i2_out fdo[NVGPU_I2_MAX_RECS];
  struct nvgpu_i2_out gemo[NVGPU_I2_MAX_RECS];
};

static const u8 nvgpu_i2_zero[8];

/* The tables to use before HELLO has picked the host's (DRM only). */
static const struct nvgpu_schema_set *nvgpu_i2_set(struct nvgpu_device *dev) {
  return dev->schema ? dev->schema : &nvgpu_schema_sets[0];
}

const struct nvgpu_schema_set *nvgpu_schema_select(const char *driver_version) {
  unsigned int a, b, c;
  u32 v, i;

  /* The DRM tables are the same for every host; only NVKMS's layouts move
   * between releases (REGISTER_SURFACE is command 16 in one and 17 in the
   * next, R:nvdirect §0.6a), so an unparsable version gets no NVKMS table,
   * never a guessed one. */
  if (!driver_version ||
      sscanf(driver_version, "%u.%u.%u", &a, &b, &c) != 3)
    return &nvgpu_schema_sets[0];
  v = NVGPU_SCHEMA_VERSION(a, b, c);
  for (i = 1; i < ARRAY_SIZE(nvgpu_schema_sets); i++) {
    const struct nvgpu_stable *t = nvgpu_schema_sets[i].modeset;

    if (t->vmin <= v && v <= t->vmax)
      return &nvgpu_schema_sets[i];
  }
  return &nvgpu_schema_sets[0];
}

/*
 * The entry for a call: by (class, type, nr) in the DRM tables, or for NVKMS
 * by the command inside the outer struct (`prefix`, its first bytes). Size
 * and direction are compared by the caller, so that a known ioctl with the
 * wrong size is -EINVAL rather than "not ours".
 */
static const struct nvgpu_sioctl *
nvgpu_i2_lookup(const struct nvgpu_schema_set *set, u32 sclass,
                unsigned int cmd, const void *prefix, size_t prefix_len,
                const struct nvgpu_stable **tp) {
  const struct nvgpu_stable *t = set->drm;
  u32 nvkms_cmd = 0, i;

  if (sclass == NVGPU_SCLASS_MODESET) {
    t = set->modeset;
    if (!t || cmd != NVGPU_NVKMS_IOCTL_IOWR || prefix_len < 4)
      return NULL;
    nvkms_cmd = get_unaligned_le32(prefix);
  }
  for (i = 0; i < t->nioctls; i++) {
    const struct nvgpu_sioctl *e = &t->ioctls[i];

    if (e->sclass != sclass)
      continue;
    if (sclass == NVGPU_SCLASS_MODESET ? e->nvkms_cmd == nvkms_cmd
                                       : (e->cmd & 0xffff) == (cmd & 0xffff)) {
      *tp = t;
      return e;
    }
  }
  return NULL;
}

bool nvgpu_i2_has_schema(struct nvgpu_device *dev, u32 sclass,
                         unsigned int cmd, const void *arg_prefix,
                         size_t prefix_len) {
  const struct nvgpu_stable *t;

  return nvgpu_i2_lookup(nvgpu_i2_set(dev), sclass, cmd, arg_prefix,
                         prefix_len, &t) != NULL;
}

/* ───────── the kernel copies ───────── */

static int nvgpu_i2_rd(const struct nvgpu_i2_state *st, u32 b, u32 off,
                       u32 width, u64 *v) {
  const struct nvgpu_i2_kbuf *kb = &st->buf[b];

  if (width > kb->len || off > kb->len - width)
    return -EINVAL;
  *v = width == 8 ? get_unaligned_le64(kb->k + off)
                  : get_unaligned_le32(kb->k + off);
  return 0;
}

static void nvgpu_i2_wr(struct nvgpu_i2_state *st, u32 b, u32 off, u32 width,
                        u64 v) {
  struct nvgpu_i2_kbuf *kb = &st->buf[b];

  /* Only ever at positions the walk reached, which it bounds-checked. */
  if (width > kb->len || off > kb->len - width)
    return;
  if (width == 8)
    put_unaligned_le64(v, kb->k + off);
  else
    put_unaligned_le32((u32)v, kb->k + off);
}

static s64 nvgpu_i2_sext(u64 v, u32 width) {
  return width == 4 ? (s64)(s32)(u32)v : (s64)v;
}

/*
 * A new buffer of `len` bytes, the caller's `uptr`: its bytes copied in if
 * they travel IN, zeroes otherwise (the kernel never reads them, and the
 * backend's copy starts zeroed too). The request and the reply are bounded
 * here, as the walk goes, so a made-up count is refused before it is
 * allocated rather than after.
 */
static int nvgpu_i2_new_buf(struct nvgpu_device *dev,
                            struct nvgpu_i2_state *st, u64 len, u8 dir,
                            void __user *uptr, u32 *out) {
  struct nvgpu_i2_kbuf *kb;

  if (st->nbuf >= NVGPU_I2_MAX_BUFS)
    return -E2BIG;
  if (dir & NVGPU_SDIR_IN)
    st->in_bytes += ALIGN(len, 8);
  if (dir & NVGPU_SDIR_OUT)
    st->out_bytes += ALIGN(len, 8);
  if (st->in_bytes > dev->max_req || st->out_bytes > dev->max_resp)
    return -E2BIG;

  kb = &st->buf[st->nbuf];
  kb->len = len;
  kb->dir = dir;
  kb->uptr = uptr;
  if (len) {
    kb->k = kvzalloc(len, GFP_KERNEL);
    if (!kb->k)
      return -ENOMEM;
  }
  *out = st->nbuf++;
  if (!(dir & NVGPU_SDIR_IN) || !len)
    return 0;
  /* A call the driver makes itself, on memory it built (nvgpu_i2_call.kernel):
   * the caller wrote every address in it, so none is a user's to check. */
  if (st->kernel) {
    memcpy(kb->k, (const void __force *)uptr, len);
    return 0;
  }
  if (copy_from_user(kb->k, uptr, len))
    return -EFAULT;
  return 0;
}

static int nvgpu_i2_add_slot(struct nvgpu_i2_state *st,
                             const struct nvgpu_sfield *f, u32 b, u32 off,
                             u64 orig) {
  struct nvgpu_i2_slot *s;

  if (st->nslot >= NVGPU_I2_MAX_SLOTS)
    return -E2BIG;
  s = &st->slot[st->nslot++];
  s->f = f;
  s->buf = b;
  s->off = off;
  s->orig = orig;
  return 0;
}

/* A pointer's length in bytes, from the kernel copy of its struct. Mirrors
 * Walk::length in device/src/xfer.rs. */
static int nvgpu_i2_len(struct nvgpu_i2_state *st,
                        const struct nvgpu_sfield *f, u32 b, u32 base,
                        u16 first, const s32 *created, u64 *len) {
  u64 n = 0, i;
  int ret;

  switch (f->len_kind) {
  case NVGPU_SLEN_CONST:
    *len = f->len_a;
    return 0;
  case NVGPU_SLEN_COUNT:
    ret = nvgpu_i2_rd(st, b, base + f->len_a, f->len_width, &n);
    if (ret)
      return ret;
    if (check_mul_overflow(n, (u64)f->len_elem, len))
      return -E2BIG;
    return 0;
  case NVGPU_SLEN_SUM: {
    /* An earlier sibling, by the generator's check; bounded regardless. */
    s32 src = f->len_a >= first && f->len_a - first < NVGPU_SCHEMA_MAX_LIST
                  ? created[f->len_a - first]
                  : -1;
    u64 sum = 0;

    if (src >= 0) {
      const struct nvgpu_i2_kbuf *kb = &st->buf[src];

      for (i = 0; i + 4 <= kb->len; i += 4)
        sum += get_unaligned_le32(kb->k + i);
    }
    if (check_mul_overflow(sum, (u64)f->len_elem, len))
      return -E2BIG;
    return 0;
  }
  case NVGPU_SLEN_NVKMS_PARAMS:
    /* NvKmsIoctlParams.size, which NVKMS requires to be the command's params
     * size exactly (nvkms.c:5183), as do we. */
    ret = nvgpu_i2_rd(st, b, base + 4, 4, &n);
    if (ret)
      return ret;
    if (n != f->max)
      return -EINVAL;
    *len = n;
    return 0;
  }
  return -EINVAL;
}

/*
 * The canonical traversal over one field list: `n` fields from `first`, of
 * the struct at `base` in buffer `b`. Pointers get buffers depth first, in
 * field order, exactly as the backend's Walk::list assigns them.
 */
static int nvgpu_i2_walk(struct nvgpu_device *dev, struct nvgpu_i2_state *st,
                         u32 b, u32 base, u16 first, u16 n, int depth) {
  s32 created[NVGPU_SCHEMA_MAX_LIST];
  u32 i, e;
  int ret;

  if (depth > NVGPU_SCHEMA_MAX_DEPTH || n > NVGPU_SCHEMA_MAX_LIST)
    return -EINVAL;
  for (i = 0; i < n; i++)
    created[i] = -1;

  for (i = 0; i < n; i++) {
    const struct nvgpu_sfield *f = &st->t->fields[first + i];
    u32 at = base + f->off;
    u64 v, len;

    if (f->flags & NVGPU_SFF_COND) {
      ret = nvgpu_i2_rd(st, b, base + f->cond_off, 4, &v);
      if (ret)
        return ret;
      if (((u32)v & f->cond_mask) != f->cond_value)
        continue;
    }

    switch (f->kind) {
    case NVGPU_SF_PTR: {
      struct nvgpu_i2_kbuf *kb;
      u32 nb;

      ret = nvgpu_i2_rd(st, b, at, 8, &v);
      if (!ret)
        ret = nvgpu_i2_len(st, f, b, base, first, created, &len);
      if (!ret)
        ret = nvgpu_i2_add_slot(st, f, b, at, v);
      if (ret)
        return ret;
      /* NULL or empty: no buffer, and the host is handed NULL, which is
       * what it would do with an empty list anyway. */
      if (!v || !len)
        continue;
      if (len > f->max)
        return -E2BIG;
      if (f->nchild && (!f->stride || len % f->stride))
        return -EINVAL;
      ret = nvgpu_i2_new_buf(dev, st, len, f->dir, u64_to_user_ptr(v), &nb);
      if (ret)
        return ret;
      created[i] = nb;
      kb = &st->buf[nb];
      kb->f = f;
      kb->parent = b;
      kb->pbase = base;
      if (f->cb_kind == NVGPU_SCB_PARTIAL ||
          f->cb_kind == NVGPU_SCB_ALL_OR_NOTHING ||
          f->cb_kind == NVGPU_SCB_EXACT) {
        ret = nvgpu_i2_rd(st, b, base + f->cb_off, f->cb_width, &kb->sent);
        if (ret)
          return ret;
      }
      for (e = 0; f->nchild && e < len / f->stride; e++) {
        ret = nvgpu_i2_walk(dev, st, nb, e * f->stride, f->child, f->nchild,
                            depth + 1);
        if (ret)
          return ret;
      }
      break;
    }
    case NVGPU_SF_ARRAY:
      for (e = 0; e < f->count; e++) {
        ret = nvgpu_i2_walk(dev, st, b, at + e * f->stride, f->child,
                            f->nchild, depth + 1);
        if (ret)
          return ret;
      }
      break;
    default:
      ret = nvgpu_i2_rd(st, b, at, f->width, &v);
      if (!ret)
        ret = nvgpu_i2_add_slot(st, f, b, at, v);
      if (ret)
        return ret;
    }
  }
  return 0;
}

/* ───────── records ───────── */

int nvgpu_i2_add_dyn(struct nvgpu_i2_call *call, u32 kind, u32 buf, u32 off,
                     u32 len) {
  struct nvgpu_i2_state *st = call->st;
  struct nvgpu_i2_dyn *d;

  /* A dyn record names an 8-byte value slot (ATOMIC's prop_values). */
  if (!st || st->ndyn >= NVGPU_I2_MAX_RECS || buf >= st->nbuf ||
      st->buf[buf].len < 8 || off > st->buf[buf].len - 8)
    return -EINVAL;
  d = &st->dyn[st->ndyn++];
  d->kind = cpu_to_le32(kind);
  d->buf = cpu_to_le32(buf);
  d->off = cpu_to_le32(off);
  d->len = cpu_to_le32(len);
  return 0;
}

int nvgpu_i2_add_fd(struct nvgpu_i2_call *call, u32 buf, u32 off, u32 handle,
                    u32 flags) {
  struct nvgpu_i2_state *st = call->st;
  struct nvgpu_i2_fd_in *r;

  if (!st || st->nfd >= NVGPU_I2_MAX_RECS || buf >= st->nbuf ||
      st->buf[buf].len < 4 || off > st->buf[buf].len - 4)
    return -EINVAL;
  r = &st->fd[st->nfd++];
  r->buf = cpu_to_le32(buf);
  r->off = cpu_to_le32(off);
  r->handle = cpu_to_le32(handle);
  r->flags = cpu_to_le32(flags);
  return 0;
}

void *nvgpu_i2_buf(struct nvgpu_i2_call *call, u32 buf, u32 *len) {
  struct nvgpu_i2_state *st = call->st;

  if (!st || buf >= st->nbuf) {
    *len = 0;
    return NULL;
  }
  *len = st->buf[buf].len;
  return st->buf[buf].k;
}

/*
 * Descriptors the hooks handed us to be consumed by the call (fence unwraps
 * that made a temporary). The backend closes them once the call has run; if
 * it never runs, that is ours to do.
 */
static void nvgpu_i2_drop_consumed(struct nvgpu_i2_call *call) {
  struct nvgpu_i2_state *st = call->st;
  u32 i;

  for (i = 0; i < st->nfd; i++)
    if (le32_to_cpu(st->fd[i].flags) & NVGPU_I2_FD_CONSUME)
      nvgpu_close_handle_async(call->dev, le32_to_cpu(st->fd[i].handle));
}

/* The caller's descriptors and GEM handles, through the hooks. */
static int nvgpu_i2_translate(struct nvgpu_i2_call *call) {
  struct nvgpu_i2_state *st = call->st;
  const struct nvgpu_i2_ops *ops = call->ops;
  u32 i;
  int ret;

  for (i = 0; i < st->nslot; i++) {
    struct nvgpu_i2_slot *s = &st->slot[i];
    const struct nvgpu_sfield *f = s->f;

    if (f->kind == NVGPU_SF_FD_IN) {
      s64 v = nvgpu_i2_sext(s->orig, f->width);
      u32 handle = 0, flags = 0;

      if (v == f->none_value)
        continue;
      if (!ops || !ops->fd_in)
        return -EINVAL;
      ret = ops->fd_in(call, s->buf, s->off, v, f->kinds, &handle, &flags);
      if (ret < 0)
        return ret;
      /* Our number means nothing to the host; the backend writes its own
       * descriptor here, and the caller's comes back from s->orig. */
      nvgpu_i2_wr(st, s->buf, s->off, f->width, (u64)(s64)f->none_value);
      if (ret == 1)
        continue;
      ret = nvgpu_i2_add_fd(call, s->buf, s->off, handle, flags);
      if (ret) {
        if (flags & NVGPU_I2_FD_CONSUME)
          nvgpu_close_handle_async(call->dev, handle);
        return ret;
      }
    } else if (f->kind == NVGPU_SF_GEM_IN) {
      struct nvgpu_i2_gem_in *r;
      u32 owner, gem;

      if (!s->orig)
        continue;
      if (!ops || !ops->gem_in)
        return -EINVAL;
      ret = ops->gem_in(call, s->buf, s->off, (u32)s->orig, &owner, &gem);
      if (ret)
        return ret;
      /* The backend holds a zero field to have no record and any other to
       * have exactly one. */
      if (!gem || st->ngem >= NVGPU_I2_MAX_RECS)
        return -EINVAL;
      nvgpu_i2_wr(st, s->buf, s->off, 4, gem);
      r = &st->gem[st->ngem++];
      r->buf = cpu_to_le32(s->buf);
      r->off = cpu_to_le32(s->off);
      r->owner = cpu_to_le32(owner);
      r->gem = cpu_to_le32(gem);
    }
  }
  return 0;
}

static u32 nvgpu_i2_count(const struct nvgpu_i2_state *st, u8 kind) {
  u32 i, n = 0;

  for (i = 0; i < st->nslot; i++)
    n += st->slot[i].f->kind == kind;
  return n;
}

/* ───────── the request and the reply ───────── */

struct nvgpu_i2_head {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_req req;
} __packed;

struct nvgpu_i2_rhead {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_resp resp;
} __packed;

static int nvgpu_i2_build(struct nvgpu_i2_call *call, struct nvgpu_tbuf *tb) {
  struct nvgpu_i2_state *st = call->st;
  struct nvgpu_i2_head h = {};
  size_t off = 0;
  u32 i;
  int ret;

  h.hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL2);
  h.hdr.handle = cpu_to_le32(call->handle);
  h.req.cmd = cpu_to_le32(call->cmd);
  h.req.nbuf = cpu_to_le32(st->nbuf);
  h.req.nfd = cpu_to_le32(st->nfd);
  h.req.ngem = cpu_to_le32(st->ngem);
  h.req.ndyn = cpu_to_le32(st->ndyn);
  h.req.data_len = cpu_to_le32(st->in_bytes);
  h.req.render = cpu_to_le32(call->render);

  ret = nvgpu_tbuf_write(tb, off, &h, sizeof(h));
  off += sizeof(h);
  for (i = 0; !ret && i < st->nbuf; i++, off += 4) {
    __le32 len = cpu_to_le32(st->buf[i].len);

    ret = nvgpu_tbuf_write(tb, off, &len, 4);
  }
  if (!ret)
    ret = nvgpu_tbuf_write(tb, off, st->fd, st->nfd * sizeof(st->fd[0]));
  off += st->nfd * sizeof(st->fd[0]);
  if (!ret)
    ret = nvgpu_tbuf_write(tb, off, st->gem, st->ngem * sizeof(st->gem[0]));
  off += st->ngem * sizeof(st->gem[0]);
  if (!ret)
    ret = nvgpu_tbuf_write(tb, off, st->dyn, st->ndyn * sizeof(st->dyn[0]));
  off += st->ndyn * sizeof(st->dyn[0]);
  for (i = 0; !ret && i < st->nbuf; i++) {
    const struct nvgpu_i2_kbuf *kb = &st->buf[i];
    u32 pad = ALIGN(kb->len, 8) - kb->len;

    if (!(kb->dir & NVGPU_SDIR_IN))
      continue;
    ret = nvgpu_tbuf_write(tb, off, kb->k, kb->len);
    if (!ret)
      ret = nvgpu_tbuf_write(tb, off + kb->len, nvgpu_i2_zero, pad);
    off += kb->len + pad;
  }
  return ret;
}

static bool nvgpu_i2_at_slot(const struct nvgpu_i2_state *st, u8 kind, u32 b,
                             u32 off) {
  u32 i;

  for (i = 0; i < st->nslot; i++)
    if (st->slot[i].f->kind == kind && st->slot[i].buf == b &&
        st->slot[i].off == off)
      return true;
  return false;
}

static bool nvgpu_i2_at_dyn(const struct nvgpu_i2_state *st, u32 b, u32 off) {
  u32 i;

  for (i = 0; i < st->ndyn; i++)
    if (le32_to_cpu(st->dyn[i].buf) == b && le32_to_cpu(st->dyn[i].off) == off)
      return true;
  return false;
}

/* Every handle a reply created and nobody will own now. */
static void nvgpu_i2_drop_outs(struct nvgpu_i2_call *call, u32 fd_from,
                               u32 gem_from) {
  struct nvgpu_i2_state *st = call->st;
  u32 i, j;

  for (i = fd_from; i < st->nfdo; i++)
    nvgpu_close_handle_async(call->dev, st->fdo[i].handle);
  for (i = gem_from; i < st->ngemo; i++) {
    for (j = gem_from; j < i; j++)
      if (st->gemo[j].handle == st->gemo[i].handle)
        break;
    if (j == i)
      nvgpu_gem_close_async(call->dev, call->render, st->gemo[i].handle);
  }
  st->nfdo = fd_from;
  st->ngemo = gem_from;
}

/*
 * Read the reply into the kernel copies. Everything is checked against what
 * we sent and what the device says it wrote: buffer count, data length,
 * record counts no larger than the schema allows, and every record at a
 * position the schema (or a dyn record we sent) names, at most once.
 */
static int nvgpu_i2_parse(struct nvgpu_i2_call *call, struct nvgpu_tbuf *tb,
                          u32 used, u32 max_fdo, u32 max_gemo) {
  struct nvgpu_i2_state *st = call->st;
  struct nvgpu_device *dev = call->dev;
  struct nvgpu_i2_rhead h;
  size_t off = sizeof(h), need;
  u32 nfd, ngem, i, j;
  s32 status, ret;

  /*
   * The status first, from the header alone: a refusal -- by the transport or
   * by the backend before the call ran -- is a bare nvgpu_msg_hdr, with no
   * nvgpu_i2_resp behind it. Requiring the whole head first turned every
   * refusal (-ENOTTY, -EPERM, -EMSGSIZE...) into -EPROTO and leaked the
   * handles the call was to consume.
   */
  if (used < sizeof(h.hdr) || nvgpu_tbuf_read(tb, 0, &h.hdr, sizeof(h.hdr)))
    return -EPROTO;
  status = (s32)le32_to_cpu(h.hdr.status);
  if (status) {
    /* Refused before the call ran: nothing was consumed or created. */
    nvgpu_i2_drop_consumed(call);
    return status < 0 && status >= -MAX_ERRNO ? status : -EPROTO;
  }
  if (used < sizeof(h) || nvgpu_tbuf_read(tb, 0, &h, sizeof(h)))
    return -EPROTO;
  ret = (s32)le32_to_cpu(h.resp.ret);
  nfd = le32_to_cpu(h.resp.nfd);
  ngem = le32_to_cpu(h.resp.ngem);
  need = sizeof(h) + st->out_bytes + (size_t)nfd * sizeof(struct nvgpu_i2_fd_out) +
         (size_t)ngem * sizeof(struct nvgpu_i2_gem_out);
  if (le32_to_cpu(h.resp.nbuf) != st->nbuf ||
      le32_to_cpu(h.resp.data_len) != st->out_bytes || nfd > max_fdo ||
      ngem > max_gemo || used < need || ret < -MAX_ERRNO) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: IOCTL2 %s: malformed reply "
                         "(%u bytes, %u fds, %u GEM handles)\n",
                         st->e->name, used, nfd, ngem);
    return -EPROTO;
  }

  for (i = 0; i < st->nbuf; i++) {
    struct nvgpu_i2_kbuf *kb = &st->buf[i];

    if (!(kb->dir & NVGPU_SDIR_OUT))
      continue;
    if (nvgpu_tbuf_read(tb, off, kb->k, kb->len))
      return -EPROTO;
    off += ALIGN(kb->len, 8);
  }
  for (i = 0; i < nfd; i++, off += sizeof(struct nvgpu_i2_fd_out)) {
    struct nvgpu_i2_fd_out r;

    if (nvgpu_tbuf_read(tb, off, &r, sizeof(r))) {
      st->nfdo = i;
      nvgpu_i2_drop_outs(call, 0, 0);
      return -EPROTO;
    }
    st->fdo[i] = (struct nvgpu_i2_out){
        .buf = le32_to_cpu(r.buf), .off = le32_to_cpu(r.off),
        .handle = le32_to_cpu(r.handle), .kind = le32_to_cpu(r.kind)};
  }
  st->nfdo = nfd;
  for (i = 0; i < ngem; i++, off += sizeof(struct nvgpu_i2_gem_out)) {
    struct nvgpu_i2_gem_out r;

    if (nvgpu_tbuf_read(tb, off, &r, sizeof(r))) {
      st->ngemo = i;
      nvgpu_i2_drop_outs(call, 0, 0);
      return -EPROTO;
    }
    st->gemo[i] = (struct nvgpu_i2_out){
        .buf = le32_to_cpu(r.buf), .off = le32_to_cpu(r.off),
        .handle = le32_to_cpu(r.gem), .size = le64_to_cpu(r.size)};
  }
  st->ngemo = ngem;

  for (i = 0; i < st->nfdo; i++) {
    const struct nvgpu_i2_out *o = &st->fdo[i];
    bool ok = nvgpu_i2_at_slot(st, NVGPU_SF_FD_OUT, o->buf, o->off) ||
              nvgpu_i2_at_dyn(st, o->buf, o->off);

    for (j = 0; ok && j < i; j++)
      ok = st->fdo[j].buf != o->buf || st->fdo[j].off != o->off;
    if (!ok)
      goto bad;
  }
  for (i = 0; i < st->ngemo; i++) {
    const struct nvgpu_i2_out *o = &st->gemo[i];
    bool ok = o->handle &&
              nvgpu_i2_at_slot(st, NVGPU_SF_GEM_OUT, o->buf, o->off);

    for (j = 0; ok && j < i; j++)
      ok = st->gemo[j].buf != o->buf || st->gemo[j].off != o->off;
    if (!ok)
      goto bad;
  }
  call->ret = ret;
  return 0;

bad:
  dev_warn_ratelimited(&dev->vdev->dev,
                       "virtio-gpu-nv: IOCTL2 %s: the reply names a "
                       "descriptor or GEM handle where the schema has none\n",
                       st->e->name);
  nvgpu_i2_drop_outs(call, 0, 0);
  return -EPROTO;
}

/*
 * The caller's own values back where ours or the backend's were: its
 * pointers, its descriptor numbers, its GEM handles. Descriptor-out and
 * GEM-out fields start empty and are filled by the hooks.
 */
static void nvgpu_i2_restore(struct nvgpu_i2_state *st) {
  u32 i;

  for (i = 0; i < st->nslot; i++) {
    const struct nvgpu_i2_slot *s = &st->slot[i];

    switch (s->f->kind) {
    case NVGPU_SF_PTR:
      nvgpu_i2_wr(st, s->buf, s->off, 8, s->orig);
      break;
    case NVGPU_SF_FD_IN:
      nvgpu_i2_wr(st, s->buf, s->off, s->f->width, s->orig);
      break;
    case NVGPU_SF_GEM_IN:
      nvgpu_i2_wr(st, s->buf, s->off, 4, s->orig);
      break;
    case NVGPU_SF_FD_OUT:
      nvgpu_i2_wr(st, s->buf, s->off, s->f->width, (u64)-1);
      break;
    case NVGPU_SF_GEM_OUT:
      nvgpu_i2_wr(st, s->buf, s->off, 4, 0);
      break;
    }
  }
}

/*
 * What the host made, made ours: one proxy per distinct host GEM handle
 * (GETFB2 names one object once per plane, and the kernel's answer is one
 * handle, drm_framebuffer.c:646), and a guest fd per descriptor. A hook that
 * fails leaves the rest to be closed here.
 */
static int nvgpu_i2_outputs(struct nvgpu_i2_call *call) {
  struct nvgpu_i2_state *st = call->st;
  const struct nvgpu_i2_ops *ops = call->ops;
  u32 i, j;
  int ret;

  for (i = 0; i < st->ngemo; i++) {
    struct nvgpu_i2_out *o = &st->gemo[i];

    for (j = 0; j < i; j++)
      if (st->gemo[j].handle == o->handle)
        break;
    if (j < i) {
      o->kind = st->gemo[j].kind;
    } else {
      ret = ops && ops->gem_out ? ops->gem_out(call, o->buf, o->off, o->handle,
                                               o->size, &o->kind)
                                : -EINVAL;
      if (ret) {
        nvgpu_i2_drop_outs(call, 0, i);
        return ret;
      }
    }
    nvgpu_i2_wr(st, o->buf, o->off, 4, o->kind);
  }

  for (i = 0; i < st->nfdo; i++) {
    struct nvgpu_i2_out *o = &st->fdo[i];
    s64 v = -1;

    /* Handle 0: the host made the descriptor but the backend could not keep
     * it (its handle table was full) and closed it again. What the call made
     * is gone -- a lease, a fence -- so the caller hears it failed, the way a
     * host process out of descriptors would. */
    if (!o->handle)
      ret = -EMFILE;
    else
      ret = ops && ops->fd_out ? ops->fd_out(call, o->buf, o->off, o->handle,
                                             o->kind, &v)
                               : -EINVAL;
    if (ret) {
      nvgpu_i2_drop_outs(call, i, st->ngemo);
      return ret;
    }
    /* A dyn position (ATOMIC's out-fence) has no slot and is in no buffer
     * we copy back; the special writes the caller's pointer itself. */
    for (j = 0; j < st->nslot; j++)
      if (st->slot[j].f->kind == NVGPU_SF_FD_OUT &&
          st->slot[j].buf == o->buf && st->slot[j].off == o->off)
        nvgpu_i2_wr(st, o->buf, o->off, st->slot[j].f->width, (u64)v);
  }
  return 0;
}

/*
 * Which bytes [start, end) of an OUT buffer reach the caller. A line-for-line
 * mirror of CopyBack::extent (gen/src/schema/mod.rs), whose tests are the
 * kernel's fill rules stated as cases.
 */
static void nvgpu_i2_copy_extent(const struct nvgpu_sfield *f, u8 dir, u64 len,
                                 s32 ret, u64 sent, u64 left, u64 *start,
                                 u64 *end) {
  bool ok = ret == 0;
  u64 n = 0;

  *start = 0;
  switch (f->cb_kind) {
  case NVGPU_SCB_FULL:
    if (ok || dir == NVGPU_SDIR_INOUT)
      n = len;
    break;
  case NVGPU_SCB_PARTIAL:
    if (ok && check_mul_overflow(min(sent, left), (u64)f->cb_arg, &n))
      n = U64_MAX;
    break;
  case NVGPU_SCB_ALL_OR_NOTHING:
    if (ok && left <= sent && check_mul_overflow(left, (u64)f->cb_arg, &n))
      n = U64_MAX;
    break;
  case NVGPU_SCB_EXACT:
    if (ok && left == sent)
      n = len;
    break;
  case NVGPU_SCB_RANGE:
    *start = min_t(u64, f->cb_off, len);
    *end = min_t(u64, (u64)f->cb_off + f->cb_arg, len);
    return;
  }
  *end = min(n, len);
}

static int nvgpu_i2_copy_back(struct nvgpu_i2_call *call) {
  struct nvgpu_i2_state *st = call->st;
  int fault = 0;
  u32 i;

  for (i = 0; i < st->nbuf; i++) {
    const struct nvgpu_i2_kbuf *kb = &st->buf[i];
    u64 start = 0, end = kb->len, left = 0;

    if (!(kb->dir & NVGPU_SDIR_OUT) || !kb->len)
      continue;
    /* The argument goes back whole whatever happened, as drm_ioctl copies
     * out_size bytes back after the handler whatever it returned. */
    if (kb->f) {
      if (kb->f->cb_kind == NVGPU_SCB_PARTIAL ||
          kb->f->cb_kind == NVGPU_SCB_ALL_OR_NOTHING ||
          kb->f->cb_kind == NVGPU_SCB_EXACT)
        if (nvgpu_i2_rd(st, kb->parent, kb->pbase + kb->f->cb_off,
                        kb->f->cb_width, &left))
          continue;
      nvgpu_i2_copy_extent(kb->f, kb->dir, kb->len, call->ret, kb->sent, left,
                           &start, &end);
    }
    if (end <= start)
      continue;
    if (st->kernel)
      memcpy((void __force *)kb->uptr + start, kb->k + start, end - start);
    else if (copy_to_user(kb->uptr + start, kb->k + start, end - start))
      fault = -EFAULT;
  }
  return fault;
}

/* ───────── the call ───────── */

static int nvgpu_i2_gather(struct nvgpu_i2_call *call) {
  struct nvgpu_device *dev = call->dev;
  struct nvgpu_i2_state *st = call->st;
  const struct nvgpu_schema_set *set = nvgpu_i2_set(dev);
  u32 size = _IOC_SIZE(call->cmd), b0;
  u8 dir = 0;
  int ret;

  if (_IOC_DIR(call->cmd) & _IOC_WRITE)
    dir |= NVGPU_SDIR_IN;
  if (_IOC_DIR(call->cmd) & _IOC_READ)
    dir |= NVGPU_SDIR_OUT;

  if (call->sclass != NVGPU_SCLASS_MODESET) {
    st->e = nvgpu_i2_lookup(set, call->sclass, call->cmd, NULL, 0, &st->t);
    if (!st->e)
      return -ENOTTY;
    if (st->e->cmd != call->cmd) {
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: IOCTL2 %s: caller's ioctl 0x%08x "
                           "is not 0x%08x (size %u)\n",
                           st->e->name, call->cmd, st->e->cmd, size);
      return -EINVAL;
    }
  } else if (call->cmd != NVGPU_NVKMS_IOCTL_IOWR) {
    return -ENOTTY;
  }

  /*
   * 32-bit callers: every KMS struct is layout-identical except these two,
   * whose compat forms the core converts (drm_ioc32.c:334-346). We do not
   * convert; refusing is better than handing the host a misparsed struct.
   */
  if (call->sclass == NVGPU_SCLASS_KMS && in_compat_syscall() &&
      (_IOC_NR(call->cmd) == 0x3a || _IOC_NR(call->cmd) == 0xb8))
    return -EINVAL;

  /* NVKMS never writes the outer struct (nvidia-modeset-linux.c:1963). */
  if (call->sclass == NVGPU_SCLASS_MODESET)
    dir = NVGPU_SDIR_IN;
  ret = nvgpu_i2_new_buf(dev, st, size, dir, call->uarg, &b0);
  if (ret)
    return ret;
  if (call->sclass == NVGPU_SCLASS_MODESET) {
    st->e = nvgpu_i2_lookup(set, call->sclass, call->cmd, st->buf[0].k,
                            st->buf[0].len, &st->t);
    if (!st->e)
      return -ENOTTY;
  }

  ret = nvgpu_i2_walk(dev, st, 0, 0, st->e->field, st->e->nfield, 0);
  if (ret)
    return ret;
  if (nvgpu_i2_count(st, NVGPU_SF_FD_OUT) > NVGPU_I2_MAX_RECS ||
      nvgpu_i2_count(st, NVGPU_SF_GEM_OUT) > NVGPU_I2_MAX_RECS)
    return -E2BIG;
  return 0;
}

static void nvgpu_i2_free(struct nvgpu_i2_state *st) {
  u32 i;

  for (i = 0; i < st->nbuf; i++)
    kvfree(st->buf[i].k);
  kvfree(st);
}

long nvgpu_i2_ioctl(struct nvgpu_i2_call *call) {
  struct nvgpu_device *dev = call->dev;
  struct nvgpu_i2_state *st;
  struct nvgpu_tbuf *req = NULL, *resp = NULL;
  size_t req_len, resp_len;
  u32 used = 0, xflags, max_fdo, max_gemo;
  int ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  st = kvzalloc(sizeof(*st), GFP_KERNEL);
  if (!st)
    return -ENOMEM;
  call->st = st;
  call->ret = 0;
  st->kernel = call->kernel;

  ret = nvgpu_i2_gather(call);
  if (ret)
    goto out;
  ret = nvgpu_i2_translate(call);
  if (!ret && st->e->special == NVGPU_SSPECIAL_ATOMIC && call->ops &&
      call->ops->special)
    ret = call->ops->special(call, NVGPU_SSPECIAL_ATOMIC, 0);
  if (ret)
    goto drop;

  max_fdo = nvgpu_i2_count(st, NVGPU_SF_FD_OUT) + st->ndyn;
  max_gemo = nvgpu_i2_count(st, NVGPU_SF_GEM_OUT);
  req_len = sizeof(struct nvgpu_i2_head) + 4 * st->nbuf +
            sizeof(struct nvgpu_i2_fd_in) * st->nfd +
            sizeof(struct nvgpu_i2_gem_in) * st->ngem +
            sizeof(struct nvgpu_i2_dyn) * st->ndyn + st->in_bytes;
  resp_len = sizeof(struct nvgpu_i2_rhead) + st->out_bytes +
             sizeof(struct nvgpu_i2_fd_out) * max_fdo +
             sizeof(struct nvgpu_i2_gem_out) * max_gemo;
  if (req_len > dev->max_req || resp_len > dev->max_resp) {
    ret = -E2BIG;
    goto drop;
  }
  req = nvgpu_tbuf_alloc(req_len, GFP_KERNEL);
  resp = nvgpu_tbuf_alloc(resp_len, GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto drop;
  }
  ret = nvgpu_i2_build(call, req);
  if (ret)
    goto drop;

  /* Every call on a KMS or modeset file waits behind that file's executor on
   * the host; a render-node call only if its entry says so. */
  xflags = call->xflags;
  if (call->sclass != NVGPU_SCLASS_RENDER ||
      (st->e->flags & NVGPU_SIO_EXECUTOR))
    xflags |= NVGPU_XF_EXECUTOR;
  ret = nvgpu_xfer(dev, req, resp, xflags, &used);
  if (ret == -ETIMEDOUT || ret == -EINTR) {
    /* The transport owns both buffers now, closes whatever the late reply
     * creates, and releases the consumed handles if the call never runs --
     * so neither is freed nor dropped here. */
    req = resp = NULL;
    goto out;
  }
  if (ret)
    goto drop;

  ret = nvgpu_i2_parse(call, resp, used, max_fdo, max_gemo);
  if (ret)
    goto out;
  nvgpu_i2_restore(st);
  ret = nvgpu_i2_outputs(call);
  if (!ret && st->e->special == NVGPU_SSPECIAL_ATOMIC && call->ops &&
      call->ops->special)
    ret = call->ops->special(call, NVGPU_SSPECIAL_ATOMIC, 1);
  if (!ret)
    ret = nvgpu_i2_copy_back(call);
  if (!ret)
    ret = call->ret;
  goto out;

drop:
  nvgpu_i2_drop_consumed(call);
out:
  if (req)
    nvgpu_tbuf_free(req);
  if (resp)
    nvgpu_tbuf_free(resp);
  call->st = NULL;
  nvgpu_i2_free(st);
  return ret;
}
