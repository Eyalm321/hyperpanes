//! Port of `parseCli` in `src/main/workspace.ts` — the positional/stateful CLI
//! grammar (window → tab → pane state machine). Hand-rolled, NOT clap.
//!
//! Behaviours preserved 1:1 from the TS source:
//!   - `-l/--color/--cwd/--shell/--font` attach to the most recent `-c`/`--command`;
//!   - `--cwd/--shell` seen before any `-c` are launch-wide defaults applied to
//!     every pane lacking its own; `--layout` before a tab is a pending layout;
//!   - `--name` titles the just-opened window/tab (header scope), else the
//!     workspace name (before any separator), else the current tab/window;
//!   - `--window`/`--tab` separators switch the output to the `windows` shape;
//!     with no separator the legacy single-window `{ name, layout, panes }` shape;
//!   - launch routing precedence: explicit `--attach`/`--as` wins → attach;
//!     else `--new-window` or any `--window` separator → new window(s);
//!     else default → attach to the focused window as a new tab;
//!   - label default `command.trim().split(/\s+/)[0] || "shell"`;
//!   - a positional path is captured only when it (case-insensitively) ends in
//!     `.json` or `.hyperpanes` AND `exists_fn` reports it present, resolved to
//!     an absolute path.

use crate::workspace::model::{GroupSpec, PaneSpec, WindowSpec, WorkspaceFile};
use std::path::{Component, Path, PathBuf};

/// Which existing window an attach targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoutingTarget {
    /// the last-focused window (`--attach`, `--attach=focused|current`, default)
    Focused,
    /// the most-recent window (`--attach=last`)
    Last,
    /// a specific BrowserWindow id (`--attach=<id>`)
    Id(i64),
}

/// The unit merged into an attach target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachAs {
    Tab,
    Panes,
}

/// Where a second `hyperpanes …` launch puts its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchRouting {
    NewWindow,
    Attach { target: RoutingTarget, as_: AttachAs },
}

/// The result of parsing a launch command line.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedCli {
    /// A workspace assembled from inline flags (`-c`, `--layout`, …), or `None`.
    pub workspace: Option<WorkspaceFile>,
    /// A positional workspace path (`.json` or `.hyperpanes`), resolved to absolute,
    /// e.g. `hyperpanes ./dev.hyperpanes`.
    pub json_path: Option<String>,
    /// New window vs attach-to-existing for this invocation.
    pub routing: LaunchRouting,
}

// ---- internal builder state (mirrors the TS CliWin/CliTab + `cur` cursor) ----

#[derive(Default)]
struct CliTab {
    title: Option<String>,
    layout: Option<String>,
    panes: Vec<PaneSpec>,
}

#[derive(Default)]
struct CliWin {
    title: Option<String>,
    tabs: Vec<CliTab>,
}

/// Coerce a `--attach=<target>` value into a routing target. Bare/`focused`/
/// `current` → focused; `last` → last; a leading integer → that id; else focused.
fn parse_routing_target(v: &str) -> RoutingTarget {
    let s = v.to_lowercase();
    if s == "last" {
        return RoutingTarget::Last;
    }
    if s.is_empty() || s == "focused" || s == "current" {
        return RoutingTarget::Focused;
    }
    match parse_int(v) {
        Some(n) => RoutingTarget::Id(n),
        None => RoutingTarget::Focused,
    }
}

/// Mimic JS `parseInt(v, 10)`: skip leading whitespace, optional sign, then read
/// the leading run of decimal digits. `None` when no digit is found (NaN).
fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let bytes = t.as_bytes();
    let mut idx = 0;
    let mut neg = false;
    if idx < bytes.len() && (bytes[idx] == b'+' || bytes[idx] == b'-') {
        neg = bytes[idx] == b'-';
        idx += 1;
    }
    let start = idx;
    while idx < bytes.len() && bytes[idx].is_ascii_digit() {
        idx += 1;
    }
    if idx == start {
        return None;
    }
    t[start..idx]
        .parse::<i64>()
        .ok()
        .map(|n| if neg { -n } else { n })
}

/// Mimic node `path.resolve(p)` for a single segment: make absolute against the
/// process cwd, then normalise away `.` / `..` components.
fn resolve_path(p: &str) -> String {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out.to_string_lossy().into_owned()
}

/// Parse a launch command line into a workspace + routing. `argv[0]` is the
/// program path (ignored, as in the TS port). `exists_fn` decides whether a
/// positional `.json`/`.hyperpanes` is real (injected for testability).
pub fn parse_cli(argv: &[String]) -> ParsedCli {
    parse_cli_with(argv, |p| Path::new(p).exists())
}

pub fn parse_cli_with(argv: &[String], exists_fn: impl Fn(&str) -> bool) -> ParsedCli {
    let args: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    let mut windows: Vec<CliWin> = Vec::new();
    // Cursors as indices into `windows` (and nested tabs/panes).
    let mut cur_win: Option<usize> = None;
    let mut cur_tab: Option<usize> = None;
    let mut cur_pane: Option<usize> = None;

    let mut header_scope: Option<&'static str> = None; // "window" | "tab"
    let mut pending_layout: Option<String> = None;
    let mut explicit_structure = false;
    let mut used_window_separator = false;
    let mut routing_new_window = false;
    let mut routing_attach = false;
    let mut routing_target = RoutingTarget::Focused;
    let mut routing_as = AttachAs::Tab;
    let mut routing_as_set = false;
    let mut name: Option<String> = None;
    let mut default_cwd: Option<String> = None;
    let mut default_shell: Option<String> = None;
    let mut json_path: Option<String> = None;

    let mut i: usize = 0;
    while i < args.len() {
        let a = args[i].clone();

        // Launch-routing flags are handled before the structural switch so the
        // `--flag=value` form works; each path `continue`s past the switch.
        let (head, inline): (&str, Option<&str>) = match a.find('=') {
            Some(eq) => (&a[..eq], Some(&a[eq + 1..])),
            None => (a.as_str(), None),
        };

        if head == "--new-window" {
            routing_new_window = true;
            i += 1;
            continue;
        }
        if head == "--attach" || head == "--into-current" {
            routing_attach = true;
            routing_target = parse_routing_target(inline.unwrap_or(""));
            i += 1;
            continue;
        }
        if head == "--as" {
            // `inline ?? value()` — only consume the next arg when no inline value.
            let v = match inline {
                Some(s) => s.to_string(),
                None => {
                    i += 1;
                    args.get(i).cloned().unwrap_or_default()
                }
            }
            .to_lowercase();
            if v == "panes" {
                routing_as = AttachAs::Panes;
                routing_as_set = true;
            } else if v == "tab" {
                routing_as = AttachAs::Tab;
                routing_as_set = true;
            }
            i += 1;
            continue;
        }

        match a.as_str() {
            "--window" => {
                open_window(&mut windows, &mut cur_win, &mut cur_tab, &mut cur_pane);
                header_scope = Some("window");
                explicit_structure = true;
                used_window_separator = true;
            }
            "--tab" => {
                open_tab(
                    &mut windows,
                    &mut cur_win,
                    &mut cur_tab,
                    &mut cur_pane,
                    &mut pending_layout,
                );
                header_scope = Some("tab");
                explicit_structure = true;
            }
            "-c" | "--command" => {
                ensure_tab(
                    &mut windows,
                    &mut cur_win,
                    &mut cur_tab,
                    &mut cur_pane,
                    &mut pending_layout,
                );
                i += 1;
                let cmd = args.get(i).cloned();
                let (w, t) = (cur_win.unwrap(), cur_tab.unwrap());
                let tab = &mut windows[w].tabs[t];
                tab.panes.push(PaneSpec {
                    command: cmd,
                    ..Default::default()
                });
                cur_pane = Some(tab.panes.len() - 1);
                header_scope = None;
            }
            "-l" | "--label" => {
                i += 1;
                let v = args.get(i).cloned();
                if let Some(p) = pane_mut(&mut windows, cur_win, cur_tab, cur_pane) {
                    p.label = v;
                }
            }
            "--color" => {
                i += 1;
                let v = args.get(i).cloned();
                if let Some(p) = pane_mut(&mut windows, cur_win, cur_tab, cur_pane) {
                    p.color = v;
                }
            }
            "--cwd" => {
                i += 1;
                let v = args.get(i).cloned();
                if let Some(p) = pane_mut(&mut windows, cur_win, cur_tab, cur_pane) {
                    p.cwd = v;
                } else {
                    default_cwd = v;
                }
            }
            "--shell" => {
                i += 1;
                let v = args.get(i).cloned();
                if let Some(p) = pane_mut(&mut windows, cur_win, cur_tab, cur_pane) {
                    p.shell = v;
                } else {
                    default_shell = v;
                }
            }
            "--font" => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_default();
                let n = parse_int(&v);
                if let Some(p) = pane_mut(&mut windows, cur_win, cur_tab, cur_pane) {
                    if let Some(n) = n {
                        if n >= 0 {
                            p.font_size = Some(n as u32);
                        }
                    }
                }
            }
            "--layout" => {
                i += 1;
                let v = args.get(i).cloned();
                if let Some(t) = tab_mut(&mut windows, cur_win, cur_tab) {
                    t.layout = v;
                } else {
                    pending_layout = v;
                }
            }
            "--name" => {
                i += 1;
                let v = args.get(i).cloned();
                if header_scope == Some("window") {
                    if let Some(w) = win_mut(&mut windows, cur_win) {
                        w.title = v;
                    }
                } else if header_scope == Some("tab") {
                    if let Some(t) = tab_mut(&mut windows, cur_win, cur_tab) {
                        t.title = v;
                    }
                } else if !explicit_structure {
                    name = v;
                } else if let Some(t) = tab_mut(&mut windows, cur_win, cur_tab) {
                    t.title = v;
                } else if let Some(w) = win_mut(&mut windows, cur_win) {
                    w.title = v;
                }
            }
            _ => {
                let lower = a.to_lowercase();
                if !a.starts_with('-')
                    && (lower.ends_with(".json") || lower.ends_with(".hyperpanes"))
                    && exists_fn(&a)
                {
                    json_path = Some(resolve_path(&a));
                }
            }
        }
        i += 1;
    }

    // Resolve routing (explicit attach/`--as` wins; else new-window; else attach).
    let routing = if routing_attach || routing_as_set {
        LaunchRouting::Attach {
            target: routing_target,
            as_: routing_as,
        }
    } else if routing_new_window || used_window_separator {
        LaunchRouting::NewWindow
    } else {
        LaunchRouting::Attach {
            target: RoutingTarget::Focused,
            as_: AttachAs::Tab,
        }
    };

    // Finish panes (label default + launch-wide cwd/shell), then prune empties.
    let total_panes: usize = windows
        .iter()
        .flat_map(|w| w.tabs.iter())
        .map(|t| t.panes.len())
        .sum();
    if total_panes == 0 {
        return ParsedCli {
            workspace: None,
            json_path,
            routing,
        };
    }
    for w in windows.iter_mut() {
        for t in w.tabs.iter_mut() {
            for p in t.panes.iter_mut() {
                if p.label.is_none() {
                    if let Some(cmd) = &p.command {
                        let first = cmd.split_whitespace().next().unwrap_or("shell");
                        let first = if first.is_empty() { "shell" } else { first };
                        p.label = Some(first.to_string());
                    }
                }
                if let Some(dc) = &default_cwd {
                    if p.cwd.is_none() {
                        p.cwd = Some(dc.clone());
                    }
                }
                if let Some(ds) = &default_shell {
                    if p.shell.is_none() {
                        p.shell = Some(ds.clone());
                    }
                }
            }
        }
    }

    // Prune tabs with no panes, then windows with no tabs.
    let pruned: Vec<CliWin> = windows
        .into_iter()
        .map(|w| CliWin {
            title: w.title,
            tabs: w.tabs.into_iter().filter(|t| !t.panes.is_empty()).collect(),
        })
        .filter(|w| !w.tabs.is_empty())
        .collect();

    if !explicit_structure {
        // Legacy single-window / single-tab shape.
        let mut pruned = pruned;
        let mut win = pruned.swap_remove(0);
        let tab = win.tabs.swap_remove(0);
        return ParsedCli {
            workspace: Some(WorkspaceFile {
                name,
                layout: tab.layout,
                panes: Some(tab.panes),
                ..Default::default()
            }),
            json_path,
            routing,
        };
    }

    let win_specs: Vec<WindowSpec> = pruned
        .into_iter()
        .map(|w| WindowSpec {
            title: w.title,
            groups: w
                .tabs
                .into_iter()
                .map(|t| GroupSpec {
                    title: t.title,
                    layout: t.layout,
                    panes: t.panes,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
        .collect();

    ParsedCli {
        workspace: Some(WorkspaceFile {
            name,
            windows: Some(win_specs),
            ..Default::default()
        }),
        json_path,
        routing,
    }
}

// ---- cursor helpers (the TS closures, reified over index cursors) ----

fn open_window(
    windows: &mut Vec<CliWin>,
    cur_win: &mut Option<usize>,
    cur_tab: &mut Option<usize>,
    cur_pane: &mut Option<usize>,
) {
    windows.push(CliWin::default());
    *cur_win = Some(windows.len() - 1);
    *cur_tab = None;
    *cur_pane = None;
}

fn open_tab(
    windows: &mut Vec<CliWin>,
    cur_win: &mut Option<usize>,
    cur_tab: &mut Option<usize>,
    cur_pane: &mut Option<usize>,
    pending_layout: &mut Option<String>,
) {
    if cur_win.is_none() {
        open_window(windows, cur_win, cur_tab, cur_pane);
    }
    let w = cur_win.unwrap();
    let mut tab = CliTab::default();
    if let Some(layout) = pending_layout.take() {
        tab.layout = Some(layout);
    }
    windows[w].tabs.push(tab);
    *cur_tab = Some(windows[w].tabs.len() - 1);
    *cur_pane = None;
}

fn ensure_tab(
    windows: &mut Vec<CliWin>,
    cur_win: &mut Option<usize>,
    cur_tab: &mut Option<usize>,
    cur_pane: &mut Option<usize>,
    pending_layout: &mut Option<String>,
) {
    if cur_tab.is_none() {
        open_tab(windows, cur_win, cur_tab, cur_pane, pending_layout);
    }
}

fn win_mut<'a>(windows: &'a mut [CliWin], cur_win: Option<usize>) -> Option<&'a mut CliWin> {
    cur_win.and_then(move |w| windows.get_mut(w))
}

fn tab_mut<'a>(
    windows: &'a mut [CliWin],
    cur_win: Option<usize>,
    cur_tab: Option<usize>,
) -> Option<&'a mut CliTab> {
    let w = cur_win?;
    let t = cur_tab?;
    windows.get_mut(w)?.tabs.get_mut(t)
}

fn pane_mut<'a>(
    windows: &'a mut [CliWin],
    cur_win: Option<usize>,
    cur_tab: Option<usize>,
    cur_pane: Option<usize>,
) -> Option<&'a mut PaneSpec> {
    let w = cur_win?;
    let t = cur_tab?;
    let p = cur_pane?;
    windows.get_mut(w)?.tabs.get_mut(t)?.panes.get_mut(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `argv('foo', 'bar')` → ["/path/to/hyperpanes", "foo", "bar"].
    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["/path/to/hyperpanes".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    fn pane(command: &str, label: &str) -> PaneSpec {
        PaneSpec {
            command: Some(command.into()),
            label: Some(label.into()),
            ..Default::default()
        }
    }

    fn attach_focused_tab() -> LaunchRouting {
        LaunchRouting::Attach {
            target: RoutingTarget::Focused,
            as_: AttachAs::Tab,
        }
    }

    // ---- describe('parseCli') ----

    #[test]
    fn returns_nothing_for_a_bare_launch() {
        let r = parse_cli(&argv(&[]));
        assert_eq!(
            r,
            ParsedCli {
                workspace: None,
                json_path: None,
                routing: attach_focused_tab(),
            }
        );
    }

    #[test]
    fn builds_panes_from_repeated_c_flags() {
        let r = parse_cli(&argv(&["-c", "npm run dev", "-c", "tail -f log"]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![pane("npm run dev", "npm"), pane("tail -f log", "tail")]
        );
    }

    #[test]
    fn attaches_label_and_color_to_the_most_recent_command() {
        let r = parse_cli(&argv(&[
            "-c", "npm run dev", "-l", "server", "--color", "#e5484d", "-c", "psql", "--label",
            "db",
        ]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![
                PaneSpec {
                    command: Some("npm run dev".into()),
                    label: Some("server".into()),
                    color: Some("#e5484d".into()),
                    ..Default::default()
                },
                PaneSpec {
                    command: Some("psql".into()),
                    label: Some("db".into()),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn reads_layout_name_and_applies_cwd_to_panes_without_one() {
        let r = parse_cli(&argv(&[
            "--name", "dev", "--layout", "main-stack", "--cwd", "/work", "-c", "bash",
        ]));
        assert_eq!(
            r.workspace.unwrap(),
            WorkspaceFile {
                name: Some("dev".into()),
                layout: Some("main-stack".into()),
                panes: Some(vec![PaneSpec {
                    command: Some("bash".into()),
                    label: Some("bash".into()),
                    cwd: Some("/work".into()),
                    ..Default::default()
                }]),
                ..Default::default()
            }
        );
    }

    #[test]
    fn applies_shell_as_a_launch_wide_default() {
        let r = parse_cli(&argv(&["--shell", "pwsh", "-c", "npm run dev", "-c", "top"]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![
                PaneSpec {
                    command: Some("npm run dev".into()),
                    label: Some("npm".into()),
                    shell: Some("pwsh".into()),
                    ..Default::default()
                },
                PaneSpec {
                    command: Some("top".into()),
                    label: Some("top".into()),
                    shell: Some("pwsh".into()),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn captures_a_positional_json_path_that_exists() {
        let exists = |p: &str| p == "./dev.json";
        let r = parse_cli_with(&argv(&["./dev.json"]), exists);
        assert!(r.workspace.is_none());
        assert!(
            r.json_path.as_deref().unwrap().ends_with("dev.json"),
            "json_path should resolve to an absolute path ending in dev.json: {:?}",
            r.json_path
        );
    }

    #[test]
    fn captures_a_positional_hyperpanes_path_that_exists() {
        let exists = |p: &str| p == "./dev.hyperpanes";
        let r = parse_cli_with(&argv(&["./dev.hyperpanes"]), exists);
        assert!(
            r.json_path.as_deref().unwrap().ends_with("dev.hyperpanes"),
            "json_path should resolve to an absolute path ending in dev.hyperpanes: {:?}",
            r.json_path
        );
        // Case-insensitive, like the .json check.
        let exists = |p: &str| p == "./DEV.HYPERPANES";
        let r = parse_cli_with(&argv(&["./DEV.HYPERPANES"]), exists);
        assert!(r.json_path.is_some());
    }

    #[test]
    fn ignores_a_json_path_that_does_not_exist() {
        let r = parse_cli_with(&argv(&["./missing.json"]), |_| false);
        assert!(r.json_path.is_none());
        let r = parse_cli_with(&argv(&["./missing.hyperpanes"]), |_| false);
        assert!(r.json_path.is_none());
    }

    #[test]
    fn attaches_per_pane_cwd_shell_font_to_the_most_recent_c() {
        let r = parse_cli(&argv(&[
            "-c", "npm run dev", "--cwd", "/app", "--shell", "pwsh", "--font", "14", "-c", "top",
        ]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![
                PaneSpec {
                    command: Some("npm run dev".into()),
                    label: Some("npm".into()),
                    cwd: Some("/app".into()),
                    shell: Some("pwsh".into()),
                    font_size: Some(14),
                    ..Default::default()
                },
                pane("top", "top"),
            ]
        );
    }

    #[test]
    fn keeps_cwd_shell_before_any_c_as_launch_wide_defaults() {
        let r = parse_cli(&argv(&["--cwd", "/work", "-c", "a", "-c", "b", "--cwd", "/b"]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![
                PaneSpec {
                    command: Some("a".into()),
                    label: Some("a".into()),
                    cwd: Some("/work".into()),
                    ..Default::default()
                },
                PaneSpec {
                    command: Some("b".into()),
                    label: Some("b".into()),
                    cwd: Some("/b".into()), // a per-pane --cwd overrides the default
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn builds_multiple_tabs_in_one_window_with_tab() {
        let r = parse_cli(&argv(&[
            "--tab", "--name", "app", "-c", "a", "--tab", "--name", "logs", "-c", "b",
        ]));
        assert_eq!(
            r.workspace.unwrap().windows.unwrap(),
            vec![WindowSpec {
                title: None,
                groups: vec![
                    GroupSpec {
                        title: Some("app".into()),
                        panes: vec![pane("a", "a")],
                        ..Default::default()
                    },
                    GroupSpec {
                        title: Some("logs".into()),
                        panes: vec![pane("b", "b")],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }]
        );
    }

    #[test]
    fn builds_multiple_windows_with_window_titling_each() {
        let r = parse_cli(&argv(&[
            "--window", "--name", "one", "--layout", "grid", "-c", "a", "--window", "--name",
            "two", "-c", "b",
        ]));
        let ws = r.workspace.unwrap();
        assert_eq!(
            ws.windows.clone().unwrap(),
            vec![
                WindowSpec {
                    title: Some("one".into()),
                    groups: vec![GroupSpec {
                        layout: Some("grid".into()),
                        panes: vec![pane("a", "a")],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                WindowSpec {
                    title: Some("two".into()),
                    groups: vec![GroupSpec {
                        panes: vec![pane("b", "b")],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ]
        );
        assert!(ws.panes.is_none()); // windows shape, not legacy
    }

    #[test]
    fn drops_window_tab_that_never_got_a_pane() {
        let r = parse_cli(&argv(&["--window", "--tab", "--window", "-c", "only"]));
        assert_eq!(
            r.workspace.unwrap().windows.unwrap(),
            vec![WindowSpec {
                groups: vec![GroupSpec {
                    panes: vec![pane("only", "only")],
                    ..Default::default()
                }],
                ..Default::default()
            }]
        );
    }

    // ---- describe('parseCli routing') ----

    #[test]
    fn defaults_bare_legacy_launch_to_attach_focused_as_tab() {
        let r = parse_cli(&argv(&["-c", "npm run dev"]));
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn defaults_a_tab_only_launch_to_attach() {
        let r = parse_cli(&argv(&["--tab", "-c", "a", "--tab", "-c", "b"]));
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn treats_a_window_separator_as_new_window_intent_by_default() {
        let r = parse_cli(&argv(&["--window", "-c", "a"]));
        assert_eq!(r.routing, LaunchRouting::NewWindow);
    }

    #[test]
    fn honors_an_explicit_new_window_flag() {
        let r = parse_cli(&argv(&["--new-window", "-c", "a"]));
        assert_eq!(r.routing, LaunchRouting::NewWindow);
    }

    #[test]
    fn attach_forces_attach_even_with_a_window_separator() {
        let r = parse_cli(&argv(&["--attach", "--window", "-c", "a"]));
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn parses_attach_last_and_attach_id_targets() {
        assert_eq!(
            parse_cli(&argv(&["--attach=last", "-c", "a"])).routing,
            LaunchRouting::Attach {
                target: RoutingTarget::Last,
                as_: AttachAs::Tab,
            }
        );
        assert_eq!(
            parse_cli(&argv(&["--attach=3", "-c", "a"])).routing,
            LaunchRouting::Attach {
                target: RoutingTarget::Id(3),
                as_: AttachAs::Tab,
            }
        );
    }

    #[test]
    fn as_panes_implies_attach_and_sets_the_unit() {
        let r = parse_cli(&argv(&["--as", "panes", "-c", "a"]));
        assert_eq!(
            r.routing,
            LaunchRouting::Attach {
                target: RoutingTarget::Focused,
                as_: AttachAs::Panes,
            }
        );
    }

    #[test]
    fn treats_into_current_as_attach_to_the_focused_window() {
        let r = parse_cli(&argv(&["--into-current", "-c", "a"]));
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn does_not_let_a_routing_flag_leak_into_the_parsed_panes() {
        let r = parse_cli(&argv(&["--new-window", "--as", "panes", "-c", "npm run dev"]));
        assert_eq!(
            r.workspace.unwrap().panes.unwrap(),
            vec![pane("npm run dev", "npm")]
        );
    }
}
