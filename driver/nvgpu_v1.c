// SPDX-License-Identifier: GPL-2.0-only
/*
 * The protocol-v1 IOCTL exchange, in both builds: the request header, and the
 * reading of a reply's header, which every v1 path -- the C parsers
 * (nvgpu_rmio.c), the flat forwarder (nvgpu_main.c), the nvidia-drm GEM
 * forwarder (nvgpu_drm.c) -- does the same way. The Rust parsers' twins are
 * wire::ioctl_req_header() and wire::IoctlResp::parse() (driver/rust/core),
 * and the difftest compiles this file with the C parsers, so both read a
 * reply alike.
 *
 * The status is sanitised here, once: a reply's status is the backend's word
 * on what the call returned, 0 or a negative errno. Anything else -- positive,
 * or below -MAX_ERRNO -- would reach the caller as an ioctl result no native
 * driver returns (a positive one is not even an error to the C library), so it
 * fails the call with -EPROTO, its payload unread, as IOCTL2 treats one
 * (nvgpu_i2.c) -- a reply this side does not understand, like one too short
 * to hold a header (-EIO). Before this each copy of the exchange returned the
 * raw s32, and some copied the payload back beside it.
 */

#include <linux/err.h>
#include <linux/kernel.h>

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
  if (status > 0 || status < -MAX_ERRNO) {
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
