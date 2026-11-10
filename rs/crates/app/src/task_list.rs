//! Per-project task list + purge / cancel actions (hyperpanes#8).
//!
//! Everything here is pure or runs against an in-process [`WorkQueue`] (the GUI hosts the
//! control server, so it calls the same queue operations the routes use, with no HTTP hop).
//! The Slint side only renders [`TaskRow`]s and forwards click intents; the decisions
//! (which queues a row set covers, whether Cancel is enabled, what a purge/cancel reports)
//! live here so they are unit-testable without a GUI.
//!
//! Cancel semantics: a *claimed* (in-flight) task is cancelled with `nack(requeue=false)`
//! → `Failed`, exactly the route's give-up verb. A *queued* task has no queue-side cancel
//! verb today (see `docs/queue-gui-roadmap.md`), so its Cancel stays disabled with a tooltip
//! explaining why rather than inventing an unrouted delete.

use hyperpanes_core::control::work::{
    LeaseOutcome, NackOpts, QueueSummary, Task, TaskState, WorkQueue,
};

use crate::sidebar::Project;

/// Tasks fetched per queue per page; "Show more" grows the limit by this much.
pub const PAGE: usize = 50;

/// What a task panel covers: one registered project's queues, or the "Other queues" bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskScope {
    Project(String),
    Other,
}

/// The state filter a lane row selects (lane index → state), matching `lane_rows` order.
pub fn lane_state(lane: usize) -> Option<TaskState> {
    match lane {
        0 => Some(TaskState::Queued),
        1 => Some(TaskState::Claimed),
        2 => Some(TaskState::Done),
        3 => Some(TaskState::Failed),
        4 => Some(TaskState::Dead),
        _ => None,
    }
}

/// Names of the queues covered by `scope` (same mapping the badge and lanes use).
pub fn queues_for_scope(
    queues: &[QueueSummary],
    projects: &[Project],
    scope: &TaskScope,
) -> Vec<String> {
    queues
        .iter()
        .filter(|q| {
            let owner = crate::project_queue::project_for_queue(&q.queue, projects);
            match scope {
                TaskScope::Project(id) => owner == Some(id.as_str()),
                TaskScope::Other => owner.is_none(),
            }
        })
        .map(|q| q.queue.clone())
        .collect()
}

/// Tooltip shown when Cancel is disabled for a task in `state`; `None` = Cancel is enabled.
pub fn cancel_disabled_reason(state: TaskState) -> Option<&'static str> {
    match state {
        TaskState::Claimed => None,
        TaskState::Queued => {
            Some("Queued tasks can't be cancelled yet: the queue has no cancel verb for them")
        }
        _ => Some("Already finished"),
    }
}

/// One rendered task-list row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// Full task id (the action key).
    pub id: String,
    /// First 8 chars of the id, for display.
    pub id8: String,
    pub title: String,
    pub state: &'static str,
    pub attempts: String,
    pub can_cancel: bool,
    /// Tooltip explaining a disabled Cancel ("" when enabled).
    pub cancel_hint: String,
}

pub fn task_row(t: &Task) -> TaskRow {
    let reason = cancel_disabled_reason(t.state);
    TaskRow {
        id: t.id.clone(),
        id8: t.id.chars().take(8).collect(),
        title: if t.title.is_empty() {
            t.kind.clone()
        } else {
            t.title.clone()
        },
        state: t.state.as_str(),
        attempts: format!("{}/{}", t.attempts, t.max_attempts),
        can_cancel: reason.is_none(),
        cancel_hint: reason.unwrap_or("").to_string(),
    }
}

/// Fetch up to `limit` tasks per queue (optionally one state), oldest first within a queue.
/// The bool is "there may be more" (some queue returned a full page). Pages use the `seq`
/// cursor, so a long queue is read in `PAGE`-sized steps rather than one unbounded query.
pub fn fetch_tasks(
    work: &WorkQueue,
    queues: &[String],
    state: Option<TaskState>,
    limit: usize,
) -> (Vec<Task>, bool) {
    use hyperpanes_core::control::work::ListFilter;
    let mut out = Vec::new();
    let mut more = false;
    for q in queues {
        let mut after = 0u64;
        let mut got = 0usize;
        while got < limit {
            let want = (limit - got).min(PAGE);
            let page = work.list(q, ListFilter { state }, after, want);
            let n = page.len();
            if let Some(last) = page.last() {
                after = last.seq;
            }
            got += n;
            out.extend(page);
            if n < want {
                break;
            }
        }
        if got >= limit && !work.list(q, ListFilter { state }, after, 1).is_empty() {
            more = true;
        }
    }
    (out, more)
}

/// Remove done/failed/dead tasks from every queue in `queues`. Returns the count removed.
pub fn purge_terminal(work: &mut WorkQueue, queues: &[String], now: i64) -> usize {
    queues.iter().map(|q| work.purge(q, now)).sum()
}

/// Cancel an in-flight task: `nack(requeue=false)` → `Failed`. Anything else is refused
/// with a human message (queued tasks have no queue-side cancel; finished ones are done).
pub fn cancel_task(work: &mut WorkQueue, id: &str, now: i64) -> Result<(), String> {
    let Some(task) = work.get(id) else {
        return Err("Task no longer exists".into());
    };
    if let Some(why) = cancel_disabled_reason(task.state) {
        return Err(why.into());
    }
    let Some(token) = task.fencing_token else {
        return Err("Task has no active lease".into());
    };
    let opts = NackOpts {
        requeue: false,
        error: Some("cancelled from the sidebar".into()),
        delay_ms: None,
    };
    match work.nack(id, token, opts, now) {
        LeaseOutcome::Ok(_) => Ok(()),
        LeaseOutcome::Conflict => Err("Task changed state before it could be cancelled".into()),
        LeaseOutcome::NotFound => Err("Task no longer exists".into()),
    }
}

pub fn purge_message(n: usize) -> String {
    match n {
        0 => "Nothing to purge".into(),
        1 => "Purged 1 task".into(),
        n => format!("Purged {n} tasks"),
    }
}

/// An action the UI asked for, executed on the next control-host tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingAction {
    Purge,
    Cancel(String),
}

/// An open task panel (one per window): scope + lane filter + the last fetched rows.
#[derive(Debug, Clone)]
pub struct TaskPanel {
    pub scope: TaskScope,
    pub lane: usize,
    pub limit: usize,
    pub rows: Vec<TaskRow>,
    pub more: bool,
    /// Result of the last action ("toast"); empty = none.
    pub status: String,
    pub status_ok: bool,
    pub pending: Option<PendingAction>,
    /// A re-fetch was requested (opened, paged, filter changed) ahead of the 1 Hz poll.
    pub dirty: bool,
}

impl TaskPanel {
    pub fn open(scope: TaskScope, lane: usize) -> Self {
        Self {
            scope,
            lane,
            limit: PAGE,
            rows: Vec::new(),
            more: false,
            status: String::new(),
            status_ok: true,
            pending: None,
            dirty: true,
        }
    }

    pub fn needs_refresh(&self) -> bool {
        self.dirty || self.pending.is_some()
    }

    pub fn show_more(&mut self) {
        self.limit += PAGE;
        self.dirty = true;
    }

    /// Run any pending action against the queue, then re-fetch the rows (so the list
    /// refreshes after an action). `queues` is the live `queues()` overview.
    pub fn refresh(
        &mut self,
        work: &mut WorkQueue,
        queues: &[QueueSummary],
        projects: &[Project],
        now: i64,
    ) {
        let names = queues_for_scope(queues, projects, &self.scope);
        match self.pending.take() {
            Some(PendingAction::Purge) => {
                self.status = purge_message(purge_terminal(work, &names, now));
                self.status_ok = true;
            }
            Some(PendingAction::Cancel(id)) => match cancel_task(work, &id, now) {
                Ok(()) => {
                    self.status = "Task cancelled".into();
                    self.status_ok = true;
                }
                Err(e) => {
                    self.status = format!("Cancel failed: {e}");
                    self.status_ok = false;
                }
            },
            None => {}
        }
        let (tasks, more) = fetch_tasks(work, &names, lane_state(self.lane), self.limit);
        self.rows = tasks.iter().map(task_row).collect();
        self.more = more;
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperpanes_core::control::work::EnqueueOpts;

    fn proj(id: &str) -> Project {
        Project {
            id: id.into(),
            path: format!("/d/{id}"),
            name: id.into(),
            color: "#fff".into(),
            last_opened_at: None,
        }
    }

    fn enq(w: &mut WorkQueue, q: &str, title: &str) -> Task {
        let opts = EnqueueOpts {
            title: title.into(),
            ..EnqueueOpts::default()
        };
        w.enqueue(q, "{}", opts, 1_000)
    }

    #[test]
    fn scope_selects_only_its_queues() {
        let ps = vec![proj("alpha"), proj("beta")];
        let mut w = WorkQueue::open_in_memory().unwrap();
        enq(&mut w, "alpha-g1", "a");
        enq(&mut w, "alpha", "a2");
        enq(&mut w, "beta-g1", "b");
        enq(&mut w, "stray", "s");
        let qs = w.queues();
        assert_eq!(
            queues_for_scope(&qs, &ps, &TaskScope::Project("alpha".into())),
            vec!["alpha", "alpha-g1"]
        );
        assert_eq!(queues_for_scope(&qs, &ps, &TaskScope::Other), vec!["stray"]);
    }

    #[test]
    fn cancel_enablement_per_state() {
        assert!(cancel_disabled_reason(TaskState::Claimed).is_none());
        assert!(cancel_disabled_reason(TaskState::Queued)
            .unwrap()
            .contains("Queued"));
        for s in [TaskState::Done, TaskState::Failed, TaskState::Dead] {
            assert!(cancel_disabled_reason(s).is_some());
        }
    }

    #[test]
    fn row_formats_id8_attempts_and_hint() {
        let mut w = WorkQueue::open_in_memory().unwrap();
        let t = enq(&mut w, "q", "build it");
        let r = task_row(&t);
        assert_eq!(r.id8.len(), 8);
        assert!(t.id.starts_with(&r.id8));
        assert_eq!(r.title, "build it");
        assert_eq!(r.state, "queued");
        assert!(!r.can_cancel);
        assert!(!r.cancel_hint.is_empty());
    }

    #[test]
    fn cancel_claimed_task_fails_it_and_queued_is_refused() {
        let mut w = WorkQueue::open_in_memory().unwrap();
        let a = enq(&mut w, "q", "first");
        let b = enq(&mut w, "q", "second");
        let claim = w.claim("q", "worker", 60_000, 2_000).unwrap();
        assert_eq!(claim.task.id, a.id);
        assert_eq!(cancel_task(&mut w, &a.id, 3_000), Ok(()));
        assert_eq!(w.get(&a.id).unwrap().state, TaskState::Failed);
        // queued: refused, and untouched
        let err = cancel_task(&mut w, &b.id, 3_000).unwrap_err();
        assert!(err.contains("Queued"));
        assert_eq!(w.get(&b.id).unwrap().state, TaskState::Queued);
        // already terminal / missing
        assert!(cancel_task(&mut w, &a.id, 3_000).is_err());
        assert!(cancel_task(&mut w, "nope", 3_000).is_err());
    }

    #[test]
    fn purge_removes_only_terminal_across_queues() {
        let mut w = WorkQueue::open_in_memory().unwrap();
        for q in ["p-a", "p-b"] {
            let t = enq(&mut w, q, "t");
            let c = w.claim(q, "w", 60_000, 2_000).unwrap();
            assert_eq!(c.task.id, t.id);
            w.ack(&t.id, c.fencing_token, None, 2_500);
            enq(&mut w, q, "still queued");
        }
        let names = vec!["p-a".to_string(), "p-b".to_string()];
        assert_eq!(purge_terminal(&mut w, &names, 10_000), 2);
        assert_eq!(w.counts("p-a").queued, 1);
        assert_eq!(w.counts("p-a").done, 0);
        assert_eq!(purge_message(2), "Purged 2 tasks");
        assert_eq!(purge_message(0), "Nothing to purge");
    }

    #[test]
    fn fetch_pages_through_the_seq_cursor() {
        let mut w = WorkQueue::open_in_memory().unwrap();
        for i in 0..(PAGE + 5) {
            enq(&mut w, "q", &format!("t{i}"));
        }
        let names = vec!["q".to_string()];
        let (rows, more) = fetch_tasks(&w, &names, None, PAGE);
        assert_eq!(rows.len(), PAGE);
        assert!(more);
        let (rows, more) = fetch_tasks(&w, &names, None, PAGE * 2);
        assert_eq!(rows.len(), PAGE + 5);
        assert!(!more);
        let (rows, _) = fetch_tasks(&w, &names, Some(TaskState::Done), PAGE);
        assert!(rows.is_empty());
    }

    #[test]
    fn panel_refresh_runs_action_then_refetches_and_reports() {
        let ps = vec![proj("alpha")];
        let mut w = WorkQueue::open_in_memory().unwrap();
        let t = enq(&mut w, "alpha-g1", "work");
        let mut p = TaskPanel::open(TaskScope::Project("alpha".into()), 1);
        let qs = w.queues();
        p.refresh(&mut w, &qs, &ps, 2_000);
        assert!(p.rows.is_empty()); // nothing claimed yet
        w.claim("alpha-g1", "w", 60_000, 2_000).unwrap();
        p.pending = Some(PendingAction::Cancel(t.id.clone()));
        assert!(p.needs_refresh());
        let qs = w.queues();
        p.refresh(&mut w, &qs, &ps, 3_000);
        assert_eq!(p.status, "Task cancelled");
        assert!(p.status_ok);
        assert!(p.rows.is_empty()); // list refreshed: the Running lane is now empty
        p.pending = Some(PendingAction::Cancel(t.id));
        let qs = w.queues();
        p.refresh(&mut w, &qs, &ps, 4_000);
        assert!(p.status.starts_with("Cancel failed"));
        assert!(!p.status_ok);
    }
}
