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

/// A field exists only when `(u32 at off & mask) == value` -- or, with
/// `ne`, `!= value`: NVKMS's one-byte NvBool flags, which the kernel tests
/// for non-zero -- `off` in the same struct as the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cond {
    pub off: u32,
    pub mask: u32,
    pub value: u32,
    pub ne: bool,
}

impl Cond {
    /// Whether the field exists, given the u32 at `off`.
    pub fn holds(self, word: u32) -> bool {
        (word & self.mask == self.value) != self.ne
    }
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
    /// Bytes [0, left), always: `left` is the count as the host left it
    /// (NVKMS's infoStringLenWritten, copied out whatever the command
    /// returned, nvkms.c:1920-1945).
    Written {
        off: u32,
        width: u8,
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
            CopyBack::Written { .. } => left,
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
    /// `count` inline elements of `stride` bytes, walked in place; `limit`
    /// says how many of them are the kernel's.
    Array {
        count: u32,
        stride: u32,
        limit: Limit,
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

/// How many elements of an inline array the kernel reads. A descriptor in
/// an element it never reads is not a descriptor, and must not have to be
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    /// Every element.
    All,
    /// The first min(count, n), n the `width`-byte count at `off` of the
    /// enclosing struct (JOIN_SWAP_GROUP's numMembers).
    Count { off: u32, width: u8 },
    /// The first numPlanes(format), format the u32 at `off` and numPlanes
    /// the table's `planes` (REGISTER_SURFACE); an unknown format has none.
    Planes { off: u32 },
}

impl Limit {
    /// Elements of a `count`-element array the kernel reads, given the
    /// value at `off` (unused for `All`) and the table's plane counts.
    pub fn elements(self, count: u32, value: u64, planes: &[u8]) -> u32 {
        match self {
            Limit::All => count,
            Limit::Count { .. } => value.min(u64::from(count)) as u32,
            Limit::Planes { .. } => usize::try_from(value)
                .ok()
                .and_then(|v| planes.get(v))
                .map_or(0, |&n| u32::from(n).min(count)),
        }
    }
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
    /// An NVKMS command whose layout differs in the next release measured:
    /// only on a host of exactly the table's release.
    pub const NVKMS_EXACT: u32 = 1 << 9;
    /// SET_MASTER / DROP_MASTER: only on a host card file, never a lease.
    pub const MASTER: u32 = 1 << 10;
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
    /// The first is the release the table was measured from.
    pub versions: Option<(DriverVersion, DriverVersion)>,
    pub ioctls: &'static [Ioctl],
    pub fields: &'static [Field],
    /// NVKMS: numPlanes by surface format value (`Limit::Planes`).
    pub planes: &'static [u8],
    /// NVKMS: where the backend's policy finds what it checks.
    pub nvkms: Option<&'static NvkmsLayout>,
}

impl Table {
    pub fn fields(&self, s: Span) -> &'static [Field] {
        &self.fields[s.first as usize..s.first as usize + s.len as usize]
    }

    /// The entry for (class, `_IOC_TYPE`, `_IOC_NR`): the one whose whole
    /// number is `cmd` if there is one (a number may have two layouts, told
    /// apart by the size in it), else any with that type and nr. Size and
    /// direction are the caller's to compare: a known number with the
    /// wrong size is a different refusal (-EINVAL) from an unknown one
    /// (-ENOTTY).
    pub fn lookup(&self, class: Class, cmd: u32) -> Option<&'static Ioctl> {
        let mine = |e: &&Ioctl| e.class == class && e.cmd & 0xffff == cmd & 0xffff;
        self.ioctls
            .iter()
            .filter(mine)
            .find(|e| e.cmd == cmd)
            .or_else(|| self.ioctls.iter().find(mine))
    }

    /// The Modeset entry for an NVKMS command.
    pub fn lookup_nvkms(&self, nvkms_cmd: u32) -> Option<&'static Ioctl> {
        self.ioctls
            .iter()
            .find(|e| e.class == Class::Modeset && e.nvkms_cmd == nvkms_cmd)
    }
}

/// The NVKMS table for a host driver version: the one of the newest release
/// measured that is not newer than `v`, the last for anything newer than
/// every release, none for anything older than the first (the ranges are
/// generated that way, gen/schema/nvkms.py). A table for a release it was
/// not measured from is a guess, which `Len::NvkmsParams` and
/// `policy::NVKMS_EXACT` fence; `modeset_table_exact` says which it is.
pub fn modeset_table(v: DriverVersion) -> Option<&'static Table> {
    MODESET_TABLES
        .iter()
        .copied()
        .find(|t| t.versions.is_some_and(|(lo, hi)| lo <= v && v <= hi))
}

/// Whether `v` is the very release its NVKMS table was measured from.
pub fn modeset_table_exact(v: DriverVersion) -> bool {
    modeset_table(v).is_some_and(|t| t.versions.is_some_and(|(lo, _)| lo == v))
}

// ───────────────────────── NVKMS policy layout ─────────────────────────
//
// What the backend's NVKMS policy (device/src/nvkms.rs) reads and rewrites
// in a params block, per release. Generated with the tables from the same
// JSON (gen/nvkms/<release>.json); offsets are from the start of the params
// block unless a field says otherwise.

/// `count` elements of `stride` bytes from `off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arr {
    pub off: u32,
    pub count: u32,
    pub stride: u32,
}

impl Arr {
    /// Where element `i` starts.
    pub fn at(self, i: u32) -> usize {
        (self.off + i * self.stride) as usize
    }
}

/// A request that names one head or dpy: its deviceHandle, dispHandle and
/// the head (or dpyId).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsTarget {
    pub device: u32,
    pub disp: u32,
    pub what: u32,
}

/// One kind of permission in an NvKmsPermissions: per head from 595
/// (`disp` None; `head` absolute), per (disp, head) before (`head`
/// relative to a `disp` element).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsPermArr {
    pub disp: Option<Arr>,
    pub head: Arr,
}

/// An NvKmsPermissions and the deviceHandle beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsPerms {
    pub device: u32,
    pub ptype: u32,
    /// u8 layerMask per head.
    pub flip: NvkmsPermArr,
    /// NVDpyIdList (u32) per head.
    pub modeset: NvkmsPermArr,
}

/// SET_LAYER_POSITION: which disps (a bitmask), and per disp element which
/// heads (a bitmask at `heads`, relative to the element).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsLayerPosition {
    pub device: u32,
    pub disps: u32,
    pub disp: Arr,
    pub heads: u32,
    pub head: Arr,
}

/// FLIP: deviceHandle, pFlipHead, numFlipHeads, the size of one pFlipHead
/// element, and within one its `sd` and `head` (u32s), the layers, and each
/// layer's useSyncpt, syncObjects.specified and completionNotifier.awaken
/// (bytes, relative to the layer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsFlipLayout {
    pub device: u32,
    pub ptr: u32,
    pub heads: u32,
    pub head_size: u32,
    pub sd: u32,
    pub head: u32,
    pub layer: Arr,
    pub use_syncpt: u32,
    pub sync_specified: u32,
    pub awaken: u32,
}

/// SET_MODE: deviceHandle, commit (a byte), requestedDispsBitMask, then
/// disp[] (requestedHeadsBitMask), disp[].head[] (dpyIdList) and
/// disp[].head[].flip.layer[] (the same three bytes as FLIP's), each array
/// and field relative to its parent element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsSetModeLayout {
    pub device: u32,
    pub commit: u32,
    pub disps: u32,
    pub disp: Arr,
    pub heads: u32,
    pub head: Arr,
    pub dpys: u32,
    pub layer: Arr,
    pub use_syncpt: u32,
    pub sync_specified: u32,
    pub awaken: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvkmsLayout {
    /// ALLOC_DEVICE request bytes that set device-wide state when the call
    /// creates the device (registry keys, console hotplugs, no3d, SLI
    /// mosaic): (off, len).
    pub alloc_scrub: &'static [(u32, u32)],
    pub alloc_reply_device: u32,
    /// `NvKmsDispHandle dispHandles[]`, indexed by disp.
    pub alloc_reply_disps: u32,
    /// `isoIOCoherencyModes` then `nisoIOCoherencyModes`, each
    /// `{NvBool coherent; NvBool noncoherent;}`: what display lets a client
    /// allocate display memory as.
    pub alloc_reply_coherency: u32,
    /// QUERY_DPY_DYNAMIC_DATA's override flags and EDID: (off, len).
    pub dpy_dynamic_scrub: &'static [(u32, u32)],
    pub set_cursor_image: NvkmsTarget,
    pub move_cursor: NvkmsTarget,
    pub set_lut: NvkmsTarget,
    /// `what` is the dpyId.
    pub set_dpy_attribute: NvkmsTarget,
    pub layer_position: NvkmsLayerPosition,
    pub flip: NvkmsFlipLayout,
    pub set_mode: NvkmsSetModeLayout,
    /// GRANT_PERMISSIONS's request, ACQUIRE_PERMISSIONS's reply,
    /// REVOKE_PERMISSIONS's request.
    pub grant: NvkmsPerms,
    pub acquire: NvkmsPerms,
    pub revoke: NvkmsPerms,
    /// DECLARE_EVENT_INTEREST's interestMask, and the bits a guest may set.
    pub event_interest: u32,
    pub events_allowed: u32,
    /// GET_NEXT_EVENT's reply.valid (a byte).
    pub next_event_valid: u32,
    /// nvidia-drm's GRANT/REVOKE_PERMISSIONS carry a `type` (12/8 bytes;
    /// false: 535's 8/4, always MODESET).
    pub drm_grant_typed: bool,
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
            (Class::Render, 0x1e), // SET_MASTER
            (Class::Render, 0x1f), // DROP_MASTER
            (Class::Kms, 0x11),    // AUTH_MAGIC
            (Class::Kms, 0xb3),    // MAP_DUMB
            (Class::Render, 0x09), // GEM_CLOSE
        ];
        for (class, nr) in refused {
            let cmd = (b'd' as u32) << 8 | nr;
            assert!(DRM_TABLE.lookup(class, cmd).is_none(), "{class:?} {nr:#x}");
        }
    }

    /// The guest core arbitrates master; its hooks carry the outcome to the
    /// host card file as these, on the file's executor (nvidia's master_set
    /// and master_drop take nvkms_lock and blank heads, so never on the queue
    /// thread), and the backend lets them reach cards only.
    #[test]
    fn master_calls_exist_only_for_cards_and_carry_nothing() {
        for nr in [0x1e, 0x1f] {
            let cmd = (b'd' as u32) << 8 | nr;
            let e = DRM_TABLE.lookup(Class::Kms, cmd).expect("KMS entry");
            assert_eq!((e.size, e.cmd >> 30), (0, 0), "{}: DRM_IO", e.name);
            assert_eq!(e.policy, policy::MASTER, "{}", e.name);
            assert_eq!(e.exec, Exec::Executor, "{}", e.name);
            assert!(e.fields.len == 0, "{}", e.name);
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

    fn v(a: u32, b: u32, c: u32) -> DriverVersion {
        DriverVersion::new(a, b, c)
    }

    #[test]
    fn a_measured_release_gets_its_own_table_and_knows_it() {
        for (rel, name) in [
            (v(535, 129, 3), "v535_129_03"),
            (v(580, 178, 4), "v580_178_04"),
            (v(595, 71, 5), "v595_71_05"),
            (v(595, 99, 2), "v595_99_02"),
            (v(610, 57, 4), "v610_57_04"),
            (v(615, 71, 9), "v615_71_09"),
        ] {
            assert_eq!(modeset_table(rel).expect("a table").name, name);
            assert!(modeset_table_exact(rel), "{name}");
        }
    }

    #[test]
    fn a_release_between_two_measured_ones_gets_the_older_and_a_newer_one_the_last() {
        assert_eq!(modeset_table(v(600, 1, 0)).unwrap().name, "v595_99_02");
        assert_eq!(modeset_table(v(610, 57, 3)).unwrap().name, "v595_99_02");
        assert_eq!(modeset_table(v(610, 80, 0)).unwrap().name, "v610_57_04");
        assert_eq!(modeset_table(v(999, 0, 0)).unwrap().name, "v615_71_09");
        assert!(!modeset_table_exact(v(610, 80, 0)));
        assert!(modeset_table(v(535, 129, 2)).is_none(), "nothing older");
        assert!(modeset_table(v(470, 1, 1)).is_none());
    }

    #[test]
    fn command_numbers_are_the_releases_own() {
        // REGISTER_SURFACE is 16 in 535 and 615 and 17 in between; the
        // params size (152) is the same, which is why the number alone
        // must never be carried over.
        let reg = |rel| {
            modeset_table(rel)
                .unwrap()
                .ioctls
                .iter()
                .find(|e| e.name == "NVKMS_REGISTER_SURFACE")
                .unwrap()
                .nvkms_cmd
        };
        assert_eq!(reg(v(535, 129, 3)), 16);
        assert_eq!(reg(v(595, 99, 2)), 17);
        assert_eq!(reg(v(610, 57, 4)), 17);
        assert_eq!(reg(v(615, 71, 9)), 16);
    }

    #[test]
    fn every_nvkms_entry_requires_its_own_params_size() {
        for t in MODESET_TABLES {
            for e in t.ioctls {
                let [root] = t.fields(e.fields) else {
                    panic!("{}: one root field", e.name)
                };
                let Kind::Ptr {
                    len, max, copyback, ..
                } = root.kind
                else {
                    panic!("{}: the params pointer", e.name)
                };
                assert_eq!(len, Len::NvkmsParams, "{}", e.name);
                assert!(max > 0);
                assert!(matches!(copyback, CopyBack::Range { off, len } if off + len <= max));
                assert_eq!(e.cmd, NVKMS_IOCTL_IOWR);
            }
        }
        let set_mode = modeset_table(v(610, 57, 4))
            .unwrap()
            .ioctls
            .iter()
            .find(|e| e.name == "NVKMS_SET_MODE")
            .unwrap();
        let t = modeset_table(v(610, 57, 4)).unwrap();
        let Kind::Ptr { max, .. } = t.fields(set_mode.fields)[0].kind else {
            unreachable!()
        };
        assert_eq!(max, 186_784, "research/nvkms.md §5");
    }

    #[test]
    fn commands_the_design_refuses_are_in_no_table() {
        let refused = [
            "NVKMS_FRAMEBUFFER_CONSOLE_DISABLED",
            "NVKMS_REGISTER_VBLANK_INTR_CALLBACK",
            "NVKMS_UNREGISTER_VBLANK_INTR_CALLBACK",
            "NVKMS_EXPORT_VRR_SEMAPHORE_SURFACE",
            "NVKMS_VRR_SIGNAL_SEMAPHORE",
            "NVKMS_GET_3DVISION_DONGLE_PARAM_BYTES",
            "NVKMS_SET_3DVISION_AEGIS_PARAMS",
        ];
        for t in MODESET_TABLES {
            for e in t.ioctls {
                assert!(!refused.contains(&e.name), "{} in {}", e.name, t.name);
            }
            // 35 and 36 have no dispatch entry in any release.
            assert!(t.lookup_nvkms(35).is_none() || t.name == "v535_129_03");
        }
        // 580's 63 is ACCEL_VBLANK_SEM_CONTROLS, a command we keep: the
        // refusals are by name, never by number.
        let t = modeset_table(v(580, 178, 4)).unwrap();
        assert_eq!(
            t.lookup_nvkms(63).unwrap().name,
            "NVKMS_ACCEL_VBLANK_SEM_CONTROLS"
        );
    }

    #[test]
    fn a_command_whose_layout_moves_in_the_next_release_is_marked_exact() {
        // 615 dropped CHECK_LUT_NOTIFIER, so every command above 13 moved.
        let t = modeset_table(v(610, 57, 4)).unwrap();
        let exact = |n: u32| t.lookup_nvkms(n).unwrap().policy & policy::NVKMS_EXACT != 0;
        assert!(!exact(3), "QUERY_CONNECTOR_STATIC_DATA did not move");
        assert!(exact(15), "FLIP did");
        // 595.71.05 and 595.99.02 do not differ in anything tracked.
        let t = modeset_table(v(595, 71, 5)).unwrap();
        assert!(t.ioctls.iter().all(|e| e.policy & policy::NVKMS_EXACT == 0));
        // Nothing to compare the last with: newer hosts rely on the sizes.
        let t = modeset_table(v(615, 71, 9)).unwrap();
        assert!(t.ioctls.iter().all(|e| e.policy & policy::NVKMS_EXACT == 0));
    }

    #[test]
    fn flip_has_three_levels_of_pointers() {
        let t = modeset_table(v(610, 57, 4)).unwrap();
        let e = t.lookup_nvkms(15).unwrap();
        let params = &t.fields(e.fields)[0];
        let Kind::Ptr { children, .. } = params.kind else {
            unreachable!()
        };
        let [heads] = t.fields(children) else {
            panic!("pFlipHead only")
        };
        assert_eq!(heads.name, "request.pFlipHead");
        let Kind::Ptr {
            len,
            stride,
            children,
            max,
            ..
        } = heads.kind
        else {
            unreachable!()
        };
        assert_eq!(
            len,
            Len::Count {
                off: 16,
                width: 4,
                elem: 4952
            }
        );
        assert_eq!((stride, max), (4952, 32 * 4952));
        let ramps: Vec<_> = t.fields(children).iter().map(|f| (f.name, f.off)).collect();
        assert_eq!(
            ramps,
            [
                ("flip.lut.input.pRamps", 88),
                ("flip.lut.output.pRamps", 104)
            ]
        );
    }

    #[test]
    fn register_surface_reads_as_many_plane_descriptors_as_its_format_has() {
        let t = modeset_table(v(610, 57, 4)).unwrap();
        let e = t.lookup_nvkms(17).unwrap();
        let Kind::Ptr { children, .. } = t.fields(e.fields)[0].kind else {
            unreachable!()
        };
        let [planes] = t.fields(children) else {
            panic!("planes only")
        };
        assert_eq!(
            planes.cond,
            Some(Cond {
                off: 4,
                mask: 0xff,
                value: 0,
                ne: true
            })
        );
        let Kind::Array {
            count,
            stride,
            limit,
            children,
        } = planes.kind
        else {
            unreachable!()
        };
        assert_eq!((planes.off, count, stride), (16, 3, 32));
        assert_eq!(limit, Limit::Planes { off: 124 });
        let [fd] = t.fields(children) else {
            panic!("one fd")
        };
        assert!(matches!(fd.kind, Kind::FdIn { width: 4, kinds, .. }
            if kinds == K_DEV_CTL | (1 << 7)));
        // A8R8G8B8 is one plane, Y8___U8V8_N420 two, Y8___U8___V8_N420
        // three (nvkms-format.c), and a format nobody knows none.
        assert_eq!(limit.elements(count, 4, t.planes), 1);
        assert_eq!(limit.elements(count, 19, t.planes), 2);
        assert_eq!(limit.elements(count, 34, t.planes), 3);
        assert_eq!(limit.elements(count, 999, t.planes), 0);
    }

    #[test]
    fn an_nvbool_condition_holds_for_any_non_zero_byte_and_nothing_else() {
        let c = Cond {
            off: 4,
            mask: 0xff,
            value: 0,
            ne: true,
        };
        assert!(c.holds(1));
        assert!(c.holds(2), "NVKMS tests the byte for non-zero");
        assert!(
            !c.holds(0xffff_ff00),
            "the padding after it is not the flag"
        );
        let eq = Cond {
            off: 0,
            mask: 1,
            value: 1,
            ne: false,
        };
        assert!(eq.holds(3) && !eq.holds(2));
    }

    #[test]
    fn validate_mode_copies_back_what_nvkms_says_it_wrote() {
        let t = modeset_table(v(610, 57, 4)).unwrap();
        let e = t.lookup_nvkms(8).unwrap();
        let Kind::Ptr { children, .. } = t.fields(e.fields)[0].kind else {
            unreachable!()
        };
        let [info] = t.fields(children) else {
            panic!("pInfoString only")
        };
        let Kind::Ptr {
            dir,
            len,
            copyback,
            max,
            ..
        } = info.kind
        else {
            unreachable!()
        };
        assert_eq!((info.off, dir, max), (296, Dir::Out, 2048));
        assert_eq!(
            len,
            Len::Count {
                off: 288,
                width: 4,
                elem: 1
            }
        );
        assert_eq!(copyback, CopyBack::Written { off: 456, width: 4 });
        // 40 bytes written of 2048 offered, even though the command failed.
        assert_eq!(copyback.extent(Dir::Out, 2048, -1, 2048, 40), (0, 40));
        assert_eq!(copyback.extent(Dir::Out, 16, 0, 16, 40), (0, 16));
    }

    #[test]
    fn a_number_with_two_layouts_is_found_by_its_exact_size_first() {
        let typed = DRM_TABLE.lookup(Class::Kms, 0xc00c_6452).unwrap();
        let untyped = DRM_TABLE.lookup(Class::Kms, 0xc008_6452).unwrap();
        assert_eq!(typed.name, "NV_GRANT_PERMISSIONS");
        assert_eq!(untyped.name, "NV_GRANT_PERMISSIONS_UNTYPED");
        // A size neither has still finds the number, for -EINVAL.
        assert!(DRM_TABLE.lookup(Class::Kms, 0xc010_6452).is_some());
    }

    #[test]
    fn every_nvkms_layout_is_inside_its_params_blocks() {
        for t in MODESET_TABLES {
            let lo = t.nvkms.expect("an NVKMS layout");
            let size = |n: &str| {
                let e = t.ioctls.iter().find(|e| e.name == n).unwrap();
                match t.fields(e.fields)[0].kind {
                    Kind::Ptr { max, .. } => max as usize,
                    _ => unreachable!(),
                }
            };
            let set_mode = lo.set_mode;
            let last_layer = set_mode.disp.at(set_mode.disp.count - 1)
                + set_mode.head.at(set_mode.head.count - 1)
                + set_mode.layer.at(set_mode.layer.count - 1);
            for byte in [
                set_mode.use_syncpt,
                set_mode.sync_specified,
                set_mode.awaken,
            ] {
                assert!(
                    last_layer + (byte as usize) < size("NVKMS_SET_MODE"),
                    "{}",
                    t.name
                );
                assert!(byte < set_mode.layer.stride, "{}", t.name);
            }
            // FLIP's per-layer bytes sit inside one pFlipHead element, as
            // its sd and head do.
            let flip = lo.flip;
            let last_layer = flip.layer.at(flip.layer.count - 1);
            for byte in [flip.use_syncpt, flip.sync_specified, flip.awaken] {
                assert!(
                    last_layer + (byte as usize) < flip.head_size as usize,
                    "{}",
                    t.name
                );
            }
            assert!(flip.sd + 4 <= flip.head_size && flip.head + 4 <= flip.head_size);
            for &(off, len) in lo.dpy_dynamic_scrub {
                assert!((off + len) as usize <= size("NVKMS_QUERY_DPY_DYNAMIC_DATA"));
            }
            for &(off, len) in lo.alloc_scrub {
                assert!(off >= 32, "versionString passes: {}", t.name);
                assert!((off + len) as usize <= size("NVKMS_ALLOC_DEVICE"));
            }
            assert_eq!(lo.events_allowed, 0b10_0111, "{}", t.name);
        }
    }
}
