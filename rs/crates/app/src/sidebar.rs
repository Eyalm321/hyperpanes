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
    /// The flyout's last-seen open state, so the projection can detect the closed→open edge
    /// and refresh the cache (worktrees may have changed while the flyout was shut).
    static WT_LAST_OPEN: RefCell<bool> = const { RefCell::new(false) };
}

/// Note the flyout's current open state; on the closed→open transition, clear the cache so
/// the next render re-enumerates fresh. Called from the projection each tick.
pub fn note_flyout_open(open: bool) {
    WT_LAST_OPEN.with(|last| {
        let mut last = last.borrow_mut();
        if open && !*last {
            WT_CACHE.with(|c| c.borrow_mut().clear());
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
}
