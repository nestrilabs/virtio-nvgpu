//! Log lines a guest can cause, at a rate the host can afford.
//!
//! Most of what the backend logs is a guest's doing: a refused NVKMS command,
//! a HELLO of the wrong protocol, a chain too short to answer on, an RM
//! control the host turned down. Each costs the guest microseconds and the
//! host a formatted line, so a guest posting them in a loop writes on the
//! order of ten megabytes a second -- into an unrotated file under the
//! repository's own launcher (scripts/run-guest.sh), or through journald,
//! which drops them only after parsing every one (S-20). None of those sites
//! can be trusted to stay quiet on their own, and there are over two hundred
//! of them.
//!
//! So the limit is applied once, in the logger, per call site: every site
//! (file and line) gets a token bucket of [`BURST`] lines refilled at
//! [`PER_SEC`] a second. A site over its budget is silent, and the next line
//! it is allowed says how many were dropped. A site that logs rarely never
//! notices; one a guest can drive is held to a few hundred bytes a second
//! however hard it is driven. Sites are code locations, so the table of
//! buckets is bounded by the binary, not by anything the guest sends.
//!
//! Only what reaches the log is counted: a record the level filter rejects
//! never gets here, so turning on `debug` for a chase is not throttled by
//! lines that were never printed before.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

/// Lines one call site may log back to back.
pub const BURST: f64 = 50.0;
/// Lines a second one call site may log once its burst is spent.
pub const PER_SEC: f64 = 10.0;

/// One call site's budget.
#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last: Instant,
    suppressed: u64,
    /// The site's level and module, for a report of what it dropped made
    /// when no line of its own follows (`flush`).
    level: log::Level,
    module: Option<&'static str>,
}

impl Bucket {
    fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            last: now,
            suppressed: 0,
            level: log::Level::Warn,
            module: None,
        }
    }

    /// Whether a line at `now` may go out; `Some(n)` if it may, with `n` the
    /// lines dropped since the last one that went out.
    fn admit(&mut self, now: Instant) -> Option<u64> {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * PER_SEC).min(BURST);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed += 1;
            None
        }
    }
}

/// A logger that passes each call site's records to `inner` at most at the
/// bucket rate.
pub struct RateLimited<L> {
    inner: L,
    sites: Mutex<HashMap<(&'static str, u32), Bucket>>,
}

impl<L: log::Log> RateLimited<L> {
    pub fn new(inner: L) -> Self {
        Self {
            inner,
            sites: Mutex::new(HashMap::new()),
        }
    }

    /// Whether `record` may go out now, and how many of its site's were
    /// dropped before it.
    fn admit(&self, record: &log::Record, now: Instant) -> Option<u64> {
        // A record built at run time has no static location; those are not
        // guest-driven call sites and go out unmetered.
        let (Some(file), Some(line)) = (record.file_static(), record.line()) else {
            return Some(0);
        };
        let mut sites = self.sites.lock().unwrap_or_else(|p| p.into_inner());
        let b = sites.entry((file, line)).or_insert_with(|| Bucket::new(now));
        b.level = record.level();
        b.module = record.module_path_static();
        b.admit(now)
    }

    /// Say how many lines each site dropped since its last one went out:
    /// a site that went quiet after its burst would otherwise never say.
    pub fn report_suppressed(&self) {
        let pending: Vec<_> = {
            let mut sites = self.sites.lock().unwrap_or_else(|p| p.into_inner());
            let mut v: Vec<_> = sites
                .iter_mut()
                .filter(|(_, b)| b.suppressed > 0)
                .map(|(&(file, line), b)| {
                    (file, line, b.level, b.module, std::mem::take(&mut b.suppressed))
                })
                .collect();
            v.sort_unstable_by_key(|&(f, l, ..)| (f, l));
            v
        };
        for (file, line, level, module, dropped) in pending {
            self.inner.log(
                &log::Record::builder()
                    .level(level)
                    .target(module.unwrap_or("ratelimit"))
                    .file_static(Some(file))
                    .line(Some(line))
                    .module_path_static(module)
                    .args(format_args!(
                        "({dropped} similar line(s) from here dropped by the log rate limit, \
                         and none since)"
                    ))
                    .build(),
            );
        }
    }
}

impl<L: log::Log> log::Log for RateLimited<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if !self.inner.enabled(record.metadata()) {
            return;
        }
        let Some(dropped) = self.admit(record, Instant::now()) else {
            return;
        };
        if dropped > 0 {
            self.inner.log(
                &log::Record::builder()
                    .level(record.level())
                    .target(record.target())
                    .file_static(record.file_static())
                    .line(record.line())
                    .module_path_static(record.module_path_static())
                    .args(format_args!(
                        "({dropped} similar line(s) from here dropped by the log rate limit)"
                    ))
                    .build(),
            );
        }
        self.inner.log(record);
    }

    /// Reports what each site dropped (`report_suppressed`), then flushes:
    /// the backend flushes at teardown, so the last word on a noisy guest
    /// is how noisy it was.
    fn flush(&self) {
        self.report_suppressed();
        self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_site_gets_its_burst_and_then_the_refill_rate() {
        let t0 = Instant::now();
        let mut b = Bucket::new(t0);
        for _ in 0..BURST as usize {
            assert_eq!(b.admit(t0), Some(0));
        }
        assert_eq!(b.admit(t0), None, "the burst is spent");
        assert_eq!(b.admit(t0), None);
        // A tenth of a second buys one line, which reports the two dropped.
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(b.admit(t1), Some(2));
        assert_eq!(b.admit(t1), None);
    }

    #[test]
    fn a_quiet_site_never_fills_past_its_burst() {
        let t0 = Instant::now();
        let mut b = Bucket::new(t0);
        let later = t0 + Duration::from_secs(3600);
        let mut n = 0;
        while b.admit(later).is_some() {
            n += 1;
        }
        assert_eq!(n, BURST as usize);
    }

    /// Collects what gets through.
    struct Sink(Mutex<Vec<String>>);
    impl log::Log for Sink {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            self.0.lock().unwrap().push(r.args().to_string());
        }
        fn flush(&self) {}
    }

    fn record<'a>(line: u32, level: log::Level, args: std::fmt::Arguments<'a>) -> log::Record<'a> {
        log::Record::builder()
            .level(level)
            .file_static(Some("guest.rs"))
            .line(Some(line))
            .args(args)
            .build()
    }

    #[test]

    #[cfg_attr(miri, ignore = "timing: Miri runs far slower than the refill")]
    fn one_noisy_site_does_not_silence_another() {
        use log::Log;
        let l = RateLimited::new(Sink(Mutex::new(Vec::new())));
        for _ in 0..1000 {
            l.log(&record(10, log::Level::Warn, format_args!("noisy")));
        }
        l.log(&record(20, log::Level::Warn, format_args!("other")));
        let got = l.inner.0.lock().unwrap();
        // The burst goes out (and perhaps a refill or two on a slow run),
        // nowhere near all of it; the other site is untouched.
        let noisy = got.iter().filter(|s| *s == "noisy").count();
        assert!(
            (BURST as usize..BURST as usize + 5).contains(&noisy),
            "{noisy}"
        );
        assert_eq!(got.last().map(String::as_str), Some("other"));
    }

    /// A site that went quiet after its burst says, at the flush, how many
    /// it dropped -- once.
    #[test]
    #[cfg_attr(miri, ignore = "timing: Miri runs far slower than the refill")]
    fn a_flush_reports_what_a_quiet_site_dropped() {
        use log::Log;
        let l = RateLimited::new(Sink(Mutex::new(Vec::new())));
        for _ in 0..(BURST as usize + 7) {
            l.log(&record(40, log::Level::Warn, format_args!("refused")));
        }
        l.log(&record(41, log::Level::Warn, format_args!("once")));
        l.flush();
        let got = l.inner.0.lock().unwrap().clone();
        let report: Vec<_> = got.iter().filter(|s| s.contains("dropped")).collect();
        assert_eq!(report.len(), 1, "{got:?}");
        // A slow run may refill a line or two; never more than were sent.
        let n: u64 = report[0].trim_start_matches('(').split(' ').next().unwrap().parse().unwrap();
        assert!((1..=7).contains(&n), "{}", report[0]);
        assert!(report[0].contains("none since"));
        drop(got);
        l.flush();
        let again = l.inner.0.lock().unwrap();
        assert_eq!(again.iter().filter(|s| s.contains("dropped")).count(), 1);
    }

    #[test]
    fn a_filtered_out_record_spends_nothing() {
        use log::Log;
        let l = RateLimited::new(Sink(Mutex::new(Vec::new())));
        for _ in 0..1000 {
            l.log(&record(30, log::Level::Debug, format_args!("hidden")));
        }
        assert!(l.sites.lock().unwrap().is_empty());
        assert!(l.inner.0.lock().unwrap().is_empty());
    }
}
