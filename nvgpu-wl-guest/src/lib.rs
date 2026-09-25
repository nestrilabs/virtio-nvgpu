//! nvgpu-wl-guest: guest applications as clients of the host compositor.
//!
//! Listens where a Wayland compositor would (`$XDG_RUNTIME_DIR/wayland-0`),
//! and gives every client that connects its own channel through
//! `/dev/nvgpu-wl` to a connection the backend holds to the host compositor.
//! Both ends run the same engine (`wlwire`): the messages pass through with
//! their object ids untouched, and what cannot cross the VM boundary as it is
//! -- descriptors, device numbers, timestamps -- is translated on the way.
//!
//! With `--export`, the other way round: host applications reach a compositor
//! running in this guest.

pub mod channel;
pub mod daemon;
pub mod uapi;
