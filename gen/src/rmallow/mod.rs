//! Which RM controls and classes a guest may reach, per release.
//!
//! `generated.rs` is rendered by `gen/rmallow_extract.py` from the
//! per-release measurements in `gen/rmallow/*.json` -- every control RM
//! exports (the NVOC exported-method tables, with their RMCTRL flags and
//! parameter sizes) and every class it can allocate (resource_list.h, with
//! its RS flags) -- filtered by the policy written in that script: only
//! what an unprivileged process may call, what a workload the project runs
//! asks for, and what names no host resource the backend does not
//! translate. The backend refuses everything else before RM sees it
//! (`device/src/rmallow.rs`).
//!
//! A host between two measured releases uses the older one's list, as the
//! NVKMS and UVM tables do; one older than every release the oldest's, and
//! one newer the newest's. Parameter sizes are held only on a release
//! measured exactly: they move between releases.

#[rustfmt::skip]
mod generated;

pub use generated::*;

use crate::version::DriverVersion;

fn version_of(r: &Release) -> DriverVersion {
    DriverVersion::new(r.version.0, r.version.1, r.version.2)
}

/// The release whose list a host running `v` is held to, and whether it was
/// measured at exactly `v`.
pub fn release_for(v: DriverVersion) -> (&'static Release, bool) {
    let r = RELEASES
        .iter()
        .rev()
        .find(|r| version_of(r) <= v)
        .unwrap_or(&RELEASES[0]);
    (r, version_of(r) == v)
}

impl Release {
    pub fn version(&self) -> DriverVersion {
        version_of(self)
    }

    /// The allowed control `cmd`, if it is one.
    pub fn control(&self, cmd: u32) -> Option<&'static Control> {
        // `controls` is 'static; the search borrows it as such.
        let controls: &'static [Control] = self.controls;
        controls
            .binary_search_by_key(&cmd, |c| c.cmd)
            .ok()
            .map(|i| &controls[i])
    }

    /// Whether class `class` may be allocated.
    pub fn class(&self, class: u32) -> bool {
        self.classes.binary_search(&class).is_ok()
    }

    /// Whether VID_HEAP_CONTROL's `function` may run: RM implements it and
    /// every class it would allocate is allowed.
    pub fn vidheap(&self, function: u32) -> bool {
        self.vidheap.binary_search(&function).is_ok()
    }

    /// Whether DEFERRED_API may carry control `cmd` (RM runs only these).
    pub fn deferred(&self, cmd: u32) -> bool {
        self.deferred.binary_search(&cmd).is_ok()
    }
}

/// RM's name for control `cmd`, in any release measured.
pub fn control_name(cmd: u32) -> Option<&'static str> {
    CONTROL_NAMES
        .binary_search_by_key(&cmd, |&(c, _)| c)
        .ok()
        .map(|i| CONTROL_NAMES[i].1)
}

/// RM's name for class `class`, in any release measured.
pub fn class_name(class: u32) -> Option<&'static str> {
    CLASS_NAMES
        .binary_search_by_key(&class, |&(c, _)| c)
        .ok()
        .map(|i| CLASS_NAMES[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The checked-in table is what the measurements and the policy render
    /// to: nobody edited one without the other. (Re-measuring from NVIDIA's
    /// sources needs the network and is `gen/rmallow_extract.py check`.)
    #[test]
    fn the_checked_in_table_is_what_the_extractor_renders() {
        let gen_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let out = std::env::temp_dir().join(format!("rmallow-render-{}", std::process::id()));
        let status = Command::new("python3")
            .arg(gen_dir.join("rmallow_extract.py"))
            .arg("render")
            .arg("--out")
            .arg(&out)
            .status()
            .expect("python3 runs (the extractor needs it)");
        assert!(status.success(), "rmallow_extract.py render failed");
        let fresh = std::fs::read(out.join("generated.rs")).expect("rendered");
        let checked_in =
            std::fs::read(gen_dir.join("src/rmallow/generated.rs")).expect("checked in");
        assert!(
            fresh == checked_in,
            "gen/src/rmallow/generated.rs is stale: run gen/rmallow_extract.py render"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    /// What the rig's hardware runs had RM serve: `gen/rmallow/observed.txt`.
    fn observed(key: &str) -> Vec<u32> {
        include_str!("../../rmallow/observed.txt")
            .lines()
            .filter_map(|l| l.split('#').next())
            .filter_map(|l| l.trim().strip_prefix(key))
            .flat_map(|rest| rest.split_whitespace())
            .map(|h| u32::from_str_radix(h.trim_start_matches("0x"), 16).expect("hex"))
            .collect()
    }

    #[test]
    fn every_observed_control_and_class_is_allowed_in_every_release() {
        let controls = observed("controls ");
        let classes = observed("classes ");
        assert_eq!(controls.len(), 156);
        assert_eq!(classes.len(), 33);
        for r in RELEASES {
            // What a release lacks, or refuses an unprivileged caller itself
            // (the extractor checks that), is all that may be missing, and
            // from 580.178.04 on nothing is.
            if r.version() >= DriverVersion::new(580, 0, 0) {
                assert!(r.unserved_controls.is_empty(), "{}", r.version());
                assert!(r.unserved_classes.is_empty(), "{}", r.version());
            }
            for &c in &controls {
                if r.unserved_controls.contains(&c) {
                    continue;
                }
                assert!(
                    r.control(c).is_some(),
                    "{}: observed control {c:#010x} ({:?}) refused",
                    r.version(),
                    control_name(c)
                );
            }
            for &c in &classes {
                if r.unserved_classes.contains(&c) {
                    continue;
                }
                assert!(
                    r.class(c),
                    "{}: observed class {c:#06x} ({:?}) refused",
                    r.version(),
                    class_name(c)
                );
            }
        }
    }

    #[test]
    fn privileged_internal_and_host_naming_controls_are_refused() {
        for r in RELEASES {
            for (cmd, what) in [
                // PRIVILEGED: RM wants an administrator
                (0x2080_012b, "NV2080_CTRL_CMD_GPU_PROMOTE_CTX"),
                (0x2080_1805, "NV2080_CTRL_CMD_BUS_SET_PCIE_SPEED"),
                // INTERNAL: RM's own
                (
                    0x0073_0401,
                    "NV0073_CTRL_CMD_INTERNAL_GET_HOTPLUG_UNPLUG_STATE",
                ),
                // KERNEL_PRIVILEGED (neither flag)
                (0x0002_0101, "NV0002_CTRL_CMD_UPDATE_CONTEXTDMA"),
                (0x003e_0103, "NV003E_CTRL_CMD_GET_SURFACE_PHYS_PAGES"),
                // unprivileged, and no workload asks for them
                (0x2080_0112, "NV2080_CTRL_CMD_GPU_SET_POWER"),
                (0x2080_0122, "NV2080_CTRL_CMD_GPU_EXEC_REG_OPS"),
                (0x2080_206f, "NV2080_CTRL_CMD_PERF_RATED_TDP_SET_CONTROL"),
                // a host process by PID (rmctl.rs answers it)
                (0x2080_018d, "NV2080_CTRL_CMD_GPU_GET_PIDS"),
                // a host cgroup by descriptor
                (0x0000_3d0d, "NV0000_CTRL_OS_UNIX_CMD_MEMACCT_SET_LIMITS"),
                // an IMEX channel by descriptor
                (
                    0x0000_0d08,
                    "NV0000_CTRL_CMD_CLIENT_SUBSCRIBE_TO_IMEX_CHANNEL",
                ),
                // the host's firmware
                (0x0000_0130, "NV0000_CTRL_CMD_SYSTEM_EXECUTE_ACPI_METHOD"),
                // scheduling past the caller's own work
                (0xa06c_0110, "NVA06C_CTRL_CMD_MAKE_REALTIME"),
                (0xa06f_0111, "NVA06F_CTRL_CMD_RESTART_RUNLIST"),
                // exists nowhere
                (0x2080_ffff, "unknown"),
                (0xdead_beef, "unknown"),
                // a GSS legacy control nothing was seen to send
                (0x2080_a0ff, "GSS legacy, unobserved"),
                // a privileged GSS legacy control
                (0x2080_e060, "GSS legacy, privileged"),
                // a binary-API control nothing was seen to send
                (0x2081_0101, "NV2081 binary API, unobserved"),
            ] {
                assert!(
                    r.control(cmd).is_none(),
                    "{}: {what} ({cmd:#010x}) allowed",
                    r.version()
                );
            }
        }
    }

    #[test]
    fn kernel_privileged_and_unsupported_classes_are_refused() {
        for r in RELEASES {
            for (class, what) in [
                (0x0078, "NV01_EVENT_KERNEL_CALLBACK"),
                (0x007e, "NV01_EVENT_KERNEL_CALLBACK_EX"),
                (0x0081, "NV01_MEMORY_LIST_SYSTEM"),
                (0x00f1, "NV_IMEX_SESSION"),
                (0x00fd, "NV_MEMORY_MULTICAST_FABRIC"),
                (0x0092, "NV0092_RG_LINE_CALLBACK"),
                (0x9010, "NV9010_VBLANK_CALLBACK"),
                (0x402c, "NV40_I2C"),
                (0x30f1, "NV30_GSYNC"),
                (0x5080, "NV50_DEFERRED_API_CLASS"),
                (0xa0bd, "NVFBC_SW_SESSION"),
                (0xb2cc, "MAXWELL_PROFILER_DEVICE"),
                (0x208f, "NV20_SUBDEVICE_DIAG"),
                (0xc637, "AMPERE_SMC_PARTITION_REF"),
                (0xa084, "NVA084_KERNEL_HOST_VGPU_DEVICE"),
                (0x0001_0000, "no class"),
            ] {
                assert!(
                    !r.class(class),
                    "{}: {what} ({class:#x}) allowed",
                    r.version()
                );
            }
        }
    }

    #[test]
    fn every_architectures_channel_and_engine_classes_come_with_the_observed_ones() {
        let (r, _) = release_for(DriverVersion::new(610, 57, 4));
        for (class, what) in [
            (0xc46f, "TURING_CHANNEL_GPFIFO_A"),
            (0xc56f, "AMPERE_CHANNEL_GPFIFO_A"),
            (0xc86f, "HOPPER_CHANNEL_GPFIFO_A"),
            (0xc461, "TURING_USERMODE_A"),
            (0xc561, "AMPERE_USERMODE_A"),
            (0xc597, "TURING_A"),
            (0xc697, "AMPERE_A"),
            (0xc997, "ADA_A"),
            (0xc5c0, "TURING_COMPUTE_A"),
            (0xc7c0, "AMPERE_COMPUTE_B"),
            (0xc5b5, "TURING_DMA_COPY_A"),
            (0xc7b5, "AMPERE_DMA_COPY_B"),
            (0xc4b0, "NVC4B0_VIDEO_DECODER"),
            (0xc7b7, "NVC7B7_VIDEO_ENCODER"),
            (0xc9d1, "NVC9D1_VIDEO_NVJPG"),
            (0xc9fa, "NVC9FA_VIDEO_OFA"),
            (0xa0bc, "NVENC_SW_SESSION"),
        ] {
            assert!(r.class(class), "{what} ({class:#x}) refused");
        }
    }

    #[test]
    fn a_host_between_releases_uses_the_older_list() {
        let (r, exact) = release_for(DriverVersion::new(610, 57, 4));
        assert_eq!(r.version(), DriverVersion::new(610, 57, 4));
        assert!(exact);
        let (r, exact) = release_for(DriverVersion::new(600, 1, 0));
        assert_eq!(r.version(), DriverVersion::new(595, 99, 2));
        assert!(!exact);
        let (r, _) = release_for(DriverVersion::new(470, 0, 0));
        assert_eq!(r.version(), DriverVersion::new(535, 129, 3));
        let (r, _) = release_for(DriverVersion::new(999, 0, 0));
        assert_eq!(r.version(), RELEASES.last().unwrap().version());
    }

    #[test]
    fn the_tables_are_sorted_and_the_allowlist_is_a_small_part_of_what_rm_exports() {
        for r in RELEASES {
            assert!(r.controls.windows(2).all(|w| w[0].cmd < w[1].cmd));
            assert!(r.classes.windows(2).all(|w| w[0] < w[1]));
            assert!(r.vidheap.windows(2).all(|w| w[0] < w[1]));
            assert!(r.deferred.windows(2).all(|w| w[0] < w[1]));
            assert!(r.controls.len() * 4 < r.total_controls, "{}", r.version());
            assert!(r.classes.len() * 2 < r.total_classes, "{}", r.version());
            // VID_HEAP_CONTROL's OS descriptor path is osdesc.rs's to gate,
            // and it allocates an allowed class; the rest are memory.
            assert!(r.vidheap(NVOS32_FUNCTION_ALLOC_SIZE));
            assert!(r.vidheap(NVOS32_FUNCTION_FREE));
        }
        assert!(CONTROL_NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(CLASS_NAMES.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(
            control_name(0x2080_0102),
            Some("NV2080_CTRL_CMD_GPU_GET_INFO_V2")
        );
        assert_eq!(class_name(0xc56f), Some("AMPERE_CHANNEL_GPFIFO_A"));
    }
}
