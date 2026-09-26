// SPDX-License-Identifier: Apache-2.0
//! The file descriptors inside UVM's parameters.
//!
//! Six nvidia-uvm commands name another open file by descriptor: the RM
//! control file a GPU, VA space, channel or allocation belongs to
//! (`rmCtrlFd` in REGISTER_GPU, REGISTER_GPU_VASPACE, REGISTER_CHANNEL,
//! MAP_EXTERNAL_ALLOCATION and ALLOC_DEVICE_P2P), and the primary UVM file
//! MM_INITIALIZE pins (`uvmFd`). A descriptor is a number in the calling
//! process's table -- the backend's -- so the guest's number went to the
//! host as sent and named whatever the backend had open under it: another
//! guest process's files, a Wayland socket, a vring eventfd. UVM fgets
//! `uvmFd` and holds that file for the life of the VA space (uvm.c:67);
//! `rmCtrlFd` it carries into its record of the RM object and, as of 610,
//! does not yet look up (the "Bug 1624521" TODOs, uvm_user_channel.c:132,
//! uvm_va_space.c:1557) -- translated all the same, so the day it does, it
//! finds the caller's file.
//!
//! They are translated the way the RM escapes' descriptors are: the guest
//! driver turns its caller's descriptor into the backend handle of that file
//! (and refuses one that is not a file of ours), the backend turns the
//! handle into its own descriptor for the call, checking the handle is the
//! kind UVM expects (an RM control file, or a UVM file), and both put their
//! caller's value back in the reply. A negative value is UVM's "none" and
//! goes through unchanged; it names nothing in any table.
//!
//! The guest learns where the fields are from the device config's
//! descriptor table, the one that already names the RM escapes: an entry
//! whose `nr` has [`FDT_UVM`] set is a UVM command, the whole command number
//! in the low bits, and its `payload_offset` holds the field's offset in the
//! low 16 bits and the parameter block's size in the high 16. A guest driver
//! from before this only matches an entry's `nr` against an RM escape's
//! 8-bit number, so it never matches these. The block's size has to be sent
//! because UVM command numbers carry none, and one of the offsets depends on
//! the host release: MAP_EXTERNAL_ALLOCATION holds an attribute per GPU
//! before `rmCtrlFd`, and that array went from 32 to 256 GPUs in 550.40.53
//! (UVM_MAX_GPUS_V2).
//!
//! Offsets and sizes are the generated UVM table's (abi::schema::uvm_table,
//! measured per release by gen/uvm_extract.py), the same table the guest
//! sizes every UVM call by, so the two cannot disagree. A host release with
//! no table (older than the first measured, or a version that does not
//! parse) has no fields: its UVM calls are refused whole.

#![forbid(unsafe_code)]

use abi::schema::{UvmFdOf, uvm_table};
use abi::version::DriverVersion;

/// Set in a config descriptor-table entry's `nr` for a UVM command.
pub const FDT_UVM: u32 = 0x8000_0000;

/// What the descriptor must name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FdOf {
    /// An RM control file (/dev/nvidiactl).
    RmCtl,
    /// A UVM file (/dev/nvidia-uvm).
    Uvm,
}

/// One UVM command's descriptor field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UvmFdField {
    /// The UVM command number (uvm_ioctl.h, UVM_IOCTL_BASE(n) == n).
    pub cmd: u32,
    /// Offset of the i32 descriptor in the parameters.
    pub offset: u32,
    /// Size of the parameters.
    pub size: u32,
    pub of: FdOf,
}

/// The descriptor fields of the host release `v`'s UVM.
pub fn fields(v: Option<DriverVersion>) -> Vec<UvmFdField> {
    let Some(t) = v.and_then(uvm_table) else {
        return Vec::new();
    };
    t.cmds
        .iter()
        .filter_map(|c| {
            let fd = c.fd?;
            Some(UvmFdField {
                cmd: c.cmd,
                offset: fd.offset,
                size: c.size,
                of: match fd.of {
                    UvmFdOf::RmCtl => FdOf::RmCtl,
                    UvmFdOf::Uvm => FdOf::Uvm,
                },
            })
        })
        .collect()
}

/// The descriptor field of UVM command `cmd` on release `v`, if it has one.
pub fn field(v: Option<DriverVersion>, cmd: u32) -> Option<UvmFdField> {
    fields(v).into_iter().find(|f| f.cmd == cmd)
}

/// The fields as the config's descriptor table carries them: `(nr,
/// payload_offset)`.
pub fn config_entries(v: Option<DriverVersion>) -> impl Iterator<Item = (u32, u32)> {
    fields(v)
        .into_iter()
        .map(|f| (FDT_UVM | f.cmd, f.offset | (f.size << 16)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_external_allocation_moves_with_the_gpu_count_of_the_release() {
        let old = field(Some(DriverVersion::new(535, 129, 3)), 33).unwrap();
        let new = field(Some(DriverVersion::new(580, 95, 5)), 33).unwrap();
        assert_eq!((old.offset, old.size), (1184, 1200));
        assert_eq!((new.offset, new.size), (9248, 9264));
        // 550.40.07 still has 32 GPUs; from 550.40.53 on, the whole 550
        // branch has UVM_MAX_GPUS_V2 (256) in this block (measured, not the
        // 555 this once assumed).
        assert_eq!(field(Some(DriverVersion::new(550, 40, 7)), 33), Some(old));
        assert_eq!(field(Some(DriverVersion::new(550, 54, 14)), 33), Some(new));
        assert_eq!(field(Some(DriverVersion::new(555, 42, 2)), 33), Some(new));
    }

    #[test]
    fn a_host_with_no_table_has_no_fields() {
        assert!(fields(None).is_empty());
        assert!(fields(Some(DriverVersion::new(535, 129, 2))).is_empty());
        assert_eq!(config_entries(None).count(), 0);
    }

    #[test]
    fn alloc_device_p2p_names_a_file_only_where_the_release_has_it() {
        assert_eq!(field(Some(DriverVersion::new(560, 35, 3)), 78), None);
        let p2p = field(Some(DriverVersion::new(565, 57, 1)), 78).unwrap();
        assert_eq!((p2p.offset, p2p.size, p2p.of), (40, 56, FdOf::RmCtl));
    }

    #[test]
    fn every_field_is_inside_its_block_and_fits_the_config_encoding() {
        for t in abi::schema::UVM_TABLES {
            for f in fields(Some(t.versions.0)) {
                assert!(f.offset + 4 <= f.size, "{f:?}");
                assert!(f.size < 1 << 16 && f.offset < 1 << 16, "{f:?}");
                assert!(f.size <= 0x3000, "within UVM's 0x3000 bound: {f:?}");
            }
        }
    }

    #[test]
    fn a_config_entry_never_matches_an_rm_escape_number() {
        let v = Some(DriverVersion::new(610, 57, 4));
        for (nr, packed) in config_entries(v) {
            assert!(nr & FDT_UVM != 0 && nr > 0xff);
            let (off, size) = (packed & 0xffff, packed >> 16);
            assert_eq!(
                field(v, nr & !FDT_UVM).map(|f| (f.offset, f.size)),
                Some((off, size))
            );
        }
    }

    #[test]
    fn only_the_primary_file_is_a_uvm_descriptor() {
        let v = Some(DriverVersion::new(610, 57, 4));
        assert_eq!(field(v, 75).map(|f| f.of), Some(FdOf::Uvm));
        assert!(fields(v).iter().filter(|f| f.of == FdOf::Uvm).count() == 1);
        assert_eq!(field(v, 34), None, "FREE names no file");
    }
}
