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
//!     one waiter task per pane is ever in flight. The settle slides: the nudge waits until no
//!     new message arrived for [`SETTLE_MS`] (capped at [`MAX_SETTLE_MS`] after the first), so
//!     relays chained a few seconds apart still cost one nudge.
//!   • **Read-aware** — a read of the pane's inbox (`GET /panes/:id/messages`, which MCP
//!     `read_messages` and the messages resource both use) drops every pending message it
//!     returned ([`NudgeLedger::mark_read`]), and a waiter with nothing unread left types nothing.
//!     So a pane woken another way (a sender's own typed wake) that reads its mail is never
//!     nudged for it afterwards. The ledger cannot tell who read: any reader of the pane's
//!     inbox, such as a parent peeking at a child's, counts as the pane having read it.
//!   • **Rate-limited** — at most one nudge per pane per [`MIN_INTERVAL_MS`], so a chatty wave of
//!     impl agents can't turn into a typing storm.
//!   • **Opt-out** — `hp.nudge=off` meta on the pane, or `HYPERPANES_MSG_NUDGE=0` in the app's
//!     environment, disables it everywhere.
//!
//! **Wording follows what the pane can actually do.** Panes opted in by org role are launched
//! with the hyperpanes MCP server, so their nudge names the `read_messages` tool. A pane that
//! opted in only through an explicit `hp.nudge=on` may have no MCP at all, so its nudge names the
//! control-API read instead (`GET http://127.0.0.1:<port>/panes/<id>/messages?after=<seq>`, with the
//! port and bearer token from the discovery file at `$HYPERPANES_CONTROL_FILE`). A pane can pick its form explicitly:
//! `hp.nudge=mcp` (MCP wording) or `hp.nudge=http` (control-API wording); `on` keeps its
//! meaning "nudge me" and uses the role default (MCP for org roles, control-API otherwise).
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

/// Settle window: a nudge is typed only once no new message arrived for this long. Lets a burst
/// of sibling reports coalesce into one nudge, and keeps us from typing the instant a pane's own
/// turn ends. It slides with each arrival, up to [`MAX_SETTLE_MS`]. Long enough for a lead's
/// chained relays (one CLI run each, seconds apart on a loaded machine) to land as one batch.
pub const SETTLE_MS: u64 = 8_000;

/// A steady stream of mail can hold the sliding settle open this long after the first message
/// of the batch, no longer, so a chatty sender cannot starve the nudge.
pub const MAX_SETTLE_MS: i64 = 30_000;

/// How often a waiter re-checks whether the pane went quiet.
pub const POLL_MS: u64 = 2_000;

/// How a nudge tells the pane to read its mail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NudgeForm {
    /// Name the `read_messages` MCP tool (the pane has the hyperpanes MCP server).
    Mcp,
    /// Name the control-API HTTP read (works without MCP).
    Http,
}

/// Which wording a pane gets. `hp.nudge=mcp` / `http` choose explicitly; org roles default to
/// MCP (they are launched with the MCP server); any other pane (opted in via `hp.nudge=on`)
/// defaults to the control-API wording since MCP can't be assumed.
pub fn nudge_form(meta: Option<&BTreeMap<String, String>>) -> NudgeForm {
    let Some(meta) = meta else {
        return NudgeForm::Http;
    };
    match meta.get(NUDGE_META_KEY).map(String::as_str) {
        Some("mcp") => return NudgeForm::Mcp,
        Some("http") => return NudgeForm::Http,
        _ => {}
    }
    if meta
        .get("role")
        .is_some_and(|r| NUDGED_ROLES.contains(&r.as_str()))
    {
        NudgeForm::Mcp
    } else {
        NudgeForm::Http
    }
}

/// Whether a pane with this meta wants inbox nudges. Explicit `hp.nudge` wins; otherwise the
/// org roles in [`NUDGED_ROLES`] opt in and everything else stays pull-only (frozen behaviour).
pub fn wants_nudge(meta: Option<&BTreeMap<String, String>>) -> bool {
    let Some(meta) = meta else { return false };
    match meta.get(NUDGE_META_KEY).map(String::as_str) {
        Some("off") | Some("0") | Some("false") => return false,
        Some("on") | Some("1") | Some("true") | Some("mcp") | Some("http") => return true,
        _ => {}
    }
    meta.get("role")
        .is_some_and(|r| NUDGED_ROLES.contains(&r.as_str()))
}

/// The line typed into a nudged pane. One line, no shell metacharacters, and it names the exact
/// cursor to read from so the agent doesn't re-ingest its whole backlog.
pub fn nudge_text(pane_id: &str, pending: usize, first_seq: u64, form: NudgeForm) -> String {
    let plural = if pending == 1 { "" } else { "s" };
    let after = first_seq.saturating_sub(1);
    let them = if pending == 1 { "it" } else { "them" };
    let how = match form {
        NudgeForm::Mcp => format!("read_messages {{paneId:\"{pane_id}\", after:{after}}}"),
        NudgeForm::Http => format!(
            "GET http://127.0.0.1:<port>/panes/{pane_id}/messages?after={after} \
             (port and bearer token are in the JSON file at $HYPERPANES_CONTROL_FILE)"
        ),
    };
    format!(
        "[hyperpanes] inbox: {pending} new message{plural}. Read with {how}, act on {them} \
         (report/consult/decision), then continue your loop."
    )
}

/// One pane's nudge bookkeeping.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PaneNudge {
    /// Seqs of the messages that arrived since the last nudge and that no read has returned
    /// yet. Their count is what the nudge line reports; the lowest is the read cursor (minus
    /// one) we hand the agent.
    pending: Vec<u64>,
    /// A waiter task is in flight for this pane (so a burst spawns exactly one).
    waiting: bool,
    /// When the current waiter armed, for the [`MAX_WAIT_MS`] give-up and the
    /// [`MAX_SETTLE_MS`] cap.
    armed_at_ms: i64,
    /// When the newest pending message arrived, for the sliding [`SETTLE_MS`].
    last_arrival_ms: i64,
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
        e.pending.push(seq);
        e.last_arrival_ms = now_ms;
        if e.waiting {
            return false;
        }
        e.waiting = true;
        e.armed_at_ms = now_ms;
        true
    }

    /// A read of `pane_id`'s inbox handed the reader every message up to `through_seq`: those
    /// are no longer unread, so drop them from the pending batch. A waiter left with nothing
    /// pending stops at its next poll without typing; one with newer mail nudges for that only.
    pub fn mark_read(&mut self, pane_id: &str, through_seq: u64) {
        if let Some(e) = self.panes.get_mut(pane_id) {
            e.pending.retain(|&seq| seq > through_seq);
        }
    }

    /// How many messages are pending (unread, not yet nudged for) for `pane_id`.
    pub fn pending(&self, pane_id: &str) -> usize {
        self.panes.get(pane_id).map_or(0, |e| e.pending.len())
    }

    /// One waiter poll. `busy` is the pane's live activity (true ⇒ mid-turn, never type).
    /// `form` picks the wording. On [`Step::Send`] the pending batch is consumed and the waiter retires.
    pub fn poll(&mut self, pane_id: &str, busy: bool, now_ms: i64, form: NudgeForm) -> Step {
        let Some(e) = self.panes.get_mut(pane_id) else {
            return Step::Stop;
        };
        // Nothing unread left (never armed, or a read returned it all): type nothing.
        let Some(&first_seq) = e.pending.iter().min() else {
            e.waiting = false;
            return Step::Stop;
        };
        let rate_limited = e.last_sent_ms.is_some_and(|t| now_ms - t < MIN_INTERVAL_MS);
        let settling =
            now_ms - e.last_arrival_ms < SETTLE_MS as i64 && now_ms - e.armed_at_ms < MAX_SETTLE_MS;
        if busy || rate_limited || settling {
            if now_ms - e.armed_at_ms >= MAX_WAIT_MS {
                // Give up this round; the pending batch stays, so the next message re-arms.
                e.waiting = false;
                return Step::Stop;
            }
            return Step::Wait;
        }
        let text = nudge_text(pane_id, e.pending.len(), first_seq, form);
        e.pending.clear();
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
    fn explicit_on_gets_non_mcp_wording_and_role_default_keeps_mcp() {
        let on = meta(&[(NUDGE_META_KEY, "on")]);
        assert_eq!(nudge_form(Some(&on)), NudgeForm::Http);
        let role = meta(&[("role", "spec")]);
        assert_eq!(nudge_form(Some(&role)), NudgeForm::Mcp);
        // Role + explicit on: still an org pane with MCP.
        let both = meta(&[("role", "goals-orch"), (NUDGE_META_KEY, "on")]);
        assert_eq!(nudge_form(Some(&both)), NudgeForm::Mcp);
        // Explicit forms win and imply opt-in.
        let http = meta(&[("role", "spec"), (NUDGE_META_KEY, "http")]);
        assert_eq!(nudge_form(Some(&http)), NudgeForm::Http);
        let mcp = meta(&[(NUDGE_META_KEY, "mcp")]);
        assert_eq!(nudge_form(Some(&mcp)), NudgeForm::Mcp);
        assert!(wants_nudge(Some(&http)) && wants_nudge(Some(&mcp)));
    }

    #[test]
    fn text_matches_form_cursor_and_plurality() {
        let m = nudge_text("p1", 1, 5, NudgeForm::Mcp);
        assert!(m.contains("1 new message."), "{m}");
        assert!(m.contains("read_messages {paneId:\"p1\", after:4}"), "{m}");
        assert!(m.contains("act on it"), "{m}");
        let h = nudge_text("p1", 3, 10, NudgeForm::Http);
        assert!(h.contains("3 new messages."), "{h}");
        assert!(h.contains("/panes/p1/messages?after=9"), "{h}");
        assert!(h.contains("$HYPERPANES_CONTROL_FILE"), "{h}");
        assert!(!h.contains("read_messages"), "{h}");
        assert!(h.contains("act on them"), "{h}");
        assert!(nudge_text("p1", 1, 0, NudgeForm::Http).contains("after=0"));
    }

    #[test]
    fn a_burst_arms_one_waiter_and_coalesces_into_one_nudge() {
        let mut l = NudgeLedger::new();
        assert!(l.arm("p1", 10, 0)); // first message spawns the waiter
        assert!(!l.arm("p1", 11, 100)); // burst rides along
        assert!(!l.arm("p1", 12, 200));
        let settled = 200 + SETTLE_MS as i64;
        let text = match l.poll("p1", false, settled, NudgeForm::Mcp) {
            Step::Send(t) => t,
            other => panic!("expected Send, got {other:?}"),
        };
        assert!(text.contains("3 new messages"), "{text}");
        // Cursor is first_seq - 1, so read_messages returns exactly the batch.
        assert!(text.contains("after:9"), "{text}");
        // Batch consumed; the waiter retires.
        assert_eq!(
            l.poll("p1", false, settled + 1_000, NudgeForm::Mcp),
            Step::Stop
        );
    }

    #[test]
    fn never_types_into_a_busy_pane_and_gives_up_after_the_max_wait() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 1, 0);
        assert_eq!(l.poll("p1", true, 1_000, NudgeForm::Mcp), Step::Wait);
        assert_eq!(l.poll("p1", true, MAX_WAIT_MS, NudgeForm::Mcp), Step::Stop);
        // The message is still pending — a later arrival re-arms a fresh waiter.
        assert!(l.arm("p1", 2, MAX_WAIT_MS + 1));
        match l.poll(
            "p1",
            false,
            MAX_WAIT_MS + 1 + SETTLE_MS as i64,
            NudgeForm::Mcp,
        ) {
            Step::Send(t) => assert!(t.contains("2 new messages"), "{t}"),
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn rate_limits_a_chatty_wave_to_one_nudge_per_interval() {
        let mut l = NudgeLedger::new();
        let settle = SETTLE_MS as i64;
        l.arm("p1", 1, 0);
        assert!(matches!(
            l.poll("p1", false, settle, NudgeForm::Mcp),
            Step::Send(_)
        ));
        l.arm("p1", 2, settle + 10);
        // Too soon after the last nudge, even once the new message has settled.
        assert_eq!(
            l.poll("p1", false, 2 * settle + 10, NudgeForm::Mcp),
            Step::Wait
        );
        assert!(matches!(
            l.poll("p1", false, settle + MIN_INTERVAL_MS + 10, NudgeForm::Mcp),
            Step::Send(_)
        ));
    }

    #[test]
    fn the_settle_slides_with_each_arrival_up_to_the_cap() {
        let settle = SETTLE_MS as i64;
        let mut l = NudgeLedger::new();
        assert!(l.arm("p1", 1, 0));
        assert!(!l.arm("p1", 2, 4_600)); // a relay a slow CLI run behind the first
        assert_eq!(l.poll("p1", false, settle, NudgeForm::Mcp), Step::Wait);
        match l.poll("p1", false, 4_600 + settle, NudgeForm::Mcp) {
            Step::Send(t) => assert!(t.contains("2 new messages"), "{t}"),
            other => panic!("expected Send, got {other:?}"),
        }
        // A steady stream holds the settle open only until MAX_SETTLE_MS after the first.
        let mut l = NudgeLedger::new();
        let mut t = 0;
        let mut seq = 1;
        while t <= MAX_SETTLE_MS {
            l.arm("p1", seq, t);
            seq += 1;
            t += 2_000;
        }
        assert_eq!(
            l.poll("p1", false, MAX_SETTLE_MS - 1, NudgeForm::Mcp),
            Step::Wait
        );
        assert!(matches!(
            l.poll("p1", false, MAX_SETTLE_MS, NudgeForm::Mcp),
            Step::Send(_)
        ));
    }

    // t39: the receiver read its mail during the turn another wake started, so the nudge
    // waiting for it to go idle must not fire for messages it already has.
    #[test]
    fn a_read_of_everything_pending_retires_the_waiter_without_typing() {
        let mut l = NudgeLedger::new();
        assert!(l.arm("p1", 3, 0));
        l.arm("p1", 4, 100);
        l.arm("p1", 5, 200);
        assert_eq!(l.poll("p1", true, 5_000, NudgeForm::Mcp), Step::Wait); // mid-turn
        l.mark_read("p1", 5);
        assert_eq!(l.pending("p1"), 0);
        assert_eq!(l.poll("p1", false, 7_000, NudgeForm::Mcp), Step::Stop);
        // The waiter retired, so the next message arms a fresh one.
        assert!(l.arm("p1", 6, 8_000));
    }

    #[test]
    fn a_partial_read_leaves_a_nudge_for_the_unread_rest_only() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 3, 0);
        l.arm("p1", 4, 0);
        l.arm("p1", 5, 0);
        l.mark_read("p1", 4);
        match l.poll("p1", false, SETTLE_MS as i64, NudgeForm::Mcp) {
            Step::Send(t) => {
                assert!(t.contains("1 new message."), "{t}");
                assert!(t.contains("after:4"), "{t}");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn mail_that_lands_after_a_read_is_nudged_alone() {
        let mut l = NudgeLedger::new();
        assert!(l.arm("p1", 3, 0));
        l.mark_read("p1", 3);
        assert!(!l.arm("p1", 7, 100), "the first waiter is still in flight");
        match l.poll("p1", false, 100 + SETTLE_MS as i64, NudgeForm::Mcp) {
            Step::Send(t) => {
                assert!(t.contains("1 new message."), "{t}");
                assert!(t.contains("after:6"), "{t}");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn mark_read_ignores_unknown_panes_and_older_cursors() {
        let mut l = NudgeLedger::new();
        l.mark_read("nobody", 10); // no entry, no panic
        assert_eq!(l.pending("nobody"), 0);
        l.arm("p1", 8, 0);
        l.arm("p1", 9, 0);
        l.mark_read("p1", 7); // a cursor below the batch reads none of it
        assert_eq!(l.pending("p1"), 2);
    }

    #[test]
    fn drop_pane_forgets_a_closed_panes_counters() {
        let mut l = NudgeLedger::new();
        l.arm("p1", 1, 0);
        l.drop_pane("p1");
        assert_eq!(l.poll("p1", false, 0, NudgeForm::Mcp), Step::Stop);
    }
}
