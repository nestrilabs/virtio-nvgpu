// SPDX-License-Identifier: GPL-2.0
/*
 * virtio-gpu-nv: KMS on a guest DRM file (DESIGN §4).
 *
 * Every guest DRM file stands in front of a host *render* file, which owns the
 * file's GEM objects and serves its render-node ioctls. Some files also have a
 * KMS side: a second backend handle, on a host *card* or *lease* file, that
 * the KMS ioctls go to. Two kinds of file have one:
 *
 *   - an adopted lease (nvgpu_adopt_drm_file()): a host lease fd -- granted by
 *     the host compositor over wp_drm_lease_device_v1, or made by a guest
 *     CREATE_LEASE -- materialised as a clone of a guest card-node file. Its
 *     KMS handle is the lease, from the first moment;
 *   - a primary-node file of a compositor-VM guest (NVGPU_BCAP_KMS_CARD),
 *     whose host card file is opened lazily: on the first KMS ioctl, or when
 *     the file becomes guest DRM master.
 *
 * Master (RV:master). The guest DRM core arbitrates master exactly as on bare
 * metal: first opener, SET/DROP_MASTER with its own permission checks against
 * *guest* processes. The host cannot: every host file belongs to the one
 * backend process, so the host's own check (drm_auth.c:232-243, was_master &&
 * same tgid) would pass for any guest process holding a once-master fd. So the
 * host merely follows the guest: master_set/master_drop forward SET/DROP_MASTER
 * on the file's host card, and a card lazily opened for a file that is *not*
 * guest master is dropped from host master at once (HOST_OP DROP_IF_MASTER),
 * so that a probe opening card0 can never walk off with the display. A host
 * file opened while another was host master can never become master at all
 * (was_master is only set by holding it); if the guest later makes such a file
 * master, a fresh host file is opened for it -- the one host call that can take
 * a master-less card -- and the old one is kept, not master, until the guest
 * file goes, so the framebuffers and blobs made on it (ids are device-global)
 * stay valid. Client caps are replayed onto the new one.
 *
 * Ioctls. The KMS ones travel as IOCTL2 on the KMS handle, through the schema
 * interpreter (nvgpu_i2.c), with the hooks below: GEM handles in are our
 * proxies' (owner, host GEM), GEM handles out arrive re-homed into the file's
 * render handle and get proxies (or the proxy the object already has), the
 * descriptor CREATE_LEASE makes is adopted as a guest file cloned from the
 * caller's, GRANT_PERMISSIONS' modeset fd becomes its handle. VERSION,
 * GET_UNIQUE, the magic/auth pair, SET/DROP_MASTER and SET_CLIENT_NAME stay
 * with the guest core; MAP_DUMB and DESTROY_DUMB are answered from the proxy.
 *
 * Events (RV:events). The host's flip and vblank events come back as EV_DRM
 * records for the KMS handle. The guest reserves event space in the caller's
 * drm_file when the request is made, like the core does (-ENOMEM up front,
 * drm_atomic_uapi.c:1451, drm_vblank.c:1653), keeps each reservation on a
 * pending list keyed by (type, CRTC, user_data), and fills and sends it when
 * the host's event arrives, its timestamp moved into the guest's clock. A
 * WAIT_VBLANK that would block is sent as its EVENT form with a cookie of our
 * own, and the caller sleeps here for that event instead of a host thread
 * sleeping for it.
 */

#include <drm/drm.h>
#include <drm/drm_auth.h>
#include <drm/drm_device.h>
#include <drm/drm_drv.h>
#include <drm/drm_file.h>
#include <drm/drm_gem.h>
#include <linux/completion.h>
#include <linux/cred.h>
#include <linux/file.h>
#include <linux/fs.h>
#include <linux/jiffies.h>
#include <linux/math64.h>
#include <linux/mutex.h>
#include <linux/sched.h>
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/sync_file.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>
#include <linux/xarray.h>

#include "nvgpu.h"
#include "gen/nvgpu_schema.h"

/* ───────── limits and names ───────── */

/*
 * Events reserved per call. An atomic commit gets one per CRTC in it
 * (drm_atomic_uapi.c:1433-1455), so this is "more CRTCs than any GPU has";
 * anything past it is reserved when its event arrives instead.
 */
#define NVGPU_KMS_MAX_EVENTS 32
/* Out-fence pointers in one commit: one per CRTC or writeback connector. */
#define NVGPU_KMS_MAX_FENCES 32
/* CRTC_ID assignments one commit may teach us. */
#define NVGPU_KMS_MAX_LEARN 64
/* DRM_CLIENT_CAP_* remembered for a replay; the highest is 7 in 7.2. */
#define NVGPU_KMS_NCAPS 16
/* How long a blocking WAIT_VBLANK waits: the core's own bound
 * (drm_vblank.c:1850), after which it answers -EBUSY. */
#define NVGPU_KMS_VBLANK_WAIT_MS 3000

/*
 * user_data of the EVENT form a blocking WAIT_VBLANK is sent as: a tag in the
 * top 48 bits and a counter below. A caller's own `signal` value could collide
 * with one in principle; one that did would have its event taken for an
 * internal wait. The tag makes that a 2^-48 accident, never a guessable one.
 */
#define NVGPU_KMS_COOKIE_TAG 0x6e76677076626cull /* "nvgpvbl" */
#define NVGPU_KMS_COOKIE_SHIFT 16

/* What a property is, as far as this file cares (by name, like the backend). */
#define NVGPU_KPROP_PLAIN 1
#define NVGPU_KPROP_CRTC_ID 2  /* plane/connector CRTC_ID: a CRTC joins the commit */
#define NVGPU_KPROP_IN_FENCE 3 /* a sync_file descriptor, -1 none */
#define NVGPU_KPROP_OUT_PTR 4  /* a user pointer the kernel writes an fd to */

/* What an object is: a CRTC, or not -- then with the CRTC it is on (<< 2),
 * when a GETPLANE reply or a committed CRTC_ID told us. */
#define NVGPU_KOBJ_CRTC 1
#define NVGPU_KOBJ_OTHER 2

#define NVGPU_KNR(ioc) _IOC_NR(ioc)

/* ───────── state ───────── */

struct nvgpu_kms_file;

/* An EV_DRM consumer for one KMS handle of a file. */
struct nvgpu_kms_evc {
  struct nvgpu_ev_consumer c;
  struct nvgpu_kms_file *kf;
  bool registered;
};

/* The KMS side of one guest DRM file (nvgpu_fd.kms). */
struct nvgpu_kms_file {
  struct nvgpu_device *dev;
  struct nvgpu_dri_dev *dri;
  struct drm_device *drm;
  struct nvgpu_fd *nfd;
  bool adopted; /* a lease: its master state is the host's business alone */

  /* Opening the host card, the master hooks and the swap, one at a time. */
  struct mutex lock;
  /* The KMS handle calls go to (READ_ONCE: set once under `lock`, replaced
   * at most once by the swap, and always a handle that stays open until the
   * file is released -- so a call that read the old one still has a file). */
  u32 handle;
  /* The card handle the swap replaced; closed at release. */
  u32 retired;
  /* The current card handle's host file has been host master (was_master on
   * the host), so SET_MASTER on it can succeed. */
  bool host_master_ok;
  /* [0] the first KMS handle, [1] the one a swap opened. */
  struct nvgpu_kms_evc evc[2];

  /* Under drm->event_lock: reserved events not yet sent, and the blocking
   * WAIT_VBLANKs waiting for theirs. */
  struct list_head pending;
  struct list_head waiters;

  /* Device-global ids, so a verdict never goes stale for the host device's
   * life; kept per file only for the lifetime's sake. */
  struct xarray props; /* prop id -> xa_mk_value(NVGPU_KPROP_*) */
  struct xarray objs;  /* object id -> xa_mk_value(NVGPU_KOBJ_* | crtc << 2) */

  /* Under `lock`: DRM_CLIENT_CAP_* set on this file, for the swap. */
  u32 caps_set;
  u64 caps[NVGPU_KMS_NCAPS];

  atomic_t next_cookie;
};

/*
 * A reserved event. `base` first: drm_read() frees what it delivered with
 * kfree() on the drm_pending_event (drm_file.c:600).
 */
struct nvgpu_kms_pev {
  struct drm_pending_event base;
  union {
    struct drm_event e;
    struct drm_event_vblank vbl;
    struct drm_event_crtc_sequence seq;
  } ev;
  struct list_head node; /* kf->pending, under drm->event_lock */
  void *owner;           /* the call that reserved it, until it settles */
  u32 crtc_id;           /* 0: any */
  u64 user_data;
};

/* A blocking WAIT_VBLANK, waiting for the event it was turned into. */
struct nvgpu_kms_wait {
  struct list_head node; /* kf->waiters, under drm->event_lock */
  u64 cookie;
  struct completion done;
  struct drm_event_vblank ev; /* the event, timestamp already ours */
};

/* One forwarded ioctl. */
struct nvgpu_kms_call {
  struct nvgpu_i2_call call;
  struct nvgpu_kms_file *kf;
  struct drm_file *file;
  struct file *filp;
  u32 nev;
  bool replied;
  s32 host_ret;

  /* WAIT_VBLANK sent as an event */
  bool waiting;
  u64 vbl_signal;
  struct nvgpu_kms_wait wait;

  /* GETRESOURCES: the CRTC list's buffer and how many ids it held */
  u32 res_buf, res_sent;

  /* ATOMIC */
  bool commit; /* not TEST_ONLY */
  u32 values_buf;
  u32 nfence;
  u32 fence_off[NVGPU_KMS_MAX_FENCES];
  u64 fence_uptr[NVGPU_KMS_MAX_FENCES];
  u32 nlearn;
  struct {
    u32 obj, crtc;
  } learn[NVGPU_KMS_MAX_LEARN];

  /* SET_CLIENT_CAP */
  bool cap_set;
  u64 cap, cap_value;
};

static inline struct nvgpu_kms_call *to_kms_call(struct nvgpu_i2_call *call) {
  return container_of(call, struct nvgpu_kms_call, call);
}

/* ───────── IOCTL2 of our own ─────────
 *
 * The master hooks, the swap's cap replay and the property/object lookups
 * need calls that no user made: no user memory behind them, so not through
 * nvgpu_i2_ioctl(). Each is a single flat struct with no pointer the schema
 * would give a buffer (their pointer fields are zero and their counts too), so
 * the request is the header, one buffer length and the struct: the same bytes
 * the interpreter would build. Run on the file's executor like every KMS call.
 */

struct nvgpu_kms_i2_head {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_req req;
} __packed;

struct nvgpu_kms_i2_rhead {
  struct nvgpu_msg_hdr hdr;
  struct nvgpu_i2_resp resp;
} __packed;

/*
 * Returns 0 with the host's result in *host_ret (and `arg` updated from the
 * reply for an ioctl that reads back), or a transport/refusal -errno.
 */
static int nvgpu_kms_raw(struct nvgpu_kms_file *kf, u32 handle,
                         unsigned int cmd, void *arg, s32 *host_ret) {
  static const u8 zero[8];
  struct nvgpu_device *dev = kf->dev;
  u32 size = _IOC_SIZE(cmd), pad = ALIGN(size, 8);
  u32 in = (_IOC_DIR(cmd) & _IOC_WRITE) ? pad : 0;
  u32 out = (_IOC_DIR(cmd) & _IOC_READ) ? pad : 0;
  struct nvgpu_kms_i2_head h = {};
  struct nvgpu_kms_i2_rhead rh;
  struct nvgpu_tbuf *req, *resp;
  __le32 blen = cpu_to_le32(size);
  u32 used = 0;
  s32 status;
  int ret;

  if (!dev->v2)
    return -EOPNOTSUPP;
  req = nvgpu_tbuf_alloc(sizeof(h) + 4 + in, GFP_KERNEL);
  resp = nvgpu_tbuf_alloc(sizeof(rh) + out, GFP_KERNEL);
  if (!req || !resp) {
    ret = -ENOMEM;
    goto out;
  }

  h.hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL2);
  h.hdr.handle = cpu_to_le32(handle);
  h.req.cmd = cpu_to_le32(cmd);
  h.req.nbuf = cpu_to_le32(1);
  h.req.data_len = cpu_to_le32(in);
  h.req.render = cpu_to_le32(kf->nfd->handle);
  ret = nvgpu_tbuf_write(req, 0, &h, sizeof(h));
  if (!ret)
    ret = nvgpu_tbuf_write(req, sizeof(h), &blen, 4);
  if (!ret && in) {
    ret = nvgpu_tbuf_write(req, sizeof(h) + 4, arg, size);
    if (!ret)
      ret = nvgpu_tbuf_write(req, sizeof(h) + 4 + size, zero, pad - size);
  }
  if (ret)
    goto out;

  ret = nvgpu_xfer(dev, req, resp, NVGPU_XF_EXECUTOR, &used);
  if (ret == -ETIMEDOUT || ret == -EINTR)
    return ret; /* both buffers are the transport's now */
  if (ret)
    goto out;

  /* A refusal is a bare header (nvgpu_i2_parse() reads it the same way). */
  if (used < sizeof(rh.hdr) || nvgpu_tbuf_read(resp, 0, &rh.hdr, sizeof(rh.hdr))) {
    ret = -EPROTO;
    goto out;
  }
  status = (s32)le32_to_cpu(rh.hdr.status);
  if (status) {
    ret = status < 0 && status >= -MAX_ERRNO ? status : -EPROTO;
    goto out;
  }
  /* None of these ever makes a descriptor or a GEM handle; a reply saying
   * otherwise is not one we sent a request for. */
  if (used < sizeof(rh) + out || nvgpu_tbuf_read(resp, 0, &rh, sizeof(rh)) ||
      le32_to_cpu(rh.resp.nbuf) != 1 || le32_to_cpu(rh.resp.data_len) != out ||
      rh.resp.nfd || rh.resp.ngem ||
      (s32)le32_to_cpu(rh.resp.ret) < -MAX_ERRNO) {
    ret = -EPROTO;
    goto out;
  }
  *host_ret = (s32)le32_to_cpu(rh.resp.ret);
  if (out)
    ret = nvgpu_tbuf_read(resp, sizeof(rh), arg, size);
out:
  if (req)
    nvgpu_tbuf_free(req);
  if (resp)
    nvgpu_tbuf_free(resp);
  return ret;
}

/* SET_MASTER / DROP_MASTER on one handle: 0, the host's refusal, or the
 * transport's. */
static int nvgpu_kms_master_call(struct nvgpu_kms_file *kf, u32 handle,
                                 unsigned int cmd) {
  s32 hr = 0;
  int ret = nvgpu_kms_raw(kf, handle, cmd, NULL, &hr);

  return ret ? ret : hr;
}

/* ───────── handles, events on them ───────── */

static void nvgpu_kms_deliver(struct nvgpu_ev_consumer *c, u32 kind,
                              u64 cookie, const void *payload, u32 len);

/*
 * Events for `handle` start reaching this file: the consumer first, then the
 * WATCH, so nothing the host sends in between is lost.
 */
static int nvgpu_kms_bind(struct nvgpu_kms_file *kf, int slot, u32 handle) {
  struct nvgpu_kms_evc *evc = &kf->evc[slot];
  int ret;

  evc->kf = kf;
  evc->c.deliver = nvgpu_kms_deliver;
  ret = nvgpu_ev_register(kf->dev, &evc->c, NVGPU_EVKEY_HANDLE(handle));
  if (ret)
    return ret;
  ret = nvgpu_watch(kf->dev, handle, NVGPU_W_DRM, handle);
  if (ret) {
    nvgpu_ev_unregister(kf->dev, &evc->c);
    dev_warn_ratelimited(&kf->dev->vdev->dev,
                         "virtio-gpu-nv: the backend would not watch KMS "
                         "handle %u for events (%d); refusing a file whose "
                         "flips would never complete\n",
                         handle, ret);
    return ret;
  }
  evc->registered = true;
  return 0;
}

/* The file's KMS handle is now `h`, for calls and for release. */
static void nvgpu_kms_publish(struct nvgpu_kms_file *kf, u32 h) {
  WRITE_ONCE(kf->handle, h);
  /* nvgpu_fd_detach_drm() takes it at release; no ioctl can be running then,
   * nor this. */
  WRITE_ONCE(kf->nfd->kms_handle, h);
}

/*
 * A new host card file for this guest file (HOST_OP OPEN_KMS). The host makes
 * it master if nothing else is (drm_auth.c:317-335). Unless the guest file is
 * master itself that must not stand: DROP_IF_MASTER undoes it at once, and
 * says whether it had been master -- in which case the host file keeps
 * was_master and can be made master again later. Refused if the drop cannot
 * be done: a card this file might hold host master through is not handed out.
 */
static int nvgpu_kms_open_card(struct nvgpu_kms_file *kf, bool as_master,
                               u32 *out, bool *was_master) {
  struct nvgpu_device *dev = kf->dev;
  u64 args[2] = {kf->nfd->handle, (u64)kf->dri->card_index};
  u64 res[1] = {};
  u32 h;
  int ret;

  *was_master = false;
  ret = nvgpu_host_op(dev, NVGPU_OP_OPEN_KMS, args, 2, res, 1);
  if (ret) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: the backend would not open host card "
                         "%d for a guest card file: %d\n",
                         kf->dri->card_index, ret);
    return ret;
  }
  h = (u32)res[0];
  if (!h)
    return -EPROTO;
  if (!as_master) {
    args[0] = h;
    ret = nvgpu_host_op(dev, NVGPU_OP_DROP_IF_MASTER, args, 1, res, 1);
    if (ret) {
      dev_warn_ratelimited(&dev->vdev->dev,
                           "virtio-gpu-nv: a host card file opened for a "
                           "non-master guest file could not be dropped from "
                           "host master (%d); closing it\n",
                           ret);
      nvgpu_close_handle(dev, h);
      return ret;
    }
    *was_master = res[0] == 1;
  }
  *out = h;
  return 0;
}

static void nvgpu_kms_replay_caps(struct nvgpu_kms_file *kf, u32 h) {
  u32 i;

  for (i = 0; i < NVGPU_KMS_NCAPS; i++) {
    struct drm_set_client_cap c = {.capability = i, .value = kf->caps[i]};
    s32 hr = 0;
    int ret;

    if (!(kf->caps_set & BIT(i)))
      continue;
    ret = nvgpu_kms_raw(kf, h, DRM_IOCTL_SET_CLIENT_CAP, &c, &hr);
    if (ret || hr)
      dev_warn_ratelimited(&kf->dev->vdev->dev,
                           "virtio-gpu-nv: client cap %u=%llu could not be "
                           "set again on the new host card file: %d\n",
                           i, c.value, ret ? ret : hr);
  }
}

/*
 * The file's KMS handle, opening the host card on first use. A file that is
 * guest master gets the card as host master; any other has it dropped.
 */
static int nvgpu_kms_get_handle(struct nvgpu_kms_file *kf,
                                struct drm_file *file, u32 *out) {
  u32 h = READ_ONCE(kf->handle);
  bool master, was;
  int ret = 0;

  if (h) {
    *out = h;
    return 0;
  }
  if (kf->adopted)
    return -ENODEV;

  mutex_lock(&kf->lock);
  if (!kf->handle) {
    master = drm_is_current_master(file);
    ret = nvgpu_kms_open_card(kf, master, &h, &was);
    if (!ret) {
      ret = nvgpu_kms_bind(kf, 0, h);
      if (ret)
        nvgpu_close_handle(kf->dev, h);
    }
    if (!ret) {
      kf->host_master_ok = was;
      /* Guest master whose hook could not open the card then: take host
       * master now, as the hook would have. */
      if (master && !nvgpu_kms_master_call(kf, h, DRM_IOCTL_SET_MASTER))
        kf->host_master_ok = true;
      nvgpu_kms_publish(kf, h);
    }
  }
  *out = kf->handle;
  mutex_unlock(&kf->lock);
  return ret;
}

/*
 * The guest file became master, but its host file never was host master and
 * so never can be. Open another -- host master straight away, the previous
 * guest master having dropped it -- and carry on with that, keeping the old
 * one open, not master, for what was made on it.
 */
static int nvgpu_kms_swap(struct nvgpu_kms_file *kf) {
  u32 old = kf->handle, h;
  bool was;
  int ret;

  ret = nvgpu_kms_open_card(kf, true, &h, &was);
  if (ret)
    return ret;
  ret = nvgpu_kms_master_call(kf, h, DRM_IOCTL_SET_MASTER);
  if (!ret)
    ret = nvgpu_kms_bind(kf, 1, h);
  if (ret) {
    nvgpu_close_handle(kf->dev, h);
    return ret;
  }
  nvgpu_kms_replay_caps(kf, h);
  kf->retired = old;
  kf->host_master_ok = true;
  nvgpu_kms_publish(kf, h);
  dev_info(&kf->dev->vdev->dev,
           "virtio-gpu-nv: host card file %u was opened while another file "
           "was master and can never be; guest master now drives %u\n",
           old, h);
  return 0;
}

void nvgpu_kms_master_set(struct drm_file *file, bool new_master) {
  struct nvgpu_fd *nfd = file->driver_priv;
  struct nvgpu_kms_file *kf = nfd ? nfd->kms : NULL;
  bool was;
  u32 h;
  int ret;

  if (!kf || kf->adopted)
    return;

  mutex_lock(&kf->lock);
  if (!kf->handle) {
    ret = nvgpu_kms_open_card(kf, true, &h, &was);
    if (!ret) {
      ret = nvgpu_kms_bind(kf, 0, h);
      if (ret)
        nvgpu_close_handle(kf->dev, h);
    }
    if (ret)
      goto fail;
    nvgpu_kms_publish(kf, h);
  }
  /* Also for a card opened just now: it is master only if the host had none,
   * and this says which (0 if it already is, drm_auth.c:252). */
  ret = nvgpu_kms_master_call(kf, kf->handle, DRM_IOCTL_SET_MASTER);
  if (ret == -EACCES && !kf->host_master_ok && !kf->retired)
    ret = nvgpu_kms_swap(kf);
  if (ret)
    goto fail;
  kf->host_master_ok = true;
  mutex_unlock(&kf->lock);
  return;

fail:
  mutex_unlock(&kf->lock);
  dev_warn_ratelimited(&kf->dev->vdev->dev,
                       "virtio-gpu-nv: a guest file became DRM master but "
                       "its host card file could not (%d): another host "
                       "process holds the card, so its commits will fail\n",
                       ret);
}

void nvgpu_kms_master_drop(struct drm_file *file) {
  struct nvgpu_fd *nfd = file->driver_priv;
  struct nvgpu_kms_file *kf = nfd ? nfd->kms : NULL;
  int ret = 0;

  if (!kf || kf->adopted)
    return;
  mutex_lock(&kf->lock);
  /* -EINVAL: the host file was not master anyway (drm_auth.c:298). */
  if (kf->handle)
    ret = nvgpu_kms_master_call(kf, kf->handle, DRM_IOCTL_DROP_MASTER);
  mutex_unlock(&kf->lock);
  if (ret && ret != -EINVAL)
    dev_warn_ratelimited(&kf->dev->vdev->dev,
                         "virtio-gpu-nv: guest DRM master was dropped but "
                         "the host card file stays master (%d)\n",
                         ret);
}

/* ───────── adoption ─────────
 *
 * driver->open has no argument through which to say "this open is for backend
 * handle H" (drm_drv.h), and the clone goes through the whole VFS open. So
 * the adopter leaves H in a slot, marked with the task doing the open, and
 * nvgpu_drm_open() -- running in that same task, inside dentry_open() --
 * takes it (RV:adopt). The lock only serialises adopters; the open reads the
 * slot without it, since the adopter is holding it.
 */
static DEFINE_MUTEX(nvgpu_adopt_lock);
static struct {
  struct task_struct *task;
  struct nvgpu_dri_dev *dri;
  u32 handle;
  bool consumed;
} nvgpu_adopt;

static struct nvgpu_kms_file *nvgpu_kms_alloc(struct nvgpu_dri_dev *dri,
                                              struct drm_file *file,
                                              struct nvgpu_fd *nfd) {
  struct nvgpu_kms_file *kf = kzalloc(sizeof(*kf), GFP_KERNEL);

  if (!kf)
    return NULL;
  kf->dev = dri->dev;
  kf->dri = dri;
  kf->drm = file->minor->dev;
  kf->nfd = nfd;
  mutex_init(&kf->lock);
  INIT_LIST_HEAD(&kf->pending);
  INIT_LIST_HEAD(&kf->waiters);
  xa_init(&kf->props);
  xa_init(&kf->objs);
  return kf;
}

static void nvgpu_kms_free(struct nvgpu_kms_file *kf) {
  xa_destroy(&kf->props);
  xa_destroy(&kf->objs);
  mutex_destroy(&kf->lock);
  kfree(kf);
}

int nvgpu_kms_open(struct nvgpu_dri_dev *dri, struct drm_file *file,
                   struct nvgpu_fd *nfd) {
  struct nvgpu_device *dev = dri->dev;
  struct nvgpu_kms_file *kf;
  bool adopt, card;
  int ret;

  adopt = READ_ONCE(nvgpu_adopt.task) == current &&
          READ_ONCE(nvgpu_adopt.dri) == dri;
  card = !adopt && dev->v2 && (dev->backend_caps & NVGPU_BCAP_KMS_CARD) &&
         drm_is_primary_client(file) && dri->card_index >= 0 &&
         dri->card_index < dev->num_card_recs;
  if (!adopt && !card)
    return 0;

  kf = nvgpu_kms_alloc(dri, file, nfd);
  if (!kf)
    return -ENOMEM;
  if (adopt) {
    kf->adopted = true;
    ret = nvgpu_kms_bind(kf, 0, nvgpu_adopt.handle);
    if (ret) {
      nvgpu_kms_free(kf);
      return ret;
    }
    /* From here the drm_file owns the handle: a failure later in the open
     * (drm_master_open, drm_file.c:339) frees the file through postclose,
     * which closes it. */
    nvgpu_kms_publish(kf, nvgpu_adopt.handle);
    nvgpu_adopt.consumed = true;
  }
  nfd->kms = kf;
  return 0;
}

int nvgpu_adopt_drm_file(struct file *tmpl, u32 kms_handle, u32 kind,
                         int o_flags) {
  struct nvgpu_fd *tnfd = tmpl ? nvgpu_drm_file_nfd(tmpl) : NULL;
  struct nvgpu_dri_dev *dri;
  struct nvgpu_device *dev;
  struct drm_file *tfile;
  struct file *f;
  bool consumed;
  int fd, ret;

  if (!tnfd)
    return -EBADF;
  dev = tnfd->dev;
  tfile = tmpl->private_data;
  dri = tfile->minor->dev->dev_private;

  /*
   * A clone of a render node would be a renderD file, where libdrm and
   * vkAcquireDrmDisplayEXT refuse KMS; and only a lease is a KMS file we can
   * hand out -- the backend classifies every received card fd of our GPU as
   * one (hostfd.rs classify()).
   */
  if (!drm_is_primary_client(tfile) || !dri || dri->dev != dev) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: a host DRM file can only be adopted "
                         "through a card-node file of the same GPU\n");
    ret = -EINVAL;
    goto close;
  }
  if (kind != NVGPU_HK_DRM_LEASE) {
    dev_warn_ratelimited(&dev->vdev->dev,
                         "virtio-gpu-nv: refusing to adopt backend handle %u "
                         "of kind %u as a DRM file: only leases are\n",
                         kms_handle, kind);
    ret = -EINVAL;
    goto close;
  }

  /* The descriptor first, so that nothing can fail once the clone holds the
   * handle but an fd_install(). */
  fd = get_unused_fd_flags(o_flags & O_CLOEXEC);
  if (fd < 0) {
    ret = fd;
    goto close;
  }

  mutex_lock(&nvgpu_adopt_lock);
  nvgpu_adopt.dri = dri;
  nvgpu_adopt.handle = kms_handle;
  nvgpu_adopt.consumed = false;
  WRITE_ONCE(nvgpu_adopt.task, current);
  /*
   * file_clone_open() (fs.h), which is what CREATE_LEASE itself does for a
   * lessee (drm_lease.c:551) -- but read/write whatever the template was
   * opened with, so the caller can map dumb buffers, and non-blocking only if
   * asked (RV:adopt d).
   */
  f = dentry_open(&tmpl->f_path, O_RDWR | (o_flags & O_NONBLOCK),
                  tmpl->f_cred);
  consumed = nvgpu_adopt.consumed;
  WRITE_ONCE(nvgpu_adopt.task, NULL);
  nvgpu_adopt.dri = NULL;
  mutex_unlock(&nvgpu_adopt_lock);

  if (IS_ERR(f)) {
    put_unused_fd(fd);
    ret = PTR_ERR(f);
    /* Taken and then let go by the failed clone's own release. */
    if (consumed)
      return ret == -EBADF ? -EIO : ret;
    goto close;
  }
  if (!consumed) {
    /* Our node's open always runs nvgpu_drm_open(), which either takes the
     * slot or fails; an open that did neither is not ours to hand out. */
    fput(f);
    put_unused_fd(fd);
    ret = -EIO;
    goto close;
  }
  fd_install(fd, f);
  return fd;

close:
  nvgpu_close_handle(dev, kms_handle);
  return ret == -EBADF ? -EIO : ret;
}

/* ───────── events ───────── */

static bool nvgpu_kms_is_cookie(u64 v) {
  return (v >> NVGPU_KMS_COOKIE_SHIFT) == NVGPU_KMS_COOKIE_TAG;
}

/* A vblank/flip timestamp, host CLOCK_MONOTONIC -> the guest's (both u32, as
 * struct drm_event_vblank and the WAIT_VBLANK reply carry them). */
static void nvgpu_kms_tv(struct nvgpu_device *dev, u32 *sec, u32 *usec) {
  s64 ns = (s64)*sec * NSEC_PER_SEC + (s64)*usec * NSEC_PER_USEC;
  u32 rem;

  ns = nvgpu_host_to_guest_ns(dev, ns);
  if (ns < 0)
    ns = 0;
  *sec = (u32)div_u64_rem((u64)ns, NSEC_PER_SEC, &rem);
  *usec = rem / NSEC_PER_USEC;
}

/* The oldest reservation this event answers. Caller holds event_lock. */
static struct nvgpu_kms_pev *nvgpu_kms_match(struct nvgpu_kms_file *kf,
                                             u32 type, u32 crtc,
                                             u64 user_data) {
  struct nvgpu_kms_pev *p;

  list_for_each_entry(p, &kf->pending, node) {
    if (p->ev.e.type == type && p->user_data == user_data &&
        (!p->crtc_id || p->crtc_id == crtc)) {
      list_del_init(&p->node);
      return p;
    }
  }
  return NULL;
}

/*
 * An event nobody reserved for: a CRTC an atomic commit dragged in that we
 * could not see coming (a plane's current CRTC we had never been told), or
 * the late answer to a call that gave up. Reserved now if the file has room,
 * as the core would have at submission. Caller holds event_lock.
 */
static struct nvgpu_kms_pev *nvgpu_kms_late(struct nvgpu_kms_file *kf,
                                            struct drm_file *file, u32 type) {
  struct nvgpu_kms_pev *p = kzalloc(sizeof(*p), GFP_ATOMIC);

  if (!p)
    return NULL;
  INIT_LIST_HEAD(&p->node);
  p->ev.e.type = type;
  p->ev.e.length = sizeof(p->ev.vbl);
  if (drm_event_reserve_init_locked(kf->drm, file, &p->base, &p->ev.e)) {
    kfree(p);
    return NULL;
  }
  return p;
}

/* One host drm_event, `len` bytes. Caller holds event_lock; hard IRQ. */
static void nvgpu_kms_event(struct nvgpu_kms_file *kf, struct drm_file *file,
                            const u8 *raw, u32 len) {
  struct nvgpu_kms_pev *p;
  struct drm_event e;

  BUILD_BUG_ON(sizeof(struct drm_event_vblank) !=
               sizeof(struct drm_event_crtc_sequence));
  memcpy(&e, raw, sizeof(e));

  switch (e.type) {
  case DRM_EVENT_VBLANK:
  case DRM_EVENT_FLIP_COMPLETE: {
    struct drm_event_vblank vbl;

    if (len != sizeof(vbl))
      goto bad;
    memcpy(&vbl, raw, sizeof(vbl));
    nvgpu_kms_tv(kf->dev, &vbl.tv_sec, &vbl.tv_usec);

    if (e.type == DRM_EVENT_VBLANK && nvgpu_kms_is_cookie(vbl.user_data)) {
      struct nvgpu_kms_wait *w;

      list_for_each_entry(w, &kf->waiters, node) {
        if (w->cookie == vbl.user_data) {
          w->ev = vbl;
          list_del_init(&w->node);
          complete(&w->done);
          return;
        }
      }
      return; /* a wait that has given up (timeout, signal) */
    }
    p = nvgpu_kms_match(kf, e.type, vbl.crtc_id, vbl.user_data);
    if (!p)
      p = nvgpu_kms_late(kf, file, e.type);
    if (!p)
      goto full;
    p->ev.vbl = vbl;
    drm_send_event_locked(kf->drm, &p->base);
    return;
  }
  case DRM_EVENT_CRTC_SEQUENCE: {
    struct drm_event_crtc_sequence seq;

    if (len != sizeof(seq))
      goto bad;
    memcpy(&seq, raw, sizeof(seq));
    if (seq.time_ns)
      seq.time_ns = nvgpu_host_to_guest_ns(kf->dev, seq.time_ns);
    p = nvgpu_kms_match(kf, e.type, 0, seq.user_data);
    if (!p)
      p = nvgpu_kms_late(kf, file, e.type);
    if (!p)
      goto full;
    p->ev.seq = seq;
    drm_send_event_locked(kf->drm, &p->base);
    return;
  }
  }

bad:
  /* nvidia-drm sends only these three (R:nvdrm §7). */
  dev_warn_ratelimited(&kf->dev->vdev->dev,
                       "virtio-gpu-nv: host DRM event of type %u, %u bytes, "
                       "is not one this driver knows; dropped\n",
                       e.type, len);
  return;
full:
  dev_warn_ratelimited(&kf->dev->vdev->dev,
                       "virtio-gpu-nv: a host DRM event (type %u) nobody "
                       "reserved for found its file's event space full; "
                       "dropped, and its reader may wait for it forever\n",
                       e.type);
}

/*
 * EV_DRM for one of this file's KMS handles: a run of whole drm_events, as
 * the backend read them off the host file (records never split one). Hard
 * IRQ, under the event registry lock; the drm_file is reachable only while
 * the file is not released (nvgpu_fd_detach_drm() clears it under that lock).
 */
static void nvgpu_kms_deliver(struct nvgpu_ev_consumer *c, u32 kind,
                              u64 cookie, const void *payload, u32 len) {
  struct nvgpu_kms_evc *evc = container_of(c, struct nvgpu_kms_evc, c);
  struct nvgpu_kms_file *kf = evc->kf;
  const u8 *p = payload;
  struct drm_file *file;
  unsigned long flags;
  u32 off = 0;

  if (kind != NVGPU_EV_DRM)
    return;
  file = READ_ONCE(kf->nfd->drm_file);
  if (!file)
    return;

  spin_lock_irqsave(&kf->drm->event_lock, flags);
  while (len - off >= sizeof(struct drm_event)) {
    struct drm_event e;

    memcpy(&e, p + off, sizeof(e));
    if (e.length < sizeof(e) || e.length > len - off) {
      dev_warn_ratelimited(&kf->dev->vdev->dev,
                           "virtio-gpu-nv: EV_DRM for handle %llu holds a "
                           "%u-byte event with %u bytes left; rest dropped\n",
                           cookie, e.length, len - off);
      break;
    }
    nvgpu_kms_event(kf, file, p + off, e.length);
    off += e.length;
  }
  spin_unlock_irqrestore(&kf->drm->event_lock, flags);
}

/*
 * Reserve an event of `type` in the caller's file before the request goes, as
 * the core does (so -ENOMEM comes back now, not as a flip that never
 * completes), and put it where its host event will find it.
 */
static int nvgpu_kms_reserve(struct nvgpu_kms_call *kc, u32 type, u32 crtc,
                             u64 user_data) {
  struct nvgpu_kms_file *kf = kc->kf;
  struct nvgpu_kms_pev *p;
  unsigned long flags;
  int ret;

  if (kc->nev >= NVGPU_KMS_MAX_EVENTS)
    return 0;
  p = kzalloc(sizeof(*p), GFP_KERNEL);
  if (!p)
    return -ENOMEM;
  INIT_LIST_HEAD(&p->node);
  p->ev.e.type = type;
  p->ev.e.length = sizeof(p->ev.vbl);
  p->crtc_id = crtc;
  p->user_data = user_data;
  p->owner = kc;
  ret = drm_event_reserve_init(kf->drm, kc->file, &p->base, &p->ev.e);
  if (ret) {
    kfree(p);
    return ret;
  }
  spin_lock_irqsave(&kf->drm->event_lock, flags);
  list_add_tail(&p->node, &kf->pending);
  spin_unlock_irqrestore(&kf->drm->event_lock, flags);
  kc->nev++;
  return 0;
}

/*
 * The call is over. If the host did it, its reservations stay for their
 * events (and forget the call, whose memory goes); if not -- refused, failed,
 * abandoned -- they are given back. Only entries still on the pending list
 * are touched: a delivered one belongs to the core.
 */
static void nvgpu_kms_settle(struct nvgpu_kms_call *kc) {
  struct nvgpu_kms_file *kf = kc->kf;
  bool ok = kc->replied && !kc->host_ret;
  struct nvgpu_kms_pev *p, *n;
  unsigned long flags;
  LIST_HEAD(cancel);
  u32 i;

  spin_lock_irqsave(&kf->drm->event_lock, flags);
  if (kc->nev) {
    list_for_each_entry_safe(p, n, &kf->pending, node) {
      if (p->owner != kc)
        continue;
      if (ok)
        p->owner = NULL;
      else
        list_move(&p->node, &cancel);
    }
  }
  if (kc->waiting && !list_empty(&kc->wait.node))
    list_del_init(&kc->wait.node);
  spin_unlock_irqrestore(&kf->drm->event_lock, flags);

  list_for_each_entry_safe(p, n, &cancel, node) {
    list_del_init(&p->node);
    drm_event_cancel_free(kf->drm, &p->base);
  }

  if (!ok)
    return;
  /* A committed CRTC_ID is the object's CRTC from now on (a plane's current
   * CRTC joins every commit that touches the plane, drm_atomic.c:583). */
  for (i = 0; kc->commit && i < kc->nlearn; i++)
    xa_store(&kf->objs, kc->learn[i].obj,
             xa_mk_value(NVGPU_KOBJ_OTHER | ((unsigned long)kc->learn[i].crtc
                                             << 2)),
             GFP_KERNEL);
  if (kc->cap_set && kc->cap < NVGPU_KMS_NCAPS) {
    mutex_lock(&kf->lock);
    kf->caps[kc->cap] = kc->cap_value;
    kf->caps_set |= BIT(kc->cap);
    mutex_unlock(&kf->lock);
  }
}

/* ───────── what objects and properties are ───────── */

/* By name, the same way the backend classifies them (policy.rs
 * kms_prop_kind): the core's fence properties, nvidia-drm's
 * NV_DRM_OUT_FENCE_PTR, and whatever a newer kernel names *_PTR or *_FD. */
static u32 nvgpu_kms_prop_kind_of(const char *name) {
  size_t n = strnlen(name, DRM_PROP_NAME_LEN);

  if (n == 7 && !memcmp(name, "CRTC_ID", 7))
    return NVGPU_KPROP_CRTC_ID;
  if (n >= 4 && !memcmp(name + n - 4, "_PTR", 4))
    return NVGPU_KPROP_OUT_PTR;
  if (n >= 3 && !memcmp(name + n - 3, "_FD", 3))
    return NVGPU_KPROP_IN_FENCE;
  return NVGPU_KPROP_PLAIN;
}

/* A property's kind; GETPROPERTY on the host the first time an id is seen
 * (every count zero, so neither pointer is written, drm_property.c:458). */
static int nvgpu_kms_prop_class(struct nvgpu_kms_call *kc, u32 id) {
  struct nvgpu_kms_file *kf = kc->kf;
  struct drm_mode_get_property gp = {.prop_id = id};
  void *e = xa_load(&kf->props, id);
  s32 hr = 0;
  u32 kind;
  int ret;

  if (e)
    return (int)xa_to_value(e);
  ret = nvgpu_kms_raw(kf, kc->call.handle, DRM_IOCTL_MODE_GETPROPERTY, &gp,
                      &hr);
  if (ret)
    return ret;
  if (hr)
    return hr; /* an unknown id: the commit would fail on it too */
  kind = nvgpu_kms_prop_kind_of(gp.name);
  xa_store(&kf->props, id, xa_mk_value(kind), GFP_KERNEL);
  return kind;
}

/* NVGPU_KOBJ_CRTC, or NVGPU_KOBJ_OTHER with its CRTC (0: not known) in
 * *crtc. GETCRTC answers for CRTCs and -ENOENT for anything else,
 * lease-filtered alike (drm_crtc.c:553). */
static int nvgpu_kms_obj_class(struct nvgpu_kms_call *kc, u32 obj,
                               u32 *crtc) {
  struct nvgpu_kms_file *kf = kc->kf;
  void *e = xa_load(&kf->objs, obj);
  unsigned long v;

  if (!e) {
    struct drm_mode_crtc c = {.crtc_id = obj};
    s32 hr = 0;
    int ret;

    ret = nvgpu_kms_raw(kf, kc->call.handle, DRM_IOCTL_MODE_GETCRTC, &c, &hr);
    if (ret)
      return ret;
    v = hr ? NVGPU_KOBJ_OTHER : NVGPU_KOBJ_CRTC;
    /* Never over something a commit or GETPLANE taught us meanwhile (-EBUSY
     * then); a cache that cannot grow (-ENOMEM) just asks again next time. */
    if ((!hr || hr == -ENOENT) &&
        xa_insert(&kf->objs, obj, xa_mk_value(v), GFP_KERNEL) == -EBUSY) {
      e = xa_load(&kf->objs, obj);
      if (e)
        v = xa_to_value(e);
    }
  } else {
    v = xa_to_value(e);
  }
  *crtc = (u32)(v >> 2);
  return (int)(v & 3);
}

/* ───────── ATOMIC ───────── */

/* Past NVGPU_KMS_MAX_EVENTS a CRTC's event is reserved when it arrives. */
static void nvgpu_kms_add_crtc(u32 *crtcs, u32 *n, u32 crtc) {
  u32 i;

  for (i = 0; i < *n; i++)
    if (crtcs[i] == crtc)
      return;
  if (*n < NVGPU_KMS_MAX_EVENTS)
    crtcs[(*n)++] = crtc;
}

/*
 * IN_FENCE_FD: the backend sync_file behind the caller's fence descriptor. A
 * fence that has already signalled -- or one only the guest could see, which
 * nvgpu_fence_unwrap_fd() has waited for -- goes as -1, "no fence"
 * (drm_atomic_uapi.c:551), rewritten in the copy the host gets.
 */
static int nvgpu_kms_in_fence(struct nvgpu_kms_call *kc, u32 buf, u32 off,
                              s64 fd) {
  bool owned = false;
  u8 *vals;
  u32 h = 0, len;
  int ret;

  /* sync_file_get_fence() of anything that is not one is -EINVAL
   * (drm_atomic_uapi.c:558). */
  if (fd < 0 || fd > INT_MAX)
    return -EINVAL;
  /*
   * A TEST_ONLY commit waits on nothing natively: the core takes a reference
   * on the fence (drm_atomic_uapi.c:551-560) and the check never looks at
   * it. So it is only checked for being a sync_file, and the host checks the
   * commit without it -- unwrapping here would wait for a guest-only fence,
   * or merge on the host, for a commit that will never scan out.
   */
  if (!kc->commit) {
    struct dma_fence *f = sync_file_get_fence((int)fd);

    if (!f)
      return -EINVAL;
    dma_fence_put(f);
    vals = nvgpu_i2_buf(&kc->call, buf, &len);
    if (!vals || off > len || len - off < 8)
      return -EINVAL;
    put_unaligned_le64((u64)-1, vals + off);
    return 0;
  }
  ret = nvgpu_fence_unwrap_fd(kc->kf->dev, (int)fd, &h, &owned);
  if (ret < 0)
    return ret;
  if (ret > 0) {
    vals = nvgpu_i2_buf(&kc->call, buf, &len);
    if (!vals || off > len || len - off < 8)
      return -EINVAL;
    put_unaligned_le64((u64)-1, vals + off);
    return 0;
  }
  ret = nvgpu_i2_add_fd(&kc->call, buf, off, h, owned ? NVGPU_I2_FD_CONSUME : 0);
  if (ret && owned)
    nvgpu_close_handle_async(kc->kf->dev, h);
  return ret;
}

/*
 * OUT_FENCE_PTR and its kin: -1 written through the caller's pointer now, as
 * the core does when the property is set (drm_atomic_uapi.c:473, a bad
 * pointer failing the commit), and a dyn record so the backend points the
 * value at an s32 of its own and hands the sync_file back (fd_out below).
 */
static int nvgpu_kms_out_fence(struct nvgpu_kms_call *kc, u32 buf, u32 off,
                               u64 uptr) {
  int ret;

  if (kc->nfence >= NVGPU_KMS_MAX_FENCES)
    return -E2BIG;
  if (put_user(-1, (s32 __user *)u64_to_user_ptr(uptr)))
    return -EFAULT;
  ret = nvgpu_i2_add_dyn(&kc->call, NVGPU_I2_DYN_OUT_FENCE, buf, off, 4);
  if (ret)
    return ret;
  kc->fence_off[kc->nfence] = off;
  kc->fence_uptr[kc->nfence] = uptr;
  kc->nfence++;
  return 0;
}

/*
 * Before an atomic commit goes: which CRTCs it touches, for their flip
 * events, and its fence properties. Buffers follow the canonical traversal:
 * after the argument, each of objs, count_props, props and prop_values that is
 * non-NULL with a non-zero length, in that order (gen/schema/drm_kms.py).
 *
 * The kernel makes one event per CRTC whose state the commit carries
 * (drm_atomic_uapi.c:1430): a CRTC object with properties set, the CRTC a
 * plane or connector is put on (CRTC_ID), and the CRTC a touched plane or
 * connector is already on (drm_atomic.c:583, 1334). The last we know only as
 * far as GETPLANE replies and earlier commits told us; an event for a CRTC we
 * missed is reserved when it arrives instead.
 */
static int nvgpu_kms_atomic(struct nvgpu_kms_call *kc) {
  struct nvgpu_i2_call *call = &kc->call;
  struct nvgpu_kms_file *kf = kc->kf;
  u32 len, l1, l2, l3, l4, idx = 1, bobjs, bcp, bprops = 0, bvals = 0;
  u32 crtcs[NVGPU_KMS_MAX_EVENTS], ncrtc = 0;
  u8 *b0 = nvgpu_i2_buf(call, 0, &len);
  u8 *objs, *cp, *props, *vals;
  u32 flags, count, o, j, k = 0;
  bool events, fences;
  u64 sum = 0;
  int ret;

  if (!b0 || len < sizeof(struct drm_mode_atomic))
    return 0;
  flags = get_unaligned_le32(b0 + offsetof(struct drm_mode_atomic, flags));
  count = get_unaligned_le32(b0 + offsetof(struct drm_mode_atomic, count_objs));
  kc->commit = !(flags & DRM_MODE_ATOMIC_TEST_ONLY);
  events = (flags & DRM_MODE_PAGE_FLIP_EVENT) && kc->commit;
  fences = kf->dev->backend_caps & NVGPU_BCAP_FENCES;
  /*
   * Without the fence bridge a fence property is the backend's to refuse
   * (-EOPNOTSUPP, policy.rs FENCES), which it does from its own copy; there
   * is nothing to look up for it here.
   */
  if (!count || (!events && !fences))
    return 0;

  bobjs = get_unaligned_le64(b0 + offsetof(struct drm_mode_atomic, objs_ptr))
              ? idx++
              : 0;
  bcp = get_unaligned_le64(b0 + offsetof(struct drm_mode_atomic,
                                         count_props_ptr))
            ? idx++
            : 0;
  if (!bobjs || !bcp)
    return 0; /* the host faults on it; nothing will be made */
  objs = nvgpu_i2_buf(call, bobjs, &l1);
  cp = nvgpu_i2_buf(call, bcp, &l2);
  if (!objs || !cp || l1 != count * 4 || l2 != count * 4)
    return 0;
  for (o = 0; o < count; o++)
    sum += get_unaligned_le32(cp + 4 * o);
  if (sum && get_unaligned_le64(b0 + offsetof(struct drm_mode_atomic, props_ptr)))
    bprops = idx++;
  if (sum && get_unaligned_le64(b0 + offsetof(struct drm_mode_atomic,
                                              prop_values_ptr)))
    bvals = idx++;
  props = bprops ? nvgpu_i2_buf(call, bprops, &l3) : NULL;
  vals = bvals ? nvgpu_i2_buf(call, bvals, &l4) : NULL;
  if (sum && (!props || !vals || l3 != sum * 4 || l4 != sum * 8))
    return 0;
  kc->values_buf = bvals;

  for (o = 0; o < count; o++) {
    u32 obj = get_unaligned_le32(objs + 4 * o);
    u32 n = get_unaligned_le32(cp + 4 * o), on;

    if (n && events) {
      ret = nvgpu_kms_obj_class(kc, obj, &on);
      if (ret < 0)
        return ret;
      if (ret == NVGPU_KOBJ_CRTC)
        nvgpu_kms_add_crtc(crtcs, &ncrtc, obj);
      else if (on)
        nvgpu_kms_add_crtc(crtcs, &ncrtc, on);
    }
    for (j = 0; j < n; j++, k++) {
      u32 id = get_unaligned_le32(props + 4 * k);
      u64 v = get_unaligned_le64(vals + 8 * k);

      ret = nvgpu_kms_prop_class(kc, id);
      if (ret < 0)
        return ret;
      switch (ret) {
      case NVGPU_KPROP_CRTC_ID:
        if (events && v)
          nvgpu_kms_add_crtc(crtcs, &ncrtc, (u32)v);
        if (kc->nlearn < NVGPU_KMS_MAX_LEARN) {
          kc->learn[kc->nlearn].obj = obj;
          kc->learn[kc->nlearn].crtc = (u32)v;
          kc->nlearn++;
        }
        break;
      case NVGPU_KPROP_IN_FENCE:
        if (fences && (s64)v != -1) {
          ret = nvgpu_kms_in_fence(kc, bvals, 8 * k, (s64)v);
          if (ret)
            return ret;
        }
        break;
      case NVGPU_KPROP_OUT_PTR:
        if (fences && v) {
          ret = nvgpu_kms_out_fence(kc, bvals, 8 * k, v);
          if (ret)
            return ret;
        }
        break;
      }
    }
  }

  for (o = 0; events && o < ncrtc; o++) {
    ret = nvgpu_kms_reserve(
        kc, DRM_EVENT_FLIP_COMPLETE, crtcs[o],
        get_unaligned_le64(b0 + offsetof(struct drm_mode_atomic, user_data)));
    if (ret)
      return ret;
  }
  return 0;
}

/* ───────── the interpreter's hooks ───────── */

/* GRANT_PERMISSIONS' nvidia-modeset descriptor, and any other descriptor of
 * one of our character devices a KMS-class entry takes. */
static int nvgpu_kms_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off,
                           s64 user_value, u32 kinds, u32 *handle,
                           u32 *flags) {
  struct nvgpu_kms_call *kc = to_kms_call(call);
  struct nvgpu_fd *o;
  struct file *f;
  int ret = -EBADF;

  if (user_value < 0 || user_value > INT_MAX)
    return -EBADF;
  f = fget((int)user_value);
  if (!f)
    return -EBADF;
  o = nvgpu_fd_from_file(f);
  if (o && o->dev == kc->kf->dev &&
      (((kinds & NVGPU_SKIND_DEV_MODESET) &&
        o->device_type == NVGPU_DEV_MODESET) ||
       ((kinds & NVGPU_SKIND_DEV_CTL) && o->device_type == NVGPU_DEV_CTL) ||
       ((kinds & NVGPU_SKIND_DEV_GPU) && o->device_type < NVGPU_DEV_CTL) ||
       ((kinds & NVGPU_SKIND(NVGPU_HK_DEV)) &&
        o->device_type < NVGPU_DEV_DRI_BASE))) {
    *handle = o->handle;
    ret = 0;
  }
  fput(f);
  if (ret)
    dev_warn_ratelimited(&kc->kf->dev->vdev->dev,
                         "virtio-gpu-nv: KMS ioctl nr=0x%02x names fd %lld, "
                         "which is not the kind of device it takes\n",
                         _IOC_NR(call->cmd), user_value);
  return ret;
}

/* A framebuffer's or cursor's GEM handle: our proxy's (owner, host GEM); the
 * backend re-homes it into the KMS file for the one call (RV:rehome). */
static int nvgpu_kms_gem_in(struct nvgpu_i2_call *call, u32 buf, u32 off,
                            u32 guest_handle, u32 *owner, u32 *gem) {
  return nvgpu_gem_to_host(to_kms_call(call)->file, guest_handle, gem, owner);
}

/*
 * A GEM handle the call made (CREATE_DUMB, GETFB, GETFB2), already moved
 * into this file's render handle. The move is a PRIME import, and a file that
 * already had the object gets back the handle it had (drm_prime.c:304), which
 * an existing proxy owns -- so that proxy it is, with a new guest handle to it
 * as the native ioctls make (drm_framebuffer.c:557, 663).
 *
 * The interpreter closes a GEM output whose hook failed; ours are owned by a
 * proxy (made, or found) or already closed by a failed creation, so a failure
 * here is reported through the call's result instead and the field left 0.
 */
static int nvgpu_kms_gem_out(struct nvgpu_i2_call *call, u32 buf, u32 off,
                             u32 gem, u64 size, u32 *guest_handle) {
  struct nvgpu_kms_call *kc = to_kms_call(call);
  struct drm_gem_object *obj = nvgpu_gem_proxy_find(kc->kf->nfd, gem);
  int ret;

  if (obj) {
    ret = drm_gem_handle_create(kc->file, obj, guest_handle);
    drm_gem_object_put(obj);
  } else {
    ret = nvgpu_gem_proxy_create(kc->file, kc->kf->nfd, gem, (size_t)size,
                                 guest_handle);
  }
  if (ret) {
    *guest_handle = 0;
    call->ret = ret;
  }
  return 0;
}

/*
 * A descriptor the host made: CREATE_LEASE's lessee, adopted as a clone of
 * the caller's own file -- file_clone_open(lessor) is what the kernel does
 * (drm_lease.c:551), keeping the lessor's O_NONBLOCK -- or an atomic commit's
 * out-fence, a sync_file written through the caller's pointer.
 */
static int nvgpu_kms_fd_out(struct nvgpu_i2_call *call, u32 buf, u32 off,
                            u32 handle, u32 kind, s64 *user_value) {
  struct nvgpu_kms_call *kc = to_kms_call(call);
  u32 i, len;
  int fd;

  if (kc->values_buf && buf == kc->values_buf) {
    for (i = 0; i < kc->nfence && kc->fence_off[i] != off; i++)
      ;
    if (i == kc->nfence || kind != NVGPU_HK_SYNC_FILE)
      return -EINVAL;
    /* The interpreter closes the handle if this hook fails: the _noclose
     * form leaves it to it. */
    fd = nvgpu_fence_from_handle_noclose(kc->kf->dev, handle, O_CLOEXEC);
    if (fd < 0)
      return fd;
    /* The core writes it before installing (drm_atomic_uapi.c:1404); ours is
     * installed already, and a pointer that faults now leaves it so. */
    if (put_user(fd, (s32 __user *)u64_to_user_ptr(kc->fence_uptr[i])))
      dev_warn_ratelimited(&kc->kf->dev->vdev->dev,
                           "virtio-gpu-nv: out-fence fd %d could not be "
                           "written back to the commit's pointer\n",
                           fd);
    *user_value = fd;
    return 0;
  }

  if (_IOC_NR(call->cmd) == NVGPU_KNR(DRM_IOCTL_MODE_CREATE_LEASE) &&
      buf == 0 && off == offsetof(struct drm_mode_create_lease, fd)) {
    u8 *b0 = nvgpu_i2_buf(call, 0, &len);
    u32 lflags;

    if (kind != NVGPU_HK_DRM_LEASE || !b0 ||
        len < sizeof(struct drm_mode_create_lease))
      return -EINVAL;
    lflags = get_unaligned_le32(b0 +
                                offsetof(struct drm_mode_create_lease, flags));
    fd = nvgpu_adopt_drm_file(kc->filp, handle, kind,
                              (lflags & O_CLOEXEC) |
                                  (kc->filp->f_flags & O_NONBLOCK));
    if (fd < 0) {
      /* The handle is gone with the failed adoption (closed, which revokes
       * the lease on the host); not the interpreter's to close again. */
      call->ret = fd;
      *user_value = -1;
      return 0;
    }
    *user_value = fd;
    return 0;
  }
  return -EINVAL;
}

static int nvgpu_kms_special(struct nvgpu_i2_call *call, u32 special_id,
                             int phase) {
  if (special_id == NVGPU_SSPECIAL_ATOMIC && phase == 0)
    return nvgpu_kms_atomic(to_kms_call(call));
  return 0;
}

/* WAIT_VBLANK, before: an EVENT form reserves its event; a query (RELATIVE,
 * 0, drm_vblank.c:1690) goes as it is, never blocking; anything else would
 * block a host thread for up to 3 s, so it goes as the EVENT form under a
 * cookie of ours and the caller waits here. */
static int nvgpu_kms_vblank_start(struct nvgpu_kms_call *kc, u8 *b0) {
  struct nvgpu_kms_file *kf = kc->kf;
  u32 type = get_unaligned_le32(b0);
  u32 seq = get_unaligned_le32(b0 + 4);
  u64 signal = get_unaligned_le64(b0 + 8);
  unsigned long flags;

  if (type & _DRM_VBLANK_EVENT)
    return nvgpu_kms_reserve(kc, DRM_EVENT_VBLANK, 0, signal);
  if (!seq && (type & (_DRM_VBLANK_TYPES_MASK | _DRM_VBLANK_EVENT |
                       _DRM_VBLANK_NEXTONMISS)) == _DRM_VBLANK_RELATIVE)
    return 0;

  kc->vbl_signal = signal;
  kc->wait.cookie =
      (NVGPU_KMS_COOKIE_TAG << NVGPU_KMS_COOKIE_SHIFT) |
      ((u32)atomic_inc_return(&kf->next_cookie) &
       ((1u << NVGPU_KMS_COOKIE_SHIFT) - 1));
  kc->waiting = true;
  spin_lock_irqsave(&kf->drm->event_lock, flags);
  list_add_tail(&kc->wait.node, &kf->waiters);
  spin_unlock_irqrestore(&kf->drm->event_lock, flags);

  put_unaligned_le32(type | _DRM_VBLANK_EVENT, b0);
  put_unaligned_le64(kc->wait.cookie, b0 + 8);
  return 0;
}

/* The reply's tval_sec/tval_usec (longs, the seconds truncated to u32 as
 * drm_vblank.c:1731 does) into the guest's clock. */
static void nvgpu_kms_reply_tv(struct nvgpu_device *dev, u8 *b0) {
  u32 sec = (u32)get_unaligned_le64(b0 + 8);
  u32 usec = (u32)get_unaligned_le64(b0 + 16);

  nvgpu_kms_tv(dev, &sec, &usec);
  put_unaligned_le64(sec, b0 + 8);
  put_unaligned_le64(usec, b0 + 16);
}

/*
 * WAIT_VBLANK, after. The emulated wait sleeps for its event as the core
 * sleeps for the vblank (interruptibly, 3 s, drm_vblank.c:1846-1860: -EBUSY on
 * timeout, -EINTR -- not a restart -- on a signal) and answers as the core
 * does from the event's count and time. On those failures the caller's
 * `signal` goes back where the host saw our cookie; a timed-out wait's reply
 * is otherwise the host's queueing answer, not a fresh count.
 */
static int nvgpu_kms_vblank_done(struct nvgpu_kms_call *kc, u8 *b0) {
  struct nvgpu_i2_call *call = &kc->call;
  long left;

  if (!kc->waiting) {
    if (!call->ret && !(get_unaligned_le32(b0) & _DRM_VBLANK_EVENT))
      nvgpu_kms_reply_tv(kc->kf->dev, b0);
    return 0;
  }
  put_unaligned_le32(get_unaligned_le32(b0) & ~_DRM_VBLANK_EVENT, b0);
  if (call->ret) {
    put_unaligned_le64(kc->vbl_signal, b0 + 8);
    return 0;
  }
  left = wait_for_completion_interruptible_timeout(
      &kc->wait.done, msecs_to_jiffies(NVGPU_KMS_VBLANK_WAIT_MS));
  if (left > 0) {
    /* Translated when it arrived. */
    put_unaligned_le32(kc->wait.ev.sequence, b0 + 4);
    put_unaligned_le64(kc->wait.ev.tv_sec, b0 + 8);
    put_unaligned_le64(kc->wait.ev.tv_usec, b0 + 16);
    return 0;
  }
  put_unaligned_le64(kc->vbl_signal, b0 + 8);
  call->ret = left == 0 ? -EBUSY : -EINTR;
  return 0;
}

/* GETRESOURCES, before: which buffer the CRTC ids come back in, and how many
 * it holds (buffers exist for non-NULL lists of non-zero count, in field
 * order). */
static void nvgpu_kms_resources_start(struct nvgpu_kms_call *kc, u8 *b0) {
  static const u32 ptr[4] = {
      offsetof(struct drm_mode_card_res, fb_id_ptr),
      offsetof(struct drm_mode_card_res, crtc_id_ptr),
      offsetof(struct drm_mode_card_res, connector_id_ptr),
      offsetof(struct drm_mode_card_res, encoder_id_ptr)};
  static const u32 cnt[4] = {
      offsetof(struct drm_mode_card_res, count_fbs),
      offsetof(struct drm_mode_card_res, count_crtcs),
      offsetof(struct drm_mode_card_res, count_connectors),
      offsetof(struct drm_mode_card_res, count_encoders)};
  u32 i, idx = 1;

  for (i = 0; i < 4; i++) {
    u32 n = get_unaligned_le32(b0 + cnt[i]);

    if (!get_unaligned_le64(b0 + ptr[i]) || !n)
      continue;
    if (i == 1) {
      kc->res_buf = idx;
      kc->res_sent = n;
    }
    idx++;
  }
}

/* GETRESOURCES, after: remember the CRTCs, filled "as many as fit". */
static void nvgpu_kms_resources_done(struct nvgpu_kms_call *kc, u8 *b0) {
  u32 len, n, i;
  u8 *ids;

  if (!kc->res_buf)
    return;
  ids = nvgpu_i2_buf(&kc->call, kc->res_buf, &len);
  n = min(kc->res_sent,
          get_unaligned_le32(b0 + offsetof(struct drm_mode_card_res,
                                           count_crtcs)));
  for (i = 0; ids && i < n && 4 * i + 4 <= len; i++)
    xa_store(&kc->kf->objs, get_unaligned_le32(ids + 4 * i),
             xa_mk_value(NVGPU_KOBJ_CRTC), GFP_KERNEL);
}

static int nvgpu_kms_phase(struct nvgpu_i2_call *call, int phase) {
  struct nvgpu_kms_call *kc = to_kms_call(call);
  struct nvgpu_kms_file *kf = kc->kf;
  unsigned int nr = _IOC_NR(call->cmd);
  u32 len;
  u8 *b0 = nvgpu_i2_buf(call, 0, &len);
  bool ok;

  if (phase == 1) {
    kc->replied = true;
    kc->host_ret = call->ret;
  }
  if (!b0 || len != _IOC_SIZE(call->cmd))
    return 0;
  ok = phase == 1 && !call->ret;

  switch (nr) {
  case NVGPU_KNR(DRM_IOCTL_WAIT_VBLANK):
    return phase ? nvgpu_kms_vblank_done(kc, b0)
                 : nvgpu_kms_vblank_start(kc, b0);
  case NVGPU_KNR(DRM_IOCTL_CRTC_GET_SEQUENCE):
    if (ok) {
      size_t at = offsetof(struct drm_crtc_get_sequence, sequence_ns);
      s64 ns = (s64)get_unaligned_le64(b0 + at);

      if (ns)
        put_unaligned_le64(nvgpu_host_to_guest_ns(kf->dev, ns), b0 + at);
    }
    return 0;
  case NVGPU_KNR(DRM_IOCTL_CRTC_QUEUE_SEQUENCE):
    if (!phase)
      return nvgpu_kms_reserve(
          kc, DRM_EVENT_CRTC_SEQUENCE, 0,
          get_unaligned_le64(
              b0 + offsetof(struct drm_crtc_queue_sequence, user_data)));
    return 0;
  case NVGPU_KNR(DRM_IOCTL_MODE_PAGE_FLIP):
    if (!phase &&
        (get_unaligned_le32(b0 + offsetof(struct drm_mode_crtc_page_flip,
                                          flags)) &
         DRM_MODE_PAGE_FLIP_EVENT))
      return nvgpu_kms_reserve(
          kc, DRM_EVENT_FLIP_COMPLETE,
          get_unaligned_le32(b0 +
                             offsetof(struct drm_mode_crtc_page_flip, crtc_id)),
          get_unaligned_le64(
              b0 + offsetof(struct drm_mode_crtc_page_flip, user_data)));
    return 0;
  case NVGPU_KNR(DRM_IOCTL_SET_CLIENT_CAP):
    if (ok) {
      kc->cap = get_unaligned_le64(b0);
      kc->cap_value = get_unaligned_le64(b0 + 8);
      kc->cap_set = true;
    }
    return 0;
  case NVGPU_KNR(DRM_IOCTL_MODE_GETPROPERTY):
    if (ok)
      xa_store(&kf->props,
               get_unaligned_le32(b0 + offsetof(struct drm_mode_get_property,
                                                prop_id)),
               xa_mk_value(nvgpu_kms_prop_kind_of(
                   (const char *)b0 +
                   offsetof(struct drm_mode_get_property, name))),
               GFP_KERNEL);
    return 0;
  case NVGPU_KNR(DRM_IOCTL_MODE_GETCRTC):
    if (ok)
      xa_store(&kf->objs,
               get_unaligned_le32(b0 + offsetof(struct drm_mode_crtc, crtc_id)),
               xa_mk_value(NVGPU_KOBJ_CRTC), GFP_KERNEL);
    return 0;
  case NVGPU_KNR(DRM_IOCTL_MODE_GETPLANE):
    if (ok)
      xa_store(&kf->objs,
               get_unaligned_le32(b0 +
                                  offsetof(struct drm_mode_get_plane, plane_id)),
               xa_mk_value(NVGPU_KOBJ_OTHER |
                           ((unsigned long)get_unaligned_le32(
                                b0 + offsetof(struct drm_mode_get_plane,
                                              crtc_id))
                            << 2)),
               GFP_KERNEL);
    return 0;
  case NVGPU_KNR(DRM_IOCTL_MODE_GETRESOURCES):
    if (!phase)
      nvgpu_kms_resources_start(kc, b0);
    else if (ok)
      nvgpu_kms_resources_done(kc, b0);
    return 0;
  }
  return 0;
}

static const struct nvgpu_i2_ops nvgpu_kms_ops = {
    .fd_in = nvgpu_kms_fd_in,
    .gem_in = nvgpu_kms_gem_in,
    .fd_out = nvgpu_kms_fd_out,
    .gem_out = nvgpu_kms_gem_out,
    .special = nvgpu_kms_special,
    .phase = nvgpu_kms_phase,
};

/* ───────── the ioctls ───────── */

static long nvgpu_kms_forward(struct nvgpu_kms_file *kf, struct file *filp,
                              struct drm_file *file, unsigned int cmd,
                              void __user *uarg) {
  struct nvgpu_kms_call *kc;
  long ret;
  u32 h;

  ret = nvgpu_kms_get_handle(kf, file, &h);
  if (ret)
    return ret;
  kc = kzalloc(sizeof(*kc), GFP_KERNEL);
  if (!kc)
    return -ENOMEM;
  kc->kf = kf;
  kc->file = file;
  kc->filp = filp;
  INIT_LIST_HEAD(&kc->wait.node);
  init_completion(&kc->wait.done);
  kc->call.dev = kf->dev;
  kc->call.handle = h;
  kc->call.render = kf->nfd->handle;
  kc->call.sclass = NVGPU_SCLASS_KMS;
  kc->call.cmd = cmd;
  kc->call.uarg = uarg;
  kc->call.ops = &nvgpu_kms_ops;

  ret = nvgpu_i2_ioctl(&kc->call);
  nvgpu_kms_settle(kc);
  kfree(kc);
  return ret;
}

/* MAP_DUMB: the proxy's fake offset in this node; the mapping then reaches
 * the host's memory through the shared window like any proxy's. The offset is
 * zeroed on failure, as the core zeroes it (drm_dumb_buffers.c:283). */
static long nvgpu_kms_map_dumb(struct drm_file *file, unsigned int cmd,
                               void __user *uarg) {
  struct drm_mode_map_dumb md;
  int ret;

  if (_IOC_SIZE(cmd) != sizeof(md))
    return -EINVAL;
  if (copy_from_user(&md, uarg, sizeof(md)))
    return -EFAULT;
  ret = nvgpu_gem_mmap_offset(file, md.handle, &md.offset);
  if (ret)
    md.offset = 0;
  if (copy_to_user(uarg, &md, sizeof(md)))
    return -EFAULT;
  return ret;
}

/* DESTROY_DUMB is GEM_CLOSE by another name (drm_dumb_buffers.c:287). */
static long nvgpu_kms_destroy_dumb(struct drm_file *file, unsigned int cmd,
                                   void __user *uarg) {
  struct drm_mode_destroy_dumb dd;

  if (_IOC_SIZE(cmd) != sizeof(dd))
    return -EINVAL;
  if (copy_from_user(&dd, uarg, sizeof(dd)))
    return -EFAULT;
  return drm_gem_handle_delete(file, dd.handle);
}

/* The caps the core answers without DRIVER_MODESET (drm_ioctl.c:242-255),
 * which are about this node -- PRIME is ours, syncobjs ours or none -- not
 * the host's. */
static bool nvgpu_kms_core_cap(unsigned int cmd, void __user *uarg) {
  u64 cap;

  if (_IOC_SIZE(cmd) != sizeof(struct drm_get_cap) ||
      copy_from_user(&cap, uarg, sizeof(cap)))
    return false;
  return cap == DRM_CAP_PRIME || cap == DRM_CAP_TIMESTAMP_MONOTONIC ||
         cap == DRM_CAP_SYNCOBJ || cap == DRM_CAP_SYNCOBJ_TIMELINE;
}

bool nvgpu_kms_ioctl(struct file *filp, unsigned int cmd, unsigned long arg,
                     long *ret) {
  struct drm_file *file = filp->private_data;
  struct nvgpu_fd *nfd = file ? file->driver_priv : NULL;
  struct nvgpu_kms_file *kf = nfd ? nfd->kms : NULL;
  void __user *uarg = (void __user *)arg;

  if (!kf || _IOC_TYPE(cmd) != DRM_IOCTL_BASE)
    return false;

  switch (_IOC_NR(cmd)) {
  /* The guest core's: its own node, its own auth domain, its own master
   * arbitration (whose decisions the master hooks carry to the host). */
  case NVGPU_KNR(DRM_IOCTL_VERSION):
  case NVGPU_KNR(DRM_IOCTL_GET_UNIQUE):
  case NVGPU_KNR(DRM_IOCTL_GET_MAGIC):
  case NVGPU_KNR(DRM_IOCTL_AUTH_MAGIC):
  case NVGPU_KNR(DRM_IOCTL_SET_MASTER):
  case NVGPU_KNR(DRM_IOCTL_DROP_MASTER):
  case NVGPU_KNR(DRM_IOCTL_SET_CLIENT_NAME):
    return false;
  case NVGPU_KNR(DRM_IOCTL_MODE_MAP_DUMB):
    *ret = nvgpu_kms_map_dumb(file, cmd, uarg);
    return true;
  case NVGPU_KNR(DRM_IOCTL_MODE_DESTROY_DUMB):
    *ret = nvgpu_kms_destroy_dumb(file, cmd, uarg);
    return true;
  case NVGPU_KNR(DRM_IOCTL_GET_CAP):
    if (nvgpu_kms_core_cap(cmd, uarg))
      return false;
    break;
  }

  if (!nvgpu_i2_has_schema(kf->dev, NVGPU_SCLASS_KMS, cmd, NULL, 0))
    return false;
  *ret = nvgpu_kms_forward(kf, filp, file, cmd, uarg);
  return true;
}

/* ───────── release ───────── */

void nvgpu_kms_detach(struct nvgpu_fd *nfd) {
  struct nvgpu_kms_file *kf = nfd->kms;
  struct nvgpu_kms_pev *p, *n;
  unsigned long flags;
  LIST_HEAD(cancel);
  int i;

  if (!kf)
    return;
  nfd->kms = NULL;

  /* After this no deliver() runs for the file (nvgpu_ev_unregister()). */
  for (i = 0; i < ARRAY_SIZE(kf->evc); i++)
    if (kf->evc[i].registered)
      nvgpu_ev_unregister(kf->dev, &kf->evc[i].c);

  /* Reservations whose events will not come now: the space goes back while
   * the drm_file still stands (from .release), or they are simply freed if
   * the core already let go of them (drm_events_release(), postclose path). */
  spin_lock_irqsave(&kf->drm->event_lock, flags);
  list_splice_init(&kf->pending, &cancel);
  spin_unlock_irqrestore(&kf->drm->event_lock, flags);
  list_for_each_entry_safe(p, n, &cancel, node) {
    list_del_init(&p->node);
    drm_event_cancel_free(kf->drm, &p->base);
  }

  /* The current handle is the caller's to close (nvgpu_fd.kms_handle). */
  if (kf->retired)
    nvgpu_close_handle(kf->dev, kf->retired);
  nvgpu_kms_free(kf);
}
