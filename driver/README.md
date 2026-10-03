# driver

The guest kernel module, `virtio_gpu_nv.ko`. Licensed GPL-2.0
(`LICENSE-GPL-2.0`), which kernel symbol access requires.

The module registers NVIDIA's device nodes in the guest: `/dev/nvidiactl`,
`/dev/nvidia0` to `/dev/nvidiaN`, `/dev/nvidia-modeset`, `/dev/nvidia-uvm`, and
a DRM render node per GPU. NVIDIA's own user-mode driver runs unchanged on top
of them. The module forwards each `ioctl()` and `mmap()` on the control queue
and waits for the backend's answer on the same queue. Readiness for `poll()`
arrives on the event queue.

The module copies parameter blocks and does not interpret RM calls. The few
things it has to do itself are things only the guest kernel can do:

- Replace a guest file descriptor inside a parameter block with the backend's
  handle for the same file.
- Pin the pages behind memory a process registers by CPU address, and send
  their guest-physical addresses.
- Map what the backend placed. Device memory comes from the shared window,
  region 1. A CUDA semaphore pool comes from the UVM aperture, region 2, mapped
  write-back.

Which files the guest is shown, which UVM calls exist and how large each RM
allocation block is come from the backend at probe time. The module carries
the RM control tables for every supported release and picks one by the host's
version, which it reads from device config.

## Building

```sh
make -C driver KDIR=/path/to/guest/kernel/build
```

`KDIR` is the build tree of the guest kernel the module loads into, not the
host's. In a kernel tree, `CONFIG_VIRTIO_GPU_NV` builds it in tree.

`guest-kernel.config` is the configuration the rig's guest kernel is built
with. `scripts/build-guest-kernel.sh` builds that kernel. The module's
vermagic must match the guest kernel exactly, and `rig.sh module` prints it.

`gen/` and `rmctrl/` hold headers generated from NVIDIA's sources by the
scripts in the top-level `gen/`. Don't edit them by hand.
