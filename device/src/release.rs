// SPDX-License-Identifier: Apache-2.0
//! Which tables a host driver release gets, and whether each was measured
//! at it.
//!
//! Four tables stand between a guest and the host's driver, each measured
//! per release from NVIDIA's sources (gen/README.md): the RM allowlist
//! (`gen/rmallow`), the ABI profile (`gen/src/versions`), the NVKMS schema
//! (`gen/nvkms`) and the UVM blocks (`gen/uvm`). A host release none of them
//! was measured at is one whose controls, layouts and commands nobody has
//! looked at -- 580 added pointers to two controls the allowlist let
//! through -- so the backend refuses to start on it (`Coverage::measured`)
//! unless told `--allow-unmeasured-release`, and then runs on the nearest
//! older tables, saying so at every start. What "measured" means per table:
//!
//! - the RM allowlist: this very release (`gen/rmallow/<release>.json`);
//! - the NVKMS schema: this very release (`gen/nvkms/<release>.json`);
//! - the ABI profile: a profile at or below it, and the release no newer
//!   than `abi::versions::MEASURED_THROUGH` (profiles are ranges by design);
//! - the UVM blocks: a table whose range holds it (`uvm_extract.py scan`
//!   proves the ranges; the last ends at the newest release measured).
//!
//! A release older than every table has nothing to fall back on and is
//! refused whatever the flags.

#![forbid(unsafe_code)]

use abi::version::DriverVersion;

/// One table's verdict for a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// Measured at (or, for a range table, proven over) this release.
    Measured(String),
    /// Only the nearest older table: `--allow-unmeasured-release` territory.
    Nearest(String),
    /// Nothing at all.
    None,
}

impl Pick {
    fn measured(&self) -> bool {
        matches!(self, Pick::Measured(_))
    }

    fn describe(&self) -> String {
        match self {
            Pick::Measured(s) => s.clone(),
            Pick::Nearest(s) => format!("{s} (NEAREST OLDER, unmeasured)"),
            Pick::None => "NONE".into(),
        }
    }
}

/// What a host release gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub version: DriverVersion,
    pub rmallow: Pick,
    pub abi: Pick,
    pub nvkms: Pick,
    pub uvm: Pick,
}

impl Coverage {
    pub fn of(v: DriverVersion) -> Self {
        let rmallow = match abi::rmallow::release_for(v) {
            Some((r, true)) => Pick::Measured(format!("{}", r.version())),
            Some((r, false)) => Pick::Nearest(format!("{}", r.version())),
            None => Pick::None,
        };
        let abi = match (
            abi::versions::table_for(v),
            abi::versions::nearest_profile_version(v),
        ) {
            (Some(_), Some(p)) => Pick::Measured(format!("{p}")),
            (None, Some(p)) => Pick::Nearest(format!("{p}")),
            _ => Pick::None,
        };
        let nvkms = match crate::schema::modeset_table(v) {
            Some(t) if crate::schema::modeset_table_exact(v) => Pick::Measured(t.name.into()),
            Some(t) => Pick::Nearest(t.name.into()),
            None => Pick::None,
        };
        let uvm = match crate::schema::uvm_table(v) {
            Some(t) => Pick::Measured(t.name.into()),
            // Past the last table: the guest has none either (its copy of the
            // ranges is the same), so compute is refused, flag or no flag.
            None => Pick::None,
        };
        Self {
            version: v,
            rmallow,
            abi,
            nvkms,
            uvm,
        }
    }

    /// Every table measured at this release.
    pub fn measured(&self) -> bool {
        self.rmallow.measured() && self.abi.measured() && self.nvkms.measured() && self.uvm.measured()
    }

    /// Whether the backend can run at all, if told to run unmeasured: every
    /// table but UVM (compute) has something at or below this release.
    pub fn runnable(&self) -> bool {
        ![&self.rmallow, &self.abi, &self.nvkms]
            .iter()
            .any(|p| **p == Pick::None)
    }

    /// The tables, on one line.
    pub fn summary(&self) -> String {
        format!(
            "host driver {}: RM allowlist {}, ABI profile {}, NVKMS schema {}, UVM table {}",
            self.version,
            self.rmallow.describe(),
            self.abi.describe(),
            self.nvkms.describe(),
            self.uvm.describe(),
        )
    }

    /// What is not measured, for the refusal.
    pub fn unmeasured(&self) -> Vec<&'static str> {
        [
            ("the RM allowlist", &self.rmallow),
            ("the ABI profile", &self.abi),
            ("the NVKMS schema", &self.nvkms),
            ("the UVM table", &self.uvm),
        ]
        .into_iter()
        .filter(|(_, p)| !p.measured())
        .map(|(n, _)| n)
        .collect()
    }
}

/// The releases every table was measured at, for the refusal's hint.
pub fn measured_releases() -> Vec<DriverVersion> {
    abi::rmallow::RELEASES
        .iter()
        .map(|r| r.version())
        .filter(|&v| Coverage::of(v).measured())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: u32, b: u32, c: u32) -> DriverVersion {
        DriverVersion::new(a, b, c)
    }

    /// The rig runs 595.99.02; every table was measured there.
    #[test]
    fn the_rigs_release_is_measured_by_every_table() {
        let c = Coverage::of(v(595, 99, 2));
        assert!(c.measured(), "{}", c.summary());
        assert_eq!(c.rmallow, Pick::Measured("595.99.02".into()));
        assert_eq!(c.abi, Pick::Measured("595.71.05".into()));
        assert_eq!(c.nvkms, Pick::Measured("v595_99_02".into()));
        assert!(c.unmeasured().is_empty());
    }

    #[test]
    fn every_release_the_allowlist_measured_is_measured_by_every_table() {
        let all = measured_releases();
        assert_eq!(all.len(), abi::rmallow::RELEASES.len(), "{all:?}");
    }

    #[test]
    fn a_release_between_two_measured_ones_is_not_measured_but_runnable() {
        let c = Coverage::of(v(600, 1, 0));
        assert!(!c.measured());
        assert!(c.runnable());
        assert_eq!(c.rmallow, Pick::Nearest("595.99.02".into()));
        assert_eq!(c.unmeasured(), vec!["the RM allowlist", "the NVKMS schema"]);
        assert!(c.summary().contains("NEAREST OLDER"), "{}", c.summary());
    }

    #[test]
    fn a_release_newer_than_every_one_measured_is_not_measured_and_has_no_uvm() {
        let c = Coverage::of(v(620, 30, 0));
        assert!(!c.measured());
        assert!(c.runnable());
        assert_eq!(c.abi, Pick::Nearest("595.71.05".into()));
        assert_eq!(c.uvm, Pick::None);
        assert_eq!(c.unmeasured().len(), 4);
    }

    #[test]
    fn a_release_older_than_every_one_measured_cannot_run() {
        let c = Coverage::of(v(470, 256, 2));
        assert!(!c.measured());
        assert!(!c.runnable());
        assert_eq!(c.rmallow, Pick::None);
    }
}
