//! In-process command execution — replaces the Electron renderer round-trip + correlationId.
//! POST /command mutates the central `readmodel` directly and returns `{ok, result}`
//! SYNCHRONOUSLY (the set_meta echo race is now structurally impossible). Commands:
//! newPane (→ returns new paneId) / closePane / setLayout / renamePane / recolorPane / setMeta /
//! focusPane / openTab(attach) / restartPane / readScreen. PRESERVE the response shapes + status
//! mapping byte-for-byte: 500 on action error, 404 window-not-found, 400 missing-type/target,
//! 403 scope error. `readScreen` serializes the central `alacritty_terminal` Term via
//! `session::screen` (`SessionManager::render_screen`).
//!
//! Because this is in-process and synchronous, the TS 504 ("command timed out (no renderer
//! reply)") path cannot occur — no command is dispatched to a separate renderer. The string is
//! preserved in the routes layer for any command a future maintainer deliberately makes async.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::control::readmodel::{PaneInfo, PaneStatus, ReadModel, TabInfo};
use crate::control::scope::{pane_in_scope, tab_in_scope, window_in_scope, Scope};
use crate::session::spawn::EnvMap;
use crate::session_manager::{SessionManager, SpawnOptions};

/// Default pane frame color when a spawn spec omits one (cosmetic; `/state` requires a string).
const DEFAULT_PANE_COLOR: &str = "#3b82f6";

/// The HTTP outcome of a `/command` POST: a status, a JSON body, and whether the structure
/// changed (so the caller fires the coalesced `state` ping).
pub struct DispatchResult {
    pub status: u16,
    pub body: Value,
    pub notify_state: bool,
}

impl DispatchResult {
    fn err(status: u16, message: &str) -> Self {
        DispatchResult {
            status,
            body: json!({ "error": message }),
            notify_state: false,
        }
    }
    fn ok(result: Option<Value>, notify_state: bool) -> Self {
        let body = match result {
            Some(r) => json!({ "ok": true, "result": r }),
            None => json!({ "ok": true }),
        };
        DispatchResult {
            status: 200,
            body,
            notify_state,
        }
    }
}

/// Execute a `/command`, mirroring the TS `/command` handler + `applyControlCommand`.
/// `control_file` is the discovery path injected into spawned panes' env (suppressed by
/// `build_env` when a scoped token rides in the spec's env).
pub fn handle_command(
    model: &mut ReadModel,
    sessions: &SessionManager,
    control_file: Option<&str>,
    scope: Option<&Scope>,
    cmd: &Value,
) -> DispatchResult {
    let ty = match cmd.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None => return DispatchResult::err(400, "expected { type: string, … }"),
    };

    // Scope gate on the command's target (pane > tab > window).
    if let Some(denied) = command_scope_error(scope, cmd, model) {
        return DispatchResult::err(403, &denied);
    }

    // Resolve a target window: explicit windowId, else the pane's window.
    let window_id = cmd.get("windowId").and_then(Value::as_i64).or_else(|| {
        cmd.get("paneId")
            .and_then(Value::as_str)
            .and_then(|p| model.coords_of(p).map(|c| c.window_id))
    });
    if window_id.is_none() {
        return DispatchResult::err(400, "command needs a paneId or windowId");
    }
    let window_id = window_id.unwrap();

    match exec(ty, cmd, model, sessions, control_file, window_id) {
        Ok((result, notify)) => DispatchResult::ok(result, notify),
        Err(message) => DispatchResult::err(500, &message),
    }
}

/// Run one command against the live model. Returns (command result, structural?) or an error
/// string (→ 500). Result `None` ⇒ a result-less command (`{ ok: true }`).
fn exec(
    ty: &str,
    cmd: &Value,
    model: &mut ReadModel,
    sessions: &SessionManager,
    control_file: Option<&str>,
    window_id: i64,
) -> Result<(Option<Value>, bool), String> {
    match ty {
        "newPane" => {
            let spec = cmd.get("pane").cloned().unwrap_or_else(|| json!({}));
            let pane = spawn_pane(sessions, control_file, &spec)?;
            let pane_id = pane.id.clone();
            if !model.insert_pane(window_id, pane) {
                return Err(format!("window not found: {window_id}"));
            }
            Ok((Some(Value::String(pane_id)), true))
        }
        "attach" => {
            let unit = cmd.get("as").and_then(Value::as_str).unwrap_or("tab");
            let groups = cmd
                .get("groups")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if unit == "panes" {
                let mut pane_ids = Vec::new();
                for g in &groups {
                    if let Some(panes) = g.get("panes").and_then(Value::as_array) {
                        for ps in panes {
                            let pane = spawn_pane(sessions, control_file, ps)?;
                            pane_ids.push(Value::String(pane.id.clone()));
                            if !model.insert_pane(window_id, pane) {
                                return Err(format!("window not found: {window_id}"));
                            }
                        }
                    }
                }
                Ok((Some(Value::Array(pane_ids)), true))
            } else {
                let mut tab_ids = Vec::new();
                for g in &groups {
                    let tab_id = new_id();
                    let title = g
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Tab")
                        .to_string();
                    let layout = g
                        .get("layout")
                        .and_then(Value::as_str)
                        .unwrap_or("auto")
                        .to_string();
                    let mut panes = Vec::new();
                    if let Some(specs) = g.get("panes").and_then(Value::as_array) {
                        for ps in specs {
                            panes.push(spawn_pane(sessions, control_file, ps)?);
                        }
                    }
                    let tab = TabInfo {
                        id: tab_id.clone(),
                        title,
                        layout,
                        panes,
                    };
                    if !model.insert_tab(window_id, tab) {
                        return Err(format!("window not found: {window_id}"));
                    }
                    tab_ids.push(Value::String(tab_id));
                }
                Ok((Some(Value::Array(tab_ids)), true))
            }
        }
        "closePane" => {
            let pane_id = str_field(cmd, "paneId")?;
            if let Some(uid) = model.remove_pane(&pane_id) {
                sessions.kill(&uid);
            }
            Ok((None, true))
        }
        "restartPane" => {
            let pane_id = str_field(cmd, "paneId")?;
            let pane = model
                .pane(&pane_id)
                .ok_or_else(|| format!("no such pane: {pane_id}"))?;
            let old_uid = pane.session_uid.clone();
            let new_uid = new_id();
            let opts = SpawnOptions {
                uid: new_uid.clone(),
                shell: pane.shell.clone(),
                args: pane.args.clone(),
                command: pane.command.clone(),
                cwd: pane.cwd.clone(),
                env: None,
                cols: None,
                rows: None,
                pane_id: Some(pane_id.clone()),
                integration: None,
                control_file: control_file.map(str::to_string),
            };
            sessions.kill(&old_uid);
            sessions.create(opts).map_err(|e| e.to_string())?;
            model.respawn_pane(&pane_id, &new_uid);
            Ok((None, true))
        }
        "setLayout" => {
            let tab_id = str_field(cmd, "tabId")?;
            let layout = str_field(cmd, "layout")?;
            model.set_layout(&tab_id, &layout);
            Ok((None, true))
        }
        "renamePane" => {
            let pane_id = str_field(cmd, "paneId")?;
            let label = str_field(cmd, "label")?;
            let (set_subtitle, subtitle) = match cmd.get("subtitle") {
                Some(Value::String(s)) => (true, Some(s.clone())),
                Some(Value::Null) | None => (false, None),
                Some(_) => (false, None),
            };
            model.rename_pane(&pane_id, &label, set_subtitle, subtitle);
            Ok((None, false))
        }
        "recolorPane" => {
            let pane_id = str_field(cmd, "paneId")?;
            let color = str_field(cmd, "color")?;
            model.recolor_pane(&pane_id, &color);
            Ok((None, false))
        }
        "setMeta" => {
            let pane_id = str_field(cmd, "paneId")?;
            let mut patch: BTreeMap<String, Option<String>> = BTreeMap::new();
            if let Some(obj) = cmd.get("meta").and_then(Value::as_object) {
                for (k, v) in obj {
                    match v {
                        Value::String(s) => {
                            patch.insert(k.clone(), Some(s.clone()));
                        }
                        Value::Null => {
                            patch.insert(k.clone(), None);
                        }
                        _ => {}
                    }
                }
            }
            // The TRUE merged meta is echoed as the result (the synchronous #7 fix); a missing
            // pane yields no result (→ MCP set_meta reads it as {}).
            match model.set_meta(&pane_id, &patch) {
                Some(merged) => {
                    let obj: serde_json::Map<String, Value> = merged
                        .into_iter()
                        .map(|(k, v)| (k, Value::String(v)))
                        .collect();
                    Ok((Some(Value::Object(obj)), false))
                }
                None => Ok((None, false)),
            }
        }
        "focusPane" => {
            let pane_id = str_field(cmd, "paneId")?;
            model.focus_pane(&pane_id);
            Ok((None, true))
        }
        "readScreen" => {
            let pane_id = str_field(cmd, "paneId")?;
            let pane = model
                .pane(&pane_id)
                .ok_or_else(|| format!("no such pane: {pane_id}"))?;
            match sessions.render_screen(&pane.session_uid) {
                Some(text) => Ok((Some(Value::String(text)), false)),
                None => Err("screen unavailable".to_string()),
            }
        }
        other => Err(format!("unknown command type: {other}")),
    }
}

/// Build + spawn a pane session from a `{ label?, command?, args?, cwd?, shell?, color?, meta?,
/// env? }` spec, returning the read-model `PaneInfo` (not yet inserted).
fn spawn_pane(
    sessions: &SessionManager,
    control_file: Option<&str>,
    spec: &Value,
) -> Result<PaneInfo, String> {
    let pane_id = new_id();
    let session_uid = new_id();
    let command = spec
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string);
    let args = spec
        .get("args")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .filter(|a| !a.is_empty());
    let cwd = spec.get("cwd").and_then(Value::as_str).map(str::to_string);
    let shell = spec
        .get("shell")
        .and_then(Value::as_str)
        .map(str::to_string);
    // Reject an over-long explicit label — callers should send a short title, not a whole command
    // line. Returning Err fails the newPane so the MCP/control surfaces the error (#21/#22).
    const MAX_LABEL_LEN: usize = 80;
    if let Some(l) = spec.get("label").and_then(Value::as_str) {
        let n = l.chars().count();
        if n > MAX_LABEL_LEN {
            return Err(format!("label too long: {n} chars (max {MAX_LABEL_LEN})"));
        }
    }
    let label = spec
        .get("label")
        .and_then(Value::as_str)
        .map(str::to_string)
        // No explicit label → default to the command's FIRST TOKEN (e.g. "claude"), never the whole
        // command line (mirrors the CLI's `command.trim().split_whitespace()[0]` default).
        .or_else(|| {
            command
                .as_deref()
                .and_then(|c| c.split_whitespace().next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "shell".to_string());
    let color = spec
        .get("color")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PANE_COLOR)
        .to_string();
    let meta = spec.get("meta").and_then(Value::as_object).map(|o| {
        o.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<BTreeMap<String, String>>()
    });
    let env = spec.get("env").and_then(Value::as_object).map(|o| {
        o.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<EnvMap>()
    });

    // Interactive control-spawned panes get the same shell integration as GUI panes
    // (cwd OSC → project tint / clickable paths; zsh needs the bundled ZDOTDIR). The
    // TS app applied this inside the Session constructor, so dispatch passing `None`
    // here silently no-op'd integration for every control-API pane.
    let integration = command
        .is_none()
        .then(|| {
            let shell_path = shell
                .clone()
                .unwrap_or_else(crate::session::spawn::default_shell);
            crate::shell_integration::integration_for(
                &shell_path,
                &crate::shell_integration::shell_integration_dir(),
            )
            .map(|si| crate::session_manager::Integration {
                args: si.args,
                env: si.env.into_iter().collect(),
            })
        })
        .flatten();
    let opts = SpawnOptions {
        uid: session_uid.clone(),
        shell: shell.clone(),
        args: args.clone(),
        command: command.clone(),
        cwd: cwd.clone(),
        env,
        cols: None,
        rows: None,
        pane_id: Some(pane_id.clone()),
        integration,
        control_file: control_file.map(str::to_string),
    };
    sessions.create(opts).map_err(|e| e.to_string())?;

    Ok(PaneInfo {
        id: pane_id,
        session_uid,
        label,
        subtitle: None,
        color,
        command,
        args,
        cwd,
        shell,
        status: PaneStatus::Running,
        exit_code: None,
        meta: meta.filter(|m| !m.is_empty()),
    })
}

/// Whether a scoped token may run `cmd` against its target (pane > tab > window). Mirrors TS
/// `commandScopeError` exactly, including the active-tab exception for window-targeted spawns.
pub fn command_scope_error(
    scope: Option<&Scope>,
    cmd: &Value,
    model: &ReadModel,
) -> Option<String> {
    let scope = scope?; // master: anything
    if let Some(pane_id) = cmd.get("paneId").and_then(Value::as_str) {
        return match model.coords_of(pane_id) {
            None => Some(format!("unknown paneId {pane_id}")),
            Some(coords) => {
                if pane_in_scope(Some(scope), &coords) {
                    None
                } else {
                    Some(format!("paneId {pane_id} is out of scope"))
                }
            }
        };
    }
    if let Some(tab_id) = cmd.get("tabId").and_then(Value::as_str) {
        return match model.tab_window(tab_id) {
            None => Some(format!("unknown tabId {tab_id}")),
            Some(win) => {
                if tab_in_scope(Some(scope), tab_id, win) {
                    None
                } else {
                    Some(format!("tabId {tab_id} is out of scope"))
                }
            }
        };
    }
    if let Some(window_id) = cmd.get("windowId").and_then(Value::as_i64) {
        if window_in_scope(Some(scope), window_id) {
            return None;
        }
        // newPane / setLayout-without-tabId act on the window's ACTIVE tab, so a tab-scoped
        // manager may spawn into its own tab when that tab is active.
        if let Some(active_tab) = model.active_tab_id(window_id) {
            if tab_in_scope(Some(scope), &active_tab, window_id) {
                return None;
            }
        }
        return Some(format!("windowId {window_id} is out of scope"));
    }
    Some("a scoped token needs a paneId, tabId, or windowId on the command".to_string())
}

fn str_field(cmd: &Value, key: &str) -> Result<String, String> {
    cmd.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field: {key}"))
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::readmodel::{TabInfo, WindowInfo};
    use tokio::sync::mpsc::unbounded_channel;

    fn model_one_window() -> ReadModel {
        let mut m = ReadModel::new();
        m.add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab 1".into(),
                layout: "auto".into(),
                panes: vec![],
            }],
        });
        m
    }

    fn sessions() -> SessionManager {
        let (tx, _rx) = unbounded_channel();
        SessionManager::new(tx)
    }

    // newPane needs a tokio runtime (SessionManager::create spawns a driver task).
    #[tokio::test]
    async fn new_pane_spawns_inserts_and_returns_the_pane_id() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": { "label": "w", "command": "echo hi" } });
        let r = handle_command(&mut m, &s, Some("C:/control.json"), None, &cmd);
        assert_eq!(r.status, 200);
        assert!(r.notify_state);
        let id = r.body["result"].as_str().unwrap().to_string();
        assert!(m.pane(&id).is_some());
        assert_eq!(m.pane(&id).unwrap().label, "w");
    }

    #[tokio::test]
    async fn set_meta_echoes_true_merged_synchronously() {
        let mut m = model_one_window();
        let s = sessions();
        // Spawn a pane to target.
        let open = json!({ "type": "newPane", "windowId": 1, "pane": {} });
        let id = handle_command(&mut m, &s, None, None, &open).body["result"]
            .as_str()
            .unwrap()
            .to_string();
        let cmd =
            json!({ "type": "setMeta", "paneId": id, "meta": { "role": "worker", "task": "x" } });
        let r = handle_command(&mut m, &s, None, None, &cmd);
        assert_eq!(r.status, 200);
        assert_eq!(r.body["result"]["role"], json!("worker"));
        assert_eq!(r.body["result"]["task"], json!("x"));
        // Delete a key — echoed merged drops it.
        let del = json!({ "type": "setMeta", "paneId": id, "meta": { "task": null } });
        let r2 = handle_command(&mut m, &s, None, None, &del);
        assert!(r2.body["result"].get("task").is_none());
        assert_eq!(r2.body["result"]["role"], json!("worker"));
    }

    #[test]
    fn missing_type_is_400() {
        let mut m = model_one_window();
        let s = sessions();
        let r = handle_command(&mut m, &s, None, None, &json!({ "paneId": "p" }));
        assert_eq!(r.status, 400);
        assert_eq!(r.body["error"], json!("expected { type: string, … }"));
    }

    #[test]
    fn no_target_is_400() {
        let mut m = model_one_window();
        let s = sessions();
        let r = handle_command(
            &mut m,
            &s,
            None,
            None,
            &json!({ "type": "setLayout", "layout": "grid" }),
        );
        assert_eq!(r.status, 400);
        assert_eq!(r.body["error"], json!("command needs a paneId or windowId"));
    }

    #[test]
    fn scope_gate_rejects_out_of_scope_window() {
        let mut m = model_one_window();
        let s = sessions();
        let scope = Scope {
            window_ids: Some(vec![999]),
            ..Default::default()
        };
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": {} });
        let r = handle_command(&mut m, &s, None, Some(&scope), &cmd);
        assert_eq!(r.status, 403);
        assert_eq!(r.body["error"], json!("windowId 1 is out of scope"));
    }

    #[test]
    fn command_scope_error_matches_ts_messages() {
        let m = model_one_window();
        // unknown paneId
        assert_eq!(
            command_scope_error(
                Some(&Scope {
                    pane_ids: Some(vec!["p1".into()]),
                    ..Default::default()
                }),
                &json!({ "type": "closePane", "paneId": "ghost" }),
                &m,
            ),
            Some("unknown paneId ghost".to_string())
        );
        // master scope → always allowed
        assert_eq!(
            command_scope_error(None, &json!({ "type": "closePane", "paneId": "ghost" }), &m),
            None
        );
    }
}
