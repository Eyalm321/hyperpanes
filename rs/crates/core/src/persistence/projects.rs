//! Port of `src/main/projects.ts` — git-project history (`projects.json`).
//!
//! Behaviours preserved 1:1 from the TS source:
//!   * `canonical_path`: strip trailing separators; on Windows normalise `/`→`\` and
//!     uppercase the drive letter, so the same repo reported by cmd (`c:\…`), pwsh
//!     (`C:\…`) and git-bash stores identically;
//!   * a stable per-repo color: hash the canonical key with the JS `h*31 + c` rolling
//!     hash (truncated to u32, mirroring `>>> 0`) into the shared 8-slot palette, so a
//!     repo keeps its color across restarts AND across the TS→Rust switch;
//!   * `repo_name_from_url`: parse the repo name out of any remote URL;
//!   * `git_repo_name`: read the `origin` url straight from `.git/config` (no spawn);
//!   * `upsert_project_by_root`: remember a git root or bump its recency, healing a
//!     folder-name title to the real repo name (but never clobbering a user rename),
//!     and dedup-on-load (case-insensitively on Windows) self-heals legacy duplicates.

use crate::persistence::paths;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// A git repo the app remembers from a pane cd-ing into it. Structurally identical to
/// the renderer-side `Project` in `src/renderer/types.ts`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    /// normalized git-root absolute path
    pub path: String,
    /// basename of the git root (the title)
    pub name: String,
    /// frame/dot color for panes in this project
    pub color: String,
    /// epoch ms, for recency sorting
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_opened_at: Option<u64>,
}

/// The 8-slot palette a repo's color is hashed into (kept identical to the TS source
/// AND the shared renderer palette so colors are stable across the rewrite).
pub const PROJECT_COLORS: [&str; 8] = [
    "#e5484d", "#f5a623", "#30a46c", "#3b82f6", "#a855f7", "#ec4899", "#14b8a6", "#eab308",
];

/// On-disk wrapper: `{ "projects": [ … ] }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<Project>,
}

#[inline]
fn is_win() -> bool {
    cfg!(windows)
}

/// Canonical absolute path for storage. Strips trailing separators; on Windows
/// normalises `/`→`\` and uppercases the drive letter.
pub fn canonical_path(p: &str) -> String {
    // `p.replace(/[\\/]+$/, '')` — drop a run of trailing slashes/backslashes.
    let mut s: &str = p;
    while let Some(last) = s.as_bytes().last() {
        if *last == b'\\' || *last == b'/' {
            s = &s[..s.len() - 1];
        } else {
            break;
        }
    }
    let mut out = s.to_string();
    if is_win() {
        out = out.replace('/', "\\");
        // `^([a-z]):` → uppercase the drive letter (only lowercase a-z, per the regex).
        let bytes = out.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_lowercase() && bytes[1] == b':' {
            let mut chars: Vec<char> = out.chars().collect();
            chars[0] = chars[0].to_ascii_uppercase();
            out = chars.into_iter().collect();
        }
    }
    out
}

/// Dedup key — case-insensitive on Windows (its paths ignore case). Public so callers
/// matching a pane cwd's git root against a stored [`Project::path`] compare with the
/// SAME key the store dedups by (e.g. the app's recolor-propagation).
pub fn path_key(p: &str) -> String {
    let c = canonical_path(p);
    if is_win() {
        c.to_lowercase()
    } else {
        c
    }
}

/// A stable per-repo color from the canonical key, via the JS `h = (h*31 + c) >>> 0`
/// rolling hash over UTF-16 code units (so it matches `charCodeAt`).
fn color_for_path(p: &str) -> String {
    let key = path_key(p);
    let mut h: u32 = 0;
    for unit in key.encode_utf16() {
        h = h.wrapping_mul(31).wrapping_add(unit as u32);
    }
    PROJECT_COLORS[(h % PROJECT_COLORS.len() as u32) as usize].to_string()
}

/// Parse the repository name out of a remote URL (any host):
///   `https://github.com/owner/my-repo.git` → `my-repo`
///   `git@github.com:owner/my-repo.git`     → `my-repo`
///   `ssh://git@github.com/owner/My.Repo.git` → `My.Repo`
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim();
    // `.replace(/\.git$/i, '')` — strip a case-insensitive trailing `.git`.
    let without_git = if trimmed.len() >= 4 && trimmed[trimmed.len() - 4..].eq_ignore_ascii_case(".git")
    {
        &trimmed[..trimmed.len() - 4]
    } else {
        trimmed
    };
    // `.replace(/[\\/]+$/, '')` — strip trailing separators.
    let mut u = without_git;
    while let Some(last) = u.as_bytes().last() {
        if *last == b'\\' || *last == b'/' {
            u = &u[..u.len() - 1];
        } else {
            break;
        }
    }
    if u.is_empty() {
        return None;
    }
    // `u.split(/[\\/:]/).filter(Boolean)` → last non-empty segment.
    u.split(|c| c == '\\' || c == '/' || c == ':')
        .filter(|s| !s.is_empty())
        .next_back()
        .map(|s| s.to_string())
}

/// The repo's name from its `origin` remote, read straight from `.git/config`.
/// `None` when there's no plain `.git` directory (worktree/submodule pointer) or no
/// origin url — the caller falls back to the folder name.
fn git_repo_name(git_root: &str) -> Option<String> {
    let dot_git = Path::new(git_root).join(".git");
    if !dot_git.is_dir() {
        return None;
    }
    let cfg = std::fs::read_to_string(dot_git.join("config")).ok()?;
    parse_origin_url(&cfg).and_then(|u| repo_name_from_url(&u))
}

/// Extract the `url = …` value under `[remote "origin"]`, mirroring the TS regex
/// `/\[remote "origin"\][^[]*?\burl\s*=\s*(.+)/` (the url must appear before the next
/// `[`-section; `.+` captures the rest of the line).
fn parse_origin_url(cfg: &str) -> Option<String> {
    const HEADER: &str = "[remote \"origin\"]";
    let start = cfg.find(HEADER)?;
    let after = &cfg[start + HEADER.len()..];
    // `[^[]*?` cannot cross a `[`, so bound the search at the next section header.
    let section = match after.find('[') {
        Some(i) => &after[..i],
        None => after,
    };
    for line in section.lines() {
        let t = line.trim_start();
        // `\burl\s*=` — the key `url` followed by optional spaces then `=`.
        if let Some(rest) = t.strip_prefix("url") {
            let rest = rest.trim_start();
            if let Some(value) = rest.strip_prefix('=') {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

/// `path.basename` of a stored (canonical) path.
fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Collapse entries pointing at the same directory (keeping the most-recently-opened)
/// and canonicalize each stored path — self-healing duplicates saved before paths were
/// canonicalized. Preserves first-seen order (mirrors the JS `Map`).
fn dedupe(list: Vec<Project>) -> Vec<Project> {
    let mut order: Vec<String> = Vec::new();
    let mut by_key: HashMap<String, Project> = HashMap::new();
    for proj in list {
        let canon = Project {
            path: canonical_path(&proj.path),
            ..proj
        };
        let key = path_key(&canon.path);
        match by_key.get(&key) {
            // Keep the existing one only when it is strictly newer (TS replaces on >=).
            Some(prev) if prev.last_opened_at.unwrap_or(0) > canon.last_opened_at.unwrap_or(0) => {}
            _ => {
                if !by_key.contains_key(&key) {
                    order.push(key.clone());
                }
                by_key.insert(key, canon);
            }
        }
    }
    order
        .into_iter()
        .filter_map(|k| by_key.remove(&k))
        .collect()
}

/// Load + dedup the project list from `store`, persisting the cleanup if dedup changed
/// the count (the TS self-heal). Missing/corrupt files start empty.
fn load_from(store: &Path) -> Vec<Project> {
    let Ok(raw) = std::fs::read_to_string(store) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let raw_list: Vec<Project> = value
        .get("projects")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<Project>(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let raw_len = raw_list.len();
    let deduped = dedupe(raw_list);
    if deduped.len() != raw_len {
        let _ = save_to(store, &deduped);
    }
    deduped
}

/// Persist the project list to `store` (atomic) as `{ "projects": [ … ] }`.
fn save_to(store: &Path, list: &[Project]) -> std::io::Result<()> {
    let file = ProjectsFile {
        projects: list.to_vec(),
    };
    let json = serde_json::to_string_pretty(&file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    paths::write_atomic(store, json.as_bytes())
}

/// Remember a git root (or bump its recency if already known) in `store`.
fn upsert_project_by_root_in(store: &Path, root: &str) -> Project {
    let path = canonical_path(root);
    let key = path_key(&path);
    let mut list = load_from(store);
    let repo = git_repo_name(&path);

    if let Some(idx) = list.iter().position(|p| path_key(&p.path) == key) {
        list[idx].last_opened_at = Some(now_ms());
        list[idx].path = path.clone(); // canonicalize a legacy-stored path
        // Heal a folder-name title to the real repo name, but never a user rename.
        if let Some(r) = &repo {
            if list[idx].name == basename(&path) && &list[idx].name != r {
                list[idx].name = r.clone();
            }
        }
        let updated = list[idx].clone();
        let _ = save_to(store, &list);
        return updated;
    }

    let base = basename(&path);
    let project = Project {
        id: uuid::Uuid::new_v4().to_string(),
        name: repo
            .unwrap_or_else(|| if base.is_empty() { path.clone() } else { base }),
        color: color_for_path(&path),
        last_opened_at: Some(now_ms()),
        path,
    };
    list.push(project.clone());
    let _ = save_to(store, &list);
    project
}

// ---- public API over the canonical `projects.json` ----

/// Newest-first by last-opened; the sidebar renders in this order.
pub fn list_projects() -> Vec<Project> {
    let mut list = load_from(&paths::projects_json());
    list.sort_by(|a, b| b.last_opened_at.unwrap_or(0).cmp(&a.last_opened_at.unwrap_or(0)));
    list
}

/// Remember a git root (or bump its recency); returns the project.
pub fn upsert_project_by_root(root: &str) -> Project {
    upsert_project_by_root_in(&paths::projects_json(), root)
}

/// Set a project's color by id.
pub fn set_project_color(id: &str, color: &str) {
    let store = paths::projects_json();
    let mut list = load_from(&store);
    if let Some(p) = list.iter_mut().find(|p| p.id == id) {
        p.color = color.to_string();
        let _ = save_to(&store, &list);
    }
}

/// Rename a project by id.
pub fn rename_project(id: &str, name: &str) {
    let store = paths::projects_json();
    let mut list = load_from(&store);
    if let Some(p) = list.iter_mut().find(|p| p.id == id) {
        p.name = name.to_string();
        let _ = save_to(&store, &list);
    }
}

/// Forget a project by id.
pub fn remove_project(id: &str) {
    let store = paths::projects_json();
    let list: Vec<Project> = load_from(&store)
        .into_iter()
        .filter(|p| p.id != id)
        .collect();
    let _ = save_to(&store, &list);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- describe('repoNameFromUrl') ----

    #[test]
    fn parses_an_https_github_url_with_git() {
        assert_eq!(
            repo_name_from_url("https://github.com/Eyalm321/hyperpanes.git").as_deref(),
            Some("hyperpanes")
        );
    }

    #[test]
    fn parses_an_https_url_without_git() {
        assert_eq!(
            repo_name_from_url("https://github.com/owner/my-repo").as_deref(),
            Some("my-repo")
        );
    }

    #[test]
    fn parses_an_scp_style_ssh_url() {
        assert_eq!(
            repo_name_from_url("git@github.com:owner/my-repo.git").as_deref(),
            Some("my-repo")
        );
    }

    #[test]
    fn parses_an_ssh_url_and_keeps_dots_in_the_name() {
        assert_eq!(
            repo_name_from_url("ssh://git@github.com/owner/My.Repo.git").as_deref(),
            Some("My.Repo")
        );
    }

    #[test]
    fn strips_a_trailing_slash() {
        assert_eq!(
            repo_name_from_url("https://gitlab.com/group/sub/proj/").as_deref(),
            Some("proj")
        );
    }

    #[test]
    fn returns_none_for_an_empty_string() {
        assert_eq!(repo_name_from_url(""), None);
    }

    // ---- describe.skipIf(non-win)('canonicalPath (Windows)') ----

    #[cfg(windows)]
    #[test]
    fn uppercases_the_drive_letter() {
        assert_eq!(canonical_path("c:\\hyperpanes"), "C:\\hyperpanes");
    }

    #[cfg(windows)]
    #[test]
    fn normalizes_forward_slashes_and_strips_a_trailing_separator() {
        assert_eq!(canonical_path("C:/Users/me/repo/"), "C:\\Users\\me\\repo");
    }

    #[cfg(windows)]
    #[test]
    fn makes_cmd_and_pwsh_paths_identical() {
        assert_eq!(canonical_path("c:\\hyperpanes"), canonical_path("C:\\hyperpanes"));
    }

    // ---- extra coverage: color stability + upsert round-trip ----

    #[test]
    fn color_is_stable_and_in_palette() {
        let c = color_for_path("/home/me/project");
        assert!(PROJECT_COLORS.contains(&c.as_str()));
        // Same canonical key → same color regardless of trailing separator / casing.
        assert_eq!(color_for_path("/home/me/project"), color_for_path("/home/me/project/"));
    }

    #[cfg(windows)]
    #[test]
    fn color_is_drive_case_insensitive_on_windows() {
        assert_eq!(color_for_path("c:\\repo"), color_for_path("C:/repo/"));
    }

    fn unique_temp(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "hp-projects-{}-{tag}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn upsert_creates_then_dedups_on_recency() {
        let store = unique_temp("upsert");
        // Use a real directory with no .git so the name falls back to the basename.
        let root_dir = std::env::temp_dir().join(format!(
            "hp-projects-root-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root_dir).unwrap();
        let root = root_dir.to_string_lossy().into_owned();

        let first = upsert_project_by_root_in(&store, &root);
        assert!(PROJECT_COLORS.contains(&first.color.as_str()));
        assert_eq!(first.name, basename(&canonical_path(&root)));
        assert_eq!(first.path, canonical_path(&root));
        assert!(first.last_opened_at.is_some());

        // A second upsert of the same root keeps a single, same-id entry.
        let second = upsert_project_by_root_in(&store, &root);
        assert_eq!(first.id, second.id);

        let list = load_from(&store);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, first.id);

        let _ = std::fs::remove_file(&store);
        let _ = std::fs::remove_dir_all(&root_dir);
    }

    #[test]
    fn load_from_missing_or_corrupt_is_empty() {
        let missing = unique_temp("missing");
        assert!(load_from(&missing).is_empty());
        let corrupt = unique_temp("corrupt");
        std::fs::write(&corrupt, b"}{ not json").unwrap();
        assert!(load_from(&corrupt).is_empty());
        let _ = std::fs::remove_file(&corrupt);
    }

    #[test]
    fn parse_origin_url_reads_the_git_config_block() {
        let cfg = "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/owner/cool-repo.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n";
        assert_eq!(
            parse_origin_url(cfg).as_deref(),
            Some("https://github.com/owner/cool-repo.git")
        );
        assert_eq!(
            parse_origin_url(cfg).and_then(|u| repo_name_from_url(&u)).as_deref(),
            Some("cool-repo")
        );
        // No origin section → None.
        assert_eq!(parse_origin_url("[core]\n\tbare = false\n"), None);
    }
}
