// SPDX-License-Identifier: Apache-2.0
//! Capture injection (`--inject-socket PATH --inject-uid UID`): host buffers
//! a guest may open without a copy.
//!
//! A screen share on the host is a PipeWire stream of dma-bufs the desktop's
//! portal backend allocated and the compositor fills. A per-VM helper on the
//! host -- built elsewhere, run as a uid of its own -- asks the portal on the
//! guest application's behalf, consumes the stream, and hands each buffer to
//! this backend over a socket of its own ([`InjectServer`]). The backend
//! checks what it was given ([`Registry::import`]) and keeps it under an id
//! and a random token; a guest process that knows both opens it with HOST_OP
//! INJECT_OPEN, which imports the same object into the calling guest file's
//! host render file, where the guest module makes a proxy of it and a guest
//! dma-buf. PipeWire, the portal and every stream protocol stay out of this
//! process: what it parses is one fixed-size packet format
//! (`protocol::inject`).
//!
//! **Who may inject.** Only peers whose `SO_PEERCRED` uid is `--inject-uid`,
//! at most [`MAX_PEERS`] at once. The socket is bound in a private directory
//! and renamed into place, 0600, like the export socket, for the operator to
//! open to the helper's group; or systemd binds it, open to that group, and
//! hands it over ([`InjectServer::from_listener`];
//! contrib/systemd/vhost-user-nvgpu-inject@.socket). The backend
//! cannot know whether the user consented to what the helper sends: the
//! helper uid is trusted for that, and for nothing else (SECURITY.md,
//! "Capture injection").
//!
//! **What is accepted.** Each plane's descriptor must be a dma-buf
//! (`fstatfs`'s magic), and must import into a render file of this GPU,
//! opened for the purpose and held by the backend, as nvidia-drm memory:
//! GEM_IDENTIFY_OBJECT says NVKMS, which a dma-buf of another device (an
//! iGPU's, a udmabuf, a v4l2 frame) does not -- nvidia-drm imports those as
//! dma-buf objects. Every plane must resolve to one object, whose size must
//! hold every plane the layout describes, all arithmetic checked. The
//! backend keeps the object's handle and the dma-buf while the id lives:
//! at most [`MAX_BUFFERS`] ids and [`MAX_BYTES`] per VM.
//!
//! **Who may open.** A guest message names an id and its token, compared in
//! constant time; nothing a guest sends lists, enumerates or makes an id. The
//! object is imported into the caller's own render file (not a handle of the
//! backend's): the guest's reference is then a GEM handle of that file, which
//! keeps the memory alive past the helper's RELEASE, as any importer's does.
//! RELEASE (and the helper's hangup) only stops new opens. What INJECT_OPEN
//! has made is bounded per VM and per guest process ([`MAX_OPENS`]).
//!
//! **Explicit sync.** IMPORT_SYNCOBJ hands over a DRM syncobj file, at most
//! [`MAX_SYNCOBJS`] per VM, under an id and a token of its own; HOST_OP
//! INJECT_OPEN_SYNCOBJ imports it into the caller's render file, whose
//! handle is the guest's (fences are the host's, fence.rs). What its points
//! mean is the helper's and the guest daemon's business: the backend reads
//! none of them, and a guest signalling any point, or none, reaches only the
//! helper and the stream it serves.
//!
//! **Read-only, for the CPU.** Every placement of the object's mmap offset in
//! the window is made read-only ([`BackendInject::read_only`]), whichever of
//! the VM's files maps it; the guest module refuses a writable mapping of a
//! read-only placement. The GPU is another matter: nvidia-drm and RM give an
//! importer read-write GPU mappings, and there is no read-only import to ask
//! for, so a guest process holding the buffer can write it with the GPU --
//! only its own stream's buffers, which only it and the helper see.
//!
//! The files: `check.rs` (the formats and the layout rule), `host.rs` (the
//! host kernel calls, behind [`InjectHost`]), `registry.rs` (the injected
//! buffers and syncobjs, their tokens, and the [`Taint`] set),
//! `backend.rs` (INJECT_OPEN and what it made), `server.rs` (the helper's
//! socket and its packets), and `fake.rs` (a fake nvidia-drm for the tests
//! and the fuzz target).

#![forbid(unsafe_code)]

mod backend;
mod check;
#[cfg(any(test, fuzzing))]
pub mod fake;
mod host;
mod registry;
mod server;
#[cfg(test)]
mod tests;

pub use backend::BackendInject;
pub use check::{check_layout, check_request};
pub use host::{InjectHost, SysInjectHost};
pub use registry::{FileId, Opened, Registry, SharedTaint, Taint, exportable};
pub use server::{InjectServer, Served, serve_packet};

/// Injected buffers one VM may hold at once. A screen share wants four to
/// eight buffers a stream, and a VM a few streams.
pub const MAX_BUFFERS: usize = 32;
/// Bytes of injected objects one VM may hold at once: sixteen 2560x1440
/// ARGB buffers and room to spare.
pub const MAX_BYTES: u64 = 1 << 30;
/// Injected syncobjs one VM may hold at once: one or two a stream.
pub const MAX_SYNCOBJS: usize = 16;
/// Helper connections at once.
pub const MAX_PEERS: usize = 4;
/// The largest width or height accepted.
pub const MAX_DIM: u32 = 16384;
/// GEM handles INJECT_OPEN may have made in the VM's render files at once
/// (one per render file and object); a guest process a quarter
/// ([`Share::quarter`](crate::quota::Share::quarter)).
pub const MAX_OPENS: u64 = 1024;
