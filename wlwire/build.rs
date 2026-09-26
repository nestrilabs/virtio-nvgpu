// SPDX-License-Identifier: Apache-2.0
//! Generates the codec's tables from the vendored protocol XML, and refuses to
//! build if the allowlist is not closed (see `src/closure.rs`).
//!
//! Output: `$OUT_DIR/protocols.rs`, included by `src/proto.rs`. It holds one
//! `Interface` per XML interface with every request and event, their argument
//! signatures, and -- resolved here to argument indices so nothing is looked up
//! by name at run time -- the descriptor class and value rewrite each message
//! has in `src/policy_table.rs`. Also an `IfaceId` constant per interface and
//! opcode constants under `op::<interface>::{REQ,EVT}_<MESSAGE>`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

#[path = "src/closure.rs"]
mod closure;
#[path = "src/policy_table.rs"]
#[allow(dead_code)]
mod policy_table;

use policy_table::{FD_POLICY, FdClass, GLOBALS, REWRITES, Rewrite};

#[derive(Clone, Debug)]
struct XArg {
    name: String,
    ty: String,
    interface: Option<String>,
    nullable: bool,
}

#[derive(Clone, Debug)]
struct XMsg {
    name: String,
    since: u32,
    destructor: bool,
    args: Vec<XArg>,
}

#[derive(Clone, Debug)]
struct XIface {
    name: String,
    version: u32,
    requests: Vec<XMsg>,
    events: Vec<XMsg>,
    file: String,
}

fn attr(e: &BytesStart, key: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == key.as_bytes())
        .map(|a| String::from_utf8_lossy(&a.value).into_owned())
}

fn parse(path: &Path) -> Vec<XIface> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut reader = Reader::from_str(&text);
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let mut out: Vec<XIface> = Vec::new();
    let mut cur: Option<XIface> = None;
    // (is_request, message) while inside <request>/<event>
    let mut msg: Option<(bool, XMsg)> = None;
    loop {
        let ev = reader
            .read_event()
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        let (start, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            match e.name().as_ref() {
                b"interface" => {
                    cur = Some(XIface {
                        name: attr(&e, "name").expect("interface name"),
                        version: attr(&e, "version")
                            .expect("interface version")
                            .parse()
                            .unwrap(),
                        requests: Vec::new(),
                        events: Vec::new(),
                        file: file.clone(),
                    });
                }
                tag @ (b"request" | b"event") => {
                    let m = XMsg {
                        name: attr(&e, "name").expect("message name"),
                        since: attr(&e, "since").map(|s| s.parse().unwrap()).unwrap_or(1),
                        destructor: attr(&e, "type").as_deref() == Some("destructor"),
                        args: Vec::new(),
                    };
                    let is_req = tag == b"request";
                    if empty {
                        let c = cur.as_mut().expect("message outside interface");
                        if is_req {
                            c.requests.push(m)
                        } else {
                            c.events.push(m)
                        }
                    } else {
                        msg = Some((is_req, m));
                    }
                }
                b"arg" => {
                    let a = XArg {
                        name: attr(&e, "name").expect("arg name"),
                        ty: attr(&e, "type").expect("arg type"),
                        interface: attr(&e, "interface"),
                        nullable: attr(&e, "allow-null").as_deref() == Some("true"),
                    };
                    msg.as_mut().expect("arg outside message").1.args.push(a);
                }
                _ => {}
            }
        }
        match ev {
            Event::End(e) => match e.name().as_ref() {
                b"interface" => out.push(cur.take().unwrap()),
                b"request" | b"event" => {
                    let (is_req, m) = msg.take().unwrap();
                    let c = cur.as_mut().unwrap();
                    if is_req {
                        c.requests.push(m)
                    } else {
                        c.events.push(m)
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

fn upper(s: &str) -> String {
    s.to_ascii_uppercase()
}

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("protocols");
    println!("cargo:rerun-if-changed=protocols");
    println!("cargo:rerun-if-changed=src/policy_table.rs");
    println!("cargo:rerun-if-changed=src/closure.rs");

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "xml"))
        .collect();
    // wayland.xml first so wl_display is interface 0; the rest by name so the
    // generated ids are stable.
    files.sort_by_key(|p| (p.file_name().unwrap() != "wayland.xml", p.clone()));

    // Dedupe by name, keeping the highest version (the same interface can be
    // vendored in an unstable and a stable file).
    let mut by_name: BTreeMap<String, XIface> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for f in &files {
        for i in parse(f) {
            match by_name.get(&i.name) {
                Some(old) if old.version >= i.version => {}
                Some(_) => {
                    by_name.insert(i.name.clone(), i);
                }
                None => {
                    order.push(i.name.clone());
                    by_name.insert(i.name.clone(), i);
                }
            }
        }
    }
    let ifaces: Vec<&XIface> = order.iter().map(|n| &by_name[n]).collect();
    let id_of: BTreeMap<&str, usize> = ifaces
        .iter()
        .enumerate()
        .map(|(i, x)| (x.name.as_str(), i))
        .collect();

    // ── the closure check ──
    let model: Vec<closure::ModelIface> = ifaces
        .iter()
        .map(|i| {
            let conv = |m: &XMsg| closure::ModelMsg {
                name: m.name.clone(),
                nfds: m.args.iter().filter(|a| a.ty == "fd").count(),
                creates: m
                    .args
                    .iter()
                    .filter(|a| a.ty == "new_id")
                    .filter_map(|a| a.interface.clone())
                    .collect(),
            };
            closure::ModelIface {
                name: i.name.clone(),
                requests: i.requests.iter().map(conv).collect(),
                events: i.events.iter().map(conv).collect(),
            }
        })
        .collect();
    let globals: Vec<&str> = GLOBALS.iter().map(|g| g.interface).collect();
    let pol: Vec<(&str, &str)> = FD_POLICY.iter().map(|(i, m, _)| (*i, *m)).collect();
    let errors = closure::check(&model, &globals, &pol);
    if !errors.is_empty() {
        panic!(
            "wlwire: the allowlist is not closed over the descriptor policy:\n  {}",
            errors.join("\n  ")
        );
    }

    // ── generate ──
    let find_msg = |iface: &str, msg: &str| -> (&XIface, &XMsg) {
        let i = by_name
            .get(iface)
            .unwrap_or_else(|| panic!("policy names unknown interface {iface}"));
        let m = i
            .requests
            .iter()
            .chain(i.events.iter())
            .find(|m| m.name == msg)
            .unwrap_or_else(|| panic!("policy names unknown message {iface}.{msg}"));
        (i, m)
    };
    let arg_index = |iface: &str, m: &XMsg, arg: &str, ty: &str| -> usize {
        let idx = m
            .args
            .iter()
            .position(|a| a.name == arg)
            .unwrap_or_else(|| panic!("policy names unknown argument {iface}.{}({arg})", m.name));
        assert_eq!(
            m.args[idx].ty, ty,
            "{iface}.{}({arg}) is not a {ty}",
            m.name
        );
        idx
    };
    for (iface, msg, _) in REWRITES {
        find_msg(iface, msg);
    }

    let mut s = String::new();
    writeln!(
        s,
        "// @generated by wlwire/build.rs from wlwire/protocols/*.xml. Do not edit."
    )
    .unwrap();
    writeln!(s, "pub const INTERFACE_COUNT: usize = {};", ifaces.len()).unwrap();
    for (n, i) in ifaces.iter().enumerate() {
        writeln!(s, "/// `{}` v{} ({})", i.name, i.version, i.file).unwrap();
        writeln!(s, "pub const {}: IfaceId = {n};", upper(&i.name)).unwrap();
    }

    let gen_msgs = |i: &XIface, msgs: &[XMsg]| -> String {
        let mut o = String::from("&[");
        for m in msgs {
            let mut args = String::from("&[");
            for a in &m.args {
                let kind = match a.ty.as_str() {
                    "int" => "Int",
                    "uint" => "Uint",
                    "fixed" => "Fixed",
                    "string" => "Str",
                    "object" => "Object",
                    "new_id" => "NewId",
                    "array" => "Array",
                    "fd" => "Fd",
                    t => panic!("{}.{}: unknown arg type {t}", i.name, m.name),
                };
                let iface = match &a.interface {
                    Some(n) => match id_of.get(n.as_str()) {
                        Some(id) => format!("Some({id})"),
                        // An object argument of an interface nobody vendored
                        // is fine (it is only ever an existing id); a new_id of
                        // one is not, and the closure check caught it for
                        // everything reachable.
                        None => "None".to_string(),
                    },
                    None => "None".to_string(),
                };
                write!(
                    args,
                    "Arg {{ name: {:?}, kind: ArgKind::{kind}, nullable: {}, iface: {iface} }},",
                    a.name, a.nullable
                )
                .unwrap();
            }
            args.push(']');
            let nfds = m.args.iter().filter(|a| a.ty == "fd").count();
            let fd = FD_POLICY
                .iter()
                .find(|(pi, pm, _)| *pi == i.name && *pm == m.name)
                .map(|(_, _, c)| match c {
                    FdClass::ShmPool => "Some(FdKind::ShmPool)".to_string(),
                    FdClass::Dmabuf => "Some(FdKind::Dmabuf)".to_string(),
                    FdClass::Stream => "Some(FdKind::Stream)".to_string(),
                    FdClass::DrmFile => "Some(FdKind::DrmFile)".to_string(),
                    FdClass::Syncobj => "Some(FdKind::Syncobj)".to_string(),
                    FdClass::Blob { size, offset } => {
                        let si = arg_index(&i.name, m, size, "uint");
                        let oi = offset.map(|o| arg_index(&i.name, m, o, "uint"));
                        format!("Some(FdKind::Blob {{ size_arg: {si}, offset_arg: {oi:?} }})")
                    }
                })
                .unwrap_or_else(|| "None".to_string());
            let rw = REWRITES
                .iter()
                .find(|(pi, pm, _)| *pi == i.name && *pm == m.name)
                .map(|(_, _, r)| match r {
                    Rewrite::DevT { arg } => format!(
                        "Some(RewriteKind::DevT({}))",
                        arg_index(&i.name, m, arg, "array")
                    ),
                    Rewrite::ClockId { arg } => {
                        format!(
                            "Some(RewriteKind::ClockId({}))",
                            arg_index(&i.name, m, arg, "uint")
                        )
                    }
                    Rewrite::Timestamp {
                        sec_hi,
                        sec_lo,
                        nsec,
                    } => format!(
                        "Some(RewriteKind::Timestamp {{ sec_hi: {}, sec_lo: {}, nsec: {} }})",
                        arg_index(&i.name, m, sec_hi, "uint"),
                        arg_index(&i.name, m, sec_lo, "uint"),
                        arg_index(&i.name, m, nsec, "uint")
                    ),
                })
                .unwrap_or_else(|| "None".to_string());
            write!(
                o,
                "Message {{ name: {:?}, since: {}, destructor: {}, args: {args}, nfds: {nfds}, fd: {fd}, rewrite: {rw} }},",
                m.name, m.since, m.destructor
            )
            .unwrap();
        }
        o.push(']');
        o
    };

    writeln!(s, "pub static INTERFACES: [Interface; INTERFACE_COUNT] = [").unwrap();
    for i in &ifaces {
        writeln!(
            s,
            "  Interface {{ name: {:?}, version: {}, requests: {}, events: {} }},",
            i.name,
            i.version,
            gen_msgs(i, &i.requests),
            gen_msgs(i, &i.events)
        )
        .unwrap();
    }
    writeln!(s, "];").unwrap();

    writeln!(
        s,
        "pub fn iface_by_name(name: &[u8]) -> Option<IfaceId> {{ match name {{"
    )
    .unwrap();
    for (n, i) in ifaces.iter().enumerate() {
        writeln!(s, "  b{:?} => Some({n}),", i.name).unwrap();
    }
    writeln!(s, "  _ => None, }} }}").unwrap();

    writeln!(s, "#[allow(non_snake_case, dead_code)] pub mod op {{").unwrap();
    for i in &ifaces {
        writeln!(s, "  pub mod {} {{", i.name).unwrap();
        for (n, m) in i.requests.iter().enumerate() {
            writeln!(s, "    pub const REQ_{}: u16 = {n};", upper(&m.name)).unwrap();
        }
        for (n, m) in i.events.iter().enumerate() {
            writeln!(s, "    pub const EVT_{}: u16 = {n};", upper(&m.name)).unwrap();
        }
        writeln!(s, "  }}").unwrap();
    }
    writeln!(s, "}}").unwrap();

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("protocols.rs");
    std::fs::write(out, s).unwrap();
}
