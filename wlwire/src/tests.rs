//! Unit tests for the codec, the object table, the closure check, the frame
//! format, the policy, and the engine end to end (a guest engine facing a
//! client wired to a host engine facing a compositor, both in memory).

use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::AtomicI64;

use crate::closure::{self, ModelIface, ModelMsg};
use crate::engine::*;
use crate::frame::{self, Desc, DescOut};
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

#[test]
fn the_vendored_allowlist_is_closed() {
    // build.rs already refused to build otherwise; this keeps the claim next
    // to the tests and exercises the real tables.
    let model: Vec<ModelIface> = proto::INTERFACES
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
        .collect();
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
    let (f, fds) = frame::pack(&mut q, 1 << 20, 256, true);
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
    // Fences never, yet.
    assert!(p.offer(6, b"wp_linux_drm_syncobj_manager_v1", 1).is_none());
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

    /// Move everything queued on either side across, until quiet.
    fn pump(&mut self) {
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

    fn client_sends(&mut self, msgs: &[Vec<u8>], fds: Vec<OwnedFd>) -> Result<(), Fatal> {
        let mut data: Vec<u8> = msgs.concat();
        let mut fds: VecDeque<OwnedFd> = fds.into();
        let r = self.g.from_local(&mut data, &mut fds, &mut self.gp);
        if r.is_ok() {
            self.pump();
        }
        r
    }

    fn server_sends(&mut self, msgs: &[Vec<u8>], fds: Vec<OwnedFd>) -> Result<(), Fatal> {
        let mut data: Vec<u8> = msgs.concat();
        let mut fds: VecDeque<OwnedFd> = fds.into();
        let r = self.h.from_local(&mut data, &mut fds, &mut self.hp);
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
    // Straight to the host, as a guest skipping its daemon would: pools are
    // sparse memfds, but their total size is capped per connection.
    let mut last = Ok(());
    for i in 0..6u32 {
        let m = MsgBuilder::new(4, op::wl_shm::REQ_CREATE_POOL)
            .new_id(10 + i)
            .int(i32::MAX)
            .finish();
        let mut q = VecDeque::from([frame::Unit {
            rec: frame::record(frame::REC_WAYLAND, 0, 1, &m),
            descs: vec![DescOut::plain(Desc {
                c: i32::MAX as u64,
                ..Desc::new(frame::DESC_SHM_POOL)
            })],
        }]);
        let (f, fds) = frame::pack(&mut q, 1 << 20, 256, false);
        last = p.h.from_channel(&f, fds, &mut TestPlat::default());
        if last.is_err() {
            break;
        }
    }
    let e = last.unwrap_err();
    assert_eq!(e.code, ERR_NO_MEMORY);
    assert_eq!(e.blame, Blame::Channel);
}

#[test]
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
    let seals = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GET_SEALS) };
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
    let mut buf = [0u8; 64];
    let n = sys::read(client_rd.as_raw_fd(), &mut buf).unwrap();
    assert_eq!(&buf[..n], b"hello from the host");
    // ... and then end of file, because the source said so.
    assert_eq!(sys::read(client_rd.as_raw_fd(), &mut buf).unwrap(), 0);
    assert!(p.g.stream_interest().is_empty() && p.h.stream_interest().is_empty());
}

#[test]
fn a_stream_that_overruns_its_credit_is_a_protocol_error() {
    let mut s = crate::stream::Streams::new(false);
    let (_rd, wr) = sys::pipe().unwrap();
    let id = s.add_sink(wr).unwrap();
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
    let mut data = MsgBuilder::new(2, op::wl_registry::EVT_GLOBAL)
        .uint(1)
        .string(Some("zwp_linux_dmabuf_v1"))
        .uint(5)
        .finish();
    h.from_local(&mut data, &mut VecDeque::new(), &mut TestPlat::default())
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
