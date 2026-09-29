// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: transport buffers. Built from page chunks (at most 64 KiB
 * each), never vmalloc, so a request of any allowed size can be described to
 * the ring in a bounded number of scatter-gather entries whether or not
 * indirect descriptors were negotiated (nvgpu.h).
 */

#include <linux/gfp.h>
#include <linux/mm.h>
#include <linux/overflow.h>
#include <linux/scatterlist.h>
#include <linux/slab.h>
#include <linux/uaccess.h>

#include "nvgpu_xfer.h"

#define NVGPU_TBUF_CHUNK_ORDER get_order(NVGPU_TBUF_CHUNK)
/* Below this a buffer is one kmalloc with its header: most messages are tiny,
 * and a page apiece for a 40-byte CLOSE would be pure overhead. */
#define NVGPU_TBUF_INLINE_MAX 1024

struct nvgpu_tbuf *nvgpu_tbuf_alloc_sg(size_t len, gfp_t gfp,
                                       unsigned int max_sg) {
  struct nvgpu_tbuf *tb;
  size_t remaining = len;
  unsigned int n = 0;

  gfp &= ~__GFP_HIGHMEM; /* every piece must have a kernel address */
  if (!len || !max_sg)
    return NULL;

  if (len <= NVGPU_TBUF_INLINE_MAX) {
    tb = kmalloc(struct_size(tb, sg, 1) + len, gfp);
    if (!tb)
      return NULL;
    tb->len = len;
    tb->nents = 1;
    tb->inline_data = true;
    tb->release = NULL;
    sg_init_one(&tb->sg[0], &tb->sg[1], len);
    return tb;
  }

  if (len > (size_t)max_sg * NVGPU_TBUF_CHUNK)
    return NULL;

  tb = kzalloc(struct_size(tb, sg, max_sg), gfp);
  if (!tb)
    return NULL;
  sg_init_table(tb->sg, max_sg);

  /*
   * Largest pieces first, falling back an order at a time, but never below
   * the order at which what is left would no longer fit the entries left:
   * that one is asked for without __GFP_NORETRY, since failing it fails the
   * buffer. Each piece's order is get_order() of its length, which is what
   * nvgpu_tbuf_free() relies on -- a short last piece is allocated at the
   * order of its own length, not the chunk's.
   */
  while (remaining) {
    size_t need = DIV_ROUND_UP(remaining, max_sg - n);
    unsigned int lo = get_order(need);
    unsigned int hi = min_t(unsigned int, get_order(remaining),
                            NVGPU_TBUF_CHUNK_ORDER);
    struct page *pg = NULL;
    unsigned int o;
    size_t clen;

    if (lo > hi)
      goto fail;
    for (o = hi;; o--) {
      gfp_t g = o > lo ? gfp | __GFP_NORETRY | __GFP_NOWARN : gfp;

      pg = alloc_pages(g, o);
      if (pg || o == lo)
        break;
    }
    if (!pg)
      goto fail;

    clen = min_t(size_t, remaining, PAGE_SIZE << o);
    sg_set_page(&tb->sg[n++], pg, clen, 0);
    remaining -= clen;
  }

  if (n < max_sg) {
    sg_unmark_end(&tb->sg[max_sg - 1]);
    sg_mark_end(&tb->sg[n - 1]);
  }
  tb->nents = n;
  tb->len = len;
  return tb;

fail:
  tb->nents = n;
  nvgpu_tbuf_free(tb);
  return NULL;
}

/*
 * NULL on failure. The entry budget depends on the length, not the device:
 * up to 2 MiB the buffer fits the 32 entries a ring without indirect
 * descriptors allows, and larger buffers can only be sent with them anyway.
 */
struct nvgpu_tbuf *nvgpu_tbuf_alloc(size_t len, gfp_t gfp) {
  unsigned int max_sg = len <= (size_t)NVGPU_SG_MAX_DIRECT * NVGPU_TBUF_CHUNK
                            ? NVGPU_SG_MAX_DIRECT
                            : NVGPU_SG_MAX_INDIRECT;

  return nvgpu_tbuf_alloc_sg(len, gfp, max_sg);
}

void nvgpu_tbuf_free(struct nvgpu_tbuf *tb) {
  unsigned int i;

  if (!tb)
    return;
  if (tb->release)
    tb->release(tb->release_arg);
  if (!tb->inline_data)
    for (i = 0; i < tb->nents; i++)
      __free_pages(sg_page(&tb->sg[i]), get_order(tb->sg[i].length));
  kfree(tb);
}

size_t nvgpu_tbuf_len(const struct nvgpu_tbuf *tb) { return tb->len; }

/*
 * What a request's numbers stand for, kept until the request is done with:
 * a request buffer is freed by its caller once the reply is in, or by the
 * transport once a request its caller gave up on (-ETIMEDOUT, -EINTR) has
 * been answered late or is known never to run (nvgpu_req_free_orphan()) --
 * the moment the host can no longer act on what the request names. Always
 * process context. One per buffer.
 */
void nvgpu_tbuf_on_free(struct nvgpu_tbuf *tb, void (*fn)(void *arg),
                        void *arg) {
  WARN_ON(tb->release);
  tb->release = fn;
  tb->release_arg = arg;
}

enum nvgpu_tbuf_op {
  NVGPU_TB_WRITE,
  NVGPU_TB_WRITE_USER,
  NVGPU_TB_READ,
  NVGPU_TB_READ_USER,
  NVGPU_TB_ZERO,
};

/* Walks the pieces [off, off + len) falls in and does `op` on each. */
static int nvgpu_tbuf_copy(struct nvgpu_tbuf *tb, size_t off, void *kbuf,
                           void __user *ubuf, size_t len,
                           enum nvgpu_tbuf_op op) {
  size_t pos = 0;
  unsigned int i;

  if (len > tb->len || off > tb->len - len)
    return -EINVAL;

  for (i = 0; i < tb->nents && len; i++) {
    size_t seglen = tb->sg[i].length, in, n;
    u8 *va;

    if (off >= pos + seglen) {
      pos += seglen;
      continue;
    }
    in = off - pos;
    n = min(seglen - in, len);
    va = (u8 *)sg_virt(&tb->sg[i]) + in;

    switch (op) {
    case NVGPU_TB_WRITE:
      memcpy(va, kbuf, n);
      kbuf = (u8 *)kbuf + n;
      break;
    case NVGPU_TB_READ:
      memcpy(kbuf, va, n);
      kbuf = (u8 *)kbuf + n;
      break;
    case NVGPU_TB_WRITE_USER:
      if (copy_from_user(va, ubuf, n))
        return -EFAULT;
      ubuf = (u8 __user *)ubuf + n;
      break;
    case NVGPU_TB_READ_USER:
      if (copy_to_user(ubuf, va, n))
        return -EFAULT;
      ubuf = (u8 __user *)ubuf + n;
      break;
    case NVGPU_TB_ZERO:
      memset(va, 0, n);
      break;
    }
    off += n;
    len -= n;
    pos += seglen;
  }
  return 0;
}

int nvgpu_tbuf_write(struct nvgpu_tbuf *tb, size_t off, const void *src,
                     size_t len) {
  return nvgpu_tbuf_copy(tb, off, (void *)src, NULL, len, NVGPU_TB_WRITE);
}

int nvgpu_tbuf_write_user(struct nvgpu_tbuf *tb, size_t off,
                          const void __user *src, size_t len) {
  return nvgpu_tbuf_copy(tb, off, NULL, (void __user *)src, len,
                         NVGPU_TB_WRITE_USER);
}

int nvgpu_tbuf_read(const struct nvgpu_tbuf *tb, size_t off, void *dst,
                    size_t len) {
  return nvgpu_tbuf_copy((struct nvgpu_tbuf *)tb, off, dst, NULL, len,
                         NVGPU_TB_READ);
}

int nvgpu_tbuf_read_user(const struct nvgpu_tbuf *tb, size_t off,
                         void __user *dst, size_t len) {
  return nvgpu_tbuf_copy((struct nvgpu_tbuf *)tb, off, NULL, dst, len,
                         NVGPU_TB_READ_USER);
}

int nvgpu_tbuf_zero(struct nvgpu_tbuf *tb, size_t off, size_t len) {
  return nvgpu_tbuf_copy(tb, off, NULL, NULL, len, NVGPU_TB_ZERO);
}
