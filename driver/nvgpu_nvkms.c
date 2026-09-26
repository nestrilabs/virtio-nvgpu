// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: /dev/nvidia-modeset on protocol v2 -- NVKMS through the
 * schema-driven IOCTL2 interpreter, and the readiness NVKMS clients poll.
 *
 * NVKMS is one ioctl, _IOWR('m', 0, struct NvKmsIoctlParams {u32 cmd; u32
 * size; u64 address}), multiplexing some sixty commands whose params blocks
 * hold user pointers (FLIP three levels deep: params -> pFlipHead[] -> LUT
 * ramps), descriptors (surfaces, grant and unicast-event files) and nothing
 * the host could take as it stands (R:nvkms §5). The per-release tables in
 * gen/nvgpu_schema.h say which is which for the host's driver version; the
 * interpreter (nvgpu_i2.c) gathers what they name and the backend, walking
 * its own copy, refuses anything else. What is left here is what only the
 * guest can do: turn a caller's descriptor into the backend handle behind
 * it, of the kind the field takes, and keep a modeset file's readiness the
 * way NVKMS keeps it.
 *
 * Descriptors. A modeset field (grant files, unicast-event files) takes one
 * of our /dev/nvidia-modeset files; a surface plane takes one of our
 * /dev/nvidiactl files an RM object was exported to (the backend's host file
 * is where RM did the export) or one of our GEM proxies' dma-bufs, which the
 * backend exports from the proxy's owner for this call only (HOST_OP
 * PRIME_EXPORT, consumed by the call). Anything else is -EBADF here -- a
 * number forwarded as it stands would be looked up in the backend's own
 * table (R:nvkms §9B).
 *
 * Readiness. nvkms_poll reports POLLIN | POLLPRI while the file's event
 * queue is non-empty or a unicast event is pending, and keeps reporting it
 * until GET_NEXT_EVENT drains the queue or CLEAR_UNICAST_EVENT clears the
 * event (nvidia-modeset-linux.c:2012-2032, nvkms.c:3291-3359). The host says
 * "readable" through the legacy watch on the handle (nfd->pending, set from
 * the event queue; the backend re-reports every millisecond while the host
 * file stays readable). So pending is not taken by poll here, as it is for
 * the RM devices, but by those two commands: claimed (1 -> 2) before the
 * call, dropped (2 -> 0) only if the host said it is now empty and no new
 * report arrived meanwhile (which would have set it back to 1).
 *
 * Errors. NVKMS answers every failure of a command with -EPERM
 * (nvidia-modeset-linux.c:1539), and its callers expect nothing else, so a
 * command this side or the backend refused -- no table entry, a wrong size,
 * a descriptor that is not ours, a policy refusal -- is -EPERM too. The reply
 * half is copied back whatever the host answered (the interpreter's RANGE
 * copy-back; nvkms.c:5220-5243 does the same).
 *
 * Nothing here ever issues an ioctl of its own on a modeset file: the first
 * one makes it an "ioctl" file forever, useless as a grant or unicast file
 * (nvkms.c:1291-1342). v1 guests keep nvgpu_ioctl_modeset() in nvgpu_main.c.
 */

#include <linux/dma-buf.h>
#include <linux/file.h>
#include <linux/poll.h>
#include <linux/string.h>
#include <linux/uaccess.h>
#include <linux/unaligned.h>

#include "nvgpu.h"
#include "gen/nvgpu_schema.h"

/* pending: nothing known / the host said readable / claimed by a consumer. */
#define NVGPU_NVKMS_IDLE 0
#define NVGPU_NVKMS_READY 1
#define NVGPU_NVKMS_CLAIMED 2

struct nvgpu_nvkms_call {
  struct nvgpu_fd *nfd;
  const char *name;       /* the table entry's, "NVKMS_..."; NULL until known */
  u32 cmd;                /* the NVKMS command, once known */
  bool next_event;        /* GET_NEXT_EVENT */
  struct nvgpu_fd *event; /* CLEAR_UNICAST_EVENT: the file it clears */
  bool event_claimed, self_claimed;
  u8 valid;               /* GET_NEXT_EVENT's reply.valid, as the host left it */
};

/* The table entry for NVKMS command @cmd on this host, or NULL. */
static const struct nvgpu_sioctl *nvgpu_nvkms_entry(struct nvgpu_device *dev,
                                                    u32 cmd) {
  const struct nvgpu_stable *t = dev->schema ? dev->schema->modeset : NULL;
  u32 i;

  for (i = 0; t && i < t->nioctls; i++)
    if (t->ioctls[i].nvkms_cmd == cmd)
      return &t->ioctls[i];
  return NULL;
}

static bool nvgpu_nvkms_claim(struct nvgpu_fd *nfd) {
  return atomic_cmpxchg(&nfd->pending, NVGPU_NVKMS_READY,
                        NVGPU_NVKMS_CLAIMED) == NVGPU_NVKMS_READY;
}

/* A claimed file: empty now (@drained) unless a report came in meanwhile. */
static void nvgpu_nvkms_settle(struct nvgpu_fd *nfd, bool drained) {
  atomic_cmpxchg(&nfd->pending, NVGPU_NVKMS_CLAIMED,
                 drained ? NVGPU_NVKMS_IDLE : NVGPU_NVKMS_READY);
}

/*
 * Which command the call is, from the interpreter's own copy of the outer
 * struct (buffer 0: NvKmsIoctlParams, whose first word is the command) --
 * the bytes it sends, read once. From the first hook that runs.
 */
static void nvgpu_nvkms_identify(struct nvgpu_i2_call *call) {
  struct nvgpu_nvkms_call *nc = call->priv;
  const struct nvgpu_sioctl *e;
  u32 len;
  u8 *b0;

  if (nc->name)
    return;
  b0 = nvgpu_i2_buf(call, 0, &len);
  if (!b0 || len < sizeof(u32))
    return;
  nc->cmd = get_unaligned_le32(b0);
  e = nvgpu_nvkms_entry(call->dev, nc->cmd);
  if (!e)
    return;
  nc->name = e->name;
  nc->next_event = !strcmp(e->name, "NVKMS_GET_NEXT_EVENT");
}

/* One of our /dev/nvidia* character-device files of @type on this device. */
static struct nvgpu_fd *nvgpu_nvkms_chardev(struct nvgpu_device *dev,
                                            struct file *f, u32 type) {
  struct nvgpu_fd *nfd = nvgpu_fd_from_file(f);

  if (!nfd || nfd->dev != dev || nfd->device_type != type)
    return NULL;
  return nfd;
}

/*
 * A dma-buf of one of our GEM proxies, exported by the backend from the
 * proxy's owner file for this one call: the host's NVKMS imports it with
 * fget in the backend (nvkms-surface.c:526-592), so the backend must hold a
 * descriptor of its own, and it closes it once the call ran (FD_CONSUME).
 */
static int nvgpu_nvkms_dmabuf(struct nvgpu_device *dev, int fd, u32 *handle,
                              u32 *flags) {
  struct dma_buf *buf = dma_buf_get(fd);
  u64 args[2], res[2];
  u32 owner, gem;
  int ret;

  if (IS_ERR(buf))
    return -EBADF;
  ret = nvgpu_dmabuf_to_host(dev, buf, &owner, &gem);
  if (ret < 0) {
    dma_buf_put(buf);
    return -EBADF;
  }
  args[0] = owner;
  args[1] = gem;
  ret = nvgpu_host_op(dev, NVGPU_OP_PRIME_EXPORT, args, 2, res, 2);
  /*
   * Only now: the dma-buf holds the proxy, whose (owner, gem) the export
   * names, and a last close before it ran would have let the host give the
   * number to an object the caller was never given (S-25). The host's own
   * dma-buf pins the object from here.
   */
  dma_buf_put(buf);
  if (ret < 0)
    return ret;
  *handle = res[0];
  *flags = NVGPU_I2_FD_CONSUME;
  return 0;
}

static int nvgpu_nvkms_fd_in(struct nvgpu_i2_call *call, u32 buf, u32 off,
                             s64 user_value, u32 kinds, u32 *handle,
                             u32 *flags) {
  struct nvgpu_nvkms_call *nc = call->priv;
  struct nvgpu_device *dev = call->dev;
  struct nvgpu_fd *nfd = NULL;
  struct file *f;

  nvgpu_nvkms_identify(call);
  if (user_value < 0 || user_value > INT_MAX)
    goto bad;
  f = fget(user_value);
  if (!f)
    goto bad;
  if (kinds & NVGPU_SKIND_DEV_MODESET)
    nfd = nvgpu_nvkms_chardev(dev, f, NVGPU_DEV_MODESET);
  if (!nfd && (kinds & NVGPU_SKIND_DEV_CTL))
    nfd = nvgpu_nvkms_chardev(dev, f, NVGPU_DEV_CTL);
  if (nfd) {
    *handle = nfd->handle;
    /*
     * CLEAR_UNICAST_EVENT clears the file it names, not the one it runs on:
     * claim that file's readiness now, before the host clears it, so a new
     * event after the clear is not lost with the old one.
     */
    if (nfd->device_type == NVGPU_DEV_MODESET && !nc->event && nc->name &&
        !strcmp(nc->name, "NVKMS_CLEAR_UNICAST_EVENT")) {
      nvgpu_fd_get(nfd);
      nc->event = nfd;
      nc->event_claimed = nvgpu_nvkms_claim(nfd);
    }
    fput(f);
    return 0;
  }
  fput(f);
  if (kinds & NVGPU_SKIND(NVGPU_HK_DMABUF)) {
    int ret = nvgpu_nvkms_dmabuf(dev, user_value, handle, flags);

    if (ret != -EBADF)
      return ret;
  }
bad:
  dev_dbg_ratelimited(&dev->vdev->dev,
                      "virtio-gpu-nv: %s: descriptor %lld at %u+%u is not a "
                      "file of ours of the kind NVKMS takes there (kinds "
                      "%#x)\n",
                      nc->name ? nc->name : "NVKMS", user_value, buf, off,
                      kinds);
  return -EBADF;
}

/*
 * Phase 0, the request about to go: GET_NEXT_EVENT claims the file's
 * readiness now, so a report arriving while it runs is kept. Phase 1, the
 * reply parsed: what GET_NEXT_EVENT said about the queue, from the copy that
 * goes back to the caller (NvKmsGetNextEventParams.reply.valid).
 */
static int nvgpu_nvkms_phase(struct nvgpu_i2_call *call, int phase) {
  struct nvgpu_nvkms_call *nc = call->priv;
  u32 len;
  u8 *params;

  nvgpu_nvkms_identify(call);
  if (!nc->next_event)
    return 0;
  if (phase == 0) {
    nc->self_claimed = nvgpu_nvkms_claim(nc->nfd);
    return 0;
  }
  params = nvgpu_i2_buf(call, 1, &len);
  if (params && len > NVGPU_NVKMS_NEXT_EVENT_VALID_OFF)
    nc->valid = params[NVGPU_NVKMS_NEXT_EVENT_VALID_OFF];
  return 0;
}

static const struct nvgpu_i2_ops nvgpu_nvkms_ops = {
    .fd_in = nvgpu_nvkms_fd_in,
    .phase = nvgpu_nvkms_phase,
};

/*
 * NVKMS answers every failed command with -EPERM (nvidia-modeset-linux.c:
 * 1539); our refusals of a command are failures of it. What is not about the
 * command -- no memory, a signal, the device gone -- keeps its own errno.
 */
static long nvgpu_nvkms_errno(struct nvgpu_device *dev, const char *name,
                              u32 cmd, long ret) {
  switch (ret) {
  case -ENOTTY:
  case -EINVAL:
  case -E2BIG:
  case -EBADF:
  case -EOPNOTSUPP:
  case -EMSGSIZE:
    dev_dbg_ratelimited(&dev->vdev->dev,
                        "virtio-gpu-nv: NVKMS command %u (%s) refused (%ld): "
                        "%s\n",
                        cmd, name ? name : "none", ret,
                        ret == -ENOTTY  ? "no table entry for this host"
                        : ret == -EBADF ? "a descriptor that is not ours"
                        : ret == -E2BIG ? "larger than the transport carries"
                                        : "not the host's layout, or the "
                                          "backend's policy");
    return -EPERM;
  }
  return ret;
}

long nvgpu_nvkms_ioctl(struct nvgpu_fd *nfd, unsigned int cmd,
                       void __user *uarg) {
  struct nvgpu_device *dev = nfd->dev;
  struct nvgpu_nvkms_call nc = {.nfd = nfd};
  struct nvgpu_i2_call call = {
      .dev = dev,
      .handle = nfd->handle,
      .render = 0, /* NVKMS makes no GEM handle */
      .sclass = NVGPU_SCLASS_MODESET,
      .cmd = cmd,
      .uarg = uarg,
      .ops = &nvgpu_nvkms_ops,
      .priv = &nc,
  };
  long ret;

  /* nvkms_ioctl's own checks (nvidia-modeset-linux.c:1976-1985). */
  if (cmd != NVGPU_NVKMS_IOCTL_IOWR)
    return -ENOTTY;

  /*
   * Everything this decides -- which command, GET_NEXT_EVENT's claim and
   * what it found -- is decided on the interpreter's one copy of the call,
   * in its hooks: the outer struct used to be read here first, and a thread
   * rewriting it in between had one command's readiness handled for
   * another's.
   */
  nc.valid = 1;
  ret = nvgpu_i2_ioctl(&call);

  if (nc.next_event && nc.self_claimed)
    /* reply.valid: FALSE is "the queue was empty", nvkms.c:3291-3320. */
    nvgpu_nvkms_settle(nfd, !ret && !nc.valid);
  if (nc.event) {
    if (nc.event_claimed)
      nvgpu_nvkms_settle(nc.event, ret == 0);
    nvgpu_fd_put(nc.event);
  }
  return nvgpu_nvkms_errno(dev, nc.name, nc.cmd, ret);
}

__poll_t nvgpu_nvkms_poll(struct nvgpu_fd *nfd, struct file *filp,
                          struct poll_table_struct *wait) {
  poll_wait(filp, &nfd->wq, wait);
  return atomic_read(&nfd->pending) != NVGPU_NVKMS_IDLE
             ? EPOLLIN | EPOLLPRI | EPOLLRDNORM
             : 0;
}
