// SPDX-License-Identifier: Apache-2.0
//! The daemon's log levels.
//!
//! Everything goes to stderr (the guest's journal, under a service manager),
//! each line at a level: `error` (the daemon cannot go on, or a call that
//! should not fail did), `warn` (a client or the host ended a connection, a
//! budget was hit), `info` (what the daemon is serving), `debug`. The level
//! is `info` unless `NVGPU_WL_LOG` or `--log` says otherwise. Lines a guest
//! client can cause, the accept failures it can bring about among them, are
//! also rate-limited, per call site (`daemon::Logs`), whatever the level;
//! what goes through `say` here unmetered is said once per run (start,
//! configuration, a fatal error).

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicU8, Ordering};

/// How much to say, least first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    /// `error`, `warn`, `info` or `debug`.
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn from_u8(v: u8) -> Level {
        match v {
            0 => Level::Error,
            1 => Level::Warn,
            2 => Level::Info,
            _ => Level::Debug,
        }
    }
}

/// The default: every line the daemon prints unless told otherwise.
pub const DEFAULT: Level = Level::Info;

static LEVEL: AtomicU8 = AtomicU8::new(DEFAULT as u8);

/// Print lines at `level` and below from now on.
pub fn set(level: Level) {
    LEVEL.store(level as u8, Ordering::Relaxed);
}

/// The level in force.
pub fn level() -> Level {
    Level::from_u8(LEVEL.load(Ordering::Relaxed))
}

/// Whether a line at `level` is printed.
pub fn enabled(level: Level) -> bool {
    level <= self::level()
}

/// Print `line` to stderr if `level` is enabled.
pub fn say(level: Level, line: &str) {
    if enabled(level) {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_order() {
        assert_eq!(Level::parse("WARN"), Some(Level::Warn));
        assert_eq!(Level::parse(" debug "), Some(Level::Debug));
        assert_eq!(Level::parse("loud"), None);
        assert!(Level::Error < Level::Warn && Level::Warn < Level::Info);
        for l in [Level::Error, Level::Warn, Level::Info, Level::Debug] {
            assert_eq!(Level::from_u8(l as u8), l);
        }
    }
}
