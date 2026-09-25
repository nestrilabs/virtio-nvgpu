//! Where RM follows user pointers inside RM_CONTROL parameters, measured.
//!
//! `generated.rs` is rendered by `gen/rmctrl_extract.py` from the
//! per-release measurements in `gen/rmctrl/*.json`, which the same script
//! takes from each release's RM sources (embedded_param_copy.c, the
//! deprecated control table, and the handlers that copy user memory
//! themselves) and its SDK headers (offsets, by compiling a probe). The
//! backend's pointer scrub (`device/src/guestptr.rs`) reads it; nothing in it
//! is transcribed by hand.

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
        let fresh = std::fs::read(out.join("generated.rs")).expect("rendered");
        let checked_in =
            std::fs::read(gen_dir.join("src/rmctrl/generated.rs")).expect("checked in");
        assert!(
            fresh == checked_in,
            "gen/src/rmctrl/generated.rs is stale: run gen/rmctrl_extract.py render"
        );
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
}
