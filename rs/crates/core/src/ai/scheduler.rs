//! Port of `src/main/ai/scheduler.ts` — the queue/timing state machine deciding WHEN to
//! summarize a pane (FIFO + coalesce + backoff + staleness). Mirror `scheduler.test.ts`.
//!
//! The TS source is `setTimeout`/`setInterval`-driven and its tests use fake timers
//! (`advanceTimersByTimeAsync`). This port models the same machine over a **virtual
//! clock**: time only moves when [`SummaryScheduler::advance`] is called, which fires
//! every timer due in that window in chronological order — the deterministic, no-real-
//! threads equivalent of fake timers, with the clock fully injected for tests.
//!
//! Model (from the plan):
//!   - activity-driven: each `note_output` (re)arms a per-uid settle timer; when a
//!     pane goes quiet for `settle_ms`, it's enqueued once.
//!   - safety tick: a slow interval re-enqueues panes not summarized within
//!     `max_staleness_sec` (covers panes that keep dribbling output).
//!   - single global FIFO with per-pane coalescing: a uid already queued or
//!     in-flight is never double-queued; output during a job sets a rerun flag.
//!   - concurrency cap: at most `concurrency` jobs run at once.
//!   - backoff: a failed job pauses the queue with exponential backoff (the head
//!     is retried) instead of hammering an unreachable server.
//!   - status: online/offline transitions are reported once (not per job).
//!
//! A job is dispatched by invoking the injected `run_job`. Because real jobs are
//! async, `run_job` returns a [`JobStart`]: either [`JobStart::Done`] (the job
//! finished synchronously — used by most tests) or [`JobStart::InFlight`] (the job
//! is still running; the caller later resolves it via [`SummaryScheduler::complete`]
//! — the analogue of awaiting the returned promise).

use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobResult {
    Ok,
    Fail,
    Skip,
}

/// What `run_job` reports when a uid is dispatched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStart {
    /// Completed synchronously with this result.
    Done(JobResult),
    /// Still running; resolve later with [`SummaryScheduler::complete`].
    InFlight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedulerConfig {
    pub settle_ms: i64,
    pub max_staleness_sec: i64,
    pub concurrency: usize,
}

const BACKOFF_MIN_MS: i64 = 2000;
const BACKOFF_MAX_MS: i64 = 300_000;
const STALENESS_TICK_MS: i64 = 30_000;

type StatusCb = Box<dyn FnMut(bool, Option<&str>)>;

pub struct SummaryScheduler {
    cfg: SchedulerConfig,
    run_job: Box<dyn FnMut(&str) -> JobStart>,
    on_status: Option<StatusCb>,
    now: i64,

    settle_timers: HashMap<String, i64>, // uid -> fire time
    queue: VecDeque<String>,             // FIFO of uids waiting to run
    queued: HashSet<String>,             // membership mirror of `queue`
    in_flight: HashSet<String>,          // uids currently running
    rerun: HashSet<String>,              // output arrived while in-flight
    known: HashSet<String>,              // uids we've seen output from
    last_summary_at: HashMap<String, i64>,

    backoff_ms: i64,
    backoff_timer: Option<i64>,   // fire time
    staleness_timer: Option<i64>, // next tick fire time
    last_online: Option<bool>,
    running: bool,

    // synchronous (`Done`) results awaiting their post-completion processing,
    // mirroring the microtask boundary of the TS async `execute`.
    pending_results: VecDeque<(String, JobResult)>,
}

impl SummaryScheduler {
    pub fn new(
        cfg: SchedulerConfig,
        run_job: impl FnMut(&str) -> JobStart + 'static,
        on_status: Option<StatusCb>,
    ) -> Self {
        Self {
            cfg,
            run_job: Box::new(run_job),
            on_status,
            now: 0,
            settle_timers: HashMap::new(),
            queue: VecDeque::new(),
            queued: HashSet::new(),
            in_flight: HashSet::new(),
            rerun: HashSet::new(),
            known: HashSet::new(),
            last_summary_at: HashMap::new(),
            backoff_ms: 0,
            backoff_timer: None,
            staleness_timer: None,
            last_online: None,
            running: false,
            pending_results: VecDeque::new(),
        }
    }

    pub fn start(&mut self) {
        if self.running {
            return;
        }
        self.running = true;
        self.staleness_timer = Some(self.now + STALENESS_TICK_MS);
    }

    /// Stop scheduling and drop all pending timers/queue. In-flight jobs are left
    /// to settle on their own (their results are ignored once stopped).
    pub fn stop(&mut self) {
        self.running = false;
        self.settle_timers.clear();
        self.backoff_timer = None;
        self.staleness_timer = None;
        self.queue.clear();
        self.queued.clear();
        self.rerun.clear();
        self.backoff_ms = 0;
    }

    pub fn set_config(&mut self, cfg: SchedulerConfig) {
        self.cfg = cfg;
    }

    /// A pane produced output: remember it and (re)arm its settle timer.
    pub fn note_output(&mut self, uid: &str) {
        if !self.running {
            return;
        }
        self.known.insert(uid.to_string());
        if self.in_flight.contains(uid) {
            self.rerun.insert(uid.to_string()); // re-run after the current job finishes
            return;
        }
        self.settle_timers
            .insert(uid.to_string(), self.now + self.cfg.settle_ms);
    }

    /// A pane is gone (session exit / no longer published): forget all state for it.
    pub fn forget(&mut self, uid: &str) {
        self.settle_timers.remove(uid);
        self.known.remove(uid);
        self.rerun.remove(uid);
        self.last_summary_at.remove(uid);
        if self.queued.remove(uid) {
            self.queue.retain(|q| q != uid);
        }
    }

    /// Resolve a job previously reported [`JobStart::InFlight`].
    pub fn complete(&mut self, uid: &str, result: JobResult) {
        self.finish(uid.to_string(), result);
        self.pump();
    }

    /// Number of jobs currently running (for tests / introspection).
    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Advance the virtual clock by `ms`, firing every timer that comes due, in
    /// chronological order (the deterministic equivalent of fake timers).
    pub fn advance(&mut self, ms: i64) {
        let target = self.now + ms;
        loop {
            let mut earliest: Option<i64> = None;
            for &t in self.settle_timers.values() {
                earliest = Some(earliest.map_or(t, |e: i64| e.min(t)));
            }
            if let Some(t) = self.backoff_timer {
                earliest = Some(earliest.map_or(t, |e| e.min(t)));
            }
            if let Some(t) = self.staleness_timer {
                earliest = Some(earliest.map_or(t, |e| e.min(t)));
            }
            match earliest {
                Some(t) if t <= target => {
                    self.now = t;
                    self.fire_at(t);
                }
                _ => break,
            }
        }
        self.now = target;
    }

    fn fire_at(&mut self, t: i64) {
        // Settle timers due now (sorted for deterministic enqueue order).
        let mut due: Vec<String> = self
            .settle_timers
            .iter()
            .filter(|(_, &ft)| ft == t)
            .map(|(u, _)| u.clone())
            .collect();
        due.sort();
        for uid in due {
            self.settle_timers.remove(&uid);
            self.enqueue(uid);
        }
        // Backoff timer due now: clear it so the queue can resume.
        if self.backoff_timer == Some(t) {
            self.backoff_timer = None;
        }
        // Staleness tick: re-enqueue stale panes and reschedule the interval.
        if self.staleness_timer == Some(t) {
            self.staleness_timer = None;
            self.stale_tick();
            if self.running {
                self.staleness_timer = Some(t + STALENESS_TICK_MS);
            }
        }
        self.pump();
    }

    fn enqueue(&mut self, uid: String) {
        if !self.running {
            return;
        }
        if self.queued.contains(&uid) || self.in_flight.contains(&uid) {
            if self.in_flight.contains(&uid) {
                self.rerun.insert(uid);
            }
            return;
        }
        self.queue.push_back(uid.clone());
        self.queued.insert(uid);
        // Caller pumps (fire_at / complete do so after dispatch).
    }

    fn pump(&mut self) {
        loop {
            if self.running && self.backoff_timer.is_none() {
                while self.in_flight.len() < self.cfg.concurrency && !self.queue.is_empty() {
                    let uid = self.queue.pop_front().unwrap();
                    self.queued.remove(&uid);
                    self.in_flight.insert(uid.clone());
                    match (self.run_job)(&uid) {
                        JobStart::Done(r) => self.pending_results.push_back((uid, r)),
                        JobStart::InFlight => {}
                    }
                }
            }
            match self.pending_results.pop_front() {
                Some((uid, r)) => self.finish(uid, r),
                None => break,
            }
        }
    }

    // The tail of the TS async `execute`, after the job result is known. Does NOT
    // pump — the pump loop (or `complete`) drives the next round.
    fn finish(&mut self, uid: String, result: JobResult) {
        self.in_flight.remove(&uid);
        if !self.running {
            return;
        }
        match result {
            JobResult::Ok => {
                self.last_summary_at.insert(uid.clone(), self.now);
                self.reset_backoff();
                self.report(true, None);
            }
            JobResult::Fail => {
                self.report(false, None);
                self.schedule_backoff(uid); // retry the head after a growing delay
                return; // pump resumes when the backoff timer fires
            }
            JobResult::Skip => {}
        }
        // 'ok' or 'skip': if more output arrived mid-job, re-queue this pane.
        if self.rerun.remove(&uid) {
            self.enqueue(uid);
        }
    }

    // Re-enqueue panes that haven't summarized within the staleness window. The
    // real runJob skips those with nothing new, so this is a cheap backstop.
    fn stale_tick(&mut self) {
        let cutoff = self.now - self.cfg.max_staleness_sec * 1000;
        let known: Vec<String> = self.known.iter().cloned().collect();
        for uid in known {
            if *self.last_summary_at.get(&uid).unwrap_or(&0) <= cutoff {
                self.enqueue(uid);
            }
        }
    }

    fn schedule_backoff(&mut self, uid: String) {
        if !self.queued.contains(&uid) {
            self.queue.push_front(uid.clone()); // retry this one first
            self.queued.insert(uid);
        }
        self.backoff_ms = (self.backoff_ms * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
        self.backoff_timer = Some(self.now + self.backoff_ms);
    }

    fn reset_backoff(&mut self) {
        self.backoff_ms = 0;
        self.backoff_timer = None;
    }

    fn report(&mut self, online: bool, last_error: Option<&str>) {
        if self.last_online == Some(online) {
            return;
        }
        self.last_online = Some(online);
        if let Some(cb) = self.on_status.as_mut() {
            cb(online, last_error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn no_status() -> Option<StatusCb> {
        None
    }

    #[test]
    fn summarizes_a_pane_once_after_it_goes_quiet_resetting_on_new_output() {
        let runs = Rc::new(RefCell::new(Vec::<String>::new()));
        let r2 = runs.clone();
        let mut s = SummaryScheduler::new(
            SchedulerConfig {
                settle_ms: 100,
                max_staleness_sec: 9999,
                concurrency: 1,
            },
            move |uid| {
                r2.borrow_mut().push(uid.to_string());
                JobStart::Done(JobResult::Ok)
            },
            no_status(),
        );
        s.start();
        s.note_output("a");
        s.advance(60);
        s.note_output("a"); // more output -> settle timer restarts
        s.advance(60);
        assert!(runs.borrow().is_empty()); // not quiet for a full 100ms since last output
        s.advance(50);
        assert_eq!(*runs.borrow(), vec!["a".to_string()]); // fired once after settling
        s.stop();
    }

    #[test]
    fn caps_concurrency_across_panes() {
        let mut s = SummaryScheduler::new(
            SchedulerConfig {
                settle_ms: 10,
                max_staleness_sec: 9999,
                concurrency: 2,
            },
            // Jobs never finish on their own; the test resolves them via `complete`.
            |_uid| JobStart::InFlight,
            no_status(),
        );
        s.start();
        for uid in ["a", "b", "c", "d"] {
            s.note_output(uid);
        }
        s.advance(10); // all four settle, but only 2 may run at once
        assert_eq!(s.in_flight_count(), 2);
        // Free the two running slots; the other two start, still capped at 2.
        s.complete("a", JobResult::Ok);
        assert_eq!(s.in_flight_count(), 2);
        s.complete("b", JobResult::Ok);
        assert_eq!(s.in_flight_count(), 2);
        s.stop();
    }

    #[test]
    fn goes_offline_on_failure_then_back_online_on_recovery_after_backoff() {
        let statuses = Rc::new(RefCell::new(Vec::<bool>::new()));
        let st = statuses.clone();
        let result = Rc::new(RefCell::new(JobResult::Fail));
        let res = result.clone();
        let mut s = SummaryScheduler::new(
            SchedulerConfig {
                settle_ms: 10,
                max_staleness_sec: 9999,
                concurrency: 1,
            },
            move |_uid| JobStart::Done(*res.borrow()),
            Some(Box::new(move |online, _err| st.borrow_mut().push(online))),
        );
        s.start();
        s.note_output("a");
        s.advance(10); // runs -> fail -> offline + backoff armed
        assert_eq!(*statuses.borrow(), vec![false]);
        *result.borrow_mut() = JobResult::Ok;
        s.advance(2000); // backoff (min 2s) elapses -> retry head -> ok
        assert_eq!(*statuses.borrow(), vec![false, true]);
        s.stop();
    }

    #[test]
    fn skips_work_cleanly_without_flipping_status() {
        let statuses = Rc::new(RefCell::new(Vec::<bool>::new()));
        let st = statuses.clone();
        let mut s = SummaryScheduler::new(
            SchedulerConfig {
                settle_ms: 10,
                max_staleness_sec: 9999,
                concurrency: 1,
            },
            |_uid| JobStart::Done(JobResult::Skip),
            Some(Box::new(move |online, _err| st.borrow_mut().push(online))),
        );
        s.start();
        s.note_output("a");
        s.advance(10);
        assert!(statuses.borrow().is_empty()); // a skip is neither online nor offline
        s.stop();
    }
}
