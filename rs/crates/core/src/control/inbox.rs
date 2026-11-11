//! Port of `src/main/control-inbox.ts` — the durable per-pane message bus:
//! bounded ring buffer, monotonic `seq`, `dropped` accounting, at-least-once
//! read-after cursor. Mirror every case in `control-inbox.test.ts`.
//!
//! Per-pane message inbox — the structured inter-node transport for the agent
//! message bus (agent-orchestration E). Pure + in-memory so it's unit-testable
//! without a running server; ControlServer owns one instance and feeds it the
//! clock (`now`) so this module stays deterministic.
//!
//! Delivery model:
//!   • DURABLE — every message is kept in the target pane's queue, so a node that
//!     connects late (or reconnects) still reads its backlog. Push on `/events` is
//!     only a *nudge*; the read is the source of truth.
//!   • AT-LEAST-ONCE, cursor-based — `read(paneId, afterSeq)` returns everything
//!     with a higher monotonic seq, so a reader advances its own cursor. No acks,
//!     no server-side "read" state (a pane may have several readers).
//!   • BOUNDED — the oldest messages are evicted past MAX_PER_PANE so a chatty
//!     sender can't grow memory without limit (the dropped count is observable).
//!   • IN-MEMORY, EPOCH-STAMPED — the inbox lives in the GUI process and starts empty
//!     (seq back at 1) on every app start, including a GUI-only restart. Each inbox
//!     lifetime has an `epoch`, and a cursor read past the pane's latest seq (a cursor
//!     from an earlier lifetime) returns the whole backlog flagged `reset`, so a reader
//!     holding a stale cursor re-reads instead of silently missing new mail.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneMessage {
    /// global monotonic id; readers use it as a cursor
    pub seq: u64,
    /// target paneId
    pub to: String,
    /// sender id (a paneId, or a free-form orchestrator label)
    pub from: String,
    pub body: String,
    /// ms epoch, stamped by the caller
    pub ts: i64,
}

/// Keep the last N messages per pane. Generous — these are small text payloads.
pub const MAX_PER_PANE: usize = 500;

/// A cursor read: the messages, and whether the cursor was from an earlier inbox lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRead {
    pub messages: Vec<PaneMessage>,
    /// The cursor was past this pane's latest seq — it belongs to an earlier inbox (the app
    /// restarted) — so `messages` is the whole retained backlog, not a delta.
    pub reset: bool,
}

#[derive(Debug, Default)]
pub struct MessageInbox {
    by_pane: HashMap<String, Vec<PaneMessage>>,
    seq: u64,
    /// Identifies this inbox lifetime (ms epoch when it was created). Changes on every
    /// app start, so a reader that stored it can tell its cursor is stale.
    epoch: u64,
    /// Messages evicted by the per-pane cap, so callers can surface "you missed N".
    dropped: HashMap<String, usize>,
}

impl MessageInbox {
    /// A fresh inbox stamped with the current time as its epoch.
    pub fn new() -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        Self::with_epoch(now)
    }

    /// A fresh inbox with an explicit epoch (tests, deterministic callers).
    pub fn with_epoch(epoch: u64) -> Self {
        Self {
            epoch,
            ..Self::default()
        }
    }

    /// This inbox lifetime's id: stable until the app restarts, then different.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Enqueue a message for `to`. Returns the stored message (with its seq).
    pub fn post(&mut self, to: &str, from: &str, body: &str, ts: i64) -> PaneMessage {
        self.seq += 1;
        let msg = PaneMessage {
            seq: self.seq,
            to: to.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            ts,
        };
        let list = self.by_pane.entry(to.to_string()).or_default();
        list.push(msg.clone());
        if list.len() > MAX_PER_PANE {
            let overflow = list.len() - MAX_PER_PANE;
            list.drain(0..overflow);
            *self.dropped.entry(to.to_string()).or_insert(0) += overflow;
        }
        msg
    }

    /// Messages for `paneId` with seq > afterSeq (afterSeq=0 ⇒ all retained). The
    /// returned vec is a copy, ordered by seq ascending.
    pub fn read(&self, pane_id: &str, after_seq: u64) -> Vec<PaneMessage> {
        let list = match self.by_pane.get(pane_id) {
            None => return Vec::new(),
            Some(l) => l,
        };
        if after_seq > 0 {
            list.iter().filter(|m| m.seq > after_seq).cloned().collect()
        } else {
            list.clone()
        }
    }

    /// A cursor read that notices a stale cursor. `after_seq` past the pane's latest seq
    /// cannot come from this inbox (seqs only grow and the newest is never evicted), so it
    /// is a cursor from before a restart: return everything retained, flagged `reset`.
    pub fn read_cursor(&self, pane_id: &str, after_seq: u64) -> InboxRead {
        if after_seq > self.latest_seq(pane_id) {
            return InboxRead {
                messages: self.read(pane_id, 0),
                reset: true,
            };
        }
        InboxRead {
            messages: self.read(pane_id, after_seq),
            reset: false,
        }
    }

    /// How many messages were evicted for `paneId` by the cap (for "you missed N").
    pub fn dropped_count(&self, pane_id: &str) -> usize {
        self.dropped.get(pane_id).copied().unwrap_or(0)
    }

    /// The highest seq currently retained for `paneId` (0 if empty) — a fresh
    /// reader can start its cursor here to skip backlog.
    pub fn latest_seq(&self, pane_id: &str) -> u64 {
        self.by_pane
            .get(pane_id)
            .and_then(|l| l.last())
            .map_or(0, |m| m.seq)
    }

    /// Forget a pane's inbox (on close). Keeps the dropped counter cleared too.
    pub fn drop(&mut self, pane_id: &str) {
        self.by_pane.remove(pane_id);
        self.dropped.remove(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivers_durably_and_reads_by_cursor_at_least_once() {
        let mut inbox = MessageInbox::new();
        let m1 = inbox.post("p1", "mgr", "do X", 1000);
        let m2 = inbox.post("p1", "mgr", "do Y", 1001);
        inbox.post("p2", "mgr", "other", 1002); // different pane

        assert_eq!(m1.seq, 1);
        assert_eq!(m2.seq, 2);
        // Full read for p1, ordered by seq.
        let bodies: Vec<String> = inbox.read("p1", 0).into_iter().map(|m| m.body).collect();
        assert_eq!(bodies, vec!["do X", "do Y"]);
        // Cursor read: only messages after seq 1.
        let after: Vec<String> = inbox
            .read("p1", m1.seq)
            .into_iter()
            .map(|m| m.body)
            .collect();
        assert_eq!(after, vec!["do Y"]);
        // A pane with no messages reads empty.
        assert_eq!(inbox.read("p3", 0), Vec::<PaneMessage>::new());
    }

    #[test]
    fn seq_is_global_and_monotonic_across_panes() {
        let mut inbox = MessageInbox::new();
        inbox.post("a", "x", "1", 0);
        let b = inbox.post("b", "x", "2", 0);
        assert_eq!(b.seq, 2);
        assert_eq!(inbox.latest_seq("a"), 1);
        assert_eq!(inbox.latest_seq("b"), 2);
        assert_eq!(inbox.latest_seq("missing"), 0);
    }

    #[test]
    fn bounds_per_pane_history_and_counts_the_evicted_overflow() {
        let mut inbox = MessageInbox::new();
        for i in 0..(MAX_PER_PANE + 5) {
            inbox.post("p", "x", &format!("m{i}"), i as i64);
        }
        let kept = inbox.read("p", 0);
        assert_eq!(kept.len(), MAX_PER_PANE);
        assert_eq!(kept[0].body, "m5"); // first 5 evicted
        assert_eq!(inbox.dropped_count("p"), 5);
    }

    #[test]
    fn drop_forgets_a_pane_inbox_and_its_dropped_counter() {
        let mut inbox = MessageInbox::new();
        inbox.post("p", "x", "hi", 0);
        inbox.drop("p");
        assert_eq!(inbox.read("p", 0), Vec::<PaneMessage>::new());
        assert_eq!(inbox.dropped_count("p"), 0);
    }

    // t36: after a restart the seq starts again at 1, so a reader still holding `after:48`
    // used to get nothing back while new mail sat at seq 1.
    #[test]
    fn a_cursor_from_an_earlier_lifetime_reads_everything_flagged_reset() {
        let mut inbox = MessageInbox::with_epoch(7);
        inbox.post("p1", "lead", "new goal", 1);
        let r = inbox.read_cursor("p1", 48);
        assert!(r.reset);
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].body, "new goal");
        assert_eq!(inbox.epoch(), 7);
    }

    #[test]
    fn a_current_cursor_reads_the_delta_without_reset() {
        let mut inbox = MessageInbox::with_epoch(1);
        let a = inbox.post("p1", "x", "one", 1);
        inbox.post("p1", "x", "two", 2);
        let r = inbox.read_cursor("p1", a.seq);
        assert!(!r.reset);
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].body, "two");
        // Caught up exactly: nothing new, still not a reset.
        let caught_up = inbox.read_cursor("p1", inbox.latest_seq("p1"));
        assert!(!caught_up.reset);
        assert!(caught_up.messages.is_empty());
        // No cursor at all is a plain full read.
        assert!(!inbox.read_cursor("p1", 0).reset);
    }

    #[test]
    fn a_stale_cursor_on_an_empty_inbox_is_a_reset_with_no_messages() {
        let inbox = MessageInbox::with_epoch(1);
        let r = inbox.read_cursor("p1", 5);
        assert!(r.reset);
        assert!(r.messages.is_empty());
    }

    #[test]
    fn each_new_inbox_gets_its_own_epoch() {
        let a = MessageInbox::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = MessageInbox::new();
        assert!(a.epoch() > 0);
        assert_ne!(a.epoch(), b.epoch());
    }
}
