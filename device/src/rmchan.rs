// SPDX-License-Identifier: Apache-2.0
//! NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS: a guest stopping its own channels.
//!
//! NVIDIA's Vulkan driver stops its own channels with this control around
//! queue set-up and around each frame Blender starts, and goes on as if it
//! had whatever RM answers; refused, Blender's frames stop completing in
//! about two runs in five, natively as in a guest (BENCHMARKS.md, "Heavy
//! workloads"). So the RM allowlist has it (gen/rmallow/observed.txt), and
//! this module holds it to what a guest process may do with it:
//!
//! - **Its own channels only.** Every client `hClientList` names must be the
//!   calling guest process's (device/src/rmshare.rs, `CONTROL_CLIENT_LISTS`,
//!   `Rule::Process`), and a `numChannels` past the list's 64 entries refuses
//!   the call whole; each `hChannelList` entry is looked up by RM under its
//!   client, so it can name no one else's channel. GSP-RM itself checks no
//!   token here (open-gpu-kernel-modules 595.99.02,
//!   src/nvidia/src/kernel/gpu/fifo/kernel_fifo_ctrl.c
//!   subdeviceCtrlCmdFifoDisableChannels_IMPL sends the parameters to GSP as
//!   they are).
//! - **No preemption event.** `pRunlistPreemptEvent` is a kernel event RM
//!   takes only from a kernel client; a user client's is refused by RM with
//!   NV_ERR_INSUFFICIENT_PERMISSIONS. It must be NULL here, answered with
//!   that status without RM (gVisor's nvproxy refuses it too, with EINVAL:
//!   gVisor 5f20848, pkg/sentry/devices/nvproxy/frontend.go
//!   ctrlSubdevFIFODisableChannels).
//! - **Its size exactly.** 536 bytes in every release measured
//!   (gen/rmallow/*.json); the allowlist holds the size on a release
//!   measured exactly, and this on every host.
//! - **A rate.** A disable that is not `bOnlyDisableScheduling` preempts
//!   the channels off the GPU, and with them the runlist they share with
//!   every other VM and host program. Natively any process may do that as
//!   often as it likes; here each guest process gets a token bucket of
//!   [`PROC_RATE`] calls a second after a burst of [`PROC_BURST`], and the
//!   VM one of [`VM_RATE`] after [`VM_BURST`]. A process's bucket is charged
//!   for every call it asks for, served or not. The last [`VM_RESERVE`] of
//!   the VM's tokens go only to a process that has used at most
//!   [`RESERVE_FLOOR`] of its own recently, the call it is making included
//!   (fewer before it), so processes that ask past
//!   their rate cannot take the calls of one that makes a few (quota.rs has
//!   the same split for pools); it takes four processes each at its whole
//!   rate to reach the VM's. Every other field (`bDisable`,
//!   `bOnlyDisableScheduling`, `bRewindGpPut`) is a flag on the caller's
//!   own channels.
//!
//! A call over the rate is answered NV_ERR_NOT_SUPPORTED without RM: what
//! CPU-RM itself answers for this control on a GPU without GSP (the
//! function above), and what the allowlist answered before, which the
//! driver was seen to go on from without an error path of its own. The
//! rates are several times what any workload measured makes (BENCHMARKS.md,
//! "Heavy workloads").
//!
//! What this does not do: tell apart processes the guest kernel does not.
//! A guest process that forks gets a bucket a child, each within its rate;
//! enough of them take the VM's rate, reserve included, from the rest of
//! the VM -- a denial within one VM, which the guest's own process limits
//! bound, as for quota.rs's pools. The VM's bucket bounds them all against
//! other VMs. At most [`PROC_CAP`] processes hold a bucket not yet refilled;
//! a process new to the gate past that is refused until some refill.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::le;
use crate::nvos::{NV_ERR_INSUFFICIENT_PERMISSIONS, NV_ERR_INVALID_ARGUMENT, NV_ERR_NOT_SUPPORTED};
use crate::quota::Owner;

/// NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS.
pub const CTRL_FIFO_DISABLE_CHANNELS: u32 = 0x2080_110b;
/// sizeof(NV2080_CTRL_FIFO_DISABLE_CHANNELS_PARAMS), every release measured.
pub const PARAMS_SIZE: usize = 536;
/// `pRunlistPreemptEvent` (NvP64), after `bDisable`, `numChannels`,
/// `bOnlyDisableScheduling` and `bRewindGpPut`.
pub const RUNLIST_PREEMPT_EVENT: usize = 16;

/// Calls a second one guest process may make once its burst is spent.
pub const PROC_RATE: f64 = 50.0;
/// Calls one guest process may make back to back.
pub const PROC_BURST: f64 = 40.0;
/// Calls a second the VM may make, all its processes together.
pub const VM_RATE: f64 = 200.0;
/// Calls the VM may make back to back.
pub const VM_BURST: f64 = 160.0;
/// The last of the VM's tokens, kept for processes that have used little.
pub const VM_RESERVE: f64 = 40.0;
/// Tokens a process may have used, of its [`PROC_BURST`], and still take
/// from the reserve, the call that takes it included -- as a quota.rs floor
/// counts the request itself: a process that had used fewer than this
/// before the call is served from the reserve, one that had used this
/// many is not.
pub const RESERVE_FLOOR: f64 = 8.0;
/// Processes with a bucket past which the full ones are dropped (a full
/// bucket is a new one), at most once every [`PRUNE_EVERY`].
const PRUNE_AT: usize = 256;
const PRUNE_EVERY: Duration = Duration::from_millis(10);
/// Processes with a bucket not yet full, at most: a process new to the gate
/// past it is refused until some have refilled. A bucket charged once is
/// full again `1 / PROC_RATE` s later.
pub const PROC_CAP: usize = 1024;

// The budgets fit together: a process's burst and rate within the VM's, the
// reserve within the VM's burst, the floor within a process's burst.
const _: () = assert!(PROC_BURST <= VM_BURST - VM_RESERVE);
const _: () = assert!(PROC_RATE < VM_RATE);
const _: () = assert!(RESERVE_FLOOR < PROC_BURST && RESERVE_FLOOR < VM_RESERVE);

/// Why a call was answered here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Parameters of any size but RM's.
    Size,
    /// A non-NULL `pRunlistPreemptEvent`.
    PreemptEvent,
    /// The calling process is over its rate.
    ProcRate,
    /// The VM is over its rate, or only its reserve is left and the process
    /// has used more than the floor.
    VmRate,
}

impl Refused {
    /// The status the caller gets.
    pub fn status(self) -> u32 {
        match self {
            Refused::Size => NV_ERR_INVALID_ARGUMENT,
            Refused::PreemptEvent => NV_ERR_INSUFFICIENT_PERMISSIONS,
            Refused::ProcRate | Refused::VmRate => NV_ERR_NOT_SUPPORTED,
        }
    }

    pub fn why(self) -> &'static str {
        match self {
            Refused::Size => "parameters of a size RM does not take",
            Refused::PreemptEvent => {
                "a runlist preemption event, which RM takes only from the kernel"
            }
            Refused::ProcRate => "the calling guest process is over its rate",
            Refused::VmRate => "the VM is over its rate",
        }
    }
}

/// A token bucket.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl Bucket {
    fn full(burst: f64, now: Instant) -> Self {
        Bucket {
            tokens: burst,
            last: now,
        }
    }

    fn refill(&mut self, rate: f64, burst: f64, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * rate).min(burst);
        self.last = now;
    }
}

/// The budgets of one VM's calls.
#[derive(Debug)]
pub struct ChannelGate {
    vm: Bucket,
    procs: HashMap<Owner, Bucket>,
    pruned: Instant,
    /// Calls answered here, for the teardown report.
    pub refused: u64,
}

impl ChannelGate {
    pub fn new(now: Instant) -> Self {
        ChannelGate {
            vm: Bucket::full(VM_BURST, now),
            procs: HashMap::new(),
            pruned: now,
            refused: 0,
        }
    }

    /// Whether a DISABLE_CHANNELS with parameters `params` (as the guest sent
    /// them, `params_size` the size it said) from `owner` goes to RM at
    /// `now`. The clients it names are rmshare.rs's to check.
    pub fn check(
        &mut self,
        owner: Owner,
        params: &[u8],
        params_size: u32,
        now: Instant,
    ) -> Result<(), Refused> {
        let r = self.judge(owner, params, params_size, now);
        if r.is_err() {
            self.refused += 1;
        }
        r
    }

    fn judge(
        &mut self,
        owner: Owner,
        params: &[u8],
        params_size: u32,
        now: Instant,
    ) -> Result<(), Refused> {
        if params_size as usize != PARAMS_SIZE || params.len() < PARAMS_SIZE {
            return Err(Refused::Size);
        }
        if le::u64_at(params, RUNLIST_PREEMPT_EVENT) != Some(0) {
            return Err(Refused::PreemptEvent);
        }
        self.vm.refill(VM_RATE, VM_BURST, now);
        // A process's bucket is charged for every call it asks for, served
        // or not: one that asks past its rate stays empty, and so out of the
        // VM's reserve, whatever the VM itself had left for it.
        let used = match owner {
            // A guest that does not say which process calls: the VM's
            // budget alone, as quota.rs holds such calls to the VM's caps.
            Owner::Unknown => 0.0,
            Owner::Proc { .. } => {
                if !self.procs.contains_key(&owner) {
                    if self.procs.len() >= PRUNE_AT
                        && now.saturating_duration_since(self.pruned) >= PRUNE_EVERY
                    {
                        self.prune(now);
                        self.pruned = now;
                    }
                    // More processes calling at once than a guest has any
                    // use for: none of them gets a bucket until some refill.
                    if self.procs.len() >= PROC_CAP {
                        return Err(Refused::VmRate);
                    }
                }
                let b = self
                    .procs
                    .entry(owner)
                    .or_insert_with(|| Bucket::full(PROC_BURST, now));
                b.refill(PROC_RATE, PROC_BURST, now);
                if b.tokens < 1.0 {
                    return Err(Refused::ProcRate);
                }
                let used = PROC_BURST - b.tokens;
                b.tokens -= 1.0;
                used
            }
        };
        if self.vm.tokens < 1.0 || (self.vm.tokens < VM_RESERVE + 1.0 && used >= RESERVE_FLOOR) {
            return Err(Refused::VmRate);
        }
        self.vm.tokens -= 1.0;
        Ok(())
    }

    /// Drop the buckets that have refilled: a full bucket and none are the
    /// same.
    fn prune(&mut self, now: Instant) {
        self.procs.retain(|_, b| {
            b.refill(PROC_RATE, PROC_BURST, now);
            b.tokens < PROC_BURST
        });
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.procs.len()
    }
}

// ─────────────────────────────── the backend ───────────────────────────────

#[cfg(test)]
std::thread_local! {
    /// The time the backend's gate sees on this thread, when a test has
    /// stopped it: a test counting calls against a rate must not depend on
    /// how fast it runs (under Miri, or on a loaded machine, a burst takes
    /// longer than one token's refill).
    pub(crate) static FROZEN: std::cell::Cell<Option<Instant>> =
        const { std::cell::Cell::new(None) };
}

fn now() -> Instant {
    #[cfg(test)]
    if let Some(t) = FROZEN.with(|f| f.get()) {
        return t;
    }
    Instant::now()
}

impl crate::nvidia::NvidiaBackend {
    /// The guest process an RM escape is charged to: the caller when the
    /// guest says which (rmshare.rs `rm_proc_id`), else the process that
    /// opened the file.
    pub(crate) fn rm_caller(&self) -> Owner {
        match self.current_proc {
            Some(c) => Owner::Proc {
                tgid: c.tgid,
                start_ns: c.start_ns,
            },
            None => self.current_owner,
        }
    }

    /// An RM_CONTROL's parameters as the guest sent them (the NVOS54 block
    /// and the nested one): `Err` is the status to answer a
    /// DISABLE_CHANNELS with, RM never called. Any other control passes.
    /// Charged to [`Self::rm_caller`].
    pub(crate) fn rm_chan_gate(&mut self, params: &[u8]) -> Result<(), u32> {
        use crate::nvos::{NVOS54_CMD, NVOS54_PARAMS_SIZE, NVOS54_SIZE};
        if le::u32_at(params, NVOS54_CMD) != Some(CTRL_FIFO_DISABLE_CHANNELS) {
            return Ok(());
        }
        let size = le::u32_at(params, NVOS54_PARAMS_SIZE).unwrap_or(0);
        let ctl = params.get(NVOS54_SIZE..).unwrap_or(&[]);
        let owner = self.rm_caller();
        self.rmchan.check(owner, ctl, size, now()).map_err(|r| {
            log::warn!("NV2080_CTRL_CMD_FIFO_DISABLE_CHANNELS refused: {}", r.why());
            r.status()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(event: u64) -> Vec<u8> {
        let mut p = vec![0u8; PARAMS_SIZE];
        p[0] = 1; // bDisable
        p[4..8].copy_from_slice(&1u32.to_le_bytes());
        p[RUNLIST_PREEMPT_EVENT..RUNLIST_PREEMPT_EVENT + 8].copy_from_slice(&event.to_le_bytes());
        p
    }

    fn proc(tgid: u32) -> Owner {
        Owner::Proc {
            tgid,
            start_ns: 1000 + tgid as u64,
        }
    }

    #[test]
    fn the_size_is_rms_exactly() {
        let t = Instant::now();
        let mut g = ChannelGate::new(t);
        let p = params(0);
        assert_eq!(g.check(proc(1), &p, 536, t), Ok(()));
        assert_eq!(g.check(proc(1), &p, 535, t), Err(Refused::Size));
        assert_eq!(g.check(proc(1), &p, 537, t), Err(Refused::Size));
        assert_eq!(g.check(proc(1), &p[..535], 536, t), Err(Refused::Size));
        assert_eq!(Refused::Size.status(), NV_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn a_preemption_event_is_refused_as_rm_refuses_a_user_client() {
        let t = Instant::now();
        let mut g = ChannelGate::new(t);
        for ev in [1u64, 0xffff_8880_0000_0000, 1 << 63] {
            assert_eq!(
                g.check(proc(1), &params(ev), 536, t),
                Err(Refused::PreemptEvent)
            );
        }
        assert_eq!(
            Refused::PreemptEvent.status(),
            NV_ERR_INSUFFICIENT_PERMISSIONS
        );
        // Refusals take no tokens.
        for _ in 0..PROC_BURST as usize {
            assert_eq!(g.check(proc(1), &params(0), 536, t), Ok(()));
        }
    }

    #[test]
    fn a_process_under_its_rate_is_never_refused() {
        // Blender's heaviest: four calls a frame at 2.4 s a frame, and a
        // start-up burst of six in a millisecond; far under both budgets.
        let t0 = Instant::now();
        let mut g = ChannelGate::new(t0);
        let p = params(0);
        for i in 0..6 {
            assert_eq!(
                g.check(proc(1), &p, 536, t0 + Duration::from_micros(i * 200)),
                Ok(())
            );
        }
        // A sustained PROC_RATE, from a full bucket, for a minute.
        let step = Duration::from_secs_f64(1.0 / PROC_RATE);
        let mut t = t0 + Duration::from_secs(1);
        for _ in 0..(60.0 * PROC_RATE) as usize {
            assert_eq!(g.check(proc(2), &p, 536, t), Ok(()));
            t += step;
        }
    }

    #[test]
    fn a_process_over_its_rate_is_refused_and_comes_back() {
        let t = Instant::now();
        let mut g = ChannelGate::new(t);
        let p = params(0);
        for _ in 0..PROC_BURST as usize {
            assert_eq!(g.check(proc(1), &p, 536, t), Ok(()));
        }
        assert_eq!(g.check(proc(1), &p, 536, t), Err(Refused::ProcRate));
        assert_eq!(Refused::ProcRate.status(), NV_ERR_NOT_SUPPORTED);
        // Another process is not held to the first one's budget.
        assert_eq!(g.check(proc(2), &p, 536, t), Ok(()));
        // A token back after 1 / PROC_RATE s.
        let later = t + Duration::from_secs_f64(1.1 / PROC_RATE);
        assert_eq!(g.check(proc(1), &p, 536, later), Ok(()));
        assert_eq!(g.check(proc(1), &p, 536, later), Err(Refused::ProcRate));
        assert!(g.refused >= 2);
    }

    #[test]
    fn processes_that_spend_their_rate_cannot_take_a_quiet_ones_calls() {
        let t0 = Instant::now();
        let mut g = ChannelGate::new(t0);
        let p = params(0);
        // Five processes, each asking far past its rate and so spending all
        // of it: together more than the VM's, which is down to its reserve
        // and stays there.
        let step = Duration::from_micros(400);
        let mut t = t0;
        let mut hogs = 0;
        for i in 0..25_000u32 {
            if g.check(proc(100 + i % 5), &p, 536, t).is_ok() {
                hogs += 1;
            }
            t += step;
        }
        // Ten seconds: the VM's burst and rate, less what it kept back.
        let secs = 25_000.0 * 400e-6;
        assert!(hogs as f64 <= VM_BURST + VM_RATE * secs, "{hogs}");
        assert!(g.vm.tokens < VM_RESERVE + 5.0, "{}", g.vm.tokens);
        // The quiet process makes its few calls, every one of them served:
        // RESERVE_FLOOR of them, and not one more from the reserve.
        for k in 0..RESERVE_FLOOR as usize {
            assert_eq!(g.check(proc(1), &p, 536, t), Ok(()), "call {k}");
        }
        assert_eq!(g.check(proc(1), &p, 536, t), Err(Refused::VmRate));
    }

    #[test]
    fn the_vm_is_bounded_whatever_its_processes() {
        let t = Instant::now();
        let mut g = ChannelGate::new(t);
        let p = params(0);
        let mut ok = 0;
        // A fresh process a call, all at once: the VM's burst and no more.
        for i in 0..10_000u32 {
            if g.check(proc(i), &p, 536, t).is_ok() {
                ok += 1;
            }
        }
        assert!(ok as f64 <= VM_BURST, "{ok} calls at once");
        assert!(ok as f64 >= VM_BURST - VM_RESERVE, "{ok} calls at once");
        assert_eq!(g.tracked(), PROC_CAP);
        // A guest that does not say which process calls: the VM's budget.
        let mut g = ChannelGate::new(t);
        let mut ok = 0;
        for _ in 0..1000 {
            if g.check(Owner::Unknown, &p, 536, t).is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, VM_BURST as usize);
        assert_eq!(g.check(Owner::Unknown, &p, 536, t), Err(Refused::VmRate));
    }

    #[test]
    fn full_buckets_are_forgotten() {
        let t = Instant::now();
        let mut g = ChannelGate::new(t);
        let p = params(0);
        for i in 0..(PRUNE_AT as u32) {
            assert_eq!(
                g.check(proc(i), &p, 536, t + Duration::from_millis(10 * i as u64)),
                Ok(())
            );
        }
        // Long after: every bucket has refilled, and the next call prunes.
        let later = t + Duration::from_secs(60);
        assert_eq!(g.check(proc(9999), &p, 536, later), Ok(()));
        assert_eq!(g.tracked(), 1);
    }
}
