// SPDX-License-Identifier: Apache-2.0
//! Unit tests for the codec, the object table, the closure check, the frame
//! format, the policy, and the engine end to end (a guest engine facing a
//! client wired to a host engine facing a compositor, both in memory).

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::closure::{self, ModelIface, ModelMsg};
use crate::engine::*;
use crate::frame::{self, Desc, DescOut};
use crate::localin::LocalIn;
use crate::objects::{ObjError, Objects};
use crate::policy::{LeaseGate, Policy};
use crate::proto::{self, Dir, iface, op};
use crate::sys;
use crate::wire::{self, MsgBuilder, Val, WireError};

// ───────────────────────── codec ─────────────────────────

fn msg_desc(i: proto::IfaceId, dir: Dir, opcode: u16) -> &'static proto::Message {
    &iface(i).messages(dir)[opcode as usize]
}

#[test]
fn every_argument_type_decodes_to_what_was_encoded() {
    // uint + untyped new_id (string, uint, uint)
    let m = MsgBuilder::new(2, op::wl_registry::REQ_BIND)
        .uint(7)
        .generic_new_id("wl_compositor", 5, 9)
        .finish();
    let a = wire::parse(
        msg_desc(proto::WL_REGISTRY, Dir::Request, op::wl_registry::REQ_BIND),
        &m,
    )
    .unwrap();
    assert_eq!(a[0].val, Val::Uint(7));
    assert_eq!(
        a[1].val,
        Val::NewId {
            id: 9,
            iface: Some(b"wl_compositor"),
            version: 5
        }
    );

    // nullable object + ints
    let m = MsgBuilder::new(10, op::wl_surface::REQ_ATTACH)
        .object(0)
        .int(-3)
        .int(4)
        .finish();
    let a = wire::parse(
        msg_desc(proto::WL_SURFACE, Dir::Request, op::wl_surface::REQ_ATTACH),
        &m,
    )
    .unwrap();
    assert_eq!(a[0].val, Val::Object(0));
    assert_eq!(a[1].val, Val::Int(-3));

    // string + fd (the fd takes no bytes)
    let m = MsgBuilder::new(11, op::wl_data_offer::REQ_RECEIVE)
        .string(Some("text/plain"))
        .finish();
    let a = wire::parse(
        msg_desc(
            proto::WL_DATA_OFFER,
            Dir::Request,
            op::wl_data_offer::REQ_RECEIVE,
        ),
        &m,
    )
    .unwrap();
    assert_eq!(a[0].val, Val::Str(Some(b"text/plain")));
    assert_eq!(a[1].val, Val::Fd);

    // array
    let m = MsgBuilder::new(12, op::wl_keyboard::EVT_ENTER)
        .uint(1)
        .object(10)
        .array(&[1, 2, 3, 4, 5])
        .finish();
    let a = wire::parse(
        msg_desc(proto::WL_KEYBOARD, Dir::Event, op::wl_keyboard::EVT_ENTER),
        &m,
    )
    .unwrap();
    assert_eq!(a[2].val, Val::Array(&[1, 2, 3, 4, 5]));

    // fixed
    let m = MsgBuilder::new(13, op::wl_pointer::EVT_MOTION)
        .uint(5)
        .fixed(1.5)
        .fixed(-2.0)
        .finish();
    let a = wire::parse(
        msg_desc(proto::WL_POINTER, Dir::Event, op::wl_pointer::EVT_MOTION),
        &m,
    )
    .unwrap();
    assert_eq!(a[1].val, Val::Fixed(384));
    assert_eq!(a[2].val, Val::Fixed(-512));

    // typed new_id, fd, int
    let m = MsgBuilder::new(3, op::wl_shm::REQ_CREATE_POOL)
        .new_id(20)
        .int(4096)
        .finish();
    let a = wire::parse(
        msg_desc(proto::WL_SHM, Dir::Request, op::wl_shm::REQ_CREATE_POOL),
        &m,
    )
    .unwrap();
    assert_eq!(
        a[0].val,
        Val::NewId {
            id: 20,
            iface: None,
            version: 0
        }
    );
    assert_eq!(a[1].val, Val::Fd);
    assert_eq!(a[2].val, Val::Int(4096));
    assert_eq!(a[2].off, 12);
}

#[test]
fn a_malformed_message_is_refused_with_the_reason() {
    let d = msg_desc(
        proto::WL_DATA_OFFER,
        Dir::Request,
        op::wl_data_offer::REQ_RECEIVE,
    );
    // A size field that disagrees with the bytes.
    let mut m = MsgBuilder::new(11, 1).string(Some("a")).finish();
    m[6] = 0xff;
    assert_eq!(wire::parse(d, &m).unwrap_err(), WireError::BadSize);
    // A string without its NUL.
    let mut m = MsgBuilder::new(11, 1).string(Some("abc")).finish();
    m[15] = b'x';
    assert_eq!(wire::parse(d, &m).unwrap_err(), WireError::BadString);
    // A null where the signature forbids one.
    let m = MsgBuilder::new(11, 1).string(None).finish();
    assert_eq!(wire::parse(d, &m).unwrap_err(), WireError::Null);
    // Bytes after the last argument.
    let m = MsgBuilder::new(11, 1).string(Some("a")).uint(0).finish();
    assert_eq!(wire::parse(d, &m).unwrap_err(), WireError::Trailing);
    // A string running past the end.
    let m = MsgBuilder::new(11, 1).uint(64).finish();
    assert_eq!(wire::parse(d, &m).unwrap_err(), WireError::Short);
    // A typed new_id of zero.
    let m = MsgBuilder::new(3, 0).new_id(0).int(1).finish();
    let cp = msg_desc(proto::WL_SHM, Dir::Request, op::wl_shm::REQ_CREATE_POOL);
    assert_eq!(wire::parse(cp, &m).unwrap_err(), WireError::Null);
}

/// A buffer is walked whole message by whole message: a partial one waits,
/// and a size no message may have is refused as soon as its header is in,
/// however little of the rest has arrived.
#[test]
fn messages_are_taken_whole_and_a_bad_size_ends_the_walk() {
    let a = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
        .new_id(5)
        .finish();
    let b = MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
        .new_id(6)
        .finish();
    let two = [a.clone(), b.clone()].concat();
    let mut it = wire::Messages::new(&two[..two.len() - 1]);
    let (h, m) = it.next().unwrap().unwrap();
    assert_eq!(
        (h.object, h.opcode, m),
        (1, op::wl_display::REQ_SYNC, &a[..])
    );
    assert!(it.next().is_none(), "the second is not all there");
    assert_eq!((it.consumed(), it.rest()), (a.len(), &b[..b.len() - 1]));
    assert_eq!(wire::whole_message(&two[..4]), Ok(None));

    for size in [4u32, 10, (wire::MAX_MSG + 4) as u32] {
        let mut bad = a.clone();
        bad[4..8].copy_from_slice(&(size << 16).to_ne_bytes());
        let buf = [a.clone(), bad[..8].to_vec()].concat();
        let mut it = wire::Messages::new(&buf);
        assert!(it.next().unwrap().is_ok());
        assert_eq!(it.next(), Some(Err(WireError::BadSize)), "size {size}");
        assert_eq!(it.next(), None);
        assert_eq!(it.rest(), &bad[..8]);
    }
}

#[test]
fn descriptor_counts_and_classes_come_from_the_policy_table() {
    let keymap = msg_desc(proto::WL_KEYBOARD, Dir::Event, op::wl_keyboard::EVT_KEYMAP);
    assert_eq!(keymap.nfds, 1);
    assert_eq!(
        keymap.fd,
        Some(proto::FdKind::Blob {
            size_arg: 2,
            offset_arg: None
        })
    );
    let icc = msg_desc(
        proto::WP_IMAGE_DESCRIPTION_CREATOR_ICC_V1,
        Dir::Request,
        op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE,
    );
    assert_eq!(
        icc.fd,
        Some(proto::FdKind::Blob {
            size_arg: 2,
            offset_arg: Some(1)
        })
    );
    assert_eq!(
        msg_desc(
            proto::ZWP_LINUX_BUFFER_PARAMS_V1,
            Dir::Request,
            op::zwp_linux_buffer_params_v1::REQ_ADD
        )
        .fd,
        Some(proto::FdKind::Dmabuf)
    );
    assert_eq!(
        msg_desc(
            proto::ZWP_LINUX_DMABUF_FEEDBACK_V1,
            Dir::Event,
            op::zwp_linux_dmabuf_feedback_v1::EVT_MAIN_DEVICE
        )
        .rewrite,
        Some(proto::RewriteKind::DevT(0))
    );
    // Every message anywhere with a descriptor either has a class or belongs
    // to an interface no allowed global reaches.
    assert!(
        proto::INTERFACES
            .iter()
            .all(|i| i.requests.iter().chain(i.events).all(|m| m.nfds <= 2))
    );
}

// ───────────────────────── objects ─────────────────────────

#[test]
fn a_client_object_stays_a_zombie_until_delete_id() {
    let mut o = Objects::new();
    o.create(5, proto::WL_CALLBACK, 1, true).unwrap();
    o.destroyed_by_event(5); // wl_callback.done
    assert!(o.get(5).unwrap().zombie);
    // The client may not reuse the id yet.
    assert_eq!(
        o.create(5, proto::WL_CALLBACK, 1, true),
        Err(ObjError::InUse(5))
    );
    assert!(!o.delete_id(5));
    assert!(o.get(5).is_none());
    o.create(5, proto::WL_SURFACE, 4, true).unwrap();
}

#[test]
fn a_server_object_is_gone_at_its_destructor_event_and_reusable_after_a_request() {
    let mut o = Objects::new();
    let id = wire::SERVER_ID_START + 1;
    o.create(id, proto::WL_DATA_OFFER, 3, false).unwrap();
    o.destroyed_by_request(id);
    assert!(o.get(id).unwrap().zombie);
    // The server may hand the id out again.
    o.create(id, proto::WL_DATA_OFFER, 3, false).unwrap();
    o.destroyed_by_event(id);
    assert!(o.get(id).is_none());
}

#[test]
fn new_ids_must_be_in_their_creators_range_and_not_live() {
    let mut o = Objects::new();
    assert_eq!(
        o.create(wire::SERVER_ID_START, proto::WL_SURFACE, 1, true),
        Err(ObjError::WrongRange(0xff000000))
    );
    assert_eq!(
        o.create(7, proto::WL_SURFACE, 1, false),
        Err(ObjError::WrongRange(7))
    );
    assert_eq!(
        o.create(1, proto::WL_SURFACE, 1, true),
        Err(ObjError::InUse(1))
    );
    // delete_id of an object never destroyed reports it was live.
    o.create(8, proto::WL_SURFACE, 1, true).unwrap();
    assert!(o.delete_id(8));
}

// ───────────────────────── closure check ─────────────────────────

fn m(name: &str, nfds: usize, creates: &[&str]) -> ModelMsg {
    ModelMsg {
        name: name.into(),
        nfds,
        creates: creates.iter().map(|s| s.to_string()).collect(),
    }
}

fn model() -> Vec<ModelIface> {
    vec![
        ModelIface {
            name: "wl_display".into(),
            requests: vec![m("sync", 0, &["cb"])],
            events: vec![],
        },
        ModelIface {
            name: "cb".into(),
            requests: vec![],
            events: vec![m("done", 0, &[])],
        },
        ModelIface {
            name: "mgr".into(),
            requests: vec![m("get", 0, &["child"])],
            events: vec![],
        },
        // Reached only through an *event* new_id.
        ModelIface {
            name: "child".into(),
            requests: vec![],
            events: vec![m("offer", 0, &["offer"])],
        },
        ModelIface {
            name: "offer".into(),
            requests: vec![m("receive", 1, &[])],
            events: vec![],
        },
        ModelIface {
            name: "two".into(),
            requests: vec![m("both", 2, &[])],
            events: vec![],
        },
    ]
}

#[test]
fn the_closure_check_follows_new_ids_in_events_too() {
    let errs = closure::check(&model(), &["mgr"], &[]);
    assert_eq!(errs.len(), 1, "{errs:?}");
    assert!(errs[0].contains("offer.receive"), "{errs:?}");
    assert!(closure::check(&model(), &["mgr"], &[("offer", "receive")]).is_empty());
}

#[test]
fn the_closure_check_refuses_bad_policy_lines_and_unknown_interfaces() {
    let errs = closure::check(
        &model(),
        &["nope"],
        &[("offer", "recv"), ("two", "both"), ("zz", "a")],
    );
    assert!(
        errs.iter()
            .any(|e| e.contains("unknown message offer.recv")),
        "{errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("two.both carries 2")),
        "{errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("unknown interface zz")),
        "{errs:?}"
    );
    assert!(
        errs.iter().any(|e| e.contains("allowed global nope")),
        "{errs:?}"
    );
    let mut broken = model();
    broken[2].requests[0].creates = vec!["ghost".into()];
    let errs = closure::check(&broken, &["mgr"], &[("offer", "receive")]);
    assert!(errs.iter().any(|e| e.contains("ghost")), "{errs:?}");
}

/// The generated tables, as the closure check models them.
fn vendored_model() -> Vec<ModelIface> {
    proto::INTERFACES
        .iter()
        .map(|i| {
            let conv = |x: &proto::Message| ModelMsg {
                name: x.name.into(),
                nfds: x.nfds as usize,
                creates: x
                    .args
                    .iter()
                    .filter(|a| a.kind == proto::ArgKind::NewId)
                    .filter_map(|a| a.iface.map(|id| iface(id).name.to_string()))
                    .collect(),
            };
            ModelIface {
                name: i.name.into(),
                requests: i.requests.iter().map(conv).collect(),
                events: i.events.iter().map(conv).collect(),
            }
        })
        .collect()
}

#[test]
fn the_vendored_allowlist_is_closed() {
    // build.rs already refused to build otherwise; this keeps the claim next
    // to the tests and exercises the real tables.
    let model = vendored_model();
    let globals: Vec<&str> = crate::policy_table::GLOBALS
        .iter()
        .map(|g| g.interface)
        .collect();
    let pol: Vec<(&str, &str)> = crate::policy_table::FD_POLICY
        .iter()
        .map(|(a, b, _)| (*a, *b))
        .collect();
    assert_eq!(closure::check(&model, &globals, &pol), Vec::<String>::new());
    // And the hidden ones really are not listed.
    for hidden in [
        "wp_security_context_manager_v1",
        "zwlr_data_control_manager_v1",
        "ext_data_control_manager_v1",
        "zwp_virtual_keyboard_manager_v1",
        "zwp_input_method_manager_v2",
        "zwlr_screencopy_manager_v1",
        "ext_image_copy_capture_manager_v1",
        "zwlr_gamma_control_manager_v1",
        "hyprland_input_capture_manager_v1",
        "wl_drm",
        "zwlr_layer_shell_v1",
    ] {
        assert!(
            !crate::policy::listed(hidden),
            "{hidden} must not be allowed"
        );
    }
}

// ───────────────────────── frame ─────────────────────────

#[test]
fn a_frame_round_trips_with_its_descriptors_in_order() {
    let mut q = VecDeque::new();
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_BLOB, 1, 0, b"abc"),
        descs: vec![],
    });
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 2, &[0u8; 8]),
        descs: vec![
            DescOut::plain(Desc {
                a: 1,
                c: 3,
                ..Desc::new(frame::DESC_BLOB)
            }),
            DescOut::plain(Desc {
                a: 9,
                ..Desc::new(frame::DESC_STREAM)
            }),
        ],
    });
    let (mut f, fds) = frame::pack(&mut q, 1 << 20, 256, true);
    assert!(q.is_empty());
    assert_eq!(fds.len(), 2);
    let d = frame::decode(&f).unwrap();
    assert_eq!(d.flags, 0);
    assert_eq!(d.descs[0].kind, frame::DESC_BLOB);
    assert_eq!(d.descs[1].a, 9);
    let recs: Vec<_> = d.records().collect();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].payload, b"abc");
    assert_eq!(recs[1].arg, 2);
    // What a transport fills in changes that entry alone, in place.
    frame::patch_desc(&mut f, 1, |d| {
        d.fd = 42;
        d.flags |= frame::DESC_F_INVALID;
    });
    frame::patch_desc(&mut f, 2, |d| d.a = 7);
    let d = frame::decode(&f).unwrap();
    assert_eq!(frame::desc_at(&f, 0), Some(d.descs[0]));
    assert_eq!((d.descs[0].a, d.descs[0].c), (1, 3));
    assert_eq!((d.descs[1].a, d.descs[1].fd), (9, 42));
    assert!(d.descs[1].is_invalid());
    assert_eq!(d.records().count(), 2);
}

/// A record filled in place is byte for byte the record of what was written,
/// whether the fill took all it was offered, part of it, or nothing.
#[test]
fn a_record_filled_in_place_is_the_record_of_its_payload() {
    for (cap, n) in [(0, 0), (5, 5), (64, 64), (64, 13), (64, 0), (100, 99)] {
        let payload: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(7) | 1).collect();
        let got = frame::record_with(frame::REC_SHM_SYNC, 3, 40, cap, |buf| {
            buf[..n].copy_from_slice(&payload);
            // What a short read leaves past its end must not survive.
            buf[n..].fill(0xee);
            n
        });
        assert_eq!(
            got,
            frame::record(frame::REC_SHM_SYNC, 3, 40, &payload),
            "cap {cap}, wrote {n}"
        );
    }
}

/// The frame of many records is their concatenation after the descriptor
/// table, in order, however they are packed.
#[test]
fn a_packed_frame_carries_every_record_in_order() {
    let mut q = VecDeque::new();
    let mut want = Vec::new();
    for i in 0..40u32 {
        let r = frame::record(
            frame::REC_SHM_SYNC,
            i,
            i * 3,
            &vec![i as u8; (i as usize) * 37],
        );
        want.extend_from_slice(&r);
        q.push_back(frame::Unit {
            rec: r,
            descs: vec![],
        });
    }
    let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
    assert!(q.is_empty());
    assert_eq!(&f[frame::FRAME_HDR_LEN..], &want[..]);
    let d = frame::decode(&f).unwrap();
    assert_eq!(d.records().count(), 40);
}

#[test]
fn packing_stops_at_the_byte_and_descriptor_limits_and_says_more() {
    let mut q = VecDeque::new();
    for _ in 0..4 {
        q.push_back(frame::Unit {
            rec: frame::record(frame::REC_WAYLAND, 0, 1, &[0u8; 100]),
            descs: vec![DescOut::plain(Desc::new(frame::DESC_STREAM))],
        });
    }
    let (f, _) = frame::pack(&mut q, 10_000, 3, true);
    assert_eq!(frame::decode(&f).unwrap().descs.len(), 3);
    assert_eq!(frame::decode(&f).unwrap().flags, frame::FRAME_F_MORE);
    let (f, _) = frame::pack(&mut q, 16 + 24 + 16 + 104, 256, true);
    assert_eq!(frame::decode(&f).unwrap().descs.len(), 1);
    assert!(q.is_empty());
}

#[test]
fn a_corrupt_frame_is_refused() {
    let mut q = VecDeque::new();
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_HANGUP, 0, 0, &[]),
        descs: vec![],
    });
    let (f, _) = frame::pack(&mut q, 1 << 20, 256, false);
    let mut bad = f.clone();
    bad[0] ^= 1;
    assert_eq!(frame::decode(&bad).unwrap_err(), frame::FrameError::Magic);
    let mut bad = f.clone();
    bad.push(0);
    assert_eq!(frame::decode(&bad).unwrap_err(), frame::FrameError::Length);
    let mut bad = f.clone();
    bad[16 + 4] = 0xff; // record length past the end
    bad[16 + 5] = 0xff;
    assert_eq!(frame::decode(&bad).unwrap_err(), frame::FrameError::Record);
}

// ───────────────────────── policy ─────────────────────────

#[test]
fn offered_versions_are_the_minimum_of_host_xml_and_cap() {
    let p = Policy::default();
    assert_eq!(
        p.offer(1, b"wl_compositor", 6),
        Some((proto::WL_COMPOSITOR, 6))
    );
    assert_eq!(
        p.offer(1, b"wl_compositor", 99),
        Some((proto::WL_COMPOSITOR, iface(proto::WL_COMPOSITOR).version))
    );
    assert_eq!(
        p.offer(1, b"zwp_linux_dmabuf_v1", 6),
        Some((proto::ZWP_LINUX_DMABUF_V1, 5))
    );
    assert_eq!(p.offer(1, b"zwlr_data_control_manager_v1", 2), None);
    assert_eq!(p.offer(1, b"something_new_v1", 1), None);
}

#[test]
fn the_lease_device_needs_drm_files_and_a_passing_check() {
    let mut p = Policy::default();
    assert_eq!(p.offer(4, b"wp_drm_lease_device_v1", 1), None);
    p.drm_file = true;
    assert_eq!(p.offer(4, b"wp_drm_lease_device_v1", 1), None); // LeaseGate::Deny
    p.lease = LeaseGate::Check(Arc::new(|name| name == 4));
    assert!(p.offer(4, b"wp_drm_lease_device_v1", 1).is_some());
    assert!(p.offer(5, b"wp_drm_lease_device_v1", 1).is_none());
    p.drm_file = false;
    assert!(p.offer(4, b"wp_drm_lease_device_v1", 1).is_none());
    // Explicit sync: only with fences.
    assert!(p.offer(6, b"wp_linux_drm_syncobj_manager_v1", 1).is_none());
    p.fences = true;
    assert!(p.offer(6, b"wp_linux_drm_syncobj_manager_v1", 1).is_some());
}

// ───────────────────────── engine, end to end ─────────────────────────

/// Stands in for the guest kernel and the backend's handle table: a guest
/// dma-buf "resolves" to (owner 7, gem 42); the host "exports" a memfd; DRM
/// files pass through as-is.
#[derive(Default)]
struct TestPlat {
    exports: Vec<(u32, u32)>,
}

impl Platform for TestPlat {
    fn dmabuf_out(&mut self, _fd: OwnedFd) -> DescOut {
        DescOut::plain(Desc {
            a: 7,
            b: 42,
            ..Desc::new(frame::DESC_DMABUF)
        })
    }
    fn dmabuf_in(&mut self, d: &Desc, _fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        self.exports.push((d.a, d.b));
        sys::memfd(c"exported", 4096)
    }
    fn drm_file_out(&mut self, fd: OwnedFd) -> DescOut {
        DescOut {
            desc: Desc {
                b: 4,
                ..Desc::new(frame::DESC_DRM_FILE)
            },
            fd: Some(fd),
        }
    }
    fn drm_file_in(&mut self, _d: &Desc, fd: Option<OwnedFd>) -> std::io::Result<OwnedFd> {
        fd.ok_or_else(|| std::io::Error::other("no fd"))
    }
}

/// A guest engine (facing a client) and a host engine (facing a compositor)
/// joined by frames, as the channel would.
struct Pair {
    g: Engine,
    h: Engine,
    gp: TestPlat,
    hp: TestPlat,
    offset: Arc<AtomicI64>,
}

impl Pair {
    fn new(policy: Policy) -> Self {
        let offset = Arc::new(AtomicI64::new(0));
        let rewrites = Rewrites {
            devmap: vec![DevPair {
                host: (226, 129),
                guest: (226, 128),
            }],
            clock_offset: offset.clone(),
        };
        let mut g = Engine::new(EngineConfig {
            side: Side::Guest,
            local: Local::Client,
            policy: Policy {
                drm_file: true,
                lease: LeaseGate::Allow,
                fences: false,
            },
            rewrites: Some(rewrites),
            synth_released: true,
        });
        let h = Engine::new(EngineConfig {
            side: Side::Host,
            local: Local::Server,
            policy,
            rewrites: None,
            synth_released: false,
        });
        g.hello(frame::HELLO_G_DRM_FILE);
        let mut p = Self {
            g,
            h,
            gp: TestPlat::default(),
            hp: TestPlat::default(),
            offset,
        };
        p.pump();
        p
    }

    /// Move everything queued on either side across, until quiet; and close
    /// the streams either side ended, as its event loop would once it had
    /// stopped watching them.
    fn pump(&mut self) {
        drop(self.g.take_closed_streams());
        drop(self.h.take_closed_streams());
        loop {
            let mut moved = false;
            let mut q = self.g.take_units();
            while !q.is_empty() {
                let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
                self.h.from_channel(&f, fds, &mut self.hp).unwrap();
                moved = true;
            }
            let mut q = self.h.take_units();
            while !q.is_empty() {
                let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
                self.g.from_channel(&f, fds, &mut self.gp).unwrap();
                moved = true;
            }
            if !moved {
                break;
            }
        }
    }

    /// The client's messages, taken as the channel drains (the engine stops
    /// taking input with a channel's worth queued).
    fn client_sends(&mut self, msgs: &[Vec<u8>], fds: Vec<OwnedFd>) -> Result<(), Fatal> {
        let mut input = LocalIn::new(msgs.concat(), fds);
        loop {
            let before = input.len();
            self.g.from_local(&mut input, &mut self.gp)?;
            self.pump();
            if input.is_empty() || input.len() == before {
                return Ok(());
            }
        }
    }

    fn server_sends(&mut self, msgs: &[Vec<u8>], fds: Vec<OwnedFd>) -> Result<(), Fatal> {
        let r = self
            .h
            .from_local(&mut LocalIn::new(msgs.concat(), fds), &mut self.hp);
        if r.is_ok() {
            self.pump();
        }
        r
    }

    /// What reached the compositor / the client: (messages, descriptors).
    fn at_server(&mut self) -> (Vec<u8>, Vec<OwnedFd>) {
        flatten(self.h.local_out().drain())
    }
    fn at_client(&mut self) -> (Vec<u8>, Vec<OwnedFd>) {
        flatten(self.g.local_out().drain())
    }

    /// get_registry(2), then the compositor announces `globals`.
    fn registry(&mut self, globals: &[(u32, &str, u32)]) -> Vec<u8> {
        self.client_sends(
            &[MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
                .new_id(2)
                .finish()],
            vec![],
        )
        .unwrap();
        let ev: Vec<Vec<u8>> = globals
            .iter()
            .map(|(n, i, v)| {
                MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
                    .uint(*n)
                    .string(Some(i))
                    .uint(*v)
                    .finish()
            })
            .collect();
        self.server_sends(&ev, vec![]).unwrap();
        self.at_client().0
    }

    fn bind(&mut self, name: u32, ifname: &str, version: u32, id: u32) -> Result<(), Fatal> {
        self.client_sends(
            &[MsgBuilder::new(2, op::wl_registry::REQ_BIND)
                .uint(name)
                .generic_new_id(ifname, version, id)
                .finish()],
            vec![],
        )
    }
}

fn flatten(v: Vec<(Vec<u8>, Vec<OwnedFd>)>) -> (Vec<u8>, Vec<OwnedFd>) {
    let mut b = Vec::new();
    let mut f = Vec::new();
    for (d, fd) in v {
        b.extend(d);
        f.extend(fd);
    }
    (b, f)
}

fn split(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(h) = wire::peek_header(b) {
        out.push(b[..h.size as usize].to_vec());
        b = &b[h.size as usize..];
    }
    out
}

fn memfd_with(data: &[u8]) -> OwnedFd {
    let fd = sys::memfd(c"t", data.len() as u64).unwrap();
    sys::pwrite_full(fd.as_raw_fd(), data, 0).unwrap();
    fd
}

fn read_all(fd: &OwnedFd) -> Vec<u8> {
    let n = sys::file_size(fd.as_raw_fd()).unwrap();
    let mut v = vec![0u8; n as usize];
    sys::pread_full(fd.as_raw_fd(), &mut v, 0).unwrap();
    v
}

#[test]
fn the_registry_hides_what_is_not_allowed_and_clamps_the_rest() {
    let mut p = Pair::new(Policy::default());
    let out = p.registry(&[
        (1, "wl_compositor", 6),
        (2, "zwlr_data_control_manager_v1", 2),
        (3, "zwp_linux_dmabuf_v1", 6),
        (4, "wp_security_context_manager_v1", 1),
    ]);
    let msgs = split(&out);
    assert_eq!(msgs.len(), 2);
    let d = msg_desc(proto::WL_REGISTRY, Dir::Event, op::wl_registry::EVT_GLOBAL);
    let a = wire::parse(d, &msgs[1]).unwrap();
    assert_eq!(a[1].val, Val::Str(Some(b"zwp_linux_dmabuf_v1")));
    assert_eq!(a[2].val, Val::Uint(5));
    // A hidden global's removal is hidden too.
    p.server_sends(
        &[MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL_REMOVE)
            .uint(2)
            .finish()],
        vec![],
    )
    .unwrap();
    assert!(p.at_client().0.is_empty());
    assert_eq!(p.h.stats.globals_hidden, 2);
}

#[test]
fn binding_a_hidden_global_or_above_the_offer_is_fatal_on_the_host_too() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[
        (1, "wl_compositor", 6),
        (2, "zwlr_data_control_manager_v1", 2),
    ]);
    // The guest daemon refuses first.
    let e = p.bind(2, "zwlr_data_control_manager_v1", 1, 3).unwrap_err();
    assert_eq!(e.blame, Blame::Local);
    let e = p.bind(1, "wl_compositor", 7, 3).unwrap_err();
    assert_eq!(e.code, ERR_INVALID_OBJECT);
    // A guest that skips its daemon's check meets the host's.
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: Local::Server,
        policy: Policy::default(),
        rewrites: None,
        synth_released: false,
    });
    let mut q = VecDeque::new();
    let msgs = [
        MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(2)
            .finish(),
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(2)
            .generic_new_id("zwlr_data_control_manager_v1", 1, 3)
            .finish(),
    ]
    .concat();
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 0, &msgs),
        descs: vec![],
    });
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let e = h
        .from_channel(&f, fds, &mut TestPlat::default())
        .unwrap_err();
    assert_eq!(e.blame, Blame::Channel);
    assert!(e.message.contains("not offered"), "{}", e.message);
}

#[test]
fn unknown_objects_opcodes_and_versions_are_fatal() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_compositor", 6)]);
    p.bind(1, "wl_compositor", 3, 3).unwrap();
    let e = p
        .client_sends(&[MsgBuilder::new(99, 0).finish()], vec![])
        .unwrap_err();
    assert!(e.message.contains("invalid object 99"));
    let e = p
        .client_sends(&[MsgBuilder::new(3, 9).finish()], vec![])
        .unwrap_err();
    assert_eq!(e.code, ERR_INVALID_METHOD);
    // wl_surface.damage_buffer is since 4; a v3 compositor's surface refuses it.
    p.client_sends(
        &[MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
            .new_id(4)
            .finish()],
        vec![],
    )
    .unwrap();
    let e = p
        .client_sends(
            &[MsgBuilder::new(4, op::wl_surface::REQ_DAMAGE_BUFFER)
                .int(0)
                .int(0)
                .int(1)
                .int(1)
                .finish()],
            vec![],
        )
        .unwrap_err();
    assert!(e.message.contains("needs version 4"), "{}", e.message);
}

/// What an engine says it holds in descriptors is what it holds: the guest
/// daemon shares its descriptor limit among clients by this count.
#[test]
fn an_engine_counts_the_descriptors_it_holds() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(2, "wl_shm", 2), (3, "wl_data_device_manager", 3)]);
    p.bind(2, "wl_shm", 2, 4).unwrap();
    assert_eq!((p.g.held_fds(), p.h.held_fds()), (0, 0));
    p.client_sends(
        &[MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(5)
            .int(4096)
            .finish()],
        vec![sys::memfd(c"p", 4096).unwrap()],
    )
    .unwrap();
    // The client's pool here; the memfd there, and the copy of it waiting
    // for the compositor.
    assert_eq!((p.g.held_fds(), p.h.held_fds()), (1, 2));
    p.at_server();
    assert_eq!(p.h.held_fds(), 1);
    p.client_sends(
        &[MsgBuilder::new(5, op::wl_shm_pool::REQ_DESTROY).finish()],
        vec![],
    )
    .unwrap();
    assert_eq!((p.g.held_fds(), p.h.held_fds()), (0, 0));
}

/// Errors go where libwayland-server posts them: the generic ones on the
/// display (object 1), a bad bind on the registry, and an interface's own
/// on its object. Before, every error named the object the message did,
/// so a client told "invalid object 99" could not dispatch the error at
/// all (it knows no 99), and `no_memory` on `wl_shm` read as
/// `wl_shm.error.invalid_fd`.
#[test]
fn errors_are_posted_on_the_object_libwayland_posts_them_on() {
    let setup = || {
        let mut p = Pair::new(Policy::default());
        p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
        p.bind(1, "wl_compositor", 6, 3).unwrap();
        p.bind(2, "wl_shm", 2, 4).unwrap();
        p
    };
    let on = |e: Fatal| (e.object, e.code);
    let mut p = setup();
    let e = p.client_sends(&[MsgBuilder::new(99, 0).finish()], vec![]);
    assert_eq!(on(e.unwrap_err()), (1, ERR_INVALID_OBJECT));
    let mut p = setup();
    let e = p.client_sends(&[MsgBuilder::new(3, 9).finish()], vec![]);
    assert_eq!(on(e.unwrap_err()), (1, ERR_INVALID_METHOD));
    let mut p = setup();
    assert_eq!(
        on(p.bind(7, "wl_seat", 1, 5).unwrap_err()),
        (2, ERR_INVALID_OBJECT)
    );
    // A pool that is not memory: wl_shm's own invalid_fd, on wl_shm.
    let mut p = setup();
    let (_r, w) = sys::pipe().unwrap();
    let e = p.client_sends(
        &[MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(5)
            .int(4096)
            .finish()],
        vec![w],
    );
    assert_eq!(on(e.unwrap_err()), (4, ERR_SHM_INVALID_FD));
    // A buffer past the connection's budget: no_memory, on the display.
    let mut p = setup();
    let size = i32::MAX & !4095;
    let e = p.client_sends(
        &[
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(5)
                .int(size)
                .finish(),
            MsgBuilder::new(5, op::wl_shm_pool::REQ_CREATE_BUFFER)
                .new_id(6)
                .int(0)
                .int(4096)
                .int(size / 16384)
                .int(16384)
                .uint(0)
                .finish(),
        ],
        vec![sys::memfd(c"big", size as u64).unwrap()],
    );
    assert_eq!(on(e.unwrap_err()), (1, ERR_NO_MEMORY));
    // And what a client is told is what the error says.
    let f = Fatal::new(Blame::Local, 1, ERR_INVALID_OBJECT, "x");
    let m = f.display_error();
    assert_eq!(wire::peek_header(&m).unwrap().object, 1);
    assert_eq!(u32::from_ne_bytes(m[8..12].try_into().unwrap()), 1);
}

#[test]
fn a_request_on_a_destroyed_object_is_fatal_but_late_events_still_parse() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_data_device_manager", 3), (2, "wl_seat", 9)]);
    p.bind(1, "wl_data_device_manager", 3, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wl_data_device_manager::REQ_CREATE_DATA_SOURCE)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    p.client_sends(
        &[MsgBuilder::new(4, op::wl_data_source::REQ_DESTROY).finish()],
        vec![],
    )
    .unwrap();
    // The compositor had already asked the source for data: a descriptor-
    // carrying event on a zombie still has to be counted.
    let (r, w) = sys::pipe().unwrap();
    drop(r);
    p.server_sends(
        &[MsgBuilder::new(4, op::wl_data_source::EVT_SEND)
            .string(Some("text/plain"))
            .finish()],
        vec![w],
    )
    .unwrap();
    let (msgs, fds) = p.at_client();
    assert_eq!(split(&msgs).len(), 1);
    assert_eq!(fds.len(), 1);
    let e = p.client_sends(
        &[MsgBuilder::new(4, op::wl_data_source::REQ_OFFER)
            .string(Some("x"))
            .finish()],
        vec![],
    );
    assert!(e.unwrap_err().message.contains("destroyed"));
}

#[test]
fn shm_contents_reach_the_host_memfd_at_commit_and_only_the_damage_after() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
    p.bind(1, "wl_compositor", 6, 3).unwrap();
    p.bind(2, "wl_shm", 2, 4).unwrap();
    let (w, h, stride) = (16i32, 8i32, 64i32);
    let size = (stride * h) as usize;
    let mut pixels: Vec<u8> = (0..size).map(|i| i as u8).collect();
    let client_pool = memfd_with(&pixels);
    let client_raw = client_pool.try_clone().unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(5)
                .int(size as i32)
                .finish(),
            MsgBuilder::new(5, op::wl_shm_pool::REQ_CREATE_BUFFER)
                .new_id(6)
                .int(0)
                .int(w)
                .int(h)
                .int(stride)
                .uint(0)
                .finish(),
            MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(7)
                .finish(),
        ],
        vec![client_pool],
    )
    .unwrap();
    let (_, mut fds) = p.at_server();
    assert_eq!(fds.len(), 1, "the compositor gets one pool descriptor");
    let host_pool = fds.remove(0);
    assert_eq!(sys::file_size(host_pool.as_raw_fd()).unwrap(), size as u64);
    assert!(
        read_all(&host_pool).iter().all(|&b| b == 0),
        "nothing is copied before a commit"
    );

    p.client_sends(
        &[
            MsgBuilder::new(7, op::wl_surface::REQ_ATTACH)
                .object(6)
                .int(0)
                .int(0)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish(),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(
        read_all(&host_pool),
        pixels,
        "a first commit copies the whole buffer"
    );
    let (_, bytes0) = p.g.shm_stats();

    // Change rows 2..4 only, and say so.
    for b in &mut pixels[2 * stride as usize..4 * stride as usize] {
        *b = 0xee;
    }
    sys::pwrite_full(client_raw.as_raw_fd(), &pixels, 0).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(7, op::wl_surface::REQ_ATTACH)
                .object(6)
                .int(0)
                .int(0)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_DAMAGE_BUFFER)
                .int(0)
                .int(2)
                .int(w)
                .int(2)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish(),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(read_all(&host_pool), pixels);
    let (_, bytes1) = p.g.shm_stats();
    assert_eq!(
        bytes1 - bytes0,
        2 * stride as u64,
        "only the damaged rows are copied"
    );
    let (msgs, _) = p.at_server();
    assert_eq!(
        split(&msgs).len(),
        5,
        "the commits themselves still reach the compositor"
    );
}

/// An engine facing a client, on `side`, with wl_seat v1 bound as 3 from a
/// compositor that offers v9.
fn seat_v1(side: Side) -> Engine {
    let mut e = Engine::new(EngineConfig {
        side,
        local: Local::Client,
        policy: Policy::default(),
        rewrites: None,
        synth_released: false,
    });
    let mut plat = TestPlat::default();
    let m = MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
        .new_id(2)
        .finish();
    e.from_local(&mut LocalIn::new(m, []), &mut plat).unwrap();
    let g = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("wl_seat"))
        .uint(9)
        .finish();
    e.from_channel(&wayland_frame(&[g]), vec![], &mut plat)
        .unwrap();
    let m = MsgBuilder::new(2, op::wl_registry::REQ_BIND)
        .uint(1)
        .generic_new_id("wl_seat", 1, 3)
        .finish();
    e.from_local(&mut LocalIn::new(m, []), &mut plat).unwrap();
    e
}

/// An event newer than its object's version is passed on from the host's
/// compositor, as libwayland-client, which checks no version for events,
/// would take it natively; before, the guest client was killed for the
/// compositor's mistake. From a guest's compositor to a host client (export
/// mode) it is still refused: a host process would call past the end of a
/// listener made for the object's version.
#[test]
fn an_event_newer_than_its_object_passes_only_from_the_hosts_compositor() {
    let name = MsgBuilder::new(3, op::wl_seat::EVT_NAME)
        .string(Some("seat0"))
        .finish();
    let mut g = seat_v1(Side::Guest);
    g.from_channel(
        &wayland_frame(std::slice::from_ref(&name)),
        vec![],
        &mut TestPlat::default(),
    )
    .unwrap();
    let (msgs, _) = flatten(g.local_out().drain());
    assert!(msgs.ends_with(&name));
    let mut h = seat_v1(Side::Host);
    let e = h
        .from_channel(
            &wayland_frame(std::slice::from_ref(&name)),
            vec![],
            &mut TestPlat::default(),
        )
        .unwrap_err();
    assert!(e.message.contains("needs version 2"), "{}", e.message);
    // A request newer than its object is refused either way.
    let m = MsgBuilder::new(3, op::wl_seat::REQ_RELEASE).finish();
    let e = g
        .from_local(&mut LocalIn::new(m, []), &mut TestPlat::default())
        .unwrap_err();
    assert!(e.message.contains("needs version 5"), "{}", e.message);
}

/// A buffer destroyed while its surface still shows it keeps its pages in
/// the compositor's pool until the surface commits something else, as
/// natively (destroying a wl_buffer leaves the pool alone): a compositor
/// that reads shm when it paints goes on reading them. Before, they were
/// punched at the destroy, and it painted zeros.
#[test]
fn a_destroyed_buffer_keeps_its_pages_while_its_surface_shows_it() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
    p.bind(1, "wl_compositor", 6, 3).unwrap();
    p.bind(2, "wl_shm", 2, 4).unwrap();
    let page = pg() as i32;
    let pixels: Vec<u8> = (0..2 * page as usize)
        .map(|i| (i % 251) as u8 + 1)
        .collect();
    let buffer = |id: u32, offset: i32| {
        MsgBuilder::new(5, op::wl_shm_pool::REQ_CREATE_BUFFER)
            .new_id(id)
            .int(offset)
            .int(page / 4)
            .int(1)
            .int(page)
            .uint(0)
            .finish()
    };
    let show = |id: u32| {
        vec![
            MsgBuilder::new(7, op::wl_surface::REQ_ATTACH)
                .object(id)
                .int(0)
                .int(0)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish(),
        ]
    };
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(5)
                .int(2 * page)
                .finish(),
            buffer(6, 0),
            MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(7)
                .finish(),
        ],
        vec![memfd_with(&pixels)],
    )
    .unwrap();
    let host = host_pool(&mut p);
    p.client_sends(&show(6), vec![]).unwrap();
    let first = &pixels[..page as usize];
    assert_eq!(&read_all(&host)[..page as usize], first);
    // Destroyed while shown: still there.
    p.client_sends(
        &[MsgBuilder::new(6, op::wl_buffer::REQ_DESTROY).finish()],
        vec![],
    )
    .unwrap();
    assert_eq!(
        &read_all(&host)[..page as usize],
        first,
        "punched while shown"
    );
    // A commit without an attach shows the same buffer still.
    p.client_sends(
        &[MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish()],
        vec![],
    )
    .unwrap();
    assert_eq!(&read_all(&host)[..page as usize], first);
    // Another buffer shown: the first's pages go, and their charge.
    p.client_sends(&[buffer(8, page)], vec![]).unwrap();
    p.client_sends(&show(8), vec![]).unwrap();
    assert!(read_all(&host)[..page as usize].iter().all(|&b| b == 0));
    assert_eq!(allocated(&host), pg());
    // And the surface going takes a shown, destroyed buffer with it.
    p.client_sends(
        &[
            MsgBuilder::new(8, op::wl_buffer::REQ_DESTROY).finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_DESTROY).finish(),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(allocated(&host), 0);
}

/// A client with wl_compositor (3), wl_shm (4) and a surface (5), whose
/// 8 MiB buffer 7 in pool 6 it then truncates to `left` bytes behind the
/// proxy's back before committing it; the commit, and a `wl_display.sync`
/// after it, as they are taken.
fn commit_a_truncated_pool(left: u64) -> Pair {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
    p.bind(1, "wl_compositor", 6, 3).unwrap();
    p.bind(2, "wl_shm", 2, 4).unwrap();
    let (stride, height) = (4096i32, 2048i32);
    let pool = sys::memfd(c"pool", (stride * height) as u64).unwrap();
    let keep = pool.try_clone().unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(5)
                .finish(),
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(6)
                .int(stride * height)
                .finish(),
            MsgBuilder::new(6, op::wl_shm_pool::REQ_CREATE_BUFFER)
                .new_id(7)
                .int(0)
                .int(stride / 4)
                .int(height)
                .int(stride)
                .uint(0)
                .finish(),
        ],
        vec![pool],
    )
    .unwrap();
    p.at_server();
    sys::ftruncate(keep.as_raw_fd(), left).unwrap();
    let data = [
        MsgBuilder::new(5, op::wl_surface::REQ_ATTACH)
            .object(7)
            .int(0)
            .int(0)
            .finish(),
        MsgBuilder::new(5, op::wl_surface::REQ_COMMIT).finish(),
        MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(9)
            .finish(),
    ]
    .concat();
    let mut input = LocalIn::new(data, []);
    for _ in 0..8 {
        p.g.from_local(&mut input, &mut p.gp).unwrap();
        assert_eq!(p.g.channel_backlog(), p.g.channel_backlog_recount());
        p.pump();
        assert_eq!(p.g.channel_backlog(), p.g.channel_backlog_recount());
    }
    assert!(
        input.is_empty(),
        "{} bytes of input left untaken: the connection is wedged",
        input.len()
    );
    p
}

/// A client that shrinks its own pool before a commit's copy is read gets a
/// short copy, and nothing else happens to it: what the copy was counted at
/// on the channel's backlog is given back however short the read came, so
/// the backlog drains and its later requests are taken. Before, only the
/// bytes read were given back, and the rest of the buffer stayed counted for
/// good: past the input limit, the client's input was never read again.
#[test]
fn a_client_that_truncates_its_pool_gets_a_short_copy_and_is_not_wedged() {
    for left in [0, 4096, 5 << 20] {
        let mut p = commit_a_truncated_pool(left);
        assert_eq!(p.g.channel_backlog(), 0, "left {left}");
        assert!(!p.g.input_blocked());
        let (msgs, _) = p.at_server();
        let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
            .new_id(9)
            .finish();
        assert!(
            msgs.ends_with(&sync),
            "the sync after the commit reached the compositor (left {left})"
        );
        let (_, synced) = p.g.shm_stats();
        assert_eq!(synced, left, "the copy is as long as the pool is");
    }
}

#[test]
fn a_pool_resize_grows_the_host_memfd_before_the_compositor_sees_it() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(2, "wl_shm", 2)]);
    p.bind(2, "wl_shm", 2, 4).unwrap();
    p.client_sends(
        &[MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(5)
            .int(4096)
            .finish()],
        vec![memfd_with(&[0u8; 4096])],
    )
    .unwrap();
    let (_, mut fds) = p.at_server();
    let host_pool = fds.remove(0);
    p.client_sends(
        &[MsgBuilder::new(5, op::wl_shm_pool::REQ_RESIZE)
            .int(65536)
            .finish()],
        vec![],
    )
    .unwrap();
    assert_eq!(sys::file_size(host_pool.as_raw_fd()).unwrap(), 65536);
}

#[test]
fn a_guest_cannot_make_the_host_hold_unbounded_shm() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(2, "wl_shm", 2)]);
    p.bind(2, "wl_shm", 2, 4).unwrap();
    // Straight to the host, as a guest skipping its daemon would. Pools as
    // large as the protocol allows are sparse and cost only their count...
    for i in 0..4u32 {
        create_pool(&mut p, 10 + i, i32::MAX).unwrap();
    }
    // ...but what buffers cover is capped per connection: two 512 MiB
    // buffers in different pools are one more than it allows.
    let buffer = |pool: u32, id: u32| {
        MsgBuilder::new(pool, op::wl_shm_pool::REQ_CREATE_BUFFER)
            .new_id(id)
            .int(0)
            .int(16384)
            .int(8192)
            .int(65536)
            .uint(0)
            .finish()
    };
    raw_to_host(&mut p, buffer(10, 20), None).unwrap();
    let e = raw_to_host(&mut p, buffer(11, 21), None).unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(e.blame, Blame::Channel);
}

/// One message straight to the host engine, as a guest kernel skipping its
/// daemon could send it; `pool` is the size of an SHM_POOL descriptor riding
/// along.
fn raw_to_host(p: &mut Pair, m: Vec<u8>, pool: Option<u64>) -> Result<(), Fatal> {
    let descs: Vec<DescOut> = pool
        .map(|c| {
            DescOut::plain(Desc {
                c,
                ..Desc::new(frame::DESC_SHM_POOL)
            })
        })
        .into_iter()
        .collect();
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, descs.len() as u32, &m),
        descs,
    }]);
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    p.h.from_channel(&f, fds, &mut TestPlat::default())
}

/// An SHM_SYNC record straight to the host engine: `bytes` at `off` in
/// buffer `buffer`.
fn sync_to_host(p: &mut Pair, buffer: u32, off: u32, bytes: &[u8]) -> Result<(), Fatal> {
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(frame::REC_SHM_SYNC, buffer, off, bytes),
        descs: vec![],
    }]);
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    p.h.from_channel(&f, fds, &mut TestPlat::default())
}

fn create_pool(p: &mut Pair, id: u32, size: i32) -> Result<(), Fatal> {
    let m = MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
        .new_id(id)
        .int(size)
        .finish();
    raw_to_host(p, m, Some(size as u64))
}

/// `wl_shm_pool.create_buffer` straight to the host: `len` bytes at `offset`
/// (one row of them).
fn create_buffer(p: &mut Pair, pool: u32, id: u32, offset: i32, len: i32) -> Result<(), Fatal> {
    let m = MsgBuilder::new(pool, op::wl_shm_pool::REQ_CREATE_BUFFER)
        .new_id(id)
        .int(offset)
        .int(len / 4)
        .int(1)
        .int(len)
        .uint(0)
        .finish();
    raw_to_host(p, m, None)
}

fn destroy(p: &mut Pair, id: u32, opcode: u16) {
    raw_to_host(p, MsgBuilder::new(id, opcode).finish(), None).unwrap();
}

/// A connection with wl_shm bound as 4, whose host engine draws on `vm`.
fn shm_pair(vm: &Arc<crate::shm::ShmBudget>) -> Pair {
    let mut p = Pair::new(Policy::default());
    p.h.set_shm_budget(vm.clone());
    p.registry(&[(2, "wl_shm", 2)]);
    p.bind(2, "wl_shm", 2, 4).unwrap();
    p
}

/// The host memfd the compositor was handed for the one pool just made.
fn host_pool(p: &mut Pair) -> OwnedFd {
    let (_, mut fds) = p.at_server();
    assert_eq!(fds.len(), 1);
    fds.remove(0)
}

/// Bytes of `fd` that hold memory.
fn allocated(fd: &OwnedFd) -> u64 {
    sys::fstat(fd.as_raw_fd()).unwrap().st_blocks as u64 * 512
}

/// The first byte at or after `off` that is data, not a hole.
fn next_data(fd: &OwnedFd, off: u64) -> Option<u64> {
    crate::sys::seek_data(fd.as_raw_fd(), off).ok()
}

fn pg() -> u64 {
    sys::page_size()
}

#[test]
fn every_connection_of_a_vm_draws_on_one_shm_budget() {
    let vm = Arc::new(crate::shm::ShmBudget::new(3 << 20, 64));
    let mut a = shm_pair(&vm);
    let mut b = shm_pair(&vm);
    create_pool(&mut a, 10, 2 << 20).unwrap();
    create_buffer(&mut a, 10, 11, 0, 2 << 20).unwrap();
    assert_eq!(vm.used(), (2 << 20, 1));
    // Well inside b's own connection limit, but past what the VM has left:
    // refused, and nothing taken for it but its pool's place in the count.
    create_pool(&mut b, 10, 2 << 20).unwrap();
    let e = create_buffer(&mut b, 10, 11, 0, 2 << 20).unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(vm.used(), (2 << 20, 2));
    drop(b);
    // a's buffer goes, and with it its charge; another connection can have
    // the room.
    destroy(&mut a, 11, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used(), (0, 1));
    let mut b = shm_pair(&vm);
    create_pool(&mut b, 10, 2 << 20).unwrap();
    create_buffer(&mut b, 10, 11, 0, 2 << 20).unwrap();
    assert_eq!(vm.used(), (2 << 20, 2));
    drop(b);
    drop(a);
    assert_eq!(vm.used(), (0, 0), "a dropped engine gives everything back");
}

#[test]
fn the_vm_budget_stops_many_connections_each_within_its_own() {
    let vm = Arc::new(crate::shm::ShmBudget::new(8 << 20, 1024));
    let mut conns = Vec::new();
    let mut refused = None;
    for i in 0..16 {
        let mut p = shm_pair(&vm);
        create_pool(&mut p, 10, 4 << 20).unwrap();
        if let Err(e) = create_buffer(&mut p, 10, 11, 0, 3 << 20) {
            assert_eq!(e.code, ERR_NO_MEMORY);
            refused = Some(i);
            break;
        }
        conns.push(p);
    }
    assert_eq!(
        refused,
        Some(2),
        "two 3 MiB buffers fit 8 MiB, a third does not"
    );
    assert_eq!(vm.used().0, 6 << 20);
    conns.clear();
    assert_eq!(vm.used().0, 0);
}

#[test]
fn a_pool_stays_counted_while_a_buffer_made_from_it_lives() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 64));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 1 << 20).unwrap();
    raw_to_host(
        &mut p,
        MsgBuilder::new(10, op::wl_shm_pool::REQ_CREATE_BUFFER)
            .new_id(11)
            .int(0)
            .int(256)
            .int(256)
            .int(1024)
            .uint(0)
            .finish(),
        None,
    )
    .unwrap();
    assert_eq!(
        vm.used(),
        (256 << 10, 1),
        "the buffer is charged, not the pool"
    );
    // The usual order: the pool is destroyed while its buffers live on, and
    // the memfd with them.
    destroy(&mut p, 10, op::wl_shm_pool::REQ_DESTROY);
    assert_eq!(vm.used(), (256 << 10, 1));
    destroy(&mut p, 11, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used(), (0, 0));
}

#[test]
fn a_pool_resize_is_free_and_a_buffer_past_the_budget_takes_nothing() {
    let vm = Arc::new(crate::shm::ShmBudget::new(4 << 20, 64));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 1 << 20).unwrap();
    let memfd = host_pool(&mut p);
    raw_to_host(
        &mut p,
        MsgBuilder::new(10, op::wl_shm_pool::REQ_RESIZE)
            .int(64 << 20)
            .finish(),
        None,
    )
    .unwrap();
    assert_eq!(sys::file_size(memfd.as_raw_fd()).unwrap(), 64 << 20);
    assert_eq!(vm.used(), (0, 1));
    assert_eq!(allocated(&memfd), 0, "a grown pool is a hole");
    create_buffer(&mut p, 10, 11, 0, 3 << 20).unwrap();
    assert_eq!(vm.used(), (3 << 20, 1));
    let e = create_buffer(&mut p, 10, 12, 3 << 20, 2 << 20).unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(vm.used(), (3 << 20, 1));
}

#[test]
fn pools_are_counted_whatever_their_size() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 2));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 4096).unwrap();
    create_pool(&mut p, 11, 4096).unwrap();
    let e = create_pool(&mut p, 12, 4096).unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(vm.used(), (0, 2));
}

#[test]
fn overlapping_buffers_are_charged_once() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 64));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 1 << 20).unwrap();
    let pg = pg();
    // Two buffers over the same bytes, as a client reusing one region
    // under two ids does, and a third over half of them and half beyond,
    // starting part-way into a page.
    create_buffer(&mut p, 10, 11, 0, (4 * pg) as i32).unwrap();
    create_buffer(&mut p, 10, 12, 0, (4 * pg) as i32).unwrap();
    assert_eq!(vm.used().0, 4 * pg);
    create_buffer(&mut p, 10, 13, (2 * pg + 100) as i32, (4 * pg) as i32).unwrap();
    // Pages 0..7: the third touches pages 2..=6.
    assert_eq!(vm.used().0, 7 * pg);
    destroy(&mut p, 11, op::wl_buffer::REQ_DESTROY);
    assert_eq!(
        vm.used().0,
        7 * pg,
        "the second still covers what the first did"
    );
    destroy(&mut p, 12, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used().0, 5 * pg, "pages 0 and 1 are only the third's");
    destroy(&mut p, 13, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used(), (0, 1));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri files are not sparse: st_blocks after a punched hole"
)]
fn the_last_buffer_over_a_page_punches_it_out_and_gives_its_charge_back() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 64));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 1 << 20).unwrap();
    let memfd = host_pool(&mut p);
    let pg = pg();
    // a: pages 0..2 and a little of 2; b: the rest of page 2, and 3.
    let a_len = 2 * pg + 16;
    create_buffer(&mut p, 10, 11, 0, a_len as i32).unwrap();
    create_buffer(&mut p, 10, 12, a_len as i32, (4 * pg - a_len) as i32).unwrap();
    assert_eq!(vm.used().0, 4 * pg, "page 2, which both touch, once");
    sync_to_host(&mut p, 11, 0, &vec![0xaa; a_len as usize]).unwrap();
    sync_to_host(&mut p, 12, 0, &vec![0xbb; (4 * pg - a_len) as usize]).unwrap();
    assert_eq!(allocated(&memfd), 4 * pg);
    assert_eq!(next_data(&memfd, 0), Some(0));

    destroy(&mut p, 11, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used().0, 2 * pg);
    assert_eq!(allocated(&memfd), 2 * pg, "pages 0 and 1 are punched out");
    assert_eq!(next_data(&memfd, 0), Some(2 * pg));
    let mut back = vec![0u8; (4 * pg) as usize];
    sys::pread_full(memfd.as_raw_fd(), &mut back, 0).unwrap();
    assert!(back[..(2 * pg) as usize].iter().all(|&x| x == 0));
    assert!(
        back[a_len as usize..].iter().all(|&x| x == 0xbb),
        "b's bytes in the shared page are kept"
    );

    destroy(&mut p, 12, op::wl_buffer::REQ_DESTROY);
    assert_eq!(vm.used(), (0, 1));
    assert_eq!(allocated(&memfd), 0);
    assert_eq!(next_data(&memfd, 0), None);
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri files are not sparse: st_blocks after a punched hole"
)]
fn shm_sync_outside_every_live_buffer_is_refused() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 64));
    let fresh = || {
        let mut p = shm_pair(&vm);
        create_pool(&mut p, 10, 1 << 20).unwrap();
        let memfd = host_pool(&mut p);
        create_buffer(&mut p, 10, 11, 4096, 8192).unwrap();
        (p, memfd)
    };
    // Past the end of its buffer, though inside the pool.
    let (mut p, memfd) = fresh();
    let e = sync_to_host(&mut p, 11, 8000, &[1u8; 200]).unwrap_err();
    assert_eq!(e.blame, Blame::Channel);
    assert_eq!(allocated(&memfd), 0);
    // A buffer that is gone.
    let (mut p, memfd) = fresh();
    destroy(&mut p, 11, op::wl_buffer::REQ_DESTROY);
    assert!(sync_to_host(&mut p, 11, 0, &[1u8; 16]).is_err());
    assert_eq!(allocated(&memfd), 0);
    // A buffer that does not fit its pool is never tracked, nor charged.
    let (mut p, memfd) = fresh();
    create_buffer(&mut p, 10, 12, (1 << 20) - 4096, 8192).unwrap();
    assert!(sync_to_host(&mut p, 12, 0, &[1u8; 16]).is_err());
    assert_eq!(allocated(&memfd), 0);
    // Nor a pool, or an id nothing has.
    let (mut p, _) = fresh();
    assert!(sync_to_host(&mut p, 10, 0, &[1u8; 16]).is_err());
    let (mut p, _) = fresh();
    assert!(sync_to_host(&mut p, 99, 0, &[1u8; 16]).is_err());
}

/// foot, through both halves: a 512 MiB pool (its default
/// `max-shm-pool-size-mb`) that it scrolls by moving its one buffer forward
/// through the pool, wrapping round at the end, destroying the old
/// `wl_buffer` and making a new one each time. The pool is larger than the
/// VM's budget, and the buffer travels further than the budget too.
#[test]
#[cfg_attr(
    miri,
    ignore = "Miri files are not sparse: st_blocks after a punched hole"
)]
fn foot_scrolls_through_a_pool_larger_than_the_budget() {
    let budget: u64 = 8 << 20;
    let vm = Arc::new(crate::shm::ShmBudget::new(budget, 64));
    let mut p = Pair::new(Policy::default());
    p.h.set_shm_budget(vm.clone());
    p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
    p.bind(1, "wl_compositor", 6, 3).unwrap();
    p.bind(2, "wl_shm", 2, 4).unwrap();

    let pool_size: u64 = 512 << 20;
    let (w, h) = (640u64, 400u64);
    let stride = w * 4;
    let size = stride * h;
    let client_pool = sys::memfd(c"foot-wayland-shm-buffer-pool", pool_size).unwrap();
    let client_raw = client_pool.try_clone().unwrap();
    // foot starts a quarter of the way in; start near the end instead, so
    // the scroll wraps round.
    let mut offset = pool_size - 24 * size;
    let buffer = |id: u32, offset: u64| {
        MsgBuilder::new(5, op::wl_shm_pool::REQ_CREATE_BUFFER)
            .new_id(id)
            .int(offset as i32)
            .int(w as i32)
            .int(h as i32)
            .int(stride as i32)
            .uint(0)
            .finish()
    };
    let show = |id: u32| {
        [
            MsgBuilder::new(7, op::wl_surface::REQ_ATTACH)
                .object(id)
                .int(0)
                .int(0)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_DAMAGE_BUFFER)
                .int(0)
                .int(0)
                .int(w as i32)
                .int(h as i32)
                .finish(),
            MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish(),
        ]
    };
    let mut msgs = vec![
        MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(5)
            .int(pool_size as i32)
            .finish(),
        MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
            .new_id(7)
            .finish(),
        buffer(100, offset),
    ];
    msgs.extend(show(100));
    p.client_sends(&msgs, vec![client_pool]).unwrap();
    let memfd = host_pool(&mut p);
    assert_eq!(sys::file_size(memfd.as_raw_fd()).unwrap(), pool_size);

    let rows = 300;
    let mut travelled = 0;
    let mut wrapped = false;
    let pg = pg();
    for i in 1..=64u32 {
        let diff = rows * stride;
        let mut next = offset + diff;
        if next + size > pool_size {
            next = 0;
            wrapped = true;
        }
        // Mark the new position's first row, as the client would draw it.
        sys::pwrite_full(client_raw.as_raw_fd(), &[i as u8; 64], next).unwrap();
        let id = 100 + i;
        let mut msgs = vec![
            MsgBuilder::new(99 + i, op::wl_buffer::REQ_DESTROY).finish(),
            buffer(id, next),
        ];
        msgs.extend(show(id));
        p.client_sends(&msgs, vec![]).unwrap();
        offset = next;
        travelled += diff;

        let held = (offset + size).div_ceil(pg) - offset / pg;
        assert_eq!(vm.used(), (held * pg, 1), "only the live buffer is charged");
        assert!(allocated(&memfd) <= held * pg, "and only it holds memory");
        let mut first = [0u8; 64];
        sys::pread_full(memfd.as_raw_fd(), &mut first, offset).unwrap();
        assert_eq!(first, [i as u8; 64], "the compositor sees the new frame");
    }
    assert!(wrapped);
    assert!(pool_size > budget && travelled > budget);
    // Nothing but the last buffer's pages is left in the memfd.
    assert_eq!(next_data(&memfd, 0), Some(offset / pg * pg));
}

#[test]
fn a_connection_that_is_over_sheds_its_pools_at_once() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 30, 64));
    let mut p = shm_pair(&vm);
    create_pool(&mut p, 10, 1 << 20).unwrap();
    // The memfd the compositor got is its own reference; ours goes.
    let _at_compositor = p.at_server();
    p.h.shed();
    assert_eq!(vm.used(), (0, 0));
}

#[test]
fn an_empty_blob_chunk_is_refused_before_it_opens_anything() {
    let mut b = crate::blob::Blobs::new(true);
    assert_eq!(b.chunk(1, 0, &[]), Err(crate::blob::BlobError::Empty(1)));
    // A zero-length blob needs no chunks at all.
    assert!(b.take(1, 0).is_ok());
}

#[test]
fn unfinished_blobs_are_capped_by_count_not_only_by_bytes() {
    let mut b = crate::blob::Blobs::new(true);
    for id in 1..=crate::blob::MAX_INCOMING as u32 {
        b.chunk(id, 0, b"x").unwrap();
    }
    let next = crate::blob::MAX_INCOMING as u32 + 1;
    assert_eq!(
        b.chunk(next, 0, b"x"),
        Err(crate::blob::BlobError::TooMany(next))
    );
    // A blob already under way may still finish, and taking one makes room.
    b.chunk(1, 1, b"y").unwrap();
    assert!(b.take(1, 2).is_ok());
    b.chunk(next, 0, b"x").unwrap();
    b.clear();
    assert_eq!(b.take(2, 1).unwrap_err(), crate::blob::BlobError::BadId(2));
}

#[test]
#[cfg_attr(miri, ignore = "Miri has no seals (F_GET_SEALS)")]
fn a_keymap_arrives_as_a_sealed_copy() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_seat", 9)]);
    p.bind(1, "wl_seat", 9, 3).unwrap();
    p.client_sends(
        &[MsgBuilder::new(3, op::wl_seat::REQ_GET_KEYBOARD)
            .new_id(4)
            .finish()],
        vec![],
    )
    .unwrap();
    let keymap: Vec<u8> = b"xkb_keymap { ... };\0".repeat(5000); // > one record
    p.server_sends(
        &[MsgBuilder::new(4, op::wl_keyboard::EVT_KEYMAP)
            .uint(1)
            .uint(keymap.len() as u32)
            .finish()],
        vec![memfd_with(&keymap)],
    )
    .unwrap();
    let (msgs, fds) = p.at_client();
    assert_eq!(split(&msgs).len(), 1);
    assert_eq!(fds.len(), 1);
    assert_eq!(read_all(&fds[0]), keymap);
    let seals = crate::sys::seals(fds[0].as_raw_fd()).unwrap();
    assert_eq!(seals & libc::F_SEAL_WRITE, libc::F_SEAL_WRITE);
    assert_eq!(p.h.blob_stats().0, 1);
}

#[test]
fn an_icc_file_is_sent_from_its_offset_and_the_offset_becomes_zero() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wp_color_manager_v1", 1)]);
    p.bind(1, "wp_color_manager_v1", 1, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wp_color_manager_v1::REQ_CREATE_ICC_CREATOR)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let file = memfd_with(b"junkICCPROFILE");
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE)
                .uint(4)
                .uint(10)
                .finish(),
        ],
        vec![file],
    )
    .unwrap();
    let (msgs, fds) = p.at_server();
    let msgs = split(&msgs).pop().unwrap();
    let d = msg_desc(
        proto::WP_IMAGE_DESCRIPTION_CREATOR_ICC_V1,
        Dir::Request,
        op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE,
    );
    let a = wire::parse(d, &msgs).unwrap();
    assert_eq!(a[1].val, Val::Uint(0));
    assert_eq!(read_all(&fds[0]), b"ICCPROFILE");

    // A guest that says another offset over the channel (its kernel is not
    // trusted): the compositor still gets the sealed copy with offset 0,
    // never an offset past the copy's end.
    let m = MsgBuilder::new(4, op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE)
        .uint(1 << 20)
        .uint(10)
        .finish();
    let mut q = VecDeque::from([
        frame::Unit {
            rec: frame::record(frame::REC_BLOB, 9, 0, b"ICCPROFILE"),
            descs: vec![],
        },
        frame::Unit {
            rec: frame::record(frame::REC_WAYLAND, 0, 1, &m),
            descs: vec![DescOut::plain(Desc {
                a: 9,
                c: 10,
                ..Desc::new(frame::DESC_BLOB)
            })],
        },
    ]);
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    p.h.from_channel(&f, fds, &mut TestPlat::default()).unwrap();
    let (msgs, fds) = p.at_server();
    let msg = split(&msgs).pop().unwrap();
    assert_eq!(wire::parse(d, &msg).unwrap()[1].val, Val::Uint(0));
    assert_eq!(read_all(&fds[0]), b"ICCPROFILE");
}

#[test]
fn a_data_offer_pipe_becomes_a_stream_with_an_explicit_end() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_data_device_manager", 3), (2, "wl_seat", 9)]);
    p.bind(1, "wl_data_device_manager", 3, 3).unwrap();
    p.bind(2, "wl_seat", 9, 4).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wl_data_device_manager::REQ_GET_DATA_DEVICE)
                .new_id(5)
                .object(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let offer = wire::SERVER_ID_START + 3;
    p.server_sends(
        &[MsgBuilder::new(5, op::wl_data_device::EVT_DATA_OFFER)
            .new_id(offer)
            .finish()],
        vec![],
    )
    .unwrap();
    p.at_client();
    // The client asks for the data, handing over the write end of its pipe.
    let (client_rd, client_wr) = sys::pipe().unwrap();
    p.client_sends(
        &[MsgBuilder::new(offer, op::wl_data_offer::REQ_RECEIVE)
            .string(Some("text/plain"))
            .finish()],
        vec![client_wr],
    )
    .unwrap();
    let (_, mut fds) = p.at_server();
    let host_wr = fds.remove(0);
    assert!(sys::is_fifo(host_wr.as_raw_fd()));
    // The host-side source writes and closes; the host engine pumps its read
    // end.
    sys::write(host_wr.as_raw_fd(), b"hello from the host").unwrap();
    drop(host_wr);
    for _ in 0..4 {
        for i in p.h.stream_interest() {
            p.h.stream_io(i.id, i.read, i.write);
        }
        p.pump();
        for i in p.g.stream_interest() {
            p.g.stream_io(i.id, i.read, i.write);
        }
        p.pump();
    }
    p.pump();
    let mut buf = [0u8; 64];
    let n = sys::read(client_rd.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello from the host");
    // ... and then end of file, because the source said so.
    assert_eq!(sys::read(client_rd.as_raw_fd(), &mut buf).unwrap(), 0);
    assert!(p.g.stream_interest().is_empty() && p.h.stream_interest().is_empty());
}

#[test]
#[cfg_attr(
    miri,
    ignore = "Miri's pipes are unbounded: nothing is held back to overrun"
)]
fn a_stream_that_overruns_its_credit_is_a_protocol_error() {
    let mut s = crate::stream::Streams::new(false);
    let (_rd, wr) = sys::pipe().unwrap();
    let (id, _) = s.add_sink(wr).unwrap();
    let mut out = Vec::new();
    let big = vec![0u8; crate::stream::WINDOW + 1];
    // The pipe fills (64 KiB) and the rest is held; past the window it is an error.
    assert!(s.data(id, &big[..crate::stream::WINDOW], &mut out).is_ok());
    assert!(s.data(id, &big[..70 * 1024], &mut out).is_err());
}

#[test]
fn dmabuf_planes_become_host_exports_of_the_owners_object() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "zwp_linux_dmabuf_v1", 5)]);
    p.bind(1, "zwp_linux_dmabuf_v1", 4, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::zwp_linux_dmabuf_v1::REQ_CREATE_PARAMS)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let planes: Vec<Vec<u8>> = (0..2)
        .map(|i| {
            MsgBuilder::new(4, op::zwp_linux_buffer_params_v1::REQ_ADD)
                .uint(i)
                .uint(0)
                .uint(256)
                .uint(0)
                .uint(0)
                .finish()
        })
        .collect();
    p.at_server();
    p.client_sends(&planes, vec![memfd_with(b"a"), memfd_with(b"b")])
        .unwrap();
    let (msgs, fds) = p.at_server();
    assert_eq!(split(&msgs).len(), 2);
    assert_eq!(fds.len(), 2);
    assert_eq!(p.hp.exports, vec![(7, 42), (7, 42)]);
}

#[test]
fn a_descriptor_the_guest_could_not_carry_arrives_as_a_placeholder() {
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: Local::Server,
        policy: Policy::default(),
        rewrites: None,
        synth_released: false,
    });
    let mut q = VecDeque::new();
    let setup = [MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
        .new_id(2)
        .finish()]
    .concat();
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 0, &setup),
        descs: vec![],
    });
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    h.from_channel(&f, fds, &mut TestPlat::default()).unwrap();
    let data = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("zwp_linux_dmabuf_v1"))
        .uint(5)
        .finish();
    h.from_local(&mut LocalIn::new(data, []), &mut TestPlat::default())
        .unwrap();
    let m = [
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(1)
            .generic_new_id("zwp_linux_dmabuf_v1", 4, 3)
            .finish(),
        MsgBuilder::new(3, op::zwp_linux_dmabuf_v1::REQ_CREATE_PARAMS)
            .new_id(4)
            .finish(),
        MsgBuilder::new(4, op::zwp_linux_buffer_params_v1::REQ_ADD)
            .uint(-1i32 as u32)
            .uint(0)
            .uint(0)
            .uint(0)
            .uint(0)
            .finish(),
    ]
    .concat();
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 1, &m),
        descs: vec![DescOut::plain(Desc::invalid(frame::DESC_DMABUF))],
    });
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let mut plat = TestPlat::default();
    h.from_channel(&f, fds, &mut plat).unwrap();
    assert!(
        plat.exports.is_empty(),
        "nothing is exported for an invalid descriptor"
    );
    let (_, fds) = flatten(h.local_out().drain());
    assert_eq!(fds.len(), 1);
    assert_eq!(h.stats.placeholders, 1);
}

#[test]
fn a_timeline_nobody_could_name_ends_the_client_with_invalid_timeline_not_a_placeholder() {
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: Local::Server,
        policy: Policy {
            fences: true,
            ..Policy::default()
        },
        rewrites: None,
        synth_released: false,
    });
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(
            frame::REC_WAYLAND,
            0,
            0,
            &MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
                .new_id(2)
                .finish(),
        ),
        descs: vec![],
    }]);
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    h.from_channel(&f, fds, &mut TestPlat::default()).unwrap();
    let data = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("wp_linux_drm_syncobj_manager_v1"))
        .uint(1)
        .finish();
    h.from_local(&mut LocalIn::new(data, []), &mut TestPlat::default())
        .unwrap();
    h.local_out().drain();
    let m = [
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(1)
            .generic_new_id("wp_linux_drm_syncobj_manager_v1", 1, 3)
            .finish(),
        MsgBuilder::new(3, op::wp_linux_drm_syncobj_manager_v1::REQ_IMPORT_TIMELINE)
            .new_id(4)
            .finish(),
    ]
    .concat();
    // The guest kernel found no host syncobj behind the client's file.
    q.push_back(frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 1, &m),
        descs: vec![DescOut::plain(Desc::invalid(frame::DESC_SYNCOBJ))],
    });
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let e = h
        .from_channel(&f, fds, &mut TestPlat::default())
        .unwrap_err();
    assert_eq!(
        (e.object, e.code, e.blame),
        (3, ERR_SYNCOBJ_INVALID_TIMELINE, Blame::Channel),
        "{}",
        e.message
    );
    // The compositor got the bind and nothing of the import.
    let (msgs, fds) = flatten(h.local_out().drain());
    assert_eq!(split(&msgs).len(), 1);
    assert!(fds.is_empty());
    assert_eq!(h.stats.placeholders, 0);
}

#[test]
fn a_wayland_record_that_miscounts_its_descriptors_is_refused() {
    let mut h = Engine::new(EngineConfig {
        side: Side::Host,
        local: Local::Server,
        policy: Policy::default(),
        rewrites: None,
        synth_released: false,
    });
    let mut q = VecDeque::new();
    q.push_back(frame::Unit {
        rec: frame::record(
            frame::REC_WAYLAND,
            0,
            1,
            &MsgBuilder::new(1, op::wl_display::REQ_SYNC)
                .new_id(3)
                .finish(),
        ),
        descs: vec![DescOut::plain(Desc::new(frame::DESC_BLOB))],
    });
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let e = h
        .from_channel(&f, fds, &mut TestPlat::default())
        .unwrap_err();
    assert!(e.message.contains("descriptor count"), "{}", e.message);
}

#[test]
fn a_missing_descriptor_from_the_local_peer_is_fatal() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(2, "wl_shm", 2)]);
    p.bind(2, "wl_shm", 2, 4).unwrap();
    let e = p.client_sends(
        &[MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(5)
            .int(4096)
            .finish()],
        vec![],
    );
    assert!(e.unwrap_err().message.contains("descriptor expected"));
}

#[test]
fn feedback_dev_t_is_mapped_to_the_guest_node_and_unknown_nodes_to_zero() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "zwp_linux_dmabuf_v1", 5)]);
    p.bind(1, "zwp_linux_dmabuf_v1", 4, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::zwp_linux_dmabuf_v1::REQ_GET_DEFAULT_FEEDBACK)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let host = makedev(226, 129).to_ne_bytes();
    let other = makedev(226, 130).to_ne_bytes();
    p.server_sends(
        &[
            MsgBuilder::new(4, op::zwp_linux_dmabuf_feedback_v1::EVT_MAIN_DEVICE)
                .array(&host)
                .finish(),
            MsgBuilder::new(
                4,
                op::zwp_linux_dmabuf_feedback_v1::EVT_TRANCHE_TARGET_DEVICE,
            )
            .array(&other)
            .finish(),
        ],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    let msgs = split(&msgs);
    let d = msg_desc(
        proto::ZWP_LINUX_DMABUF_FEEDBACK_V1,
        Dir::Event,
        op::zwp_linux_dmabuf_feedback_v1::EVT_MAIN_DEVICE,
    );
    let Val::Array(a) = wire::parse(d, &msgs[0]).unwrap()[0].val else {
        panic!()
    };
    assert_eq!(
        major_minor(u64::from_ne_bytes(a.try_into().unwrap())),
        (226, 128)
    );
    let Val::Array(a) = wire::parse(d, &msgs[1]).unwrap()[0].val else {
        panic!()
    };
    assert_eq!(u64::from_ne_bytes(a.try_into().unwrap()), 0);
}

#[test]
fn presentation_timestamps_move_by_the_clock_offset_only_for_monotonic_clocks() {
    let mut p = Pair::new(Policy::default());
    p.offset
        .store(5_500_000_000, std::sync::atomic::Ordering::Relaxed); // host is 5.5 s ahead
    p.registry(&[(1, "wp_presentation", 2), (2, "wl_compositor", 6)]);
    p.bind(1, "wp_presentation", 2, 3).unwrap();
    p.bind(2, "wl_compositor", 6, 4).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(5)
                .finish(),
            MsgBuilder::new(3, op::wp_presentation::REQ_FEEDBACK)
                .object(5)
                .new_id(6)
                .finish(),
            MsgBuilder::new(3, op::wp_presentation::REQ_FEEDBACK)
                .object(5)
                .new_id(7)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let presented = |id: u32| {
        MsgBuilder::new(id, op::wp_presentation_feedback::EVT_PRESENTED)
            .uint(0)
            .uint(100)
            .uint(200_000_000)
            .uint(16_666_666)
            .uint(0)
            .uint(1)
            .uint(0)
            .finish()
    };
    p.server_sends(
        &[
            MsgBuilder::new(3, op::wp_presentation::EVT_CLOCK_ID)
                .uint(1)
                .finish(),
            presented(6),
        ],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    let m = &split(&msgs)[1];
    let d = msg_desc(
        proto::WP_PRESENTATION_FEEDBACK,
        Dir::Event,
        op::wp_presentation_feedback::EVT_PRESENTED,
    );
    let a = wire::parse(d, m).unwrap();
    assert_eq!(
        (a[1].val, a[2].val),
        (Val::Uint(94), Val::Uint(700_000_000))
    );
    // Not a clock the offset applies to: left alone.
    p.server_sends(
        &[
            MsgBuilder::new(3, op::wp_presentation::EVT_CLOCK_ID)
                .uint(0)
                .finish(),
            presented(7),
        ],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    let msgs = split(&msgs);
    let a = wire::parse(d, &msgs[1]).unwrap();
    assert_eq!(a[1].val, Val::Uint(100));
    // The presented event is a destructor: feedback 6 is gone (a zombie
    // until delete_id).
    assert!(p.g.objects().get(6).unwrap().zombie);
}

/// One WAYLAND record of `msgs`, framed as the channel carries it.
fn wayland_frame(msgs: &[Vec<u8>]) -> Vec<u8> {
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(frame::REC_WAYLAND, 0, 0, &msgs.concat()),
        descs: vec![],
    }]);
    frame::pack(&mut q, 1 << 20, 256, false).0
}

#[test]
fn lease_submits_are_counted_in_a_frame_before_it_is_let_in() {
    let mut p = Pair::new(Policy {
        drm_file: false,
        lease: LeaseGate::Allow,
        fences: false,
    });
    let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
        .new_id(20)
        .finish();
    p.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    // A connection never offered a lease device: nothing to count, and
    // nothing is parsed.
    let mut none = Pair::new(Policy::default());
    none.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    let submit = |dev: u32, req: u32, lease: u32| {
        vec![
            MsgBuilder::new(dev, op::wp_drm_lease_device_v1::REQ_CREATE_LEASE_REQUEST)
                .new_id(req)
                .finish(),
            MsgBuilder::new(req, op::wp_drm_lease_request_v1::REQ_SUBMIT)
                .new_id(lease)
                .finish(),
        ]
    };
    // A device bound, a request made and submitted, all in one frame.
    let mut one = vec![
        MsgBuilder::new(2, op::wl_registry::REQ_BIND)
            .uint(1)
            .generic_new_id("wp_drm_lease_device_v1", 1, 3)
            .finish(),
    ];
    one.extend(submit(3, 4, 5));
    assert_eq!(p.h.lease_submits(&wayland_frame(&one)), 1);
    assert_eq!(none.h.lease_submits(&wayland_frame(&one)), 0);
    // A device the engine already knows, two submits and something else.
    p.bind(1, "wp_drm_lease_device_v1", 1, 3).unwrap();
    let mut two = submit(3, 4, 5);
    two.push(sync.clone());
    two.extend(submit(3, 6, 7));
    assert_eq!(p.h.lease_submits(&wayland_frame(&two)), 2);
    assert_eq!(p.h.lease_submits(&wayland_frame(&[sync])), 0);
    assert_eq!(p.h.lease_submits(b"not a frame"), 0);
}

/// A registry the frame itself creates is followed like any other object:
/// get_registry, bind through it, create a request and submit, all in one
/// frame, count one (they counted none, and went past the throttle).
#[test]
fn lease_submits_through_a_registry_made_in_the_same_frame_are_counted() {
    let mut p = Pair::new(Policy {
        drm_file: false,
        lease: LeaseGate::Allow,
        fences: false,
    });
    p.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    let msgs = vec![
        MsgBuilder::new(1, op::wl_display::REQ_GET_REGISTRY)
            .new_id(9)
            .finish(),
        MsgBuilder::new(9, op::wl_registry::REQ_BIND)
            .uint(1)
            .generic_new_id("wp_drm_lease_device_v1", 1, 10)
            .finish(),
        MsgBuilder::new(10, op::wp_drm_lease_device_v1::REQ_CREATE_LEASE_REQUEST)
            .new_id(11)
            .finish(),
        MsgBuilder::new(11, op::wp_drm_lease_request_v1::REQ_SUBMIT)
            .new_id(12)
            .finish(),
    ];
    assert_eq!(p.h.lease_submits(&wayland_frame(&msgs)), 1);
}

/// What the engine lets through is held to what was counted and admitted:
/// a submit past it ends the connection rather than reaching the compositor.
#[test]
fn a_submit_past_the_admitted_count_is_fatal() {
    let mut p = Pair::new(Policy {
        drm_file: true,
        lease: LeaseGate::Allow,
        fences: false,
    });
    p.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    p.bind(1, "wp_drm_lease_device_v1", 1, 3).unwrap();
    let submit = |req: u32| {
        vec![
            MsgBuilder::new(3, op::wp_drm_lease_device_v1::REQ_CREATE_LEASE_REQUEST)
                .new_id(req)
                .finish(),
            MsgBuilder::new(req, op::wp_drm_lease_request_v1::REQ_SUBMIT)
                .new_id(req + 1)
                .finish(),
        ]
    };
    p.h.allow_lease_submits(Some(1));
    p.h.from_channel(&wayland_frame(&submit(4)), vec![], &mut p.hp)
        .unwrap();
    assert_eq!(p.h.stats.lease_submits, 1);
    let e =
        p.h.from_channel(&wayland_frame(&submit(6)), vec![], &mut p.hp)
            .unwrap_err();
    assert_eq!(e.blame, Blame::Channel);
    assert_eq!(p.h.stats.lease_submits, 1);
}

#[test]
fn a_lease_device_released_without_the_event_gets_one_synthesised() {
    let mut p = Pair::new(Policy {
        drm_file: false,
        lease: LeaseGate::Allow,
        fences: false,
    });
    p.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    // The host learnt from the guest's HELLO that it can adopt DRM files.
    assert_eq!(p.h.stats.globals_offered, 1);
    p.bind(1, "wp_drm_lease_device_v1", 1, 3).unwrap();
    // drm_fd: a DRM file from the compositor, adopted on the way.
    p.server_sends(
        &[MsgBuilder::new(3, op::wp_drm_lease_device_v1::EVT_DRM_FD).finish()],
        vec![memfd_with(b"card")],
    )
    .unwrap();
    let (_, fds) = p.at_client();
    assert_eq!(fds.len(), 1);
    p.client_sends(
        &[MsgBuilder::new(3, op::wp_drm_lease_device_v1::REQ_RELEASE).finish()],
        vec![],
    )
    .unwrap();
    // Hyprland 0.56: no `released`, just the delete_id.
    p.server_sends(
        &[MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
            .uint(3)
            .finish()],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    let msgs = split(&msgs);
    assert_eq!(msgs.len(), 2);
    let h = wire::peek_header(&msgs[0]).unwrap();
    assert_eq!(
        (h.object, h.opcode),
        (3, op::wp_drm_lease_device_v1::EVT_RELEASED)
    );
    assert_eq!(p.g.stats.released_synthesised, 1);
    assert!(p.g.objects().get(3).is_none());
}

#[test]
fn a_lease_device_that_did_send_released_gets_no_second_one() {
    let mut p = Pair::new(Policy {
        drm_file: false,
        lease: LeaseGate::Allow,
        fences: false,
    });
    p.registry(&[(1, "wp_drm_lease_device_v1", 1)]);
    p.bind(1, "wp_drm_lease_device_v1", 1, 3).unwrap();
    p.client_sends(
        &[MsgBuilder::new(3, op::wp_drm_lease_device_v1::REQ_RELEASE).finish()],
        vec![],
    )
    .unwrap();
    p.server_sends(
        &[
            MsgBuilder::new(3, op::wp_drm_lease_device_v1::EVT_RELEASED).finish(),
            MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                .uint(3)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    assert_eq!(split(&msgs).len(), 2);
    assert_eq!(p.g.stats.released_synthesised, 0);
}

/// Hyprland with the lease patch (patches/hyprland): a desktop output leased,
/// the lease ending, the connector advertised again to a client that had
/// destroyed its object for it, and `released` sent by the compositor itself.
/// The proxy must carry every step, and synthesise `released` only for the
/// device the compositor did not answer -- per resource, never twice.
#[test]
fn a_lease_round_trip_keeps_hyprlands_released_and_re_advertised_connectors() {
    use op::wp_drm_lease_connector_v1 as conn_op;
    use op::wp_drm_lease_device_v1 as dev_op;
    let mut p = Pair::new(Policy {
        drm_file: false,
        lease: LeaseGate::Allow,
        fences: false,
    });
    p.registry(&[
        (1, "wp_drm_lease_device_v1", 1),
        (2, "wp_drm_lease_device_v1", 1),
    ]);
    p.bind(1, "wp_drm_lease_device_v1", 1, 3).unwrap();
    p.bind(2, "wp_drm_lease_device_v1", 1, 4).unwrap();
    p.at_server();
    const CONN: u32 = 0xff00_0000;
    let advertise = |dev: u32| {
        vec![
            MsgBuilder::new(dev, dev_op::EVT_CONNECTOR)
                .new_id(CONN)
                .finish(),
            MsgBuilder::new(CONN, conn_op::EVT_NAME)
                .string(Some("DP-2"))
                .finish(),
            MsgBuilder::new(CONN, conn_op::EVT_CONNECTOR_ID)
                .uint(90)
                .finish(),
            MsgBuilder::new(CONN, conn_op::EVT_DONE).finish(),
            MsgBuilder::new(dev, dev_op::EVT_DONE).finish(),
        ]
    };
    let mut first = vec![MsgBuilder::new(3, dev_op::EVT_DRM_FD).finish()];
    first.extend(advertise(3));
    first.push(MsgBuilder::new(4, dev_op::EVT_DRM_FD).finish());
    first.push(MsgBuilder::new(4, dev_op::EVT_DONE).finish());
    p.server_sends(&first, vec![memfd_with(b"card"), memfd_with(b"card")])
        .unwrap();
    let (msgs, fds) = p.at_client();
    assert_eq!(split(&msgs).len(), first.len());
    assert_eq!(fds.len(), 2);

    // Lease DP-2: request 5, lease 6; the lease fd is a DRM file like drm_fd.
    p.client_sends(
        &[
            MsgBuilder::new(3, dev_op::REQ_CREATE_LEASE_REQUEST)
                .new_id(5)
                .finish(),
            MsgBuilder::new(5, op::wp_drm_lease_request_v1::REQ_REQUEST_CONNECTOR)
                .object(CONN)
                .finish(),
            MsgBuilder::new(5, op::wp_drm_lease_request_v1::REQ_SUBMIT)
                .new_id(6)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(split(&p.at_server().0).len(), 3);
    p.server_sends(
        &[
            MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                .uint(5)
                .finish(),
            MsgBuilder::new(6, op::wp_drm_lease_v1::EVT_LEASE_FD).finish(),
        ],
        vec![memfd_with(b"lease")],
    )
    .unwrap();
    let (_, fds) = p.at_client();
    assert_eq!(fds.len(), 1, "the lease fd");

    // The lease ends; the client drops the lease and its connector object,
    // and Hyprland advertises the output again -- reusing the freed server
    // id, as libwayland-server's id map does.
    p.server_sends(
        &[MsgBuilder::new(6, op::wp_drm_lease_v1::EVT_FINISHED).finish()],
        vec![],
    )
    .unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(6, op::wp_drm_lease_v1::REQ_DESTROY).finish(),
            MsgBuilder::new(CONN, conn_op::REQ_DESTROY).finish(),
        ],
        vec![],
    )
    .unwrap();
    let mut again = vec![
        MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
            .uint(6)
            .finish(),
    ];
    again.extend(advertise(3));
    p.server_sends(&again, vec![]).unwrap();
    let (msgs, _) = p.at_client();
    let msgs = split(&msgs);
    assert_eq!(msgs.len(), again.len() + 1, "finished, then all of these");
    let h = wire::peek_header(&msgs[2]).unwrap();
    assert_eq!((h.object, h.opcode), (3, dev_op::EVT_CONNECTOR));

    // Release both devices. Hyprland answers device 3 with `released`; device
    // 4's compositor (an unpatched one) only frees the id.
    p.client_sends(
        &[
            MsgBuilder::new(3, dev_op::REQ_RELEASE).finish(),
            MsgBuilder::new(4, dev_op::REQ_RELEASE).finish(),
        ],
        vec![],
    )
    .unwrap();
    p.server_sends(
        &[
            MsgBuilder::new(3, dev_op::EVT_RELEASED).finish(),
            MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                .uint(3)
                .finish(),
            MsgBuilder::new(1, op::wl_display::EVT_DELETE_ID)
                .uint(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let (msgs, _) = p.at_client();
    let released = |dev: u32| {
        split(&msgs)
            .iter()
            .filter(|m| {
                let h = wire::peek_header(m).unwrap();
                (h.object, h.opcode) == (dev, dev_op::EVT_RELEASED)
            })
            .count()
    };
    assert_eq!((released(3), released(4)), (1, 1));
    assert_eq!(p.g.stats.released_synthesised, 1);
}

#[test]
fn the_host_error_record_reaches_the_guest_as_a_remote_fatal() {
    let mut p = Pair::new(Policy::default());
    let f = Fatal {
        object: 1,
        code: ERR_IMPLEMENTATION,
        message: "gone".into(),
        blame: Blame::Channel,
    };
    let mut q = VecDeque::from([f.record()]);
    let (fr, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let e =
        p.g.from_channel(&fr, fds, &mut TestPlat::default())
            .unwrap_err();
    assert_eq!(e.blame, Blame::Remote);
    assert_eq!(e.message, "gone");
    // And becomes a wl_display.error for the client.
    let m = e.display_error();
    let d = msg_desc(proto::WL_DISPLAY, Dir::Event, op::wl_display::EVT_ERROR);
    let a = wire::parse(d, &m).unwrap();
    assert_eq!(a[1].val, Val::Uint(ERR_IMPLEMENTATION));
}

#[test]
fn local_output_never_puts_more_than_28_descriptors_in_one_sendmsg() {
    let mut lo = crate::localout::LocalOut::default();
    for _ in 0..30 {
        lo.push(
            &MsgBuilder::new(1, 0).finish(),
            vec![sys::placeholder_fd().unwrap()],
        );
    }
    let segs = lo.drain();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].1.len(), 28);
    assert_eq!(segs[1].1.len(), 2);
    // Each segment's descriptors precede the messages that consume them.
    assert_eq!(segs[0].0.len(), 28 * 8);
}

#[test]
fn dev_t_encoding_matches_glibc() {
    assert_eq!(makedev(226, 128), 0xe280);
    assert_eq!(major_minor(makedev(4095, 1 << 20)), (4095, 1 << 20));
    assert_eq!(major_minor(makedev(0x12345, 0xabcdef)), (0x12345, 0xabcdef));
}

/// wl_shm bound as 4 and wl_compositor as 3, a surface 7, and pool 5 over
/// `pool` (`size` bytes as the client claims it).
fn shm_client(p: &mut Pair, pool: OwnedFd, size: i32) {
    p.registry(&[(1, "wl_compositor", 6), (2, "wl_shm", 2)]);
    p.bind(1, "wl_compositor", 6, 3).unwrap();
    p.bind(2, "wl_shm", 2, 4).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                .new_id(5)
                .int(size)
                .finish(),
            MsgBuilder::new(3, op::wl_compositor::REQ_CREATE_SURFACE)
                .new_id(7)
                .finish(),
        ],
        vec![pool],
    )
    .unwrap();
}

fn shm_buffer(pool: u32, id: u32, offset: i32, height: i32, stride: i32) -> Vec<u8> {
    MsgBuilder::new(pool, op::wl_shm_pool::REQ_CREATE_BUFFER)
        .new_id(id)
        .int(offset)
        .int(stride / 4)
        .int(height)
        .int(stride)
        .uint(0)
        .finish()
}

/// The client's side charges what its buffers cover as the server's side
/// does, so a buffer the host would refuse is refused before the daemon
/// reads a byte of it, and the client is told. Before, the client's side
/// took any buffer its pool's claimed size allowed, and read all of it at
/// every commit.
#[test]
fn a_client_side_buffer_past_the_connections_bytes_is_refused_before_it_is_read() {
    let mut p = Pair::new(Policy::default());
    // Sparse: a client's claim costs the client nothing.
    shm_client(&mut p, sys::memfd(c"t", 0).unwrap(), i32::MAX);
    p.client_sends(&[shm_buffer(5, 10, 0, 8192, 65536)], vec![])
        .unwrap();
    let e = p
        .client_sends(&[shm_buffer(5, 11, 1 << 30, 8192, 65536)], vec![])
        .unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(e.blame, Blame::Local);
}

/// A commit costs the client's side one record at a time, not the buffer:
/// the copy is read as the channel takes it, and the client's input after a
/// commit that fills the channel's queue waits until the channel drains.
#[test]
fn a_commit_is_read_as_the_channel_takes_it_and_input_waits_behind_it() {
    let mut p = Pair::new(Policy::default());
    let (stride, height) = (4096i32, 2048i32); // 8 MiB
    let size = (stride * height) as usize;
    let pixels: Vec<u8> = (0..size).map(|i| (i / 4096) as u8 ^ i as u8).collect();
    shm_client(&mut p, memfd_with(&pixels), size as i32);
    p.client_sends(&[shm_buffer(5, 6, 0, height, stride)], vec![])
        .unwrap();
    let (_, mut fds) = p.at_server();
    let host_pool = fds.pop().unwrap();
    let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
        .new_id(20)
        .finish();
    let data = [
        MsgBuilder::new(7, op::wl_surface::REQ_ATTACH)
            .object(6)
            .int(0)
            .int(0)
            .finish(),
        MsgBuilder::new(7, op::wl_surface::REQ_COMMIT).finish(),
        sync.clone(),
    ]
    .concat();
    let mut input = LocalIn::new(data, []);
    p.g.from_local(&mut input, &mut p.gp).unwrap();
    // The commit went in; what follows it waits for the channel.
    assert_eq!(
        input.bytes(),
        sync,
        "input past a full channel queue is left"
    );
    assert!(p.g.input_blocked());
    assert!(p.g.channel_backlog() >= size);
    // A frame's worth is read, not the buffer.
    let mut q = p.g.take_units_upto(256 * 1024);
    let taken: usize = q.iter().map(|u| u.bytes()).sum();
    assert!(taken <= 256 * 1024 + 2 * frame::MAX_REC_PAYLOAD, "{taken}");
    while !q.is_empty() {
        let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
        p.h.from_channel(&f, fds, &mut p.hp).unwrap();
    }
    p.pump();
    assert!(!p.g.input_blocked());
    assert_eq!(read_all(&host_pool), pixels);
    p.g.from_local(&mut input, &mut p.gp).unwrap();
    assert!(input.is_empty());
    p.pump();
    let (msgs, _) = p.at_server();
    assert_eq!(split(&msgs).last(), Some(&sync));
}

/// A blob is read as the channel takes it too: the first record at once
/// (all of a keymap), the rest as frames are made.
#[test]
fn a_large_blob_is_read_a_record_at_a_time() {
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wp_color_manager_v1", 1)]);
    p.bind(1, "wp_color_manager_v1", 1, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wp_color_manager_v1::REQ_CREATE_ICC_CREATOR)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let icc: Vec<u8> = (0..(4 << 20)).map(|i: u32| (i * 7) as u8).collect();
    let data = MsgBuilder::new(4, op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE)
        .uint(0)
        .uint(icc.len() as u32)
        .finish();
    p.g.from_local(&mut LocalIn::new(data, [memfd_with(&icc)]), &mut p.gp)
        .unwrap();
    assert!(p.g.channel_backlog() >= icc.len());
    let mut q = p.g.take_units_upto(1);
    assert_eq!(q.len(), 1, "one record");
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    p.h.from_channel(&f, fds, &mut p.hp).unwrap();
    p.pump();
    let (_, fds) = p.at_server();
    assert_eq!(read_all(&fds[0]), icc);
}

/// Sinks grant less the more of them are open: one transfer at full speed,
/// many together a bounded total. Before, every sink granted `WINDOW`, and
/// 256 streams held 64 MiB per connection.
#[test]
fn sinks_grant_less_as_more_streams_are_open() {
    use crate::stream::{MIN_WINDOW, SINK_TOTAL, WINDOW, window};
    assert_eq!(window(1), WINDOW);
    assert_eq!(window(crate::stream::MAX_STREAMS), MIN_WINDOW);
    assert!(window(crate::stream::MAX_STREAMS) * crate::stream::MAX_STREAMS <= SINK_TOTAL);
    let mut s = crate::stream::Streams::new(false);
    s.set_peer_windows(true);
    let mut pipes = Vec::new();
    let mut total = 0;
    for i in 1..=64 {
        let (rd, wr) = sys::pipe().unwrap();
        let (_, first) = s.add_sink(wr).unwrap();
        assert_eq!(first as usize, window(i));
        total += first as usize;
        pipes.push(rd);
    }
    assert!(total <= 10 << 20, "{total} granted to 64 streams");
    // A peer that does not take the grant from the descriptor starts at
    // WINDOW, and is told nothing.
    let mut old = crate::stream::Streams::new(false);
    let (_rd, wr) = sys::pipe().unwrap();
    assert_eq!(old.add_sink(wr).unwrap().1, 0);
}

/// A sink gives credit back only up to its share of what sinks may hold
/// now, counting what waits in it: with 32 streams open, a sink that took a
/// full window and wrote a pipe's worth grants nothing more until it drains.
#[test]
#[cfg_attr(miri, ignore = "Miri's pipes are unbounded")]
fn a_sink_credits_back_no_more_than_its_share() {
    let mut s = crate::stream::Streams::new(false);
    let mut keep = Vec::new();
    let mut first = None;
    for _ in 0..32 {
        let (rd, wr) = sys::pipe().unwrap();
        let (id, _) = s.add_sink(wr).unwrap();
        first.get_or_insert((id, rd.try_clone().unwrap()));
        keep.push(rd);
    }
    let (id, rd) = first.unwrap();
    let mut out = Vec::new();
    // An old peer's first window, all of it.
    s.data(id, &vec![7u8; crate::stream::WINDOW], &mut out)
        .unwrap();
    let credit = |out: &[frame::Unit]| -> u32 {
        out.iter()
            .filter_map(|u| {
                let f = frame::Records { buf: &u.rec }.next()?.ok()?;
                (f.ty == frame::REC_STREAM_CREDIT).then_some(f.arg)
            })
            .sum()
    };
    assert_eq!(credit(&out), 0, "more waits here than a share");
    // The reader drains; the sink writes the rest and grants its share.
    let mut buf = vec![0u8; 1 << 20];
    let mut got = 0;
    for _ in 0..16 {
        sys::set_nonblock(rd.as_raw_fd()).unwrap();
        while let Ok(n @ 1..) = sys::read(rd.as_raw_fd(), &mut buf) {
            got += n;
        }
        out.clear();
        s.io(id, false, true, &mut out);
        if got == crate::stream::WINDOW {
            break;
        }
    }
    assert_eq!(got, crate::stream::WINDOW);
    let share = crate::stream::window(32) as u32;
    assert!(credit(&out) <= share, "{} > {share}", credit(&out));
}

struct Budget(std::sync::atomic::AtomicUsize, usize);
impl crate::stream::ByteBudget for Budget {
    fn take(&self, n: usize) -> bool {
        use std::sync::atomic::Ordering::*;
        self.0
            .fetch_update(AcqRel, Acquire, |u| (u + n <= self.1).then_some(u + n))
            .is_ok()
    }
    fn give(&self, n: usize) {
        self.0.fetch_sub(n, std::sync::atomic::Ordering::AcqRel);
    }
}

/// A charge one budget refuses is taken from none of them: those before it
/// give theirs back, and those after are never asked.
#[test]
fn budgets_take_from_all_or_none() {
    use crate::budget::Budgets;
    use crate::shm::{ShmBudget, ShmCharge};
    use std::sync::atomic::Ordering::Relaxed;
    let (a, b, c) = (
        Arc::new(Budget(Default::default(), 100)),
        Arc::new(Budget(Default::default(), 50)),
        Arc::new(Budget(Default::default(), 100)),
    );
    let mut bs = Budgets::<dyn crate::stream::ByteBudget>::default();
    bs.push(a.clone());
    bs.push(b.clone());
    bs.push(c.clone());
    assert!(bs.take(40));
    assert!(!bs.take(20), "the second is at its limit");
    assert_eq!(
        (a.0.load(Relaxed), b.0.load(Relaxed), c.0.load(Relaxed)),
        (40, 40, 40)
    );
    bs.give(40);
    assert_eq!(
        (a.0.load(Relaxed), b.0.load(Relaxed), c.0.load(Relaxed)),
        (0, 0, 0)
    );

    let (vm, conn) = (
        Arc::new(ShmBudget::new(1 << 20, 2)),
        Arc::new(ShmBudget::new(1 << 20, 8)),
    );
    let mut shm = Budgets::<dyn ShmCharge>::default();
    shm.push(conn.clone());
    shm.push(vm.clone());
    assert!(shm.take(4096, 1) && shm.take(0, 1));
    assert!(!shm.take(0, 1), "the VM has no third pool");
    assert_eq!((conn.used(), vm.used()), ((4096, 2), (4096, 2)));
    shm.give(4096, 2);
    assert_eq!((conn.used(), vm.used()), ((0, 0), (0, 0)));
}

/// What sinks hold is charged to the owner's budget; data past it ends that
/// stream with ENOBUFS, and the connection goes on.
#[test]
#[cfg_attr(miri, ignore = "Miri's pipes are unbounded")]
fn a_sink_past_the_budget_ends_its_stream_not_the_connection() {
    let b = Arc::new(Budget(Default::default(), 100 * 1024));
    let mut s = crate::stream::Streams::new(false);
    s.add_budget(b.clone());
    let (_rd, wr) = sys::pipe().unwrap();
    let (id, _) = s.add_sink(wr).unwrap();
    let mut out = Vec::new();
    // A pipe's worth goes through; the rest waits here, charged.
    s.data(id, &vec![1u8; 96 * 1024], &mut out).unwrap();
    assert_eq!(b.0.load(std::sync::atomic::Ordering::Relaxed), s.held());
    assert!(s.held() > 0);
    out.clear();
    s.data(id, &vec![1u8; 96 * 1024], &mut out).unwrap();
    let eof = out
        .iter()
        .filter_map(|u| frame::Records { buf: &u.rec }.next()?.ok())
        .find(|r| r.ty == frame::REC_STREAM_EOF)
        .expect("the stream is ended");
    assert_eq!(eof.arg, libc::ENOBUFS as u32);
    assert!(s.is_empty());
    assert_eq!(b.0.load(std::sync::atomic::Ordering::Relaxed), 0);
}

/// Unfinished blobs are memfd pages this side holds: charged to the shm
/// budgets as their chunks arrive, and dropped, charge and all, when the
/// WAYLAND record after them does not take them. Before, they were charged
/// to nothing, and held until the connection ended.
#[test]
fn unfinished_blobs_are_charged_and_dropped_when_nothing_takes_them() {
    let vm = Arc::new(crate::shm::ShmBudget::new(1 << 20, 64));
    let mut p = Pair::new(Policy::default());
    p.h.set_shm_budget(vm.clone());
    let chunk = |id: u32, off: u32, n: usize| frame::Unit {
        rec: frame::record(frame::REC_BLOB, id, off, &vec![3u8; n]),
        descs: vec![],
    };
    let send = |p: &mut Pair, units: Vec<frame::Unit>| {
        let mut q: VecDeque<frame::Unit> = units.into();
        let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
        p.h.from_channel(&f, fds, &mut TestPlat::default())
    };
    let c = frame::MAX_REC_PAYLOAD;
    send(
        &mut p,
        (0..8).map(|i| chunk(1, (i * c) as u32, c)).collect(),
    )
    .unwrap();
    assert_eq!(vm.used().0, 8 * c as u64);
    // Past the VM's budget: refused.
    let more: Vec<frame::Unit> = (0..9).map(|i| chunk(3, (i * c) as u32, c)).collect();
    assert!(send(&mut p, more).is_err());
    drop(p);
    let mut p = Pair::new(Policy::default());
    p.h.set_shm_budget(vm.clone());
    assert_eq!(vm.used().0, 0, "a connection's blobs go with it");
    send(&mut p, vec![chunk(1, 0, c)]).unwrap();
    assert_eq!(vm.used().0, c as u64);
    // A WAYLAND record that does not take it: gone.
    let sync = MsgBuilder::new(1, op::wl_display::REQ_SYNC)
        .new_id(30)
        .finish();
    send(
        &mut p,
        vec![frame::Unit {
            rec: frame::record(frame::REC_WAYLAND, 0, 0, &sync),
            descs: vec![],
        }],
    )
    .unwrap();
    assert_eq!(vm.used().0, 0);
}

/// Error text a peer controls -- the far side's ERROR record, an interface
/// name it bound -- is escaped and bounded before it reaches a log or a
/// client's wl_display.error: a terminal escape, a newline starting a fake
/// log line or a bidirectional override arrives as text. Before, the ERROR
/// payload was taken verbatim.
#[test]
fn error_text_from_a_peer_arrives_printable_and_bounded() {
    let mut p = Pair::new(Policy::default());
    let evil = format!(
        "\u{1b}]0;owned\u{7}\u{1b}[2J\nERROR: fake line\u{202e}{}",
        "x".repeat(10_000)
    );
    let mut q = VecDeque::from([frame::Unit {
        rec: frame::record(frame::REC_ERROR, 1, 3, evil.as_bytes()),
        descs: vec![],
    }]);
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
    let e = p.g.from_channel(&f, fds, &mut p.gp).unwrap_err();
    assert_eq!(e.blame, Blame::Remote);
    assert!(e.message.len() <= MAX_FATAL_TEXT + 3, "{}", e.message.len());
    assert!(!e.message.chars().any(|c| c.is_control() || c == '\u{202e}'));
    assert!(
        e.message.starts_with("\\u{1b}]0;owned\\u{7}"),
        "{}",
        e.message
    );
    // An interface name the guest bound, in the host's error.
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wl_compositor", 6)]);
    let bind = MsgBuilder::new(2, op::wl_registry::REQ_BIND)
        .uint(1)
        .generic_new_id("wl_compositor\n[fake] x", 1, 3)
        .finish();
    let e = raw_to_host(&mut p, bind, None).unwrap_err();
    assert!(
        e.message.contains("wl_compositor\\n[fake] x"),
        "{}",
        e.message
    );
    assert!(!e.message.chars().any(char::is_control));
}

/// Shm pools and a client's blobs are read on the connection's own thread
/// (the backend's, under its lock): only memory is taken, never a file whose
/// server decides how long a read takes (FUSE), nor something that is not a
/// file at all. Before, any descriptor was taken and read.
#[test]
#[cfg_attr(miri, ignore = "Miri has no fstatfs")]
fn a_pool_or_a_clients_blob_must_be_memory() {
    // Not memory: an eventfd, and a file on the source tree's filesystem
    // (unless that is tmpfs too).
    let file: OwnedFd = std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"))
        .unwrap()
        .into();
    let on_disk = !sys::is_shmem(file.as_raw_fd());
    assert!(sys::is_shmem(sys::memfd(c"t", 0).unwrap().as_raw_fd()));
    let mut not_memory: Vec<OwnedFd> = vec![sys::eventfd().unwrap()];
    if on_disk {
        not_memory.push(file);
    }
    for fd in not_memory {
        let mut p = Pair::new(Policy::default());
        p.registry(&[(2, "wl_shm", 2)]);
        p.bind(2, "wl_shm", 2, 4).unwrap();
        let e = p
            .client_sends(
                &[MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
                    .new_id(5)
                    .int(4096)
                    .finish()],
                vec![fd],
            )
            .unwrap_err();
        assert_eq!((e.object, e.code), (4, ERR_SHM_INVALID_FD));
        assert_eq!(e.blame, Blame::Local);
    }
    // A blob from such a file goes as an invalid descriptor: the compositor
    // gets a placeholder, and the connection goes on.
    let mut p = Pair::new(Policy::default());
    p.registry(&[(1, "wp_color_manager_v1", 1)]);
    p.bind(1, "wp_color_manager_v1", 1, 3).unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(3, op::wp_color_manager_v1::REQ_CREATE_ICC_CREATOR)
                .new_id(4)
                .finish(),
        ],
        vec![],
    )
    .unwrap();
    let (rd, _wr) = sys::pipe().unwrap();
    p.client_sends(
        &[
            MsgBuilder::new(4, op::wp_image_description_creator_icc_v1::REQ_SET_ICC_FILE)
                .uint(0)
                .uint(10)
                .finish(),
        ],
        vec![rd],
    )
    .unwrap();
    let (_, fds) = p.at_server();
    assert_eq!(fds.len(), 1);
    assert_eq!(
        sys::file_size(fds[0].as_raw_fd()).unwrap(),
        0,
        "a placeholder"
    );
    assert_eq!(p.h.stats.placeholders, 1);
}

/// A value rewrite is of a message the allowlist can reach; one of anything
/// else is dead code that reads as a promise (ext_image_copy_capture's was:
/// every capture protocol is hidden).
#[test]
fn every_rewrite_is_of_an_interface_the_allowlist_reaches() {
    let model = vendored_model();
    let mut reach = std::collections::BTreeSet::new();
    for g in crate::policy_table::GLOBALS {
        reach.extend(closure::reachable(&model, g.interface).unwrap());
    }
    reach.insert("wl_display".to_string());
    reach.insert("wl_registry".to_string());
    for (iface, msg, _) in crate::policy_table::REWRITES {
        assert!(
            reach.contains(*iface),
            "{iface}.{msg} is rewritten but never reached"
        );
    }
}
