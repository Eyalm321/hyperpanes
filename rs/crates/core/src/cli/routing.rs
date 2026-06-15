//! Port of the second-instance launch-routing in `src/main/workspace.ts`:
//! `resolveSecondInstanceWindows` — a 2nd `hyperpanes …` invocation's argv + cwd → the
//! window specs to open plus the routing to apply (new window vs attach).
//!
//! REUSES the routing enums and parser already defined in `crate::cli::parse`
//! (`RoutingTarget` / `AttachAs` / `LaunchRouting` / `parse_cli`) — nothing redefined
//! here. The Electron-specific `routeLaunch` (BrowserWindow placement) is UI and is
//! out of scope.
//!
//! Parity rules: CLI/json only — NO last-session fallback (a relaunch with no args
//! should just focus, not reopen the saved session), so `windows` is empty when the
//! relaunch carried no content. A `.json` launch is always new-window (a file
//! describes whole windows), overriding whatever routing the flags parsed to.

use crate::cli::parse::{parse_cli, LaunchRouting};
use crate::workspace::io;
use crate::workspace::model::WindowSpec;

/// The windows a second invocation wants, plus how to route them.
#[derive(Debug, Clone, PartialEq)]
pub struct SecondInstance {
    pub windows: Vec<WindowSpec>,
    pub routing: LaunchRouting,
}

/// Resolve a second `hyperpanes …` invocation (its `argv` + `cwd`) into windows +
/// routing. No last-session fallback.
pub fn resolve_second_instance_windows(argv: &[String], cwd: &str) -> SecondInstance {
    let parsed = parse_cli(argv);

    if let Some(ws) = parsed.workspace {
        let resolved = io::resolve_cwds(&ws, cwd);
        return SecondInstance {
            windows: io::windows_of(Some(&resolved)),
            routing: parsed.routing,
        };
    }

    if let Some(json_path) = parsed.json_path {
        let file = io::read_workspace(json_path);
        return SecondInstance {
            windows: io::windows_of(file.as_ref()),
            routing: LaunchRouting::NewWindow,
        };
    }

    // No content: caller just focuses; routing is whatever the flags implied.
    SecondInstance {
        windows: Vec::new(),
        routing: parsed.routing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::parse::{AttachAs, RoutingTarget};

    fn argv(rest: &[&str]) -> Vec<String> {
        let mut v = vec!["/path/to/hyperpanes".to_string()];
        v.extend(rest.iter().map(|s| s.to_string()));
        v
    }

    fn attach_focused_tab() -> LaunchRouting {
        LaunchRouting::Attach {
            target: RoutingTarget::Focused,
            as_: AttachAs::Tab,
        }
    }

    #[test]
    fn bare_relaunch_carries_no_windows_and_attaches() {
        let r = resolve_second_instance_windows(&argv(&[]), ".");
        assert_eq!(r.windows, vec![]);
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn inline_legacy_launch_attaches_one_window() {
        let r = resolve_second_instance_windows(&argv(&["-c", "npm run dev"]), ".");
        // Legacy single-window shape → one window with one tab.
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].groups.len(), 1);
        assert_eq!(
            r.windows[0].groups[0].panes[0].command.as_deref(),
            Some("npm run dev")
        );
        assert_eq!(r.routing, attach_focused_tab());
    }

    #[test]
    fn window_separator_routes_to_new_window() {
        let r = resolve_second_instance_windows(&argv(&["--window", "-c", "a"]), ".");
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.routing, LaunchRouting::NewWindow);
    }

    #[test]
    fn as_panes_forces_attach_panes() {
        let r = resolve_second_instance_windows(&argv(&["--as", "panes", "-c", "a"]), ".");
        assert_eq!(
            r.routing,
            LaunchRouting::Attach {
                target: RoutingTarget::Focused,
                as_: AttachAs::Panes,
            }
        );
    }

    #[test]
    fn json_launch_is_always_new_window() {
        let json =
            std::env::temp_dir().join(format!("hp-routing-{}-launch.json", std::process::id()));
        std::fs::write(&json, br#"{"panes":[{"command":"x","label":"x"}]}"#).unwrap();
        let json_str = json.to_string_lossy().into_owned();
        let r = resolve_second_instance_windows(&argv(&[&json_str]), ".");
        assert_eq!(r.routing, LaunchRouting::NewWindow);
        assert_eq!(r.windows.len(), 1);
        let _ = std::fs::remove_file(&json);
    }
}
