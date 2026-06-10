//! Port of the workspace-file I/O in `src/main/workspace.ts`: `read_workspace` /
//! `write_workspace` / `resolve_cwds` / `windows_of` / `has_panes`, over the serde
//! model in `crate::workspace::model`.
//!
//! Parity rules preserved 1:1:
//!   * `resolve_cwds` rewrites every pane `cwd` at all three nesting levels
//!     (top-level `panes`, each group's panes, each window's groups' panes): an
//!     absolute cwd is kept verbatim, a relative one is resolved against `base_dir`;
//!   * `has_panes` is true when ANY of `panes` / `groups` / `windows` is present (an
//!     empty array still counts, mirroring `Array.isArray`);
//!   * `read_workspace` returns `None` on read/parse error or a contentless file, and
//!     otherwise resolves cwds against the file's own directory;
//!   * `windows_of` normalises any file into a flat window list with the schema's
//!     precedence (`windows` → `groups` → `panes`), dropping groupless windows.
//!
//! The `.hyperpanes` format (docs/hyperpanes-format.md, option (b)): on-disk files are
//! a **versioned container** `{ "format": "hyperpanes", "version": 1, "workspace": {…} }`
//! ([`WorkspaceEnvelope`]). The reader accepts BOTH that envelope and a bare legacy
//! `WorkspaceFile` object (treated as "version 0"); the writer always emits the
//! versioned form. A present-but-wrong `format`, or a `version` newer than this build
//! understands, is rejected with a clear error ([`parse_workspace_str`]). The envelope
//! keeps the byte-identical 2-space pretty round-trip contract of the inner payload.

use crate::workspace::model::{GroupSpec, PaneSpec, WindowSpec, WorkspaceFile};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// The magic `format` discriminator of a versioned workspace container.
pub const ENVELOPE_FORMAT: &str = "hyperpanes";
/// The newest envelope `version` this build reads and the version it writes.
pub const ENVELOPE_VERSION: u32 = 1;

/// The versioned on-disk container: `{ "format": "hyperpanes", "version": 1,
/// "workspace": { … } }`. Field declaration order is the canonical file order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceEnvelope {
    pub format: String,
    pub version: u32,
    pub workspace: WorkspaceFile,
}

impl WorkspaceEnvelope {
    /// Wrap a workspace payload in the current-version envelope.
    pub fn wrap(workspace: WorkspaceFile) -> Self {
        Self {
            format: ENVELOPE_FORMAT.to_string(),
            version: ENVELOPE_VERSION,
            workspace,
        }
    }
}

/// Parse workspace-file text, accepting both shapes: the versioned envelope (a top-level
/// object with a `format` key) and the bare legacy `WorkspaceFile` ("version 0"). An
/// envelope with the wrong `format` or a `version` this build doesn't understand is an
/// error (a bare object is never mistaken for an envelope — the legacy schema has no
/// `format` field).
pub fn parse_workspace_str(raw: &str) -> Result<WorkspaceFile, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("invalid JSON: {e}"))?;

    let is_envelope = value
        .as_object()
        .is_some_and(|obj| obj.contains_key("format"));
    if !is_envelope {
        // Legacy bare WorkspaceFile (version 0).
        return serde_json::from_value(value).map_err(|e| format!("invalid workspace: {e}"));
    }

    let obj = value.as_object().unwrap();
    match obj.get("format").and_then(|f| f.as_str()) {
        Some(ENVELOPE_FORMAT) => {}
        other => {
            return Err(format!(
                "not a hyperpanes workspace: \"format\" is {:?}, expected \"{ENVELOPE_FORMAT}\"",
                other.unwrap_or("<non-string>")
            ));
        }
    }
    match obj.get("version").and_then(|v| v.as_u64()) {
        Some(v) if (1..=ENVELOPE_VERSION as u64).contains(&v) => {}
        Some(v) => {
            return Err(format!(
                "workspace version {v} is newer than this build understands \
                 (max {ENVELOPE_VERSION}) — update hyperpanes to open it"
            ));
        }
        None => {
            return Err("hyperpanes workspace is missing a numeric \"version\" field".to_string());
        }
    }
    let workspace = obj
        .get("workspace")
        .cloned()
        .ok_or_else(|| "hyperpanes workspace is missing the \"workspace\" payload".to_string())?;
    serde_json::from_value(workspace).map_err(|e| format!("invalid workspace payload: {e}"))
}

/// Node `path.isAbsolute` semantics for the current platform. On Windows a leading
/// `/` or `\`, or a drive-rooted `C:\` / `C:/`, is absolute (a drive-relative `C:foo`
/// is NOT); on POSIX, a leading `/`.
fn node_is_absolute(p: &str) -> bool {
    let b = p.as_bytes();
    if b.is_empty() {
        return false;
    }
    if cfg!(windows) {
        if b[0] == b'/' || b[0] == b'\\' {
            return true;
        }
        b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'/' || b[2] == b'\\')
    } else {
        b[0] == b'/'
    }
}

/// Normalise away `.` / `..` components (like the tail of node `path.resolve`).
fn normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `path.resolve(base, p)` for a relative `p`: join onto `base`, make absolute against
/// the process cwd if needed, then normalise.
fn resolve_from(base: &str, p: &str) -> String {
    let mut combined = PathBuf::from(base);
    combined.push(p);
    let abs = if combined.is_absolute() {
        combined
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(combined)
    };
    normalize(abs).to_string_lossy().into_owned()
}

fn fix_panes(panes: &mut [PaneSpec], base_dir: &str) {
    for p in panes.iter_mut() {
        if let Some(cwd) = &p.cwd {
            p.cwd = Some(if node_is_absolute(cwd) {
                cwd.clone()
            } else {
                resolve_from(base_dir, cwd)
            });
        }
    }
}

/// Resolve relative pane cwds against `base_dir`, across all three nesting levels.
pub fn resolve_cwds(file: &WorkspaceFile, base_dir: &str) -> WorkspaceFile {
    let mut out = file.clone();
    if let Some(panes) = out.panes.as_mut() {
        fix_panes(panes, base_dir);
    }
    if let Some(groups) = out.groups.as_mut() {
        for g in groups.iter_mut() {
            fix_panes(&mut g.panes, base_dir);
        }
    }
    if let Some(windows) = out.windows.as_mut() {
        for w in windows.iter_mut() {
            for g in w.groups.iter_mut() {
                fix_panes(&mut g.panes, base_dir);
            }
        }
    }
    out
}

/// A file is loadable if it describes panes at any nesting level.
pub fn has_panes(file: &WorkspaceFile) -> bool {
    file.panes.is_some() || file.groups.is_some() || file.windows.is_some()
}

/// Read + validate a workspace file, resolving relative cwds against its directory.
/// Returns `None` on read/parse error or a contentless file.
pub fn read_workspace<P: AsRef<Path>>(path: P) -> Option<WorkspaceFile> {
    let path = path.as_ref();
    let raw = std::fs::read_to_string(path).ok()?;
    let file = match parse_workspace_str(&raw) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("[hyperpanes] {}: {e}", path.display());
            return None;
        }
    };
    if !has_panes(&file) {
        return None;
    }
    let base_dir = path
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".to_string());
    Some(resolve_cwds(&file, &base_dir))
}

/// Write a workspace file (pretty, 2-space) in the versioned `.hyperpanes` container
/// form (`format`/`version`/`workspace`). Returns `false` on error (mirroring the TS
/// `writeWorkspace` boolean). The reader stays tolerant of bare legacy files, so older
/// `.json` workspaces keep loading even though saves are now always versioned.
pub fn write_workspace<P: AsRef<Path>>(path: P, data: &WorkspaceFile) -> bool {
    let envelope = WorkspaceEnvelope::wrap(data.clone());
    let Ok(json) = serde_json::to_string_pretty(&envelope) else {
        return false;
    };
    std::fs::write(path, json).is_ok()
}

/// Normalise any workspace file into a flat list of windows the launcher seeds from.
/// Precedence: `windows` (verbatim, groupless dropped) → `groups` (one window) →
/// `panes` (one window, one tab). `[]` for `None` / contentless input.
pub fn windows_of(file: Option<&WorkspaceFile>) -> Vec<WindowSpec> {
    let Some(file) = file else {
        return Vec::new();
    };

    if let Some(windows) = &file.windows {
        if !windows.is_empty() {
            return windows
                .iter()
                .filter(|w| !w.groups.is_empty())
                .cloned()
                .collect();
        }
    }

    if let Some(groups) = &file.groups {
        if !groups.is_empty() {
            return vec![WindowSpec {
                title: file.name.clone(),
                active: file.active,
                groups: groups.clone(),
                ..Default::default()
            }];
        }
    }

    if let Some(panes) = &file.panes {
        if !panes.is_empty() {
            return vec![WindowSpec {
                title: file.name.clone(),
                groups: vec![GroupSpec {
                    title: file.name.clone(),
                    layout: file.layout.clone(),
                    panes: panes.clone(),
                    ..Default::default()
                }],
                ..Default::default()
            }];
        }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_label(label: &str) -> PaneSpec {
        PaneSpec {
            label: Some(label.into()),
            ..Default::default()
        }
    }

    // ---- describe('windowsOf') ----

    #[test]
    fn returns_empty_for_null_or_contentless() {
        assert_eq!(windows_of(None), vec![]);
        assert_eq!(windows_of(Some(&WorkspaceFile::default())), vec![]);
        assert_eq!(
            windows_of(Some(&WorkspaceFile {
                panes: Some(vec![]),
                ..Default::default()
            })),
            vec![]
        );
    }

    #[test]
    fn wraps_top_level_panes_as_one_window_with_one_tab() {
        let file = WorkspaceFile {
            name: Some("x".into()),
            layout: Some("grid".into()),
            panes: Some(vec![pane_label("a")]),
            ..Default::default()
        };
        assert_eq!(
            windows_of(Some(&file)),
            vec![WindowSpec {
                title: Some("x".into()),
                groups: vec![GroupSpec {
                    title: Some("x".into()),
                    layout: Some("grid".into()),
                    panes: vec![pane_label("a")],
                    ..Default::default()
                }],
                ..Default::default()
            }]
        );
    }

    #[test]
    fn wraps_groups_as_one_window_of_tabs_carrying_active() {
        let groups = vec![GroupSpec {
            title: Some("t1".into()),
            panes: vec![pane_label("a")],
            ..Default::default()
        }];
        let file = WorkspaceFile {
            name: Some("x".into()),
            groups: Some(groups.clone()),
            active: Some(0),
            ..Default::default()
        };
        assert_eq!(
            windows_of(Some(&file)),
            vec![WindowSpec {
                title: Some("x".into()),
                active: Some(0),
                groups,
                ..Default::default()
            }]
        );
    }

    #[test]
    fn uses_windows_verbatim_dropping_groupless_windows() {
        let win = WindowSpec {
            title: Some("w".into()),
            groups: vec![GroupSpec {
                panes: vec![pane_label("a")],
                ..Default::default()
            }],
            ..Default::default()
        };
        let empty = WindowSpec {
            title: Some("empty".into()),
            groups: vec![],
            ..Default::default()
        };
        let file = WorkspaceFile {
            windows: Some(vec![win.clone(), empty]),
            ..Default::default()
        };
        assert_eq!(windows_of(Some(&file)), vec![win]);
    }

    // ---- read / write round-trip + has_panes + resolve_cwds ----

    fn temp_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hp-workspace-{}-{tag}.json", std::process::id()))
    }

    #[test]
    fn read_returns_none_for_missing_invalid_or_contentless() {
        let missing = temp_file("missing");
        let _ = std::fs::remove_file(&missing);
        assert!(read_workspace(&missing).is_none());

        let invalid = temp_file("invalid");
        std::fs::write(&invalid, b"not json").unwrap();
        assert!(read_workspace(&invalid).is_none());

        let empty = temp_file("empty");
        std::fs::write(&empty, b"{}").unwrap();
        assert!(read_workspace(&empty).is_none());

        let _ = std::fs::remove_file(&invalid);
        let _ = std::fs::remove_file(&empty);
    }

    #[test]
    fn write_then_read_round_trips_a_no_cwd_workspace() {
        let path = temp_file("roundtrip");
        let ws = WorkspaceFile {
            name: Some("dev".into()),
            layout: Some("main-stack".into()),
            panes: Some(vec![PaneSpec {
                command: Some("npm run dev".into()),
                label: Some("server".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(write_workspace(&path, &ws));
        // No cwds → resolve_cwds is a no-op, so the read value equals what we wrote.
        assert_eq!(read_workspace(&path), Some(ws));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_then_read_preserves_pane_zoom() {
        // Task 14: a zoomed pane's font_size survives a real on-disk save → load.
        let path = temp_file("zoom");
        let ws = WorkspaceFile {
            name: Some("z".into()),
            panes: Some(vec![PaneSpec {
                label: Some("zoomed".into()),
                font_size: Some(22),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(write_workspace(&path, &ws));
        let back = read_workspace(&path).expect("reads back");
        assert_eq!(back.panes.unwrap()[0].font_size, Some(22));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn has_panes_detects_any_nesting_level() {
        assert!(!has_panes(&WorkspaceFile::default()));
        assert!(has_panes(&WorkspaceFile {
            panes: Some(vec![]),
            ..Default::default()
        }));
        assert!(has_panes(&WorkspaceFile {
            groups: Some(vec![]),
            ..Default::default()
        }));
        assert!(has_panes(&WorkspaceFile {
            windows: Some(vec![]),
            ..Default::default()
        }));
    }

    #[test]
    fn resolve_cwds_keeps_absolute_and_resolves_relative() {
        let abs = if cfg!(windows) { "C:\\abs\\dir" } else { "/abs/dir" };
        let base = if cfg!(windows) { "C:\\base" } else { "/base" };
        let ws = WorkspaceFile {
            panes: Some(vec![
                PaneSpec {
                    cwd: Some(abs.to_string()),
                    ..Default::default()
                },
                PaneSpec {
                    cwd: Some("sub".to_string()),
                    ..Default::default()
                },
                PaneSpec {
                    cwd: None,
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };
        let out = resolve_cwds(&ws, base);
        let panes = out.panes.unwrap();
        // Absolute kept verbatim.
        assert_eq!(panes[0].cwd.as_deref(), Some(abs));
        // Relative resolved under base.
        let resolved = panes[1].cwd.as_deref().unwrap();
        assert!(resolved.ends_with("sub"), "resolved cwd: {resolved}");
        assert!(resolved.contains("base"), "resolved cwd: {resolved}");
        // Absent stays absent.
        assert_eq!(panes[2].cwd, None);
    }

    // ---- the .hyperpanes versioned container (format/version/workspace) ----

    #[test]
    fn write_emits_versioned_envelope_and_read_round_trips() {
        let path = temp_file("envelope");
        let ws = WorkspaceFile {
            name: Some("dev".into()),
            panes: Some(vec![PaneSpec {
                command: Some("npm run dev".into()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        assert!(write_workspace(&path, &ws));
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"format\": \"hyperpanes\""), "versioned form: {raw}");
        assert!(raw.contains("\"version\": 1"), "versioned form: {raw}");
        assert_eq!(read_workspace(&path), Some(ws));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reads_legacy_bare_json_as_version_0() {
        let path = temp_file("legacy");
        std::fs::write(&path, br#"{"panes":[{"command":"old","label":"l"}]}"#).unwrap();
        let ws = read_workspace(&path).expect("legacy bare file loads");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("old"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_wrong_format_with_a_clear_error() {
        let err = parse_workspace_str(r#"{"format":"notpanes","version":1,"workspace":{}}"#)
            .unwrap_err();
        assert!(
            err.contains("not a hyperpanes workspace") && err.contains("notpanes"),
            "error names the bad format: {err}"
        );
        // A non-string format is also a clear rejection, not a parse panic.
        let err = parse_workspace_str(r#"{"format":7,"version":1,"workspace":{}}"#).unwrap_err();
        assert!(err.contains("not a hyperpanes workspace"), "error: {err}");
    }

    #[test]
    fn rejects_future_or_missing_version_with_a_clear_error() {
        let err = parse_workspace_str(r#"{"format":"hyperpanes","version":2,"workspace":{}}"#)
            .unwrap_err();
        assert!(
            err.contains("version 2") && err.contains("newer"),
            "error explains the version gap: {err}"
        );
        let err = parse_workspace_str(r#"{"format":"hyperpanes","workspace":{}}"#).unwrap_err();
        assert!(err.contains("version"), "error mentions the missing version: {err}");
    }

    #[test]
    fn rejects_envelope_without_workspace_payload() {
        let err = parse_workspace_str(r#"{"format":"hyperpanes","version":1}"#).unwrap_err();
        assert!(err.contains("workspace"), "error names the missing payload: {err}");
    }

    #[test]
    fn envelope_round_trips_byte_identically_through_pretty_printing() {
        // The 2-space byte-identical contract re-stated for the container.
        let json = r#"{
  "format": "hyperpanes",
  "version": 1,
  "workspace": {
    "name": "dev",
    "panes": [
      {
        "command": "npm run dev"
      }
    ]
  }
}"#;
        let parsed: WorkspaceEnvelope = serde_json::from_str(json).expect("parse");
        let out = serde_json::to_string_pretty(&parsed).expect("serialize");
        assert_eq!(out, json, "envelope round-trip mismatch");
        // And the tolerant reader extracts the same inner payload.
        assert_eq!(parse_workspace_str(json).unwrap(), parsed.workspace);
    }

    #[test]
    fn read_rejects_invalid_envelope_files() {
        // End-to-end: a wrong-format file on disk reads as None (with a logged error).
        let path = temp_file("badformat");
        std::fs::write(
            &path,
            br#"{"format":"other","version":1,"workspace":{"panes":[{"command":"x"}]}}"#,
        )
        .unwrap();
        assert!(read_workspace(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_cwds_walks_groups_and_windows() {
        let base = if cfg!(windows) { "C:\\base" } else { "/base" };
        let ws = WorkspaceFile {
            groups: Some(vec![GroupSpec {
                panes: vec![PaneSpec {
                    cwd: Some("g".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            windows: Some(vec![WindowSpec {
                groups: vec![GroupSpec {
                    panes: vec![PaneSpec {
                        cwd: Some("w".into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }]),
            ..Default::default()
        };
        let out = resolve_cwds(&ws, base);
        assert!(out.groups.unwrap()[0].panes[0]
            .cwd
            .as_deref()
            .unwrap()
            .ends_with("g"));
        assert!(out.windows.unwrap()[0].groups[0].panes[0]
            .cwd
            .as_deref()
            .unwrap()
            .ends_with("w"));
    }
}
