//! Everything in this crate that is `unsafe`, and nothing else.
//!
//! The rest of the crate is `#![forbid(unsafe_code)]` (lib.rs, and each
//! module's own attribute), and `scripts/check-unsafe.sh` fails the build of
//! a tree where `unsafe` -- or a raw address made from a pointer -- appears
//! outside this directory. So the whole of what the backend trusts itself to
//! get right without the compiler's help is here, each block with a
//! `SAFETY:` comment saying what makes it sound:
//!
//! - [`block`]: the host kernel's parameter blocks. An [`block::Arena`] owns
//!   the argument of one call and every buffer a pointer in it addresses,
//!   and is the only thing that writes an address into one -- only into a
//!   field declared a pointer, and only another of its blocks. Guest bytes
//!   are copied in as data; the host block is built, never patched, and the
//!   reply is a copy with the caller's own values back in place.
//! - [`ioctl`]: the one `ioctl(2)` with an argument, which takes only an
//!   argument an arena built.
//! - [`guarded`]: the buffers the host writes into, hard against a guard
//!   page.
//! - [`mem`]: mappings, each owned by a type that unmaps it once, and every
//!   `MAP_FIXED` checked to land inside a range that type owns; and
//!   [`mem::HostSpan`], the only non-arena memory a pointer slot can name.
//! - [`fd`], [`net`]: descriptors and sockets, returned as `OwnedFd`.
//! - [`proc`]: identity, privileges, limits, Landlock and seccomp.
//! - [`pod`]: the protocol's wire structs as bytes and back.
//!
//! What cannot be made sound by construction, and is said so where it is:
//! which fields of a parameter block the host dereferences (the ABI tables,
//! measured from the drivers' sources), and that a descriptor number the
//! backend sets is the file it means (the handle table's, by `BorrowedFd`).

pub mod block;
pub mod fd;
pub mod guarded;
pub mod ioctl;
pub mod mem;
pub mod net;
pub mod pod;
pub mod proc;
