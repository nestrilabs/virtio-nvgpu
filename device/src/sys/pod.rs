//! Wire structs as bytes and back.
//!
//! The protocol's messages (protocol::messages) and the virtio config space
//! are `repr(C)` structs of integers: any bytes are a valid value of one,
//! and -- checked below, field by field -- none has padding, so every byte
//! of one is initialised. [`Pod`] says so, for exactly those types; the
//! rest of the crate reads and writes them through [`read`], [`bytes`] and
//! [`write`], which check lengths.

use protocol::messages::*;

/// A type of integers only, with no padding: valid for any bytes, and every
/// byte of a value initialised.
///
/// # Safety
/// Implemented only here, for the integers, arrays of a Pod type, and types
/// that are `repr(C)` (or `repr(C, packed)`) structs whose fields are
/// integers or arrays of them, with no padding between or after them (the
/// `no_padding` test below destructures each exhaustively and adds the
/// fields' sizes up).
pub unsafe trait Pod: Sized + 'static {}

/// A `T` from `buf` at `off`, if `buf` holds one there.
pub fn read<T: Pod + Copy>(buf: &[u8], off: usize) -> Option<T> {
    let end = off.checked_add(size_of::<T>())?;
    let b = buf.get(off..end)?;
    // SAFETY: `b` holds size_of::<T>() bytes, and any bytes are a valid T
    // (the Pod contract); read_unaligned takes no alignment from `b`.
    Some(unsafe { b.as_ptr().cast::<T>().read_unaligned() })
}

/// `v`'s bytes.
pub fn bytes<T: Pod>(v: &T) -> &[u8] {
    // SAFETY: every byte of a Pod value is initialised (no padding), and the
    // slice borrows `v` for exactly its size.
    unsafe { std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

/// Write `v` into `buf` at `off`: the bytes written, or `None` (nothing
/// written) if it does not fit.
pub fn write<T: Pod>(buf: &mut [u8], off: usize, v: &T) -> Option<usize> {
    let n = size_of::<T>();
    buf.get_mut(off..off.checked_add(n)?)?
        .copy_from_slice(bytes(v));
    Some(n)
}

macro_rules! pod {
    ($($t:ident { $($f:ident),* $(,)? })*) => {
        $(
            // SAFETY: a repr(C) struct of integers and integer arrays; the
            // `no_padding` test checks its fields fill it.
            unsafe impl Pod for $t {}
        )*

        #[cfg(test)]
        #[test]
        fn no_padding() {
            $(
                {
                    let v: $t = read(&[0u8; size_of::<$t>()], 0).unwrap();
                    let $t { $($f),* } = v;
                    let fields = 0 $(+ size_of_val(&$f))*;
                    assert_eq!(size_of::<$t>(), fields, "{} has padding", stringify!($t));
                }
            )*
        }
    };
}

pod! {
    MsgHeader { msg_type, handle, status, req_id }
    OpenReq { device_type, flags }
    IoctlReq { cmd, data_len, nested_offset, nested_len, deep_ptr_offset, deep_len }
    DeepSegHdr { count, reserved }
    DeepSeg { ptr_offset, len }
    OsDescHdr { nruns, flags }
    OsDescRun { gpa, pages, reserved }
    ProcId { start_ns, tgid, euid }
    IoctlResp { data_len, nested_len, deep_len }
    MmapReq { size, offset, prot, padding }
    MmapResp { guest_phys_addr, size, mapping_id, caching, flags, reserved }
    MunmapReq { mapping_id, padding }
    FileEntry { path_len, content_len }
    HelloReq { proto, flags, guest_caps, uvm_aperture_mib }
    HelloResp { proto, backend_caps, max_req, max_resp, num_cards, reserved }
    TimeSyncResp { host_mono_ns }
    TimeSyncResp2 { host_mono_ns, host_realtime_ns, host_mono_raw_ns, reserved }
    Ioctl2Req { cmd, flags, nbuf, nfd, ngem, ndyn, data_len, render }
    Ioctl2FdIn { buf, off, handle, flags }
    Ioctl2GemIn { buf, off, owner, gem }
    Ioctl2Dyn { kind, buf, off, len }
    Ioctl2Resp { ret, nbuf, nfd, ngem, data_len, reserved }
    Ioctl2FdOut { buf, off, handle, kind }
    Ioctl2GemOut { buf, off, gem, reserved, size }
    WatchReq { handle, flags, cookie }
    UnwatchReq { handle, pad }
    HostOpReq { op, nargs, args }
    HostOpResp { nres, pad, res }
    EvRec { kind, len, cookie }
    EvFence { status, pad, timestamp_ns }
    EvHotplug { flags, pad }
    CardRecord { name_len, major, minor, render_index }
    WlRecvReq { max_bytes, max_desc }
    WlSendResp { accepted, backlog }
}

// SAFETY: repr(C, packed) -- no padding by construction -- of integers and
// integer arrays (virtio.rs: the config space, its GPU slots and its
// descriptor-translation entries).
unsafe impl Pod for crate::virtio::VirtioGpuNvConfig {}

// SAFETY: integers: any bytes are a value, and every byte is part of it.
unsafe impl Pod for u8 {}
// SAFETY: as above.
unsafe impl Pod for u16 {}
// SAFETY: as above.
unsafe impl Pod for u32 {}
// SAFETY: as above.
unsafe impl Pod for u64 {}
// SAFETY: as above.
unsafe impl Pod for i32 {}
// SAFETY: as above.
unsafe impl Pod for i64 {}
// SAFETY: an array of a Pod type has no padding of its own: its elements
// are laid out back to back, each of which is all value.
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}
