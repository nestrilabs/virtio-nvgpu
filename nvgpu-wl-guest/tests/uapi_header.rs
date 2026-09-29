// SPDX-License-Identifier: Apache-2.0
//! `driver/uapi/nvgpu_wl.h` against its mirrors, `nvgpu_wl_guest::uapi` and
//! `wlwire::frame`: every integer the header defines, and every struct's
//! size and field offsets. The daemon and the guest module ship separately,
//! and nothing else checks that they agree.

use nvgpu_wl_guest::uapi::*;
use protocol::cheader::Header;
use std::mem::{offset_of, size_of};
use wlwire::frame;

fn header() -> Header {
    Header::parse(include_str!("../../driver/uapi/nvgpu_wl.h"))
}

#[test]
fn every_define_is_mirrored() {
    let h = header();
    assert_eq!(h.bare, ["_UAPI_NVGPU_WL_H"]);
    let ours: &[(&str, u64)] = &[
        ("NVGPU_WL_UAPI_VERSION", UAPI_VERSION.into()),
        ("NVGPU_WL_CAP_WAYLAND", CAP_WAYLAND.into()),
        ("NVGPU_WL_CAP_EXPORT", CAP_EXPORT.into()),
        ("NVGPU_WL_CAP_DRM_FILE", CAP_DRM_FILE.into()),
        ("NVGPU_WL_CAP_DMABUF_IMPORT", CAP_DMABUF_IMPORT.into()),
        ("NVGPU_WL_CAP_SYNCOBJ", CAP_SYNCOBJ.into()),
        ("NVGPU_WL_MAX_DEVMAP", MAX_DEVMAP as u64),
        ("NVGPU_WL_DEV_RENDER", DEV_RENDER.into()),
        ("NVGPU_WL_DEV_CARD", DEV_CARD.into()),
        ("NVGPU_WL_CONNECT", CONNECT.into()),
        ("NVGPU_WL_LISTEN", LISTEN.into()),
        ("NVGPU_WL_ACCEPT", ACCEPT.into()),
        ("NVGPU_WL_XFER_MORE", XFER_MORE.into()),
        ("NVGPU_WL_IOC_HELLO", IOC_HELLO),
        ("NVGPU_WL_IOC_CONNECT", IOC_CONNECT),
        ("NVGPU_WL_IOC_SEND", IOC_SEND),
        ("NVGPU_WL_IOC_RECV", IOC_RECV),
        ("NVGPU_WL_IOC_CONNECT_FOR", IOC_CONNECT_FOR),
        ("NVGPU_WL_FRAME_MAGIC", frame::FRAME_MAGIC.into()),
        ("NVGPU_WL_FRAME_VERSION", frame::FRAME_VERSION.into()),
        ("NVGPU_WL_MAX_DESC", frame::MAX_DESC as u64),
        ("NVGPU_WL_MIN_FRAME", MIN_FRAME as u64),
        ("NVGPU_WL_FRAME_F_MORE", frame::FRAME_F_MORE.into()),
        ("NVGPU_WL_DESC_DMABUF", frame::DESC_DMABUF.into()),
        ("NVGPU_WL_DESC_SHM_POOL", frame::DESC_SHM_POOL.into()),
        ("NVGPU_WL_DESC_BLOB", frame::DESC_BLOB.into()),
        ("NVGPU_WL_DESC_STREAM", frame::DESC_STREAM.into()),
        ("NVGPU_WL_DESC_DRM_FILE", frame::DESC_DRM_FILE.into()),
        ("NVGPU_WL_DESC_SYNCOBJ", frame::DESC_SYNCOBJ.into()),
        ("NVGPU_WL_DESC_F_INVALID", frame::DESC_F_INVALID.into()),
        ("NVGPU_WL_REC_HELLO", frame::REC_HELLO.into()),
        ("NVGPU_WL_REC_WAYLAND", frame::REC_WAYLAND.into()),
        ("NVGPU_WL_REC_STREAM_DATA", frame::REC_STREAM_DATA.into()),
        ("NVGPU_WL_REC_STREAM_EOF", frame::REC_STREAM_EOF.into()),
        (
            "NVGPU_WL_REC_STREAM_CREDIT",
            frame::REC_STREAM_CREDIT.into(),
        ),
        ("NVGPU_WL_REC_SHM_SYNC", frame::REC_SHM_SYNC.into()),
        ("NVGPU_WL_REC_BLOB", frame::REC_BLOB.into()),
        ("NVGPU_WL_REC_ERROR", frame::REC_ERROR.into()),
        ("NVGPU_WL_REC_HANGUP", frame::REC_HANGUP.into()),
    ];
    for (name, value) in &h.defines {
        let Some((_, mine)) = ours.iter().find(|(n, _)| n == name) else {
            panic!("{name} has no mirror");
        };
        assert_eq!(value, mine, "{name}");
    }
    for (name, _) in ours {
        assert!(h.defines.contains_key(*name), "{name} is not in the header");
    }
}

#[test]
fn every_struct_is_laid_out_as_the_header_says() {
    let h = header();
    macro_rules! fields {
        ($t:ty, [$($f:ident),* $(,)?]) => {
            (size_of::<$t>(), vec![$((stringify!($f).to_string(), offset_of!($t, $f))),*])
        };
    }
    let mirrors = [
        (
            "nvgpu_wl_devmap",
            fields!(
                Devmap,
                [host_major, host_minor, guest_major, guest_minor, flags, pad]
            ),
        ),
        (
            "nvgpu_wl_hello",
            fields!(
                Hello,
                [version, caps, clock_offset_ns, max_frame, ndev, dev]
            ),
        ),
        ("nvgpu_wl_connect", fields!(Connect, [mode, flags])),
        (
            "nvgpu_wl_connect_for",
            fields!(ConnectFor, [mode, flags, pid, pad]),
        ),
        (
            "nvgpu_wl_xfer",
            fields!(
                Xfer,
                [frame, len, max_desc, card_fd, render_fd, flags, backlog]
            ),
        ),
    ];
    // The frame's own structs wlwire writes field by field: where each field
    // lands in what it writes.
    let desc = frame::Desc {
        kind: 1,
        flags: 2,
        fd: 3,
        a: 4,
        b: 5,
        c: 6,
    };
    let mut d = Vec::new();
    desc.write(&mut d);
    let rec = frame::record(7, 8, 9, &[0; 10]);
    let unit = frame::Unit {
        rec: rec.clone(),
        descs: vec![frame::DescOut::plain(desc)],
    };
    let (f, _) = frame::pack(&mut [unit].into(), 1 << 20, 1, false);
    // (struct, the bytes written, each field as (name, width, value)).
    type Written<'a> = (&'a str, &'a [u8], &'a [(&'a str, usize, u64)]);
    let written: [Written; 3] = [
        (
            "nvgpu_wl_frame_hdr",
            &f[..frame::FRAME_HDR_LEN],
            &[
                ("magic", 4, u64::from(frame::FRAME_MAGIC)),
                ("version", 2, u64::from(frame::FRAME_VERSION)),
                ("ndesc", 2, 1),
                ("rec_len", 4, rec.len() as u64),
                ("flags", 4, 0),
            ],
        ),
        (
            "nvgpu_wl_desc",
            &d,
            &[
                ("kind", 2, 1),
                ("flags", 2, 2),
                ("fd", 4, 3),
                ("a", 4, 4),
                ("b", 4, 5),
                ("c", 8, 6),
            ],
        ),
        (
            "nvgpu_wl_rec",
            &rec[..frame::REC_HDR_LEN],
            &[
                ("type", 2, 7),
                ("flags", 2, 0),
                ("len", 4, 10),
                ("id", 4, 8),
                ("arg", 4, 9),
            ],
        ),
    ];
    for (name, c) in &h.structs {
        if let Some((_, bytes, fields)) = written.iter().find(|w| w.0 == name) {
            assert_eq!(c.size, bytes.len(), "sizeof(struct {name})");
            assert_eq!(c.fields.len(), fields.len(), "struct {name}");
            for &(field, width, value) in *fields {
                let at = c.offset(field);
                let mut v = [0u8; 8];
                v[..width].copy_from_slice(&bytes[at..at + width]);
                assert_eq!(u64::from_le_bytes(v), value, "struct {name}, {field}");
            }
            continue;
        }
        let Some((_, (size, fields))) = mirrors.iter().find(|m| m.0 == name) else {
            panic!("struct {name} has no mirror");
        };
        assert_eq!((c.size, &c.fields), (*size, fields), "struct {name}");
    }
}
