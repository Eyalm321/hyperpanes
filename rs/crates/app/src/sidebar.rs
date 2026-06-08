//! Sidebar / projects — Wave-2 feature plugging into **Seam #1** (state) + the
//! `core::persistence::projects` history.
//!
//! A toggleable side panel listing the git projects the app remembers (newest-first),
//! fed by pane cwd → enclosing git root → `upsert_project_by_root`. The native port of
//! `components/Sidebar.tsx` + `store/useProjects.ts`: the canonical list lives in
//! `projects.json` (owned by core); the panel renders a cached copy refreshed whenever
//! it opens or a pane reports a new cwd. Selecting a project opens a fresh pane cd'd
//! into its repo.

use std::path::{Path, PathBuf};

pub use hyperpanes_core::persistence::projects::Project;
use hyperpanes_core::persistence::projects;

/// The remembered projects, newest-first (the order the panel renders).
pub fn list() -> Vec<Project> {
    projects::list_projects()
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

/// A pane reported a new working directory: if it sits inside a git repo, remember the
/// repo root (or bump its recency) and return the refreshed, newest-first list. Returns
/// `None` when the cwd isn't in a repo (nothing changed).
pub fn note_cwd(cwd: &str) -> Option<Vec<Project>> {
    let root = git_root_of(cwd)?;
    projects::upsert_project_by_root(&root.to_string_lossy());
    Some(list())
}

#[cfg(test)]
mod tests {
    use super::*;

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
