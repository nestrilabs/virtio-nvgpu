// SPDX-License-Identifier: GPL-2.0-only
/*
 * An atomic commit's arrays, as the caller laid them out: which objects it
 * sets which properties on, and so which CRTCs the commit carries (for their
 * flip events) and which of its values are fences. Moved from nvgpu_kms.c,
 * which keeps what the parse asks of it (nvgpu_atomic_ops): what an object
 * or a property is, the fence plumbing, the event reservations.
 *
 * This is the C implementation of what driver/rust/core/src/guest/atomic.rs
 * implements in Rust; the Makefile builds one or the other (NVGPU_RUST), and
 * driver/rust/difftest runs both on the same commits.
 *
 * Buffers follow the canonical traversal: after the argument, each of objs,
 * count_props, props and prop_values that is non-NULL with a non-zero
 * length, in that order (gen/schema/drm_kms.py). The kernel makes one event
 * per CRTC whose state the commit carries (drm_atomic_uapi.c:1430): a CRTC
 * object with properties set, the CRTC a plane or connector is put on
 * (CRTC_ID), and the CRTC a touched plane or connector is already on
 * (drm_atomic.c:583, 1334), as far as nvgpu_kms.c knows it.
 */

#include <linux/kernel.h>
#include <linux/unaligned.h>

#include "nvgpu.h"

/* Past NVGPU_ATOMIC_MAX_EVENTS a CRTC's event is reserved when it arrives. */
static void nvgpu_atomic_add_crtc(u32 *crtcs, u32 *n, u32 crtc) {
  u32 i;

  for (i = 0; i < *n; i++)
    if (crtcs[i] == crtc)
      return;
  if (*n < NVGPU_ATOMIC_MAX_EVENTS)
    crtcs[(*n)++] = crtc;
}

int nvgpu_atomic_parse(struct nvgpu_i2_call *call, bool fences,
                       const struct nvgpu_atomic_ops *ops, void *ctx,
                       struct nvgpu_atomic_out *out) {
  u32 len, l1, l2, l3, l4, idx = 1, bobjs, bcp, bprops = 0, bvals = 0;
  u32 crtcs[NVGPU_ATOMIC_MAX_EVENTS], ncrtc = 0, nlearn = 0;
  u8 *b0 = nvgpu_i2_buf(call, 0, &len);
  u8 *objs, *cp, *props, *vals;
  u32 flags, count, o, j, k = 0;
  bool events;
  u64 sum = 0;
  int ret;

  if (!b0 || len < NVGPU_ATOMIC_SIZE)
    return 0;
  flags = get_unaligned_le32(b0 + NVGPU_ATOMIC_FLAGS);
  count = get_unaligned_le32(b0 + NVGPU_ATOMIC_COUNT_OBJS);
  out->commit = !(flags & NVGPU_ATOMIC_TEST_ONLY);
  events = (flags & NVGPU_ATOMIC_FLIP_EVENT) && out->commit;
  /*
   * Without the fence bridge a fence property is the backend's to refuse
   * (-EOPNOTSUPP, policy.rs FENCES), which it does from its own copy; there
   * is nothing to look up for it here.
   */
  if (!count || (!events && !fences))
    return 0;

  bobjs = get_unaligned_le64(b0 + NVGPU_ATOMIC_OBJS_PTR) ? idx++ : 0;
  bcp = get_unaligned_le64(b0 + NVGPU_ATOMIC_COUNT_PROPS_PTR) ? idx++ : 0;
  if (!bobjs || !bcp)
    return 0; /* the host faults on it; nothing will be made */
  objs = nvgpu_i2_buf(call, bobjs, &l1);
  cp = nvgpu_i2_buf(call, bcp, &l2);
  /* In u64: count * 4 in u32 could wrap to a length the buffers have (the
   * walk bounds count far below that, but this does not rely on it). */
  if (!objs || !cp || l1 != (u64)count * 4 || l2 != (u64)count * 4)
    return 0;
  for (o = 0; o < count; o++)
    sum += get_unaligned_le32(cp + 4 * o);
  if (sum && get_unaligned_le64(b0 + NVGPU_ATOMIC_PROPS_PTR))
    bprops = idx++;
  if (sum && get_unaligned_le64(b0 + NVGPU_ATOMIC_VALUES_PTR))
    bvals = idx++;
  props = bprops ? nvgpu_i2_buf(call, bprops, &l3) : NULL;
  vals = bvals ? nvgpu_i2_buf(call, bvals, &l4) : NULL;
  if (sum && (!props || !vals || l3 != sum * 4 || l4 != sum * 8))
    return 0;
  out->values_buf = bvals;

  for (o = 0; o < count; o++) {
    u32 obj = get_unaligned_le32(objs + 4 * o);
    u32 n = get_unaligned_le32(cp + 4 * o), on = 0;

    if (n && events) {
      ret = ops->obj_class(ctx, obj, &on);
      if (ret < 0)
        return ret;
      if (ret == NVGPU_KOBJ_CRTC)
        nvgpu_atomic_add_crtc(crtcs, &ncrtc, obj);
      else if (on)
        nvgpu_atomic_add_crtc(crtcs, &ncrtc, on);
    }
    for (j = 0; j < n; j++, k++) {
      u32 id = get_unaligned_le32(props + 4 * k);
      u64 v = get_unaligned_le64(vals + 8 * k);

      ret = ops->prop_class(ctx, id);
      if (ret < 0)
        return ret;
      switch (ret) {
      case NVGPU_KPROP_CRTC_ID:
        if (events && v)
          nvgpu_atomic_add_crtc(crtcs, &ncrtc, (u32)v);
        if (nlearn < NVGPU_ATOMIC_MAX_LEARN) {
          ops->learn(ctx, obj, (u32)v);
          nlearn++;
        }
        break;
      case NVGPU_KPROP_IN_FENCE:
        if (fences && (s64)v != -1) {
          ret = ops->in_fence(ctx, call->st, bvals, 8 * k, (s64)v);
          if (ret)
            return ret;
        }
        break;
      case NVGPU_KPROP_OUT_PTR:
        if (fences && v) {
          ret = ops->out_fence(ctx, call->st, bvals, 8 * k, v);
          if (ret)
            return ret;
        }
        break;
      }
    }
  }

  for (o = 0; events && o < ncrtc; o++) {
    ret = ops->reserve(ctx, crtcs[o],
                       get_unaligned_le64(b0 + NVGPU_ATOMIC_USER_DATA));
    if (ret)
      return ret;
  }
  return 0;
}
