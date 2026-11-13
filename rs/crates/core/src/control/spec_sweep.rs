//! Spec-pane sweeper — closes idle or finished goal spec-agent panes, resumably.
//!
//! A goal's spec agent is an interactive `claude` TUI that sits idle forever once its goal is
//! over (or its orchestrator forgot it), holding a pty, a process tree and a lot of RSS. This
//! module decides, once a minute, which of those panes are safe to close, writes a handover
//! record + resume command, tells the parent, and closes the pane.
//!
//! Layout (same split as `control::nudge`):
//!   • [`decide`] — PURE policy: one observation in, [`Verdict`] out. Every skip rule is its
//!     own [`SkipReason`]; nothing closes while any applies.
//!   • [`SweepLedger`] — small in-memory bookkeeping fed by the inbox post handler (reports,
//!     inbound timestamps, open consults) plus the close-hysteresis state.
//!   • [`SweepEnv`] — everything impure (panes, peers, /proc, work queue, screen) behind a
//!     trait so [`sweep_pass`] is testable with fakes; [`LiveEnv`] is the real one.
//!   • Close artifacts ([`write_record`], [`append_log`]) take the state dir explicitly.
//!
//! The policy is deliberately conservative: when a signal cannot be read (non-Linux process
//! scan, missing peer record, unreadable screen) the pane is KEPT.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use crate::claude_panes::{valid_config_dir, valid_resume_cwd, valid_session_id};
use crate::claude_peers::PeerStatus;
use crate::control::server::{notify_state, now_ms, Shared};
use crate::persistence::control_settings::SpecSweepSettings;
use crate::session::screen::InputLineKind;

/// How often the ticker sweeps.
pub const SWEEP_TICK_MS: u64 = 60_000;
/// A close verdict must have stood on a previous sweep at least this long ago (and every sweep
/// since must have said close too).
pub const HYSTERESIS_MS: i64 = 60_000;
/// Descendant processes started within this long of the claude process are launch-time helpers
/// (MCP servers etc.), not work in flight.
pub const DESCENDANT_GRACE_MS: i64 = 60_000;
/// Pane meta keys the policy reads.
pub const META_GOAL_STATE: &str = "hp.goalState";
pub const META_IDLE_CLOSE: &str = "hp.idleClose";
pub const META_REPORT: &str = "hp.report";
pub const META_QUEUE: &str = "hp.queue";
/// How many reports / screen lines go into a close record.
const RECORD_REPORTS: usize = 10;
const RECORD_SCREEN_LINES: usize = 60;
/// Cap per-pane open-consult bookkeeping.
const MAX_CONSULTS: usize = 64;

// ---- settings ----------------------------------------------------------------------------

/// Effective sweeper configuration (settings + env override).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepCfg {
    pub enabled: bool,
    pub idle_close_ms: i64,
    pub done_close_ms: i64,
    pub dry_run: bool,
}

impl Default for SweepCfg {
    fn default() -> Self {
        Self::from_settings(&SpecSweepSettings::default(), None)
    }
}

impl SweepCfg {
    /// `env` is the raw `HYPERPANES_SPEC_SWEEP` value: `0`/`false`/`off` disables.
    pub fn from_settings(s: &SpecSweepSettings, env: Option<&str>) -> Self {
        let env_off = matches!(env, Some("0") | Some("false") | Some("off"));
        Self {
            enabled: s.enabled && !env_off,
            idle_close_ms: s.idle_close_hours as i64 * 3_600_000,
            done_close_ms: s.done_close_minutes as i64 * 60_000,
            dry_run: s.dry_run,
        }
    }

    /// Load from `control-settings.json` + the environment.
    pub fn load() -> Self {
        let st = crate::persistence::control_settings::load()
            .spec_sweep
            .unwrap_or_default();
        Self::from_settings(&st, std::env::var("HYPERPANES_SPEC_SWEEP").ok().as_deref())
    }
}

// ---- observation + policy ------------------------------------------------------------------

/// A spec agent's report kind (first word of its inbox message to the parent).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportKind {
    Progress,
    NeedsDecision,
    Blocked,
    Done,
    Failed,
}

impl ReportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ReportKind::Progress => "progress",
            ReportKind::NeedsDecision => "needs-decision",
            ReportKind::Blocked => "blocked",
            ReportKind::Done => "done",
            ReportKind::Failed => "failed",
        }
    }
}

/// Parse the report kind from a message body: its first word, case-insensitive, with trailing
/// `:`/`,`/`.` stripped. `None` for anything that is not a report.
pub fn parse_report_kind(body: &str) -> Option<ReportKind> {
    let word = body
        .split_whitespace()
        .next()?
        .trim_end_matches([':', ',', '.', ';'])
        .to_ascii_lowercase();
    Some(match word.as_str() {
        "progress" => ReportKind::Progress,
        "needs-decision" => ReportKind::NeedsDecision,
        "blocked" => ReportKind::Blocked,
        "done" => ReportKind::Done,
        "failed" => ReportKind::Failed,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub kind: ReportKind,
    pub ts: i64,
}

/// `hp.idleClose` meta: absent, `off`, or an hours override.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IdleOverride {
    Default,
    Off,
    Hours(f64),
}

impl IdleOverride {
    pub fn from_meta(meta: &BTreeMap<String, String>) -> Self {
        match meta.get(META_IDLE_CLOSE).map(|s| s.trim()) {
            None | Some("") => IdleOverride::Default,
            Some(v) => {
                let lower = v.to_ascii_lowercase();
                if matches!(lower.as_str(), "off" | "false" | "0" | "no" | "never") {
                    IdleOverride::Off
                } else {
                    match v.parse::<f64>() {
                        Ok(h) if h.is_finite() && h > 0.0 => IdleOverride::Hours(h),
                        _ => IdleOverride::Default,
                    }
                }
            }
        }
    }
}

/// What a spec pane's `hp.report` meta points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportFile {
    /// `hp.report` unset.
    NotSet,
    /// Set and the file exists and is non-empty.
    Ready,
    /// Set but the file is missing or empty.
    Missing,
}

/// Live descendant processes of the claude process (beyond launch-time helpers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descendants {
    None,
    Some,
    /// Could not be determined (non-Linux, /proc unreadable): treated as "has child".
    Unknown,
}

/// The peer-registry view of a pane's session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerObs {
    pub status: PeerStatus,
    pub status_updated_at: Option<i64>,
    pub pid: u32,
    pub started_at: Option<i64>,
}

/// The pane's live Claude session marker.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: String,
    pub config_dir: String,
}

/// Everything [`decide`] needs about one spec pane.
#[derive(Debug, Clone)]
pub struct SpecPaneObs {
    pub pane_id: String,
    pub goal: String,
    pub parent: Option<String>,
    pub project: Option<String>,
    pub meta_done: bool,
    pub idle_override: IdleOverride,
    pub busy: bool,
    pub last_output_at: Option<i64>,
    pub peer: Option<PeerObs>,
    /// A selection dialog is showing, or the prompt box holds a typed draft.
    pub asking: bool,
    pub last_report: Option<Report>,
    /// Newest message addressed TO the pane.
    pub last_inbound_ts: Option<i64>,
    /// Newest message to OR from the pane.
    pub last_message_ts: Option<i64>,
    pub claimed_tasks: usize,
    pub worker_children: usize,
    pub descendants: Descendants,
    pub pending_nudges: usize,
    pub report_file: ReportFile,
    pub rss_kb: Option<u64>,
    pub session: Option<SessionInfo>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    IdleCloseOff,
    Busy,
    NoPeer,
    PeerNotIdle,
    Asking,
    UnknownIdle,
    NotIdleLongEnough,
    OpenQuestion,
    ClaimedTask,
    WorkerRunning,
    LiveDescendant,
    PendingNudge,
    ReportMissing,
    Disabled,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::IdleCloseOff => "idle-close-off",
            SkipReason::Busy => "busy",
            SkipReason::NoPeer => "no-peer",
            SkipReason::PeerNotIdle => "peer-not-idle",
            SkipReason::Asking => "asking",
            SkipReason::UnknownIdle => "unknown-idle",
            SkipReason::NotIdleLongEnough => "not-idle-long-enough",
            SkipReason::OpenQuestion => "open-question",
            SkipReason::ClaimedTask => "claimed-task",
            SkipReason::WorkerRunning => "worker-running",
            SkipReason::LiveDescendant => "live-descendant",
            SkipReason::PendingNudge => "pending-nudge",
            SkipReason::ReportMissing => "report-missing",
            SkipReason::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Goal reported done (or `hp.goalState=done`) and the pane idled past the done grace.
    GoalDone,
    /// Idle past the idle threshold with nothing in flight.
    Idle,
}

impl CloseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::GoalDone => "goal-done",
            CloseReason::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Keep(SkipReason),
    Close(CloseReason),
}

impl Verdict {
    pub fn describe(&self) -> String {
        match self {
            Verdict::Keep(r) => format!("keep:{}", r.as_str()),
            Verdict::Close(r) => format!("close:{}", r.as_str()),
        }
    }
}

/// `idle_since` = newest of last pty output, peer status change (when the peer is idle) and
/// the last inbox message to/from the pane. `None` when nothing is known.
pub fn idle_since(o: &SpecPaneObs) -> Option<i64> {
    let peer_ts = o
        .peer
        .filter(|p| p.status.is_idle())
        .and_then(|p| p.status_updated_at);
    [o.last_output_at, peer_ts, o.last_message_ts]
        .into_iter()
        .flatten()
        .max()
}

/// The pure close policy. See the module docs; every `Keep` is its own reason.
pub fn decide(o: &SpecPaneObs, cfg: &SweepCfg, now: i64) -> Verdict {
    if !cfg.enabled {
        return Verdict::Keep(SkipReason::Disabled);
    }
    if o.idle_override == IdleOverride::Off {
        return Verdict::Keep(SkipReason::IdleCloseOff);
    }
    if o.busy {
        return Verdict::Keep(SkipReason::Busy);
    }
    let Some(peer) = o.peer else {
        return Verdict::Keep(SkipReason::NoPeer);
    };
    if !peer.status.is_idle() {
        return Verdict::Keep(SkipReason::PeerNotIdle);
    }
    if o.asking {
        return Verdict::Keep(SkipReason::Asking);
    }
    let Some(since) = idle_since(o) else {
        return Verdict::Keep(SkipReason::UnknownIdle);
    };
    let idle = now - since;
    let done = o.meta_done || o.last_report.is_some_and(|r| r.kind == ReportKind::Done);
    let idle_limit = match o.idle_override {
        IdleOverride::Hours(h) => (h * 3_600_000.0) as i64,
        _ => cfg.idle_close_ms,
    };
    let reason = if done && idle >= cfg.done_close_ms {
        CloseReason::GoalDone
    } else if idle >= idle_limit {
        CloseReason::Idle
    } else {
        return Verdict::Keep(SkipReason::NotIdleLongEnough);
    };
    // Trigger B only: an unanswered needs-decision / blocked report means the parent still owes
    // this pane an answer.
    if reason == CloseReason::Idle {
        if let Some(r) = o.last_report {
            let open = matches!(r.kind, ReportKind::NeedsDecision | ReportKind::Blocked);
            let answered = o.last_inbound_ts.is_some_and(|t| t > r.ts);
            if open && !answered {
                return Verdict::Keep(SkipReason::OpenQuestion);
            }
        }
    }
    if o.claimed_tasks > 0 {
        return Verdict::Keep(SkipReason::ClaimedTask);
    }
    if o.worker_children > 0 {
        return Verdict::Keep(SkipReason::WorkerRunning);
    }
    if o.descendants != Descendants::None {
        return Verdict::Keep(SkipReason::LiveDescendant);
    }
    if o.pending_nudges > 0 {
        return Verdict::Keep(SkipReason::PendingNudge);
    }
    if o.report_file == ReportFile::Missing {
        return Verdict::Keep(SkipReason::ReportMissing);
    }
    Verdict::Close(reason)
}

/// Whether the screen shows the agent waiting on a human: a selection dialog, or a typed
/// (non-ghost) draft in the prompt box. `kind` is the structured prompt-box read when the
/// screen has one; without it any text after the last `❯` is treated as a draft.
pub fn screen_asking(text: &str, input: Option<(InputLineKind, &str)>) -> bool {
    const MARKERS: &[&str] = &["Enter to select", "↑/↓", "Esc to cancel", "Do you want"];
    let lines: Vec<&str> = text.lines().collect();
    let tail = &lines[lines.len().saturating_sub(30)..];
    if tail.iter().any(|l| MARKERS.iter().any(|m| l.contains(m))) {
        return true;
    }
    match input {
        Some((InputLineKind::Typed, _)) => true,
        Some((InputLineKind::Empty | InputLineKind::Suggestion, _)) => false,
        None => tail.iter().rev().any(|l| {
            let t = l.trim().trim_start_matches(['│', '|']).trim_start();
            t.strip_prefix('❯')
                .is_some_and(|rest| !rest.trim().trim_end_matches(['│', '|']).trim().is_empty())
        }),
    }
}

// ---- ledger --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenConsult {
    pub from: String,
    pub seq: u64,
    pub ts: i64,
}

#[derive(Debug, Default, Clone)]
struct PaneRec {
    /// Last report this pane (role=spec) sent anyone.
    last_report: Option<Report>,
    /// Newest message addressed to the pane.
    last_inbound_ts: Option<i64>,
    /// Newest message the pane sent.
    last_sent_ts: Option<i64>,
    /// First sweep ts of the current unbroken run of close verdicts.
    pending_close_since: Option<i64>,
    /// A dry-run `wouldClose` was already logged for the current run.
    dry_logged: bool,
    /// needs-decision messages addressed TO this pane (from spec panes).
    consults_in: Vec<OpenConsult>,
    /// Highest seq this pane has sent to each recipient.
    last_sent_seq_to: HashMap<String, u64>,
}

/// Per-pane sweeper bookkeeping for the whole server. Pure; callers supply timestamps.
#[derive(Debug, Default)]
pub struct SweepLedger {
    panes: HashMap<String, PaneRec>,
}

impl SweepLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one inbox post. `from_pane` is the sender's canonical pane id when the sender is
    /// a pane; `from_is_spec` says its meta role is `spec`.
    pub fn record_post(
        &mut self,
        to: &str,
        from_pane: Option<&str>,
        from_is_spec: bool,
        seq: u64,
        ts: i64,
        body: &str,
    ) {
        let kind = parse_report_kind(body);
        {
            let t = self.panes.entry(to.to_string()).or_default();
            t.last_inbound_ts = Some(t.last_inbound_ts.map_or(ts, |x| x.max(ts)));
            if from_is_spec && kind == Some(ReportKind::NeedsDecision) {
                if let Some(f) = from_pane {
                    t.consults_in.push(OpenConsult {
                        from: f.to_string(),
                        seq,
                        ts,
                    });
                    if t.consults_in.len() > MAX_CONSULTS {
                        t.consults_in.remove(0);
                    }
                }
            }
        }
        if let Some(f) = from_pane {
            let s = self.panes.entry(f.to_string()).or_default();
            s.last_sent_ts = Some(s.last_sent_ts.map_or(ts, |x| x.max(ts)));
            let e = s.last_sent_seq_to.entry(to.to_string()).or_insert(0);
            *e = (*e).max(seq);
            if from_is_spec {
                if let Some(k) = kind {
                    if s.last_report.is_none_or(|r| ts >= r.ts) {
                        s.last_report = Some(Report { kind: k, ts });
                    }
                }
            }
        }
    }

    pub fn last_report(&self, pane: &str) -> Option<Report> {
        self.panes.get(pane).and_then(|p| p.last_report)
    }

    pub fn last_inbound_ts(&self, pane: &str) -> Option<i64> {
        self.panes.get(pane).and_then(|p| p.last_inbound_ts)
    }

    pub fn last_sent_ts(&self, pane: &str) -> Option<i64> {
        self.panes.get(pane).and_then(|p| p.last_sent_ts)
    }

    /// needs-decision messages addressed TO `pane_id` by a spec pane that `pane_id` has not
    /// replied to since (no later message from `pane_id` to that sender).
    pub fn open_consults(&self, pane_id: &str) -> Vec<OpenConsult> {
        let Some(rec) = self.panes.get(pane_id) else {
            return Vec::new();
        };
        rec.consults_in
            .iter()
            .filter(|c| {
                rec.last_sent_seq_to
                    .get(&c.from)
                    .is_none_or(|&replied| replied <= c.seq)
            })
            .cloned()
            .collect()
    }

    /// Hysteresis: feed this sweep's verdict. Returns `true` when the pane may be closed NOW:
    /// a close verdict stood on an earlier sweep at least [`HYSTERESIS_MS`] ago and nothing
    /// but close verdicts came in between. Any keep resets.
    pub fn note_verdict(&mut self, pane: &str, close: bool, now: i64) -> bool {
        let r = self.panes.entry(pane.to_string()).or_default();
        if !close {
            r.pending_close_since = None;
            r.dry_logged = false;
            return false;
        }
        match r.pending_close_since {
            None => {
                r.pending_close_since = Some(now);
                false
            }
            Some(first) => now - first >= HYSTERESIS_MS,
        }
    }

    /// Dry-run helper: returns true once per run of close verdicts.
    pub fn take_dry_log(&mut self, pane: &str) -> bool {
        let r = self.panes.entry(pane.to_string()).or_default();
        !std::mem::replace(&mut r.dry_logged, true)
    }

    pub fn drop_pane(&mut self, pane: &str) {
        self.panes.remove(pane);
    }

    /// Forget panes that are gone, so ids don't leak.
    pub fn retain_panes(&mut self, live: &std::collections::HashSet<String>) {
        self.panes.retain(|id, _| live.contains(id));
    }
}

// ---- environment ---------------------------------------------------------------------------

/// A spec pane as the model lists it.
#[derive(Debug, Clone)]
pub struct SpecPane {
    pub pane_id: String,
    pub uid: String,
    pub meta: BTreeMap<String, String>,
    pub cwd: Option<String>,
}

/// Screen read reduced to what the policy and the record need.
#[derive(Debug, Clone)]
pub struct ScreenObs {
    pub text: String,
    pub input: Option<(InputLineKind, String)>,
}

/// Every impure input of a sweep.
pub trait SweepEnv {
    fn now_ms(&self) -> i64;
    fn spec_panes(&self) -> Vec<SpecPane>;
    /// Activity heuristic says mid-turn (or anything but quiet).
    fn is_busy(&self, p: &SpecPane) -> bool;
    fn last_output_at(&self, p: &SpecPane) -> Option<i64>;
    fn session(&self, p: &SpecPane) -> Option<SessionInfo>;
    fn peer(&self, session_id: &str) -> Option<PeerObs>;
    /// Claimed tasks across the named queues.
    fn claimed_tasks(&self, queues: &[String]) -> usize;
    /// Live, non-exited panes whose `meta.parent` is `pane_id` and role is worker.
    fn worker_children(&self, pane_id: &str) -> usize;
    fn descendants(&self, pid: u32, grace_ms: i64) -> Descendants;
    fn screen(&self, p: &SpecPane) -> Option<ScreenObs>;
    fn rss_kb(&self, pid: u32) -> Option<u64>;
    fn pending_nudges(&self, pane_id: &str) -> usize;
    fn report_file(&self, path: &str) -> ReportFile;
    /// Newest inbox message addressed to the pane (durable inbox, survives restarts).
    fn inbox_last_to(&self, pane_id: &str) -> Option<i64>;
    /// `(body, ts)` of messages `from_pane` posted into `parent`'s inbox, oldest first.
    fn reports_from(&self, parent: &str, from_pane: &str) -> Vec<(String, i64)>;
}

/// Candidate goal-queue names for a pane.
pub fn queue_names(meta: &BTreeMap<String, String>, goal: &str) -> Vec<String> {
    if let Some(q) = meta.get(META_QUEUE).filter(|q| !q.trim().is_empty()) {
        return vec![q.trim().to_string()];
    }
    if goal.is_empty() {
        return Vec::new();
    }
    let mut out = vec![goal.to_string()];
    if let Some(base) = meta
        .get("project")
        .and_then(|p| Path::new(p.trim_end_matches('/')).file_name())
        .and_then(|b| b.to_str())
        .filter(|b| !b.is_empty())
    {
        out.push(format!("{base}-{goal}"));
    }
    out
}

/// Gather one pane's observation.
pub fn observe(env: &dyn SweepEnv, ledger: &SweepLedger, p: &SpecPane) -> SpecPaneObs {
    let meta = &p.meta;
    let goal = meta.get("goal").cloned().unwrap_or_default();
    let parent = meta.get("parent").filter(|s| !s.is_empty()).cloned();
    let session = env.session(p);
    let peer = session
        .as_ref()
        .filter(|s| !s.session_id.is_empty())
        .and_then(|s| env.peer(&s.session_id));
    let screen = env.screen(p);
    // A pane whose screen cannot be read cannot be shown NOT to be asking: keep it.
    let asking = screen
        .as_ref()
        .is_none_or(|s| screen_asking(&s.text, s.input.as_ref().map(|(k, t)| (*k, t.as_str()))));

    // Reports: merge what the ledger saw live with what the parent's durable inbox holds.
    let mut last_report = ledger.last_report(&p.pane_id);
    let mut last_sent = ledger.last_sent_ts(&p.pane_id);
    if let Some(par) = &parent {
        for (body, ts) in env.reports_from(par, &p.pane_id) {
            last_sent = Some(last_sent.map_or(ts, |x| x.max(ts)));
            if let Some(kind) = parse_report_kind(&body) {
                if last_report.is_none_or(|r| ts >= r.ts) {
                    last_report = Some(Report { kind, ts });
                }
            }
        }
    }
    let inbound = match (
        ledger.last_inbound_ts(&p.pane_id),
        env.inbox_last_to(&p.pane_id),
    ) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    let last_message_ts = [inbound, last_sent].into_iter().flatten().max();

    let descendants = match peer {
        Some(pe) => env.descendants(pe.pid, DESCENDANT_GRACE_MS),
        None => Descendants::Unknown,
    };
    let report_file = match meta
        .get(META_REPORT)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        None => ReportFile::NotSet,
        Some(path) => env.report_file(path),
    };
    SpecPaneObs {
        pane_id: p.pane_id.clone(),
        goal: goal.clone(),
        parent,
        project: meta.get("project").cloned(),
        meta_done: meta
            .get(META_GOAL_STATE)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("done")),
        idle_override: IdleOverride::from_meta(meta),
        busy: env.is_busy(p),
        last_output_at: env.last_output_at(p),
        peer,
        asking,
        last_report,
        last_inbound_ts: inbound,
        last_message_ts,
        claimed_tasks: env.claimed_tasks(&queue_names(meta, &goal)),
        worker_children: env.worker_children(&p.pane_id),
        descendants,
        pending_nudges: env.pending_nudges(&p.pane_id),
        report_file,
        rss_kb: peer.and_then(|pe| env.rss_kb(pe.pid)),
        session,
        cwd: p.cwd.clone(),
    }
}

// ---- sweep pass ----------------------------------------------------------------------------

/// What a sweep decided for one pane after hysteresis.
#[derive(Debug, Clone)]
pub struct Action {
    pub obs: SpecPaneObs,
    pub reason: CloseReason,
    pub idle_ms: i64,
    /// Dry-run: log only (and only once per run).
    pub dry_run: bool,
}

/// One pass over every spec pane: observe, decide, apply hysteresis. Pure over `env`.
pub fn sweep_pass(env: &dyn SweepEnv, ledger: &mut SweepLedger, cfg: &SweepCfg) -> Vec<Action> {
    let now = env.now_ms();
    let panes = env.spec_panes();
    let live: std::collections::HashSet<String> = panes.iter().map(|p| p.pane_id.clone()).collect();
    ledger.retain_panes(&live);
    let mut out = Vec::new();
    for p in &panes {
        let obs = observe(env, ledger, p);
        let verdict = decide(&obs, cfg, now);
        let close = matches!(verdict, Verdict::Close(_));
        let ripe = ledger.note_verdict(&p.pane_id, close, now);
        let Verdict::Close(reason) = verdict else {
            continue;
        };
        if !ripe {
            continue;
        }
        if cfg.dry_run && !ledger.take_dry_log(&p.pane_id) {
            continue;
        }
        let idle_ms = idle_since(&obs).map_or(0, |s| now - s);
        out.push(Action {
            obs,
            reason,
            idle_ms,
            dry_run: cfg.dry_run,
        });
    }
    out
}

// ---- close artifacts -----------------------------------------------------------------------

/// `cd '<cwd>' && CLAUDE_CONFIG_DIR='<dir>' claude --resume <sid>`; `None` when the id or cwd
/// fail their shell-safety gates. The config-dir prefix is omitted when empty or unsafe.
pub fn resume_command(s: &SessionInfo, fallback_cwd: Option<&str>) -> Option<String> {
    let cwd = if s.cwd.is_empty() {
        fallback_cwd.unwrap_or("")
    } else {
        &s.cwd
    };
    if !valid_session_id(&s.session_id) || !valid_resume_cwd(cwd) {
        return None;
    }
    let cfg = if !s.config_dir.is_empty() && valid_config_dir(&s.config_dir) {
        format!("CLAUDE_CONFIG_DIR='{}' ", s.config_dir)
    } else {
        String::new()
    };
    Some(format!(
        "cd '{cwd}' && {cfg}claude --resume {}",
        s.session_id
    ))
}

/// Everything written about one close.
#[derive(Debug, Clone)]
pub struct CloseRecord {
    pub goal: String,
    pub reason: String,
    pub pane_id: String,
    pub parent: Option<String>,
    pub session: SessionInfo,
    pub cwd: String,
    pub idle_min: i64,
    pub rss_kb: Option<u64>,
    pub resume: Option<String>,
    pub reports: Vec<String>,
    pub screen_tail: String,
}

fn safe_name(s: &str) -> String {
    let t: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if t.is_empty() {
        "_".into()
    } else {
        t
    }
}

/// `<state>/closed-spec-panes/<goal>-<first 8 of pane id>.md`
pub fn record_path(state_dir: &Path, goal: &str, pane_id: &str) -> PathBuf {
    let short: String = pane_id.chars().take(8).collect();
    state_dir.join("closed-spec-panes").join(format!(
        "{}-{}.md",
        safe_name(goal),
        safe_name(&short)
    ))
}

pub fn render_record(r: &CloseRecord, now: i64) -> String {
    let mut s = String::new();
    s.push_str(&format!("# Closed spec pane: goal {}\n\n", r.goal));
    s.push_str(&format!("- closedAt (epoch ms): {now}\n"));
    s.push_str(&format!("- reason: {}\n", r.reason));
    s.push_str(&format!("- idle: {}m\n", r.idle_min));
    s.push_str(&format!("- pane: {}\n", r.pane_id));
    s.push_str(&format!(
        "- parent: {}\n",
        r.parent.as_deref().unwrap_or("-")
    ));
    s.push_str(&format!("- session: {}\n", r.session.session_id));
    s.push_str(&format!("- configDir: {}\n", r.session.config_dir));
    s.push_str(&format!("- cwd: {}\n", r.cwd));
    if let Some(kb) = r.rss_kb {
        s.push_str(&format!("- rssKb: {kb}\n"));
    }
    s.push_str("\n## Resume\n\n");
    match &r.resume {
        Some(c) => s.push_str(&format!("```sh\n{c}\n```\n")),
        None => s.push_str("(no safe resume command: session id or cwd unavailable)\n"),
    }
    s.push_str("\n## Last reports to the parent\n\n");
    if r.reports.is_empty() {
        s.push_str("(none found)\n");
    }
    for rep in &r.reports {
        s.push_str(&format!("- {}\n", rep.replace('\n', "\n  ")));
    }
    s.push_str("\n## Last screen lines\n\n```\n");
    s.push_str(&r.screen_tail);
    s.push_str("\n```\n");
    s
}

/// Write the handover record. Returns its path.
pub fn write_record(state_dir: &Path, r: &CloseRecord, now: i64) -> std::io::Result<PathBuf> {
    let path = record_path(state_dir, &r.goal, &r.pane_id);
    crate::persistence::paths::write_atomic_private(&path, render_record(r, now).as_bytes())?;
    Ok(path)
}

/// The JSON line appended to `spec-sweep.log`.
pub fn log_entry(
    r: &CloseRecord,
    record: Option<&Path>,
    would_close: bool,
    now: i64,
) -> serde_json::Value {
    let mut v = json!({
        "ts": now,
        "paneId": r.pane_id,
        "goal": r.goal,
        "parent": r.parent,
        "reason": r.reason,
        "sessionId": r.session.session_id,
        "configDir": r.session.config_dir,
        "cwd": r.cwd,
        "idleMin": r.idle_min,
        "rssKb": r.rss_kb,
        "resume": r.resume,
        "record": record.map(|p| p.display().to_string()),
    });
    if would_close {
        v["wouldClose"] = json!(true);
    }
    v
}

pub fn append_log(
    state_dir: &Path,
    r: &CloseRecord,
    record: Option<&Path>,
    would_close: bool,
    now: i64,
) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(state_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir.join("spec-sweep.log"))?;
    writeln!(f, "{}", log_entry(r, record, would_close, now))
}

/// The inbox line for the parent.
pub fn parent_message(r: &CloseRecord, record: &Path) -> String {
    format!(
        "spec pane closed: goal {} ({}, idle {}m). Resume: {}. Record: {}",
        r.goal,
        r.reason,
        r.idle_min,
        r.resume.as_deref().unwrap_or("unavailable"),
        record.display()
    )
}

/// Last `n` lines of `text`.
pub fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Build the close record for an action from the env.
pub fn build_record(env: &dyn SweepEnv, pane: &SpecPane, a: &Action) -> CloseRecord {
    let o = &a.obs;
    let session = o.session.clone().unwrap_or_default();
    let cwd = if session.cwd.is_empty() {
        o.cwd.clone().unwrap_or_default()
    } else {
        session.cwd.clone()
    };
    let resume = resume_command(&session, o.cwd.as_deref());
    let reports = match &o.parent {
        Some(par) => {
            let all = env.reports_from(par, &o.pane_id);
            all[all.len().saturating_sub(RECORD_REPORTS)..]
                .iter()
                .map(|(b, ts)| format!("[{ts}] {b}"))
                .collect()
        }
        None => Vec::new(),
    };
    CloseRecord {
        goal: o.goal.clone(),
        reason: a.reason.as_str().to_string(),
        pane_id: o.pane_id.clone(),
        parent: o.parent.clone(),
        session,
        cwd,
        idle_min: a.idle_ms / 60_000,
        rss_kb: o.rss_kb,
        resume,
        reports,
        screen_tail: env
            .screen(pane)
            .map(|s| tail_lines(&s.text, RECORD_SCREEN_LINES))
            .unwrap_or_default(),
    }
}

// ---- live environment ----------------------------------------------------------------------

/// The real [`SweepEnv`] over a running server.
pub struct LiveEnv {
    shared: Arc<Shared>,
    peers: Vec<crate::claude_peers::Peer>,
}

impl LiveEnv {
    pub fn new(shared: &Arc<Shared>) -> Self {
        Self {
            shared: Arc::clone(shared),
            peers: crate::claude_peers::read_all()
                .into_iter()
                .filter(|p| p.is_live())
                .collect(),
        }
    }
}

impl SweepEnv for LiveEnv {
    fn now_ms(&self) -> i64 {
        now_ms()
    }

    fn spec_panes(&self) -> Vec<SpecPane> {
        let m = self.shared.model.lock().unwrap();
        m.panes()
            .into_iter()
            .filter(|r| r.status != crate::control::readmodel::PaneStatus::Exited)
            .filter_map(|r| {
                let info = m.pane(&r.pane_id)?;
                let meta = info.meta.clone()?;
                (meta.get("role").map(String::as_str) == Some("spec")).then(|| SpecPane {
                    pane_id: r.pane_id.clone(),
                    uid: r.session_uid.clone(),
                    meta,
                    cwd: info.cwd.clone(),
                })
            })
            .collect()
    }

    fn is_busy(&self, p: &SpecPane) -> bool {
        use crate::control::readmodel::Activity;
        let status = {
            let m = self.shared.model.lock().unwrap();
            match m.pane(&p.pane_id) {
                Some(i) => i.status,
                None => return true,
            }
        };
        let act = crate::control::server::activity_for(
            &self.shared.sessions,
            self.shared.idle_threshold_ms,
            &p.uid,
            status,
        );
        // Only a positively quiet pane counts as idle.
        !matches!(act, Activity::Idle | Activity::AwaitingInput)
    }

    fn last_output_at(&self, p: &SpecPane) -> Option<i64> {
        self.shared
            .sessions
            .last_output_at(&p.uid)
            .map(|t| t as i64)
    }

    fn session(&self, p: &SpecPane) -> Option<SessionInfo> {
        crate::claude_panes::read_pane_session(&p.pane_id).map(|s| SessionInfo {
            session_id: s.session_id,
            cwd: s.cwd,
            config_dir: s.config_dir,
        })
    }

    fn peer(&self, session_id: &str) -> Option<PeerObs> {
        self.peers
            .iter()
            .find(|p| p.session_id == session_id)
            .map(|p| PeerObs {
                status: p.status,
                status_updated_at: p.status_updated_at,
                pid: p.pid,
                started_at: p.started_at,
            })
    }

    fn claimed_tasks(&self, queues: &[String]) -> usize {
        let wq = self.shared.work.lock().unwrap();
        queues.iter().map(|q| wq.counts(q).claimed).sum()
    }

    fn worker_children(&self, pane_id: &str) -> usize {
        let m = self.shared.model.lock().unwrap();
        m.panes()
            .iter()
            .filter(|r| r.status != crate::control::readmodel::PaneStatus::Exited)
            .filter_map(|r| m.pane(&r.pane_id))
            .filter(|i| {
                i.meta.as_ref().is_some_and(|mm| {
                    mm.get("parent").map(String::as_str) == Some(pane_id)
                        && mm.get("role").map(String::as_str) == Some("worker")
                })
            })
            .count()
    }

    fn descendants(&self, pid: u32, grace_ms: i64) -> Descendants {
        proc_descendants(pid, grace_ms)
    }

    fn screen(&self, p: &SpecPane) -> Option<ScreenObs> {
        self.shared
            .sessions
            .render_screen_read(&p.uid)
            .map(|r| ScreenObs {
                text: r.text,
                input: r.input_line.map(|l| (l.kind, l.text)),
            })
    }

    fn rss_kb(&self, pid: u32) -> Option<u64> {
        proc_rss_kb(pid)
    }

    fn pending_nudges(&self, pane_id: &str) -> usize {
        self.shared.nudges.lock().unwrap().pending(pane_id)
    }

    fn report_file(&self, path: &str) -> ReportFile {
        match std::fs::metadata(path) {
            Ok(m) if m.is_file() && m.len() > 0 => ReportFile::Ready,
            _ => ReportFile::Missing,
        }
    }

    fn inbox_last_to(&self, pane_id: &str) -> Option<i64> {
        self.shared
            .inbox
            .lock()
            .unwrap()
            .read(pane_id, 0)
            .last()
            .map(|m| m.ts)
    }

    fn reports_from(&self, parent: &str, from_pane: &str) -> Vec<(String, i64)> {
        self.shared
            .inbox
            .lock()
            .unwrap()
            .read(parent, 0)
            .into_iter()
            .filter(|m| m.from == from_pane)
            .map(|m| (m.body, m.ts))
            .collect()
    }
}

// ---- /proc helpers -------------------------------------------------------------------------

/// The bits of `/proc/<pid>/stat` the sweeper needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatInfo {
    pub state: char,
    pub ppid: u32,
    /// Field 22: start time in clock ticks since boot.
    pub start_ticks: u64,
}

/// Parse a `/proc/<pid>/stat` line. `comm` (field 2) may contain spaces and parentheses, so
/// split after the LAST `)`.
pub fn parse_stat(line: &str) -> Option<StatInfo> {
    let rest = &line[line.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // After `)`: f[0] = field 3 (state), f[1] = field 4 (ppid), f[19] = field 22 (starttime).
    Some(StatInfo {
        state: f.first()?.chars().next()?,
        ppid: f.get(1)?.parse().ok()?,
        start_ticks: f.get(19)?.parse().ok()?,
    })
}

/// Process start in epoch ms from `starttime` ticks, the boot time (epoch seconds) and the
/// clock-tick rate.
pub fn start_ms(start_ticks: u64, btime_s: u64, clk_tck: u64) -> i64 {
    (btime_s * 1000 + start_ticks * 1000 / clk_tck.max(1)) as i64
}

/// Pure core of the descendant scan: `procs` is `(pid, stat)`; returns whether any live
/// descendant of `root` started more than `grace_ms` after `root` did.
pub fn descendants_beyond_grace(
    procs: &[(u32, StatInfo)],
    root: u32,
    btime_s: u64,
    clk_tck: u64,
    grace_ms: i64,
) -> Descendants {
    let Some((_, root_stat)) = procs.iter().find(|(p, _)| *p == root) else {
        return Descendants::Unknown;
    };
    let root_start = start_ms(root_stat.start_ticks, btime_s, clk_tck);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for (pid, st) in procs {
        children.entry(st.ppid).or_default().push(*pid);
    }
    let by_pid: HashMap<u32, &StatInfo> = procs.iter().map(|(p, s)| (*p, s)).collect();
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        for &c in children.get(&p).map(Vec::as_slice).unwrap_or(&[]) {
            if !seen.insert(c) {
                continue;
            }
            stack.push(c);
            let st = by_pid[&c];
            if matches!(st.state, 'Z' | 'X') {
                continue;
            }
            if start_ms(st.start_ticks, btime_s, clk_tck) > root_start + grace_ms {
                return Descendants::Some;
            }
        }
    }
    Descendants::None
}

#[cfg(target_os = "linux")]
fn proc_descendants(pid: u32, grace_ms: i64) -> Descendants {
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Descendants::Unknown;
    };
    let procs: Vec<(u32, StatInfo)> = rd
        .flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            let line = std::fs::read_to_string(e.path().join("stat")).ok()?;
            Some((pid, parse_stat(&line)?))
        })
        .collect();
    let btime = std::fs::read_to_string("/proc/stat").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("btime ")?.trim().parse::<u64>().ok())
    });
    let Some(btime) = btime else {
        return Descendants::Unknown;
    };
    // SAFETY: sysconf has no preconditions.
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let tck = if tck > 0 { tck as u64 } else { 100 };
    descendants_beyond_grace(&procs, pid, btime, tck, grace_ms)
}

#[cfg(not(target_os = "linux"))]
fn proc_descendants(_pid: u32, _grace_ms: i64) -> Descendants {
    Descendants::Unknown
}

#[cfg(target_os = "linux")]
fn proc_rss_kb(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn proc_rss_kb(_pid: u32) -> Option<u64> {
    None
}

// ---- server glue ---------------------------------------------------------------------------

/// Record one inbox post into the ledger (called by the post route).
pub fn record_post(shared: &Arc<Shared>, to: &str, from: &str, seq: u64, body: &str, ts: i64) {
    let (from_pane, from_is_spec) = {
        let m = shared.model.lock().unwrap();
        match m
            .resolve_pane_id(from)
            .and_then(|id| m.pane(&id).map(|p| (id, p)))
        {
            Some((id, p)) => {
                let spec = p
                    .meta
                    .as_ref()
                    .and_then(|mm| mm.get("role"))
                    .is_some_and(|r| r == "spec");
                (Some(id), spec)
            }
            None => (None, false),
        }
    };
    shared.spec_sweep.lock().unwrap().record_post(
        to,
        from_pane.as_deref(),
        from_is_spec,
        seq,
        ts,
        body,
    );
}

/// Run one sweep and perform its closes.
pub fn sweep_once(shared: &Arc<Shared>) {
    let cfg = SweepCfg::load();
    if !cfg.enabled {
        return;
    }
    let env = LiveEnv::new(shared);
    let actions = {
        let mut ledger = shared.spec_sweep.lock().unwrap();
        sweep_pass(&env, &mut ledger, &cfg)
    };
    let state_dir = crate::persistence::paths::state_dir();
    for a in actions {
        let pane = SpecPane {
            pane_id: a.obs.pane_id.clone(),
            uid: {
                let m = shared.model.lock().unwrap();
                match m.pane(&a.obs.pane_id) {
                    Some(i) => i.session_uid.clone(),
                    None => continue,
                }
            },
            meta: BTreeMap::new(),
            cwd: a.obs.cwd.clone(),
        };
        perform_close(shared, &env, &state_dir, &pane, &a);
    }
}

fn perform_close(
    shared: &Arc<Shared>,
    env: &LiveEnv,
    state_dir: &Path,
    pane: &SpecPane,
    a: &Action,
) {
    let now = now_ms();
    let rec = build_record(env, pane, a);
    let summary = format!(
        "[spec-sweep] {} spec pane {} goal {} ({}, idle {}m) session {}",
        if a.dry_run { "would close" } else { "closing" },
        rec.pane_id,
        rec.goal,
        rec.reason,
        rec.idle_min,
        rec.session.session_id
    );
    if a.dry_run {
        if let Err(e) = append_log(state_dir, &rec, None, true, now) {
            eprintln!("[spec-sweep] log write failed: {e}");
        }
        eprintln!("{summary}");
        return;
    }
    let path = match write_record(state_dir, &rec, now) {
        Ok(p) => Some(p),
        Err(e) => {
            // No handover, no close: a closed pane with no record is unrecoverable by hand.
            eprintln!(
                "[spec-sweep] record write failed, keeping pane {}: {e}",
                rec.pane_id
            );
            return;
        }
    };
    if let Err(e) = append_log(state_dir, &rec, path.as_deref(), false, now) {
        eprintln!("[spec-sweep] log write failed: {e}");
    }
    eprintln!("{summary}");
    if let (Some(parent), Some(path)) = (&rec.parent, &path) {
        let known = shared.model.lock().unwrap().pane(parent).is_some();
        if known {
            let body = parent_message(&rec, path);
            let msg = shared
                .inbox
                .lock()
                .unwrap()
                .post(parent, "hyperpanes", &body, now);
            let coords = shared.model.lock().unwrap().coords_of(parent);
            shared.events.broadcast_for_pane(
                coords.as_ref(),
                &crate::control::events::ControlEvent::Message {
                    to: parent.clone(),
                    from: "hyperpanes".to_string(),
                    seq: msg.seq,
                    body,
                },
            );
        }
    }
    let control_file = shared.control_file.to_str().map(str::to_string);
    let result = {
        let mut m = shared.model.lock().unwrap();
        crate::control::dispatch::handle_command(
            &mut m,
            &shared.sessions,
            control_file.as_deref(),
            None,
            &json!({ "type": "closePane", "paneId": rec.pane_id }),
            &shared.speech,
        )
    };
    shared.reconcile_policies();
    if result.notify_state {
        notify_state(shared);
    }
    shared.nudges.lock().unwrap().drop_pane(&rec.pane_id);
    shared.spec_sweep.lock().unwrap().drop_pane(&rec.pane_id);
}

/// A spec pane's row for a team panel.
#[derive(Debug, Clone)]
pub struct SpecPaneInfo {
    pub pane_id: String,
    pub goal: String,
    pub parent: Option<String>,
    pub project: Option<String>,
    /// Age of the claude session from its peer record, when known.
    pub age_ms: Option<i64>,
    pub idle_ms: Option<i64>,
    pub rss_kb: Option<u64>,
    pub verdict: String,
}

/// Read-only view of every spec pane and what the sweeper would do. Does not touch the
/// hysteresis state.
pub fn snapshot(shared: &Arc<Shared>) -> Vec<SpecPaneInfo> {
    let cfg = SweepCfg::load();
    let env = LiveEnv::new(shared);
    let now = now_ms();
    let ledger = shared.spec_sweep.lock().unwrap();
    env.spec_panes()
        .iter()
        .map(|p| {
            let o = observe(&env, &ledger, p);
            SpecPaneInfo {
                pane_id: o.pane_id.clone(),
                goal: o.goal.clone(),
                parent: o.parent.clone(),
                project: o.project.clone(),
                age_ms: o.peer.and_then(|pe| pe.started_at).map(|s| now - s),
                idle_ms: idle_since(&o).map(|s| now - s),
                rss_kb: o.rss_kb,
                verdict: decide(&o, &cfg, now).describe(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
