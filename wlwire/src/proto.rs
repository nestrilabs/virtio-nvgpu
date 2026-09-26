// SPDX-License-Identifier: Apache-2.0
//! The protocol tables, generated at build time from `protocols/*.xml` (see
//! `build.rs`), and the types they are made of.

#![forbid(unsafe_code)]

/// Index into [`INTERFACES`].
pub type IfaceId = u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgKind {
    Int,
    Uint,
    Fixed,
    Str,
    Object,
    /// Typed when `Arg::iface` is set (one u32 on the wire); untyped otherwise
    /// (`wl_registry.bind`: interface string, version, id).
    NewId,
    Array,
    Fd,
}

#[derive(Clone, Copy, Debug)]
pub struct Arg {
    pub name: &'static str,
    pub kind: ArgKind,
    pub nullable: bool,
    pub iface: Option<IfaceId>,
}

/// A descriptor class with its argument positions resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdKind {
    ShmPool,
    Dmabuf,
    Blob {
        size_arg: u8,
        offset_arg: Option<u8>,
    },
    Stream,
    DrmFile,
    Syncobj,
}

/// A value rewrite with its argument positions resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RewriteKind {
    DevT(u8),
    Timestamp { sec_hi: u8, sec_lo: u8, nsec: u8 },
    ClockId(u8),
}

#[derive(Clone, Copy, Debug)]
pub struct Message {
    pub name: &'static str,
    pub since: u32,
    pub destructor: bool,
    pub args: &'static [Arg],
    pub nfds: u8,
    pub fd: Option<FdKind>,
    pub rewrite: Option<RewriteKind>,
}

#[derive(Debug)]
pub struct Interface {
    pub name: &'static str,
    pub version: u32,
    pub requests: &'static [Message],
    pub events: &'static [Message],
}

include!(concat!(env!("OUT_DIR"), "/protocols.rs"));

/// The interface a generated id stands for.
pub fn iface(id: IfaceId) -> &'static Interface {
    &INTERFACES[id as usize]
}

/// Which way a message travels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Client to server.
    Request,
    /// Server to client.
    Event,
}

impl Interface {
    pub fn messages(&self, dir: Dir) -> &'static [Message] {
        match dir {
            Dir::Request => self.requests,
            Dir::Event => self.events,
        }
    }
}
