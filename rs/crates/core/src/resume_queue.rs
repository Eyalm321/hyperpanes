//! Durable session→prompt queue — the "speak-first" half of claude-resume.
//!
//! `queue_prompt` stores a message for a *conversation* (a Claude session id, see
//! [`crate::claude_panes`]) in `<state dir>/resume-prompts.json`. The GUI's delivery
//! tick watches the claude-sessions marker dir: when a marker for a queued session
//! (re)appears — the resumed claude's SessionStart hook wrote it, so the agent is up —
//! the prompt is typed into the owning pane and removed from the queue.
//!
//! Deliver-once, file-backed (survives GUI relaunch, daemon death, reboot — the whole
//! point: "after the restart, continue X" outlives every process involved).

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::persistence::paths;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueuedPrompt {
    /// Claude session id the prompt is addressed to (validated at enqueue).
    pub session_id: String,
    /// The message typed into the pane once the session is ready.
    pub text: String,
    /// ms epoch, stamped at enqueue — for observability, not ordering (FIFO by position).
    pub queued_at: u64,
}

fn queue_file() -> PathBuf {
    paths::resume_prompts_json()
}

fn load() -> Vec<QueuedPrompt> {
    let Ok(text) = fs::read_to_string(queue_file()) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

fn persist(all: &[QueuedPrompt]) {
    if let Ok(json) = serde_json::to_vec_pretty(all) {
        let _ = paths::write_atomic(&queue_file(), &json);
    }
}

/// Append a prompt for `session_id`. The id must be marker-shaped
/// ([`crate::claude_panes::valid_session_id`]) and the text non-empty.
pub fn enqueue(session_id: &str, text: &str) -> Result<(), String> {
    if !crate::claude_panes::valid_session_id(session_id) {
        return Err(format!("not a valid session id: {session_id}"));
    }
    if text.trim().is_empty() {
        return Err("empty prompt".into());
    }
    let mut all = load();
    all.push(QueuedPrompt {
        session_id: session_id.to_string(),
        text: text.to_string(),
        queued_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    persist(&all);
    Ok(())
}

/// Remove and return every queued prompt for `session_id`, oldest first.
pub fn take_for(session_id: &str) -> Vec<QueuedPrompt> {
    let all = load();
    let (taken, kept): (Vec<_>, Vec<_>) = all.into_iter().partition(|p| p.session_id == session_id);
    if !taken.is_empty() {
        persist(&kept);
    }
    taken
}

/// Does anything wait for any session? Cheap gate for the delivery tick (one stat).
pub fn is_empty() -> bool {
    match fs::metadata(queue_file()) {
        Err(_) => true,
        // A written-out empty array is 2 bytes ("[]") — anything bigger may hold work.
        Ok(m) => m.len() <= 2,
    }
}

/// Peek at all queued prompts (for a `/state`-style listing; never removes).
pub fn list() -> Vec<QueuedPrompt> {
    load()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    /// Queue tests share one process-global state dir (paths reads env), so serialize them.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        L.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    fn scratch_state_dir() {
        let dir = std::env::temp_dir().join(format!("hp-rq-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("XDG_STATE_HOME", &dir);
    }

    #[test]
    fn enqueue_take_roundtrip_is_fifo_and_deliver_once() {
        let _g = lock();
        scratch_state_dir();
        assert!(is_empty());
        enqueue("deadbeef-0000", "first").unwrap();
        enqueue("deadbeef-0000", "second").unwrap();
        enqueue("cafecafe-1111", "other session").unwrap();
        assert!(!is_empty());

        let taken = take_for("deadbeef-0000");
        assert_eq!(
            taken.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        // Deliver-once: a second take finds nothing; the other session's prompt survives.
        assert!(take_for("deadbeef-0000").is_empty());
        assert_eq!(take_for("cafecafe-1111").len(), 1);
        assert!(is_empty());
    }

    #[test]
    fn rejects_invalid_ids_and_empty_text() {
        let _g = lock();
        scratch_state_dir();
        assert!(enqueue("$(boom)", "hi").is_err());
        assert!(enqueue("deadbeef-0000", "   ").is_err());
        assert!(is_empty());
    }
}
