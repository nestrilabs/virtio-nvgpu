// SPDX-License-Identifier: GPL-2.0-only
//! The IOCTL2 schema, as the guest's tables lay it out.
//!
//! The tables are C (`driver/gen/nvgpu_schema.h`, generated with the
//! backend's copy by `gen/schema_gen.py`) and stay the one copy the module
//! has: [`SField`] and [`SIoctl`] are `#[repr(C)]` mirrors of `struct
//! nvgpu_sfield` and `struct nvgpu_sioctl`, so the kernel hands the C arrays
//! to this code as slices, and the host tests do the same with the header
//! compiled by the C compiler. The tables are trusted (they are compiled in),
//! but nothing here indexes them unchecked either.

use super::wire::le32;

/// `NVGPU_HK_DEV`, `NVGPU_HK_DRI_RENDER`, `NVGPU_HK_WAYLAND`: the backend
/// handle kinds a file of the module's own can be.
pub const HK_DEV: u32 = 1;
/// See [`HK_DEV`].
pub const HK_DRI_RENDER: u32 = 2;
/// See [`HK_DEV`].
pub const HK_WAYLAND: u32 = 10;
/// `NVGPU_SKIND_DEV_CTL`, `_MODESET`, `_GPU`: one device each.
pub const SKIND_DEV_CTL: u32 = 1 << 16;
/// See [`SKIND_DEV_CTL`].
pub const SKIND_DEV_MODESET: u32 = 1 << 17;
/// See [`SKIND_DEV_CTL`].
pub const SKIND_DEV_GPU: u32 = 1 << 18;
/// `NVGPU_DEV_*` (`nvgpu_wire.h`): what an open of the module's asked for.
pub const DEV_CTL: u32 = 255;
/// See [`DEV_CTL`].
pub const DEV_UVM: u32 = 256;
/// See [`DEV_CTL`].
pub const DEV_UVM_TOOLS: u32 = 257;
/// See [`DEV_CTL`].
pub const DEV_MODESET: u32 = 258;
/// See [`DEV_CTL`].
pub const DEV_WAYLAND: u32 = 259;
/// See [`DEV_CTL`].
pub const DEV_DRI_BASE: u32 = 512;
/// See [`DEV_CTL`].
pub const DEV_DRI_CARD_BASE: u32 = 1024;

/// `nvgpu_fd_kind_allowed()`: whether a file of the module's of
/// `device_type` may stand in a descriptor field that allows `kinds` (an
/// `FD_IN` mask) -- the backend's `schema::kind_allowed()` bit for bit: the
/// handle kind's own bit (`1 << HK_*`), or the `SKIND_DEV_*` bit of the one
/// device it is.
pub fn fd_kind_allowed(device_type: u32, kinds: u32) -> bool {
    let (hk, dev_bit) = match device_type {
        t if t < DEV_CTL => (HK_DEV, SKIND_DEV_GPU),
        DEV_CTL => (HK_DEV, SKIND_DEV_CTL),
        DEV_MODESET => (HK_DEV, SKIND_DEV_MODESET),
        DEV_UVM | DEV_UVM_TOOLS => (HK_DEV, 0),
        DEV_WAYLAND => (HK_WAYLAND, 0),
        t if (DEV_DRI_BASE..DEV_DRI_CARD_BASE).contains(&t) => (HK_DRI_RENDER, 0),
        _ => return false,
    };
    kinds & (1u32 << hk) != 0 || kinds & dev_bit != 0
}

/// `NVGPU_SF_PTR`.
pub const SF_PTR: u8 = 1;
/// `NVGPU_SF_ARRAY`.
pub const SF_ARRAY: u8 = 2;
/// `NVGPU_SF_FD_IN`.
pub const SF_FD_IN: u8 = 3;
/// `NVGPU_SF_FD_OUT`.
pub const SF_FD_OUT: u8 = 4;
/// `NVGPU_SF_GEM_IN`.
pub const SF_GEM_IN: u8 = 5;
/// `NVGPU_SF_GEM_OUT`.
pub const SF_GEM_OUT: u8 = 6;

/// `NVGPU_SDIR_IN`.
pub const SDIR_IN: u8 = 1;
/// `NVGPU_SDIR_OUT`.
pub const SDIR_OUT: u8 = 2;
/// `NVGPU_SDIR_INOUT`.
pub const SDIR_INOUT: u8 = 3;

/// `NVGPU_SFF_COND`.
pub const SFF_COND: u8 = 1 << 0;
/// `NVGPU_SFF_COND_NE`.
pub const SFF_COND_NE: u8 = 1 << 2;

/// `NVGPU_SLEN_CONST`.
pub const SLEN_CONST: u8 = 1;
/// `NVGPU_SLEN_COUNT`.
pub const SLEN_COUNT: u8 = 2;
/// `NVGPU_SLEN_SUM`.
pub const SLEN_SUM: u8 = 3;
/// `NVGPU_SLEN_NVKMS_PARAMS`.
pub const SLEN_NVKMS_PARAMS: u8 = 4;
/// `NVGPU_SLEN_PLANES`.
pub const SLEN_PLANES: u8 = 5;

/// `NVGPU_SCB_NONE`.
pub const SCB_NONE: u8 = 0;
/// `NVGPU_SCB_FULL`.
pub const SCB_FULL: u8 = 1;
/// `NVGPU_SCB_PARTIAL`.
pub const SCB_PARTIAL: u8 = 2;
/// `NVGPU_SCB_ALL_OR_NOTHING`.
pub const SCB_ALL_OR_NOTHING: u8 = 3;
/// `NVGPU_SCB_EXACT`.
pub const SCB_EXACT: u8 = 4;
/// `NVGPU_SCB_RANGE`.
pub const SCB_RANGE: u8 = 5;
/// `NVGPU_SCB_WRITTEN`.
pub const SCB_WRITTEN: u8 = 6;

/// `NVGPU_SSPECIAL_ATOMIC`.
pub const SSPECIAL_ATOMIC: u8 = 1;

/// `NVGPU_SIO_EXECUTOR`.
pub const SIO_EXECUTOR: u16 = 1 << 0;

/// `NVGPU_SCHEMA_MAX_LIST`.
pub const MAX_LIST: usize = 32;
/// `NVGPU_SCHEMA_MAX_DEPTH`.
pub const MAX_DEPTH: u32 = 4;

/// `NVGPU_NVKMS_IOCTL_IOWR`.
pub const NVKMS_IOCTL_IOWR: u32 = 0xc010_6d00;

/// `NVGPU_SCLASS_RENDER`.
pub const SCLASS_RENDER: u32 = 1;
/// `NVGPU_SCLASS_KMS`.
pub const SCLASS_KMS: u32 = 2;
/// `NVGPU_SCLASS_MODESET`.
pub const SCLASS_MODESET: u32 = 3;

/// `struct nvgpu_sfield`, field for field.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SField {
    /// Offset in the struct holding it.
    pub off: u32,
    /// `NVGPU_SF_*`.
    pub kind: u8,
    /// Bytes (descriptor fields: 4 or 8).
    pub width: u8,
    /// PTR: `NVGPU_SDIR_*`.
    pub dir: u8,
    /// `NVGPU_SFF_*`.
    pub flags: u8,
    /// COND: the word tested, its offset.
    pub cond_off: u32,
    /// COND: the mask.
    pub cond_mask: u32,
    /// COND: the value.
    pub cond_value: u32,
    /// `NVGPU_SLEN_*`.
    pub len_kind: u8,
    /// COUNT: the count's width.
    pub len_width: u8,
    /// `NVGPU_SCB_*`.
    pub cb_kind: u8,
    /// The copy-back count's width.
    pub cb_width: u8,
    /// Length rule argument (bytes, offset or field index).
    pub len_a: u32,
    /// Element size.
    pub len_elem: u32,
    /// PTR: most bytes.
    pub max: u32,
    /// Copy-back count offset; RANGE: start.
    pub cb_off: u32,
    /// Copy-back element size; RANGE: length.
    pub cb_arg: u32,
    /// FD_IN: `NVGPU_SKIND*`.
    pub kinds: u32,
    /// FD_IN: the "no descriptor" value.
    pub none_value: i32,
    /// PTR / ARRAY element size.
    pub stride: u32,
    /// ARRAY: elements.
    pub count: u32,
    /// First field of one element.
    pub child: u16,
    /// Fields of one element.
    pub nchild: u16,
}

/// `struct nvgpu_sioctl`, field for field. `name` is C's, never read here.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SIoctl {
    /// `const char *name`, opaque.
    pub name: *const u8,
    /// The whole ioctl number.
    pub cmd: u32,
    /// MODESET: the NVKMS command.
    pub nvkms_cmd: u32,
    /// `_IOC_SIZE(cmd)`.
    pub size: u32,
    /// `NVGPU_SCLASS_*`.
    pub sclass: u8,
    /// `NVGPU_SSPECIAL_*`.
    pub special: u8,
    /// `NVGPU_SIO_*`.
    pub flags: u16,
    /// `NVGPU_SPOL_*`.
    pub policy: u32,
    /// First field.
    pub field: u16,
    /// Fields.
    pub nfield: u16,
}

// The layouts the kernel's slices are built on; nvgpu_rs.h asserts the same
// sizes on the C side.
const _: () = assert!(core::mem::size_of::<SField>() == 64);
const _: () = assert!(core::mem::size_of::<SIoctl>() == 32 || core::mem::size_of::<usize>() != 8);

/// One of `struct nvgpu_stable`'s tables, as slices.
#[derive(Clone, Copy, Debug)]
pub struct Table<'t> {
    /// Its entries.
    pub ioctls: &'t [SIoctl],
    /// Every entry's fields.
    pub fields: &'t [SField],
    /// `numPlanes` by NVKMS surface format.
    pub planes: &'t [u8],
}

/// `struct nvgpu_schema_set`: the DRM tables, and NVKMS's for this host.
#[derive(Clone, Copy, Debug)]
pub struct SchemaSet<'t> {
    /// `drm`.
    pub drm: Table<'t>,
    /// `modeset`, if this host has one.
    pub modeset: Option<Table<'t>>,
}

/// Which table of a [`SchemaSet`] an entry was found in. Zero is `Drm`
/// (the i2 state relies on all-zero bytes being valid).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Which {
    /// `set.drm`.
    #[default]
    Drm = 0,
    /// `set.modeset`.
    Modeset,
}

impl<'t> SchemaSet<'t> {
    /// The table `w` names (the DRM one if there is no NVKMS table, which a
    /// lookup never returns).
    pub fn table(&self, w: Which) -> Table<'t> {
        match (w, self.modeset) {
            (Which::Modeset, Some(t)) => t,
            _ => self.drm,
        }
    }

    /// `nvgpu_i2_lookup()`: the entry for a call, by (class, type, nr) in the
    /// DRM tables -- the one whose whole number is `cmd` if there is one --
    /// or for NVKMS by the command in the first bytes of the outer struct
    /// (`prefix`). Size and direction are the caller's to compare.
    pub fn lookup(&self, sclass: u32, cmd: u32, prefix: &[u8]) -> Option<(Which, usize)> {
        let (which, t, nvkms_cmd) = if sclass == SCLASS_MODESET {
            let t = self.modeset?;
            if cmd != NVKMS_IOCTL_IOWR {
                return None;
            }
            (Which::Modeset, t, le32(prefix, 0)?)
        } else {
            (Which::Drm, self.drm, 0)
        };
        let mut found = None;
        for i in 0..t.ioctls.len() {
            let Some(e) = t.ioctls.get(i) else { break };
            if u32::from(e.sclass) != sclass {
                continue;
            }
            if sclass == SCLASS_MODESET {
                if e.nvkms_cmd == nvkms_cmd {
                    found = Some(i);
                    break;
                }
            } else if e.cmd & 0xffff == cmd & 0xffff {
                if found.is_none() {
                    found = Some(i);
                }
                if e.cmd == cmd {
                    found = Some(i);
                    break;
                }
            }
        }
        found.map(|i| (which, i))
    }
}

#[cfg(test)]
#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    /// The host's kind_allowed (device/src/schema.rs), case by case: a
    /// device bit admits that device and no other, HK_DEV any character
    /// device, a DRM file of ours is its render handle.
    #[test]
    fn fd_kinds_are_the_hosts() {
        let hk = |k: u32| 1u32 << k;
        for (t, kinds, ok) in [
            (DEV_MODESET, SKIND_DEV_MODESET, true),
            (DEV_MODESET, SKIND_DEV_CTL, false),
            (DEV_CTL, SKIND_DEV_CTL, true),
            (DEV_CTL, SKIND_DEV_MODESET | SKIND_DEV_GPU, false),
            (0, SKIND_DEV_GPU, true),
            (247, SKIND_DEV_GPU, true),
            (0, SKIND_DEV_CTL, false),
            (DEV_UVM, SKIND_DEV_GPU | SKIND_DEV_CTL | SKIND_DEV_MODESET, false),
            (DEV_UVM, hk(HK_DEV), true),
            (DEV_UVM_TOOLS, hk(HK_DEV), true),
            (DEV_MODESET, hk(HK_DEV), true),
            (3, hk(HK_DEV), true),
            (DEV_DRI_BASE, hk(HK_DEV), false),
            (DEV_DRI_BASE, hk(HK_DRI_RENDER), true),
            (DEV_DRI_BASE + 7, hk(HK_DRI_RENDER), true),
            (DEV_DRI_CARD_BASE, hk(HK_DRI_RENDER), false),
            (DEV_WAYLAND, hk(HK_DEV), false),
            (DEV_WAYLAND, hk(HK_WAYLAND), true),
            (260, u32::MAX, false),
            (DEV_CTL, 0, false),
        ] {
            assert_eq!(fd_kind_allowed(t, kinds), ok, "type {t} kinds {kinds:#x}");
        }
    }

    pub(crate) fn io(cmd: u32, sclass: u32, nvkms_cmd: u32) -> SIoctl {
        SIoctl {
            name: core::ptr::null(),
            cmd,
            nvkms_cmd,
            size: (cmd >> 16) & 0x3fff,
            sclass: sclass as u8,
            special: 0,
            flags: 0,
            policy: 0,
            field: 0,
            nfield: 0,
        }
    }

    #[test]
    fn lookup_prefers_the_exact_number() {
        let drm = [
            io(0xc010_6400 | 0x42, SCLASS_KMS, 0),
            io(0xc018_6400 | 0x42, SCLASS_KMS, 0),
            io(0xc018_6400 | 0x43, SCLASS_RENDER, 0),
        ];
        let set = SchemaSet {
            drm: Table {
                ioctls: &drm,
                fields: &[],
                planes: &[],
            },
            modeset: None,
        };
        // Same type and nr, other size: the first with that nr.
        assert_eq!(
            set.lookup(SCLASS_KMS, 0xc020_6442, &[]),
            Some((Which::Drm, 0))
        );
        assert_eq!(
            set.lookup(SCLASS_KMS, 0xc018_6442, &[]),
            Some((Which::Drm, 1))
        );
        // Another class's entry is not ours.
        assert_eq!(set.lookup(SCLASS_KMS, 0xc018_6443, &[]), None);
        assert_eq!(
            set.lookup(SCLASS_RENDER, 0xc018_6443, &[]),
            Some((Which::Drm, 2))
        );
        // No NVKMS table: nothing for MODESET.
        assert_eq!(
            set.lookup(SCLASS_MODESET, NVKMS_IOCTL_IOWR, &[1, 0, 0, 0]),
            None
        );
    }

    #[test]
    fn lookup_nvkms_by_command() {
        let drm = [io(1, SCLASS_KMS, 0)];
        let ms = [
            io(NVKMS_IOCTL_IOWR, SCLASS_MODESET, 7),
            io(NVKMS_IOCTL_IOWR, SCLASS_MODESET, 16),
        ];
        let set = SchemaSet {
            drm: Table {
                ioctls: &drm,
                fields: &[],
                planes: &[],
            },
            modeset: Some(Table {
                ioctls: &ms,
                fields: &[],
                planes: &[],
            }),
        };
        assert_eq!(
            set.lookup(SCLASS_MODESET, NVKMS_IOCTL_IOWR, &[16, 0, 0, 0, 9]),
            Some((Which::Modeset, 1))
        );
        assert_eq!(
            set.lookup(SCLASS_MODESET, NVKMS_IOCTL_IOWR, &[17, 0, 0, 0]),
            None
        );
        // A prefix too short to hold the command, or another number.
        assert_eq!(
            set.lookup(SCLASS_MODESET, NVKMS_IOCTL_IOWR, &[16, 0, 0]),
            None
        );
        assert_eq!(
            set.lookup(SCLASS_MODESET, 0xc010_6d01, &[16, 0, 0, 0]),
            None
        );
    }
}
