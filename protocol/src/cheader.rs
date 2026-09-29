// SPDX-License-Identifier: BSD-3-Clause OR GPL-2.0-or-later
//! A reader for the C headers the wire is declared in (`driver/nvgpu_wire.h`,
//! `driver/uapi/nvgpu_wl.h`), for the tests that hold their Rust mirrors to
//! them: every integer `#define`, and every struct's size and field offsets
//! as a C compiler lays it out for x86-64.
//!
//! It reads the subset those headers are written in, and panics on anything
//! else, so a header that grows a construct it does not know fails the tests
//! rather than being skipped:
//!
//! - `#define NAME expr`, where `expr` is integers (decimal or hex, with
//!   `u`/`l` suffixes), names defined above it, `( )`, `+`, `*`, `<<`, `|`
//!   and `_IO`/`_IOR`/`_IOW`/`_IOWR` of a character, a number and a struct.
//!   A define with no value (an include guard) has none. A function-like
//!   macro is left out.
//! - `struct name { fields } [__packed];`, where a field is a fixed-width
//!   kernel type (`__u8` .. `__le64`, `__s32`, `char`) or a struct declared
//!   above it, optionally an array of a constant length.
//!
//! Only for tests (the `cheader` feature); it allocates, where the rest of
//! this crate is `no_std`.

extern crate std;

use std::collections::BTreeMap;
use std::string::{String, ToString};
use std::vec::Vec;

/// One struct as C lays it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub size: usize,
    pub align: usize,
    /// Each field's name and byte offset, in declaration order.
    pub fields: Vec<(String, usize)>,
}

impl Layout {
    /// The offset of `field`; panics if there is none.
    pub fn offset(&self, field: &str) -> usize {
        self.fields
            .iter()
            .find(|(n, _)| n == field)
            .unwrap_or_else(|| panic!("no field {field}"))
            .1
    }
}

/// The defines and structs of one header.
#[derive(Debug, Default)]
pub struct Header {
    /// Every define with an integer value.
    pub defines: BTreeMap<String, u64>,
    /// Every define with no value (include guards).
    pub bare: Vec<String>,
    pub structs: BTreeMap<String, Layout>,
}

impl Header {
    pub fn parse(text: &str) -> Header {
        let text = strip_comments(text);
        let mut h = Header::default();
        // Join continuation lines, then read top-level items in order: a
        // define may name a struct declared above it, and a struct a define.
        let joined = text.replace("\\\n", " ");
        let mut rest = joined.as_str();
        while let Some(pos) = next_item(rest) {
            rest = &rest[pos..];
            if let Some(line) = rest.strip_prefix("#define") {
                let end = line.find('\n').unwrap_or(line.len());
                h.read_define(line[..end].trim());
                rest = &line[end..];
            } else {
                let close = rest.find('}').expect("struct without }");
                let end = close + rest[close..].find(';').expect("struct without ;") + 1;
                h.read_struct(&rest[..end]);
                rest = &rest[end..];
            }
        }
        h
    }

    /// The value of `name`; panics if the header has none.
    pub fn define(&self, name: &str) -> u64 {
        *self
            .defines
            .get(name)
            .unwrap_or_else(|| panic!("{name} not defined"))
    }

    /// The layout of `struct name`; panics if the header has none.
    pub fn layout(&self, name: &str) -> &Layout {
        self.structs
            .get(name)
            .unwrap_or_else(|| panic!("struct {name} not declared"))
    }

    fn read_define(&mut self, body: &str) {
        // A function-like macro is text, not a value.
        if body
            .split_whitespace()
            .next()
            .is_some_and(|n| n.contains('('))
        {
            return;
        }
        let (name, value) = match body.split_once(char::is_whitespace) {
            Some((n, v)) => (n, v.trim()),
            None => (body, ""),
        };
        if value.is_empty() {
            self.bare.push(name.to_string());
            return;
        }
        let v = Expr::new(value, self).eval();
        self.defines.insert(name.to_string(), v);
    }

    fn read_struct(&mut self, text: &str) {
        let text = text.trim().strip_prefix("struct").expect("struct").trim();
        let (name, body) = text.split_once('{').expect("struct body");
        let (body, tail) = body.rsplit_once('}').expect("struct end");
        let packed = match tail.trim().trim_end_matches(';').trim() {
            "" => false,
            "__packed" => true,
            t => panic!("struct {name}: unknown attribute {t}"),
        };
        let (mut off, mut align) = (0usize, 1usize);
        let mut fields = Vec::new();
        for decl in body.split(';').map(str::trim).filter(|d| !d.is_empty()) {
            let (ty, declarator) = decl.rsplit_once(char::is_whitespace).expect("field");
            let (fname, count) = match declarator.split_once('[') {
                Some((n, dim)) => {
                    let dim = dim.strip_suffix(']').expect("array ]");
                    (n, Expr::new(dim, self).eval() as usize)
                }
                None => (declarator, 1),
            };
            let (size, falign) = self.type_layout(ty.trim());
            let falign = if packed { 1 } else { falign };
            off = off.next_multiple_of(falign);
            fields.push((fname.to_string(), off));
            off += size * count;
            align = align.max(falign);
        }
        let size = off.next_multiple_of(align);
        self.structs.insert(
            name.trim().to_string(),
            Layout {
                size,
                align,
                fields,
            },
        );
    }

    fn type_layout(&self, ty: &str) -> (usize, usize) {
        if let Some(s) = ty.strip_prefix("struct") {
            let l = self.layout(s.trim());
            return (l.size, l.align);
        }
        let n = match ty {
            "char" | "__u8" | "__s8" | "u8" => 1,
            "__u16" | "__s16" | "__le16" => 2,
            "__u32" | "__s32" | "__le32" => 4,
            "__u64" | "__s64" | "__le64" => 8,
            _ => panic!("unknown field type {ty}"),
        };
        (n, n)
    }
}

/// Where the next `#define` or top-level `struct ... {` starts in `s`.
fn next_item(s: &str) -> Option<usize> {
    let mut at = 0;
    for line in s.split_inclusive('\n') {
        let t = line.trim_start();
        if t.starts_with("#define") {
            return Some(at + line.len() - t.len());
        }
        if t.starts_with("struct ") && t.trim_end().ends_with('{') {
            return Some(at + line.len() - t.len());
        }
        at += line.len();
    }
    None
}

fn strip_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("/*") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find("*/").expect("unterminated comment");
        // Keep line structure, so a define ends where it did.
        out.extend(rest[i..i + end].chars().filter(|&c| c == '\n'));
        rest = &rest[i + end + 2..];
    }
    out.push_str(rest);
    // `//` comments, to the end of the line.
    out.lines()
        .map(|l| l.split_once("//").map_or(l, |(a, _)| a))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A define's value: a small recursive-descent reader over `|`, `<<`, `+`,
/// `*`, parentheses, numbers, earlier defines and the `_IO*` macros.
struct Expr<'a> {
    toks: Vec<&'a str>,
    at: usize,
    h: &'a Header,
}

impl<'a> Expr<'a> {
    fn new(s: &'a str, h: &'a Header) -> Self {
        let mut toks = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace() {
                i += 1;
            } else if c.is_ascii_alphanumeric() || c == b'_' {
                let j = i + b[i..]
                    .iter()
                    .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .unwrap_or(b.len() - i);
                toks.push(&s[i..j]);
                i = j;
            } else if c == b'\'' {
                toks.push(&s[i..i + 3]);
                i += 3;
            } else if s[i..].starts_with("<<") {
                toks.push("<<");
                i += 2;
            } else {
                toks.push(&s[i..i + 1]);
                i += 1;
            }
        }
        Self { toks, at: 0, h }
    }

    fn eval(mut self) -> u64 {
        let v = self.or();
        assert_eq!(
            self.at,
            self.toks.len(),
            "trailing tokens in {:?}",
            self.toks
        );
        v
    }

    fn peek(&self) -> Option<&'a str> {
        self.toks.get(self.at).copied()
    }

    fn take(&mut self, want: &str) {
        assert_eq!(self.peek(), Some(want), "in {:?}", self.toks);
        self.at += 1;
    }

    fn or(&mut self) -> u64 {
        let mut v = self.shift();
        while self.peek() == Some("|") {
            self.at += 1;
            v |= self.shift();
        }
        v
    }

    fn shift(&mut self) -> u64 {
        let mut v = self.sum();
        while self.peek() == Some("<<") {
            self.at += 1;
            v <<= self.sum();
        }
        v
    }

    fn sum(&mut self) -> u64 {
        let mut v = self.product();
        while self.peek() == Some("+") {
            self.at += 1;
            v += self.product();
        }
        v
    }

    fn product(&mut self) -> u64 {
        let mut v = self.atom();
        while self.peek() == Some("*") {
            self.at += 1;
            v *= self.atom();
        }
        v
    }

    fn atom(&mut self) -> u64 {
        let t = self.peek().expect("expression ends early");
        self.at += 1;
        if t == "(" {
            let v = self.or();
            self.take(")");
            return v;
        }
        if let Some(dir) = match t {
            "_IO" => Some(0),
            "_IOW" => Some(1),
            "_IOR" => Some(2),
            "_IOWR" => Some(3),
            _ => None,
        } {
            self.take("(");
            let ty = self.peek().expect("ioctl type");
            let ty = u64::from(ty.as_bytes()[1]);
            self.at += 1;
            self.take(",");
            let nr = self.or();
            let size = if dir == 0 {
                0
            } else {
                self.take(",");
                self.take("struct");
                let s = self.peek().expect("ioctl struct");
                self.at += 1;
                self.h.layout(s).size as u64
            };
            self.take(")");
            return (dir << 30) | (size << 16) | (ty << 8) | nr;
        }
        if t.as_bytes()[0].is_ascii_digit() {
            let t = t.trim_end_matches(['u', 'U', 'l', 'L']);
            return match t.strip_prefix("0x") {
                Some(x) => u64::from_str_radix(x, 16),
                None => t.parse(),
            }
            .unwrap_or_else(|_| panic!("bad number {t}"));
        }
        self.h.define(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_defines_and_lays_structs_out_as_c_does() {
        let h = Header::parse(
            "#ifndef G\n#define G\n/* a\n * comment */\n#define A 3 /* x */\n\
             #define B (1u << A)\n#define C (16 + 2 * 4)\n#define D 0x10u\n\
             struct p {\n  __le32 a;\n  __u8 b; /* c */\n  __le64 c[A];\n} __packed;\n\
             struct n {\n  __u8 a;\n  __s64 b;\n  __u16 c;\n  struct p d[2];\n};\n\
             #define E _IOWR('W', 0x42, struct n)\n",
        );
        assert_eq!(h.bare, ["G"]);
        assert_eq!(h.define("A"), 3);
        assert_eq!(h.define("B"), 8);
        assert_eq!(h.define("C"), 24);
        assert_eq!(h.define("D"), 16);
        let p = h.layout("p");
        assert_eq!(
            (p.size, p.align, p.offset("b"), p.offset("c")),
            (29, 1, 4, 5)
        );
        let n = h.layout("n");
        assert_eq!((n.offset("b"), n.offset("c"), n.offset("d")), (8, 16, 18));
        assert_eq!((n.size, n.align), (80, 8));
        assert_eq!(
            h.define("E"),
            (3 << 30) | (80 << 16) | (u64::from(b'W') << 8) | 0x42
        );
    }
}
