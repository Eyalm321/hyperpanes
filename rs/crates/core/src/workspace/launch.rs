//! Port of `resolveLaunchWorkspace` / `getInitialWorkspace` / `getInitialWindows`
//! from `src/main/workspace.ts` — resolve the workspace to open on launch.
//!
//! Precedence (mirrors the TS): inline `-c` flags win, then an explicit positional
//! `.json`, then the last session (`last-workspace.json`); inline workspaces have
//! their relative cwds resolved against the launch directory. `get_initial_windows`
//! normalises the result through `windows_of` (last-session restore included).

use crate::cli::parse::parse_cli;
use crate::persistence::paths;
use crate::workspace::io;
use crate::workspace::model::{WindowSpec, WorkspaceFile};
use std::path::Path;

/// What to load on launch, parameterized by `argv` + `cwd` (so it also serves the
/// `second-instance` event). Relative cwds resolve against `cwd`.
pub fn resolve_launch_workspace(argv: &[String], cwd: &str) -> Option<WorkspaceFile> {
    resolve_launch_workspace_with(argv, cwd, &paths::last_workspace_json())
}

/// Resolve ONLY an explicitly-requested launch workspace from `argv` — an inline `-c …` flag
/// set or a positional `.json` — WITHOUT the last-session fallback. The headless core
/// bootstrap uses this (argv-only); the native GUI uses [`resolve_launch_workspace`] so a
/// plain relaunch restores the last session (#14 — the GUI writes `last-workspace.json`
/// when its final window closes). Relative cwds resolve against `cwd`.
pub fn resolve_cli_workspace(argv: &[String], cwd: &str) -> Option<WorkspaceFile> {
    let parsed = parse_cli(argv);
    if let Some(ws) = parsed.workspace {
        return Some(io::resolve_cwds(&ws, cwd));
    }
    if let Some(json_path) = parsed.json_path {
        return io::read_workspace(json_path);
    }
    None
}

/// The launch resolution with the last-session path injected (for testability). Inline / explicit
/// `.json` (via [`resolve_cli_workspace`]) win; otherwise fall back to the last session.
fn resolve_launch_workspace_with(
    argv: &[String],
    cwd: &str,
    last_path: &Path,
) -> Option<WorkspaceFile> {
    if let Some(ws) = resolve_cli_workspace(argv, cwd) {
        return Some(ws);
    }
    if last_path.exists() {
        io::read_workspace(last_path)
    } else {
        None
    }
}

/// What to load on launch from this process's own argv + cwd.
pub fn get_initial_workspace() -> Option<WorkspaceFile> {
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    resolve_launch_workspace(&argv, &cwd)
}

/// The window list to open on first launch (last-session restore included).
pub fn get_initial_windows() -> Vec<WindowSpec> {
    io::windows_of(get_initial_workspace().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["/path/to/hyperpanes".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    fn temp_file(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hp-launch-{}-{tag}.json", std::process::id()))
    }

    #[test]
    fn inline_flags_win_over_everything() {
        // A present last-session file must be ignored when inline `-c` is given.
        let last = temp_file("inline-last");
        std::fs::write(&last, br#"{"panes":[{"command":"old"}]}"#).unwrap();
        let ws = resolve_launch_workspace_with(&argv(&["-c", "npm run dev"]), ".", &last)
            .expect("inline workspace");
        let panes = ws.panes.unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].command.as_deref(), Some("npm run dev"));
        let _ = std::fs::remove_file(&last);
    }

    #[test]
    fn falls_back_to_last_session_when_no_args() {
        let last = temp_file("fallback-last");
        std::fs::write(&last, br#"{"panes":[{"command":"restored","label":"r"}]}"#).unwrap();
        let ws = resolve_launch_workspace_with(&argv(&[]), ".", &last).expect("last session");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("restored"));
        let _ = std::fs::remove_file(&last);
    }

    #[test]
    fn returns_none_with_no_args_and_no_last_session() {
        let last = temp_file("none-last");
        let _ = std::fs::remove_file(&last);
        assert!(resolve_launch_workspace_with(&argv(&[]), ".", &last).is_none());
    }

    #[test]
    fn cli_workspace_takes_inline_but_never_last_session() {
        // `resolve_cli_workspace` is what the GUI uses: inline `-c` resolves…
        let ws = resolve_cli_workspace(&argv(&["-c", "npm run dev"]), ".").expect("inline workspace");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("npm run dev"));
        // …but a plain launch yields None even though a last-session file exists on disk (the GUI
        // must stay EmptyTab on a bare launch — that fallback belongs only to resolve_launch_workspace).
        assert!(resolve_cli_workspace(&argv(&[]), ".").is_none());
    }

    #[test]
    fn json_path_is_read_when_present() {
        let json = temp_file("explicit");
        std::fs::write(&json, br#"{"panes":[{"command":"fromfile","label":"f"}]}"#).unwrap();
        let json_str = json.to_string_lossy().into_owned();
        let no_last = temp_file("explicit-nolast");
        let _ = std::fs::remove_file(&no_last);
        let ws = resolve_launch_workspace_with(&argv(&[&json_str]), ".", &no_last)
            .expect("workspace from json path");
        assert_eq!(ws.panes.unwrap()[0].command.as_deref(), Some("fromfile"));
        let _ = std::fs::remove_file(&json);
    }
}
