// SPDX-License-Identifier: GPL-2.0-only
/*
 * virtio-gpu-nv: the GPU as the guest's PCI core sees it -- a root bus and
 * device at the host GPU's own address, serving the host's config space --
 * and GET_SYS_FILES, which brings that config space with the DRI devices,
 * the host card nodes and their GET_DEV_INFO sizes.
 */

#include <linux/kernel.h>
#include <linux/numa.h>
#include <linux/pci.h>
#include <linux/slab.h>
#include <linux/string.h>

#include "nvgpu.h"

/* ───────── The fake PCI bus ───────── */

static int nvgpu_pci_read(struct pci_bus *bus, unsigned int devfn, int where,
                          int size, u32 *val) {
  struct nvgpu_pci_root *root =
      container_of(to_pci_sysdata(bus), struct nvgpu_pci_root, sd);
  u8 slot = PCI_SLOT(devfn);
  u8 func = PCI_FUNC(devfn);

  /* Only respond to our specific device */
  if (slot != root->slot.slot || func != root->slot.func) {
    *val = ~0u;
    return PCIBIOS_DEVICE_NOT_FOUND;
  }

  if (!root->slot.config_valid || where + size > (int)sizeof(root->slot.config)) {
    *val = ~0u;
    return PCIBIOS_BAD_REGISTER_NUMBER;
  }

  switch (size) {
  case 1:
    *val = root->slot.config[where];
    break;
  case 2:
    *val = le16_to_cpu(*(u16 *)&root->slot.config[where]);
    break;
  case 4:
    *val = le32_to_cpu(*(u32 *)&root->slot.config[where]);
    break;
  default:
    *val = ~0u;
    return PCIBIOS_BAD_REGISTER_NUMBER;
  }
  return PCIBIOS_SUCCESSFUL;
}

static int nvgpu_pci_write(struct pci_bus *bus, unsigned int devfn, int where,
                           int size, u32 val) {
  /* Config space is read-only from guest perspective */
  return PCIBIOS_FUNC_NOT_SUPPORTED;
}

static struct pci_ops nvgpu_pci_ops = {
    .read = nvgpu_pci_read,
    .write = nvgpu_pci_write,
};

/* Parse "DDDD:BB:SS.F" into components.
 * Returns 0 on success. */
static int nvgpu_parse_pci_addr(const char *addr, u16 *domain, u8 *bus,
                                u8 *slot, u8 *func) {
  unsigned int d, b, s, f;

  if (sscanf(addr, "%04x:%02x:%02x.%1x", &d, &b, &s, &f) != 4)
    return -EINVAL;

  *domain = (u16)d;
  *bus = (u8)b;
  *slot = (u8)s;
  *func = (u8)f;
  return 0;
}

/*
 * Whether the bus the GPU's host address names is already in this guest. The
 * device can only be registered where the host has it (userspace finds the
 * GPU by that address), and a bus the VMM already populated cannot be made
 * again: pci_scan_root_bus_bridge() refuses it with -EEXIST, saying why only
 * at dev_dbg. The usual cause is a VMM bridge whose secondary bus is the GPU's:
 * crosvm's hot-plug root port sits on 00:xx and has bus 1 behind it, and many
 * hosts have their GPU at 0000:01:00.0. Asked first so the log names both.
 */
static bool nvgpu_pci_bus_taken(struct nvgpu_device *dev,
                                const struct nvgpu_pci_root *root) {
  struct pci_bus *b = pci_find_bus(root->slot.domain, root->slot.bus_nr);

  if (!b)
    return false;
  if (b->self)
    dev_err(&dev->vdev->dev,
            "virtio-gpu-nv: cannot put the GPU at its host address %s: bus "
            "%04x:%02x already exists in this guest, behind the VMM's bridge "
            "%s [%04x:%04x]. The VMM has a device at the host GPU's address; "
            "start it without that bridge (crosvm: --no-pci-hotplug-port)\n",
            root->slot.pci_addr, root->slot.domain, root->slot.bus_nr,
            pci_name(b->self), b->self->vendor, b->self->device);
  else
    dev_err(&dev->vdev->dev,
            "virtio-gpu-nv: cannot put the GPU at its host address %s: bus "
            "%04x:%02x is already one of this guest's root buses. The VMM "
            "has devices at the host GPU's address\n",
            root->slot.pci_addr, root->slot.domain, root->slot.bus_nr);
  return true;
}

int nvgpu_pci_init(struct nvgpu_device *dev) {
  int i, ret, err = 0;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];
    struct pci_host_bridge *bridge;

    if (!root->slot.config_valid) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: no config space for %s, skipping\n",
               root->slot.pci_addr);
      continue;
    }

    if (nvgpu_pci_bus_taken(dev, root)) {
      err = err ?: -EEXIST;
      continue;
    }

    bridge = pci_alloc_host_bridge(0);
    if (!bridge) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_alloc_host_bridge failed for %s\n",
              root->slot.pci_addr);
      err = err ?: -ENOMEM;
      continue;
    }

    /* One bus resource covering exactly our bus number */
    root->bus_res = (struct resource){
        .start = root->slot.bus_nr,
        .end = root->slot.bus_nr,
        .flags = IORESOURCE_BUS,
    };
    pci_add_resource(&bridge->windows, &root->bus_res);

    bridge->dev.parent = &dev->vdev->dev;
    /* x86 reads the domain from the sysdata (pci_domain_nr()), not from
     * bridge->domain_nr, which stays PCI_DOMAIN_NR_NOT_SET: set, the bridge's
     * release takes it for a number this driver allocated from the PCI
     * core's emulated-domain IDA and frees it there, and ida_free() WARNs on
     * a number it never handed out. That was the WARN on every failed scan,
     * and the reason the bridge was never freed after a successful one. */
    root->sd.domain = (int)root->slot.domain;
    /* No node to claim: the GPU is the host's, and the guest's idea of
     * distance to it means nothing. NUMA_NO_NODE lets every allocation made
     * against this device fall back to the caller's node. */
    root->sd.node = NUMA_NO_NODE;
    bridge->sysdata = &root->sd;
    bridge->ops = &nvgpu_pci_ops;
    bridge->busnr = root->slot.bus_nr;
    root->nvdev = dev;

    ret = pci_scan_root_bus_bridge(bridge);
    if (ret) {
      dev_err(&dev->vdev->dev,
              "virtio-gpu-nv: pci_scan_root_bus_bridge %s: %d%s\n",
              root->slot.pci_addr, ret,
              ret == -EEXIST ? " (bus already present in this guest)" : "");
      pci_free_host_bridge(bridge);
      err = err ?: ret;
      continue;
    }

    pci_bus_add_devices(bridge->bus);
    root->bridge = bridge;
    root->registered = true;

    /* Save the one pci_dev on this bus so DRI init can use it as a parent */
    {
      struct pci_dev *pdev;
      list_for_each_entry(pdev, &bridge->bus->devices, bus_list) {
        root->pdev = pdev;
        break;
      }
    }

    dev_dbg(&dev->vdev->dev, "virtio-gpu-nv: registered fake PCI device %s\n",
            root->slot.pci_addr);
  }

  return err;
}

void nvgpu_pci_cleanup(struct nvgpu_device *dev) {
  int i;

  for (i = 0; i < dev->num_pci_roots; i++) {
    struct nvgpu_pci_root *root = &dev->pci_roots[i];

    if (!root->registered)
      continue;

    /* Removing the root bus deletes the bridge's device and drops the bus's
     * reference to it; the one pci_alloc_host_bridge() gave is ours. */
    pci_remove_root_bus(root->bridge->bus);
    pci_free_host_bridge(root->bridge);
    root->bridge = NULL;
    root->pdev = NULL;
    root->registered = false;
  }
}

/* ───────── GET_SYS_FILES ───────── */

/*
 * Section 3: the host card nodes. A backend sends it in every mode, for the
 * host card numbers (the Wayland devmap maps a compositor's scanout dev_t by
 * them); the cards are openable only when it also says NVGPU_BCAP_KMS_CARD
 * (nvgpu_kms_open() checks, and the backend refuses OPEN_KMS otherwise). An
 * older backend sends it only with --kms-card, or ends the stream after
 * section 2, and then there is nothing between `p` and `end` and no card is
 * recorded; the parse never runs past what the device wrote.
 */
static const u8 *nvgpu_parse_card_section(struct nvgpu_device *dev,
                                          const u8 *p, const u8 *end) {
  struct nvgpu_card_record rec;
  __le32 raw_count;
  u32 count, i;

  dev->num_card_recs = 0;
  if (end - p < (ptrdiff_t)sizeof(raw_count))
    return NULL;
  memcpy(&raw_count, p, sizeof(raw_count));
  count = le32_to_cpu(raw_count);
  p += sizeof(raw_count);

  for (i = 0; i < count; i++) {
    struct nvgpu_card_rec *c;
    u32 name_len, render_index;

    if (end - p < (ptrdiff_t)sizeof(rec)) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card section truncated at entry %u\n", i);
      return NULL;
    }
    memcpy(&rec, p, sizeof(rec));
    p += sizeof(rec);
    name_len = le32_to_cpu(rec.name_len);
    render_index = le32_to_cpu(rec.render_index);
    if (name_len == 0 || name_len > end - p) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card entry %u bad name_len %u\n", i, name_len);
      return NULL;
    }

    /*
     * Kept in the order sent even when unusable, because EV_HOTPLUG names a
     * card by its position here; one that names no DRI device we registered
     * is recorded and never attached.
     */
    if (dev->num_card_recs >= NVGPU_MAX_DRI_DEVS) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card entry %u is past the %d this driver keeps\n",
               i, NVGPU_MAX_DRI_DEVS);
      return NULL;
    }
    c = &dev->cards[dev->num_card_recs];
    memset(c->name, 0, sizeof(c->name));
    memcpy(c->name, p, min_t(u32, name_len, sizeof(c->name) - 1));
    c->major = le32_to_cpu(rec.major);
    c->minor = le32_to_cpu(rec.minor);
    c->render_index = render_index;
    p += name_len;

    if (render_index < (u32)dev->num_dri_devs &&
        dev->dri_devs[render_index].card_index < 0)
      dev->dri_devs[render_index].card_index = dev->num_card_recs;
    else
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: card %s names DRI record %u, which is not "
               "one this guest keeps or already has a card\n",
               c->name, render_index);

    dev_dbg(&dev->vdev->dev,
            "virtio-gpu-nv: host card node %s (%u:%u) for DRI record %u\n",
            c->name, c->major, c->minor, render_index);
    dev->num_card_recs++;
  }
  return p;
}

/*
 * Section 4: the size of each DRI record's host GET_DEV_INFO struct, in
 * section 2's order. The record's nine words are the 36-byte layout whatever
 * this says (the backend normalises a 535 host's 20 bytes, and 545's and
 * 550's 28 and 32); the size says which of them the host really had, which
 * nvgpu_drm_get_dev_info() uses to name a guest-userspace/host-kernel release
 * mismatch. Absent from an older backend, whose records stay at 36.
 */
static void nvgpu_parse_dev_info_sizes(struct nvgpu_device *dev, const u8 *p,
                                       const u8 *end) {
  __le32 raw;
  u32 count, i;

  if (!p || end - p < (ptrdiff_t)sizeof(raw))
    return;
  memcpy(&raw, p, sizeof(raw));
  count = le32_to_cpu(raw);
  p += sizeof(raw);
  if (count > (u32)((end - p) / sizeof(raw))) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: GET_DEV_INFO size section truncated\n");
    return;
  }
  for (i = 0; i < count && i < (u32)dev->num_dri_devs; i++) {
    memcpy(&raw, p + 4 * i, sizeof(raw));
    dev->dri_devs[i].dev_info_size = le32_to_cpu(raw);
  }
}

/*
 * Section 1, one record: a GPU's config space
 * ("bus/pci/devices/<addr>/config"), kept for the fake PCI device at that
 * address if the address is one of the GPU slots the config space named. Other PCI sysfs files (vendor, device...)
 * the kernel makes itself once the pci_dev is registered; other paths are
 * skipped.
 */
static void nvgpu_sys_file(struct nvgpu_device *dev,
                           const struct nvgpu_file_rec *rec) {
  char path[256] = {};
  char pci_addr[16] = {};
  char *rest, *slash;
  int pi, gi;

  /* NUL-terminated, however long the path the backend sent. */
  memcpy(path, rec->path, min(rec->path_len, (u32)(sizeof(path) - 1)));
  if (strncmp(path, "bus/pci/devices/", 16) != 0)
    return;
  rest = path + 16; /* "<addr>/<filename>" */
  slash = strchr(rest, '/');
  if (!slash || strcmp(slash + 1, "config") != 0)
    return;
  memcpy(pci_addr, rest, min((size_t)(slash - rest), sizeof(pci_addr) - 1));

  /* Find existing slot or allocate new one */
  for (pi = 0; pi < dev->num_pci_roots; pi++)
    if (strcmp(dev->pci_roots[pi].slot.pci_addr, pci_addr) == 0)
      break;

  /* Match against known GPU slots to avoid creating
   * entries for unrelated PCI devices */
  if (pi == dev->num_pci_roots) {
    /* The slots config space had: num_gpus may say up to 248. */
    for (gi = 0;
         gi < (int)min_t(u32, dev->num_gpus, ARRAY_SIZE(dev->gpu_slots));
         gi++) {
      struct nvgpu_pci_slot *ps;

      if (strcmp(dev->gpu_slots[gi].pci_addr, pci_addr) != 0)
        continue;
      pi = dev->num_pci_roots;
      if (pi < NVGPU_MAX_PCI_SLOTS) {
        ps = &dev->pci_roots[pi].slot;
        memcpy(ps->pci_addr, pci_addr, sizeof(pci_addr));
        if (nvgpu_parse_pci_addr(pci_addr, &ps->domain, &ps->bus_nr,
                                 &ps->slot, &ps->func) == 0)
          dev->num_pci_roots++;
        else
          pi = dev->num_pci_roots; /* parse failed */
      }
      break;
    }
  }

  if (pi < dev->num_pci_roots) {
    struct nvgpu_pci_slot *ps = &dev->pci_roots[pi].slot;
    u32 copy = min(rec->content_len, (u32)sizeof(ps->config));

    memcpy(ps->config, rec->content, copy);
    ps->config_valid = true;
    dev_dbg(&dev->vdev->dev,
            "virtio-gpu-nv: stored config space for %s (%u bytes)\n",
            pci_addr, copy);
  }
}

/*
 * Section 2: the DRI devices, a u32 count and that many records of
 * {name_len, major, minor, slot_index, dev_info[9]} and the name. Every
 * record is walked, and only the first NVGPU_MAX_DRI_DEVS kept: section 3
 * starts after the last one, so stopping early would leave the card records
 * unreachable. Where section 3 starts, or NULL if this one did not end
 * whole.
 */
static const u8 *nvgpu_parse_dri_section(struct nvgpu_device *dev,
                                         const u8 *p, const u8 *end) {
  __le32 raw_num_dri;
  u32 num_dri, i;

  if (end - p < (ptrdiff_t)sizeof(raw_num_dri)) {
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: sys stream truncated before DRI section\n");
    return NULL;
  }
  memcpy(&raw_num_dri, p, sizeof(__le32));
  num_dri = le32_to_cpu(raw_num_dri);
  p += 4;

  dev->num_dri_devs = 0;
  for (i = 0; i < num_dri; i++) {
    __le32 raw_name_len, raw_major, raw_minor, raw_slot, raw_info;
    u32 name_len, major, minor, slot_index, nl;
    u32 info[NVGPU_DEV_INFO_WORDS];
    struct nvgpu_dri_dev *d;
    int w;

    /* name_len + major + minor + slot_index, then the dev_info words */
    if (end - p < NVGPU_DRI_RECORD_BYTES) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: DRI section truncated at entry %u\n", i);
      return NULL;
    }

    memcpy(&raw_name_len, p, sizeof(__le32));
    memcpy(&raw_major, p + 4, sizeof(__le32));
    memcpy(&raw_minor, p + 8, sizeof(__le32));
    memcpy(&raw_slot, p + 12, sizeof(__le32));
    name_len = le32_to_cpu(raw_name_len);
    major = le32_to_cpu(raw_major);
    minor = le32_to_cpu(raw_minor);
    slot_index = le32_to_cpu(raw_slot);
    for (w = 0; w < NVGPU_DEV_INFO_WORDS; w++) {
      memcpy(&raw_info, p + 16 + 4 * w, sizeof(__le32));
      info[w] = le32_to_cpu(raw_info);
    }
    p += NVGPU_DRI_RECORD_BYTES;

    if (name_len == 0 || name_len > end - p) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: DRI entry %u bad name_len %u\n", i, name_len);
      return NULL;
    }
    if (dev->num_dri_devs >= NVGPU_MAX_DRI_DEVS) {
      dev_warn(&dev->vdev->dev,
               "virtio-gpu-nv: DRI entry %u is past the %d this driver "
               "keeps\n",
               i, NVGPU_MAX_DRI_DEVS);
      p += name_len;
      continue;
    }

    d = &dev->dri_devs[dev->num_dri_devs];
    nl = min(name_len, (u32)(sizeof(d->name) - 1));
    memset(d->name, 0, sizeof(d->name));
    memcpy(d->name, p, nl);
    d->major = major;
    d->minor = minor;
    d->slot_index = slot_index;
    d->card_index = -1;
    memcpy(d->dev_info, info, sizeof(info));
    d->dev_info_size = sizeof(info);
    dev->num_dri_devs++;

    dev_dbg(&dev->vdev->dev,
            "virtio-gpu-nv: DRI %s (%u:%u) slot %u, nvidia gpu_id=0x%x, "
            "page kind %u/%u, sector layout %u\n",
            d->name, major, minor, slot_index, info[0], info[4], info[5],
            info[6]);
    p += name_len;
  }
  return p;
}

int nvgpu_fetch_sys_files(struct nvgpu_device *dev) {
  struct nvgpu_msg_hdr *req;
  struct nvgpu_file_rec rec;
  const u8 *p, *end;
  u8 *resp_buf;
  const int resp_max = 128 * 1024;
  bool truncated;
  u32 used;
  int ret = 0;

  req = kzalloc(sizeof(*req), GFP_KERNEL);
  if (!req)
    return -ENOMEM;

  /* kvzalloc — zeroed so unwritten tail is never misread as data */
  resp_buf = kvzalloc(resp_max, GFP_KERNEL);
  if (!resp_buf) {
    kfree(req);
    return -ENOMEM;
  }

  req->msg_type = cpu_to_le32(NVGPU_MSG_GET_SYS_FILES);
  req->handle = 0;
  req->status = 0;
  req->req_id = 0;

  ret = nvgpu_send_recv_used(dev, req, sizeof(*req), resp_buf, resp_max,
                             &used);
  if (ret < 0)
    goto out;

  p = resp_buf;
  end = resp_buf + used;

  /* Section 1: sysfs files. */
  while (nvgpu_file_rec_next(&p, end, &rec, &truncated))
    nvgpu_sys_file(dev, &rec);
  if (truncated)
    dev_warn(&dev->vdev->dev,
             "virtio-gpu-nv: sys stream truncated at sysfs section\n");

  /* Sections 2, 3 and 4: DRI devices, card nodes, GET_DEV_INFO sizes. */
  p = nvgpu_parse_dri_section(dev, p, end);
  if (p)
    nvgpu_parse_dev_info_sizes(dev, nvgpu_parse_card_section(dev, p, end),
                               end);

out:
  kvfree(resp_buf);
  kfree(req);
  return ret;
}
