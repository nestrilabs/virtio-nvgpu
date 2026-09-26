// SPDX-License-Identifier: Apache-2.0
// gen/src/version.rs
//
// NVIDIA driver version representation and parsing.
//
// Ported from gVisor pkg/sentry/devices/nvproxy/version.go.

use core::fmt;

/// A parsed NVIDIA driver version, e.g. `535.129.03`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DriverVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl DriverVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Parse from the string returned by `NV_ESC_CHECK_VERSION_STR`.
    /// Expected format: `"535.129.03"`. NVIDIA numbers some releases in two
    /// parts (`"550.67"`, `"595.80"`); those are `.0`, so they fall in the
    /// range of the release below them like any other. The guest driver
    /// reads a version the same way (nvgpu_i2.c, `nvgpu_host_version`), and
    /// the two must agree: both pick their NVKMS and UVM tables by it.
    pub fn parse(s: &str) -> Option<Self> {
        let mut parts = s.trim().splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = match parts.next() {
            Some(p) => p.parse().ok()?,
            None => 0,
        };
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{:02}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let v = DriverVersion::new(535, 129, 3);
        assert_eq!(v.to_string(), "535.129.03");
    }

    #[test]
    fn parse_ok() {
        assert_eq!(
            DriverVersion::parse("535.129.03"),
            Some(DriverVersion::new(535, 129, 3))
        );
    }

    #[test]
    fn a_two_part_release_is_its_point_zero() {
        assert_eq!(
            DriverVersion::parse("595.80"),
            Some(DriverVersion::new(595, 80, 0))
        );
        assert_eq!(DriverVersion::parse("595"), None);
        assert_eq!(DriverVersion::parse("595.x"), None);
    }
}
