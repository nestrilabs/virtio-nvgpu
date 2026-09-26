//! The Wayland wire format: one message is `u32 object`, `u32 (size << 16 |
//! opcode)`, then the arguments, every one a multiple of four bytes, in host
//! byte order (libwayland `connection.c`). Strings and arrays are a u32 length
//! and padded bytes; a string's length counts its NUL and 0 means null.
//! Descriptors take no bytes at all: they travel beside the stream, in one
//! FIFO per connection, and a message consumes as many as its signature has
//! `fd` arguments -- which is why nothing here can skip a message it does not
//! understand.

#![forbid(unsafe_code)]

use crate::proto::{Arg, ArgKind, Message};

/// libwayland refuses anything larger, on both ends (`WL_MAX_MESSAGE_SIZE`).
pub const MAX_MSG: usize = 4096;
/// Ids from here up are allocated by the server (`WL_SERVER_ID_START`).
pub const SERVER_ID_START: u32 = 0xff00_0000;
/// The most descriptors libwayland reads per `recvmsg` (`MAX_FDS_OUT`, and
/// the `CLEN` control buffer the receiver sizes from it). More in one
/// `sendmsg` to a libwayland peer are truncated.
pub const MAX_FDS_PER_SENDMSG: usize = 28;
/// The most descriptors a proxy holds from its local peer that no message has
/// taken yet: libwayland's own input ring (`wl_connection.fds_in`, 4096 bytes
/// of them), past which it closes the connection. Descriptors cost nothing to
/// send and are only taken by the messages that carry them, so without this a
/// peer sending them beside messages that carry none makes the proxy hold
/// them for ever, to its descriptor limit and every other client's cost.
pub const MAX_FDS_QUEUED: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub object: u32,
    pub opcode: u16,
    pub size: u16,
}

/// The header at the start of `buf`, if 8 bytes are there. The size is not
/// validated here.
pub fn peek_header(buf: &[u8]) -> Option<Header> {
    if buf.len() < 8 {
        return None;
    }
    let w = u32::from_ne_bytes(buf[4..8].try_into().unwrap());
    Some(Header {
        object: u32::from_ne_bytes(buf[0..4].try_into().unwrap()),
        opcode: (w & 0xffff) as u16,
        size: (w >> 16) as u16,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Size field below 8, above 4096, or not a multiple of 4.
    BadSize,
    /// The arguments ran past the message's size.
    Short,
    /// Bytes left over after the last argument.
    Trailing,
    /// A string with no NUL where its length says, or a NUL inside it.
    BadString,
    /// A null string, object or new_id where the signature does not allow one.
    Null,
}

/// One decoded argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Val<'a> {
    Int(i32),
    Uint(u32),
    Fixed(i32),
    /// Without the NUL. `None` is a null string.
    Str(Option<&'a [u8]>),
    Object(u32),
    /// `iface`/`version` only for the untyped new_id of `wl_registry.bind`.
    NewId {
        id: u32,
        iface: Option<&'a [u8]>,
        version: u32,
    },
    Array(&'a [u8]),
    Fd,
}

/// An argument and where it sits: `off` is the byte offset, from the start of
/// the whole message, of its first word (for strings and arrays, the length;
/// for an untyped new_id, the interface string's length).
#[derive(Clone, Copy, Debug)]
pub struct At<'a> {
    pub off: usize,
    pub val: Val<'a>,
}

fn word(buf: &[u8], off: usize) -> Result<u32, WireError> {
    buf.get(off..off + 4)
        .map(|b| u32::from_ne_bytes(b.try_into().unwrap()))
        .ok_or(WireError::Short)
}

fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

fn string(buf: &[u8], off: usize) -> Result<(Option<&[u8]>, usize), WireError> {
    let len = word(buf, off)? as usize;
    if len == 0 {
        return Ok((None, off + 4));
    }
    let end = off + 4 + len;
    let body = buf.get(off + 4..end).ok_or(WireError::Short)?;
    if body[len - 1] != 0 || body[..len - 1].contains(&0) {
        return Err(WireError::BadString);
    }
    let next = off + 4 + pad4(len);
    if next > buf.len() {
        return Err(WireError::Short);
    }
    Ok((Some(&body[..len - 1]), next))
}

/// Decode every argument of `msg` (the whole message, header included) against
/// its signature. Checks the size field, that the arguments fill the message
/// exactly, and nullability.
pub fn parse<'a>(desc: &Message, msg: &'a [u8]) -> Result<Vec<At<'a>>, WireError> {
    let h = peek_header(msg).ok_or(WireError::Short)?;
    let size = h.size as usize;
    if !(8..=MAX_MSG).contains(&size) || !size.is_multiple_of(4) || size != msg.len() {
        return Err(WireError::BadSize);
    }
    let mut out = Vec::with_capacity(desc.args.len());
    let mut off = 8;
    for a in desc.args {
        let at = off;
        let val = match a.kind {
            ArgKind::Int => {
                off += 4;
                Val::Int(word(msg, at)? as i32)
            }
            ArgKind::Uint => {
                off += 4;
                Val::Uint(word(msg, at)?)
            }
            ArgKind::Fixed => {
                off += 4;
                Val::Fixed(word(msg, at)? as i32)
            }
            ArgKind::Str => {
                let (s, next) = string(msg, at)?;
                if s.is_none() && !a.nullable {
                    return Err(WireError::Null);
                }
                off = next;
                Val::Str(s)
            }
            ArgKind::Object => {
                let id = word(msg, at)?;
                if id == 0 && !a.nullable {
                    return Err(WireError::Null);
                }
                off += 4;
                Val::Object(id)
            }
            ArgKind::NewId if a.iface.is_some() => {
                let id = word(msg, at)?;
                if id == 0 {
                    return Err(WireError::Null);
                }
                off += 4;
                Val::NewId {
                    id,
                    iface: None,
                    version: 0,
                }
            }
            ArgKind::NewId => {
                let (name, next) = string(msg, at)?;
                let name = name.ok_or(WireError::Null)?;
                let version = word(msg, next)?;
                let id = word(msg, next + 4)?;
                if id == 0 {
                    return Err(WireError::Null);
                }
                off = next + 8;
                Val::NewId {
                    id,
                    iface: Some(name),
                    version,
                }
            }
            ArgKind::Array => {
                let len = word(msg, at)? as usize;
                let body = msg.get(at + 4..at + 4 + len).ok_or(WireError::Short)?;
                off = at + 4 + pad4(len);
                if off > msg.len() {
                    return Err(WireError::Short);
                }
                Val::Array(body)
            }
            ArgKind::Fd => Val::Fd,
        };
        out.push(At { off: at, val });
    }
    if off != msg.len() {
        return Err(WireError::Trailing);
    }
    Ok(out)
}

/// Overwrite the u32 argument word at `off` in a message.
pub fn put_word(msg: &mut [u8], off: usize, v: u32) {
    msg[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

/// Builds one message. Used to synthesise the few messages the proxy itself
/// originates (`wl_display.error`, a missing `released`), and by tests.
pub struct MsgBuilder {
    buf: Vec<u8>,
}

impl MsgBuilder {
    pub fn new(object: u32, opcode: u16) -> Self {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&object.to_ne_bytes());
        buf.extend_from_slice(&(opcode as u32).to_ne_bytes());
        Self { buf }
    }
    pub fn uint(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_ne_bytes());
        self
    }
    pub fn int(self, v: i32) -> Self {
        self.uint(v as u32)
    }
    pub fn fixed(self, v: f64) -> Self {
        self.int((v * 256.0) as i32)
    }
    pub fn object(self, id: u32) -> Self {
        self.uint(id)
    }
    pub fn new_id(self, id: u32) -> Self {
        self.uint(id)
    }
    /// A null string when `None`.
    pub fn string(mut self, s: Option<&str>) -> Self {
        match s {
            None => self.uint(0),
            Some(s) => {
                self.buf
                    .extend_from_slice(&((s.len() + 1) as u32).to_ne_bytes());
                self.buf.extend_from_slice(s.as_bytes());
                self.buf.push(0);
                while !self.buf.len().is_multiple_of(4) {
                    self.buf.push(0);
                }
                self
            }
        }
    }
    pub fn array(mut self, a: &[u8]) -> Self {
        self.buf.extend_from_slice(&(a.len() as u32).to_ne_bytes());
        self.buf.extend_from_slice(a);
        while !self.buf.len().is_multiple_of(4) {
            self.buf.push(0);
        }
        self
    }
    /// `wl_registry.bind`'s untyped new_id.
    pub fn generic_new_id(self, iface: &str, version: u32, id: u32) -> Self {
        self.string(Some(iface)).uint(version).uint(id)
    }
    pub fn finish(mut self) -> Vec<u8> {
        let size = self.buf.len() as u32;
        let w = u32::from_ne_bytes(self.buf[4..8].try_into().unwrap());
        self.buf[4..8].copy_from_slice(&((size << 16) | (w & 0xffff)).to_ne_bytes());
        self.buf
    }
}

/// Signature check helper for the few places that look at a message by name.
pub fn arg_index(desc: &Message, name: &str) -> Option<usize> {
    desc.args.iter().position(|a: &Arg| a.name == name)
}
