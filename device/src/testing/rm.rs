// SPDX-License-Identifier: Apache-2.0
//! Test-only: a host RM with an object tree, for the backend's RM tests.
//!
//! Installed as a backend's host ioctl, it answers the escapes the
//! backend's RM gates sit in front of -- RM_ALLOC, RM_CONTROL (its
//! parameters kept, [`FakeRm::last_params`], and CLIENT_GET_HANDLE_INFO's
//! class answered from the tree), RM_FREE, RM_DUP_OBJECT, RM_SHARE, and
//! EXPORT_TO_DMABUF_FD (`FakeRm::export`) -- as resserv does (NVIDIA's
//! src/nvidia/src/libraries/resserv): a client unknown to it is
//! INVALID_OBJECT_HANDLE, an object unknown in a known client is
//! OBJECT_NOT_FOUND, a new handle already in use is INSERT_DUPLICATE_NAME,
//! a zero one is generated and written back, and freeing an object frees
//! everything under it. A test that expects RM to refuse gets RM's refusal,
//! not an NV_OK the backend's gate was never tested against.
//!
//! What a test needs beyond the tree -- a control RM does not serve, a
//! class it does not know, a refusal of its own -- it asks for with
//! [`FakeRm::only_controls`], [`FakeRm::only_classes`] or a hook.
//!
//! The host ioctl is a `fn`, so the RM is the test thread's: one per
//! thread, made fresh by [`install`].

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::RawFd;

use abi::ioctl::{
    NV_ESC_EXPORT_TO_DMABUF_FD, NV_ESC_RM_ALLOC, NV_ESC_RM_CONTROL, NV_ESC_RM_DUP_OBJECT,
    NV_ESC_RM_FREE, NV_ESC_RM_SHARE,
};

use crate::hostfd;
use crate::le::{put_u32, u32_at};
use crate::nvidia::NvidiaBackend;
use crate::nvos::*;

pub const NV_ERR_INSERT_DUPLICATE_NAME: u32 = 0x19;
pub const NV_ERR_OBJECT_NOT_FOUND: u32 = 0x57;

/// Where RM's generated handles start (resserv's RS_UNIQUE_HANDLE_BASE and
/// the client handle base the driver uses).
const OBJECT_BASE: u32 = 0xcaf0_0000;
const CLIENT_BASE: u32 = 0xc1d0_0001;

/// One RM call that reached the fake, as a test asks after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Call {
    /// The escape: NV_ESC_RM_ALLOC and so on.
    pub nr: u32,
    pub client: u32,
    /// The object the call is on: the parent of an allocation, the source
    /// object of a duplicate.
    pub object: u32,
    /// The class of an allocation, the command of a control; 0 otherwise.
    pub key: u32,
}

/// A status a test has RM answer a call with, in place of its own; `None`
/// leaves it to the tree. Asked only of a call the tree would allow.
pub type Hook = fn(&Call) -> Option<u32>;

#[derive(Clone, Copy)]
struct Object {
    parent: u32,
    class: u32,
}

#[derive(Default)]
pub struct FakeRm {
    /// (client, handle); a client is its own object, its parent 0.
    objects: BTreeMap<(u32, u32), Object>,
    next_client: u32,
    next_object: u32,
    seen: Vec<Call>,
    /// The parameters of the last RM_CONTROL RM served, as it read them.
    pub last_params: Vec<u8>,
    controls: Option<BTreeSet<u32>>,
    classes: Option<BTreeSet<u32>>,
    hook: Option<Hook>,
}

std::thread_local! {
    static RM: RefCell<FakeRm> = RefCell::new(FakeRm::new());
}

/// A fresh RM on this thread, and `be`'s host ioctl answered by it.
pub fn install(be: &mut NvidiaBackend) {
    RM.with(|rm| *rm.borrow_mut() = FakeRm::new());
    be.set_host_ioctl_for_test(serve);
}

/// This thread's RM.
pub fn with<R>(f: impl FnOnce(&mut FakeRm) -> R) -> R {
    RM.with(|rm| f(&mut rm.borrow_mut()))
}

/// The calls that reached RM since the last asking, in order.
pub fn seen() -> Vec<Call> {
    with(|rm| std::mem::take(&mut rm.seen))
}

impl FakeRm {
    fn new() -> Self {
        Self {
            next_client: CLIENT_BASE,
            next_object: OBJECT_BASE,
            ..Self::default()
        }
    }

    /// RM serves these controls and no other: the rest are NOT_SUPPORTED.
    pub fn only_controls(&mut self, cmds: &[u32]) {
        self.controls = Some(cmds.iter().copied().collect());
    }

    /// RM knows these classes, besides a client's, and no other: the rest
    /// are INVALID_CLASS.
    pub fn only_classes(&mut self, classes: &[u32]) {
        self.classes = Some(classes.iter().copied().collect());
    }

    pub fn set_hook(&mut self, hook: Hook) {
        self.hook = Some(hook);
    }

    pub fn exists(&self, client: u32, h: u32) -> bool {
        self.objects.contains_key(&(client, h))
    }

    /// Allocate `h` (0: a handle RM picks) of `class` under `parent` in
    /// `client`, as RM_ALLOC does: a root class makes a client, and
    /// `client` and `parent` are not looked at. The handle, or RM's status.
    pub fn alloc(&mut self, client: u32, parent: u32, h: u32, class: u32) -> Result<u32, u32> {
        if ROOT_CLASSES.contains(&class) {
            let h = match h {
                0 => self.fresh_client(),
                h if self.objects.contains_key(&(h, h)) => {
                    return Err(NV_ERR_INSERT_DUPLICATE_NAME);
                }
                h => h,
            };
            self.objects.insert((h, h), Object { parent: 0, class });
            return Ok(h);
        }
        self.client(client)?;
        self.object(client, parent)?;
        if self.classes.as_ref().is_some_and(|c| !c.contains(&class)) {
            return Err(NV_ERR_INVALID_CLASS);
        }
        let h = self.new_handle(client, h)?;
        self.objects.insert((client, h), Object { parent, class });
        Ok(h)
    }

    /// Free `h` in `client`, and every object under it; `h == client`
    /// frees the client.
    pub fn free(&mut self, client: u32, h: u32) -> Result<(), u32> {
        self.client(client)?;
        self.object(client, h)?;
        let mut gone = vec![h];
        while let Some(p) = gone.pop() {
            self.objects.remove(&(client, p));
            gone.extend(
                self.objects
                    .iter()
                    .filter(|&(&(c, _), o)| c == client && o.parent == p)
                    .map(|(&(_, h), _)| h),
            );
        }
        Ok(())
    }

    /// A client handle not given before: RM moves on from the last one,
    /// and a freed client's number is not handed out again at once.
    fn fresh_client(&mut self) -> u32 {
        while self
            .objects
            .contains_key(&(self.next_client, self.next_client))
        {
            self.next_client += 1;
        }
        self.next_client += 1;
        self.next_client - 1
    }

    fn client(&self, client: u32) -> Result<(), u32> {
        match self.objects.get(&(client, client)) {
            Some(o) if o.parent == 0 => Ok(()),
            _ => Err(NV_ERR_INVALID_OBJECT_HANDLE),
        }
    }

    fn object(&self, client: u32, h: u32) -> Result<Object, u32> {
        self.objects
            .get(&(client, h))
            .copied()
            .ok_or(NV_ERR_OBJECT_NOT_FOUND)
    }

    /// A new object's handle in `client`, as clientValidateNewResourceHandle
    /// takes it, or the one RM generates for 0.
    fn new_handle(&mut self, client: u32, h: u32) -> Result<u32, u32> {
        if h == 0 {
            while self.objects.contains_key(&(client, self.next_object)) {
                self.next_object += 1;
            }
            self.next_object += 1;
            return Ok(self.next_object - 1);
        }
        if h == client {
            return Err(NV_ERR_INVALID_OBJECT_HANDLE);
        }
        if self.objects.contains_key(&(client, h)) {
            return Err(NV_ERR_INSERT_DUPLICATE_NAME);
        }
        Ok(h)
    }

    fn hook(&self, call: &Call) -> Result<(), u32> {
        match self.hook.and_then(|h| h(call)) {
            Some(status) => Err(status),
            None => Ok(()),
        }
    }

    /// One escape's block; the status RM leaves in it.
    fn escape(&mut self, nr: u32, a: &mut [u8]) -> Option<u32> {
        let w = |at| u32_at(a, at);
        let status = match nr {
            NV_ESC_RM_ALLOC => {
                let (client, parent, h, class) = (
                    w(NVOS64_H_ROOT)?,
                    w(NVOS64_H_OBJECT_PARENT)?,
                    w(NVOS64_H_OBJECT_NEW)?,
                    w(NVOS64_H_CLASS)?,
                );
                self.seen(nr, client, parent, class);
                let call = Call {
                    nr,
                    client,
                    object: parent,
                    key: class,
                };
                let made = self
                    .hook(&call)
                    .and_then(|()| self.alloc(client, parent, h, class));
                if let Ok(h) = made {
                    put_u32(a, NVOS64_H_OBJECT_NEW, h)?;
                }
                made.err()
            }
            NV_ESC_RM_CONTROL => {
                let (client, object, cmd) =
                    (w(NVOS54_H_CLIENT)?, w(NVOS54_H_OBJECT)?, w(NVOS54_CMD)?);
                self.seen(nr, client, object, cmd);
                let call = Call {
                    nr,
                    client,
                    object,
                    key: cmd,
                };
                self.client(client)
                    .and_then(|()| self.object(client, object).map(drop))
                    .and_then(|()| match &self.controls {
                        Some(c) if !c.contains(&cmd) => Err(NV_ERR_NOT_SUPPORTED),
                        _ => Ok(()),
                    })
                    .and_then(|()| self.hook(&call))
                    .err()
            }
            NV_ESC_RM_FREE => {
                let (client, h) = (w(NVOS00_H_ROOT)?, w(NVOS00_H_OBJECT_OLD)?);
                self.seen(nr, client, h, 0);
                let call = Call {
                    nr,
                    client,
                    object: h,
                    key: 0,
                };
                self.hook(&call).and_then(|()| self.free(client, h)).err()
            }
            NV_ESC_RM_DUP_OBJECT => {
                let (client, parent, h) = (
                    w(NVOS55_H_CLIENT)?,
                    w(NVOS55_H_PARENT)?,
                    w(NVOS55_H_OBJECT)?,
                );
                let (src_client, src) = (w(NVOS55_H_CLIENT_SRC)?, w(NVOS55_H_OBJECT_SRC)?);
                self.seen(nr, client, src, 0);
                let call = Call {
                    nr,
                    client,
                    object: src,
                    key: 0,
                };
                let made = self
                    .client(src_client)
                    .and_then(|()| self.object(src_client, src))
                    .and_then(|o| {
                        self.client(client)?;
                        self.object(client, parent)?;
                        self.hook(&call)?;
                        let h = self.new_handle(client, h)?;
                        self.objects.insert(
                            (client, h),
                            Object {
                                parent,
                                class: o.class,
                            },
                        );
                        Ok(h)
                    });
                if let Ok(h) = made {
                    put_u32(a, NVOS55_H_OBJECT, h)?;
                }
                made.err()
            }
            NV_ESC_RM_SHARE => {
                let (client, object) = (w(NVOS57_H_CLIENT)?, w(NVOS57_H_OBJECT)?);
                self.seen(nr, client, object, 0);
                let call = Call {
                    nr,
                    client,
                    object,
                    key: 0,
                };
                self.client(client)
                    .and_then(|()| self.object(client, object).map(drop))
                    .and_then(|()| self.hook(&call))
                    .err()
            }
            _ => return Some(NV_OK),
        };
        Some(status.unwrap_or(NV_OK))
    }

    /// EXPORT_TO_DMABUF_FD's block `a` (either layout), as nv_dma_buf_export
    /// makes a new dma-buf (`fd` -1): no check of whose `hClient` is, each
    /// handle an object of it that is no client, and the dma-buf a memfd
    /// named `nv_dmabuf` of `totalSize` bytes, its number written to `fd`.
    /// The append form is refused as for a dma-buf already whole
    /// (NV_ERR_INVALID_ARGUMENT). The status.
    fn export(&mut self, a: &mut [u8]) -> Option<u32> {
        let l = crate::rmexport::Layout::for_len(a.len())?;
        let (fd, client) = (crate::le::i32_at(a, 0)?, u32_at(a, 4)?);
        let num = u32_at(a, 12)?;
        let first = u32_at(a, l.handles)?;
        self.seen(NV_ESC_EXPORT_TO_DMABUF_FD, client, first, num);
        let call = Call {
            nr: NV_ESC_EXPORT_TO_DMABUF_FD,
            client,
            object: first,
            key: num,
        };
        if fd != -1 || num == 0 || num > 128 {
            return Some(NV_ERR_INVALID_ARGUMENT);
        }
        if let Err(s) = self.client(client) {
            return Some(s);
        }
        for i in 0..num as usize {
            let h = u32_at(a, l.handles + i * 4)?;
            match self.object(client, h) {
                Ok(o) if !ROOT_CLASSES.contains(&o.class) => {}
                Ok(_) => return Some(NV_ERR_INVALID_OBJECT_HANDLE),
                Err(s) => return Some(s),
            }
        }
        if let Err(s) = self.hook(&call) {
            return Some(s);
        }
        let size = crate::le::u64_at(a, 24)?;
        let m = crate::sys::fd::memfd(&export_name(), libc::MFD_CLOEXEC).ok()?;
        crate::sys::fd::ftruncate(&m, size).ok()?;
        let raw = std::os::fd::IntoRawFd::into_raw_fd(m);
        a[0..4].copy_from_slice(&raw.to_le_bytes());
        Some(NV_OK)
    }

    fn seen(&mut self, nr: u32, client: u32, object: u32, key: u32) {
        self.seen.push(Call {
            nr,
            client,
            object,
            key,
        });
    }
}

/// An RM_CONTROL RM served: its parameters, as RM read them, kept for the
/// test ([`FakeRm::last_params`]); and CLIENT_GET_HANDLE_INFO's CLASSID
/// answered from the tree, as cliresCtrlCmdClientGetHandleInfo_IMPL does.
fn control_params(arg: &mut crate::sys::block::Arg<'_>) -> u32 {
    let (b, mut others) = arg.split();
    let (Some(client), Some(cmd), Some(p), Some(size)) = (
        u32_at(b, NVOS54_H_CLIENT),
        u32_at(b, NVOS54_CMD),
        crate::le::u64_at(b, NVOS54_PARAMS),
        u32_at(b, NVOS54_PARAMS_SIZE),
    ) else {
        return NV_OK;
    };
    let params = others.read(p, size as usize).unwrap_or_default();
    let mut status = NV_OK;
    if cmd == 0x0d02 && u32_at(&params, 4) == Some(2) {
        let class = u32_at(&params, 0)
            .and_then(|h| with(|rm| rm.object(client, h).ok()))
            .map(|o| o.class);
        match class {
            Some(c) => {
                others.write(p + 8, &u64::from(c).to_le_bytes());
            }
            None => status = NV_ERR_OBJECT_NOT_FOUND,
        }
    }
    with(|rm| rm.last_params = params);
    status
}

/// The name of this thread's export memfds: `nv_dmabuf` and the thread, so
/// a test counting the ones still open counts its own
/// ([`open_exports`]).
pub fn export_name() -> std::ffi::CString {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    std::thread_local! {
        static ME: u64 = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let me = ME.with(|m| *m);
    std::ffi::CString::new(format!("nv_dmabuf-{me}-")).expect("no NUL")
}

/// How many of this thread's export memfds this process has open.
pub fn open_exports() -> usize {
    let want = format!("/memfd:{}", export_name().to_string_lossy());
    std::fs::read_dir("/proc/self/fd")
        .map(|d| {
            d.filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
                .filter(|l| l.to_string_lossy().starts_with(&want))
                .count()
        })
        .unwrap_or(0)
}

/// Where an escape's block holds its status.
fn status_at(nr: u32) -> Option<(usize, usize)> {
    Some(match nr {
        NV_ESC_RM_ALLOC => (NVOS64_SIZE, NVOS64_STATUS),
        NV_ESC_RM_CONTROL => (NVOS54_SIZE, NVOS54_STATUS),
        NV_ESC_RM_FREE => (NVOS00_SIZE, NVOS00_STATUS),
        NV_ESC_RM_DUP_OBJECT => (NVOS55_SIZE, NVOS55_STATUS),
        NV_ESC_RM_SHARE => (NVOS57_SIZE, NVOS57_STATUS),
        _ => return None,
    })
}

/// The host ioctl: an RM escape of a size other than its block's is
/// EINVAL, as nvidia.ko's escape switch answers it; any other ioctl is
/// left as it came and succeeds.
fn serve(_: RawFd, request: u64, arg: &mut crate::sys::block::Arg<'_>) -> i32 {
    let request = request as u32;
    let nr = hostfd::ioc_nr(request);
    if hostfd::ioc_type(request) != b'F' {
        return 0;
    }
    if nr == NV_ESC_EXPORT_TO_DMABUF_FD {
        let size = hostfd::ioc_size(request);
        let (Some(a), Some(l)) = (
            arg.bytes().get_mut(..size),
            crate::rmexport::Layout::for_len(size),
        ) else {
            return -libc::EINVAL;
        };
        return match with(|rm| rm.export(a)) {
            Some(status) => {
                put_u32(a, l.status, status);
                0
            }
            None => -libc::EINVAL,
        };
    }
    let Some((size, at)) = status_at(nr) else {
        return 0;
    };
    // RM_ALLOC takes NVOS21 too: NVOS64 without its last fields.
    let (size, at) = if nr == NV_ESC_RM_ALLOC && hostfd::ioc_size(request) == NVOS21_SIZE {
        (NVOS21_SIZE, NVOS21_STATUS)
    } else {
        (size, at)
    };
    let Some(a) = arg.bytes().get_mut(..size) else {
        return -libc::EINVAL;
    };
    if hostfd::ioc_size(request) != size {
        return -libc::EINVAL;
    }
    let control = nr == NV_ESC_RM_CONTROL;
    let mut status = with(|rm| rm.escape(nr, a));
    if control && status == Some(NV_OK) {
        status = Some(control_params(arg));
    }
    let Some(a) = arg.bytes().get_mut(..size) else {
        return -libc::EINVAL;
    };
    match status {
        Some(status) => {
            put_u32(a, at, status);
            0
        }
        None => -libc::EINVAL,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_answers_as_resserv_does() {
        let mut rm = FakeRm::new();
        let c = rm.alloc(0, 0, 0, NV01_ROOT_CLIENT).unwrap();
        assert_eq!(c, CLIENT_BASE);
        assert_eq!(
            rm.alloc(0, 0, c, NV01_ROOT),
            Err(NV_ERR_INSERT_DUPLICATE_NAME)
        );
        assert_eq!(
            rm.alloc(c + 1, c + 1, 1, 0x80),
            Err(NV_ERR_INVALID_OBJECT_HANDLE)
        );
        assert_eq!(rm.alloc(c, 7, 1, 0x80), Err(NV_ERR_OBJECT_NOT_FOUND));
        assert_eq!(rm.alloc(c, c, c, 0x80), Err(NV_ERR_INVALID_OBJECT_HANDLE));
        assert_eq!(rm.alloc(c, c, 1, 0x80), Ok(1));
        assert_eq!(rm.alloc(c, c, 1, 0x80), Err(NV_ERR_INSERT_DUPLICATE_NAME));
        assert_eq!(rm.alloc(c, 1, 2, 0x2080), Ok(2));
        let m = rm.alloc(c, 2, 0, 0x3e).unwrap();
        assert_eq!(m, OBJECT_BASE);
        rm.only_classes(&[0x80]);
        assert_eq!(rm.alloc(c, c, 3, 0x3e), Err(NV_ERR_INVALID_CLASS));
        // Freeing the device frees what is under it, and nothing else.
        let other = rm.alloc(0, 0, 0, NV01_ROOT).unwrap();
        assert_eq!(rm.alloc(other, other, 1, 0x80), Ok(1));
        rm.free(c, 1).unwrap();
        assert!(!rm.exists(c, 2) && !rm.exists(c, m) && rm.exists(c, c));
        assert!(rm.exists(other, 1));
        assert_eq!(rm.free(c, 1), Err(NV_ERR_OBJECT_NOT_FOUND));
        rm.free(other, other).unwrap();
        assert!(!rm.exists(other, 1));
        assert_eq!(rm.free(other, other), Err(NV_ERR_INVALID_OBJECT_HANDLE));
    }
}
