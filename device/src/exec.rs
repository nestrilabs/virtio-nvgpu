//! Per-file serial executors for host calls that may wait.
//!
//! The queue thread serves every guest request in ring order, so anything it
//! blocks on, everything behind it waits for: RM traffic from unrelated guest
//! processes, CUDA, other files' flips. And host display calls do block. A
//! blocking atomic commit in nvidia-drm holds the modeset locks and waits up to
//! three seconds, twice (nvidia-drm-modeset.c:765-778, 896-930); GETCONNECTOR
//! takes `mode_config.mutex` behind it (drm_connector.c:3373); NVKMS commands
//! queue on `nvkms_lock` behind a SET_MODE. None of those waits can be
//! interrupted, so they cannot be cancelled either -- they can only be moved
//! off the queue thread.
//!
//! So every IOCTL2 on a card, lease or modeset file, and every schema entry
//! marked EXECUTOR, runs here. Each host file gets a FIFO of its own and runs
//! at most one job at a time, which is the ordering a native process calling
//! from one thread would see; different files run in parallel on a pool of at
//! most [`MAX_THREADS`] threads. Jobs carry whatever they need (duplicated
//! descriptors, the ring epoch they were taken under), so nothing here knows
//! about guests, rings or the backend.
//!
//! There is no cancellation: host display waits are bounded, and the guest
//! waits for the answer with a timeout of its own. A session reset takes back
//! the jobs that have not started ([`ExecPool::cancel_pending`]) and runs each
//! one's cancel path, which closes what it holds and answers its chain if the
//! ring is still the one it came from; jobs already running finish and find
//! their results discarded by the epoch or session check at completion.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

/// Most executor threads. One per busy host file is the ideal; a guest with
/// more files busy at once than this shares threads between them.
pub const MAX_THREADS: usize = 16;

/// A unit of work. `run(false)` executes it; `run(true)` is the cancel path,
/// taken instead when a session reset withdraws a job that never started.
pub type Job = Box<dyn FnOnce(bool) + Send>;

#[derive(Default)]
struct State {
    /// Waiting jobs, per host file.
    queues: HashMap<u64, VecDeque<Job>>,
    /// Files with waiting jobs and none running, in the order they became so.
    ready: VecDeque<u64>,
    /// Files with a job on a thread right now.
    running: HashSet<u64>,
    threads: usize,
    idle: usize,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    max_threads: usize,
}

/// A pool of serial executors, keyed by host file.
#[derive(Clone)]
pub struct ExecPool {
    shared: Arc<Shared>,
}

impl Default for ExecPool {
    fn default() -> Self {
        Self::new(MAX_THREADS)
    }
}

impl ExecPool {
    pub fn new(max_threads: usize) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
                max_threads: max_threads.max(1),
            }),
        }
    }

    /// Queue `job` behind every earlier job for the same `file`.
    pub fn submit(&self, file: u64, job: Job) {
        let mut st = self.shared.state.lock().unwrap();
        let q = st.queues.entry(file).or_default();
        q.push_back(job);
        let first = q.len() == 1;
        if first && !st.running.contains(&file) {
            st.ready.push_back(file);
        }
        // Spawn when the files waiting for a thread outnumber the workers that
        // can still take one. `idle` counts a worker from the moment it waits
        // until it has the lock back, so a worker already woken by an earlier
        // submit still counts as idle here; it will take one file and then run
        // that file's job to the end. Testing `idle == 0` instead let a second
        // submit in that window spawn nothing and lose its notify, and the
        // second file then waited behind the first file's job -- a three
        // second commit, say -- with threads to spare (S-27). Comparing counts
        // over-spawns at worst, when a busy worker would have come back for
        // the file soon anyway; `max_threads` still bounds it.
        if st.ready.len() > st.idle && st.threads < self.shared.max_threads {
            st.threads += 1;
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("nvgpu-exec".into())
                .spawn(move || worker(shared));
            if let Err(e) = spawned {
                st.threads -= 1;
                log::error!("executor thread would not start: {e}");
            }
        }
        drop(st);
        self.shared.wake.notify_one();
    }

    /// Take back every job that has not started, and run its cancel path.
    ///
    /// Called with no backend lock held: cancel paths complete their chains,
    /// which takes the ring lock.
    pub fn cancel_pending(&self) {
        let jobs: Vec<Job> = {
            let mut st = self.shared.state.lock().unwrap();
            st.ready.clear();
            st.queues.drain().flat_map(|(_, q)| q).collect()
        };
        if !jobs.is_empty() {
            log::info!(
                "executors: {} queued job(s) withdrawn by a session reset",
                jobs.len()
            );
        }
        for job in jobs {
            job(true);
        }
    }

    /// Jobs waiting (not running), for tests and logging.
    pub fn pending(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap()
            .queues
            .values()
            .map(VecDeque::len)
            .sum()
    }
}

fn worker(shared: Arc<Shared>) {
    let mut st = shared.state.lock().unwrap();
    loop {
        let Some(file) = st.ready.pop_front() else {
            st.idle += 1;
            st = shared.wake.wait(st).unwrap();
            st.idle -= 1;
            continue;
        };
        let Some(job) = st.queues.get_mut(&file).and_then(VecDeque::pop_front) else {
            continue;
        };
        st.running.insert(file);
        drop(st);

        job(false);

        st = shared.state.lock().unwrap();
        st.running.remove(&file);
        match st.queues.get(&file) {
            Some(q) if !q.is_empty() => st.ready.push_back(file),
            _ => {
                st.queues.remove(&file);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::time::Duration;

    #[test]
    fn jobs_on_one_file_run_in_order_and_never_overlap() {
        let pool = ExecPool::new(4);
        let log = Arc::new(Mutex::new(Vec::new()));
        let busy = Arc::new(Mutex::new(false));
        let (tx, rx) = channel();
        for i in 0..20 {
            let (log, busy, tx) = (log.clone(), busy.clone(), tx.clone());
            pool.submit(
                7,
                Box::new(move |_| {
                    assert!(
                        !std::mem::replace(&mut *busy.lock().unwrap(), true),
                        "overlap"
                    );
                    std::thread::sleep(Duration::from_micros(200));
                    log.lock().unwrap().push(i);
                    *busy.lock().unwrap() = false;
                    tx.send(()).unwrap();
                }),
            );
        }
        for _ in 0..20 {
            rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(*log.lock().unwrap(), (0..20).collect::<Vec<_>>());
    }

    /// The point of the pool: one file stuck in a three-second commit does
    /// not hold up another file's work.
    #[test]
    fn a_blocked_file_does_not_hold_up_another() {
        let pool = ExecPool::new(4);
        let (gate_tx, gate_rx) = channel::<()>();
        let (done_tx, done_rx) = channel();
        pool.submit(1, Box::new(move |_| gate_rx.recv().unwrap()));
        pool.submit(2, Box::new(move |_| done_tx.send(2).unwrap()));
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)), Ok(2));
        gate_tx.send(()).unwrap();
    }

    /// A worker woken for one file still counts as idle until it has the
    /// lock back; a second file submitted in that window must get a thread
    /// of its own rather than wait behind the first file's job.
    #[test]
    fn a_second_file_submitted_while_the_idle_worker_wakes_is_not_left_behind() {
        for _ in 0..100 {
            let pool = ExecPool::new(4);
            let (warm_tx, warm_rx) = channel();
            pool.submit(99, Box::new(move |_| warm_tx.send(()).unwrap()));
            warm_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Let the one worker go back to waiting: exactly one idle.
            while pool.shared.state.lock().unwrap().idle != 1 {
                std::thread::yield_now();
            }
            let (gate_tx, gate_rx) = channel::<()>();
            let (done_tx, done_rx) = channel();
            pool.submit(1, Box::new(move |_| gate_rx.recv().unwrap()));
            pool.submit(2, Box::new(move |_| done_tx.send(2).unwrap()));
            assert_eq!(
                done_rx.recv_timeout(Duration::from_secs(5)),
                Ok(2),
                "file 2 waited behind file 1's blocked job"
            );
            gate_tx.send(()).unwrap();
        }
    }

    #[test]
    fn a_session_reset_cancels_what_has_not_started() {
        let pool = ExecPool::new(1);
        let (gate_tx, gate_rx) = channel::<()>();
        let (started_tx, started_rx) = channel();
        let (out_tx, out_rx) = channel();
        pool.submit(
            1,
            Box::new(move |_| {
                started_tx.send(()).unwrap();
                gate_rx.recv().unwrap();
            }),
        );
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        for i in 0..3 {
            let out = out_tx.clone();
            pool.submit(
                1,
                Box::new(move |cancelled| out.send((i, cancelled)).unwrap()),
            );
        }
        assert_eq!(pool.pending(), 3);
        pool.cancel_pending();
        let got: Vec<_> = out_rx.try_iter().collect();
        assert_eq!(got, vec![(0, true), (1, true), (2, true)]);
        gate_tx.send(()).unwrap();
        assert_eq!(pool.pending(), 0);
    }
}
