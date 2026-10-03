# protocol

The wire format both halves agree on: message types, request and response
headers, and the constants that name devices.
Definitions only, no logic.

Licensed BSD-3-Clause OR GPL-2.0+ (`LICENSE-BSD-3-Clause`, `LICENSE-GPL-2.0`).
Apache-2.0 is not compatible with GPL-2.0, so the GPL guest module could not
include an Apache-2.0 header. The dual licence lets the module and the
Apache-2.0 host crate share one set of definitions. It covers what is written
here; code ported from elsewhere keeps its original terms.

`src/messages.rs` is the Rust side. The guest module's C side is in
`driver/virtio_gpu_nv.c`, and `messages.rs` mirrors it field for field.
Nothing checks the two against each other automatically, so a change to one
has to be made to the other in the same commit.

## Shared memory regions

| id | name | what goes in it |
|---|---|---|
| 1 | window | device memory the guest maps: RM mappings, DRM objects |
| 2 | UVM aperture | CUDA semaphore pools, one memory slot each, at offsets the backend picks |

The ids are defined in `device/bin/vhost-user-nvgpu.rs` and the guest
module, not here. The guest looks regions up by id, and the VMM decides which
BAR holds each one.
