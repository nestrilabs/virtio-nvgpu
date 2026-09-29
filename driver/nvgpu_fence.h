/* SPDX-License-Identifier: GPL-2.0-only */
/*
 * virtio-gpu-nv: what the fence files share and nothing else includes --
 * nvgpu_fence.c (host fence proxies, unwrapping, the consumer graveyard, the
 * IOCTL2 calls they make), nvgpu_syncobj.c (the syncobj ioctls and the
 * waits slept here) and nvgpu_semsurf.c (semaphore-surface fences). The
 * design, contexts and lock order are nvgpu_fence.c's header.
 */

#ifndef NVGPU_FENCE_H
#define NVGPU_FENCE_H

#include <linux/list.h>
#include <linux/types.h>

#include "nvgpu.h"

struct dma_fence;
struct drm_file;

/*
 * A registry consumer owned by something that can die in any context. It is
 * unregistered only from process context: whatever frees its owner buries it
 * here instead, and nvgpu_fence_reap() finishes the job. A buried consumer
 * may still be called; its deliver() finds nothing and returns.
 */
struct nvgpu_fence_ev {
  struct nvgpu_ev_consumer c;
  struct nvgpu_device *dev;
  bool registered;
  u32 id; /* host fences: the index in nvgpu_host_fences */
  struct list_head dead;
  void (*free)(struct nvgpu_fence_ev *e);
};

/* Bury `e` (any context); nvgpu_fence_reap() retires it later. */
void nvgpu_fence_bury(struct nvgpu_fence_ev *e);
/*
 * Process context only. A registered consumer holds its device (taken with
 * the registration): a buried one may be retired long after remove(), by
 * whoever reaps next, and a syncobj wait's can outlive every file (S-26).
 */
void nvgpu_fence_ev_retire(struct nvgpu_fence_ev *e);
/* Retire everything buried so far. Process context. */
void nvgpu_fence_reap(void);

/* nvgpu_fence_unwrap_ex()'s answer for a fence the host has no counterpart of,
 * not signalled yet, when the caller asked not to wait for it. */
#define NVGPU_UNWRAP_FOREIGN 2
int nvgpu_fence_unwrap_ex(struct nvgpu_device *dev, struct dma_fence *f,
                          u32 *handle, bool *owned, bool wait);

/* What a call's hooks hand the interpreter. */
struct nvgpu_fence_call {
  struct nvgpu_fd *nfd;
  struct drm_file *file;
  /* FD_IN: the one descriptor field, resolved before the call, and the
   * fence keeping a proxy's own handle open (nvgpu_fence_unwrap_fd()). */
  bool has_in;
  bool in_used;
  u32 in_handle;
  u32 in_flags;
  struct dma_fence *in_ref;
  /* FD_OUT: the kind the call must produce, and whether the sync_file is
   * wanted as a proxy fence (`fence`, referenced) rather than a descriptor. */
  u32 want_kind;
  bool want_fence;
  struct dma_fence *fence;
  /* GEM_IN: (offset in the argument) -> (owner file, host handle). */
  u32 ngem;
  struct {
    u32 off, owner, gem;
  } gem[2];
};

/* One render-class IOCTL2 call with these hooks (nvgpu_fence.c). */
long nvgpu_fence_call(struct nvgpu_fence_call *p, u32 target,
                      unsigned int cmd, void *arg, bool built);
/* nvgpu_semsurf.c: a proxy for a new host fence context (the gem_out hook). */
int nvgpu_fence_ctx_create(struct nvgpu_fence_call *p, u32 host_handle,
                           u32 *guest_handle);
/* nvgpu_syncobj.c: the syncobj half of nvgpu_fence_device_dead(). */
void nvgpu_syncobj_device_dead(struct nvgpu_device *dev);

#endif /* NVGPU_FENCE_H */
