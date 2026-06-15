//! The CENTRAL read-model — AUTHORITATIVE here (not an Electron renderer mirror). Holds the
//! windows → tabs → panes tree + reverse indexes (uidToPane / paneIndex / tabToWindow) rebuilt
//! on STRUCTURE change only, a per-window structural fingerprint, and per-pane `activity`
//! (busy/idle/exited) computed centrally from `session_manager` `last_output_at` at the
//! `idleAlertSeconds` threshold. Serializes the EXACT `/state` JSON (PaneState:
//! id/sessionUid/label/color/command?/args?/cwd?/shell?/subtitle?/status/exitCode?/activity/meta?,
//! optionals OMITTED when unset). Scope-filtered per request.
//!
//! In Electron the tree is published by renderers; in this single process it is mutated
//! DIRECTLY by `control::dispatch` (newPane/closePane/…), which is what collapses `/command`
//! into a synchronous call. Field ORDER in the serialized JSON matches `src/main/control-server.ts`'s
//! `ControlPaneInfo` declaration (id, sessionUid, label, subtitle, color, command, args, cwd,
//! shell, status, exitCode, activity, meta) — we serialize ordered structs directly, never a
//! key-sorted `serde_json::Value`, so the bytes match the TS source.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use crate::control::scope::{pane_in_scope, PaneCoords, Scope, ScopeTree};

/// Process liveness of a pane's pty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStatus {
    Running,
    Exited,
}

impl PaneStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PaneStatus::Running => "running",
            PaneStatus::Exited => "exited",
        }
    }
}

/// The orchestration liveness heuristic (agent-orchestration B). `idle` = no pty output for
/// the idle threshold; `busy` = recently producing output; `exited` = process gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Busy,
    Idle,
    /// Phase-4 precise state: a command finished / the shell is at a prompt, derived from
    /// OSC-133 markers (not from output silence). For the FROZEN legacy `activity` frame +
    /// `/state` field this DOWN-MAPS to `idle` (see [`Activity::legacy_str`]); the precise
    /// value is exposed on the new `liveness` frame instead.
    AwaitingInput,
    Exited,
}

impl Activity {
    /// The precise wire string (used by the new `liveness` channel and internal logic).
    pub fn as_str(self) -> &'static str {
        match self {
            Activity::Busy => "busy",
            Activity::Idle => "idle",
            Activity::AwaitingInput => "awaiting-input",
            Activity::Exited => "exited",
        }
    }

    /// The LEGACY-frame string: `AwaitingInput` collapses to `idle` so the frozen
    /// `activity` frame + `/state.activity` field keep their `busy|idle|exited` value set
    /// and no existing client sees a novel value.
    pub fn legacy_str(self) -> &'static str {
        match self {
            Activity::AwaitingInput => "idle",
            other => other.as_str(),
        }
    }
}

/// One pane in the read-model. Mirrors `ControlPaneInfo`; optional fields are `None` when
/// unset and OMITTED from `/state`.
#[derive(Debug, Clone)]
pub struct PaneInfo {
    pub id: String,
    pub session_uid: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub color: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
    pub status: PaneStatus,
    pub exit_code: Option<i32>,
    pub meta: Option<BTreeMap<String, String>>,
}

/// One tab (group): a layout + its panes.
#[derive(Debug, Clone)]
pub struct TabInfo {
    pub id: String,
    pub title: String,
    pub layout: String,
    pub panes: Vec<PaneInfo>,
}

/// One window: its tabs + the active tab id.
#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub window_id: i64,
    pub active_tab_id: Option<String>,
    pub tabs: Vec<TabInfo>,
}

/// A lightweight projection of a pane for the activity ticker / event fan-out.
#[derive(Debug, Clone)]
pub struct PaneRef {
    pub pane_id: String,
    pub session_uid: String,
    pub status: PaneStatus,
    pub coords: PaneCoords,
}

/// The authoritative windows→tabs→panes tree plus reverse indexes (rebuilt on structure
/// change). Window order is insertion order (a `Vec`), matching the TS `Map` iteration order.
#[derive(Debug, Default)]
pub struct ReadModel {
    windows: Vec<WindowInfo>,
    uid_to_pane: HashMap<String, String>,
    pane_loc: HashMap<String, (i64, String)>, // paneId → (windowId, tabId)
    tab_to_window: HashMap<String, i64>,
}

impl ReadModel {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- structure maintenance ------------------------------------------------------------

    /// Append a window (initial seed / a new OS window).
    pub fn add_window(&mut self, window: WindowInfo) {
        self.windows.push(window);
        self.reindex();
    }

    /// Drop a window and everything under it.
    pub fn drop_window(&mut self, window_id: i64) -> bool {
        let before = self.windows.len();
        self.windows.retain(|w| w.window_id != window_id);
        let removed = self.windows.len() != before;
        if removed {
            self.reindex();
        }
        removed
    }

    /// Rebuild the reverse indexes from the tree. Runs only on structure change.
    fn reindex(&mut self) {
        let mut uid_to_pane = HashMap::new();
        let mut pane_loc = HashMap::new();
        let mut tab_to_window = HashMap::new();
        for w in &self.windows {
            for t in &w.tabs {
                tab_to_window.insert(t.id.clone(), w.window_id);
                for p in &t.panes {
                    uid_to_pane.insert(p.session_uid.clone(), p.id.clone());
                    pane_loc.insert(p.id.clone(), (w.window_id, t.id.clone()));
                }
            }
        }
        self.uid_to_pane = uid_to_pane;
        self.pane_loc = pane_loc;
        self.tab_to_window = tab_to_window;
    }

    // ---- lookups --------------------------------------------------------------------------

    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// A pane's addressing coordinates, or `None` if unknown. (Inherent name avoids clashing
    /// with the `ScopeTree::pane_coords` trait method below.)
    pub fn coords_of(&self, pane_id: &str) -> Option<PaneCoords> {
        self.pane_loc.get(pane_id).map(|(w, t)| PaneCoords {
            pane_id: pane_id.to_string(),
            tab_id: t.clone(),
            window_id: *w,
        })
    }

    pub fn tab_window(&self, tab_id: &str) -> Option<i64> {
        self.tab_to_window.get(tab_id).copied()
    }

    pub fn has_window(&self, window_id: i64) -> bool {
        self.windows.iter().any(|w| w.window_id == window_id)
    }

    pub fn uid_to_pane(&self, uid: &str) -> Option<String> {
        self.uid_to_pane.get(uid).cloned()
    }

    pub fn pane(&self, pane_id: &str) -> Option<&PaneInfo> {
        let (w, t) = self.pane_loc.get(pane_id)?;
        self.windows
            .iter()
            .find(|win| win.window_id == *w)?
            .tabs
            .iter()
            .find(|tab| &tab.id == t)?
            .panes
            .iter()
            .find(|p| p.id == pane_id)
    }

    fn pane_mut(&mut self, pane_id: &str) -> Option<&mut PaneInfo> {
        let (w, t) = self.pane_loc.get(pane_id)?.clone();
        self.windows
            .iter_mut()
            .find(|win| win.window_id == w)?
            .tabs
            .iter_mut()
            .find(|tab| tab.id == t)?
            .panes
            .iter_mut()
            .find(|p| p.id == pane_id)
    }

    /// The active tab id of a window (or its first tab if none is marked active).
    pub fn active_tab_id(&self, window_id: i64) -> Option<String> {
        let w = self.windows.iter().find(|w| w.window_id == window_id)?;
        w.active_tab_id
            .clone()
            .or_else(|| w.tabs.first().map(|t| t.id.clone()))
    }

    pub fn first_window_id(&self) -> Option<i64> {
        self.windows.first().map(|w| w.window_id)
    }

    /// A flat projection of every pane (for the activity ticker + event resolution).
    pub fn panes(&self) -> Vec<PaneRef> {
        let mut out = Vec::new();
        for w in &self.windows {
            for t in &w.tabs {
                for p in &t.panes {
                    out.push(PaneRef {
                        pane_id: p.id.clone(),
                        session_uid: p.session_uid.clone(),
                        status: p.status,
                        coords: PaneCoords {
                            pane_id: p.id.clone(),
                            tab_id: t.id.clone(),
                            window_id: w.window_id,
                        },
                    });
                }
            }
        }
        out
    }

    // ---- mutations (driven by control::dispatch) ------------------------------------------

    /// Insert a pane into a window's active tab (or its first tab). Returns false if the
    /// window has no tab to host it.
    pub fn insert_pane(&mut self, window_id: i64, pane: PaneInfo) -> bool {
        let target_tab = match self.active_tab_id(window_id) {
            Some(t) => t,
            None => return false,
        };
        if let Some(w) = self.windows.iter_mut().find(|w| w.window_id == window_id) {
            if let Some(tab) = w.tabs.iter_mut().find(|t| t.id == target_tab) {
                tab.panes.push(pane);
                self.reindex();
                return true;
            }
        }
        false
    }

    /// Insert a pane into a SPECIFIC tab. Returns false if the tab is unknown.
    pub fn insert_pane_in_tab(&mut self, tab_id: &str, pane: PaneInfo) -> bool {
        for w in &mut self.windows {
            if let Some(tab) = w.tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.panes.push(pane);
                self.reindex();
                return true;
            }
        }
        false
    }

    /// Append a whole tab to a window. Returns false if the window is unknown.
    pub fn insert_tab(&mut self, window_id: i64, tab: TabInfo) -> bool {
        if let Some(w) = self.windows.iter_mut().find(|w| w.window_id == window_id) {
            w.tabs.push(tab);
            self.reindex();
            return true;
        }
        false
    }

    /// Remove a pane; returns its session uid so the caller can kill the pty. The host tab is
    /// kept even if it becomes empty (mirrors the read-model — closing the GUI tab is a
    /// separate window concern).
    pub fn remove_pane(&mut self, pane_id: &str) -> Option<String> {
        let (w, t) = self.pane_loc.get(pane_id)?.clone();
        let win = self.windows.iter_mut().find(|win| win.window_id == w)?;
        let tab = win.tabs.iter_mut().find(|tab| tab.id == t)?;
        let idx = tab.panes.iter().position(|p| p.id == pane_id)?;
        let uid = tab.panes.remove(idx).session_uid;
        self.reindex();
        Some(uid)
    }

    /// Set a tab's tiling layout. Returns false if the tab is unknown.
    pub fn set_layout(&mut self, tab_id: &str, layout: &str) -> bool {
        for w in &mut self.windows {
            if let Some(tab) = w.tabs.iter_mut().find(|t| t.id == tab_id) {
                tab.layout = layout.to_string();
                return true;
            }
        }
        false
    }

    /// Rename a pane's header. `set_subtitle` decides whether `subtitle` is touched at all;
    /// an empty subtitle clears it (TS `subtitle:""`).
    pub fn rename_pane(
        &mut self,
        pane_id: &str,
        label: &str,
        set_subtitle: bool,
        subtitle: Option<String>,
    ) -> bool {
        match self.pane_mut(pane_id) {
            Some(p) => {
                p.label = label.to_string();
                if set_subtitle {
                    p.subtitle = subtitle.filter(|s| !s.is_empty());
                }
                true
            }
            None => false,
        }
    }

    pub fn recolor_pane(&mut self, pane_id: &str, color: &str) -> bool {
        match self.pane_mut(pane_id) {
            Some(p) => {
                p.color = color.to_string();
                true
            }
            None => false,
        }
    }

    /// Merge a metadata patch (string → set, null → delete) and return the TRUE merged meta
    /// (mirrors the synchronous `setMeta` echo, agent-orchestration #7). `None` ⇒ no such pane.
    pub fn set_meta(
        &mut self,
        pane_id: &str,
        patch: &BTreeMap<String, Option<String>>,
    ) -> Option<BTreeMap<String, String>> {
        let p = self.pane_mut(pane_id)?;
        let mut merged = p.meta.clone().unwrap_or_default();
        for (k, v) in patch {
            match v {
                Some(val) => {
                    merged.insert(k.clone(), val.clone());
                }
                None => {
                    merged.remove(k);
                }
            }
        }
        p.meta = if merged.is_empty() {
            None
        } else {
            Some(merged.clone())
        };
        Some(merged)
    }

    /// Focus a pane: mark its tab active in its window. Returns false if unknown.
    pub fn focus_pane(&mut self, pane_id: &str) -> bool {
        let (w, t) = match self.pane_loc.get(pane_id) {
            Some(loc) => loc.clone(),
            None => return false,
        };
        if let Some(win) = self.windows.iter_mut().find(|win| win.window_id == w) {
            win.active_tab_id = Some(t);
            return true;
        }
        false
    }

    /// Mark a pane exited (from a session `Exit` event), returning its id + coords so the
    /// caller can fan out the `exit`/`activity` frames. Identified by session uid.
    pub fn mark_exited(&mut self, uid: &str, code: i32) -> Option<(String, PaneCoords)> {
        let pane_id = self.uid_to_pane.get(uid)?.clone();
        let coords = self.coords_of(&pane_id)?;
        if let Some(p) = self.pane_mut(&pane_id) {
            p.status = PaneStatus::Exited;
            p.exit_code = Some(code);
        }
        Some((pane_id, coords))
    }

    /// Update a pane's cwd from an OSC-7 sniff (identified by session uid).
    pub fn set_cwd(&mut self, uid: &str, cwd: &str) -> bool {
        let pane_id = match self.uid_to_pane.get(uid) {
            Some(id) => id.clone(),
            None => return false,
        };
        match self.pane_mut(&pane_id) {
            Some(p) => {
                p.cwd = Some(cwd.to_string());
                true
            }
            None => false,
        }
    }

    /// Set a pane's session uid + reset status to running (used by restartPane).
    pub fn respawn_pane(&mut self, pane_id: &str, new_uid: &str) -> bool {
        match self.pane_mut(pane_id) {
            Some(p) => {
                p.session_uid = new_uid.to_string();
                p.status = PaneStatus::Running;
                p.exit_code = None;
                true
            }
            None => false,
        }
    }

    // ---- serialization (`GET /state`) -----------------------------------------------------

    /// Serialize `/state` filtered to `scope` (None = master = everything verbatim). `activity`
    /// resolves each pane's liveness from the session manager. Returns an ordered struct so the
    /// JSON byte order matches the TS source.
    pub fn state_for_scope(
        &self,
        scope: Option<&Scope>,
        activity: &dyn Fn(&PaneInfo) -> Activity,
    ) -> StateOut {
        let mut windows = Vec::new();
        for w in &self.windows {
            let mut tabs = Vec::new();
            for t in &w.tabs {
                let mut panes = Vec::new();
                for p in &t.panes {
                    let in_scope = pane_in_scope(
                        scope,
                        &PaneCoords {
                            pane_id: p.id.clone(),
                            tab_id: t.id.clone(),
                            window_id: w.window_id,
                        },
                    );
                    if !in_scope {
                        continue;
                    }
                    panes.push(pane_out(p, activity(p)));
                }
                // Scoped views drop tabs/windows left empty; the master view keeps them verbatim.
                if scope.is_some() && panes.is_empty() {
                    continue;
                }
                tabs.push(TabOut {
                    id: t.id.clone(),
                    title: t.title.clone(),
                    layout: t.layout.clone(),
                    panes,
                });
            }
            if scope.is_some() && tabs.is_empty() {
                continue;
            }
            windows.push(WindowOut {
                window_id: w.window_id,
                active_tab_id: w.active_tab_id.clone(),
                tabs,
            });
        }
        StateOut { windows }
    }
}

/// `ScopeTree` over the live model, for `scope::check_mintable` (no-escalation minting).
impl ScopeTree for ReadModel {
    fn pane_coords(&self, pane_id: &str) -> Option<PaneCoords> {
        self.coords_of(pane_id)
    }
    fn tab_window(&self, tab_id: &str) -> Option<i64> {
        ReadModel::tab_window(self, tab_id)
    }
    fn has_window(&self, window_id: i64) -> bool {
        ReadModel::has_window(self, window_id)
    }
}

// ---- ordered serialization structs --------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StateOut {
    pub windows: Vec<WindowOut>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowOut {
    pub window_id: i64,
    pub active_tab_id: Option<String>, // string | null — always present
    pub tabs: Vec<TabOut>,
}

#[derive(Debug, Serialize)]
pub struct TabOut {
    pub id: String,
    pub title: String,
    pub layout: String,
    pub panes: Vec<PaneOut>,
}

/// The serialized pane shape — field order matches `ControlPaneInfo`/`ControlPane`, optionals
/// omitted when unset, `activity` always present.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PaneOut {
    pub id: String,
    pub session_uid: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    pub color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub activity: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<BTreeMap<String, String>>,
}

fn pane_out(p: &PaneInfo, activity: Activity) -> PaneOut {
    PaneOut {
        id: p.id.clone(),
        session_uid: p.session_uid.clone(),
        label: p.label.clone(),
        subtitle: p.subtitle.clone(),
        color: p.color.clone(),
        command: p.command.clone(),
        args: p.args.clone().filter(|a| !a.is_empty()),
        cwd: p.cwd.clone(),
        shell: p.shell.clone(),
        status: p.status.as_str(),
        exit_code: p.exit_code,
        // Frozen legacy field: AwaitingInput down-maps to "idle" (the precise value rides
        // the new `liveness` frame, not `/state`).
        activity: activity.legacy_str(),
        meta: p.meta.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane(id: &str, uid: &str) -> PaneInfo {
        PaneInfo {
            id: id.to_string(),
            session_uid: uid.to_string(),
            label: "shell".to_string(),
            subtitle: None,
            color: "#888888".to_string(),
            command: None,
            args: None,
            cwd: None,
            shell: None,
            status: PaneStatus::Running,
            exit_code: None,
            meta: None,
        }
    }

    fn seeded() -> ReadModel {
        let mut m = ReadModel::new();
        m.add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".to_string()),
            tabs: vec![TabInfo {
                id: "t1".to_string(),
                title: "Tab 1".to_string(),
                layout: "auto".to_string(),
                panes: vec![pane("p1", "u1")],
            }],
        });
        m
    }

    fn busy(_p: &PaneInfo) -> Activity {
        Activity::Busy
    }

    #[test]
    fn serializes_state_with_omitted_optionals_and_field_order() {
        let m = seeded();
        let out = m.state_for_scope(None, &busy);
        let s = serde_json::to_string(&out).unwrap();
        // Optionals omitted; activity present; key ORDER preserved (struct order, not sorted).
        assert_eq!(
            s,
            r##"{"windows":[{"windowId":1,"activeTabId":"t1","tabs":[{"id":"t1","title":"Tab 1","layout":"auto","panes":[{"id":"p1","sessionUid":"u1","label":"shell","color":"#888888","status":"running","activity":"busy"}]}]}]}"##
        );
    }

    #[test]
    fn serializes_all_optional_fields_when_present() {
        let mut m = seeded();
        {
            let p = m.pane_mut("p1").unwrap();
            p.subtitle = Some("sub".into());
            p.command = Some("claude".into());
            p.args = Some(vec!["--flag".into()]);
            p.cwd = Some("C:\\proj".into());
            p.shell = Some("pwsh".into());
            p.exit_code = Some(0);
            let mut meta = BTreeMap::new();
            meta.insert("role".to_string(), "worker".to_string());
            p.meta = Some(meta);
            p.status = PaneStatus::Exited;
        }
        let out = m.state_for_scope(None, &|_p| Activity::Exited);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&out).unwrap()).unwrap();
        let pane = &v["windows"][0]["tabs"][0]["panes"][0];
        assert_eq!(pane["subtitle"], json!("sub"));
        assert_eq!(pane["command"], json!("claude"));
        assert_eq!(pane["args"], json!(["--flag"]));
        assert_eq!(pane["cwd"], json!("C:\\proj"));
        assert_eq!(pane["shell"], json!("pwsh"));
        assert_eq!(pane["exitCode"], json!(0));
        assert_eq!(pane["activity"], json!("exited"));
        assert_eq!(pane["meta"], json!({ "role": "worker" }));
    }

    #[test]
    fn scope_filters_to_in_scope_panes_dropping_empty_tabs_and_windows() {
        let mut m = seeded();
        m.insert_pane(1, pane("p2", "u2"));
        // Add a second window with its own pane.
        m.add_window(WindowInfo {
            window_id: 2,
            active_tab_id: Some("t2".to_string()),
            tabs: vec![TabInfo {
                id: "t2".into(),
                title: "Tab 2".into(),
                layout: "auto".into(),
                panes: vec![pane("p3", "u3")],
            }],
        });
        let scope = Scope {
            pane_ids: Some(vec!["p2".into()]),
            ..Default::default()
        };
        let out = m.state_for_scope(Some(&scope), &busy);
        // Only window 1 / tab t1 / pane p2 survives.
        assert_eq!(out.windows.len(), 1);
        assert_eq!(out.windows[0].window_id, 1);
        assert_eq!(out.windows[0].tabs[0].panes.len(), 1);
        assert_eq!(out.windows[0].tabs[0].panes[0].id, "p2");
    }

    #[test]
    fn insert_and_remove_pane_maintains_indexes() {
        let mut m = seeded();
        assert!(m.insert_pane(1, pane("p2", "u2")));
        assert_eq!(m.uid_to_pane("u2").as_deref(), Some("p2"));
        assert_eq!(m.coords_of("p2").unwrap().tab_id, "t1");
        let uid = m.remove_pane("p2").unwrap();
        assert_eq!(uid, "u2");
        assert_eq!(m.uid_to_pane("u2"), None);
        assert!(m.coords_of("p2").is_none());
    }

    #[test]
    fn set_meta_merges_and_deletes_and_echoes_true_merged() {
        let mut m = seeded();
        let mut patch = BTreeMap::new();
        patch.insert("role".to_string(), Some("worker".to_string()));
        patch.insert("task".to_string(), Some("build".to_string()));
        let merged = m.set_meta("p1", &patch).unwrap();
        assert_eq!(merged.get("role").unwrap(), "worker");
        // Delete one key; keep the other.
        let mut patch2 = BTreeMap::new();
        patch2.insert("task".to_string(), None);
        let merged2 = m.set_meta("p1", &patch2).unwrap();
        assert_eq!(merged2.len(), 1);
        assert!(merged2.contains_key("role"));
        // Full clear ⇒ {} echoed, meta omitted from /state.
        let mut patch3 = BTreeMap::new();
        patch3.insert("role".to_string(), None);
        let merged3 = m.set_meta("p1", &patch3).unwrap();
        assert!(merged3.is_empty());
        assert!(m.pane("p1").unwrap().meta.is_none());
        // Missing pane ⇒ None.
        assert!(m.set_meta("ghost", &patch).is_none());
    }

    #[test]
    fn rename_recolor_layout_focus_and_exit() {
        let mut m = seeded();
        assert!(m.rename_pane("p1", "Worker", true, Some("idle".into())));
        assert_eq!(m.pane("p1").unwrap().label, "Worker");
        assert_eq!(m.pane("p1").unwrap().subtitle.as_deref(), Some("idle"));
        // Empty subtitle clears.
        assert!(m.rename_pane("p1", "Worker", true, Some(String::new())));
        assert!(m.pane("p1").unwrap().subtitle.is_none());
        assert!(m.recolor_pane("p1", "#e5484d"));
        assert_eq!(m.pane("p1").unwrap().color, "#e5484d");
        assert!(m.set_layout("t1", "grid"));
        assert!(m.focus_pane("p1"));
        let (id, coords) = m.mark_exited("u1", 3).unwrap();
        assert_eq!(id, "p1");
        assert_eq!(coords.window_id, 1);
        assert_eq!(m.pane("p1").unwrap().status, PaneStatus::Exited);
        assert_eq!(m.pane("p1").unwrap().exit_code, Some(3));
    }
}
