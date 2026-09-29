// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: /proc/driver/nvidia, the host's own tree as GET_PROC_FILES
 * sends it, served as static files -- where NVIDIA's userspace and NVML look
 * before they will talk to a device at all -- and the reader of the record
 * streams GET_PROC_FILES and GET_SYS_FILES are made of.
 */

#include <linux/kernel.h>
#include <linux/proc_fs.h>
#include <linux/seq_file.h>
#include <linux/slab.h>
#include <linux/string.h>

#include "nvgpu.h"

/* ───────── The host's file streams ───────── */

/* Each length is checked against what is left, never their sum past the end. */
bool nvgpu_file_rec_next(const u8 **p, const u8 *end,
                         struct nvgpu_file_rec *r, bool *truncated) {
  __le32 raw[2];

  *truncated = false;
  if (end - *p < (ptrdiff_t)sizeof(raw))
    return false;
  memcpy(raw, *p, sizeof(raw));
  r->path_len = le32_to_cpu(raw[0]);
  r->content_len = le32_to_cpu(raw[1]);
  *p += sizeof(raw);
  if (!r->path_len && !r->content_len)
    return false;
  if (r->path_len > end - *p || r->content_len > end - *p - r->path_len) {
    *truncated = true;
    return false;
  }
  r->path = *p;
  r->content = *p + r->path_len;
  *p += r->path_len + r->content_len;
  return true;
}

/* ───────── /proc/driver/nvidia ───────── */

/*
 * One file of the host's /proc/driver/nvidia, as GET_PROC_FILES sent it: a
 * static copy, served as a seq file. Kept on the device's list, and freed
 * after remove_proc_subtree() has taken the entries away -- which waits out
 * any reader -- since the proc core frees an entry, never its data.
 */
struct nvgpu_proc_buf {
  struct list_head node; /* nvgpu_device.proc_bufs */
  char *data;
  size_t len;
};

/* Where every entry goes, and the one subtree remove takes down. */
#define NVGPU_PROC_ROOT "driver/nvidia"

static int nvgpu_proc_buf_show(struct seq_file *m, void *v) {
  struct nvgpu_proc_buf *b = m->private;
  seq_write(m, b->data, b->len);
  return 0;
}

static int nvgpu_proc_buf_open(struct inode *inode, struct file *filp) {
  return single_open(filp, nvgpu_proc_buf_show, pde_data(inode));
}

static const struct proc_ops nvgpu_proc_buf_ops = {
    .proc_open = nvgpu_proc_buf_open,
    .proc_read = seq_read,
    .proc_lseek = seq_lseek,
    .proc_release = single_release,
};

/* Simple directory cache — avoids duplicate proc_mkdir calls */
#define NVGPU_PROC_MAX_DIRS 32

struct nvgpu_proc_dir_cache {
  char path[128];
  struct proc_dir_entry *entry;
};

static struct nvgpu_proc_dir_cache nvgpu_dir_cache[NVGPU_PROC_MAX_DIRS];
static int nvgpu_dir_cache_count;

static void nvgpu_dir_cache_reset(void) {
  memset(nvgpu_dir_cache, 0, sizeof(nvgpu_dir_cache));
  nvgpu_dir_cache_count = 0;
}

static struct proc_dir_entry *
nvgpu_proc_mkdir_cached(const char *path, struct proc_dir_entry *parent) {
  int i;

  /* Check cache first */
  for (i = 0; i < nvgpu_dir_cache_count; i++) {
    if (strcmp(nvgpu_dir_cache[i].path, path) == 0)
      return nvgpu_dir_cache[i].entry;
  }

  /* Not cached — create it */
  struct proc_dir_entry *entry = proc_mkdir(path, parent);

  /* Cache it even if NULL — so we don't retry failed creates */
  if (nvgpu_dir_cache_count < NVGPU_PROC_MAX_DIRS) {
    strscpy(nvgpu_dir_cache[nvgpu_dir_cache_count].path, path, 128);
    nvgpu_dir_cache[nvgpu_dir_cache_count].entry = entry;
    nvgpu_dir_cache_count++;
  }

  return entry;
}

static struct proc_dir_entry *nvgpu_proc_mkdir_parents(char *pathbuf,
                                                       char **leaf_name) {
  struct proc_dir_entry *parent = NULL;
  char built[256] = {};
  char *slash;
  char *p;

  slash = strrchr(pathbuf, '/');
  if (!slash) {
    *leaf_name = pathbuf;
    return NULL;
  }

  *leaf_name = slash + 1;
  *slash = '\0';

  /* Walk each component, building the full path as we go
   * so the cache key is always the full absolute component */
  p = pathbuf;
  while (*p) {
    char *next = strchr(p, '/');
    if (next)
      *next = '\0';

    /* Append component to built path */
    if (built[0])
      strlcat(built, "/", sizeof(built));
    strlcat(built, p, sizeof(built));

    /* The proc core makes /proc/driver itself (proc_root_init); a second
     * proc_mkdir of it WARNs "already registered". Every path is created
     * from the root by its full name, so there is nothing to look up. */
    if (strcmp(built, "driver") == 0)
      parent = NULL;
    else
      parent = nvgpu_proc_mkdir_cached(built, NULL);

    if (next) {
      *next = '/';
      p = next + 1;
    } else {
      break;
    }
  }

  /* No entry for the leaf's directory (it is /proc/driver, or its mkdir
   * failed): name the leaf in full, which the proc core resolves from the
   * root, rather than dropping it into /proc itself. */
  if (!parent) {
    *slash = '/';
    *leaf_name = pathbuf;
  }

  return parent;
}

int nvgpu_proc_init(struct nvgpu_device *dev) {
  struct nvgpu_msg_hdr *req;
  const u8 *p, *end;
  u8 *resp_buf;
  /* 512 KiB — vastly more than needed, avoids any size guessing */
  const size_t resp_size = 512 * 1024;
  struct nvgpu_file_rec rec;
  bool truncated;
  u32 used;
  int ret = 0;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  if (!req)
    return -ENOMEM;

  /*
   * Any memory will do: nvgpu_send_recv_used() copies through transport
   * buffers, so this is never put on the ring itself. It used to be, with
   * sg_init_one(), which is wrong for the vmalloc memory kvmalloc may return.
   */
  resp_buf = kvzalloc(resp_size, GFP_KERNEL);
  if (!resp_buf) {
    kfree(req);
    return -ENOMEM;
  }

  req->msg_type = cpu_to_le32(NVGPU_MSG_GET_PROC_FILES);
  req->handle = 0;
  req->status = 0;
  req->req_id = 0;

  ret = nvgpu_send_recv_used(dev, req, sizeof(*req), resp_buf, resp_size,
                             &used);
  if (ret < 0) {
    dev_err(&dev->vdev->dev, "virtio-gpu-nv: GET_PROC_FILES failed: %d\n", ret);
    goto out;
  }

  p = resp_buf;
  end = resp_buf + used;

  nvgpu_dir_cache_reset();

  while (nvgpu_file_rec_next(&p, end, &rec, &truncated)) {
    struct nvgpu_proc_buf *buf;
    char *pathbuf, *leaf;
    struct proc_dir_entry *parent = NULL;

    /*
     * Only under /proc/driver/nvidia, which remove() takes down whole: an
     * entry anywhere else would outlive its data. The backend sends nothing
     * else (device/src/nvidia/hostnodes.rs prefixes every path).
     */
    if (rec.path_len <= sizeof(NVGPU_PROC_ROOT) ||
        memcmp(rec.path, NVGPU_PROC_ROOT "/", sizeof(NVGPU_PROC_ROOT))) {
      dev_dbg(&dev->vdev->dev,
              "virtio-gpu-nv: a proc file outside " NVGPU_PROC_ROOT
              " skipped\n");
      continue;
    }

    buf = kzalloc(sizeof(*buf), GFP_KERNEL);
    if (!buf) {
      ret = -ENOMEM;
      goto out;
    }

    buf->data = kmemdup(rec.content, rec.content_len, GFP_KERNEL);
    if (!buf->data) {
      kfree(buf);
      ret = -ENOMEM;
      goto out;
    }
    buf->len = rec.content_len;

    pathbuf = kmemdup_nul(rec.path, rec.path_len, GFP_KERNEL);
    if (!pathbuf) {
      kfree(buf->data);
      kfree(buf);
      ret = -ENOMEM;
      goto out;
    }

    parent = nvgpu_proc_mkdir_parents(pathbuf, &leaf);
    if (proc_create_data(leaf, 0444, parent, &nvgpu_proc_buf_ops, buf)) {
      list_add(&buf->node, &dev->proc_bufs);
      dev_dbg(&dev->vdev->dev, "virtio-gpu-nv: /proc/%s (%u bytes)\n",
              pathbuf, rec.content_len);
    } else {
      /* A name the proc core refused (a duplicate): nothing reads it. */
      dev_dbg(&dev->vdev->dev, "virtio-gpu-nv: /proc/%s not made\n",
              pathbuf);
      kfree(buf->data);
      kfree(buf);
    }

    kfree(pathbuf);
  }
  if (truncated)
    dev_warn(&dev->vdev->dev, "virtio-gpu-nv: proc stream truncated\n");

out:
  kvfree(resp_buf);
  kfree(req);
  return ret;
}

/* Take /proc/driver/nvidia down, readers waited out, then free its data. */
void nvgpu_proc_cleanup(struct nvgpu_device *dev) {
  struct nvgpu_proc_buf *b, *n;

  remove_proc_subtree(NVGPU_PROC_ROOT, NULL);
  list_for_each_entry_safe(b, n, &dev->proc_bufs, node) {
    list_del(&b->node);
    kfree(b->data);
    kfree(b);
  }
}
