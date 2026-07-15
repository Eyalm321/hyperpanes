//! Per-pane Claude Code session markers — the bridge that lets a relaunch resume the
//! conversation each pane was having.
//!
//! A Claude Code `SessionStart` hook (`resources/claude/hp-claude-session-hook.sh`) runs
//! inside every `claude` a user launches in a pane. The pane's environment carries
//! `HYPERPANES_PANE_ID`, and the hook's stdin carries the conversation's `session_id` —
//! so the hook writes `<state dir>/claude-sessions/<pane-id>.json`:
//!
//! ```json
//! { "sessionId": "0198c4a2-…", "cwd": "/home/me/dev/x" }
//! ```
//!
//! `SessionEnd` removes the marker, so a marker exists exactly while a conversation is
//! live in that pane. The GUI's relaunch snapshot ([`crate::workspace::model::PaneSpec`]
//! via `to_session_file`) embeds the id as pane meta (key [`META_KEY`]); on restore, a
//! pane whose live session did NOT survive re-spawns and resumes the conversation with
//! `claude --resume <id>` in its original cwd.
//!
//! The session id is embedded into a command line at restore, so [`read_pane_session`]
//! accepts only ids matching Claude's UUID shape (`[0-9a-fA-F-]`) — anything else is
//! treated as a corrupt/hostile marker and ignored.

use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

use crate::persistence::paths;

/// Pane-meta key under which the snapshot records the pane's live Claude session id.
pub const META_KEY: &str = "claude.session";

/// Pane-meta key for the conversation's working directory. `claude --resume <id>` only
/// finds sessions belonging to the CURRENT directory's project, and a pane's own cwd
/// snapshot can be stale (a shell parked inside a TUI never re-emits OSC 7 after a GUI
/// re-attach) — so the hook-reported cwd is authoritative and restore must `cd` first.
pub const META_CWD_KEY: &str = "claude.cwd";

/// Is `cwd` safe to interpolate into a single-quoted `cd '<cwd>'`? Absolute, no control
/// characters, and no single quotes (rather than escaping, refuse — real project paths
/// never contain them, and refusing keeps the injection reasoning trivial).
pub fn valid_resume_cwd(cwd: &str) -> bool {
    cwd.starts_with('/')
        && cwd.len() < 1024
        && !cwd.contains('\'')
        && !cwd.chars().any(|c| c.is_control())
}

/// One pane's live Claude conversation, as reported by the session hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneClaudeSession {
    /// The conversation's session id — what `claude --resume <id>` takes.
    pub session_id: String,
    /// The conversation's working directory at SessionStart (informational; the pane's
    /// own cwd snapshot is what restore actually uses).
    #[serde(default)]
    pub cwd: String,
}

/// The marker file for one pane id.
fn marker_path(pane_id: &str) -> PathBuf {
    paths::claude_sessions_dir().join(format!("{pane_id}.json"))
}

/// Is `id` shaped like a Claude session id (UUID: hex + dashes, sane length)? The id is
/// later interpolated into a shell command line, so this is a safety gate, not a nicety.
/// Public because restore re-checks ids read back from `workspace.json` — that file is
/// user-editable, so the write-time validation here cannot be assumed to hold.
pub fn valid_session_id(id: &str) -> bool {
    (8..=64).contains(&id.len()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Read the live-session marker for `pane_id`, if one exists and is well-formed.
/// Missing file, unparseable JSON, or a malformed id all yield `None` — a marker is
/// best-effort state written by an external hook, never trusted blindly.
pub fn read_pane_session(pane_id: &str) -> Option<PaneClaudeSession> {
    let text = fs::read_to_string(marker_path(pane_id)).ok()?;
    let parsed: PaneClaudeSession = serde_json::from_str(&text).ok()?;
    valid_session_id(&parsed.session_id).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_uuid_shaped_ids_only() {
        assert!(valid_session_id("0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a"));
        assert!(valid_session_id("deadbeef"));
        // Shell metacharacters, spaces, quotes — all refused.
        assert!(!valid_session_id("abc; rm -rf /"));
        assert!(!valid_session_id("abc def"));
        assert!(!valid_session_id("$(boom)"));
        assert!(!valid_session_id("ab")); // too short
        assert!(!valid_session_id(&"a".repeat(65))); // too long
    }

    #[test]
    fn resume_cwd_gate() {
        assert!(valid_resume_cwd("/home/me/dev/x"));
        assert!(valid_resume_cwd("/tmp/a b/c")); // spaces fine inside single quotes
        assert!(!valid_resume_cwd("relative/path"));
        assert!(!valid_resume_cwd("/has'quote"));
        assert!(!valid_resume_cwd("/has\nnewline"));
        assert!(!valid_resume_cwd(""));
    }

    #[test]
    fn parses_marker_json_shape() {
        let parsed: PaneClaudeSession = serde_json::from_str(
            r#"{ "sessionId": "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a", "cwd": "/w" }"#,
        )
        .unwrap();
        assert_eq!(parsed.session_id, "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a");
        assert_eq!(parsed.cwd, "/w");
        // cwd is optional — an older/minimal hook payload still parses.
        let bare: PaneClaudeSession =
            serde_json::from_str(r#"{ "sessionId": "deadbeef" }"#).unwrap();
        assert_eq!(bare.cwd, "");
    }
}
