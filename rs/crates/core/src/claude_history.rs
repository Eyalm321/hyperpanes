//! Reader for Claude Code's per-project **session history**
//! (`~/.claude/projects/<ENCODED_CWD>/<session-id>.jsonl`).
//!
//! Claude Code writes one JSONL transcript per session, in a directory whose name is the
//! session's working directory with every **non-alphanumeric** character replaced by `-`.
//! Verified against the live `~/.claude/projects` layout on this machine:
//!
//! ```text
//! C:\hyperpanes                          -> C--hyperpanes
//! C:\hyperpanes.fanout\track19-history   -> C--hyperpanes-fanout-track19-history
//! C:\canora\.claude-worktrees\festive-…  -> C--canora--claude-worktrees-festive-…
//! ```
//!
//! Note the `.` in `hyperpanes.fanout` is encoded too (the folder is `…hyperpanes-fanout…`,
//! NOT `…hyperpanes.fanout…`), and runs are **not** collapsed — `:` then `\` becomes `--`.
//! So the rule is simply: keep `[A-Za-z0-9]`, map everything else to `-`.
//!
//! The reader is intentionally cheap. It lists the `*.jsonl` files, takes each file's mtime
//! (for recency ordering) and a line count (a proxy for transcript length), and JSON-parses
//! only a bounded *prefix* of each file to recover a one-line summary — it never fully parses
//! a large transcript.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Max characters kept for a session's summary line (the UI elides further if needed).
const SUMMARY_MAX: usize = 160;
/// How many leading JSONL records we JSON-parse looking for a summary / first user message.
/// Bounded so a huge transcript is never fully parsed just to label it.
const SUMMARY_SCAN_LINES: usize = 60;

/// Which agent harness a history entry comes from. Claude Code is the only source today;
/// the enum exists so UI-facing rows carry a generic `source` label instead of hard-wiring
/// "claude", letting other harnesses (their own readers feeding the same session shape)
/// plug in later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistorySource {
    #[default]
    Claude,
}

impl HistorySource {
    /// The human label shown next to a session row (e.g. "Claude").
    pub fn label(self) -> &'static str {
        match self {
            HistorySource::Claude => "Claude",
        }
    }
}

/// One Claude Code session transcript discovered on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSession {
    /// The session id — the `.jsonl` filename stem (a UUID). This is what
    /// `claude --resume <id>` takes.
    pub id: String,
    /// The harness that produced this transcript (see [`HistorySource`]).
    pub source: HistorySource,
    /// Last-modified time as epoch milliseconds (the file mtime), or `None` if unavailable.
    /// Drives newest-first ordering and a relative-time label.
    pub started_at: Option<u64>,
    /// A one-line summary: a `summary`-type record if the transcript has one, else the first
    /// user message's text — whitespace-collapsed and truncated. Empty if neither was found.
    pub summary: String,
    /// The first user message's text (cleaned like `summary`) when one was found in the
    /// scanned prefix — kept separately so search can match the opening prompt even when an
    /// explicit `summary` record won the summary slot. Often equals `summary`.
    pub first_user: String,
    /// Number of JSONL records (line count) — a cheap proxy for transcript length.
    pub message_count: usize,
}

/// Does `session` match the (case-insensitive, substring) search `query`? An empty/blank
/// query matches everything. Matches against the summary AND the first user message — the
/// latter so a transcript whose summary record replaced the opening prompt is still found
/// by what the user remembers typing.
pub fn session_matches(session: &ClaudeSession, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    session.summary.to_lowercase().contains(&q) || session.first_user.to_lowercase().contains(&q)
}

/// Filter `sessions` down to those matching `query` (see [`session_matches`]), preserving
/// order. An empty/blank query returns the full list.
pub fn filter_sessions(sessions: &[ClaudeSession], query: &str) -> Vec<ClaudeSession> {
    sessions.iter().filter(|s| session_matches(s, query)).cloned().collect()
}

/// Encode an absolute project path the way Claude Code names its per-project transcript
/// directory: every character that is not ASCII alphanumeric becomes `-` (so `:`, `\`, `/`,
/// `.`, `_`, spaces … all map to `-`). No run-collapsing — each char maps to exactly one `-`.
pub fn encode_project_dir(project_root: &Path) -> String {
    encode_path_str(&project_root.to_string_lossy())
}

/// String form of [`encode_project_dir`] (kept separate so tests can pass raw path strings
/// without constructing platform `Path`s).
pub fn encode_path_str(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// `~/.claude/projects` for the current user (`%USERPROFILE%` on Windows, `$HOME` elsewhere),
/// or `None` if no home directory is known.
pub fn claude_projects_root() -> Option<PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join(".claude").join("projects"))
}

/// The sessions for `project_root`, resolved against the real `~/.claude/projects`. A missing
/// home dir or missing encoded directory yields an empty list. Newest-first.
pub fn sessions_for_project(project_root: &Path) -> Vec<ClaudeSession> {
    let Some(root) = claude_projects_root() else {
        return Vec::new();
    };
    sessions_for_project_in(&root, project_root)
}

/// Like [`sessions_for_project`] but against an explicit `projects_root` (the directory that
/// holds the encoded per-project folders) — the testable seam.
pub fn sessions_for_project_in(projects_root: &Path, project_root: &Path) -> Vec<ClaudeSession> {
    let dir = projects_root.join(encode_project_dir(project_root));
    sessions_in_dir(&dir)
}

/// List + summarize every `*.jsonl` transcript directly inside `session_dir`, newest-first
/// (by mtime). A missing/unreadable directory yields an empty list.
pub fn sessions_in_dir(session_dir: &Path) -> Vec<ClaudeSession> {
    let Ok(entries) = fs::read_dir(session_dir) else {
        return Vec::new();
    };
    let mut out: Vec<ClaudeSession> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // Only `*.jsonl` transcripts; skip subdirs and any sidecar files.
        if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl")) {
            continue;
        }
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let started_at = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64);
        let (summary, first_user, message_count) = summarize_file(&path);
        out.push(ClaudeSession {
            id,
            source: HistorySource::Claude,
            started_at,
            summary,
            first_user,
            message_count,
        });
    }
    sort_newest_first(&mut out);
    out
}

/// Order sessions newest-first by `started_at`, breaking ties by id (descending) so the
/// result is deterministic even when mtimes are equal.
fn sort_newest_first(v: &mut [ClaudeSession]) {
    v.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| b.id.cmp(&a.id)));
}

/// Read `path` once: count every record (cheaply) and, within the first
/// [`SUMMARY_SCAN_LINES`] records, recover a summary (preferring a `summary`-type record,
/// else the first `user` message's text) AND the first user message itself (kept for
/// search). Returns `(summary, first_user, line_count)`.
fn summarize_file(path: &Path) -> (String, String, usize) {
    use std::io::{BufRead, BufReader};
    let Ok(file) = fs::File::open(path) else {
        return (String::new(), String::new(), 0);
    };
    let reader = BufReader::new(file);
    let mut count = 0usize;
    let mut summary: Option<String> = None;
    let mut first_user: Option<String> = None;

    for (i, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        count += 1;
        // Only the bounded prefix is JSON-parsed; the rest is counted blind. Keep scanning
        // until BOTH the summary and the first user message are in hand (the first user
        // message feeds search even when a summary record wins the summary slot).
        if i >= SUMMARY_SCAN_LINES || (summary.is_some() && first_user.is_some()) {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("summary") => {
                if let Some(s) = v.get("summary").and_then(|s| s.as_str()) {
                    if !s.trim().is_empty() {
                        summary = Some(clean_summary(s));
                    }
                }
            }
            Some("user") if first_user.is_none() => {
                if let Some(text) = user_text(&v) {
                    if !text.trim().is_empty() {
                        first_user = Some(clean_summary(&text));
                    }
                }
            }
            _ => {}
        }
    }

    let first_user = first_user.unwrap_or_default();
    (summary.unwrap_or_else(|| first_user.clone()), first_user, count)
}

/// Pull the user-visible text out of a `user`-type record's `message.content`, which is either
/// a plain string or an array of content blocks (the first `{"type":"text"}` block wins).
/// Returns `None` for tool-result-only messages with no text.
fn user_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Collapse all whitespace (incl. newlines) to single spaces, trim, and truncate to
/// [`SUMMARY_MAX`] characters (char-boundary safe, with an ellipsis).
fn clean_summary(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, SUMMARY_MAX)
}

/// Truncate `s` to at most `max` characters, appending `…` when shortened.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- encoding (verified against the live ~/.claude/projects dir) ----

    #[test]
    fn encodes_paths_like_claude_code() {
        assert_eq!(encode_path_str("C:\\hyperpanes"), "C--hyperpanes");
        // The `.` is encoded too — the live folder is `…hyperpanes-fanout…`, not `….fanout…`.
        assert_eq!(
            encode_path_str("C:\\hyperpanes.fanout\\track19-history"),
            "C--hyperpanes-fanout-track19-history"
        );
        // Runs are not collapsed: `\` then `.` becomes `--`.
        assert_eq!(
            encode_path_str("C:\\canora\\.claude-worktrees\\festive-swartz-147488"),
            "C--canora--claude-worktrees-festive-swartz-147488"
        );
        // Forward slashes / underscores / spaces all map to `-` as well.
        assert_eq!(encode_path_str("/home/me/my_repo dir"), "-home-me-my-repo-dir");
    }

    #[test]
    fn encode_project_dir_matches_string_form() {
        let p = Path::new("C:\\hyperpanes");
        assert_eq!(encode_project_dir(p), "C--hyperpanes");
    }

    // ---- sessions_in_dir against synthetic transcripts ----

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-claude-hist-{}-{tag}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_dir_is_empty() {
        let missing = std::env::temp_dir().join(format!("hp-claude-none-{}", uuid::Uuid::new_v4()));
        assert!(sessions_in_dir(&missing).is_empty());
    }

    #[test]
    fn reads_summary_user_and_count() {
        let dir = temp_dir("read");

        // A transcript whose first user message is a plain string.
        std::fs::write(
            dir.join("aaaa.jsonl"),
            "{\"type\":\"mode\",\"mode\":\"normal\"}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Fix the   sidebar\\nlayout bug\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":\"ok\"}}\n",
        )
        .unwrap();

        // A transcript with an explicit summary record (preferred over any user message).
        std::fs::write(
            dir.join("bbbb.jsonl"),
            "{\"type\":\"summary\",\"summary\":\"Ported the worktree tree\",\"leafUuid\":\"x\"}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"ignored because summary wins\"}}\n",
        )
        .unwrap();

        // A transcript whose first user content is an array of blocks.
        std::fs::write(
            dir.join("cccc.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"array form prompt\"}]}}\n",
        )
        .unwrap();

        // A non-jsonl sidecar that must be ignored.
        std::fs::write(dir.join("notes.txt"), "ignore me").unwrap();

        let sessions = sessions_in_dir(&dir);
        assert_eq!(sessions.len(), 3, "three .jsonl files, the .txt ignored");

        let by_id: std::collections::HashMap<&str, &ClaudeSession> =
            sessions.iter().map(|s| (s.id.as_str(), s)).collect();

        let a = by_id["aaaa"];
        assert_eq!(a.summary, "Fix the sidebar layout bug"); // whitespace collapsed
        assert_eq!(a.message_count, 3);

        let b = by_id["bbbb"];
        assert_eq!(b.summary, "Ported the worktree tree"); // summary record beats user msg
        // …but the opening prompt is still captured separately, for search (#25).
        assert_eq!(b.first_user, "ignored because summary wins");
        assert_eq!(b.message_count, 2);

        let c = by_id["cccc"];
        assert_eq!(c.summary, "array form prompt"); // extracted from a content block
        assert_eq!(c.message_count, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_through_encoded_subdir() {
        let projects_root = temp_dir("root");
        let project = Path::new("C:\\hyperpanes");
        let encoded = projects_root.join("C--hyperpanes");
        std::fs::create_dir_all(&encoded).unwrap();
        std::fs::write(
            encoded.join("sess-1.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .unwrap();

        let sessions = sessions_for_project_in(&projects_root, project);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess-1");
        assert_eq!(sessions[0].summary, "hello");

        let _ = std::fs::remove_dir_all(&projects_root);
    }

    #[test]
    fn empty_summary_when_no_user_or_summary() {
        let dir = temp_dir("nosum");
        std::fs::write(
            dir.join("d.jsonl"),
            "{\"type\":\"mode\",\"mode\":\"normal\"}\n{\"type\":\"assistant\",\"message\":{}}\n",
        )
        .unwrap();
        let sessions = sessions_in_dir(&dir);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].summary.is_empty());
        assert_eq!(sessions[0].message_count, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bare session with just an id/time/summary/first-user — test shorthand.
    fn sess(id: &str, started_at: Option<u64>, summary: &str, first_user: &str) -> ClaudeSession {
        ClaudeSession {
            id: id.into(),
            source: HistorySource::Claude,
            started_at,
            summary: summary.into(),
            first_user: first_user.into(),
            message_count: 0,
        }
    }

    #[test]
    fn sort_is_newest_first() {
        let mut v = vec![
            sess("old", Some(100), "", ""),
            sess("new", Some(300), "", ""),
            sess("mid", Some(200), "", ""),
            sess("unknown", None, "", ""),
        ];
        sort_newest_first(&mut v);
        let ids: Vec<&str> = v.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "mid", "old", "unknown"]);
    }

    // ---- search / filter (#25) ----

    #[test]
    fn empty_or_blank_query_matches_everything() {
        let v = vec![sess("a", None, "Fix the sidebar", ""), sess("b", None, "", "")];
        assert_eq!(filter_sessions(&v, "").len(), 2);
        assert_eq!(filter_sessions(&v, "   ").len(), 2);
    }

    #[test]
    fn filter_is_case_insensitive_substring_over_summary() {
        let v = vec![
            sess("a", None, "Fix the Sidebar layout", "fix the sidebar layout"),
            sess("b", None, "Port the worktree tree", "port it"),
        ];
        let hits = filter_sessions(&v, "SIDEBAR");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "a");
        assert!(filter_sessions(&v, "tre").iter().any(|s| s.id == "b"));
        assert!(filter_sessions(&v, "no such thing").is_empty());
    }

    #[test]
    fn filter_matches_first_user_when_summary_record_won() {
        // Summary came from a `summary` record; the opening prompt only lives in first_user.
        let v = vec![sess("a", None, "Ported the worktree tree", "please refactor the parser")];
        assert_eq!(filter_sessions(&v, "refactor the parser").len(), 1);
        assert!(filter_sessions(&v, "missing").is_empty());
    }

    #[test]
    fn filter_preserves_order() {
        let v = vec![
            sess("first", Some(300), "alpha beta", ""),
            sess("second", Some(200), "beta gamma", ""),
            sess("third", Some(100), "gamma alpha", ""),
        ];
        let ids: Vec<String> = filter_sessions(&v, "alpha").into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["first", "third"]);
    }

    #[test]
    fn source_label_is_claude() {
        assert_eq!(HistorySource::Claude.label(), "Claude");
        assert_eq!(HistorySource::default(), HistorySource::Claude);
    }

    #[test]
    fn truncates_long_summaries() {
        let long = "x".repeat(SUMMARY_MAX + 50);
        let cleaned = clean_summary(&long);
        assert_eq!(cleaned.chars().count(), SUMMARY_MAX);
        assert!(cleaned.ends_with('…'));
    }

    // ---- tolerant smoke test against the real ~/.claude/projects (absence-tolerant) ----

    #[test]
    fn real_projects_dir_does_not_panic() {
        // Whatever the machine has (or doesn't), this must never panic and must return sane
        // rows. On the dev box this exercises the live `C--hyperpanes` transcripts.
        let sessions = sessions_for_project(Path::new("C:\\hyperpanes"));
        for s in &sessions {
            assert!(!s.id.is_empty());
            assert!(s.summary.chars().count() <= SUMMARY_MAX);
        }
    }
}
