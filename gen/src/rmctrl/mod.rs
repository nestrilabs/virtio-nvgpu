// SPDX-License-Identifier: Apache-2.0
//! Where RM follows user pointers inside RM_CONTROL parameters, measured.
//!
//! `generated.rs` is rendered by `gen/rmctrl_extract.py` from the
//! per-release measurements in `gen/rmctrl/*.json`, which the same script
//! takes from each release's RM sources (embedded_param_copy.c, the
//! deprecated control table, and the handlers that copy user memory
//! themselves) and its SDK headers (offsets, by compiling a probe). The
//! backend's pointer scrub (`device/src/guestptr.rs`) reads it; nothing in it
//! is transcribed by hand.
//!
//! The same measurements give how much RM copies through each pointer
//! (`DEEP_CONTROLS`, `IDLE_CHANNELS`), which is what the backend checks a
//! guest's deep segments against (`device/src/deepseg.rs`); the guest's copy
//! of the rows it uses, `driver/gen/nvgpu_rm_deep.h`, is rendered with it.

mod generated;

pub use generated::*;

/// The controls RM follows a pointer in, by number.
pub fn pointers(cmd: u32) -> Option<&'static ControlPointers> {
    CONTROL_POINTERS.iter().find(|c| c.cmd == cmd)
}

/// Whether `cmd` is a control whose pointers have no fixed place.
pub fn refused(cmd: u32) -> bool {
    REFUSED_CONTROLS.iter().any(|&(c, _)| c == cmd)
}

/// The control `cmd`, if the guest may send the data for its pointers.
pub fn deep_control(cmd: u32) -> Option<&'static DeepControl> {
    DEEP_CONTROLS.iter().find(|c| c.cmd == cmd)
}

/// Whether `cmd` is a control whose pointers are always zeroed.
pub fn zeroed(cmd: u32) -> bool {
    ZEROED_CONTROLS.iter().any(|&(c, _)| c == cmd)
}

impl DeepPtr {
    /// How many bytes RM copies through this pointer, computed from
    /// `params` -- the block RM is about to be handed -- as RM computes it:
    /// the counts multiplied in NvU32 (wrapping, as C's unsigned arithmetic
    /// does), then by the element size with an overflow check
    /// (portSafeMulU32, whose failure RM answers NV_ERR_INVALID_ARGUMENT).
    /// `None` for that overflow, or for a count outside `params`.
    pub fn size(&self, params: &[u8]) -> Option<u32> {
        let mut n = self.scale;
        for c in self.counts {
            let b = params.get(c.offset..c.offset.checked_add(c.width)?)?;
            let mut v = [0u8; 4];
            v.get_mut(..b.len())?.copy_from_slice(b);
            n = n.wrapping_mul(u32::from_le_bytes(v));
        }
        n.checked_mul(self.elem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The checked-in table is what the measurements render to: nobody
    /// edited one without the other. (Re-measuring from NVIDIA's sources
    /// needs the network and is `gen/rmctrl_extract.py check`.)
    #[test]
    fn the_checked_in_table_is_what_the_extractor_renders() {
        let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::env::temp_dir().join(format!("rmctrl-render-{}", std::process::id()));
        let status = Command::new("python3")
            .arg(gen_dir.join("rmctrl_extract.py"))
            .arg("render")
            .arg("--out")
            .arg(&out)
            .status()
            .expect("python3 runs (the extractor needs it)");
        assert!(status.success(), "rmctrl_extract.py render failed");
        for (rendered, checked_in) in [
            ("generated.rs", gen_dir.join("src/rmctrl/generated.rs")),
            // The guest's copy of the deep rows: both halves are rendered from
            // the same measurements, so this is also the test that they agree.
            ("nvgpu_rm_deep.h", gen_dir.join("../driver/gen/nvgpu_rm_deep.h")),
        ] {
            let fresh = std::fs::read(out.join(rendered)).expect("rendered");
            let checked_in = std::fs::read(&checked_in).expect("checked in");
            assert!(
                fresh == checked_in,
                "{rendered} is stale: run gen/rmctrl_extract.py render"
            );
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn every_pointer_is_an_aligned_offset_and_every_command_is_listed_once() {
        let mut seen = std::collections::HashSet::new();
        for c in CONTROL_POINTERS {
            assert!(seen.insert(c.cmd), "{:#x} twice", c.cmd);
            assert!(!refused(c.cmd), "{} both listed and refused", c.name);
            assert!(!c.ptrs.is_empty(), "{}", c.name);
            for &o in c.ptrs {
                assert_eq!(o % 8, 0, "{} at {o}", c.name);
            }
        }
    }

    /// Spot checks against RM's own sources: FIFO_GET_CHANNELLIST copies
    /// through two pointers (embedded_param_copy.c:292-306), and 535
    /// numbers the NV0073 ACPI call differently from later releases.
    #[test]
    fn the_table_has_what_the_sources_show() {
        assert_eq!(pointers(0x0080_170d).map(|c| c.ptrs), Some(&[8, 16][..]));
        assert!(pointers(0x0073_0120).is_some() && pointers(0x0073_0168).is_some());
        assert!(refused(0x402c_0105), "I2C_TRANSACTION");
        assert!(pointers(0x2080_0a01).is_none(), "a control with no pointer");
    }

    /// FIFO_GET_CHANNELLIST: numChannels (at 0) NvU32s through both lists,
    /// the handle list copied in only (SKIP_COPYOUT), the channel list both
    /// ways (embedded_param_copy.c:292-306). GET_P2P_CAPS: gpuCount squared
    /// NvU32s, out only (:585-617).
    #[test]
    fn deep_rules_are_rms() {
        let c = deep_control(0x0080_170d).expect("GET_CHANNELLIST");
        let mut p = [0u8; 24];
        p[..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(c.ptrs.iter().map(|d| d.ptr).collect::<Vec<_>>(), [8, 16]);
        assert!(c.ptrs.iter().all(|d| d.size(&p) == Some(12)));
        assert_eq!(
            c.ptrs
                .iter()
                .map(|d| (d.copy_in, d.copy_out))
                .collect::<Vec<_>>(),
            [(true, false), (true, true)]
        );
        let c = deep_control(0x0000_0127).expect("GET_P2P_CAPS");
        let mut p = [0u8; 176];
        p[128..132].copy_from_slice(&3u32.to_le_bytes());
        assert!(c.ptrs.iter().all(|d| d.size(&p) == Some(36) && !d.copy_in));
        // RM's NvU32 product wraps; its multiply by the element size is
        // checked, and so is ours.
        p[128..132].copy_from_slice(&0x1_0000u32.to_le_bytes());
        assert_eq!(c.ptrs[0].size(&p), Some(0));
        p[128..132].copy_from_slice(&0x8000u32.to_le_bytes());
        assert_eq!(c.ptrs[0].size(&p), None);
        assert_eq!(c.ptrs[0].size(&p[..130]), None, "a count past the block");
        for cmd in [0x130, 0x0073_0120, 0x101] {
            assert!(zeroed(cmd) && deep_control(cmd).is_none(), "{cmd:#x}");
        }
    }

    /// Every deep row names exactly the pointers CONTROL_POINTERS has for
    /// the command, and IDLE_CHANNELS is NVOS30 as guestptr.rs knows it.
    #[test]
    fn deep_rows_agree_with_the_pointer_table() {
        for c in DEEP_CONTROLS {
            let listed = pointers(c.cmd).expect(c.name).ptrs;
            let ptrs: Vec<usize> = c.ptrs.iter().map(|d| d.ptr).collect();
            assert_eq!(ptrs, listed, "{}", c.name);
            assert!(c.ptrs.len() <= 4 && !zeroed(c.cmd), "{}", c.name);
        }
        assert_eq!(IDLE_CHANNELS_SIZE, 56);
        assert_eq!(
            IDLE_CHANNELS
                .ptrs
                .iter()
                .map(|d| (d.ptr, d.counts[0].offset))
                .collect::<Vec<_>>(),
            [(16, 12), (24, 12), (32, 12)]
        );
        assert_eq!(
            (IDLE_CHANNELS_FLAGS, IDLE_CHANNELS_LIST_BITS, IDLE_CHANNELS_LIST),
            (40, (4, 7), 0)
        );
    }
}
