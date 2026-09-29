// SPDX-License-Identifier: Apache-2.0
//! Frame-pacing counters: what crosses the boundary, how often, and how long
//! each crossing takes on this side (ARCHITECTURE.md, "Frame pacing").
//!
//! A frame of a game in a guest is paced by the round trips on its present
//! path -- the explicit-sync calls, the fence and syncobj waits, the Wayland
//! messages -- and by how soon a host event (a fence signalling, an RM
//! notifier, a compositor's frame callback) reaches the guest. These
//! counters say which: the rate of each kind of message, how long the
//! backend held each before answering, how the guest's waits went (ready at
//! the first poll, or registered and woken), and how long an event took from
//! the host to the event queue.
//!
//! Always on, and cheap enough to be: relaxed atomics and two clock reads a
//! message. Nothing here is a guest-sized allocation -- every table is fixed,
//! and the IOCTL2 names are the schema's own `&'static str`s, a closed set.
//! One process serves one VM, so the counters are the process's.
//!
//! [`summary`] is logged at teardown; `--pacing-stats SECS` also logs the
//! change every SECS seconds, and `NVGPU_PACING_STATS=SECS` does the same.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

/// Histogram buckets: bucket `i` holds times in `[2^(i-1), 2^i)` µs (bucket
/// 0 is under 1 µs), the last everything from 2^(BUCKETS-2) µs up.
pub const BUCKETS: usize = 18;

/// A latency histogram in powers of two of a microsecond, with the count,
/// sum and maximum beside it.
pub struct Hist {
    b: [AtomicU64; BUCKETS],
    n: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

#[allow(clippy::declare_interior_mutable_const)]
const ZERO: AtomicU64 = AtomicU64::new(0);

impl Hist {
    pub const fn new() -> Self {
        Self {
            b: [ZERO; BUCKETS],
            n: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    pub fn record_ns(&self, ns: u64) {
        self.b[bucket(ns)].fetch_add(1, Relaxed);
        self.n.fetch_add(1, Relaxed);
        self.sum_ns.fetch_add(ns, Relaxed);
        self.max_ns.fetch_max(ns, Relaxed);
    }

    pub fn record(&self, d: Duration) {
        self.record_ns(u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
    }

    pub fn since(&self, t0: Instant) {
        self.record(t0.elapsed());
    }

    pub fn snap(&self) -> HistSnap {
        HistSnap {
            b: std::array::from_fn(|i| self.b[i].load(Relaxed)),
            n: self.n.load(Relaxed),
            sum_ns: self.sum_ns.load(Relaxed),
            max_ns: self.max_ns.load(Relaxed),
        }
    }
}

impl Default for Hist {
    fn default() -> Self {
        Self::new()
    }
}

/// The bucket for `ns`.
pub fn bucket(ns: u64) -> usize {
    let us = ns / 1000;
    if us == 0 {
        0
    } else {
        ((64 - us.leading_zeros()) as usize).min(BUCKETS - 1)
    }
}

/// A histogram read at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HistSnap {
    pub b: [u64; BUCKETS],
    pub n: u64,
    pub sum_ns: u64,
    /// The largest ever (not per interval: a maximum does not subtract).
    pub max_ns: u64,
}

impl HistSnap {
    /// What happened between `prev` and this one.
    pub fn since(&self, prev: &HistSnap) -> HistSnap {
        HistSnap {
            b: std::array::from_fn(|i| self.b[i].saturating_sub(prev.b[i])),
            n: self.n.saturating_sub(prev.n),
            sum_ns: self.sum_ns.saturating_sub(prev.sum_ns),
            max_ns: self.max_ns,
        }
    }

    pub fn mean_us(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum_ns as f64 / self.n as f64 / 1000.0
        }
    }

    /// The upper edge (µs) of the bucket the `p`th percentile falls in: an
    /// upper bound, within a factor of two.
    pub fn pct_us(&self, p: f64) -> u64 {
        if self.n == 0 {
            return 0;
        }
        let want = ((self.n as f64) * p / 100.0).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (i, &c) in self.b.iter().enumerate() {
            seen += c;
            if seen >= want {
                return 1u64 << i;
            }
        }
        1u64 << (BUCKETS - 1)
    }

    /// Of the samples, how many took at least `us` microseconds (by bucket:
    /// `us` rounded down to a power of two).
    pub fn at_least_us(&self, us: u64) -> u64 {
        let from = bucket(us.saturating_mul(1000));
        self.b[from..].iter().sum()
    }

    /// "n=.. mean=..us p50<=..us p99<=..us max=..us".
    pub fn fmt(&self) -> String {
        format!(
            "n={} mean={:.1}us p50<={}us p99<={}us p99.9<={}us max={:.1}us",
            self.n,
            self.mean_us(),
            self.pct_us(50.0),
            self.pct_us(99.0),
            self.pct_us(99.9),
            self.max_ns as f64 / 1000.0
        )
    }
}

/// What the transport answers, by message type (`protocol::messages::MsgType`,
/// 1..=17; 0 for anything else).
pub const MSG_KINDS: usize = 18;

/// HOST_OP ops (`protocol::messages::OP_*`, 1..=13; 0 for anything else).
pub const HOST_OPS: usize = 16;

/// Every counter of the process.
pub struct Counters {
    /// Messages served, by type.
    pub msgs: [AtomicU64; MSG_KINDS],
    /// From the chain leaving the ring to its completion being in the used
    /// ring, by message type: the backend's part of a guest round trip.
    pub service: [Hist; MSG_KINDS],
    /// HOST_OPs by op.
    pub host_ops: [AtomicU64; HOST_OPS],
    /// IOCTL2s run on an executor rather than inline on the queue thread.
    pub ioctl2_executor: AtomicU64,
    /// SYNCOBJ_WAIT and SYNCOBJ_TIMELINE_WAIT, which run as polls
    /// (fence.rs): ready at once, not yet (-ETIME), or another error.
    pub wait_ready: AtomicU64,
    pub wait_etime: AtomicU64,
    pub wait_error: AtomicU64,
    /// HOST_OP SYNCOBJ_WATCH: a new host registration, a join of one, or
    /// refused over the cap (the guest then polls with its backoff).
    pub watch_new: AtomicU64,
    pub watch_joined: AtomicU64,
    pub watch_over_cap: AtomicU64,
    /// Readiness the pump found on an edge (or on arming), and what only
    /// its level sweep found: a level-readable descriptor the guest had not
    /// drained, reported again.
    pub ev_edge: AtomicU64,
    pub ev_sweep: AtomicU64,
    /// Records delivered: readiness, fence completions, DRM event bytes.
    pub ev_ready: AtomicU64,
    pub ev_fence: AtomicU64,
    pub ev_drm: AtomicU64,
    /// From a host sync_file signalling (its own timestamp) to its record
    /// going into a guest buffer.
    pub fence_delivery: Hist,
    /// From the pump waking with readiness to the record going into a guest
    /// buffer.
    pub pump_delivery: Hist,
    /// Flushes that found records waiting and no buffer posted: the guest
    /// had not handed buffers back yet, and delivery waits for its kick.
    pub no_buffer: AtomicU64,
    /// Sweeps the pump ran (each at most one a millisecond).
    pub sweeps: AtomicU64,
    /// Kicks on the event queue (the guest handing buffers back after the
    /// pump asked for one).
    pub event_kicks: AtomicU64,
    /// Chains the queue thread found by looking at the ring after a drain
    /// (`--queue-poll-us`), each a guest kick and a wakeup saved.
    pub queue_polled: AtomicU64,
    /// Armed-readiness mode: the guest's arms, and host events on legacy
    /// handles nobody waited on (each a record, and often a guest
    /// interrupt, that did not have to be sent).
    pub arms: AtomicU64,
    pub ev_unarmed: AtomicU64,
    /// IOCTL2s by schema name.
    ioctl2_names: Mutex<BTreeMap<&'static str, [u64; 2]>>,
}

impl Counters {
    pub const fn new() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const H: Hist = Hist::new();
        Self {
            msgs: [ZERO; MSG_KINDS],
            service: [H; MSG_KINDS],
            host_ops: [ZERO; HOST_OPS],
            ioctl2_executor: AtomicU64::new(0),
            wait_ready: AtomicU64::new(0),
            wait_etime: AtomicU64::new(0),
            wait_error: AtomicU64::new(0),
            watch_new: AtomicU64::new(0),
            watch_joined: AtomicU64::new(0),
            watch_over_cap: AtomicU64::new(0),
            ev_edge: AtomicU64::new(0),
            ev_sweep: AtomicU64::new(0),
            ev_ready: AtomicU64::new(0),
            ev_fence: AtomicU64::new(0),
            ev_drm: AtomicU64::new(0),
            fence_delivery: Hist::new(),
            pump_delivery: Hist::new(),
            no_buffer: AtomicU64::new(0),
            sweeps: AtomicU64::new(0),
            event_kicks: AtomicU64::new(0),
            queue_polled: AtomicU64::new(0),
            arms: AtomicU64::new(0),
            ev_unarmed: AtomicU64::new(0),
            ioctl2_names: Mutex::new(BTreeMap::new()),
        }
    }

    /// A message of type `t` (its wire value) was answered, `t0` being when
    /// its chain was taken off the ring.
    pub fn served(&self, t: u32, t0: Instant) {
        let i = if (t as usize) < MSG_KINDS { t as usize } else { 0 };
        self.msgs[i].fetch_add(1, Relaxed);
        self.service[i].since(t0);
    }

    pub fn host_op(&self, op: u32) {
        let i = if (op as usize) < HOST_OPS { op as usize } else { 0 };
        self.host_ops[i].fetch_add(1, Relaxed);
    }

    /// An IOCTL2 named `name` finished with host result `result`.
    pub fn ioctl2(&self, name: &'static str, result: Option<i32>) {
        let failed = !matches!(result, Some(0));
        if let Ok(mut m) = self.ioctl2_names.lock() {
            let e = m.entry(name).or_insert([0, 0]);
            e[0] += 1;
            e[1] += failed as u64;
        }
        if matches!(name, "SYNCOBJ_WAIT" | "SYNCOBJ_TIMELINE_WAIT") {
            match result {
                Some(0) => &self.wait_ready,
                Some(e) if e == -libc::ETIME || e == libc::ETIME => &self.wait_etime,
                _ => &self.wait_error,
            }
            .fetch_add(1, Relaxed);
        }
    }

    pub fn snap(&self) -> Snap {
        let load = |a: &AtomicU64| a.load(Relaxed);
        Snap {
            at: Instant::now(),
            msgs: std::array::from_fn(|i| load(&self.msgs[i])),
            service: std::array::from_fn(|i| self.service[i].snap()),
            host_ops: std::array::from_fn(|i| load(&self.host_ops[i])),
            ioctl2_executor: load(&self.ioctl2_executor),
            wait: [
                load(&self.wait_ready),
                load(&self.wait_etime),
                load(&self.wait_error),
            ],
            watch: [
                load(&self.watch_new),
                load(&self.watch_joined),
                load(&self.watch_over_cap),
            ],
            ev: [
                load(&self.ev_edge),
                load(&self.ev_sweep),
                load(&self.ev_ready),
                load(&self.ev_fence),
                load(&self.ev_drm),
            ],
            fence_delivery: self.fence_delivery.snap(),
            pump_delivery: self.pump_delivery.snap(),
            no_buffer: load(&self.no_buffer),
            sweeps: load(&self.sweeps),
            kicks: [load(&self.event_kicks), load(&self.queue_polled)],
            armed: [load(&self.arms), load(&self.ev_unarmed)],
            ioctl2_names: self
                .ioctl2_names
                .lock()
                .map(|m| m.clone())
                .unwrap_or_default(),
        }
    }
}

impl Default for Counters {
    fn default() -> Self {
        Self::new()
    }
}

/// The process's counters.
pub static PACING: Counters = Counters::new();

/// Every counter at one moment.
#[derive(Clone, Debug)]
pub struct Snap {
    pub at: Instant,
    pub msgs: [u64; MSG_KINDS],
    pub service: [HistSnap; MSG_KINDS],
    pub host_ops: [u64; HOST_OPS],
    pub ioctl2_executor: u64,
    /// ready, etime, error
    pub wait: [u64; 3],
    /// new, joined, over the cap
    pub watch: [u64; 3],
    /// edge, sweep, ready records, fence records, drm records
    pub ev: [u64; 5],
    pub fence_delivery: HistSnap,
    pub pump_delivery: HistSnap,
    pub no_buffer: u64,
    pub sweeps: u64,
    /// event-queue kicks, chains found by polling the control ring
    pub kicks: [u64; 2],
    /// arms, legacy events while unarmed (not sent)
    pub armed: [u64; 2],
    pub ioctl2_names: BTreeMap<&'static str, [u64; 2]>,
}

/// The names the report gives message types (MsgType's wire values).
const MSG_NAMES: [&str; MSG_KINDS] = [
    "other",
    "open",
    "close",
    "ioctl",
    "mmap",
    "munmap",
    "proc_files",
    "sys_files",
    "event_ready",
    "hello",
    "ioctl2",
    "time_sync",
    "event_data",
    "watch",
    "unwatch",
    "host_op",
    "wl_send",
    "wl_recv",
];

const OP_NAMES: [&str; HOST_OPS] = [
    "other",
    "prime_export",
    "dmabuf_import",
    "sync_merge",
    "new_eventfd",
    "fd_kind",
    "signaled_sync_file",
    "open_kms",
    "drop_if_master",
    "close_many",
    "syncobj_watch",
    "osdesc_reap",
    "inject_open",
    "inject_open_syncobj",
    "op14",
    "op15",
];

/// The report of what happened between `prev` (None: since the start) and
/// `now`, as log lines: rates per second, the service time of each message
/// type that was used, the waits, the events and their latency, and the
/// IOCTL2s by name.
pub fn summary(prev: Option<&Snap>, now: &Snap, start: Instant) -> Vec<String> {
    let secs = now
        .at
        .duration_since(prev.map_or(start, |p| p.at))
        .as_secs_f64()
        .max(1e-3);
    let d = |a: u64, b: Option<u64>| a.saturating_sub(b.unwrap_or(0));
    let rate = |n: u64| n as f64 / secs;
    let mut out = Vec::new();

    let mut line = format!("pacing: {secs:.1}s:");
    let mut total = 0;
    for (i, name) in MSG_NAMES.iter().enumerate() {
        let n = d(now.msgs[i], prev.map(|p| p.msgs[i]));
        total += n;
        if n != 0 {
            let _ = write!(line, " {name}={:.0}/s", rate(n));
        }
    }
    let _ = write!(line, " (all {:.0}/s)", rate(total));
    out.push(line);

    for (i, name) in MSG_NAMES.iter().enumerate() {
        let h = match prev {
            Some(p) => now.service[i].since(&p.service[i]),
            None => now.service[i],
        };
        if h.n != 0 {
            out.push(format!("pacing: service {name}: {}", h.fmt()));
        }
    }

    let mut ops = String::new();
    for (i, name) in OP_NAMES.iter().enumerate() {
        let n = d(now.host_ops[i], prev.map(|p| p.host_ops[i]));
        if n != 0 {
            let _ = write!(ops, " {name}={:.0}/s", rate(n));
        }
    }
    let w: [u64; 3] = std::array::from_fn(|i| d(now.wait[i], prev.map(|p| p.wait[i])));
    let r: [u64; 3] = std::array::from_fn(|i| d(now.watch[i], prev.map(|p| p.watch[i])));
    out.push(format!(
        "pacing: host_op{}; syncobj waits (polls) ready={:.0}/s not-yet={:.0}/s error={:.0}/s; \
         watch new={:.0}/s joined={:.0}/s over-cap={}; ioctl2 on an executor {:.0}/s",
        if ops.is_empty() { " none" } else { &ops },
        rate(w[0]),
        rate(w[1]),
        rate(w[2]),
        rate(r[0]),
        rate(r[1]),
        r[2],
        rate(d(now.ioctl2_executor, prev.map(|p| p.ioctl2_executor))),
    ));

    let e: [u64; 5] = std::array::from_fn(|i| d(now.ev[i], prev.map(|p| p.ev[i])));
    let (fd, pd) = match prev {
        Some(p) => (
            now.fence_delivery.since(&p.fence_delivery),
            now.pump_delivery.since(&p.pump_delivery),
        ),
        None => (now.fence_delivery, now.pump_delivery),
    };
    out.push(format!(
        "pacing: events: readiness on an edge {:.0}/s, found by the sweep {:.0}/s ({:.0} sweeps/s); \
         records ready={:.0}/s fence={:.0}/s drm={:.0}/s; no guest buffer {}; event-queue kicks \
         {:.0}/s; chains found by polling the ring {:.0}/s",
        rate(e[0]),
        rate(e[1]),
        rate(d(now.sweeps, prev.map(|p| p.sweeps))),
        rate(e[2]),
        rate(e[3]),
        rate(e[4]),
        d(now.no_buffer, prev.map(|p| p.no_buffer)),
        rate(d(now.kicks[0], prev.map(|p| p.kicks[0]))),
        rate(d(now.kicks[1], prev.map(|p| p.kicks[1]))),
    ));
    out.push(format!(
        "pacing: legacy readiness: arms {:.0}/s, events nobody waited on {:.0}/s (not sent)",
        rate(d(now.armed[0], prev.map(|p| p.armed[0]))),
        rate(d(now.armed[1], prev.map(|p| p.armed[1]))),
    ));
    out.push(format!("pacing: fence signalled -> queued: {}", fd.fmt()));
    out.push(format!("pacing: pump woke -> queued: {}", pd.fmt()));

    let mut names: Vec<(u64, &str, u64)> = now
        .ioctl2_names
        .iter()
        .map(|(k, v)| {
            let p = prev.and_then(|p| p.ioctl2_names.get(k)).copied().unwrap_or([0, 0]);
            (v[0].saturating_sub(p[0]), *k, v[1].saturating_sub(p[1]))
        })
        .filter(|(n, _, _)| *n != 0)
        .collect();
    names.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    if !names.is_empty() {
        let mut l = String::from("pacing: ioctl2:");
        for (n, k, f) in names.iter().take(24) {
            let _ = write!(l, " {k}={:.0}/s", rate(*n));
            if *f != 0 {
                let _ = write!(l, "({f} failed)");
            }
        }
        out.push(l);
    }
    out
}

/// When the process's counters started (the first call).
pub fn start() -> Instant {
    static START: Mutex<Option<Instant>> = Mutex::new(None);
    let mut s = START.lock().unwrap_or_else(|e| e.into_inner());
    *s.get_or_insert_with(Instant::now)
}

/// Log [`summary`] every `every`, on a thread of its own, until the process
/// ends. For `--pacing-stats` / `NVGPU_PACING_STATS`.
pub fn spawn_reporter(every: Duration) -> std::io::Result<()> {
    let start = start();
    std::thread::Builder::new()
        .name("nvgpu-pacing".into())
        .spawn(move || {
            let mut prev = PACING.snap();
            loop {
                std::thread::sleep(every);
                let now = PACING.snap();
                let busy = now.msgs.iter().sum::<u64>() != prev.msgs.iter().sum::<u64>()
                    || now.ev != prev.ev;
                if busy {
                    for l in summary(Some(&prev), &now, start) {
                        log::warn!("{l}");
                    }
                }
                prev = now;
            }
        })
        .map(|_| ())
}

/// The whole run's report, for teardown.
pub fn log_summary() {
    let now = PACING.snap();
    if now.msgs.iter().sum::<u64>() == 0 {
        return;
    }
    for l in summary(None, &now, start()) {
        log::warn!("{l}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_are_powers_of_two_of_a_microsecond() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(999), 0);
        assert_eq!(bucket(1_000), 1);
        assert_eq!(bucket(1_999), 1);
        assert_eq!(bucket(2_000), 2);
        assert_eq!(bucket(3_999), 2);
        assert_eq!(bucket(4_000), 3);
        assert_eq!(bucket(1_000_000), 10); // 1 ms: [512, 1024) µs
        assert_eq!(bucket(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn a_histogram_reports_bounds_and_intervals() {
        let h = Hist::new();
        for _ in 0..98 {
            h.record_ns(3_000); // bucket 2, <= 4 us
        }
        h.record_ns(700_000); // bucket 10, <= 1024 us
        h.record_ns(5_000_000); // bucket 13, <= 8192 us
        let s = h.snap();
        assert_eq!(s.n, 100);
        assert_eq!(s.pct_us(50.0), 4);
        assert_eq!(s.pct_us(99.0), 1024);
        assert_eq!(s.pct_us(100.0), 8192);
        assert_eq!(s.max_ns, 5_000_000);
        assert_eq!(s.at_least_us(500), 2);
        let later = {
            h.record_ns(3_000);
            h.snap()
        };
        let d = later.since(&s);
        assert_eq!(d.n, 1);
        assert_eq!(d.b[2], 1);
        assert_eq!(d.pct_us(99.0), 4);
    }

    #[test]
    fn waits_are_told_apart_by_their_result() {
        let c = Counters::new();
        c.ioctl2("SYNCOBJ_WAIT", Some(0));
        c.ioctl2("SYNCOBJ_TIMELINE_WAIT", Some(-libc::ETIME));
        c.ioctl2("SYNCOBJ_TIMELINE_WAIT", Some(-libc::EINVAL));
        c.ioctl2("SYNCOBJ_SIGNAL", Some(0));
        let s = c.snap();
        assert_eq!(s.wait, [1, 1, 1]);
        assert_eq!(s.ioctl2_names["SYNCOBJ_TIMELINE_WAIT"], [2, 2]);
        assert_eq!(s.ioctl2_names["SYNCOBJ_SIGNAL"], [1, 0]);
    }

    #[test]
    fn the_report_gives_rates_and_names_what_was_used() {
        let c = Counters::new();
        let start = Instant::now();
        let t0 = Instant::now();
        c.served(10, t0); // ioctl2
        c.served(15, t0); // host_op
        c.served(99, t0); // out of range: "other"
        c.host_op(10);
        c.host_op(200);
        c.ioctl2("SYNCOBJ_WAIT", Some(-libc::ETIME));
        let lines = summary(None, &c.snap(), start).join("\n");
        assert!(lines.contains("ioctl2="), "{lines}");
        assert!(lines.contains("host_op="), "{lines}");
        assert!(lines.contains("other="), "{lines}");
        assert!(lines.contains("syncobj_watch="), "{lines}");
        assert!(lines.contains("service ioctl2: n=1"), "{lines}");
        assert!(lines.contains("not-yet="), "{lines}");
        assert!(lines.contains("SYNCOBJ_WAIT="), "{lines}");
        assert!(!lines.contains("service wl_send"), "{lines}");
    }

    #[test]
    fn an_interval_reports_only_what_happened_in_it() {
        let c = Counters::new();
        let start = Instant::now();
        c.served(10, Instant::now());
        let a = c.snap();
        c.served(16, Instant::now());
        let b = c.snap();
        let lines = summary(Some(&a), &b, start).join("\n");
        assert!(lines.contains("wl_send="), "{lines}");
        assert!(!lines.contains(" ioctl2="), "{lines}");
    }
}
