//! Background scanner for the sidebar's expensive enumerations (#6): git worktree
//! listing and Claude session-history reads, both of which used to run synchronously on
//! the UI thread inside the render projection. One dedicated thread (the simpler
//! `update.rs` thread+snapshot pattern, not the full ambient-AI tokio engine) services
//! scan jobs over std mpsc channels; the UI pump drains finished results each tick and
//! folds them into the sidebar's caches, marking the state dirty so the projection
//! re-runs with fresh rows.
//!
//! The session side rides core's [`SessionCache`]: the thread keeps one cache per
//! project, so a refresh request re-parses only transcripts whose mtime/size changed —
//! repeated flyout opens cost a `read_dir` + stats, not a re-read of every `.jsonl`.
//!
//! UI-side, a pending set per job kind dedupes requests: the projection may ask for the
//! same project every dirty tick while a scan is in flight, and only the first ask
//! enqueues a job.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};

use hyperpanes_core::claude_history::{ClaudeSession, SessionCache};

use crate::sidebar::{self, WorktreeRow};

/// One scan request, sent UI → scanner thread.
enum Job {
    /// Re-scan `~/.claude/projects/<encoded>/*.jsonl` for this project root.
    Sessions(String),
    /// Re-run `git worktree list --porcelain` in this repo.
    Worktrees(String),
}

/// One finished scan, sent scanner thread → UI (drained by [`drain`]).
enum ScanResult {
    Sessions(String, Vec<ClaudeSession>),
    Worktrees(String, Vec<WorktreeRow>),
}

/// The UI-thread handle: the job sender plus the result receiver the pump drains.
struct Scanner {
    tx: Sender<Job>,
    rx: Receiver<ScanResult>,
}

thread_local! {
    /// The scanner handle, spawned lazily on first use (UI thread only).
    static SCANNER: Scanner = spawn_scanner();
    /// Project roots with a session scan in flight — dedupes per-tick re-requests.
    static PENDING_SESS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    /// Repo paths with a worktree scan in flight.
    static PENDING_WT: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/// Spawn the scanner thread and return its UI-side handle. The thread owns one
/// [`SessionCache`] per project (the mtime/size fingerprints live there, across
/// flyout open/close cycles) and exits when the UI side drops the channels.
fn spawn_scanner() -> Scanner {
    let (job_tx, job_rx) = channel::<Job>();
    let (res_tx, res_rx) = channel::<ScanResult>();
    std::thread::Builder::new()
        .name("history-scan".to_string())
        .spawn(move || {
            let mut caches: HashMap<String, SessionCache> = HashMap::new();
            while let Ok(job) = job_rx.recv() {
                let res = match job {
                    Job::Sessions(root) => {
                        let cache = caches.entry(root.clone()).or_default();
                        // Union every account's transcript store: a session run under a
                        // rotated/non-default CLAUDE_CONFIG_DIR lives in that account's
                        // projects/, not ~/.claude (multi-account resume/history).
                        let sessions = cache.scan_project_all(Path::new(&root));
                        ScanResult::Sessions(root, sessions)
                    }
                    Job::Worktrees(repo) => {
                        let rows = sidebar::enumerate_worktrees(&repo);
                        ScanResult::Worktrees(repo, rows)
                    }
                };
                if res_tx.send(res).is_err() {
                    break;
                }
            }
        })
        .ok();
    Scanner {
        tx: job_tx,
        rx: res_rx,
    }
}

/// Ask for a (re-)scan of `project_root`'s Claude sessions. No-op while one is already
/// in flight for that project.
pub fn request_sessions(project_root: &str) {
    let fresh = PENDING_SESS.with(|p| p.borrow_mut().insert(project_root.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::Sessions(project_root.to_string()));
        });
    }
}

/// Ask for a (re-)enumeration of `repo_path`'s worktrees. No-op while one is in flight.
pub fn request_worktrees(repo_path: &str) {
    let fresh = PENDING_WT.with(|p| p.borrow_mut().insert(repo_path.to_string()));
    if fresh {
        SCANNER.with(|s| {
            let _ = s.tx.send(Job::Worktrees(repo_path.to_string()));
        });
    }
}

/// Drain every finished scan into the sidebar caches. Returns `true` when anything
/// landed — the caller (the pump) marks the state dirty so the projection re-runs and
/// the flyout refreshes. Called every tick; an empty channel is a cheap `try_recv` miss.
pub fn drain() -> bool {
    let mut any = false;
    SCANNER.with(|s| {
        while let Ok(res) = s.rx.try_recv() {
            any = true;
            match res {
                ScanResult::Sessions(root, sessions) => {
                    PENDING_SESS.with(|p| {
                        p.borrow_mut().remove(&root);
                    });
                    sidebar::apply_sessions(&root, sessions);
                }
                ScanResult::Worktrees(repo, rows) => {
                    PENDING_WT.with(|p| {
                        p.borrow_mut().remove(&repo);
                    });
                    sidebar::apply_worktrees(&repo, rows);
                }
            }
        }
    });
    any
}
