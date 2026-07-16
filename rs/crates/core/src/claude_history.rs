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

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// Max characters kept for a session's summary line (the UI elides further if needed).
const SUMMARY_MAX: usize = 160;
/// How many leading JSONL records we JSON-parse looking for a summary / first user message.
/// Bounded so a huge transcript is never fully parsed just to label it.
const SUMMARY_SCAN_LINES: usize = 60;
/// Cap on the searchable full text extracted per session (bytes of message text kept).
/// Keeps memory sane on huge transcripts — search sees the conversation's first ~32 KB.
const FULL_TEXT_MAX: usize = 32 * 1024;
/// How many leading JSONL records are JSON-parsed for full-text extraction. Records past
/// this are only counted, never parsed — a second bound (besides [`FULL_TEXT_MAX`]) so a
/// transcript that is mostly tool traffic (little message text per record) stays cheap.
const FULL_TEXT_SCAN_LINES: usize = 2000;

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
    /// Searchable full text of the conversation: the text of user AND assistant messages
    /// (in order, space-joined), **lowercased** at extraction so substring search never
    /// re-lowers it, and capped at [`FULL_TEXT_MAX`] bytes / [`FULL_TEXT_SCAN_LINES`]
    /// records so a huge transcript stays bounded. Used by [`session_matches`] as the
    /// slow path when the summary / opening prompt didn't match.
    pub full_text: String,
}

/// Does `session` match the (case-insensitive, substring) search `query`? An empty/blank
/// query matches everything. Fast path: the summary and the first user message (cheap,
/// always present). Slow path: the cached [`ClaudeSession::full_text`] — the bounded
/// extract of the whole conversation's user+assistant text — so search reaches *inside*
/// a transcript, not just its label.
pub fn session_matches(session: &ClaudeSession, query: &str) -> bool {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return true;
    }
    if session.summary.to_lowercase().contains(&q) || session.first_user.to_lowercase().contains(&q)
    {
        return true;
    }
    // full_text is stored lowercased, so this is a plain substring scan.
    session.full_text.contains(&q)
}

/// Filter `sessions` down to those matching `query` (see [`session_matches`]), preserving
/// order. An empty/blank query returns the full list.
pub fn filter_sessions(sessions: &[ClaudeSession], query: &str) -> Vec<ClaudeSession> {
    sessions
        .iter()
        .filter(|s| session_matches(s, query))
        .cloned()
        .collect()
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

/// Every `projects/` transcript store to search, across all Claude accounts. `claude` writes
/// transcripts under `$CLAUDE_CONFIG_DIR/projects`, so a session run under a rotated/non-default
/// account (the goals system rotates them; a user may export `CLAUDE_CONFIG_DIR` too) lives in
/// that account's store, NOT `~/.claude/projects`. Union of: `$CLAUDE_CONFIG_DIR` (if set), every
/// registered account ([`crate::claude_accounts::config_dirs`]), and the default `~/.claude` —
/// deduped, order-preserved (default first). Empty only when no home dir is known.
pub fn claude_projects_roots() -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    let push = |dir: PathBuf, roots: &mut Vec<PathBuf>| {
        let p = dir.join("projects");
        if !roots.contains(&p) {
            roots.push(p);
        }
    };
    if let Some(root) = claude_projects_root() {
        roots.push(root); // ~/.claude/projects first (the primary account)
    }
    for dir in crate::claude_accounts::config_dirs() {
        push(dir, &mut roots);
    }
    if let Some(cfg) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|c| !c.is_empty()) {
        push(PathBuf::from(cfg), &mut roots);
    }
    roots
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
        if let Some(s) = read_session_file(&entry.path()) {
            out.push(s);
        }
    }
    sort_newest_first(&mut out);
    out
}

/// Read one `*.jsonl` transcript into a [`ClaudeSession`] (id from the filename stem,
/// `started_at` from the file mtime, summary/first-user/full-text from a bounded parse).
/// `None` for non-`.jsonl` paths or a stem-less filename — the per-file seam shared by
/// [`sessions_in_dir`] and the incremental [`SessionCache`].
pub fn read_session_file(path: &Path) -> Option<ClaudeSession> {
    if !path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
    {
        return None;
    }
    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned())?;
    let started_at = file_fingerprint(path).map(|(mtime, _)| mtime);
    let (summary, first_user, full_text, message_count) = summarize_file(path);
    Some(ClaudeSession {
        id,
        source: HistorySource::Claude,
        started_at,
        summary,
        first_user,
        message_count,
        full_text,
    })
}

/// `(mtime epoch ms, size bytes)` for `path`, or `None` when stat fails. The change
/// fingerprint the [`SessionCache`] keys re-scans on.
fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let m = fs::metadata(path).ok()?;
    let mtime = m
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)?;
    Some((mtime, m.len()))
}

/// Order sessions newest-first by `started_at`, breaking ties by id (descending) so the
/// result is deterministic even when mtimes are equal.
fn sort_newest_first(v: &mut [ClaudeSession]) {
    v.sort_by(|a, b| {
        b.started_at
            .cmp(&a.started_at)
            .then_with(|| b.id.cmp(&a.id))
    });
}

/// Read `path` once: count every record (cheaply) and, within a bounded prefix, recover
/// (a) a summary — preferring a `summary`-type record, else the first `user` message's
/// text — scanned over the first [`SUMMARY_SCAN_LINES`] records; (b) the first user
/// message itself (kept for search); and (c) the lowercased full-conversation text of
/// user + assistant messages, capped at [`FULL_TEXT_MAX`] bytes and
/// [`FULL_TEXT_SCAN_LINES`] records. Records past both bounds are counted blind, never
/// JSON-parsed. Returns `(summary, first_user, full_text, line_count)`.
fn summarize_file(path: &Path) -> (String, String, String, usize) {
    use std::io::{BufRead, BufReader};
    let Ok(file) = fs::File::open(path) else {
        return (String::new(), String::new(), String::new(), 0);
    };
    let reader = BufReader::new(file);
    let mut count = 0usize;
    let mut summary: Option<String> = None;
    let mut first_user: Option<String> = None;
    let mut full = String::new();

    for (i, line) in reader.lines().enumerate() {
        let Ok(line) = line else { break };
        count += 1;
        let summary_done = i >= SUMMARY_SCAN_LINES || (summary.is_some() && first_user.is_some());
        let full_done = i >= FULL_TEXT_SCAN_LINES || full.len() >= FULL_TEXT_MAX;
        // Only the bounded prefix is JSON-parsed; the rest is counted blind.
        if summary_done && full_done {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("summary") if !summary_done => {
                if let Some(s) = v.get("summary").and_then(|s| s.as_str()) {
                    if !s.trim().is_empty() {
                        summary = Some(clean_summary(s));
                    }
                }
            }
            Some("user") => {
                if let Some(text) = message_text(&v) {
                    if !text.trim().is_empty() {
                        if first_user.is_none() && !summary_done {
                            first_user = Some(clean_summary(&text));
                        }
                        if !full_done {
                            push_full_text(&mut full, &text);
                        }
                    }
                }
            }
            Some("assistant") if !full_done => {
                if let Some(text) = message_text(&v) {
                    push_full_text(&mut full, &text);
                }
            }
            _ => {}
        }
    }

    let first_user = first_user.unwrap_or_default();
    (
        summary.unwrap_or_else(|| first_user.clone()),
        first_user,
        full,
        count,
    )
}

/// Append one message's text to the accumulated full-text extract: whitespace-collapsed,
/// lowercased (search is case-insensitive and never re-lowers the stored text), space-
/// separated from the previous message, and truncated (char-boundary safe) once the
/// [`FULL_TEXT_MAX`] cap is reached.
fn push_full_text(full: &mut String, text: &str) {
    if full.len() >= FULL_TEXT_MAX {
        return;
    }
    let remaining = FULL_TEXT_MAX - full.len();
    if !full.is_empty() {
        full.push(' ');
    }
    let collapsed = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if collapsed.len() <= remaining {
        full.push_str(&collapsed);
    } else {
        // Cut at the last char boundary at or below the byte budget.
        let mut cut = remaining;
        while cut > 0 && !collapsed.is_char_boundary(cut) {
            cut -= 1;
        }
        full.push_str(&collapsed[..cut]);
    }
}

/// Pull the human-visible text out of a `user`/`assistant` record's `message.content`,
/// which is either a plain string or an array of content blocks (all `{"type":"text"}`
/// blocks, space-joined — tool_use / tool_result blocks are skipped). Returns `None` for
/// tool-traffic-only messages with no text.
fn message_text(v: &serde_json::Value) -> Option<String> {
    let content = v.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(t);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
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

// ===== incremental, fingerprint-keyed session cache =====

/// An incremental cache over one or more session directories: each transcript is keyed by
/// its `(mtime, size)` fingerprint and re-parsed **only** when that fingerprint changes
/// (or the file is new). Deleted files drop out on the next scan. Designed to live on a
/// long-running background scanner thread, so repeated "refresh this project" requests
/// cost one `read_dir` + a stat per file — not a re-parse of every transcript.
#[derive(Default)]
pub struct SessionCache {
    files: HashMap<PathBuf, CachedFile>,
    /// How many files the most recent [`SessionCache::scan_dir`] actually (re-)parsed —
    /// observable cache effectiveness, used by tests to prove unchanged files are reused.
    last_scan_parsed: usize,
}

struct CachedFile {
    mtime_ms: u64,
    size: u64,
    session: ClaudeSession,
}

impl SessionCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Files (re-)parsed by the most recent [`scan_dir`](Self::scan_dir) call.
    pub fn last_scan_parsed(&self) -> usize {
        self.last_scan_parsed
    }

    /// Scan `project_root`'s sessions through the real `~/.claude/projects` (the
    /// non-test entry point; see [`scan_dir`](Self::scan_dir) for the seam).
    pub fn scan_project(&mut self, project_root: &Path) -> Vec<ClaudeSession> {
        let Some(root) = claude_projects_root() else {
            return Vec::new();
        };
        self.scan_dir(&root.join(encode_project_dir(project_root)))
    }

    /// Scan `project_root`'s sessions across EVERY account's transcript store
    /// ([`claude_projects_roots`]), merged newest-first. A session run under a rotated or
    /// non-default `CLAUDE_CONFIG_DIR` lives in that account's `projects/`, not `~/.claude`,
    /// so the resume/history browser must union them. Session ids are unique across accounts,
    /// so the merge needs no dedup. One cache backs all dirs (files keyed by full path), so
    /// each account's unchanged transcripts stay warm across scans.
    pub fn scan_project_all(&mut self, project_root: &Path) -> Vec<ClaudeSession> {
        let encoded = encode_project_dir(project_root);
        let mut out: Vec<ClaudeSession> = Vec::new();
        let mut parsed = 0usize;
        for root in claude_projects_roots() {
            out.extend(self.scan_dir(&root.join(&encoded)));
            parsed += self.last_scan_parsed;
        }
        self.last_scan_parsed = parsed;
        sort_newest_first(&mut out);
        out
    }

    /// List `session_dir`, re-parsing only new/changed transcripts (by mtime+size) and
    /// reusing cached [`ClaudeSession`]s for the rest; entries whose file vanished are
    /// dropped. Returns the directory's sessions newest-first. A missing/unreadable
    /// directory yields an empty list (and evicts that directory's cached files).
    pub fn scan_dir(&mut self, session_dir: &Path) -> Vec<ClaudeSession> {
        self.last_scan_parsed = 0;
        let mut seen: Vec<PathBuf> = Vec::new();
        let mut out: Vec<ClaudeSession> = Vec::new();
        if let Ok(entries) = fs::read_dir(session_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
                {
                    continue;
                }
                let fp = file_fingerprint(&path);
                let fresh = match (self.files.get(&path), fp) {
                    (Some(c), Some((mtime, size))) => c.mtime_ms == mtime && c.size == size,
                    _ => false,
                };
                if !fresh {
                    let Some(session) = read_session_file(&path) else {
                        continue;
                    };
                    self.last_scan_parsed += 1;
                    let (mtime_ms, size) = fp.unwrap_or((0, 0));
                    self.files.insert(
                        path.clone(),
                        CachedFile {
                            mtime_ms,
                            size,
                            session,
                        },
                    );
                }
                if let Some(c) = self.files.get(&path) {
                    out.push(c.session.clone());
                    seen.push(path);
                }
            }
        }
        // Evict cached entries under `session_dir` whose file no longer exists.
        let seen: std::collections::HashSet<&PathBuf> = seen.iter().collect();
        self.files
            .retain(|p, _| !p.starts_with(session_dir) || seen.contains(p));
        sort_newest_first(&mut out);
        out
    }
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
        assert_eq!(
            encode_path_str("/home/me/my_repo dir"),
            "-home-me-my-repo-dir"
        );
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
            full_text: String::new(),
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
        let v = vec![
            sess("a", None, "Fix the sidebar", ""),
            sess("b", None, "", ""),
        ];
        assert_eq!(filter_sessions(&v, "").len(), 2);
        assert_eq!(filter_sessions(&v, "   ").len(), 2);
    }

    #[test]
    fn filter_is_case_insensitive_substring_over_summary() {
        let v = vec![
            sess(
                "a",
                None,
                "Fix the Sidebar layout",
                "fix the sidebar layout",
            ),
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
        let v = vec![sess(
            "a",
            None,
            "Ported the worktree tree",
            "please refactor the parser",
        )];
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
        let ids: Vec<String> = filter_sessions(&v, "alpha")
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["first", "third"]);
    }

    // ---- full-conversation search (#1) ----

    /// A transcript: opening prompt, then an assistant answer, then a later user message —
    /// only the opening prompt is in summary/first_user; the rest only in full_text.
    fn write_convo(dir: &Path, name: &str) {
        std::fs::write(
            dir.join(name),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"fix the sidebar\"}}\n\
             {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"The RefCell reborrow was the culprit\"},{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"x\",\"input\":{}}]}}\n\
             {\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"content\":\"secret-tool-output\"},{\"type\":\"text\",\"text\":\"now add Zanzibar telemetry\"}]}}\n",
        )
        .unwrap();
    }

    #[test]
    fn full_text_match_hits_inside_the_conversation() {
        let dir = temp_dir("fulltext");
        write_convo(&dir, "s.jsonl");
        let sessions = sessions_in_dir(&dir);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];

        // Fast path still works (summary / opening prompt).
        assert!(session_matches(s, "sidebar"));
        // Assistant text, mid-conversation — only reachable via full_text (#1).
        assert!(
            session_matches(s, "RefCell REBORROW"),
            "assistant text should match"
        );
        // A later user message, past the first one.
        assert!(
            session_matches(s, "zanzibar"),
            "later user text should match"
        );
        // Miss: not in the conversation at all.
        assert!(
            !session_matches(s, "kubernetes"),
            "absent term must not match"
        );
        // Tool traffic (tool_result content) is NOT searchable text.
        assert!(!session_matches(s, "secret-tool-output"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_text_is_capped() {
        let dir = temp_dir("fullcap");
        // One huge user message, far past the cap.
        let big = format!(
            "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":\"{}\"}}}}\n",
            "word ".repeat(FULL_TEXT_MAX / 2)
        );
        std::fs::write(dir.join("big.jsonl"), big).unwrap();
        let sessions = sessions_in_dir(&dir);
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].full_text.len() <= FULL_TEXT_MAX);
        assert!(sessions[0].full_text.starts_with("word word"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- SessionCache: fingerprint-keyed incremental re-scan (#6) ----

    #[test]
    fn cache_reuses_unchanged_files_and_reparses_on_change() {
        let dir = temp_dir("cache");
        write_convo(&dir, "a.jsonl");
        std::fs::write(
            dir.join("b.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .unwrap();

        let mut cache = SessionCache::new();
        let first = cache.scan_dir(&dir);
        assert_eq!(first.len(), 2);
        assert_eq!(cache.last_scan_parsed(), 2, "cold scan parses everything");

        // Unchanged: a re-scan parses nothing and returns the same rows.
        let second = cache.scan_dir(&dir);
        assert_eq!(second, first);
        assert_eq!(
            cache.last_scan_parsed(),
            0,
            "warm scan must reuse the cache"
        );

        // Touch one file: append a record AND bump its mtime well past the original
        // (size + mtime both move — either alone must invalidate, jointly they must).
        let b = dir.join("b.jsonl");
        let mut content = std::fs::read_to_string(&b).unwrap();
        content.push_str(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"again\"}}\n",
        );
        std::fs::write(&b, content).unwrap();
        let f = std::fs::File::options().write(true).open(&b).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(f);

        let third = cache.scan_dir(&dir);
        assert_eq!(
            cache.last_scan_parsed(),
            1,
            "only the changed file re-parses"
        );
        let b_row = third.iter().find(|s| s.id == "b").unwrap();
        assert_eq!(
            b_row.message_count, 2,
            "the re-parse saw the appended record"
        );
        assert!(
            session_matches(b_row, "again"),
            "new text is searchable after the bump"
        );

        // mtime bump alone (same size, same content) also invalidates.
        let a = dir.join("a.jsonl");
        let f = std::fs::File::options().write(true).open(&a).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60))
            .unwrap();
        drop(f);
        cache.scan_dir(&dir);
        assert_eq!(
            cache.last_scan_parsed(),
            1,
            "an mtime-only bump re-parses that file"
        );

        // Deleting a file drops it from the next scan.
        std::fs::remove_file(&b).unwrap();
        let after_delete = cache.scan_dir(&dir);
        assert_eq!(after_delete.len(), 1);
        assert_eq!(after_delete[0].id, "a");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_missing_dir_is_empty() {
        let mut cache = SessionCache::new();
        let missing = std::env::temp_dir().join(format!("hp-claude-cm-{}", uuid::Uuid::new_v4()));
        assert!(cache.scan_dir(&missing).is_empty());
        assert_eq!(cache.last_scan_parsed(), 0);
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
