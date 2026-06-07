//! Port of `src/main/control-scope.ts` — token scope validation:
//! `paneInScope` / `windowInScope` / `tabInScope` / `checkMintable` / `coerceScope`,
//! including the no-escalation rules and the active-tab command exception.
//! Mirror every case in `control-scope.test.ts`. Preserve message strings verbatim
//! (MCP may surface them).
//!
//! Capability scoping for the control API (agent-orchestration F). A token is
//! either the master token (unscoped — the root/CEO, written to control.json) or
//! a minted token carrying a Scope that limits which panes/tabs/windows it can
//! reach. Scoping is opt-in: the single-orchestrator case ignores it entirely;
//! recursive orgs hand each manager a subtree-scoped token.
//!
//! These are the pure predicates; ControlServer applies them on every route and
//! uses the live pane tree to validate sub-scope minting (canMint).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A scope names allowed targets at any level. A pane is reachable if it matches
/// on ANY level (its own id, its tab, or its window). Empty/absent arrays match
/// nothing at that level. A `None` scope (master) means unscoped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub window_ids: Option<Vec<i64>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tab_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub pane_ids: Option<Vec<String>>,
}

/// A pane's addressing coordinates, from the server read-model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneCoords {
    pub pane_id: String,
    pub tab_id: String,
    pub window_id: i64,
}

/// The live pane tree used by `check_mintable` to resolve ids and validate
/// sub-scope minting (the TS `tree` object of closures).
pub trait ScopeTree {
    fn pane_coords(&self, pane_id: &str) -> Option<PaneCoords>;
    fn tab_window(&self, tab_id: &str) -> Option<i64>;
    fn has_window(&self, window_id: i64) -> bool;
}

/// Whether `scope` (`None` = unscoped/master) may touch a specific pane.
pub fn pane_in_scope(scope: Option<&Scope>, c: &PaneCoords) -> bool {
    let scope = match scope {
        None => return true,
        Some(s) => s,
    };
    scope
        .pane_ids
        .as_ref()
        .is_some_and(|v| v.contains(&c.pane_id))
        || scope.tab_ids.as_ref().is_some_and(|v| v.contains(&c.tab_id))
        || scope
            .window_ids
            .as_ref()
            .is_some_and(|v| v.contains(&c.window_id))
}

/// Whether `scope` may act on a whole window (e.g. a window-targeted command).
pub fn window_in_scope(scope: Option<&Scope>, window_id: i64) -> bool {
    let scope = match scope {
        None => return true,
        Some(s) => s,
    };
    scope
        .window_ids
        .as_ref()
        .is_some_and(|v| v.contains(&window_id))
}

/// Whether `scope` may act on a tab (its tab id, or its owning window).
pub fn tab_in_scope(scope: Option<&Scope>, tab_id: &str, window_id: i64) -> bool {
    let scope = match scope {
        None => return true,
        Some(s) => s,
    };
    scope
        .tab_ids
        .as_ref()
        .is_some_and(|v| v.iter().any(|t| t == tab_id))
        || scope
            .window_ids
            .as_ref()
            .is_some_and(|v| v.contains(&window_id))
}

/// Validate a requested scope: every named id must resolve to a real target and
/// be reachable by the minter's scope (so a parent can only mint NARROWER tokens
/// — no privilege escalation). `tree` comes from the live tree; unknown ids are
/// rejected. Returns the first problem, or `None` if OK.
pub fn check_mintable(
    parent: Option<&Scope>,
    child: &Scope,
    tree: &dyn ScopeTree,
) -> Option<String> {
    if let Some(windows) = &child.window_ids {
        for &w in windows {
            if !tree.has_window(w) {
                return Some(format!("unknown windowId {w}"));
            }
            if !window_in_scope(parent, w) {
                return Some(format!("windowId {w} is outside the minting token's scope"));
            }
        }
    }
    if let Some(tabs) = &child.tab_ids {
        for t in tabs {
            let win = match tree.tab_window(t) {
                None => return Some(format!("unknown tabId {t}")),
                Some(w) => w,
            };
            if !tab_in_scope(parent, t, win) {
                return Some(format!("tabId {t} is outside the minting token's scope"));
            }
        }
    }
    if let Some(panes) = &child.pane_ids {
        for p in panes {
            let coords = match tree.pane_coords(p) {
                None => return Some(format!("unknown paneId {p}")),
                Some(c) => c,
            };
            if !pane_in_scope(parent, &coords) {
                return Some(format!("paneId {p} is outside the minting token's scope"));
            }
        }
    }
    let has_any = child.window_ids.as_ref().is_some_and(|v| !v.is_empty())
        || child.tab_ids.as_ref().is_some_and(|v| !v.is_empty())
        || child.pane_ids.as_ref().is_some_and(|v| !v.is_empty());
    if !has_any {
        return Some("scope must name at least one windowId, tabId, or paneId".to_string());
    }
    None
}

/// Validate + normalize an untrusted scope payload (from JSON over `/tokens`).
/// Drops non-arrays / wrong element types; returns `None` if nothing usable.
pub fn coerce_scope(v: &Value) -> Option<Scope> {
    let obj = v.as_object()?;
    let nums = |key: &str| -> Option<Vec<i64>> {
        obj.get(key)
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_i64).collect())
    };
    let strs = |key: &str| -> Option<Vec<String>> {
        obj.get(key).and_then(Value::as_array).map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str().map(str::to_owned))
                .collect()
        })
    };
    let mut scope = Scope::default();
    if let Some(w) = nums("windowIds") {
        if !w.is_empty() {
            scope.window_ids = Some(w);
        }
    }
    if let Some(t) = strs("tabIds") {
        if !t.is_empty() {
            scope.tab_ids = Some(t);
        }
    }
    if let Some(p) = strs("paneIds") {
        if !p.is_empty() {
            scope.pane_ids = Some(p);
        }
    }
    if scope.window_ids.is_some() || scope.tab_ids.is_some() || scope.pane_ids.is_some() {
        Some(scope)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn coords(pane_id: &str, tab_id: &str, window_id: i64) -> PaneCoords {
        PaneCoords {
            pane_id: pane_id.to_string(),
            tab_id: tab_id.to_string(),
            window_id,
        }
    }

    fn scope_panes(ids: &[&str]) -> Scope {
        Scope {
            pane_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }
    fn scope_tabs(ids: &[&str]) -> Scope {
        Scope {
            tab_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }
    fn scope_windows(ids: &[i64]) -> Scope {
        Scope {
            window_ids: Some(ids.to_vec()),
            ..Default::default()
        }
    }

    // --- scope predicates ---------------------------------------------------

    #[test]
    fn null_scope_master_reaches_everything() {
        assert!(pane_in_scope(None, &coords("p", "t", 1)));
        assert!(window_in_scope(None, 99));
        assert!(tab_in_scope(None, "t", 1));
    }

    #[test]
    fn pane_in_scope_matches_on_pane_tab_or_window_level() {
        assert!(pane_in_scope(Some(&scope_panes(&["p1"])), &coords("p1", "t", 1)));
        assert!(pane_in_scope(Some(&scope_tabs(&["t1"])), &coords("p9", "t1", 1)));
        assert!(pane_in_scope(Some(&scope_windows(&[2])), &coords("p9", "t9", 2)));
        assert!(!pane_in_scope(Some(&scope_panes(&["p1"])), &coords("p2", "t", 1)));
        assert!(!pane_in_scope(Some(&scope_tabs(&["t1"])), &coords("p", "t2", 1)));
    }

    #[test]
    fn window_and_tab_predicates() {
        assert!(window_in_scope(Some(&scope_windows(&[1])), 1));
        // a tab scope grants no whole window
        assert!(!window_in_scope(Some(&scope_tabs(&["t"])), 1));
        assert!(tab_in_scope(Some(&scope_tabs(&["t1"])), "t1", 5));
        // window scope covers its tabs
        assert!(tab_in_scope(Some(&scope_windows(&[5])), "t1", 5));
    }

    // --- checkMintable (no privilege escalation) ----------------------------

    struct TestTree;
    impl ScopeTree for TestTree {
        fn pane_coords(&self, pane_id: &str) -> Option<PaneCoords> {
            match pane_id {
                "p1" => Some(coords("p1", "t1", 1)),
                "p2" => Some(coords("p2", "t2", 2)),
                _ => None,
            }
        }
        fn tab_window(&self, tab_id: &str) -> Option<i64> {
            match tab_id {
                "t1" => Some(1),
                "t2" => Some(2),
                _ => None,
            }
        }
        fn has_window(&self, window_id: i64) -> bool {
            window_id == 1 || window_id == 2
        }
    }

    #[test]
    fn master_mints_any_real_sub_scope() {
        assert_eq!(check_mintable(None, &scope_panes(&["p1"]), &TestTree), None);
        assert_eq!(check_mintable(None, &scope_windows(&[2]), &TestTree), None);
    }

    #[test]
    fn rejects_unknown_ids() {
        assert!(check_mintable(None, &scope_panes(&["ghost"]), &TestTree)
            .unwrap()
            .contains("unknown paneId"));
        assert!(check_mintable(None, &scope_tabs(&["nope"]), &TestTree)
            .unwrap()
            .contains("unknown tabId"));
        assert!(check_mintable(None, &scope_windows(&[9]), &TestTree)
            .unwrap()
            .contains("unknown windowId"));
    }

    #[test]
    fn rejects_an_empty_scope() {
        assert!(check_mintable(None, &Scope::default(), &TestTree)
            .unwrap()
            .contains("at least one"));
    }

    #[test]
    fn window_scoped_parent_may_mint_pane_in_that_window_but_not_another() {
        let parent = scope_windows(&[1]);
        // p1 ∈ window 1
        assert_eq!(
            check_mintable(Some(&parent), &scope_panes(&["p1"]), &TestTree),
            None
        );
        // p2 ∈ window 2
        assert!(
            check_mintable(Some(&parent), &scope_panes(&["p2"]), &TestTree)
                .unwrap()
                .contains("outside")
        );
        assert!(
            check_mintable(Some(&parent), &scope_windows(&[2]), &TestTree)
                .unwrap()
                .contains("outside")
        );
    }

    // --- coerceScope --------------------------------------------------------

    #[test]
    fn coerce_keeps_well_typed_arrays_drops_junk_returns_none_when_empty() {
        assert_eq!(
            coerce_scope(&json!({ "paneIds": ["a", 1, "b"], "tabIds": "x", "windowIds": [2, "3"] })),
            Some(Scope {
                pane_ids: Some(vec!["a".to_string(), "b".to_string()]),
                window_ids: Some(vec![2]),
                tab_ids: None,
            })
        );
        assert_eq!(coerce_scope(&json!({ "paneIds": [] })), None);
        assert_eq!(coerce_scope(&Value::Null), None);
        assert_eq!(coerce_scope(&json!("nope")), None);
    }
}
