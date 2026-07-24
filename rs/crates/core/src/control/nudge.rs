//! Inbox nudges — waking an agent pane that has mail.
//!
//! The message bus (`control::inbox`) is PULL-only: a post lands in the target pane's durable
//! queue and pings live WS clients, but nothing writes to the pane's pty. That is fine for a
//! client that polls, and fatal for the goals org: an orchestrator/spec agent is an interactive
//! `claude` TUI, so the moment it finishes a turn it blocks on stdin forever. Reports from its
//! children pile up unread until a human types into the pane.
//!
//! A nudge closes that loop: when a message lands for a pane that has opted in (org roles —
//! `goals-orch` / `spec`, or an explicit `hp.nudge=on` meta), we wait for the pane to go quiet
//! and then type ONE short line into it telling it to read its inbox. Rules that keep this from
//! becoming a footgun:
//!   • **Never mid-turn** — only when the pane's activity is not `Busy`, so we never inject into
//!     a running command or a half-typed prompt.
//!   • **Coalesced** — a burst of messages produces one nudge naming the whole seq range; only
//!     one waiter task per pane is ever in flight.
//!   • **Rate-limited** — at most one nudge per pane per [`MIN_INTERVAL_MS`], so a chatty wave of
//!     impl agents can't turn into a typing storm.
//!   • **Opt-out** — `hp.nudge=off` meta on the pane, or `HYPERPANES_MSG_NUDGE=0` in the app's
//!     environment, disables it everywhere.
//!
//! This module is pure + in-memory so the policy is unit-testable without a server; the routes
//! layer owns the timer and the pty write.

use std::collections::{BTreeMap, HashMap};

/// Pane `meta.role` values that get nudged by default — the goals-system org tiers that run as
/// interactive TUIs and are expected to act on their inbox.
pub const NUDGED_ROLES: &[&str] = &["goals-orch", "spec"];

/// Meta key an embedder/agent can set to force nudging on (`on`) or off (`off`) regardless of role.
pub const NUDGE_META_KEY: &str = "hp.nudge";

/// Never nudge the same pane more often than this, however much mail arrives.
pub const MIN_INTERVAL_MS: i64 = 60_000;

/// How long a waiter re-checks the pane's activity before it gives up (the pane stayed busy the
/// whole time). The pending count survives — the next message re-arms a fresh waiter.
pub const MAX_WAIT_MS: i64 = 30 * 60_000;

/// Settle window before the first delivery attempt: lets a burst of sibling reports coalesce into
/// one nudge, and keeps us from typing the instant a pane's own turn ends.
pub const SETTLE_MS: u64 = 3_000;

/// How often a waiter re-checks whether the pane went quiet.
pub const POLL_MS: u64 = 2_000;

/// Whether a pane with this meta wants inbox nudges. Explicit `hp.nudge` wins; otherwise the
/// org roles in [`NUDGED_ROLES`] opt in and everything else stays pull-only (frozen behaviour).
pub fn wants_nudge(meta: Option<&BTreeMap<String, String>>) -> bool {
    let Some(meta) = meta else { return false };
    match meta.get(NUDGE_META_KEY).map(String::as_str) {
        Some("off") | Some("0") | Some("false") => return false,
        Some("on") | Some("1") | Some("true") => return true,
        _ => {}
    }
    meta.get("role")
        .is_some_and(|r| NUDGED_ROLES.contains(&r.as_str()))
}

/// The line typed into a nudged pane. One line, no shell metacharacters, and it names the exact
/// cursor to read from so the agent doesn't re-ingest its whole backlog.
pub fn nudge_text(pane_id: &str, pending: usize, first_seq: u64) -> String {
    let plural = if pending == 1 { "" } else { "s" };
    format!(
        "[hyperpanes] inbox: {pending} new message{plural}. Read with read_messages \
         {{paneId:\"{pane_id}\", after:{}}}, act on {} (report/consult/decision), then continue \
         your loop.",
        first_seq.saturating_sub(1),
        if pending == 1 { "it" } else { "them" }
    )
}

/// One pane's nudge bookkeeping.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PaneNudge {
    /// Unread-since-last-nudge count (what the nudge line reports).
    pending: usize,
    /// Seq of the first message in the pending batch — the read cursor we hand the agent.
    first_seq: u64,
    /// A waiter task is in flight for this pane (so a burst spawns exactly one).
    waiting: bool,
    /// When the current waiter armed, for the [`MAX_WAIT_MS`] give-up.
    armed_at_ms: i64,
    /// When we last typed into this pane, for [`MIN_INTERVAL_MS`].
    last_sent_ms: Option<i64>,
}

/// What a waiter should do on this poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Pane is busy / rate-limited — check again later.
    Wait,
    /// Type this line into the pane; the waiter is done afterwards.
    Send(String),
    /// Nothing pending, or the waiter hit [`MAX_WAIT_MS`]; the waiter is done.
    Stop,
}

/// Per-pane nudge state for the whole server. Pure — the caller supplies `now` and the pane's
/// current activity, and performs the actual write.
#[derive(Debug, Default)]
pub struct NudgeLedger {
    panes: HashMap<String, PaneNudge>,
}

impl NudgeLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a message for `pane_id`. Returns `true` if the caller should spawn a waiter task
    /// (no other waiter is in flight for this pane).
    pub fn arm(&mut self, pane_id: &str, seq: u64, now_ms: i64) -> bool {
        let e = self.panes.entry(pane_id.to_string()).or_default();
        if e.pending == 0 {
            e.first_seq = seq;
        }
        e.pending += 1;
        if e.waiting {
            return false;
        }
        e.waiting = true;
        e.armed_at_ms = now_ms;
        true
    }

    /// One waiter poll. `busy` is the pane's live activity (true ⇒ mid-turn, never type).
    /// On [`Step::Send`] the pending batch is consumed and the waiter retires.
    pub fn poll(&mut self, pane_id: &str, busy: bool, now_ms: i64) -> Step {
        let Some(e) = self.panes.get_mut(pane_id) else {
            return Step::Stop;
        };
        if e.pending == 0 {
            e.waiting = false;
            return Step::Stop;
        }
        let rate_limited = e.last_sent_ms.is_some_and(|t| now_ms - t < MIN_INTERVAL_MS);
        if busy || rate_limited {
            if now_ms - e.armed_at_ms >= MAX_WAIT_MS {
                // Give up this round; the pending batch stays, so the next message re-arms.
                e.waiting = false;
                return Step::Stop;
            }
            return Step::Wait;
        }
        let text = nudge_text(pane_id, e.pending, e.first_seq);
        e.pending = 0;
        e.first_seq = 0;
        e.waiting = false;
        e.last_sent_ms = Some(now_ms);
        Step::Send(text)
    }

    /// Forget a pane (on close), so a closed pane's counters don't leak.
    pub fn drop_pane(&mut self, pane_id: &str) {
        self.panes.remove(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn org_roles_opt_in_and_explicit_meta_wins_both_ways() {
        assert!(wants_nudge(Some(&meta(&[("role", "goals-orch")]))));
        assert!(wants_nudge(Some(&meta(&[("role", "spec")]))));
        // Impl/worker panes and plain panes stay pull-only.
        assert!(!wants_nudge(Some(&meta(&[("role", "impl")]))));
        assert!(!wants_nudge(Some(&meta(&[]))));
        assert!(!wants_nudge(None));
        // Explicit override in both directions.
        assert!(!wants_nudge(Some(&meta(&[
            ("role", "spec"),
            (NUDGE_META_KEY, "off")
        ]))));
        assert!(wants_nudge(Some(&meta(&[(NUDGE_META_KEY, "on")]))));
    }

    #[test]
    fn a_burst_arms_one_waiter_and_coalesces_into_one_nudge() {
        let mut l = NudgeLedger::new();
        assert!(l.arm("p1", 10, 0)); // first message spawns the waiter
        assert!(!l.arm("p1", 11, 100)); // burst rides along
        assert!(!l.arm("p1", 12, 200));
        let text = match l.poll("p1", false, 5_000) {
            Step::Send(t) => t,
            other => panic!("expected Send, got {other:?}"),
        };
        assert!(text.contains("3 new messages"), "{text}");
        // Cursor is first_seq - 1, so read_messages returns exactly the batch.
        assert!(text.contains("after:9"), "{text}");
        // Batch consumed; the waiter retires.
        assert_eq!(l.poll("p1", false, 6_000), Step::Stop);
    }

    #[test]
    fn never_types_into_a_busy_pane_and_gives_up_after_the_max_wait() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 1, 0);
        assert_eq!(l.poll("p1", true, 1_000), Step::Wait);
        assert_eq!(l.poll("p1", true, MAX_WAIT_MS), Step::Stop);
        // The message is still pending — a later arrival re-arms a fresh waiter.
        assert!(l.arm("p1", 2, MAX_WAIT_MS + 1));
        match l.poll("p1", false, MAX_WAIT_MS + 2) {
            Step::Send(t) => assert!(t.contains("2 new messages"), "{t}"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn rate_limits_a_chatty_wave_to_one_nudge_per_interval() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 1, 0);
        assert!(matches!(l.poll("p1", false, 0), Step::Send(_)));
        l.arm("p1", 2, 10);
        assert_eq!(l.poll("p1", false, 10), Step::Wait); // too soon
        assert!(matches!(
            l.poll("p1", false, MIN_INTERVAL_MS + 10),
            Step::Send(_)
        ));
    }

    #[test]
    fn drop_pane_forgets_a_closed_panes_counters() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 1, 0);
        l.drop_pane("p1");
        assert_eq!(l.poll("p1", false, 0), Step::Stop);
    }
}
