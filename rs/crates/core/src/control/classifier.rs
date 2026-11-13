//! The always-on screen classifier. It renders a pane's screen once its genuine output settles and
//! caches the verdict (awaiting / asking / question, see `session::activity::ScreenVerdict`) that
//! `activity_for`, the tab's ASK state and the "needs input" alerts read, and it steps every
//! pane's ask episode on the clock. It runs whether or not the control server is enabled: the app
//! spawns [`run_screen_classifier`] at startup.
//!
//! Bounded, so idle panes cost nothing:
//! - a pane is rendered only when its content cursor moved since its last verdict: an unchanged
//!   screen is never re-rendered (cosmetic redraws such as a blink do not move the cursor);
//! - once its genuine output has been quiet for [`CLASSIFY_SETTLE_MS`], or, while it never
//!   goes quiet (a dialog with a ticking clock or spinner on screen), every
//!   [`CLASSIFY_MAX_WAIT_MS`];
//! - at most [`MAX_RENDERS_PER_TICK`] renders per [`CLASSIFIER_TICK_MS`] tick, served round-robin
//!   by uid so a crowd of changed panes cannot starve any of them;
//! - a render that fails (a gone pane, a daemon timeout) is retried after [`RETRY_AFTER_MS`],
//!   not every tick.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::control::server::{classify_screen, now_ms};
use crate::session::activity::ActivitySignal;
use crate::session_manager::SessionManager;

/// How long a pane's genuine output must have been quiet before its screen is classified.
pub const CLASSIFY_SETTLE_MS: u64 = 750;
/// The classifier's tick.
pub const CLASSIFIER_TICK_MS: u64 = 500;
/// The most screens rendered per tick.
pub const MAX_RENDERS_PER_TICK: usize = 4;
/// How long a pane whose render failed waits before it is tried again (same content).
pub const RETRY_AFTER_MS: u64 = 10_000;
/// How often a pane whose output never goes quiet for [`CLASSIFY_SETTLE_MS`] is rendered
/// anyway, so a dialog with a ticking element on screen still reads ASK.
pub const CLASSIFY_MAX_WAIT_MS: u64 = 1_500;

/// Whether this pane's screen should be classified at `now`: its content changed since its last
/// verdict (or it never had one) and its genuine output has been quiet for
/// [`CLASSIFY_SETTLE_MS`]. A pane that never produced output has nothing to classify.
pub fn needs_classification(sig: &ActivitySignal, now: u64) -> bool {
    let Some(t) = sig.content_at else {
        return false;
    };
    now.saturating_sub(t) >= CLASSIFY_SETTLE_MS && sig.classification_stale()
}

/// The classifier's memory between ticks: where the round-robin left off, recent failures, and
/// when each changing pane was last rendered.
#[derive(Debug, Default)]
pub struct ClassifierPlan {
    /// The last uid served, so the next tick starts after it.
    last_served: Option<String>,
    /// uid → (content cursor whose render failed, when to try that content again).
    failed: HashMap<String, (u64, u64)>,
    /// uid → when it was last picked, or first seen changing without settling: the clock of
    /// [`CLASSIFY_MAX_WAIT_MS`].
    picked_at: HashMap<String, u64>,
}

impl ClassifierPlan {
    /// Choose this tick's renders among `panes` (uid, signal), skipping `inflight` ones: those
    /// that [`needs_classification`], or that changed and have waited [`CLASSIFY_MAX_WAIT_MS`]
    /// without settling, minus recent failures at the same content, in uid order starting after
    /// the last uid served, at most `cap`.
    pub fn pick(
        &mut self,
        panes: &[(String, ActivitySignal)],
        inflight: &HashSet<String>,
        now: u64,
        cap: usize,
    ) -> Vec<String> {
        let live: HashSet<&str> = panes.iter().map(|(u, _)| u.as_str()).collect();
        self.failed.retain(|u, _| live.contains(u.as_str()));
        self.picked_at.retain(|u, _| live.contains(u.as_str()));
        let mut due: Vec<&str> = Vec::new();
        for (uid, sig) in panes {
            let changed = sig.content_at.is_some() && sig.classification_stale();
            if !changed {
                // Classified at its current content: a later change starts a fresh clock.
                self.picked_at.remove(uid);
                continue;
            }
            let waiting_since = *self.picked_at.entry(uid.clone()).or_insert(now);
            let overdue = now.saturating_sub(waiting_since) >= CLASSIFY_MAX_WAIT_MS;
            if !inflight.contains(uid)
                && (needs_classification(sig, now) || overdue)
                && !matches!(self.failed.get(uid), Some(&(cursor, retry_at))
                    if cursor == sig.content_cursor && now < retry_at)
            {
                due.push(uid.as_str());
            }
        }
        due.sort_unstable();
        // Rotate so the first uid after the last one served comes first.
        let start = match &self.last_served {
            Some(last) => due.partition_point(|u| *u <= last.as_str()),
            None => 0,
        };
        due.rotate_left(start);
        due.truncate(cap);
        if let Some(last) = due.last() {
            self.last_served = Some(last.to_string());
        }
        for uid in &due {
            self.picked_at.insert(uid.to_string(), now);
        }
        due.into_iter().map(str::to_string).collect()
    }

    /// Record that rendering `uid` at content `cursor` failed at `now`.
    pub fn note_failed(&mut self, uid: &str, cursor: u64, now: u64) {
        self.failed
            .insert(uid.to_string(), (cursor, now + RETRY_AFTER_MS));
    }
}

/// One tick's bookkeeping over every live session: step each pane's ask episode at `now`, then
/// choose the renders ([`ClassifierPlan::pick`]). Cheap: one signal snapshot per pane, no I/O.
pub fn plan_tick(
    sessions: &SessionManager,
    plan: &mut ClassifierPlan,
    inflight: &HashSet<String>,
    now: u64,
) -> Vec<String> {
    let mut panes = Vec::new();
    for uid in sessions.uids() {
        sessions.step_ask(&uid, now);
        if let Some(sig) = sessions.activity_signal(&uid) {
            panes.push((uid, sig));
        }
    }
    plan.pick(&panes, inflight, now, MAX_RENDERS_PER_TICK)
}

/// Run the classifier forever on the current Tokio runtime. Renders happen in `spawn_blocking`
/// (the daemon backend round-trips), at most one in flight per pane.
pub async fn run_screen_classifier(sessions: Arc<SessionManager>) {
    let mut interval = tokio::time::interval(Duration::from_millis(CLASSIFIER_TICK_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let plan: Arc<Mutex<ClassifierPlan>> = Arc::default();
    let inflight: Arc<Mutex<HashSet<String>>> = Arc::default();
    loop {
        interval.tick().await;
        let now = now_ms() as u64;
        let picked = {
            let busy = lock(&inflight).clone();
            plan_tick(&sessions, &mut lock(&plan), &busy, now)
        };
        if picked.is_empty() {
            continue;
        }
        lock(&inflight).extend(picked.iter().cloned());
        let sessions = Arc::clone(&sessions);
        let plan = Arc::clone(&plan);
        let inflight = Arc::clone(&inflight);
        let release = Release {
            inflight: Arc::clone(&inflight),
            uids: picked.clone(),
        };
        tokio::task::spawn_blocking(move || {
            let _release = release;
            for uid in &picked {
                let cursor = sessions.activity_signal(uid).map(|s| s.content_cursor);
                if classify_screen(&sessions, uid).is_none() {
                    if let Some(c) = cursor {
                        lock(&plan).note_failed(uid, c, now_ms() as u64);
                    }
                }
                lock(&inflight).remove(uid);
            }
        });
    }
}

/// Takes `uids` out of the in-flight set when dropped, so a render that panics never leaves its
/// pane (or the rest of its batch) in flight, and so never classified again.
struct Release {
    inflight: Arc<Mutex<HashSet<String>>>,
    uids: Vec<String>,
}

impl Drop for Release {
    fn drop(&mut self) {
        let mut set = lock(&self.inflight);
        for uid in &self.uids {
            set.remove(uid);
        }
    }
}

/// Lock, ignoring poison: the classifier's sets stay usable after a panicking render.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::activity::ScreenVerdict;

    fn sig(content_at: Option<u64>, cursor: u64, verdict_at: Option<u64>) -> ActivitySignal {
        ActivitySignal {
            content_at,
            content_cursor: cursor,
            verdict: verdict_at.map(|c| ScreenVerdict {
                cursor: c,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn panes(n: usize, f: impl Fn(usize) -> ActivitySignal) -> Vec<(String, ActivitySignal)> {
        (0..n).map(|i| (format!("p{i:02}"), f(i))).collect()
    }

    #[test]
    fn settled_changed_content_needs_a_verdict() {
        let now = 100_000;
        assert!(!needs_classification(&sig(None, 0, None), now));
        assert!(!needs_classification(&sig(Some(now - 100), 5, None), now));
        assert!(needs_classification(&sig(Some(now - 1_000), 5, None), now));
        assert!(needs_classification(
            &sig(Some(now - 1_000), 5, Some(4)),
            now
        ));
        assert!(!needs_classification(
            &sig(Some(now - 1_000), 5, Some(5)),
            now
        ));
        // Long idle (e.g. after a GUI restart) but never classified: classify once.
        assert!(needs_classification(&sig(Some(1), 5, None), now));
    }

    #[test]
    fn forty_idle_unchanged_panes_cost_zero_renders() {
        let now = 1_000_000;
        // Every pane went quiet long ago and its verdict matches its current content.
        let all = panes(40, |i| sig(Some(now - 60_000), i as u64, Some(i as u64)));
        let mut plan = ClassifierPlan::default();
        for tick in 0..20 {
            let picked = plan.pick(
                &all,
                &HashSet::new(),
                now + tick * 500,
                MAX_RENDERS_PER_TICK,
            );
            assert!(picked.is_empty(), "tick {tick} rendered {picked:?}");
        }
    }

    #[test]
    fn renders_are_capped_per_tick_and_served_round_robin() {
        let now = 1_000_000;
        // 10 of 40 panes changed and settled; the rest are unchanged.
        let all = panes(40, |i| {
            if i % 4 == 0 {
                sig(Some(now - 2_000), 7, Some(6))
            } else {
                sig(Some(now - 60_000), 7, Some(7))
            }
        });
        let mut plan = ClassifierPlan::default();
        let t1 = plan.pick(&all, &HashSet::new(), now, MAX_RENDERS_PER_TICK);
        let t2 = plan.pick(&all, &HashSet::new(), now + 500, MAX_RENDERS_PER_TICK);
        let t3 = plan.pick(&all, &HashSet::new(), now + 1_000, MAX_RENDERS_PER_TICK);
        assert_eq!(t1, ["p00", "p04", "p08", "p12"]);
        assert_eq!(t2, ["p16", "p20", "p24", "p28"]);
        // Wraps around: the last two, then back to the start.
        assert_eq!(t3, ["p32", "p36", "p00", "p04"]);
    }

    #[test]
    fn inflight_and_unsettled_panes_wait() {
        let now = 1_000_000;
        let all = vec![
            ("a".to_string(), sig(Some(now - 2_000), 1, None)),
            ("b".to_string(), sig(Some(now - 100), 1, None)),
        ];
        let inflight: HashSet<String> = ["a".to_string()].into();
        assert!(ClassifierPlan::default()
            .pick(&all, &inflight, now, MAX_RENDERS_PER_TICK)
            .is_empty());
    }

    #[test]
    fn a_failed_render_is_retried_later_or_on_new_content() {
        let now = 1_000_000;
        let mut plan = ClassifierPlan::default();
        let one = vec![("a".to_string(), sig(Some(now - 2_000), 3, None))];
        assert_eq!(plan.pick(&one, &HashSet::new(), now, 4), ["a"]);
        plan.note_failed("a", 3, now);
        assert!(plan.pick(&one, &HashSet::new(), now + 500, 4).is_empty());
        assert_eq!(
            plan.pick(&one, &HashSet::new(), now + RETRY_AFTER_MS, 4),
            ["a"]
        );
        plan.note_failed("a", 3, now);
        let changed = vec![("a".to_string(), sig(Some(now - 1_000), 4, None))];
        assert_eq!(plan.pick(&changed, &HashSet::new(), now + 500, 4), ["a"]);
    }

    #[test]
    fn a_pane_that_never_settles_is_rendered_every_max_wait() {
        let mut plan = ClassifierPlan::default();
        let start = 1_000_000;
        let mut renders = Vec::new();
        for tick in 0..20u64 {
            let now = start + tick * CLASSIFIER_TICK_MS;
            // Output every 200 ms (a ticking clock under a dialog): never quiet for the settle
            // window, and the last verdict is always behind the content.
            let one = vec![("a".to_string(), sig(Some(now - 100), tick + 10, Some(tick)))];
            if !plan.pick(&one, &HashSet::new(), now, 4).is_empty() {
                renders.push(now - start);
            }
        }
        assert_eq!(renders, [1_500, 3_000, 4_500, 6_000, 7_500, 9_000]);
        // Once classified at its current content it costs nothing, and a later change waits a
        // full CLASSIFY_MAX_WAIT_MS again (or settles first).
        let now = start + 20 * CLASSIFIER_TICK_MS;
        let current = vec![("a".to_string(), sig(Some(now - 100), 50, Some(50)))];
        assert!(plan.pick(&current, &HashSet::new(), now, 4).is_empty());
        let changed = vec![("a".to_string(), sig(Some(now + 400), 51, Some(50)))];
        assert!(plan
            .pick(&changed, &HashSet::new(), now + 500, 4)
            .is_empty());
        assert_eq!(
            plan.pick(
                &changed,
                &HashSet::new(),
                now + 500 + CLASSIFY_MAX_WAIT_MS,
                4
            ),
            ["a"]
        );
    }

    #[test]
    fn a_panicking_render_leaves_nothing_in_flight() {
        let batch = vec!["a".to_string(), "b".to_string()];
        let inflight: Arc<Mutex<HashSet<String>>> =
            Arc::new(Mutex::new(batch.iter().cloned().collect()));
        let release = Release {
            inflight: Arc::clone(&inflight),
            uids: batch,
        };
        let set = Arc::clone(&inflight);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _release = release;
            // Panic while holding the set's lock: it is poisoned when the guard runs.
            let _held = set.lock().unwrap();
            panic!("render failed");
        }));
        assert!(result.is_err());
        assert!(lock(&inflight).is_empty());
    }
}
