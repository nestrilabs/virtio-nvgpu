//! IOCTL2: the schema-driven vectored ioctl (protocol v2).
//!
//! The guest sends an ioctl's argument and every buffer a pointer in it
//! reaches. This module is the authority on what that request may be: it
//! walks the backend's own schema for (class, cmd) over the bytes received,
//! recomputes every pointer field, length, descriptor field and GEM field, and
//! refuses anything that disagrees. Guest-supplied layout is never trusted.
//!
//! Three phases, so the host ioctl can run without the backend mutex:
//!  - `prepare` (under the mutex): parse, validate, build host buffers, `dup`
//!    every descriptor the call needs.
//!  - `Prepared::execute` (no mutex, possibly on an executor thread): GEM
//!    re-homing and validation, the host ioctl, GEM-out re-homing, closing
//!    temporaries. Everything it touches it owns.
//!  - `Prepared::finish` (under the mutex): adopt fd outs, build the response.
//!
//! Workstream SCHEMA owns this file; workstream BACKEND calls it. The API
//! below is the agreed contract.

use std::os::fd::{OwnedFd, RawFd};

use crate::hostfd::HandleKind;
use crate::schema::SchemaClass;

/// Why a request was refused before reaching the host. Carried back to the
/// guest as a negative errno in the response header.
pub type Errno = i32;

/// What `prepare` needs from the backend's state.
pub trait Env {
    /// The descriptor and kind behind a handle, duplicated so the call owns it
    /// for as long as it needs (a racing CLOSE cannot pull it away).
    fn dup_handle(&self, handle: u32) -> Option<(OwnedFd, HandleKind)>;
    /// The kind of a handle without duplicating it.
    fn kind(&self, handle: u32) -> Option<HandleKind>;
    /// The NVKMS schema for the host driver version, if one exists.
    fn nvkms_version(&self) -> Option<abi::version::DriverVersion>;
}

/// A validated request ready to run.
pub struct Prepared {
    _private: (),
}

/// Parse and validate an IOCTL2 payload (the bytes after the MsgHeader) for a
/// call on `target` (a handle of `target_kind`). `render` is the request's
/// render handle field, already checked to be a render handle.
pub fn prepare(
    env: &dyn Env,
    class: SchemaClass,
    target: u32,
    target_kind: HandleKind,
    payload: &[u8],
) -> Result<Prepared, Errno> {
    let _ = (env, class, target, target_kind, payload);
    Err(libc::ENOTTY)
}

impl Prepared {
    /// Whether the schema asks for this call to run on the target file's
    /// serial executor rather than inline on the queue thread.
    pub fn wants_executor(&self) -> bool {
        true
    }

    /// Run the host side: GEM-in re-homing into the target file and NVKMS-type
    /// validation (for framebuffer-creating calls), the host ioctl on
    /// `target_fd`, GEM-out re-homing into the render file, closing every
    /// temporary. Returns the host ioctl's result (0 or -errno).
    pub fn execute(&mut self, target_fd: RawFd) -> i32 {
        let _ = target_fd;
        -libc::ENOTTY
    }

    /// Adopt descriptors the host produced at schema positions (via `adopt`,
    /// which inserts into the handle table and returns (handle, kind)), then
    /// build the response payload (`Ioctl2Resp` onwards, without MsgHeader).
    pub fn finish(self, adopt: &mut dyn FnMut(OwnedFd) -> (u32, HandleKind)) -> Vec<u8> {
        let _ = adopt;
        Vec::new()
    }

    /// The host copy of buffer `i` (0 = the ioctl argument), for policy hooks
    /// that inspect a request before it runs or a reply after.
    pub fn buffer(&self, i: usize) -> Option<&[u8]> {
        let _ = i;
        None
    }

    /// Mutable access for policy hooks that sanitise a request (e.g. clearing
    /// NVKMS override flags) before `execute`.
    pub fn buffer_mut(&mut self, i: usize) -> Option<&mut [u8]> {
        let _ = i;
        None
    }

    /// The ioctl number being run.
    pub fn cmd(&self) -> u32 {
        0
    }
}
