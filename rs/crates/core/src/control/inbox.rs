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
//!   • EPOCH-STAMPED — each inbox lifetime has an `epoch`, and a cursor read past the pane's
//!     latest seq (a cursor from an earlier lifetime) returns the whole backlog flagged
//!     `reset`, so a reader holding a stale cursor re-reads instead of silently missing mail.
//!   • OPTIONALLY DURABLE — [`MessageInbox::open`] backs the inbox with SQLite (`messages.db`),
//!     write-through, with the in-memory maps as the read cache. A durable inbox keeps its
//!     epoch and its seq counter across app restarts and reboots, so mail posted before a
//!     restart is readable after it and cursors stay valid. Without a store (tests, a
//!     non-owner instance) the inbox is in-memory: it starts empty with a fresh epoch.
//!   • ALIASED — a restored pane gets a new id; [`MessageInbox::register_alias`] records
//!     `old → new`, re-keys the stored mail to the new id and keeps the table durable.
//!   • RETENTION — MAX_PER_PANE messages per pane and MAX_AGE_MS of age.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection};

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

/// Messages older than this are pruned (14 days, ms).
pub const MAX_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;

fn wall_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// A cursor read: the messages, and whether the cursor was from an earlier inbox lifetime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRead {
    pub messages: Vec<PaneMessage>,
    /// The cursor was past this pane's latest seq — it belongs to an earlier inbox (the app
    /// restarted) — so `messages` is the whole retained backlog, not a delta.
    pub reset: bool,
}

#[derive(Default)]
pub struct MessageInbox {
    /// Write-through SQLite store; `None` = purely in-memory.
    store: Option<Connection>,
    /// Durable old-id → new-id table (chains collapsed).
    aliases: HashMap<String, String>,
    by_pane: HashMap<String, Vec<PaneMessage>>,
    seq: u64,
    /// Identifies this inbox lifetime (ms epoch when it was created). Changes on every
    /// app start, so a reader that stored it can tell its cursor is stale.
    epoch: u64,
    /// Messages evicted by the per-pane cap, so callers can surface "you missed N".
    dropped: HashMap<String, usize>,
}

impl std::fmt::Debug for MessageInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MessageInbox")
            .field("seq", &self.seq)
            .field("epoch", &self.epoch)
            .field("durable", &self.store.is_some())
            .finish()
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS messages(
    seq INTEGER PRIMARY KEY, to_pane TEXT NOT NULL, from_id TEXT NOT NULL,
    body TEXT NOT NULL, ts INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS messages_to ON messages(to_pane, seq);
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS dropped(pane TEXT PRIMARY KEY, count INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS aliases(
    old_id TEXT PRIMARY KEY, new_id TEXT NOT NULL, ts INTEGER NOT NULL);
";

impl MessageInbox {
    /// Open (or create) the durable inbox at `path`. The epoch is minted once with the DB and
    /// stays stable across reopen; the seq counter continues from its persisted maximum so
    /// seqs are never reused even after pruning. Retained messages are loaded, with age
    /// pruning (against `now_ms`) applied.
    pub fn open(path: &Path, now_ms: i64) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.execute_batch(SCHEMA)?;
        let get_meta = |k: &str| -> Option<i64> {
            conn.query_row("SELECT value FROM meta WHERE key=?1", [k], |r| r.get(0))
                .ok()
        };
        let epoch = match get_meta("epoch") {
            Some(e) => e as u64,
            None => {
                conn.execute(
                    "INSERT INTO meta(key,value) VALUES('epoch',?1)",
                    [now_ms.max(1)],
                )?;
                now_ms.max(1) as u64
            }
        };
        let mut seq = get_meta("max_seq").unwrap_or(0) as u64;
        let mut inbox = Self::with_epoch(epoch);
        {
            let mut st =
                conn.prepare("SELECT seq,to_pane,from_id,body,ts FROM messages ORDER BY seq")?;
            let rows = st.query_map([], |r| {
                Ok(PaneMessage {
                    seq: r.get::<_, i64>(0)? as u64,
                    to: r.get(1)?,
                    from: r.get(2)?,
                    body: r.get(3)?,
                    ts: r.get(4)?,
                })
            })?;
            for m in rows {
                let m = m?;
                seq = seq.max(m.seq);
                inbox.by_pane.entry(m.to.clone()).or_default().push(m);
            }
            let mut st = conn.prepare("SELECT pane,count FROM dropped")?;
            for d in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (p, c) = d?;
                inbox.dropped.insert(p, c as usize);
            }
            let mut st = conn.prepare("SELECT old_id,new_id FROM aliases")?;
            for a in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
                let (o, n) = a?;
                inbox.aliases.insert(o, n);
            }
        }
        inbox.seq = seq;
        inbox.store = Some(conn);
        inbox.prune_age(now_ms);
        let panes: Vec<String> = inbox.by_pane.keys().cloned().collect();
        for p in panes {
            inbox.enforce_cap(&p);
        }
        Ok(inbox)
    }

    /// Whether a durable store backs this inbox.
    pub fn is_durable(&self) -> bool {
        self.store.is_some()
    }

    fn db(&self, what: &str, sql: &str, p: impl rusqlite::Params) {
        if let Some(c) = &self.store {
            if let Err(e) = c.execute(sql, p) {
                eprintln!("[inbox] durable write failed ({what}): {e}");
            }
        }
    }

    /// Drop messages older than `MAX_AGE_MS` relative to `now_ms` (memory and store).
    pub fn prune_age(&mut self, now_ms: i64) {
        let cutoff = now_ms - MAX_AGE_MS;
        for list in self.by_pane.values_mut() {
            list.retain(|m| m.ts >= cutoff);
        }
        self.by_pane.retain(|_, l| !l.is_empty());
        self.db("prune", "DELETE FROM messages WHERE ts<?1", [cutoff]);
    }

    /// Trim `pane` to MAX_PER_PANE, counting evictions in `dropped`.
    fn enforce_cap(&mut self, pane: &str) {
        let Some(list) = self.by_pane.get_mut(pane) else {
            return;
        };
        if list.len() <= MAX_PER_PANE {
            return;
        }
        let overflow = list.len() - MAX_PER_PANE;
        let cut_seq = list[overflow - 1].seq;
        list.drain(0..overflow);
        let total = {
            let d = self.dropped.entry(pane.to_string()).or_insert(0);
            *d += overflow;
            *d
        };
        self.db(
            "evict",
            "DELETE FROM messages WHERE to_pane=?1 AND seq<=?2",
            params![pane, cut_seq as i64],
        );
        self.db(
            "dropped",
            "INSERT OR REPLACE INTO dropped(pane,count) VALUES(?1,?2)",
            params![pane, total as i64],
        );
    }

    /// Record `old → new`: re-key `old`'s mail to `new` (seqs kept), point any alias that
    /// targeted `old` at `new` (chain collapse), and make `new` live again if it had itself
    /// been aliased away. Self-aliases and empty ids are ignored (returns false).
    pub fn register_alias(&mut self, old: &str, new: &str, ts: i64) -> bool {
        if old.is_empty() || new.is_empty() || old == new {
            return false;
        }
        self.aliases.remove(new);
        for v in self.aliases.values_mut() {
            if v == old {
                *v = new.to_string();
            }
        }
        self.aliases.insert(old.to_string(), new.to_string());
        if let Some(mut moved) = self.by_pane.remove(old) {
            for m in &mut moved {
                m.to = new.to_string();
            }
            let list = self.by_pane.entry(new.to_string()).or_default();
            list.append(&mut moved);
            list.sort_by_key(|m| m.seq);
        }
        if let Some(d) = self.dropped.remove(old) {
            *self.dropped.entry(new.to_string()).or_insert(0) += d;
        }
        if let Some(c) = &self.store {
            let r = (|| -> rusqlite::Result<()> {
                c.execute("DELETE FROM aliases WHERE old_id=?1", [new])?;
                c.execute("UPDATE aliases SET new_id=?1 WHERE new_id=?2", [new, old])?;
                c.execute(
                    "INSERT OR REPLACE INTO aliases(old_id,new_id,ts) VALUES(?1,?2,?3)",
                    params![old, new, ts],
                )?;
                c.execute(
                    "UPDATE messages SET to_pane=?1 WHERE to_pane=?2",
                    [new, old],
                )?;
                c.execute("DELETE FROM dropped WHERE pane=?1", [old])?;
                if let Some(n) = self.dropped.get(new) {
                    c.execute(
                        "INSERT OR REPLACE INTO dropped(pane,count) VALUES(?1,?2)",
                        params![new, *n as i64],
                    )?;
                }
                Ok(())
            })();
            if let Err(e) = r {
                eprintln!("[inbox] durable write failed (alias): {e}");
            }
        }
        self.enforce_cap(new);
        true
    }

    /// The alias table (old id → new id).
    pub fn aliases(&self) -> &HashMap<String, String> {
        &self.aliases
    }

    /// A fresh inbox stamped with the current time as its epoch.
    pub fn new() -> Self {
        Self::with_epoch(wall_ms() as u64)
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
        self.by_pane
            .entry(to.to_string())
            .or_default()
            .push(msg.clone());
        self.db(
            "post",
            "INSERT INTO messages(seq,to_pane,from_id,body,ts) VALUES(?1,?2,?3,?4,?5)",
            params![msg.seq as i64, to, from, body, ts],
        );
        self.db(
            "seq",
            "INSERT OR REPLACE INTO meta(key,value) VALUES('max_seq',?1)",
            [msg.seq as i64],
        );
        self.enforce_cap(to);
        if self.store.is_some() {
            self.prune_age(ts);
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
        self.db("drop", "DELETE FROM messages WHERE to_pane=?1", [pane_id]);
        self.db("drop", "DELETE FROM dropped WHERE pane=?1", [pane_id]);
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

    // ---- durable store + aliases (od2-A) ----

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-inbox-{tag}-{}-{}",
            std::process::id(),
            wall_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("messages.db")
    }

    #[test]
    fn durable_inbox_survives_reopen_and_continues_seq() {
        let path = scratch("reopen");
        let (epoch, latest) = {
            let mut a = MessageInbox::open(&path, 1_000).unwrap();
            a.post("p1", "lead", "one", 1_000);
            a.post("p2", "lead", "other", 1_001);
            a.post("p1", "lead", "two", 1_002);
            (a.epoch(), a.latest_seq("p1"))
        };
        let mut b = MessageInbox::open(&path, 2_000).unwrap();
        assert_eq!(b.epoch(), epoch);
        assert_eq!(b.latest_seq("p1"), latest);
        let bodies: Vec<_> = b.read("p1", 0).into_iter().map(|m| m.body).collect();
        assert_eq!(bodies, vec!["one", "two"]);
        assert_eq!(b.post("p1", "x", "three", 2_001).seq, latest + 1);
        // an old in-memory-lifetime cursor past latest still resets
        assert!(b.read_cursor("p1", 99).reset);
    }

    #[test]
    fn seq_never_reuses_after_the_newest_message_is_pruned() {
        let path = scratch("seqreuse");
        {
            let mut a = MessageInbox::open(&path, 0).unwrap();
            a.post("p", "x", "old", 0);
        }
        // reopen far in the future: the message ages out, but seq must continue
        let mut b = MessageInbox::open(&path, MAX_AGE_MS + 10).unwrap();
        assert!(b.read("p", 0).is_empty());
        assert_eq!(b.post("p", "x", "new", MAX_AGE_MS + 11).seq, 2);
    }

    #[test]
    fn durable_retention_by_count_survives_reopen_with_dropped() {
        let path = scratch("count");
        {
            let mut a = MessageInbox::open(&path, 10).unwrap();
            for i in 0..(MAX_PER_PANE + 3) {
                a.post("p", "x", &format!("m{i}"), 10);
            }
            assert_eq!(a.dropped_count("p"), 3);
        }
        let b = MessageInbox::open(&path, 10).unwrap();
        let kept = b.read("p", 0);
        assert_eq!(kept.len(), MAX_PER_PANE);
        assert_eq!(kept[0].body, "m3");
        assert_eq!(b.dropped_count("p"), 3);
    }

    #[test]
    fn retention_by_age_on_open_and_on_post() {
        let path = scratch("age");
        {
            let mut a = MessageInbox::open(&path, 0).unwrap();
            a.post("p", "x", "ancient", 0);
            a.post("p", "x", "recent", MAX_AGE_MS);
        }
        let mut b = MessageInbox::open(&path, MAX_AGE_MS + 1).unwrap();
        let bodies: Vec<_> = b.read("p", 0).into_iter().map(|m| m.body).collect();
        assert_eq!(bodies, vec!["recent"]);
        // posting far later prunes the now-stale "recent" one too
        b.post("p", "x", "latest", 3 * MAX_AGE_MS);
        let bodies: Vec<_> = b.read("p", 0).into_iter().map(|m| m.body).collect();
        assert_eq!(bodies, vec!["latest"]);
    }

    #[test]
    fn failed_open_is_an_error_and_memory_inbox_is_not_durable() {
        let dir = scratch("fail").parent().unwrap().to_path_buf();
        // a directory is not a database file
        assert!(MessageInbox::open(&dir, 0).is_err());
        assert!(!MessageInbox::new().is_durable());
    }

    #[test]
    fn alias_rekeys_mail_collapses_chains_and_survives_reopen() {
        let path = scratch("alias");
        {
            let mut a = MessageInbox::open(&path, 5).unwrap();
            let m1 = a.post("old", "x", "hello", 5);
            assert!(!a.register_alias("same", "same", 5));
            assert!(a.register_alias("old", "mid", 6));
            a.post("mid", "x", "again", 7);
            assert!(a.register_alias("mid", "new", 8));
            assert_eq!(a.aliases().get("old").map(String::as_str), Some("new"));
            assert_eq!(a.aliases().get("mid").map(String::as_str), Some("new"));
            let got = a.read("new", 0);
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].seq, m1.seq);
            assert!(got.iter().all(|m| m.to == "new"));
            assert!(a.read("old", 0).is_empty());
        }
        let b = MessageInbox::open(&path, 9).unwrap();
        assert_eq!(b.read("new", 0).len(), 2);
        assert_eq!(b.aliases().get("old").map(String::as_str), Some("new"));
        assert_eq!(b.aliases().get("mid").map(String::as_str), Some("new"));
    }
}
