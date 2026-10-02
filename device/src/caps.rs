//! What a guest is served.
//!
//! The backend takes one option, `--caps`, a comma list of `graphics`,
//! `compute`, `video` and `utility`, named the way NVIDIA's container toolkit
//! names driver capabilities. The bits travel to the guest in config `caps`
//! (`NVGPU_CAP_*` in `driver/virtio_gpu_nv.c`), and the guest uses them to
//! decide which device nodes exist. The backend enforces them whatever the
//! guest does with them.
//!
//! | cap      | served                                                  |
//! |----------|---------------------------------------------------------|
//! | always   | `nvidiactl`, `nvidiaN`, RM objects every workload needs |
//! | graphics | 3D classes, the DRM render node, `nvidia-modeset`       |
//! | compute  | `nvidia-uvm`                                            |
//! | video    | NVENC, NVDEC, NVJPG and OFA classes                     |
//! | utility  | reserved for the monitoring controls; not yet enforced  |
//!
//! Compute is UVM, not the compute class: NVIDIA's Vulkan driver allocates the
//! compute class for its compute queues, so gating the class would take
//! graphics with it. CUDA cannot run without UVM.
//!
//! `nvidia-uvm-tools` is served under no capability. It pins user buffers and
//! copies through process memory, and nothing but a profiler opens it.

use std::fmt;

/// Bit values, shared with the guest driver.
pub const COMPUTE: u32 = 1 << 0;
pub const GRAPHICS: u32 = 1 << 1;
pub const VIDEO: u32 = 1 << 2;
pub const UTILITY: u32 = 1 << 3;

const NAMES: [(&str, u32); 4] = [
    ("graphics", GRAPHICS),
    ("compute", COMPUTE),
    ("video", VIDEO),
    ("utility", UTILITY),
];

/// A set of capabilities. Never empty: a guest with none could open nothing
/// useful, and an empty set on the wire means a backend from before
/// capabilities existed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps(u32);

impl Caps {
    /// What `--caps` defaults to. Compute is opt-in.
    pub const DEFAULT: Caps = Caps(GRAPHICS | VIDEO | UTILITY);

    /// Parse a comma list. Unknown and empty names are refused rather than
    /// skipped: a misspelt capability would otherwise serve less than asked
    /// with nothing said.
    pub fn parse(list: &str) -> Result<Caps, String> {
        let mut bits = 0;
        for name in list.split(',').map(str::trim) {
            let Some(&(_, bit)) = NAMES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) else {
                return Err(format!(
                    "unknown capability {name:?}; expected a comma list of graphics, compute, video, utility"
                ));
            };
            bits |= bit;
        }
        if bits == 0 {
            return Err("no capabilities given".into());
        }
        Ok(Caps(bits))
    }

    pub fn bits(self) -> u32 {
        self.0
    }

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit == bit
    }

    /// The capability an RM class needs, if it needs one. `None` is a class
    /// every workload uses (clients, devices, memory, channels, the compute
    /// class), or one this list does not know, which the RM allowlist decides.
    pub fn for_class(class: u32) -> Option<u32> {
        if THREE_D_CLASSES.contains(&class) {
            Some(GRAPHICS)
        } else if VIDEO_CLASSES.contains(&class) {
            Some(VIDEO)
        } else {
            None
        }
    }
}

impl fmt::Display for Caps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<_> = NAMES
            .iter()
            .filter(|(_, b)| self.has(*b))
            .map(|(n, _)| *n)
            .collect();
        f.write_str(&names.join(","))
    }
}

/// 3D engine classes, FERMI_A to BLACKWELL_B. From the `cl*97.h` headers of
/// NVIDIA's open-gpu-kernel-modules 595.104.02.
pub const THREE_D_CLASSES: &[u32] = &[
    0x9097, 0xa097, 0xa197, 0xb097, 0xb197, 0xc097, 0xc197, 0xc397, 0xc597, 0xc697, 0xc797, 0xc997,
    0xcb97, 0xcd97, 0xce97,
];

/// Video engine classes: NVENC (`*b7`), NVDEC (`*b0`), NVJPG (`*d1`) and OFA
/// (`*fa`). From the same headers.
pub const VIDEO_CLASSES: &[u32] = &[
    // NVENC
    0xb4b7, 0xc0b7, 0xc1b7, 0xc2b7, 0xc3b7, 0xc4b7, 0xc7b7, 0xc9b7, 0xceb7, 0xcfb7, 0xd0b7, 0xd1b7,
    0xd5b7, //
    // NVDEC
    0xa0b0, 0xb0b0, 0xb6b0, 0xb8b0, 0xc1b0, 0xc2b0, 0xc3b0, 0xc4b0, 0xc6b0, 0xc7b0, 0xc9b0, 0xcdb0,
    0xceb0, 0xcfb0, 0xd1b0, //
    // NVJPG
    0xb8d1, 0xc4d1, 0xc9d1, 0xcdd1, 0xcfd1, //
    // OFA
    0xb8fa, 0xc6fa, 0xc7fa, 0xc9fa, 0xcdfa, 0xcefa, 0xd1fa,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_leaves_compute_out() {
        assert!(!Caps::DEFAULT.has(COMPUTE));
        assert!(Caps::DEFAULT.has(GRAPHICS | VIDEO | UTILITY));
        assert_eq!(Caps::DEFAULT.to_string(), "graphics,video,utility");
    }

    #[test]
    fn a_list_parses_in_any_order_and_case() {
        let c = Caps::parse("Compute, graphics").expect("parses");
        assert_eq!(c.bits(), COMPUTE | GRAPHICS);
        assert_eq!(c.to_string(), "graphics,compute");
    }

    #[test]
    fn a_misspelt_or_empty_list_is_refused() {
        assert!(Caps::parse("graphics,cmopute").is_err());
        assert!(Caps::parse("").is_err());
        assert!(Caps::parse(",").is_err());
    }

    /// The bits are a contract with the guest driver's NVGPU_CAP_* defines.
    #[test]
    fn bits_match_the_guest_driver() {
        assert_eq!((COMPUTE, GRAPHICS, VIDEO, UTILITY), (1, 2, 4, 8));
    }

    #[test]
    fn classes_map_to_the_capability_that_serves_them() {
        assert_eq!(Caps::for_class(0xc797), Some(GRAPHICS)); // AMPERE_B
        assert_eq!(Caps::for_class(0xc7b7), Some(VIDEO)); // Ampere NVENC
        assert_eq!(Caps::for_class(0xc9b0), Some(VIDEO)); // Ada NVDEC
        assert_eq!(Caps::for_class(0xc7c0), None); // AMPERE_COMPUTE_B
        assert_eq!(Caps::for_class(0x0080), None); // NV01_DEVICE_0
    }

    #[test]
    fn no_class_is_in_both_lists() {
        for c in THREE_D_CLASSES {
            assert!(!VIDEO_CLASSES.contains(c), "{c:#x}");
        }
    }
}
