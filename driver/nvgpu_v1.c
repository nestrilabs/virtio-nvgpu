// SPDX-License-Identifier: GPL-2.0-only
/*
 * The protocol-v1 IOCTL exchange, in both builds: the request header, the
 * reading of a reply's header, which every v1 path -- the C parsers
 * (nvgpu_rmio.c), the nvidia-drm GEM forwarder (nvgpu_drm.c) -- does the same
 * way, and the flat round trip of a block with no pointer in it
 * (nvgpu_ioctl_flat()), which the C parsers' flat escapes and the flat
 * nvidia-drm calls (nvgpu_drm.c, nvgpu_gem.c) both make. The Rust parsers' twins are
 * wire::ioctl_req_header(), wire::IoctlResp::parse() and rm.rs's flat path
 * (driver/rust/core), and the difftest compiles this file with the C
 * parsers, so both read a reply alike.
 *
 * The status is sanitised here, once for every v1 path: a reply's status is
 * the backend's word on what the call returned, 0 or a negative errno.
 * Anything else (nvgpu_status_valid()) would reach the caller as an ioctl
 * result no native driver returns (a positive one is not even an error to the
 * C library), so it fails the call with -EPROTO, its payload unread, as IOCTL2
 * treats one -- a reply this side does not understand, like one too short to
 * hold a header (-EIO).
 */

#include <linux/err.h>
#include <linux/kernel.h>
#include <linux/slab.h>

#include "nvgpu.h"

void nvgpu_ioctl_req_init(struct nvgpu_ioctl_req *req, u32 handle, u32 cmd,
                          u32 data_len, u32 nested_off, u32 nested_len,
                          u32 deep_off, u32 deep_len) {
  req->hdr.msg_type = cpu_to_le32(NVGPU_MSG_IOCTL);
  req->hdr.handle = cpu_to_le32(handle);
  req->hdr.status = 0;
  req->hdr.req_id = 0;
  req->cmd = cpu_to_le32(cmd);
  req->data_len = cpu_to_le32(data_len);
  req->nested_offset = cpu_to_le32(nested_off);
  req->nested_len = cpu_to_le32(nested_len);
  req->deep_ptr_offset = cpu_to_le32(deep_off);
  req->deep_len = cpu_to_le32(deep_len);
}

int nvgpu_ioctl_reply_parse(const void *resp, u32 used,
                            struct nvgpu_ioctl_reply *r) {
  const struct nvgpu_ioctl_resp *h = resp;
  s32 status;

  memset(r, 0, sizeof(*r));
  if (!nvgpu_resp_has(used, 0, sizeof(h->hdr)))
    return -EIO;
  status = (s32)le32_to_cpu(h->hdr.status);
  r->used = used;
  r->raw = status;
  if (nvgpu_resp_has(used, 0, sizeof(*h))) {
    r->full = true;
    r->data_len = le32_to_cpu(h->data_len);
    r->nested_len = le32_to_cpu(h->nested_len);
    r->deep_len = le32_to_cpu(h->deep_len);
  }
  if (!nvgpu_status_valid(status)) {
    r->status = -EPROTO;
    return -EPROTO;
  }
  r->status = status;
  return 0;
}

int nvgpu_ioctl_exchange(struct nvgpu_device *dev, void *req, size_t req_len,
                         void *resp, size_t resp_max,
                         struct nvgpu_ioctl_reply *r) {
  u32 used;
  int ret;

  ret = nvgpu_send_recv_used(dev, req, (int)req_len, resp, (int)resp_max,
                             &used);
  if (ret < 0)
    return ret;
  return nvgpu_ioctl_reply_parse(resp, used, r);
}

long nvgpu_ioctl_flat(struct nvgpu_device *dev, u32 handle, unsigned int cmd,
                      void *buf, u32 sz, u32 flags, u32 *back) {
  bool proc = (flags & NVGPU_FLAT_PROC) && nvgpu_proc_ids(dev);
  size_t req_total = sizeof(struct nvgpu_ioctl_req) + sz +
                     (proc ? sizeof(struct nvgpu_proc_id) : 0);
  size_t resp_max = sizeof(struct nvgpu_ioctl_resp) + sz;
  struct nvgpu_ioctl_reply r;
  u8 *req_buf, *resp_buf;
  long ret;

  if (back)
    *back = 0;
  req_buf = kvmalloc(req_total, GFP_KERNEL);
  resp_buf = kvmalloc(resp_max, GFP_KERNEL);
  if (!req_buf || !resp_buf) {
    ret = -ENOMEM;
    goto out;
  }

  nvgpu_ioctl_req_init((struct nvgpu_ioctl_req *)req_buf, handle, cmd, sz, 0,
                       0, 0, 0);
  if (sz)
    memcpy(req_buf + sizeof(struct nvgpu_ioctl_req), buf, sz);
  if (proc)
    nvgpu_proc_id_fill(dev, req_buf + sizeof(struct nvgpu_ioctl_req) + sz);

  ret = nvgpu_ioctl_exchange(dev, req_buf, req_total, resp_buf, resp_max, &r);
  if (ret < 0)
    goto out;
  ret = r.status;

  if (flags & NVGPU_FLAT_WHOLE) {
    if (r.full && r.data_len >= sz &&
        nvgpu_resp_has(r.used, sizeof(struct nvgpu_ioctl_resp), sz)) {
      memcpy(buf, resp_buf + sizeof(struct nvgpu_ioctl_resp), sz);
      if (back)
        *back = sz;
    } else if (ret >= 0) {
      /*
       * A success that did not carry the struct back: `buf` still holds what
       * was sent, which a caller reading an answer out of it (ALLOC_NVKMS's
       * handle, MAP_OFFSET's offset) would take for the host's -- a proxy
       * for a handle number its caller chose.
       */
      ret = -EIO;
    }
  } else if (sz && r.data_len && r.data_len <= sz &&
             nvgpu_resp_has(r.used, sizeof(struct nvgpu_ioctl_resp),
                            r.data_len)) {
    /* Only what the device wrote: a failed call comes back as a bare
     * header, and nothing past it is anybody's to read. */
    memcpy(buf, resp_buf + sizeof(struct nvgpu_ioctl_resp), r.data_len);
    if (back)
      *back = r.data_len;
  }

out:
  kvfree(req_buf);
  kvfree(resp_buf);
  return ret;
}
