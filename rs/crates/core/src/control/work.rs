//! Durable, claimable **work queue** for the control plane (worker-pool Phase 2).
//!
//! Where `inbox.rs` is a per-pane *fan-out notification bus* (directed, at-least-once,
//! lost on restart, no ownership), this is its inverse: a **competing-consumers** queue
//! a controller pane uses to dispatch tasks to a pool of headless worker panes. It adds
//! exactly the three properties the inbox lacks — **claim ownership**, **leases with
//! fencing**, and **durability** — and deliberately reuses every inbox idiom a reviewer
//! already knows (monotonic `seq` as a list cursor, per-namespace grouping, an injected
//! `now: i64` clock so the core is deterministic without a wall clock).
//!
//! State machine (doc 01 §1 / doc 05): a `Task` flows
//!
//! ```text
//!                 claim (lease minted, fencing_token++, attempts++)
//!     Queued ───────────────────────────────────────────────▶ Claimed
//!       ▲  ▲                                                    │ │ │
//!       │  │ nack(requeue, attempts<max) / visibility reap      │ │ │ ack
//!       │  └────────────────────────────────────────────────────┘ │ ▼
//!       │            available_at = now + backoff                  │ Done   (terminal)
//!       │                                                          │
//!       │  nack(requeue, attempts>=max) / reap-when-exhausted      ▼
//!       │                                                        Dead   (terminal, dead-letter)
//!       └─  nack(requeue=false) ───────────────────────────────▶ Failed (terminal, gave up)
//! ```
//!
//! **Delivery semantics — at-least-once (kept honest, inbox-style).** A worker that
//! finishes its side effect and then dies *before* `ack` has its lease reaped and the
//! task re-run. Handlers MUST be idempotent. Exactly-once is not offered; `dedupe_key`
//! collapses duplicate *enqueues*, never duplicate *executions*.
//!
//! **Fencing (doc 05 invariant).** Each claim mints a strictly-increasing
//! `fencing_token`. The classic hazard — worker A stalls past its deadline, the reaper
//! requeues, worker B claims and works, then A wakes and tries to `ack` — is defeated
//! because A still holds the *old* token: every A op returns [`LeaseOutcome::Conflict`].
//! Only the holder of the current token may `ack`/`nack`/`extend`. This is what makes the
//! visibility timeout *safe*, not merely convenient.
//!
//! **Persistence — SQLite (WAL), the design's choice.** A claimable queue's three hard
//! operations (atomic "claim the oldest queued AND mark it, exactly once"; indexed
//! visibility-timeout reap; count-by-state) are all *queries*; a log answers them only by
//! replaying a full in-memory index on boot. SQLite gives transactions, indexes and
//! crash-consistency for free, with a `:memory:` test story that is *more* deterministic
//! than a log. The pure rules ([`backoff`], [`should_dead_letter`], [`TaskState`]) stay
//! free functions with plain-value unit tests; [`WorkQueue`] is the thin stateful shell
//! that runs them against SQLite — open it with [`WorkQueue::open_in_memory`] for tests
//! (mirrors `serve_for_test`) or [`WorkQueue::open`] for a real DB file.
//!
//! Scope note: the queue is **generic** — `payload`/`source` are opaque to it (doc 05
//! "opaque at storage, typed at the edges"). Application-level fields the controller and
//! worker agree on (repo, base, prompt, branch, pr, …) ride inside the opaque `payload`;
//! the queue owns only lifecycle + lease + retry/backoff columns.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use serde::{Deserialize, Serialize};

use super::holder::{self, Holder};

/// Default dead-letter threshold (doc 01 §1: dead-letter once `attempts == max_attempts`).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 5;
/// Default lease length minted on claim when the caller passes `lease_ms <= 0`.
pub const DEFAULT_VISIBILITY_MS: i64 = 30_000;
/// Backoff base: first retry waits ~this long (doc 01 §2.3).
pub const BACKOFF_BASE_MS: i64 = 1_000;
/// Backoff ceiling so exponential growth can't run away.
pub const BACKOFF_CAP_MS: i64 = 60_000;

/// Bump on any schema change; gates forward migrations on open (doc 01 §3.4).
const SCHEMA_VERSION: i64 = 3;

// ---------------------------------------------------------------------------
// Pure value types + rules (no DB — unit-tested with plain values, inbox-style)
// ---------------------------------------------------------------------------

/// The five task states (doc 05). `Failed` = a worker/operator explicitly gave up
/// (`nack(requeue=false)`); `Dead` = retries exhausted (the dead-letter). Both are terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TaskState {
    Queued,
    Claimed,
    Done,
    Failed,
    Dead,
}

impl TaskState {
    /// Lowercase wire form — also the value stored in the `state` column.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
            TaskState::Failed => "failed",
            TaskState::Dead => "dead",
        }
    }

    /// Inverse of [`as_str`](Self::as_str); unknown text falls back to `Queued` (we fully
    /// control writes, so this only guards against a hand-edited DB).
    pub fn from_wire(s: &str) -> TaskState {
        match s {
            "claimed" => TaskState::Claimed,
            "done" => TaskState::Done,
            "failed" => TaskState::Failed,
            "dead" => TaskState::Dead,
            _ => TaskState::Queued,
        }
    }

    /// `Done`/`Failed`/`Dead` are terminal — no further lease ops are accepted.
    pub fn is_terminal(self) -> bool {
        matches!(self, TaskState::Done | TaskState::Failed | TaskState::Dead)
    }
}

/// Retry backoff (pure). `override_ms` wins when set (clamped non-negative); otherwise
/// exponential `base * 2^(attempts-1)` capped at [`BACKOFF_CAP_MS`]. `attempts` is the
/// task's attempt count *at nack time* (already incremented by the preceding claim), so
/// the first retry (attempts == 1) waits [`BACKOFF_BASE_MS`].
pub fn backoff(attempts: u32, override_ms: Option<i64>) -> i64 {
    if let Some(o) = override_ms {
        return o.max(0);
    }
    let shift = attempts.saturating_sub(1).min(20);
    BACKOFF_BASE_MS
        .saturating_mul(1i64 << shift)
        .min(BACKOFF_CAP_MS)
}

/// Whether a failed attempt has exhausted its retries and must dead-letter (doc 05).
pub fn should_dead_letter(attempts: u32, max_attempts: u32) -> bool {
    attempts >= max_attempts
}

/// The durable task record. Field order is serialization-stable (we serialize the struct
/// directly, never a key-sorted `Value`), and timestamps are ms-epoch `i64` to match the
/// injected-`now` clock every other control core uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    /// uuid v4, queue-assigned, stable across retries — the join key everywhere (doc 05).
    pub id: String,
    /// Logical queue / namespace (e.g. `"build"`); default `"default"`.
    pub queue: String,
    /// Global monotonic id; doubles as the [`list`](WorkQueue::list) cursor (inbox parity).
    pub seq: u64,
    /// Discriminator for the typed edges (`ci_failure | lint | issue | review | manual | …`).
    pub kind: String,
    /// Short human title (seeds a pane label / PR title at the edges).
    pub title: String,
    pub state: TaskState,
    /// Opaque body — JSON string or text; the queue never parses it.
    pub payload: String,
    /// Higher first; default 0.
    pub priority: i64,
    /// Incremented on each claim.
    pub attempts: u32,
    /// Dead-letter once `attempts >= max_attempts`.
    pub max_attempts: u32,
    /// ms epoch; claimable only when `available_at <= now` (delay / backoff gate;
    /// doc 05's `not_before`).
    pub available_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_by: Option<String>,
    /// Monotonic fencing token for the current claim (doc 05). Retained across requeue so
    /// the counter never regresses on restart; only meaningful while `state == Claimed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fencing_token: Option<u64>,
    /// ms epoch; reaped back to `Queued` (or dead-lettered) once `now` passes this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility_deadline: Option<i64>,
    /// Recorded on `ack`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Recorded on `nack` / dead-letter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Optional idempotent-enqueue key (collapses duplicate enqueues while live).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The goal this task belongs to (goals system), if any. Free-form id owned by the
    /// orchestrator; the queue only stores + echoes it (and indexes it for per-goal listing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal_id: Option<String>,
    /// Task ids this task depends on: it stays unclaimable until EVERY listed task is `Done`
    /// (a missing/purged dep id counts as unsatisfied, so it blocks). Empty/None ⇒ no deps.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<Vec<String>>,
}

/// Options for [`WorkQueue::enqueue`]. `Default` mirrors the doc's defaults.
#[derive(Debug, Clone)]
pub struct EnqueueOpts {
    pub kind: String,
    pub title: String,
    pub priority: i64,
    pub max_attempts: u32,
    /// Default lease length for this task; used by `claim` when the caller passes `lease_ms <= 0`.
    pub visibility_timeout_ms: i64,
    /// Schedule for the future (ms epoch); `None` ⇒ available now.
    pub available_at: Option<i64>,
    pub dedupe_key: Option<String>,
    /// Goal this task belongs to (goals system); stored + echoed, not interpreted.
    pub goal_id: Option<String>,
    /// Task ids that must all be `Done` before this task becomes claimable.
    pub depends_on: Option<Vec<String>>,
}

impl Default for EnqueueOpts {
    fn default() -> Self {
        EnqueueOpts {
            kind: "manual".to_string(),
            title: String::new(),
            priority: 0,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            visibility_timeout_ms: DEFAULT_VISIBILITY_MS,
            available_at: None,
            dedupe_key: None,
            goal_id: None,
            depends_on: None,
        }
    }
}

/// Options for [`WorkQueue::nack`].
#[derive(Debug, Clone, Default)]
pub struct NackOpts {
    /// `true` ⇒ retry (→ `Queued` with backoff, or `Dead` if exhausted);
    /// `false` ⇒ give up now (→ `Failed`).
    pub requeue: bool,
    pub error: Option<String>,
    /// Override the computed backoff for this requeue (ms).
    pub delay_ms: Option<i64>,
}

/// Filter for [`WorkQueue::list`].
#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub state: Option<TaskState>,
}

/// Returned by [`WorkQueue::claim`]: the now-`Claimed` task plus the fencing token the
/// worker must present to `ack`/`nack`/`extend`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub task: Task,
    pub fencing_token: u64,
}

/// Result of a lease-guarded op (`ack`/`nack`/`extend`). `Conflict` ⇒ HTTP 409 (stale
/// lease / wrong state); `NotFound` ⇒ 404.
#[derive(Debug, Clone, PartialEq, Eq)]
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::large_enum_variant)]
pub enum LeaseOutcome {
    Ok(Task),
    Conflict,
    NotFound,
}

/// What the reaper / fast-requeue did to a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Disposition {
    Requeued,
    DeadLettered,
}

/// A task the reaper (or worker-exit fast path) acted on, plus the disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaped {
    pub task: Task,
    pub disposition: Disposition,
}

/// What boot recovery did: `reaped` = claims of dead holders, requeued or dead-lettered;
/// `kept` = claims of live holders, left `Claimed` (same fence) with a re-armed lease.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recovery {
    pub reaped: Vec<Reaped>,
    pub kept: Vec<Task>,
}

/// Queue depth by state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub queued: usize,
    pub claimed: usize,
    pub done: usize,
    pub failed: usize,
    pub dead: usize,
}

impl Counts {
    fn add(&mut self, state: &str, n: usize) {
        match TaskState::from_wire(state) {
            TaskState::Queued => self.queued += n,
            TaskState::Claimed => self.claimed += n,
            TaskState::Done => self.done += n,
            TaskState::Failed => self.failed += n,
            TaskState::Dead => self.dead += n,
        }
    }

    pub fn total(&self) -> usize {
        self.queued + self.claimed + self.done + self.failed + self.dead
    }
}

/// One queue and its depth, for a `/queues`-style overview.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSummary {
    pub queue: String,
    pub counts: Counts,
}

// ---------------------------------------------------------------------------
// The stateful shell: WorkQueue over SQLite
// ---------------------------------------------------------------------------

/// All `Task` columns, in the order [`row_to_task`] reads them. (Excludes the internal
/// columns: `visibility_timeout_ms`, which only `claim` and boot recovery read, and
/// `holder_pid`/`holder_start`, the claim holder's identity, which only boot recovery reads.
/// None of them are on the wire.)
const COLS: &str = "id, queue, seq, kind, title, state, payload, priority, attempts, \
                    max_attempts, available_at, claimed_by, fencing_token, \
                    visibility_deadline, result, error, dedupe_key, created_at, updated_at, \
                    goal_id, depends_on";

const SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS schema_version (v INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (
    id                    TEXT PRIMARY KEY,
    queue                 TEXT NOT NULL,
    seq                   INTEGER NOT NULL UNIQUE,
    kind                  TEXT NOT NULL DEFAULT 'manual',
    title                 TEXT NOT NULL DEFAULT '',
    state                 TEXT NOT NULL,
    payload               TEXT NOT NULL,
    priority              INTEGER NOT NULL DEFAULT 0,
    attempts              INTEGER NOT NULL DEFAULT 0,
    max_attempts          INTEGER NOT NULL DEFAULT 5,
    visibility_timeout_ms INTEGER NOT NULL DEFAULT 30000,
    available_at          INTEGER NOT NULL,
    claimed_by            TEXT,
    fencing_token         INTEGER,
    visibility_deadline   INTEGER,
    result                TEXT,
    error                 TEXT,
    dedupe_key            TEXT,
    created_at            INTEGER NOT NULL,
    updated_at            INTEGER NOT NULL,
    goal_id               TEXT,
    depends_on            TEXT,
    holder_pid            INTEGER,
    holder_start          TEXT
);
CREATE INDEX IF NOT EXISTS idx_claimable  ON tasks(queue, state, available_at, priority, seq);
CREATE INDEX IF NOT EXISTS idx_visibility ON tasks(state, visibility_deadline);
CREATE INDEX IF NOT EXISTS idx_seq        ON tasks(queue, seq);
CREATE INDEX IF NOT EXISTS idx_claimed_by ON tasks(claimed_by) WHERE state='claimed';
CREATE UNIQUE INDEX IF NOT EXISTS uq_dedupe ON tasks(queue, dedupe_key)
    WHERE dedupe_key IS NOT NULL AND state IN ('queued','claimed');
";

/// The durable queue. Holds one `rusqlite::Connection` (Send, not Sync — the embedder
/// wraps it in a `Mutex`, exactly like every other control component) plus the in-memory
/// `seq` / `fence` counters loaded once on open so mint stays a single round-trip.
pub struct WorkQueue {
    conn: Connection,
    seq: u64,
    fence: u64,
}

impl WorkQueue {
    /// Open (or create) the queue DB at `path`, in WAL mode. Does **not** run the boot
    /// requeue — the embedder calls [`recover_in_flight`](Self::recover_in_flight) after
    /// open (doc 01 §3.3) so tests can observe pre-recovery state.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// Open an ephemeral in-memory queue — the deterministic test substrate (doc 01 §3.1),
    /// the queue's analogue of `serve_for_test`'s ephemeral server.
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        // WAL = single writer + concurrent readers (matches one Mutex<WorkQueue>);
        // NORMAL is durable across a process crash (the stated requirement). On a
        // `:memory:` DB these PRAGMAs are no-ops, which is fine for tests.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=2000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // Migrations. `SCHEMA`'s `CREATE TABLE IF NOT EXISTS` gives a *fresh* DB every current
        // column, but leaves an *existing* older table untouched — so forward-migrate by the
        // recorded version. Read it (0 = fresh), apply each step, then stamp the current version.
        let current: i64 = conn
            .query_row("SELECT COALESCE(MAX(v),0) FROM schema_version", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        if current == 0 {
            conn.execute(
                "INSERT INTO schema_version (v) VALUES (?1)",
                params![SCHEMA_VERSION],
            )?;
        } else if current < SCHEMA_VERSION {
            // v1 -> v2: goal-DAG columns. Only an existing v1 table is missing them (a fresh DB
            // got them from `SCHEMA`), so ADD COLUMN never hits a duplicate here.
            if current < 2 {
                conn.execute_batch(
                    "ALTER TABLE tasks ADD COLUMN goal_id TEXT;
                     ALTER TABLE tasks ADD COLUMN depends_on TEXT;",
                )?;
            }
            // v2 -> v3: claim-holder identity (boot recovery keeps a live holder's claim).
            if current < 3 {
                conn.execute_batch(
                    "ALTER TABLE tasks ADD COLUMN holder_pid INTEGER;
                     ALTER TABLE tasks ADD COLUMN holder_start TEXT;",
                )?;
            }
            conn.execute("UPDATE schema_version SET v = ?1", params![SCHEMA_VERSION])?;
        }
        // Indexes over migration-added columns: create AFTER the ALTER, so `goal_id` exists on
        // both a fresh DB (created by SCHEMA) and a migrated v1 DB (added just above).
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_goal ON tasks(goal_id) WHERE goal_id IS NOT NULL;",
        )?;
        // Resume the monotonic counters. `seq` survives on every row; `fencing_token` is
        // never nulled on requeue (only overwritten by the next claim), so MAX never regresses.
        let seq: i64 =
            conn.query_row("SELECT COALESCE(MAX(seq),0) FROM tasks", [], |r| r.get(0))?;
        let fence: i64 = conn.query_row(
            "SELECT COALESCE(MAX(fencing_token),0) FROM tasks",
            [],
            |r| r.get(0),
        )?;
        Ok(WorkQueue {
            conn,
            seq: seq as u64,
            fence: fence as u64,
        })
    }

    // --- writes -----------------------------------------------------------

    /// Enqueue a task into `queue`. If `opts.dedupe_key` matches a still-live row
    /// (`queued`/`claimed`) the existing task is returned unchanged (idempotent enqueue);
    /// the key frees once that task reaches a terminal state.
    pub fn enqueue(&mut self, queue: &str, payload: &str, opts: EnqueueOpts, now: i64) -> Task {
        if let Some(key) = opts.dedupe_key.as_deref() {
            if let Some(existing) = self.find_live_by_dedupe(queue, key) {
                return existing;
            }
        }
        self.seq += 1;
        let seq = self.seq;
        let id = uuid::Uuid::new_v4().to_string();
        let available_at = opts.available_at.unwrap_or(now);
        // depends_on is stored as a JSON array string (queried via json_each in `claim`).
        let depends_on_json = opts
            .depends_on
            .as_ref()
            .map(|d| serde_json::to_string(d).unwrap_or_else(|_| "[]".to_string()));
        self.conn
            .execute(
                "INSERT INTO tasks
                 (id, queue, seq, kind, title, state, payload, priority, attempts,
                  max_attempts, visibility_timeout_ms, available_at, dedupe_key,
                  created_at, updated_at, goal_id, depends_on)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7, 0, ?8, ?9, ?10, ?11, ?12, ?12, ?13, ?14)",
                params![
                    id,
                    queue,
                    seq as i64,
                    opts.kind,
                    opts.title,
                    payload,
                    opts.priority,
                    opts.max_attempts as i64,
                    opts.visibility_timeout_ms,
                    available_at,
                    opts.dedupe_key,
                    now,
                    opts.goal_id,
                    depends_on_json,
                ],
            )
            .expect("enqueue insert");
        Self::fetch(&self.conn, &id)
            .ok()
            .flatten()
            .expect("row just inserted")
    }

    /// The competing-consumers primitive: atomically pick the best claimable task in
    /// `queue` and mark it `Claimed` for `worker`, minting a fresh fencing token. Returns
    /// `None` if nothing is claimable. Ordered priority DESC, then FIFO (`available_at`,
    /// `seq`). Runs in a single `BEGIN IMMEDIATE` transaction with a guarded re-check, so
    /// two concurrent claimers can never win the same row. `lease_ms <= 0` falls back to
    /// the task's `visibility_timeout_ms`. Records no holder process, so boot recovery
    /// treats the claim as dead (see [`claim_held`](Self::claim_held)).
    pub fn claim(&mut self, queue: &str, worker: &str, lease_ms: i64, now: i64) -> Option<Claim> {
        self.claim_held(queue, worker, None, lease_ms, now)
    }

    /// [`claim`](Self::claim), recording the claiming process as the claim's `holder`
    /// (from [`holder::identify`]). Boot recovery leaves a claim whose holder is still
    /// alive in place, fence and all, instead of requeueing it, so a worker that survived
    /// the previous owner (a GUI relaunch with the session daemon on) can still ack.
    pub fn claim_held(
        &mut self,
        queue: &str,
        worker: &str,
        holder: Option<&Holder>,
        lease_ms: i64,
        now: i64,
    ) -> Option<Claim> {
        let next_fence = self.fence + 1;
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .ok()?;
        let picked: Option<(String, i64)> = tx
            .query_row(
                "SELECT id, visibility_timeout_ms FROM tasks
                 WHERE queue = ?1 AND state = 'queued' AND available_at <= ?2
                   AND (
                     depends_on IS NULL
                     OR NOT EXISTS (
                       SELECT 1 FROM json_each(tasks.depends_on) AS dep
                       WHERE NOT EXISTS (
                         SELECT 1 FROM tasks d WHERE d.id = dep.value AND d.state = 'done'
                       )
                     )
                   )
                 ORDER BY priority DESC, available_at ASC, seq ASC
                 LIMIT 1",
                params![queue, now],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .ok()?;
        let (id, default_lease) = picked?; // None ⇒ tx dropped (rolled back), no token spent
        let lease = if lease_ms > 0 {
            lease_ms
        } else {
            default_lease
        };
        let deadline = now + lease;
        // Always overwrite the holder columns (NULL when none), so a previous claim's holder
        // can never be mistaken for this one's.
        tx.execute(
            "UPDATE tasks SET state='claimed', claimed_by=?2, fencing_token=?3,
                 attempts = attempts + 1, visibility_deadline=?4, updated_at=?5,
                 holder_pid=?6, holder_start=?7
             WHERE id=?1 AND state='queued'",
            params![
                id,
                worker,
                next_fence as i64,
                deadline,
                now,
                holder.map(|h| h.pid as i64),
                holder.and_then(|h| h.start.as_deref()),
            ],
        )
        .ok()?;
        let task = Self::fetch(&tx, &id).ok().flatten()?;
        tx.commit().ok()?;
        self.fence = next_fence;
        Some(Claim {
            task,
            fencing_token: next_fence,
        })
    }

    /// Complete a claimed task (`Claimed → Done`), recording `result`. Lease-guarded.
    /// Re-acking an already-`Done` task with the **same** token is idempotent success (a
    /// worker retrying a lost 200); any other state / token mismatch is `Conflict`.
    pub fn ack(
        &mut self,
        id: &str,
        fencing_token: u64,
        result: Option<&str>,
        now: i64,
    ) -> LeaseOutcome {
        let Some(task) = Self::fetch(&self.conn, id).ok().flatten() else {
            return LeaseOutcome::NotFound;
        };
        if task.state == TaskState::Done {
            return if task.fencing_token == Some(fencing_token) {
                LeaseOutcome::Ok(task)
            } else {
                LeaseOutcome::Conflict
            };
        }
        if task.state != TaskState::Claimed || task.fencing_token != Some(fencing_token) {
            return LeaseOutcome::Conflict;
        }
        let _ = self.conn.execute(
            "UPDATE tasks SET state='done', result=?2, visibility_deadline=NULL, updated_at=?3
             WHERE id=?1",
            params![id, result, now],
        );
        LeaseOutcome::Ok(
            Self::fetch(&self.conn, id)
                .ok()
                .flatten()
                .expect("acked row"),
        )
    }

    /// Fail/retry a claimed task. Lease-guarded. `requeue=true` & `attempts < max_attempts`
    /// → `Queued` with `available_at = now + backoff`; `requeue=true` & exhausted →
    /// `Dead`; `requeue=false` → `Failed`. The stale-worker case (token mismatch) is `Conflict`.
    pub fn nack(&mut self, id: &str, fencing_token: u64, opts: NackOpts, now: i64) -> LeaseOutcome {
        let Some(task) = Self::fetch(&self.conn, id).ok().flatten() else {
            return LeaseOutcome::NotFound;
        };
        if task.state != TaskState::Claimed || task.fencing_token != Some(fencing_token) {
            return LeaseOutcome::Conflict;
        }
        if !opts.requeue {
            let _ = self.conn.execute(
                "UPDATE tasks SET state='failed', visibility_deadline=NULL, error=?2, updated_at=?3
                 WHERE id=?1",
                params![id, opts.error, now],
            );
        } else if should_dead_letter(task.attempts, task.max_attempts) {
            let _ = self.conn.execute(
                "UPDATE tasks SET state='dead', visibility_deadline=NULL, error=?2, updated_at=?3
                 WHERE id=?1",
                params![id, opts.error, now],
            );
        } else {
            let next = now + backoff(task.attempts, opts.delay_ms);
            let _ = self.conn.execute(
                "UPDATE tasks SET state='queued', claimed_by=NULL, visibility_deadline=NULL,
                     available_at=?2, error=?3, updated_at=?4
                 WHERE id=?1",
                params![id, next, opts.error, now],
            );
        }
        LeaseOutcome::Ok(
            Self::fetch(&self.conn, id)
                .ok()
                .flatten()
                .expect("nacked row"),
        )
    }

    /// Heartbeat for a long task: extend the current lease by `extra_ms` (from the existing
    /// deadline — never shortens it). Lease-guarded.
    pub fn extend(
        &mut self,
        id: &str,
        fencing_token: u64,
        extra_ms: i64,
        now: i64,
    ) -> LeaseOutcome {
        let Some(task) = Self::fetch(&self.conn, id).ok().flatten() else {
            return LeaseOutcome::NotFound;
        };
        if task.state != TaskState::Claimed || task.fencing_token != Some(fencing_token) {
            return LeaseOutcome::Conflict;
        }
        let new_deadline = task.visibility_deadline.unwrap_or(now) + extra_ms;
        let _ = self.conn.execute(
            "UPDATE tasks SET visibility_deadline=?2, updated_at=?3 WHERE id=?1",
            params![id, new_deadline, now],
        );
        LeaseOutcome::Ok(
            Self::fetch(&self.conn, id)
                .ok()
                .flatten()
                .expect("extended row"),
        )
    }

    // --- reaping ----------------------------------------------------------

    /// The visibility-timeout sweep (run on a background ticker). Every `claimed` task
    /// whose deadline has passed is requeued (`attempts < max_attempts`) or dead-lettered.
    /// Requeue keeps the now-stale token on the row, so the late worker is fenced out.
    pub fn reap_expired(&mut self, now: i64) -> Vec<Reaped> {
        let ids = self.claimed_ids(
            "visibility_deadline IS NOT NULL AND visibility_deadline <= ?1",
            params![now],
        );
        self.requeue_or_dead_letter(ids, now, "visibility timeout: lease expired")
    }

    /// Fast requeue on worker exit (doc 01 §6): immediately recover a dead pane's in-flight
    /// claims without waiting a full visibility timeout. Hook this from the session
    /// `Exit` arm once `uid → paneId` is resolved.
    pub fn requeue_worker(&mut self, worker: &str, now: i64) -> Vec<Reaped> {
        let ids = self.claimed_ids("claimed_by = ?1", params![worker]);
        self.requeue_or_dead_letter(ids, now, "worker exited: lease released")
    }

    /// Boot recovery (doc 01 §3.3), run once by the DB's new owner: requeue every claim
    /// whose holder died with (or since) the previous owner, and leave live holders alone.
    /// Returns what was requeued / dead-lettered; see
    /// [`recover_in_flight_with`](Self::recover_in_flight_with) for the rules.
    pub fn recover_in_flight(&mut self, now: i64) -> Vec<Reaped> {
        self.recover_in_flight_with(now, &holder::is_alive).reaped
    }

    /// Boot recovery with an injectable liveness probe (`alive(pid, start_fingerprint)`;
    /// production passes [`holder::is_alive`]). Per `claimed` row:
    ///
    /// - **No holder recorded** (an in-process claim, an MCP `claim_task`, a pre-v3 row, a
    ///   remote claimer): requeue now (exhausted ones dead-letter). That is the original
    ///   premise: the holder lived in the previous owner and died with it.
    /// - **Holder recorded but dead** (pid gone or a zombie, or its start fingerprint no
    ///   longer matches: the pid was recycled, or the machine rebooted): requeue now. A
    ///   full restart, a crash that took the session daemon down, and a reboot all land
    ///   here, so recovering a truly dead claim is as immediate as before.
    /// - **Holder alive** (a worker that survived a GUI relaunch in the session daemon):
    ///   keep it `Claimed` with its fencing token, so its ack/extend still succeed, and
    ///   re-arm the lease to `max(deadline, now + visibility_timeout_ms)`, giving it one
    ///   full lease to reconnect even if the app was down longer than its lease. If the
    ///   probe was fooled (a recycled pid on a platform without a start fingerprint), the
    ///   lease reaper requeues the claim once that re-armed lease expires.
    pub fn recover_in_flight_with(
        &mut self,
        now: i64,
        alive: &dyn Fn(u32, Option<&str>) -> bool,
    ) -> Recovery {
        let mut dead = Vec::new();
        let mut live = Vec::new();
        for (id, pid, start) in self.claimed_holders() {
            match pid {
                Some(pid) if alive(pid, start.as_deref()) => live.push(id),
                _ => dead.push(id),
            }
        }
        let reaped = self.requeue_or_dead_letter(
            dead,
            now,
            "control server restarted: in-flight task recovered",
        );
        let mut kept = Vec::new();
        for id in live {
            let _ = self.conn.execute(
                "UPDATE tasks SET visibility_deadline =
                     MAX(COALESCE(visibility_deadline, ?2), ?2 + visibility_timeout_ms),
                     updated_at = ?2
                 WHERE id = ?1 AND state = 'claimed'",
                params![id, now],
            );
            if let Some(t) = Self::fetch(&self.conn, &id).ok().flatten() {
                kept.push(t);
            }
        }
        Recovery { reaped, kept }
    }

    /// `(id, holder_pid, holder_start)` of every `claimed` row.
    fn claimed_holders(&self) -> Vec<(String, Option<u32>, Option<String>)> {
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT id, holder_pid, holder_start FROM tasks WHERE state='claimed'")
        else {
            return Vec::new();
        };
        let rows = match stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?
                    .and_then(|p| u32::try_from(p).ok()),
                r.get::<_, Option<String>>(2)?,
            ))
        }) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        rows
    }

    /// Collect ids of `claimed` rows matching an extra predicate.
    fn claimed_ids(&self, predicate: &str, p: impl Params) -> Vec<String> {
        let sql = format!("SELECT id FROM tasks WHERE state='claimed' AND {predicate}");
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let ids = match stmt.query_map(p, |r| r.get::<_, String>(0)) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        ids
    }

    fn requeue_or_dead_letter(&mut self, ids: Vec<String>, now: i64, reason: &str) -> Vec<Reaped> {
        let mut out = Vec::new();
        for id in ids {
            let Some(task) = Self::fetch(&self.conn, &id).ok().flatten() else {
                continue;
            };
            if task.state != TaskState::Claimed {
                continue; // raced with an ack/nack since we collected ids
            }
            let disposition = if should_dead_letter(task.attempts, task.max_attempts) {
                let _ = self.conn.execute(
                    "UPDATE tasks SET state='dead', visibility_deadline=NULL, error=?2,
                         updated_at=?3 WHERE id=?1",
                    params![id, reason, now],
                );
                Disposition::DeadLettered
            } else {
                let _ = self.conn.execute(
                    "UPDATE tasks SET state='queued', claimed_by=NULL, visibility_deadline=NULL,
                         available_at=?2, updated_at=?2 WHERE id=?1",
                    params![id, now],
                );
                Disposition::Requeued
            };
            if let Some(updated) = Self::fetch(&self.conn, &id).ok().flatten() {
                out.push(Reaped {
                    task: updated,
                    disposition,
                });
            }
        }
        out
    }

    // --- reads ------------------------------------------------------------

    /// Fetch one task by id.
    pub fn get(&self, id: &str) -> Option<Task> {
        Self::fetch(&self.conn, id).ok().flatten()
    }

    /// List a queue's tasks with `seq > after` (the cursor), optionally filtered by state,
    /// ordered by `seq` ascending, capped at `limit`.
    pub fn list(&self, queue: &str, filter: ListFilter, after: u64, limit: usize) -> Vec<Task> {
        let mut sql = format!("SELECT {COLS} FROM tasks WHERE queue = ?1 AND seq > ?2");
        if let Some(state) = filter.state {
            // state.as_str() is a fixed enum string — safe to inline (no injection surface).
            sql.push_str(&format!(" AND state = '{}'", state.as_str()));
        }
        sql.push_str(" ORDER BY seq ASC LIMIT ?3");
        let mut stmt = match self.conn.prepare(&sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let tasks = match stmt.query_map(params![queue, after as i64, limit as i64], row_to_task) {
            Ok(rows) => rows.filter_map(Result::ok).collect(),
            Err(_) => Vec::new(),
        };
        tasks
    }

    /// Depth-by-state for one queue (so a controller can tell when its batch has drained).
    pub fn counts(&self, queue: &str) -> Counts {
        let mut c = Counts::default();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT state, COUNT(*) FROM tasks WHERE queue = ?1 GROUP BY state")
        else {
            return c;
        };
        if let Ok(rows) = stmt.query_map(params![queue], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        }) {
            for (state, n) in rows.flatten() {
                c.add(&state, n);
            }
        }
        c
    }

    /// Every queue with at least one task, plus its depth — for a `/queues` overview.
    /// Sorted by queue name (deterministic).
    pub fn queues(&self) -> Vec<QueueSummary> {
        let mut map: std::collections::BTreeMap<String, Counts> = std::collections::BTreeMap::new();
        let Ok(mut stmt) = self
            .conn
            .prepare("SELECT queue, state, COUNT(*) FROM tasks GROUP BY queue, state")
        else {
            return Vec::new();
        };
        if let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as usize,
            ))
        }) {
            for (queue, state, n) in rows.flatten() {
                map.entry(queue).or_default().add(&state, n);
            }
        }
        map.into_iter()
            .map(|(queue, counts)| QueueSummary { queue, counts })
            .collect()
    }

    /// Retention: delete terminal (`done`/`failed`/`dead`) tasks in `queue` last touched at
    /// or before `older_than` (ms epoch). Returns the number removed.
    pub fn purge(&mut self, queue: &str, older_than: i64) -> usize {
        self.conn
            .execute(
                "DELETE FROM tasks WHERE queue = ?1
                 AND state IN ('done','failed','dead') AND updated_at <= ?2",
                params![queue, older_than],
            )
            .unwrap_or(0)
    }

    // --- helpers ----------------------------------------------------------

    fn find_live_by_dedupe(&self, queue: &str, key: &str) -> Option<Task> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {COLS} FROM tasks
                     WHERE queue = ?1 AND dedupe_key = ?2 AND state IN ('queued','claimed')
                     LIMIT 1"
                ),
                params![queue, key],
                row_to_task,
            )
            .optional()
            .ok()
            .flatten()
    }

    fn fetch(conn: &Connection, id: &str) -> rusqlite::Result<Option<Task>> {
        conn.query_row(
            &format!("SELECT {COLS} FROM tasks WHERE id = ?1"),
            params![id],
            row_to_task,
        )
        .optional()
    }
}

/// Map a `tasks` row (in `COLS` order) to a [`Task`].
fn row_to_task(r: &Row) -> rusqlite::Result<Task> {
    let state: String = r.get(5)?;
    Ok(Task {
        id: r.get(0)?,
        queue: r.get(1)?,
        seq: r.get::<_, i64>(2)? as u64,
        kind: r.get(3)?,
        title: r.get(4)?,
        state: TaskState::from_wire(&state),
        payload: r.get(6)?,
        priority: r.get(7)?,
        attempts: r.get::<_, i64>(8)? as u32,
        max_attempts: r.get::<_, i64>(9)? as u32,
        available_at: r.get(10)?,
        claimed_by: r.get(11)?,
        fencing_token: r.get::<_, Option<i64>>(12)?.map(|v| v as u64),
        visibility_deadline: r.get(13)?,
        result: r.get(14)?,
        error: r.get(15)?,
        dedupe_key: r.get(16)?,
        created_at: r.get(17)?,
        updated_at: r.get(18)?,
        goal_id: r.get(19)?,
        // depends_on is stored as a JSON array string; parse back to Vec (None on NULL/garbage).
        depends_on: r
            .get::<_, Option<String>>(20)?
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q() -> WorkQueue {
        WorkQueue::open_in_memory().expect("open :memory:")
    }

    fn enq(wq: &mut WorkQueue, queue: &str, payload: &str, now: i64) -> Task {
        wq.enqueue(queue, payload, EnqueueOpts::default(), now)
    }

    // --- pure rules -------------------------------------------------------

    #[test]
    fn backoff_grows_exponentially_caps_and_honors_override() {
        assert_eq!(backoff(1, None), 1_000); // first retry = base
        assert_eq!(backoff(2, None), 2_000);
        assert_eq!(backoff(3, None), 4_000);
        assert_eq!(backoff(4, None), 8_000);
        assert_eq!(backoff(100, None), BACKOFF_CAP_MS); // capped, no overflow
        assert_eq!(backoff(0, None), 1_000); // never negative shift
        assert_eq!(backoff(3, Some(250)), 250); // override wins
        assert_eq!(backoff(3, Some(-5)), 0); // override clamped non-negative
    }

    #[test]
    fn state_as_str_roundtrips_and_marks_terminals() {
        for s in [
            TaskState::Queued,
            TaskState::Claimed,
            TaskState::Done,
            TaskState::Failed,
            TaskState::Dead,
        ] {
            assert_eq!(TaskState::from_wire(s.as_str()), s);
        }
        assert!(!TaskState::Queued.is_terminal());
        assert!(!TaskState::Claimed.is_terminal());
        assert!(TaskState::Done.is_terminal());
        assert!(TaskState::Failed.is_terminal());
        assert!(TaskState::Dead.is_terminal());
        assert_eq!(TaskState::from_wire("garbage"), TaskState::Queued);
        assert!(should_dead_letter(5, 5));
        assert!(!should_dead_letter(4, 5));
    }

    // --- enqueue / claim --------------------------------------------------

    #[test]
    fn enqueue_then_claim_marks_claimed_and_mints_fence() {
        let mut wq = q();
        let t = enq(&mut wq, "build", "job-1", 1000);
        assert_eq!(t.state, TaskState::Queued);
        assert_eq!(t.seq, 1);
        assert_eq!(t.attempts, 0);
        assert!(t.fencing_token.is_none());
        assert_eq!(t.available_at, 1000);

        let claim = wq.claim("build", "wkr-A", 5000, 2000).expect("claimable");
        assert_eq!(claim.fencing_token, 1);
        assert_eq!(claim.task.state, TaskState::Claimed);
        assert_eq!(claim.task.attempts, 1);
        assert_eq!(claim.task.claimed_by.as_deref(), Some("wkr-A"));
        assert_eq!(claim.task.fencing_token, Some(1));
        assert_eq!(claim.task.visibility_deadline, Some(7000)); // 2000 + 5000

        // Queue now empty ⇒ next claim is None.
        assert!(wq.claim("build", "wkr-B", 5000, 2100).is_none());
    }

    #[test]
    fn seq_is_global_and_monotonic_across_queues() {
        let mut wq = q();
        let a = enq(&mut wq, "x", "1", 0);
        let b = enq(&mut wq, "y", "2", 0);
        let c = enq(&mut wq, "x", "3", 0);
        assert_eq!((a.seq, b.seq, c.seq), (1, 2, 3));
    }

    #[test]
    fn competing_consumers_claim_each_task_exactly_once() {
        let mut wq = q();
        for i in 0..3 {
            enq(&mut wq, "build", &format!("job-{i}"), 0);
        }
        let mut claimed_ids = Vec::new();
        // Alternate two "workers" pulling from the same queue.
        for round in 0..5 {
            let worker = if round % 2 == 0 { "A" } else { "B" };
            if let Some(c) = wq.claim("build", worker, 1000, 10 + round) {
                claimed_ids.push(c.task.id);
            }
        }
        claimed_ids.sort();
        claimed_ids.dedup();
        assert_eq!(claimed_ids.len(), 3); // each of the 3 tasks claimed once, no double-claim
        assert_eq!(wq.counts("build").claimed, 3);
        assert_eq!(wq.counts("build").queued, 0);
    }

    #[test]
    fn claim_order_is_priority_then_fifo() {
        let mut wq = q();
        wq.enqueue(
            "build",
            "low",
            EnqueueOpts {
                priority: 1,
                ..Default::default()
            },
            0,
        );
        wq.enqueue(
            "build",
            "high-a",
            EnqueueOpts {
                priority: 9,
                ..Default::default()
            },
            0,
        );
        wq.enqueue(
            "build",
            "high-b",
            EnqueueOpts {
                priority: 9,
                ..Default::default()
            },
            0,
        );
        // Highest priority first; ties broken by enqueue order (seq).
        assert_eq!(
            wq.claim("build", "w", 1000, 1).unwrap().task.payload,
            "high-a"
        );
        assert_eq!(
            wq.claim("build", "w", 1000, 2).unwrap().task.payload,
            "high-b"
        );
        assert_eq!(wq.claim("build", "w", 1000, 3).unwrap().task.payload, "low");
    }

    #[test]
    fn depends_on_gates_claim_until_every_dep_is_done() {
        let mut wq = q();
        // A has no deps; B waits on A; C waits on a task id that never exists.
        let a = wq.enqueue("g1", "build-a", EnqueueOpts::default(), 0);
        let b = wq.enqueue(
            "g1",
            "build-b",
            EnqueueOpts {
                depends_on: Some(vec![a.id.clone()]),
                ..Default::default()
            },
            0,
        );
        wq.enqueue(
            "g1",
            "build-c",
            EnqueueOpts {
                depends_on: Some(vec!["no-such-task".into()]),
                ..Default::default()
            },
            0,
        );
        // Only A is claimable: B is blocked by A (still queued), C by a missing dep.
        let first = wq.claim("g1", "w", 1000, 1).unwrap();
        assert_eq!(first.task.id, a.id);
        assert!(wq.claim("g1", "w", 1000, 2).is_none());
        // Finish A → B's dependency is satisfied and B becomes claimable.
        assert!(matches!(
            wq.ack(&a.id, first.fencing_token, None, 3),
            LeaseOutcome::Ok(_)
        ));
        let second = wq.claim("g1", "w", 1000, 4).unwrap();
        assert_eq!(second.task.id, b.id);
        // B's dependsOn round-trips through storage/row mapping.
        assert_eq!(second.task.depends_on.as_deref(), Some(&[a.id][..]));
        // C's missing dependency never resolves — it stays unclaimable.
        assert!(wq.claim("g1", "w", 1000, 5).is_none());
    }

    #[test]
    fn claim_respects_available_at_delay() {
        let mut wq = q();
        wq.enqueue(
            "build",
            "later",
            EnqueueOpts {
                available_at: Some(5000),
                ..Default::default()
            },
            1000,
        );
        assert!(wq.claim("build", "w", 1000, 1000).is_none()); // not yet available
        assert!(wq.claim("build", "w", 1000, 4999).is_none());
        assert!(wq.claim("build", "w", 1000, 5000).is_some()); // now available
    }

    #[test]
    fn claim_lease_falls_back_to_task_visibility_timeout() {
        let mut wq = q();
        wq.enqueue(
            "build",
            "j",
            EnqueueOpts {
                visibility_timeout_ms: 1234,
                ..Default::default()
            },
            0,
        );
        let c = wq.claim("build", "w", 0, 100).unwrap(); // lease_ms <= 0 ⇒ use task default
        assert_eq!(c.task.visibility_deadline, Some(100 + 1234));
    }

    // --- ack / nack / extend ---------------------------------------------

    #[test]
    fn ack_completes_records_result_and_is_idempotent() {
        let mut wq = q();
        enq(&mut wq, "build", "j", 0);
        let c = wq.claim("build", "w", 1000, 10).unwrap();
        let out = wq.ack(&c.task.id, c.fencing_token, Some("artifact://x"), 20);
        match out {
            LeaseOutcome::Ok(t) => {
                assert_eq!(t.state, TaskState::Done);
                assert_eq!(t.result.as_deref(), Some("artifact://x"));
                assert!(t.visibility_deadline.is_none());
            }
            other => panic!("expected Ok, got {other:?}"),
        }
        // Re-ack with the same token = idempotent success (lost-200 retry).
        assert!(matches!(
            wq.ack(&c.task.id, c.fencing_token, None, 30),
            LeaseOutcome::Ok(_)
        ));
        // Wrong token / unknown id.
        assert_eq!(wq.ack(&c.task.id, 999, None, 40), LeaseOutcome::Conflict);
        assert_eq!(wq.ack("nope", 1, None, 40), LeaseOutcome::NotFound);
    }

    #[test]
    fn stale_lease_after_reap_is_fenced_out() {
        let mut wq = q();
        enq(&mut wq, "build", "j", 0);
        let a = wq.claim("build", "A", 100, 1000).unwrap(); // token 1, deadline 1100
                                                            // A stalls; reaper requeues past the deadline.
        let reaped = wq.reap_expired(1200);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].disposition, Disposition::Requeued);
        assert_eq!(reaped[0].task.state, TaskState::Queued);

        // B claims the requeued task → strictly higher token.
        let b = wq.claim("build", "B", 100, 1300).unwrap();
        assert_eq!(b.fencing_token, 2);
        assert_eq!(b.task.attempts, 2);

        // A wakes and tries to ack with its STALE token ⇒ Conflict; B's token wins.
        assert_eq!(
            wq.ack(&a.task.id, a.fencing_token, None, 1500),
            LeaseOutcome::Conflict
        );
        assert!(matches!(
            wq.ack(&b.task.id, b.fencing_token, None, 1500),
            LeaseOutcome::Ok(_)
        ));
    }

    #[test]
    fn nack_requeues_with_backoff_then_dead_letters_when_exhausted() {
        let mut wq = q();
        wq.enqueue(
            "build",
            "j",
            EnqueueOpts {
                max_attempts: 2,
                ..Default::default()
            },
            0,
        );

        let c1 = wq.claim("build", "w", 1000, 100).unwrap(); // attempts 1
        let out1 = wq.nack(
            &c1.task.id,
            c1.fencing_token,
            NackOpts {
                requeue: true,
                error: Some("boom".into()),
                delay_ms: None,
            },
            200,
        );
        match out1 {
            LeaseOutcome::Ok(t) => {
                assert_eq!(t.state, TaskState::Queued);
                assert_eq!(t.error.as_deref(), Some("boom"));
                assert_eq!(t.available_at, 200 + backoff(1, None)); // backoff applied
                assert!(t.claimed_by.is_none());
            }
            other => panic!("expected requeue, got {other:?}"),
        }

        // Re-claim (attempts 2 == max) and nack-requeue ⇒ dead-letter.
        let c2 = wq.claim("build", "w", 1000, 5000).unwrap();
        assert_eq!(c2.task.attempts, 2);
        let out2 = wq.nack(
            &c2.task.id,
            c2.fencing_token,
            NackOpts {
                requeue: true,
                ..Default::default()
            },
            6000,
        );
        match out2 {
            LeaseOutcome::Ok(t) => assert_eq!(t.state, TaskState::Dead),
            other => panic!("expected dead-letter, got {other:?}"),
        }
        assert_eq!(wq.counts("build").dead, 1);
    }

    #[test]
    fn nack_without_requeue_marks_failed() {
        let mut wq = q();
        enq(&mut wq, "build", "j", 0);
        let c = wq.claim("build", "w", 1000, 10).unwrap();
        let out = wq.nack(
            &c.task.id,
            c.fencing_token,
            NackOpts {
                requeue: false,
                error: Some("give up".into()),
                delay_ms: None,
            },
            20,
        );
        match out {
            LeaseOutcome::Ok(t) => {
                assert_eq!(t.state, TaskState::Failed);
                assert_eq!(t.error.as_deref(), Some("give up"));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // A second nack on the now-terminal task is a Conflict (no longer claimed).
        assert_eq!(
            wq.nack(&c.task.id, c.fencing_token, NackOpts::default(), 30),
            LeaseOutcome::Conflict
        );
    }

    #[test]
    fn extend_renews_the_visibility_deadline() {
        let mut wq = q();
        enq(&mut wq, "build", "j", 0);
        let c = wq.claim("build", "w", 1000, 100).unwrap(); // deadline 1100
        match wq.extend(&c.task.id, c.fencing_token, 500, 200) {
            LeaseOutcome::Ok(t) => assert_eq!(t.visibility_deadline, Some(1600)), // 1100 + 500
            other => panic!("expected Ok, got {other:?}"),
        }
        assert_eq!(wq.extend(&c.task.id, 999, 500, 300), LeaseOutcome::Conflict);
        assert_eq!(wq.extend("nope", 1, 500, 300), LeaseOutcome::NotFound);
    }

    // --- reaping ----------------------------------------------------------

    #[test]
    fn reaper_dead_letters_when_no_retries_remain() {
        let mut wq = q();
        wq.enqueue(
            "build",
            "j",
            EnqueueOpts {
                max_attempts: 1,
                ..Default::default()
            },
            0,
        );
        let _c = wq.claim("build", "w", 100, 1000).unwrap(); // attempts 1 == max
        let reaped = wq.reap_expired(2000);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].disposition, Disposition::DeadLettered);
        assert_eq!(reaped[0].task.state, TaskState::Dead);
        // Nothing left claimable.
        assert!(wq.claim("build", "w", 100, 3000).is_none());
    }

    #[test]
    fn reaper_ignores_tasks_within_their_deadline() {
        let mut wq = q();
        enq(&mut wq, "build", "j", 0);
        let c = wq.claim("build", "w", 1000, 100).unwrap(); // deadline 1100
        assert!(wq.reap_expired(1000).is_empty()); // not yet expired
                                                   // Still claimed; an ack with the original token still works.
        assert!(matches!(
            wq.ack(&c.task.id, c.fencing_token, None, 1050),
            LeaseOutcome::Ok(_)
        ));
    }

    #[test]
    fn requeue_worker_recovers_only_that_workers_claims() {
        let mut wq = q();
        enq(&mut wq, "build", "a", 0);
        enq(&mut wq, "build", "b", 0);
        let _a = wq.claim("build", "A", 100_000, 10).unwrap();
        let b = wq.claim("build", "B", 100_000, 11).unwrap();
        let reaped = wq.requeue_worker("A", 20);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].disposition, Disposition::Requeued);
        // B's claim is untouched (still within a long lease).
        assert_eq!(wq.get(&b.task.id).unwrap().state, TaskState::Claimed);
        assert_eq!(wq.counts("build").queued, 1);
        assert_eq!(wq.counts("build").claimed, 1);
    }

    // --- durability + boot recovery --------------------------------------

    #[test]
    fn survives_reopen_and_boot_recovery_keeps_fence_monotonic() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hp-work-test-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let id = {
            let mut wq = WorkQueue::open(&path).unwrap();
            enq(&mut wq, "build", "durable", 0);
            let c = wq.claim("build", "w", 100_000, 10).unwrap();
            assert_eq!(c.fencing_token, 1);
            c.task.id
        }; // drop closes the connection (WAL checkpoint)

        // Reopen: the claimed task is still durably present (the inbox would have lost it).
        let mut wq = WorkQueue::open(&path).unwrap();
        let recovered = wq.get(&id).unwrap();
        assert_eq!(recovered.state, TaskState::Claimed);
        assert_eq!(recovered.fencing_token, Some(1));

        // Boot recovery requeues the (now-dead) worker's in-flight task.
        let reaped = wq.recover_in_flight(1000);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].disposition, Disposition::Requeued);
        assert_eq!(wq.get(&id).unwrap().state, TaskState::Queued);

        // The fence counter resumed from MAX, so the next claim is strictly higher.
        let c2 = wq.claim("build", "w2", 100_000, 2000).unwrap();
        assert_eq!(c2.fencing_token, 2);

        let _ = std::fs::remove_file(&path);
    }

    // --- boot recovery: holder liveness (GUI relaunch with the session daemon on) ---

    fn holder(pid: u32, start: &str) -> Holder {
        Holder {
            pid,
            start: Some(start.to_string()),
        }
    }

    /// The bug: a worker that survived a GUI relaunch had its claim requeued by the new
    /// owner's boot recovery. A live holder's claim must stay Claimed with the SAME fence,
    /// and the surviving worker's ack with that fence must succeed.
    #[test]
    fn boot_recovery_keeps_a_live_holders_claim_and_its_fence() {
        let mut wq = q();
        let t = enq(&mut wq, "build", "x", 0);
        let c = wq
            .claim_held("build", "w", Some(&holder(4242, "boot:1")), 30_000, 10)
            .unwrap();
        let rec =
            wq.recover_in_flight_with(1_000, &|pid, start| pid == 4242 && start == Some("boot:1"));
        assert!(rec.reaped.is_empty(), "a live holder's claim was requeued");
        assert_eq!(rec.kept.len(), 1);
        let kept = wq.get(&t.id).unwrap();
        assert_eq!(kept.state, TaskState::Claimed);
        assert_eq!(kept.fencing_token, Some(c.fencing_token));
        assert_eq!(kept.attempts, 1, "kept, not re-claimed");
        assert!(matches!(
            wq.ack(&t.id, c.fencing_token, Some("ok"), 2_000),
            LeaseOutcome::Ok(_)
        ));
        assert_eq!(wq.get(&t.id).unwrap().state, TaskState::Done);
    }

    /// Crash / full restart / reboot: the holder is dead (or its pid now belongs to some
    /// other process: fingerprint mismatch), so recovery is immediate, exactly as before.
    #[test]
    fn boot_recovery_requeues_dead_recycled_and_holderless_claims_immediately() {
        let mut wq = q();
        let dead = enq(&mut wq, "build", "dead", 0);
        let recycled = enq(&mut wq, "build", "recycled", 0);
        let anon = enq(&mut wq, "build", "anon", 0);
        let live = enq(&mut wq, "build", "live", 0);
        // Long leases, so only recovery (never lease expiry) can move them.
        wq.claim_held("build", "w", Some(&holder(1, "boot:1")), 600_000, 10)
            .unwrap();
        wq.claim_held("build", "w", Some(&holder(2, "boot:OLD")), 600_000, 10)
            .unwrap();
        wq.claim("build", "w", 600_000, 10).unwrap(); // no holder (in-process / MCP)
        wq.claim_held("build", "w", Some(&holder(3, "boot:1")), 600_000, 10)
            .unwrap();
        // pid 1 is gone; pid 2 is alive but started at a different time (recycled /
        // rebooted); pid 3 is the same live process.
        let alive = |pid: u32, start: Option<&str>| match pid {
            2 | 3 => start == Some("boot:1"),
            _ => false,
        };
        let rec = wq.recover_in_flight_with(1_000, &alive);
        let mut reaped: Vec<_> = rec.reaped.iter().map(|r| r.task.id.clone()).collect();
        reaped.sort();
        let mut want = vec![dead.id.clone(), recycled.id.clone(), anon.id.clone()];
        want.sort();
        assert_eq!(reaped, want);
        for id in [&dead.id, &recycled.id, &anon.id] {
            let t = wq.get(id).unwrap();
            assert_eq!(
                t.state,
                TaskState::Queued,
                "dead holder's claim must requeue at boot"
            );
            assert_eq!(
                t.available_at, 1_000,
                "claimable immediately (zero added latency)"
            );
        }
        assert_eq!(wq.get(&live.id).unwrap().state, TaskState::Claimed);
    }

    /// A kept claim gets one full lease from boot even if the app was down longer than its
    /// lease, and the lease is never shortened. This is also the stated worst case for a
    /// claim whose holder only LOOKED alive (a recycled pid without a fingerprint): the
    /// reaper requeues it at max(old deadline, boot + visibility_timeout_ms).
    #[test]
    fn boot_recovery_rearms_a_kept_lease_and_bounds_the_worst_case() {
        let mut wq = q();
        let a = enq(&mut wq, "build", "expired-while-down", 0); // vt = 30s default
        let b = enq(&mut wq, "build", "long-lease", 0);
        wq.claim_held("build", "w", Some(&holder(7, "s")), 5_000, 0)
            .unwrap(); // deadline 5_000
        wq.claim_held("build", "w", Some(&holder(7, "s")), 600_000, 0)
            .unwrap(); // deadline 600_000
        let boot = 100_000; // the app was down well past a's lease
        let rec = wq.recover_in_flight_with(boot, &|_, _| true);
        assert_eq!(rec.kept.len(), 2);
        assert_eq!(
            wq.get(&a.id).unwrap().visibility_deadline,
            Some(boot + DEFAULT_VISIBILITY_MS),
            "an expired kept lease is re-armed to boot + visibility_timeout_ms"
        );
        assert_eq!(
            wq.get(&b.id).unwrap().visibility_deadline,
            Some(600_000),
            "re-arm never shortens a lease"
        );
        // If that holder was in fact dead (pid reused, no fingerprint), nothing heartbeats:
        // the reaper requeues it exactly at the re-armed deadline, not before.
        assert!(wq.reap_expired(boot + DEFAULT_VISIBILITY_MS - 1).is_empty());
        let reaped = wq.reap_expired(boot + DEFAULT_VISIBILITY_MS);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].task.id, a.id);
        assert_eq!(reaped[0].disposition, Disposition::Requeued);
    }

    /// The production probe, end to end: a claim held by THIS (live) process survives boot
    /// recovery; one held by a reaped child process does not.
    #[test]
    fn boot_recovery_with_the_os_probe() {
        let mut wq = q();
        let mine = enq(&mut wq, "build", "mine", 0);
        let gone = enq(&mut wq, "build", "gone", 0);
        let me = holder::identify(std::process::id()).unwrap();
        wq.claim_held("build", "w", Some(&me), 600_000, 10).unwrap();
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit 0"])
                .spawn()
                .unwrap()
        } else {
            std::process::Command::new("true").spawn().unwrap()
        };
        let child_holder = holder::identify(child.id()); // may already be None if it exited
        child.wait().unwrap();
        let dead = child_holder.unwrap_or(Holder {
            pid: child.id(),
            start: None,
        });
        wq.claim_held("build", "w", Some(&dead), 600_000, 10)
            .unwrap();
        let reaped = wq.recover_in_flight(1_000);
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0].task.id, gone.id);
        assert_eq!(wq.get(&mine.id).unwrap().state, TaskState::Claimed);
    }

    /// A re-claim overwrites the holder: a requeued task claimed again WITHOUT a holder
    /// must not inherit the previous claim's (still live) holder.
    #[test]
    fn a_reclaim_never_inherits_the_previous_holder() {
        let mut wq = q();
        let t = enq(&mut wq, "build", "x", 0);
        let c = wq
            .claim_held("build", "w1", Some(&holder(9, "s")), 600_000, 0)
            .unwrap();
        wq.nack(
            &t.id,
            c.fencing_token,
            NackOpts {
                requeue: true,
                delay_ms: Some(0),
                ..Default::default()
            },
            1,
        );
        wq.claim("build", "w2", 600_000, 2).unwrap(); // holder-less re-claim
        let rec = wq.recover_in_flight_with(3, &|_, _| true);
        assert_eq!(rec.reaped.len(), 1, "a holder-less claim requeues at boot");
        assert!(rec.kept.is_empty());
    }

    /// A v2 DB (pre-holder) migrates to v3; its in-flight claims have no holder, so the
    /// first boot on the new schema requeues them, as the old code would have.
    #[test]
    fn migrates_a_v2_database_and_requeues_its_claims_at_boot() {
        let path =
            std::env::temp_dir().join(format!("hp-work-migrate-v2-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (v INTEGER NOT NULL);
                 CREATE TABLE tasks (
                     id TEXT PRIMARY KEY, queue TEXT NOT NULL, seq INTEGER NOT NULL UNIQUE,
                     kind TEXT NOT NULL, title TEXT NOT NULL, state TEXT NOT NULL,
                     payload TEXT NOT NULL, priority INTEGER NOT NULL, attempts INTEGER NOT NULL,
                     max_attempts INTEGER NOT NULL, visibility_timeout_ms INTEGER NOT NULL,
                     available_at INTEGER NOT NULL, claimed_by TEXT, fencing_token INTEGER,
                     visibility_deadline INTEGER, result TEXT, error TEXT, dedupe_key TEXT,
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                     goal_id TEXT, depends_on TEXT);
                 INSERT INTO schema_version (v) VALUES (2);
                 INSERT INTO tasks (id, queue, seq, kind, title, state, payload, priority,
                     attempts, max_attempts, visibility_timeout_ms, available_at, claimed_by,
                     fencing_token, visibility_deadline, created_at, updated_at)
                 VALUES ('c1','g1',1,'manual','','claimed','p',0,1,5,30000,0,'w',3,9999999,0,0);",
            )
            .unwrap();
        }
        let mut wq = WorkQueue::open(&path).unwrap();
        let v: i64 = wq
            .conn
            .query_row("SELECT MAX(v) FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let reaped = wq.recover_in_flight(1_000);
        assert_eq!(reaped.len(), 1);
        assert_eq!(wq.get("c1").unwrap().state, TaskState::Queued);
        assert_eq!(wq.get("c1").unwrap().fencing_token, Some(3));
        // ...and the holder columns work post-migration.
        let me = holder::identify(std::process::id()).unwrap();
        let c = wq.claim_held("g1", "w", Some(&me), 600_000, 2_000).unwrap();
        assert_eq!(
            c.fencing_token, 4,
            "fence resumes from MAX across the migration"
        );
        assert!(wq.recover_in_flight(3_000).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn opens_and_migrates_a_v1_database_preserving_rows() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("hp-work-migrate-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Hand-build a v1 DB: the pre-DAG schema (no goal_id/depends_on) + a live row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (v INTEGER NOT NULL);
                 CREATE TABLE tasks (
                     id TEXT PRIMARY KEY, queue TEXT NOT NULL, seq INTEGER NOT NULL UNIQUE,
                     kind TEXT NOT NULL, title TEXT NOT NULL, state TEXT NOT NULL,
                     payload TEXT NOT NULL, priority INTEGER NOT NULL, attempts INTEGER NOT NULL,
                     max_attempts INTEGER NOT NULL, visibility_timeout_ms INTEGER NOT NULL,
                     available_at INTEGER NOT NULL, claimed_by TEXT, fencing_token INTEGER,
                     visibility_deadline INTEGER, result TEXT, error TEXT, dedupe_key TEXT,
                     created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 INSERT INTO schema_version (v) VALUES (1);
                 INSERT INTO tasks (id, queue, seq, kind, title, state, payload, priority,
                     attempts, max_attempts, visibility_timeout_ms, available_at, created_at,
                     updated_at)
                 VALUES ('old1','g1',1,'manual','','queued','p',0,0,5,30000,0,0,0);",
            )
            .unwrap();
        }

        // Opening with current code migrates v1 -> v2 (ADD COLUMN) without losing the row.
        let mut wq = WorkQueue::open(&path).unwrap();
        let old = wq.get("old1").expect("v1 row survived migration");
        assert_eq!(old.state, TaskState::Queued);
        assert_eq!(old.depends_on, None); // new column reads as NULL for the old row

        // The DAG columns are usable post-migration: a new task can depend on the old one.
        let b = wq.enqueue(
            "g1",
            "needs-old1",
            EnqueueOpts {
                depends_on: Some(vec!["old1".into()]),
                ..Default::default()
            },
            0,
        );
        // old1 (no deps) is claimable; B is gated until old1 is done.
        let first = wq.claim("g1", "w", 1000, 1).unwrap();
        assert_eq!(first.task.id, "old1");
        assert!(wq.claim("g1", "w", 1000, 2).is_none());
        assert!(matches!(
            wq.ack("old1", first.fencing_token, None, 3),
            LeaseOutcome::Ok(_)
        ));
        assert_eq!(wq.claim("g1", "w", 1000, 4).unwrap().task.id, b.id);

        let _ = std::fs::remove_file(&path);
    }

    // --- dedupe -----------------------------------------------------------

    #[test]
    fn dedupe_key_collapses_live_enqueues_and_frees_on_terminal() {
        let mut wq = q();
        let first = wq.enqueue(
            "build",
            "v1",
            EnqueueOpts {
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            0,
        );
        // Second enqueue with the same key while live ⇒ returns the SAME task, no insert.
        let dup = wq.enqueue(
            "build",
            "v2-ignored",
            EnqueueOpts {
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            1,
        );
        assert_eq!(dup.id, first.id);
        assert_eq!(dup.payload, "v1");
        assert_eq!(wq.counts("build").total(), 1);

        // Drive the original to terminal; the key frees up.
        let c = wq.claim("build", "w", 1000, 10).unwrap();
        wq.ack(&c.task.id, c.fencing_token, None, 20);
        let third = wq.enqueue(
            "build",
            "v3",
            EnqueueOpts {
                dedupe_key: Some("k".into()),
                ..Default::default()
            },
            30,
        );
        assert_ne!(third.id, first.id); // a fresh task now that the key is free
        assert_eq!(third.payload, "v3");
    }

    // --- reads ------------------------------------------------------------

    #[test]
    fn list_is_a_seq_cursor_with_optional_state_filter() {
        let mut wq = q();
        let a = enq(&mut wq, "build", "a", 0);
        let _b = enq(&mut wq, "build", "b", 0);
        enq(&mut wq, "other", "x", 0); // a different queue is excluded

        let all = wq.list("build", ListFilter::default(), 0, 100);
        assert_eq!(
            all.iter().map(|t| t.payload.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );

        // Cursor: only tasks after a.seq.
        let after = wq.list("build", ListFilter::default(), a.seq, 100);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].payload, "b");

        // State filter: claim one, then filter by state.
        wq.claim("build", "w", 1000, 1);
        let claimed = wq.list(
            "build",
            ListFilter {
                state: Some(TaskState::Claimed),
            },
            0,
            100,
        );
        assert_eq!(claimed.len(), 1);
        let queued = wq.list(
            "build",
            ListFilter {
                state: Some(TaskState::Queued),
            },
            0,
            100,
        );
        assert_eq!(queued.len(), 1);
    }

    #[test]
    fn counts_and_queues_reflect_state_distribution() {
        let mut wq = q();
        enq(&mut wq, "build", "a", 0);
        enq(&mut wq, "build", "b", 0);
        enq(&mut wq, "deploy", "c", 0);
        let c = wq.claim("build", "w", 1000, 1).unwrap();
        wq.ack(&c.task.id, c.fencing_token, None, 2);

        let bc = wq.counts("build");
        assert_eq!(bc.done, 1);
        assert_eq!(bc.queued, 1);
        assert_eq!(bc.total(), 2);

        let qs = wq.queues();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0].queue, "build"); // BTreeMap ⇒ sorted
        assert_eq!(qs[1].queue, "deploy");
        assert_eq!(qs[1].counts.queued, 1);
    }

    #[test]
    fn purge_removes_only_old_terminal_tasks() {
        let mut wq = q();
        // A low-priority keeper that is never claimed (so it stays Queued); the two
        // high-priority tasks are claimed first and driven to Done at different times.
        enq(&mut wq, "build", "stays-queued", 0); // priority 0
        wq.enqueue(
            "build",
            "old",
            EnqueueOpts {
                priority: 5,
                ..Default::default()
            },
            0,
        );
        wq.enqueue(
            "build",
            "new",
            EnqueueOpts {
                priority: 5,
                ..Default::default()
            },
            0,
        );

        let c1 = wq.claim("build", "w", 1000, 1).unwrap();
        assert_eq!(c1.task.payload, "old"); // higher priority claimed before the keeper
        wq.ack(&c1.task.id, c1.fencing_token, None, 100); // done @ updated_at 100
        let c2 = wq.claim("build", "w", 1000, 1).unwrap();
        assert_eq!(c2.task.payload, "new");
        wq.ack(&c2.task.id, c2.fencing_token, None, 9_000); // done @ updated_at 9000

        // Purge terminal tasks updated at/under 500: removes only the first done task.
        let removed = wq.purge("build", 500);
        assert_eq!(removed, 1);
        assert_eq!(wq.counts("build").done, 1); // the @9000 one remains
        assert_eq!(wq.counts("build").queued, 1); // the keeper is never purged
    }

    #[test]
    fn get_returns_none_for_missing() {
        let wq = q();
        assert!(wq.get("ghost").is_none());
    }

    #[test]
    fn task_serializes_camelcase_and_omits_unset_fields() {
        let mut wq = q();
        let t = enq(&mut wq, "build", "j", 0);
        let json = serde_json::to_string(&t).unwrap();
        assert!(json.contains(r#""maxAttempts":5"#));
        assert!(json.contains(r#""availableAt":0"#));
        assert!(json.contains(r#""state":"queued""#));
        // Unset optionals are omitted (skip_serializing_if), like the inbox/lock structs.
        assert!(!json.contains("claimedBy"));
        assert!(!json.contains("fencingToken"));
        assert!(!json.contains("\"result\""));
    }
}
