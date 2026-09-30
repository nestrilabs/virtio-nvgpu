// SPDX-License-Identifier: Apache-2.0
//! What the backend reads and rewrites to hold a VM to `--vram-limit`, per
//! release.
//!
//! `generated.rs` is rendered by `gen/vidmem_extract.py` from the per-release
//! measurements in `gen/vidmem/*.json`, which the same script takes from each
//! release's SDK headers by compiling a probe: the offsets of every field the
//! backend's video-memory accounting reads (`device/src/vidmem.rs`), the sizes
//! of the blocks they are in, and the constants they are read with. The
//! FB_INFO indices are among what moves: HEAP_RECLAIMABLE is 0x3c in
//! 595.99.02, 0x44 in 615.71.09, and absent from 610.57.04.
//!
//! Only a release measured exactly has a layout: the backend refuses
//! `--vram-limit` on any other, rather than rewrite replies by a neighbour's.

#[rustfmt::skip]
mod generated;

pub use generated::*;

use crate::version::DriverVersion;

/// The layout of release `v`, measured at exactly `v`.
pub fn layout_for(v: DriverVersion) -> Option<&'static Layout> {
    LAYOUTS
        .iter()
        .find(|l| DriverVersion::new(l.version.0, l.version.1, l.version.2) == v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The checked-in table is what the measurements render to. (Measuring
    /// again from NVIDIA's sources needs the network: `gen/vidmem_extract.py
    /// check`.)
    #[test]
    fn the_checked_in_table_is_what_the_extractor_renders() {
        let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::env::temp_dir().join(format!("vidmem-render-{}", std::process::id()));
        let status = Command::new("python3")
            .arg(gen_dir.join("vidmem_extract.py"))
            .arg("render")
            .arg("--out")
            .arg(&out)
            .status()
            .expect("python3 runs (the extractor needs it)");
        assert!(status.success(), "vidmem_extract.py render failed");
        let fresh = std::fs::read(out.join("generated.rs")).expect("rendered");
        let checked_in =
            std::fs::read(gen_dir.join("src/vidmem/generated.rs")).expect("checked in");
        assert!(
            fresh == checked_in,
            "gen/src/vidmem/generated.rs is stale: run gen/vidmem_extract.py render"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Every release the RM allowlist was measured at has a layout, and only
    /// those: `--vram-limit` works wherever the backend starts without
    /// `--allow-unmeasured-release`.
    #[test]
    fn every_measured_release_has_a_layout() {
        for r in crate::rmallow::RELEASES {
            let v = r.version();
            let l = layout_for(v).expect("a layout");
            assert_eq!(DriverVersion::new(l.version.0, l.version.1, l.version.2), v);
        }
        assert_eq!(LAYOUTS.len(), crate::rmallow::RELEASES.len());
        assert!(layout_for(DriverVersion::new(600, 1, 0)).is_none());
    }

    /// What the backend relies on of each layout: a list entry inside its
    /// block, the list inside the V2 block at RM's maximum, and the fields
    /// it reads inside the blocks it reads them from.
    #[test]
    fn the_fields_read_are_inside_their_blocks() {
        for l in LAYOUTS {
            let v = l.version;
            let entry = l.fb_get_info_v2_fb_info_list
                + l.fb_info_sizeof * l.nv2080_ctrl_fb_info_max_list_size as usize;
            assert!(entry <= l.fb_get_info_v2_sizeof, "{v:?}");
            assert!(l.fb_info_index + 4 <= l.fb_info_sizeof, "{v:?}");
            assert!(l.fb_info_data + 4 <= l.fb_info_sizeof, "{v:?}");
            assert!(l.nv_memory_allocation_size + 8 <= l.nv_memory_allocation_sizeof);
            for at in [
                l.nvos32_total,
                l.nvos32_free,
                l.nvos32_alloc_size_size,
                l.nvos32_alloc_size_range_size,
                l.nvos32_alloc_tiled_pitch_height_size,
                l.nvos32_info_size,
            ] {
                assert!(at + 8 <= l.nvos32_sizeof, "{v:?} {at}");
            }
            assert!(l.nvos02_limit + 8 <= l.nvos02_sizeof, "{v:?}");
            assert!(
                l.unix_export_objects_to_fd_objects
                    + 4 * l.nv0000_ctrl_os_unix_export_objects_to_fd_max_objects as usize
                    <= l.unix_export_objects_to_fd_num_objects,
                "{v:?}"
            );
            assert!(
                l.unix_import_objects_from_fd_objects
                    + 4 * l.nv0000_ctrl_os_unix_import_objects_to_fd_max_objects as usize
                    <= l.unix_import_objects_from_fd_sizeof,
                "{v:?}"
            );
        }
    }
}
