# device

The host side of virtio-nvgpu: a Rust library that serves the device, and
`vhost-user-nvgpu`, the backend binary built on it. Licensed Apache-2.0
(`LICENSE-APACHE-2.0`).

The library names no VMM. Everything a VMM owns is a trait the embedding VMM
implements: descriptor chains, the event queue, guest memory, and placing host
memory where the guest can reach it (`shm::WindowPlacer`). The backend binary
implements those traits over vhost-user, so any VMM with a vhost-user frontend
can run it.

## Layout

| path | what it does |
|---|---|
| `src/nvidia/mod.rs` | the backend's state for one VM, and the message loop |
| `src/nvidia/ioctl.rs` | forwarded ioctls, the ABI size check, RM allowlist refusals |
| `src/nvidia/rmctrl.rs`, `nested.rs` | RM controls whose parameters hold pointers: the backend supplies those buffers itself |
| `src/nvidia/uvm.rs` | UVM calls: size check, descriptor and client checks, the backend's own init flags |
| `src/nvidia/osdesc.rs`, `src/guestmem.rs` | memory a guest registers by CPU address, rebuilt from the guest's own pages |
| `src/nvidia/vidmem.rs`, `src/vram.rs` | the video memory limit: admission, charges, and what the guest is told |
| `src/nvidia/window.rs`, `src/shm.rs` | mappings placed in the shared window |
| `src/nvidia/aperture.rs` | CUDA semaphore pools, placed in the UVM aperture |
| `src/nvidia/open.rs`, `files.rs` | opens, closes, and the host files the guest is shown |
| `src/caps.rs` | `--caps`: which device nodes and RM classes a guest is served |
| `src/sandbox.rs`, `posture.rs` | seccomp, Landlock and the privilege drop, before the first guest message |
| `bin/vhost-user-nvgpu.rs` | the backend binary |

## Running it

```sh
vhost-user-nvgpu --socket /run/nvgpu/vm1.sock \
    --caps graphics,video,utility --vram-limit-mib 4096
```

The backend refuses to run as root or with `CAP_SYS_ADMIN`. It refuses to start
on a kernel without seccomp or Landlock, on a driver release older than every
ABI profile, and with `--vram-limit-mib` on a release with no video memory
table of its own. No flag turns any of these off. `--caps` lists what a guest
is served. Compute (`/dev/nvidia-uvm`) is off unless `compute` is in it.

One backend serves one VM. Each VM gets its own process and its own socket.

## Tests

```sh
cargo test -p device
```

The tests run against a fake host driver and need no GPU. Tests that need a
card run in a guest on a GPU host, through `scripts/rig/`.

Tables of NVIDIA's struct layouts and command lists come from `gen/`, one per
driver release. The backend picks the table for the host's release at start.
