//! Sidebar / projects — Wave-2 feature plugging into **Seam #1** (state) + the
//! `core::persistence::projects` history.
//!
//! A toggleable side panel listing the git projects the app remembers (newest-first),
//! fed by pane cwd → enclosing git root → `upsert_project_by_root`. The native port of
//! `components/Sidebar.tsx` + `store/useProjects.ts`: the canonical list lives in
//! `projects.json` (owned by core); the panel renders a cached copy refreshed whenever
//! it opens or a pane reports a new cwd. Selecting a project opens a fresh pane cd'd
//! into its repo.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use hyperpanes_core::persistence::projects::Project;
use hyperpanes_core::persistence::projects;

/// The remembered projects, newest-first (the order the panel renders), self-healed:
/// any project whose repo folder no longer exists on disk is forgotten (removed from
/// `projects.json`) and dropped from the result. Done app-side via the existing core
/// `remove_project`, so a deleted/moved repo silently disappears from the rail.
pub fn list() -> Vec<Project> {
    let all = projects::list_projects();
    let mut kept = Vec::with_capacity(all.len());
    for p in all {
        if Path::new(&p.path).is_dir() {
            kept.push(p);
        } else {
            projects::remove_project(&p.id);
        }
    }
    kept
}

/// Walk up from `cwd` looking for the nearest ancestor that contains a `.git` entry,
/// returning that directory as the git root. Mirrors what the Electron main process
/// did before calling `upsertProjectByRoot`. `None` when `cwd` isn't inside a repo.
pub fn git_root_of(cwd: &str) -> Option<PathBuf> {
    let mut dir: Option<&Path> = Some(Path::new(cwd));
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

// ===== git worktrees =====
//
// The second level of the sidebar tree: each remembered project's git worktrees, listed
// via `git worktree list --porcelain` run in the repo `path`, and removable via
// `git worktree remove -- "<path>"`. All self-contained here so the sidebar feature doesn't
// reach into the central `State` (a parallel track owns that).

/// One worktree of a project's repo. `branch` is a human label (the short branch name, or
/// `(detached)` / `(bare)`); `is_main` marks the main checkout — `git worktree remove`
/// refuses it, so its trash affordance is shown disabled. `locked` mirrors the porcelain
/// `locked` flag: git also refuses a plain (`--force`-less) remove of a locked worktree, so
/// its trash is shown disabled too (rather than letting the user click into a swallowed
/// error). `prunable` mirrors the porcelain `prunable` flag (the working dir is missing /
/// the worktree is stale) and is surfaced purely as an annotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeRow {
    pub path: String,
    pub branch: String,
    pub is_main: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// Spawn a child process without flashing a console window — the same `CREATE_NO_WINDOW`
/// trick `core::paths` uses (its trait is private, so a tiny local copy lives here). On a
/// GUI app each `git` spawn would otherwise briefly pop a console on Windows.
trait NoWindow {
    fn no_window(&mut self) -> &mut Self;
}
impl NoWindow for Command {
    #[cfg(windows)]
    fn no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        self.creation_flags(CREATE_NO_WINDOW)
    }
    #[cfg(not(windows))]
    fn no_window(&mut self) -> &mut Self {
        self
    }
}

/// Turn a `branch` porcelain value (`refs/heads/feature`) into a short display label.
fn short_branch(refname: &str) -> String {
    refname.strip_prefix("refs/heads/").unwrap_or(refname).to_string()
}

/// Parse the `git worktree list --porcelain` output into rows. Records are separated by a
/// blank line; each starts with `worktree <path>` and may carry `HEAD <sha>`, `branch
/// <ref>`, `bare`, `detached`, `locked`, `prunable`. The FIRST record is always the main
/// working tree (git lists it first), so we flag it `is_main`.
fn parse_porcelain(out: &str) -> Vec<WorktreeRow> {
    let mut rows = Vec::new();
    let mut path: Option<String> = None;
    let mut head: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut detached = false;
    let mut bare = false;
    let mut locked = false;
    let mut prunable = false;

    // Flush the record accumulated so far (if any) into a row.
    let mut flush = |path: &mut Option<String>,
                     head: &mut Option<String>,
                     branch: &mut Option<String>,
                     detached: &mut bool,
                     bare: &mut bool,
                     locked: &mut bool,
                     prunable: &mut bool,
                     rows: &mut Vec<WorktreeRow>| {
        if let Some(p) = path.take() {
            let label = if *bare {
                "(bare)".to_string()
            } else if let Some(b) = branch.take() {
                short_branch(&b)
            } else if *detached {
                match head.take() {
                    Some(h) => format!("(detached {})", &h[..h.len().min(8)]),
                    None => "(detached)".to_string(),
                }
            } else {
                "(no branch)".to_string()
            };
            let is_main = rows.is_empty();
            rows.push(WorktreeRow {
                path: p,
                branch: label,
                is_main,
                locked: *locked,
                prunable: *prunable,
            });
        }
        *head = None;
        *branch = None;
        *detached = false;
        *bare = false;
        *locked = false;
        *prunable = false;
    };

    for line in out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            flush(&mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut locked, &mut prunable, &mut rows);
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            // A new record begins — flush the previous one first.
            flush(&mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut locked, &mut prunable, &mut rows);
            path = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("HEAD ") {
            head = Some(rest.to_string());
        } else if let Some(rest) = line.strip_prefix("branch ") {
            branch = Some(rest.to_string());
        } else if line == "detached" {
            detached = true;
        } else if line == "bare" {
            bare = true;
        } else if line == "locked" || line.starts_with("locked ") {
            // `locked` or `locked <reason>` — git refuses a plain `worktree remove` either way.
            locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            // `prunable` or `prunable <reason>` — the working tree is stale / its dir is gone.
            prunable = true;
        }
    }
    flush(&mut path, &mut head, &mut branch, &mut detached, &mut bare, &mut locked, &mut prunable, &mut rows);
    rows
}

/// Run `git worktree list --porcelain` in `repo_path` and parse the result. Any failure
/// (no git, not a repo) yields an empty list — the project simply shows no worktrees.
fn enumerate_worktrees(repo_path: &str) -> Vec<WorktreeRow> {
    let Ok(out) = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_path)
        .no_window()
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    parse_porcelain(&String::from_utf8_lossy(&out.stdout))
}

/// Remove the worktree at `path` via `git worktree remove -- "<path>"`, run from that repo's
/// main checkout (`-C` not needed — git resolves the worktree from the path). Plain remove
/// (no `--force`): git refuses dirty/locked worktrees and the error is surfaced. The `--`
/// end-of-options sentinel is mandatory: a worktree path beginning with `-` (or any
/// `-`-prefixed token git would otherwise read as a flag) must be treated as a positional
/// path, never an option. Returns the trimmed stderr on failure.
pub fn remove_worktree(repo_path: &str, worktree_path: &str) -> Result<(), String> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--", worktree_path])
        .current_dir(repo_path)
        .no_window()
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let msg = err.trim();
    Err(if msg.is_empty() { "git worktree remove failed".to_string() } else { msg.to_string() })
}

thread_local! {
    /// Per-UI-thread cache of each project's worktree list, keyed by repo path. Render runs
    /// every tick, so enumerating git on each pass would be wasteful — instead we enumerate
    /// once per project and reuse it until invalidated (a delete) or the flyout is reopened.
    static WT_CACHE: RefCell<HashMap<String, Vec<WorktreeRow>>> = RefCell::new(HashMap::new());
    /// Per-UI-thread cache of each project's Claude session history, keyed by repo path. Same
    /// rationale as `WT_CACHE`: scanning `~/.claude/projects` every tick would be wasteful, so
    /// we read it once per project and reuse until the flyout is reopened.
    static CLAUDE_CACHE: RefCell<HashMap<String, Vec<ClaudeSessionRow>>> = RefCell::new(HashMap::new());
    /// The flyout's last-seen open state, so the projection can detect the closed→open edge
    /// and refresh the cache (worktrees may have changed while the flyout was shut).
    static WT_LAST_OPEN: RefCell<bool> = const { RefCell::new(false) };
}

/// Note the flyout's current open state; on the closed→open transition, clear the caches so
/// the next render re-enumerates fresh. Called from the projection each tick.
pub fn note_flyout_open(open: bool) {
    WT_LAST_OPEN.with(|last| {
        let mut last = last.borrow_mut();
        if open && !*last {
            WT_CACHE.with(|c| c.borrow_mut().clear());
            CLAUDE_CACHE.with(|c| c.borrow_mut().clear());
        }
        *last = open;
    });
}

/// The worktrees of the repo at `repo_path`, served from the cache (enumerated on first
/// miss). Cheap to call every render.
pub fn worktrees_for(repo_path: &str) -> Vec<WorktreeRow> {
    WT_CACHE.with(|c| {
        if let Some(rows) = c.borrow().get(repo_path) {
            return rows.clone();
        }
        let rows = enumerate_worktrees(repo_path);
        c.borrow_mut().insert(repo_path.to_string(), rows.clone());
        rows
    })
}

/// Drop `repo_path`'s cached worktrees so the next render re-enumerates (used after a
/// successful removal so the tree reflects the change immediately).
pub fn invalidate(repo_path: &str) {
    WT_CACHE.with(|c| {
        c.borrow_mut().remove(repo_path);
    });
}

// ===== Claude Code session history =====
//
// The third level of the sidebar tree (under each project, alongside its worktrees): the
// project's recent Claude Code sessions, read from `~/.claude/projects/<ENCODED_CWD>/*.jsonl`
// by the pure `core::claude_history` reader. This layer adds only UI-presentation shaping (a
// relative-time label + a small per-project cap) and a thread-local cache mirroring the
// worktree cache above.
//
// `#[allow(dead_code)]`: these are the READY fan-in seam. `claude_sessions_for` is called by
// the projection (paneview.rs) once `ProjectItem` gains a `sessions` field, and
// `claude_resume_command` by the resume-session callback handler (app.rs) — both off-limits to
// this parallel track. Until those two ~1-line hops land, the items are exercised only by the
// unit tests below, so the binary build would otherwise flag them unused.

/// How many recent sessions to surface per project in the rail (the most-recent N).
pub const CLAUDE_HISTORY_LIMIT: usize = 8;

/// One Claude session row, shaped for the sidebar: the resume id, a one-line summary, a
/// human relative-time string ("2h ago") and the message count. Built from
/// [`hyperpanes_core::claude_history::ClaudeSession`] with the timestamp turned into a label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeSessionRow {
    pub id: String,
    pub summary: String,
    pub when: String,
    pub count: i32,
}

#[allow(dead_code)]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// First 8 chars of a session id (the UUID head) — a fallback label when a transcript has no
/// summary line nor a first user message.
#[allow(dead_code)]
fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

/// A compact relative-time label for `started_at` (epoch ms) vs `now` (epoch ms). Empty when
/// the timestamp is unknown. Coarse buckets — minutes → hours → days → weeks → months → years.
#[allow(dead_code)]
fn relative_time(started_at: Option<u64>, now: u64) -> String {
    let Some(t) = started_at else { return String::new() };
    let secs = now.saturating_sub(t) / 1000;
    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    if days < 7 {
        return format!("{days}d ago");
    }
    let weeks = days / 7;
    if weeks < 5 {
        return format!("{weeks}w ago");
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months}mo ago");
    }
    format!("{}y ago", days / 365)
}

/// The recent Claude sessions for `project_root`, served from the cache (read on first miss).
/// Cheap to call every render; capped to [`CLAUDE_HISTORY_LIMIT`] newest-first rows.
#[allow(dead_code)] // called by the projection (paneview.rs) at fan-in; see section header.
pub fn claude_sessions_for(project_root: &str) -> Vec<ClaudeSessionRow> {
    CLAUDE_CACHE.with(|c| {
        if let Some(rows) = c.borrow().get(project_root) {
            return rows.clone();
        }
        let now = now_ms();
        let rows: Vec<ClaudeSessionRow> =
            hyperpanes_core::claude_history::sessions_for_project(Path::new(project_root))
                .into_iter()
                .take(CLAUDE_HISTORY_LIMIT)
                .map(|s| ClaudeSessionRow {
                    when: relative_time(s.started_at, now),
                    count: s.message_count.min(i32::MAX as usize) as i32,
                    summary: if s.summary.is_empty() {
                        format!("session {}", short_id(&s.id))
                    } else {
                        s.summary
                    },
                    id: s.id,
                })
                .collect();
        c.borrow_mut().insert(project_root.to_string(), rows.clone());
        rows
    })
}

/// The shell command that resumes a Claude session in a fresh pane: `claude --resume <id>`.
/// Session ids are UUIDs (hex + `-`), so no shell-quoting is required. The caller spawns this
/// via the existing New-Pane path (`State::add_pane_opts` with `command` + the project `cwd`).
#[allow(dead_code)] // called by the resume-session handler (app.rs) at fan-in; see header.
pub fn claude_resume_command(session_id: &str) -> String {
    format!("claude --resume {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_main_and_linked_worktrees() {
        let out = "\
worktree /home/u/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/u/repo-feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature/x

worktree /home/u/repo-detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let rows = parse_porcelain(out);
        assert_eq!(rows.len(), 3);
        assert!(rows[0].is_main);
        assert_eq!(rows[0].branch, "main");
        assert!(!rows[1].is_main);
        assert_eq!(rows[1].branch, "feature/x");
        assert_eq!(rows[1].path, "/home/u/repo-feature");
        assert_eq!(rows[2].branch, "(detached 33333333)");
    }

    #[test]
    fn parses_bare_main() {
        let out = "\
worktree /home/u/bare-repo
bare

worktree /home/u/bare-repo-wt
HEAD 4444444444444444444444444444444444444444
branch refs/heads/dev
";
        let rows = parse_porcelain(out);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_main);
        assert_eq!(rows[0].branch, "(bare)");
        assert_eq!(rows[1].branch, "dev");
    }

    #[test]
    fn parses_locked_and_prunable_flags() {
        // `locked`/`prunable` appear with or without a trailing reason.
        let out = "\
worktree /home/u/repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /home/u/repo-locked
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feature
locked being-worked-on

worktree /home/u/repo-prunable
HEAD 3333333333333333333333333333333333333333
branch refs/heads/old
prunable
";
        let rows = parse_porcelain(out);
        assert_eq!(rows.len(), 3);
        // main carries neither flag
        assert!(!rows[0].locked && !rows[0].prunable);
        // locked (with reason) → locked, not prunable
        assert!(rows[1].locked && !rows[1].prunable);
        // prunable (bare keyword) → prunable, not locked
        assert!(!rows[2].locked && rows[2].prunable);
    }

    #[test]
    fn empty_output_is_empty() {
        assert!(parse_porcelain("").is_empty());
        assert!(parse_porcelain("\n\n").is_empty());
    }

    #[test]
    fn finds_git_root_at_self() {
        let tmp = std::env::temp_dir().join(format!("hp-sb-root-{}", std::process::id()));
        let repo = tmp.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let found = git_root_of(&nested.to_string_lossy()).unwrap();
        assert_eq!(found, repo);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn no_repo_is_none() {
        // The system temp dir itself is (almost certainly) not a git repo.
        let outside = std::env::temp_dir().join(format!("hp-sb-nope-{}", std::process::id()));
        std::fs::create_dir_all(&outside).unwrap();
        assert!(git_root_of(&outside.to_string_lossy()).is_none());
        let _ = std::fs::remove_dir_all(&outside);
    }

    // ---- Claude session history shaping ----

    #[test]
    fn relative_time_buckets() {
        let now = 1_000_000_000_000u64; // arbitrary epoch ms
        assert_eq!(relative_time(None, now), "");
        assert_eq!(relative_time(Some(now), now), "just now");
        assert_eq!(relative_time(Some(now - 90_000), now), "1m ago"); // 90s
        assert_eq!(relative_time(Some(now - 3 * 3_600_000), now), "3h ago");
        assert_eq!(relative_time(Some(now - 2 * 86_400_000), now), "2d ago");
        assert_eq!(relative_time(Some(now - 3 * 7 * 86_400_000), now), "3w ago");
        // A timestamp in the future (clock skew) saturates to "just now", never underflows.
        assert_eq!(relative_time(Some(now + 5000), now), "just now");
    }

    #[test]
    fn resume_command_is_claude_resume() {
        assert_eq!(
            claude_resume_command("0517332c-4987-439d-b154-6ec67856fdb3"),
            "claude --resume 0517332c-4987-439d-b154-6ec67856fdb3"
        );
    }

    #[test]
    fn short_id_takes_uuid_head() {
        assert_eq!(short_id("0517332c-4987-439d"), "0517332c");
        assert_eq!(short_id("abc"), "abc");
    }

    #[test]
    fn sessions_for_missing_project_is_empty_and_caches() {
        // A path with no encoded `~/.claude/projects` dir → empty, and a second call hits the
        // cache (same result) without panicking.
        let bogus = std::env::temp_dir()
            .join(format!("hp-claude-bogus-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        assert!(claude_sessions_for(&bogus).is_empty());
        assert!(claude_sessions_for(&bogus).is_empty());
    }
}
