//! The IOCTL2 schema: for every ioctl a guest may run through IOCTL2, what
//! its argument points at and where its descriptors and GEM handles are.
//!
//! The tables are generated (`generated.rs`, from `gen/schema/*.py` by
//! `gen/schema_gen.py`, the same run that writes the guest's
//! `driver/gen/nvgpu_schema.h`); this module defines what an entry means. The
//! interpreter that walks one over a request is `device/src/xfer.rs`, and the
//! language, including the canonical traversal both sides share, is
//! documented in `gen/schema/lang.py`.

#[rustfmt::skip]
mod generated;

pub use generated::{DRM_TABLE, MODESET_TABLES, MULTI_PLANE_FORMATS};

use crate::version::DriverVersion;

/// `FdIn` kind bits: bit n is the protocol's `HK_*` value n (so `HK_DEV`'s
/// bit means "any of our devices"), and these name one device each.
pub const K_DEV_CTL: u32 = 1 << 16;
pub const K_DEV_MODESET: u32 = 1 << 17;
pub const K_DEV_GPU: u32 = 1 << 18;

/// `_IOWR('m', 0, struct NvKmsIoctlParams)`, the only NVKMS ioctl.
pub const NVKMS_IOCTL_IOWR: u32 = 0xc010_6d00;

/// Which kind of host file an entry runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Class {
    Render,
    Kms,
    Modeset,
}

/// Where the backend runs a call: inline on the queue thread, or on the
/// host file's serial executor (anything that can wait on a modeset lock or
/// `nvkms_lock`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exec {
    Inline,
    Executor,
}

/// Which way a buffer's bytes travel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    In,
    Out,
    InOut,
}

impl Dir {
    /// The caller's bytes are sent.
    pub fn has_in(self) -> bool {
        matches!(self, Dir::In | Dir::InOut)
    }

    /// The host's bytes come back.
    pub fn has_out(self) -> bool {
        matches!(self, Dir::Out | Dir::InOut)
    }
}

/// A field exists only when `(u32 at off & mask) == value`, `off` in the
/// same struct as the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cond {
    pub off: u32,
    pub mask: u32,
    pub value: u32,
}

/// How many bytes a pointer reaches, from the IN bytes of its struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Len {
    /// A fixed length; `Const(0)` is a pointer the host always sees as NULL.
    Const(u32),
    /// The `width`-byte unsigned count at `off`, times `elem`.
    Count { off: u32, width: u8, elem: u32 },
    /// Σ of the u32 elements of the buffer field `field` (an absolute index
    /// into the table's fields, an earlier sibling) created, times `elem`.
    Sum { field: u16, elem: u32 },
    /// `NvKmsIoctlParams.size` (u32 @4), which must equal the entry's cap.
    NvkmsParams,
}

/// What of a buffer the host wrote reaches the caller. The backend always
/// returns every OUT byte; this is the guest's rule, and mirrors what the
/// kernel would have written into the caller's memory. Counts are `in` (the
/// value the caller sent) and `out` (the value the host left) of the field at
/// `off` in the pointer's struct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyBack {
    None,
    /// All of it: OUT-only buffers on success, IN/OUT buffers always.
    Full,
    /// On success, min(in, out) elements.
    Partial {
        off: u32,
        width: u8,
        elem: u32,
    },
    /// On success, `out` elements if out <= in, else none.
    AllOrNothing {
        off: u32,
        width: u8,
        elem: u32,
    },
    /// On success, all of it if out == in, else none.
    Exact {
        off: u32,
        width: u8,
    },
    /// Bytes [off, off + len), always.
    Range {
        off: u32,
        len: u32,
    },
}

impl CopyBack {
    /// The bytes `[start, end)` of a `len`-byte buffer of direction `dir`
    /// that reach the caller after a call that returned `ret`, given the
    /// rule's count as the caller sent it (`sent`) and as the host left it
    /// (`left`); rules without a count ignore both.
    ///
    /// This is the definition; the guest's `nvgpu_i2_copy_extent()` is a
    /// line-for-line mirror of it, and the tests below are the kernel's fill
    /// rules (R:drmcore §2) stated as cases.
    pub fn extent(self, dir: Dir, len: u64, ret: i32, sent: u64, left: u64) -> (u64, u64) {
        let ok = ret == 0;
        let n = match self {
            CopyBack::None => 0,
            // An IN/OUT buffer holds the caller's own bytes wherever the
            // kernel wrote nothing, so it may always go back; an OUT-only one
            // holds our zeroes, which may not.
            CopyBack::Full if ok || dir == Dir::InOut => len,
            CopyBack::Full => 0,
            CopyBack::Partial { elem, .. } if ok => sent.min(left).saturating_mul(u64::from(elem)),
            CopyBack::AllOrNothing { elem, .. } if ok && left <= sent => {
                left.saturating_mul(u64::from(elem))
            }
            CopyBack::Exact { .. } if ok && left == sent => len,
            CopyBack::Partial { .. } | CopyBack::AllOrNothing { .. } | CopyBack::Exact { .. } => 0,
            CopyBack::Range { off, len: n } => {
                let start = u64::from(off).min(len);
                return (start, (u64::from(off) + u64::from(n)).min(len));
            }
        };
        (0, n.min(len))
    }
}

/// A contiguous run of the table's fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub first: u16,
    pub len: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A u64 user pointer. `children` are the fields of each `stride`-byte
    /// element of what it points at.
    Ptr {
        dir: Dir,
        len: Len,
        copyback: CopyBack,
        max: u32,
        stride: u32,
        children: Span,
    },
    /// `count` inline elements of `stride` bytes, walked in place.
    Array {
        count: u32,
        stride: u32,
        children: Span,
    },
    /// A descriptor the caller passes: a backend handle of one of `kinds`,
    /// or `none`.
    FdIn { width: u8, kinds: u32, none: i32 },
    /// A descriptor the host creates.
    FdOut { width: u8 },
    /// A GEM handle the caller passes (u32).
    GemIn { validate_nvkms: bool },
    /// A GEM handle the host creates (u32).
    GemOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub off: u32,
    pub cond: Option<Cond>,
    pub kind: Kind,
}

impl Field {
    /// Bytes the field occupies in its struct.
    pub fn width(&self) -> u32 {
        match self.kind {
            Kind::Ptr { .. } => 8,
            Kind::Array { count, stride, .. } => count * stride,
            Kind::FdIn { width, .. } | Kind::FdOut { width } => width as u32,
            Kind::GemIn { .. } | Kind::GemOut => 4,
        }
    }
}

/// Code on both sides, named by the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    None,
    /// DRM ATOMIC: fence and pointer properties in prop_values.
    Atomic,
    /// NVKMS: buffer 0 is `NvKmsIoctlParams`; the entry is keyed by its `cmd`.
    NvkmsParams,
}

/// Backend-only checks an entry opts into (`Ioctl::policy` bits).
pub mod policy {
    /// Fence schemas: the FENCES hook decides.
    pub const FENCE: u32 = 1 << 0;
    /// nvidia-drm GRANT_PERMISSIONS: MODESET grants only.
    pub const GRANT: u32 = 1 << 1;
    /// nvidia-drm REVOKE_PERMISSIONS: MODESET revocations only.
    pub const REVOKE: u32 = 1 << 2;
    /// The u32 fb_id @0 becomes one of the file's own framebuffers.
    pub const FB_CREATE: u32 = 1 << 3;
    /// The u32 fb_id @0 stops being one.
    pub const FB_REMOVE: u32 = 1 << 4;
    /// GEM handles come back only for the file's own framebuffers.
    pub const FB_READ: u32 = 1 << 5;
    /// Legacy property set: never an fd or pointer property.
    pub const SETPROP: u32 = 1 << 6;
    /// ADDFB2: the handles pixel_format does not use must be 0.
    pub const FB_PLANES: u32 = 1 << 7;
    /// An NVKMS command: the NVKMS hook decides.
    pub const NVKMS: u32 = 1 << 8;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ioctl {
    pub name: &'static str,
    pub class: Class,
    /// The full ioctl number, direction and size included.
    pub cmd: u32,
    /// Modeset entries: the NVKMS command inside the outer struct.
    pub nvkms_cmd: u32,
    /// `_IOC_SIZE(cmd)`.
    pub size: u32,
    pub exec: Exec,
    pub special: Special,
    pub policy: u32,
    /// The host never writes the argument (NVKMS's outer struct), so none of
    /// it goes back.
    pub arg_in_only: bool,
    pub fields: Span,
}

impl Ioctl {
    /// Direction of buffer 0, from the ioctl number's direction bits.
    pub fn arg_dir(&self) -> Option<Dir> {
        let w = self.cmd & (1 << 30) != 0;
        let r = self.cmd & (1 << 31) != 0 && !self.arg_in_only;
        match (w, r) {
            (true, true) => Some(Dir::InOut),
            (true, false) => Some(Dir::In),
            (false, true) => Some(Dir::Out),
            (false, false) => None,
        }
    }
}

pub struct Table {
    pub name: &'static str,
    /// Host driver versions the table is for, inclusive; None for any.
    pub versions: Option<(DriverVersion, DriverVersion)>,
    pub ioctls: &'static [Ioctl],
    pub fields: &'static [Field],
}

impl Table {
    pub fn fields(&self, s: Span) -> &'static [Field] {
        &self.fields[s.first as usize..s.first as usize + s.len as usize]
    }

    /// The entry for (class, `_IOC_TYPE`, `_IOC_NR`). Size and direction are
    /// the caller's to compare: a known number with the wrong size is a
    /// different refusal (-EINVAL) from an unknown one (-ENOTTY).
    pub fn lookup(&self, class: Class, cmd: u32) -> Option<&'static Ioctl> {
        self.ioctls
            .iter()
            .find(|e| e.class == class && e.cmd & 0xffff == cmd & 0xffff)
    }

    /// The Modeset entry for an NVKMS command.
    pub fn lookup_nvkms(&self, nvkms_cmd: u32) -> Option<&'static Ioctl> {
        self.ioctls
            .iter()
            .find(|e| e.class == Class::Modeset && e.nvkms_cmd == nvkms_cmd)
    }
}

/// The NVKMS table for a host driver version, if one was generated for it.
pub fn modeset_table(v: DriverVersion) -> Option<&'static Table> {
    MODESET_TABLES
        .iter()
        .copied()
        .find(|t| t.versions.is_some_and(|(lo, hi)| lo <= v && v <= hi))
}

/// How many planes (and so GEM handles) a pixel format has.
pub fn format_planes(fourcc: u32) -> u32 {
    MULTI_PLANE_FORMATS
        .iter()
        .find(|(f, _)| *f == fourcc)
        .map_or(1, |&(_, n)| n as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The checked-in C and Rust tables are what the Python source says.
    /// Both halves interpret "the same schema" only if nobody edited one
    /// without regenerating, so this regenerates into a temporary directory
    /// and compares byte for byte.
    #[test]
    fn the_checked_in_tables_are_what_the_generator_writes() {
        let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let repo = gen_dir.parent().expect("gen/ has a parent");
        let out = std::env::temp_dir().join(format!("schema-gen-{}", std::process::id()));
        let status = Command::new("python3")
            .arg(gen_dir.join("schema_gen.py"))
            .arg("--out")
            .arg(&out)
            .status()
            .expect("python3 runs (the schema generator needs it)");
        assert!(status.success(), "schema_gen.py failed");
        for rel in ["driver/gen/nvgpu_schema.h", "gen/src/schema/generated.rs"] {
            let fresh = std::fs::read(out.join(rel)).expect("generated");
            let checked_in = std::fs::read(repo.join(rel)).expect("checked in");
            assert!(
                fresh == checked_in,
                "{rel} is stale: run gen/schema_gen.py and commit the result"
            );
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn every_entry_is_found_by_its_own_number() {
        for e in DRM_TABLE.ioctls {
            let found = DRM_TABLE.lookup(e.class, e.cmd).expect("found");
            assert_eq!(found.name, e.name);
            assert_eq!((e.cmd >> 16) & 0x3fff, e.size, "{}", e.name);
        }
    }

    #[test]
    fn nothing_the_design_refuses_has_an_entry() {
        let refused = [
            (Class::Render, 0x42), // GEM_IMPORT_USERSPACE_MEMORY
            (Class::Kms, 0x09),    // GEM_CLOSE
            (Class::Kms, 0x0a),    // GEM_FLINK
            (Class::Kms, 0x0b),    // GEM_OPEN
            (Class::Kms, 0x2d),    // PRIME_HANDLE_TO_FD
            (Class::Kms, 0x2e),    // PRIME_FD_TO_HANDLE
            (Class::Kms, 0x1e),    // SET_MASTER
            (Class::Kms, 0x1f),    // DROP_MASTER
            (Class::Kms, 0x11),    // AUTH_MAGIC
            (Class::Kms, 0xb3),    // MAP_DUMB
            (Class::Render, 0x09), // GEM_CLOSE
        ];
        for (class, nr) in refused {
            let cmd = (b'd' as u32) << 8 | nr;
            assert!(DRM_TABLE.lookup(class, cmd).is_none(), "{class:?} {nr:#x}");
        }
    }

    const P4: CopyBack = CopyBack::Partial {
        off: 0,
        width: 4,
        elem: 4,
    };
    const A68: CopyBack = CopyBack::AllOrNothing {
        off: 0,
        width: 4,
        elem: 68,
    };
    const EXACT: CopyBack = CopyBack::Exact { off: 0, width: 4 };

    #[test]
    fn a_partial_fill_returns_what_fit_and_nothing_on_failure() {
        // GETRESOURCES: 2 slots offered, 5 exist: 2 written.
        assert_eq!(P4.extent(Dir::Out, 8, 0, 2, 5), (0, 8));
        // 5 offered, 2 exist: 2 written.
        assert_eq!(P4.extent(Dir::Out, 20, 0, 5, 2), (0, 8));
        assert_eq!(P4.extent(Dir::Out, 20, -EINVAL, 5, 2), (0, 0));
    }

    #[test]
    fn all_or_nothing_returns_everything_only_when_everything_fit() {
        // GETCONNECTOR modes: 3 offered, 2 exist: both written.
        assert_eq!(A68.extent(Dir::Out, 204, 0, 3, 2), (0, 136));
        // 1 offered, 2 exist: nothing written, only the count.
        assert_eq!(A68.extent(Dir::Out, 68, 0, 1, 2), (0, 0));
    }

    #[test]
    fn exact_returns_the_blob_only_when_the_length_matched() {
        assert_eq!(EXACT.extent(Dir::Out, 128, 0, 128, 128), (0, 128));
        assert_eq!(EXACT.extent(Dir::Out, 64, 0, 64, 128), (0, 0));
    }

    #[test]
    fn full_returns_an_out_buffer_only_on_success_and_an_inout_one_always() {
        assert_eq!(CopyBack::Full.extent(Dir::Out, 512, 0, 0, 0), (0, 512));
        assert_eq!(CopyBack::Full.extent(Dir::Out, 512, -EINVAL, 0, 0), (0, 0));
        assert_eq!(
            CopyBack::Full.extent(Dir::InOut, 512, -EINVAL, 0, 0),
            (0, 512)
        );
    }

    #[test]
    fn a_range_returns_the_nvkms_reply_half_even_on_failure() {
        let reply = CopyBack::Range { off: 12, len: 32 };
        assert_eq!(reply.extent(Dir::InOut, 44, 0, 0, 0), (12, 44));
        assert_eq!(reply.extent(Dir::InOut, 44, -1, 0, 0), (12, 44));
    }

    /// The abi crate does without libc; the value is all these need.
    const EINVAL: i32 = 22;

    #[test]
    fn a_two_plane_format_has_two_planes_and_an_unknown_one_has_one() {
        // DRM_FORMAT_NV12, DRM_FORMAT_XRGB8888
        assert_eq!(format_planes(0x3231_564e), 2);
        assert_eq!(format_planes(0x3432_5258), 1);
    }

    #[test]
    fn the_example_nvkms_table_is_selected_only_for_its_version() {
        let t = modeset_table(DriverVersion::new(610, 57, 4)).expect("table");
        assert!(t.lookup_nvkms(3).is_some());
        assert!(modeset_table(DriverVersion::new(595, 71, 5)).is_none());
    }
}
