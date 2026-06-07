//! Port of `src/main/ai/ai-store.ts` — AI settings + memory persistence
//! (`ai-settings.json` / `ai-memory.json`) with ATOMIC temp-then-rename writes. Takes the
//! file path as a PARAMETER (DI), not `persistence::paths`, to stay decoupled. Mirror
//! `ai-store.test.ts`.
//!
//! On-disk memory for the ambient-AI feature: per-project rolling summaries + a
//! timeline, and per-pane records. An in-memory cache fronts an ATOMIC write
//! (write a temp file, then rename over the target) so a crash mid-write can never
//! corrupt an existing good file. Path-injected so it can be unit-tested against a
//! tmp file.
//!
//! The TS source debounces writes via `setTimeout`; that timer belongs to the live
//! integration layer, so this port collapses it to caller-driven [`AiMemoryStore::flush`]
//! (mutations update the in-memory cache; `flush` persists atomically). All tests
//! drive persistence through `flush`, matching the TS test suite.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const TIMELINE_CAP: usize = 200;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A single dated note on a project's timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub ts: i64,
    pub kind: TimelineKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimelineKind {
    Milestone,
    Note,
    Error,
}

/// A project's rolling memory: a rewritten summary plus a capped FIFO timeline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMemory {
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub summary_updated_at: i64,
    #[serde(default)]
    pub timeline: Vec<TimelineEntry>,
}

/// A pane's last-known state and rolling summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneMemory {
    pub pane_id: String,
    #[serde(default)]
    pub project_path: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub last_cwd: String,
    #[serde(default)]
    pub last_command: Option<String>,
    #[serde(default)]
    pub updated_at: i64,
}

/// The whole persisted document. `version` lets future migrations branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AiMemoryFile {
    version: u8,
    #[serde(default)]
    projects: HashMap<String, ProjectMemory>,
    #[serde(default)]
    panes: HashMap<String, PaneMemory>,
}

impl Default for AiMemoryFile {
    fn default() -> Self {
        Self {
            version: 1,
            projects: HashMap::new(),
            panes: HashMap::new(),
        }
    }
}

/// Patch for [`AiMemoryStore::upsert_project`]; `None` fields are left as-is.
#[derive(Debug, Clone, Default)]
pub struct ProjectPatch {
    pub name: Option<String>,
    pub summary: Option<String>,
    pub summary_updated_at: Option<i64>,
    pub timeline: Option<Vec<TimelineEntry>>,
}

/// Patch for [`AiMemoryStore::upsert_pane`]; `None` fields are left as-is. Nullable
/// fields use `Option<Option<_>>`: `Some(None)` writes null, `None` keeps the value.
#[derive(Debug, Clone, Default)]
pub struct PanePatch {
    pub project_path: Option<Option<String>>,
    pub label: Option<String>,
    pub subtitle: Option<String>,
    pub summary: Option<String>,
    pub last_cwd: Option<String>,
    pub last_command: Option<Option<String>>,
}

pub struct AiMemoryStore {
    file_path: PathBuf,
    data: AiMemoryFile,
}

impl AiMemoryStore {
    pub fn new(file_path: impl Into<PathBuf>) -> Self {
        Self {
            file_path: file_path.into(),
            data: AiMemoryFile::default(),
        }
    }

    /// Read the file into the in-memory cache. A missing OR corrupt/unparseable
    /// file is tolerated — we start empty, never panicking out of `load`.
    pub fn load(&mut self) {
        self.data = match std::fs::read_to_string(&self.file_path) {
            Ok(text) => match serde_json::from_str::<AiMemoryFile>(&text) {
                Ok(mut parsed) => {
                    parsed.version = 1;
                    parsed
                }
                Err(_) => AiMemoryFile::default(),
            },
            Err(_) => AiMemoryFile::default(),
        };
    }

    pub fn get_project(&self, path: &str) -> Option<&ProjectMemory> {
        self.data.projects.get(path)
    }

    /// Create-or-update a project record, shallow-merging the patch over any
    /// existing record. Stamps `summary_updated_at` whenever the summary is touched.
    /// Stores by the caller-supplied key verbatim (no path canonicalization).
    pub fn upsert_project(&mut self, path: &str, patch: ProjectPatch) -> ProjectMemory {
        let mut merged = self.data.projects.get(path).cloned().unwrap_or(ProjectMemory {
            path: path.to_string(),
            name: String::new(),
            summary: String::new(),
            summary_updated_at: 0,
            timeline: Vec::new(),
        });
        if let Some(name) = patch.name {
            merged.name = name;
        }
        let summary_touched = patch.summary.is_some();
        if let Some(summary) = patch.summary {
            merged.summary = summary;
        }
        if let Some(ts) = patch.summary_updated_at {
            merged.summary_updated_at = ts;
        }
        if let Some(timeline) = patch.timeline {
            merged.timeline = timeline;
        }
        merged.path = path.to_string();
        if summary_touched {
            merged.summary_updated_at = now_ms();
        }
        self.data.projects.insert(path.to_string(), merged.clone());
        merged
    }

    /// Push an entry onto a project's timeline (creating the project if absent),
    /// trimming to the most-recent `TIMELINE_CAP` entries (drop oldest).
    pub fn append_timeline(&mut self, path: &str, entry: TimelineEntry) {
        if !self.data.projects.contains_key(path) {
            self.upsert_project(path, ProjectPatch::default());
        }
        let project = self.data.projects.get_mut(path).expect("just inserted");
        project.timeline.push(entry);
        if project.timeline.len() > TIMELINE_CAP {
            let drop = project.timeline.len() - TIMELINE_CAP;
            project.timeline.drain(0..drop);
        }
    }

    pub fn get_pane(&self, pane_id: &str) -> Option<&PaneMemory> {
        self.data.panes.get(pane_id)
    }

    /// Create-or-update a pane record, shallow-merging the patch and bumping
    /// `updated_at`. Stores by the caller-supplied paneId verbatim.
    pub fn upsert_pane(&mut self, pane_id: &str, patch: PanePatch) -> PaneMemory {
        let mut merged = self.data.panes.get(pane_id).cloned().unwrap_or(PaneMemory {
            pane_id: pane_id.to_string(),
            project_path: None,
            label: String::new(),
            subtitle: String::new(),
            summary: String::new(),
            last_cwd: String::new(),
            last_command: None,
            updated_at: 0,
        });
        if let Some(project_path) = patch.project_path {
            merged.project_path = project_path;
        }
        if let Some(label) = patch.label {
            merged.label = label;
        }
        if let Some(subtitle) = patch.subtitle {
            merged.subtitle = subtitle;
        }
        if let Some(summary) = patch.summary {
            merged.summary = summary;
        }
        if let Some(last_cwd) = patch.last_cwd {
            merged.last_cwd = last_cwd;
        }
        if let Some(last_command) = patch.last_command {
            merged.last_command = last_command;
        }
        merged.pane_id = pane_id.to_string();
        merged.updated_at = now_ms();
        self.data.panes.insert(pane_id.to_string(), merged.clone());
        merged
    }

    pub fn prune_pane(&mut self, pane_id: &str) {
        self.data.panes.remove(pane_id);
    }

    /// Drop every pane whose id is not in the keep-list.
    pub fn prune_panes_except(&mut self, keep_pane_ids: &[String]) {
        let keep: std::collections::HashSet<&str> =
            keep_pane_ids.iter().map(|s| s.as_str()).collect();
        self.data.panes.retain(|id, _| keep.contains(id.as_str()));
    }

    /// Force an immediate atomic write of the in-memory cache.
    pub fn flush(&mut self) {
        self.write_now();
    }

    // Atomic write: serialize to a temp sibling, then rename over the target so a
    // reader never sees a half-written file.
    fn write_now(&mut self) {
        self.data.version = 1;
        let tmp = with_tmp_suffix(&self.file_path);
        let result = (|| -> std::io::Result<()> {
            if let Some(parent) = self.file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let json = serde_json::to_string_pretty(&self.data)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            std::fs::write(&tmp, json)?;
            std::fs::rename(&tmp, &self.file_path)?;
            Ok(())
        })();
        if let Err(err) = result {
            eprintln!("failed to write ai-memory.json: {err}");
        }
    }
}

fn with_tmp_suffix(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // A self-cleaning temp dir under the OS temp directory.
    struct TempDir {
        dir: PathBuf,
    }
    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("ai-store-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            Self { dir }
        }
        fn file(&self) -> PathBuf {
            self.dir.join("ai-memory.json")
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn proj(name: Option<&str>, summary: Option<&str>) -> ProjectPatch {
        ProjectPatch {
            name: name.map(str::to_string),
            summary: summary.map(str::to_string),
            ..Default::default()
        }
    }

    // ---- load ----
    #[test]
    fn starts_empty_when_the_file_is_missing() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        assert!(store.get_project("/x").is_none());
        assert!(store.get_pane("p1").is_none());
    }

    #[test]
    fn tolerates_a_corrupt_file_and_starts_empty() {
        let td = TempDir::new();
        std::fs::write(td.file(), "{ this is not: valid json ]]").unwrap();
        let mut store = AiMemoryStore::new(td.file());
        store.load(); // must not panic
        assert!(store.get_project("/x").is_none());
    }

    // ---- projects ----
    #[test]
    fn upserts_then_gets_a_project_roundtrip() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        let p = store.upsert_project("/repo", proj(Some("repo"), Some("hi")));
        assert_eq!(p.path, "/repo");
        assert_eq!(p.name, "repo");
        assert_eq!(p.summary, "hi");
        assert_eq!(store.get_project("/repo"), Some(&p));
    }

    #[test]
    fn shallow_merges_patches_and_stamps_summary_updated_at_on_summary_change() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_project("/repo", proj(Some("repo"), Some("first")));
        let before = store.get_project("/repo").unwrap().summary_updated_at;
        let merged = store.upsert_project("/repo", proj(None, Some("second")));
        assert_eq!(merged.name, "repo"); // preserved by shallow merge
        assert_eq!(merged.summary, "second");
        assert!(merged.summary_updated_at >= before);
    }

    #[test]
    fn stores_by_the_caller_supplied_key_without_canonicalizing() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_project("c:\\Repo", proj(Some("a"), None));
        store.upsert_project("C:\\repo", proj(Some("b"), None));
        assert_eq!(store.get_project("c:\\Repo").unwrap().name, "a");
        assert_eq!(store.get_project("C:\\repo").unwrap().name, "b");
    }

    #[test]
    fn initializes_a_new_project_with_sane_defaults() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        let p = store.upsert_project("/repo", ProjectPatch::default());
        assert_eq!(p.name, "");
        assert_eq!(p.summary, "");
        assert!(p.timeline.is_empty());
    }

    // ---- appendTimeline ----
    #[test]
    fn appends_entries_onto_a_project_creating_it_if_absent() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        let e = TimelineEntry {
            ts: 1,
            kind: TimelineKind::Note,
            text: "hello".into(),
        };
        store.append_timeline("/repo", e.clone());
        assert_eq!(store.get_project("/repo").unwrap().timeline, vec![e]);
    }

    #[test]
    fn caps_the_timeline_at_200_entries_dropping_oldest() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        for i in 0..250 {
            store.append_timeline(
                "/repo",
                TimelineEntry {
                    ts: i,
                    kind: TimelineKind::Note,
                    text: format!("e{i}"),
                },
            );
        }
        let tl = &store.get_project("/repo").unwrap().timeline;
        assert_eq!(tl.len(), 200);
        assert_eq!(tl[0].ts, 50); // oldest 50 dropped
        assert_eq!(tl[199].ts, 249);
    }

    // ---- panes ----
    #[test]
    fn upserts_then_gets_a_pane_roundtrip() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        let pane = store.upsert_pane(
            "p1",
            PanePatch {
                label: Some("shell".into()),
                last_cwd: Some("/tmp".into()),
                ..Default::default()
            },
        );
        assert_eq!(pane.pane_id, "p1");
        assert_eq!(pane.label, "shell");
        assert_eq!(pane.last_cwd, "/tmp");
        assert_eq!(store.get_pane("p1"), Some(&pane));
    }

    #[test]
    fn shallow_merges_pane_patches_and_bumps_updated_at() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_pane(
            "p1",
            PanePatch {
                label: Some("shell".into()),
                last_command: Some(Some("ls".into())),
                ..Default::default()
            },
        );
        let before = store.get_pane("p1").unwrap().updated_at;
        let merged = store.upsert_pane(
            "p1",
            PanePatch {
                last_command: Some(Some("pwd".into())),
                ..Default::default()
            },
        );
        assert_eq!(merged.label, "shell"); // preserved
        assert_eq!(merged.last_command.as_deref(), Some("pwd"));
        assert!(merged.updated_at >= before);
    }

    #[test]
    fn initializes_a_new_pane_with_sane_defaults() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        let pane = store.upsert_pane("p1", PanePatch::default());
        assert!(pane.project_path.is_none());
        assert_eq!(pane.label, "");
        assert_eq!(pane.subtitle, "");
        assert_eq!(pane.summary, "");
        assert_eq!(pane.last_cwd, "");
        assert!(pane.last_command.is_none());
    }

    #[test]
    fn prune_pane_removes_a_single_pane() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_pane("p1", PanePatch::default());
        store.upsert_pane("p2", PanePatch::default());
        store.prune_pane("p1");
        assert!(store.get_pane("p1").is_none());
        assert!(store.get_pane("p2").is_some());
    }

    #[test]
    fn prune_panes_except_keeps_only_the_listed_panes() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_pane("p1", PanePatch::default());
        store.upsert_pane("p2", PanePatch::default());
        store.upsert_pane("p3", PanePatch::default());
        store.prune_panes_except(&["p2".to_string()]);
        assert!(store.get_pane("p1").is_none());
        assert!(store.get_pane("p2").is_some());
        assert!(store.get_pane("p3").is_none());
    }

    #[test]
    fn prune_panes_except_with_an_empty_keep_list_removes_all_panes() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_pane("p1", PanePatch::default());
        store.upsert_pane("p2", PanePatch::default());
        store.prune_panes_except(&[]);
        assert!(store.get_pane("p1").is_none());
        assert!(store.get_pane("p2").is_none());
    }

    // ---- persistence ----
    #[test]
    fn flush_writes_valid_json_and_a_fresh_store_loads_it_back() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_project("/repo", proj(Some("repo"), Some("s")));
        store.append_timeline(
            "/repo",
            TimelineEntry {
                ts: 1,
                kind: TimelineKind::Milestone,
                text: "m".into(),
            },
        );
        store.upsert_pane(
            "p1",
            PanePatch {
                label: Some("shell".into()),
                ..Default::default()
            },
        );
        store.flush();

        assert!(td.file().exists());
        let parsed: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(td.file()).unwrap()).unwrap();
        assert_eq!(parsed["version"], 1);

        let mut reloaded = AiMemoryStore::new(td.file());
        reloaded.load();
        assert_eq!(reloaded.get_project("/repo").unwrap().name, "repo");
        assert_eq!(reloaded.get_project("/repo").unwrap().timeline.len(), 1);
        assert_eq!(reloaded.get_pane("p1").unwrap().label, "shell");
    }

    #[test]
    fn flush_can_be_called_with_no_pending_changes_without_error() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.flush(); // must not panic
    }

    #[test]
    fn does_not_leave_a_temp_file_behind_after_an_atomic_write() {
        let td = TempDir::new();
        let mut store = AiMemoryStore::new(td.file());
        store.load();
        store.upsert_project("/repo", proj(Some("repo"), None));
        store.flush();
        let tmp = with_tmp_suffix(&td.file());
        assert!(!tmp.exists());
    }
}
