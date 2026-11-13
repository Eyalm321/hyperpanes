//! In-process command execution — replaces the Electron renderer round-trip + correlationId.
//! POST /command mutates the central `readmodel` directly and returns `{ok, result}`
//! SYNCHRONOUSLY (the set_meta echo race is now structurally impossible). Commands:
//! newPane (→ returns new paneId) / closePane / setLayout / renameTab / recolorTab / renamePane /
//! recolorPane / setMeta / focusPane / openTab(attach) / restartPane / readScreen / recoverPane.
//! PRESERVE the response shapes + status mapping byte-for-byte: 500 on action error, 404
//! window-not-found, 400 missing-type/target, 403 scope error. `readScreen` serializes the
//! central `alacritty_terminal` Term via `session::screen` (`SessionManager::render_screen`).
//!
//! `recoverPane` is the agent-pane API-error recovery contract documented in
//! `docs/agent-recovery.md`, built on the pure detection/classification/repair logic in
//! [`crate::claude_recovery`]. `inspect` is read-only; `repair`/`resume` share their
//! session-resolution (explicit `sessionId` override > live marker > newest same-cwd scan
//! candidate) via [`resolve_recover_target`], and `resume` shares its respawn mechanics with
//! `restartPane { resume: true }` via [`respawn_resuming`] rather than duplicating it.
//!
//! Because this is in-process and synchronous, the TS 504 ("command timed out (no renderer
//! reply)") path cannot occur — no command is dispatched to a separate renderer. The string is
//! preserved in the routes layer for any command a future maintainer deliberately makes async.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::claude_recovery::{self, ErrorClass};
use crate::control::readmodel::{PaneInfo, PaneStatus, ReadModel, TabInfo};
use crate::control::scope::{pane_in_scope, tab_in_scope, window_in_scope, Scope};
use crate::control::speech_service::SpeechService;
use crate::session::spawn::EnvMap;
use crate::session_manager::{SessionManager, SpawnOptions};

/// Default pane frame color when a spawn spec omits one (cosmetic; `/state` requires a string).
const DEFAULT_PANE_COLOR: &str = "#3b82f6";

/// Top-level keys that indicate a `newPane` spawn spec was mistakenly flattened instead of
/// nested under `pane` (see the `newPane` guard in `handle_command`).
const NEW_PANE_SPEC_KEYS: &[&str] = &[
    "command",
    "args",
    "cwd",
    "label",
    "subtitle",
    "color",
    "shell",
    "env",
    "meta",
    "project",
    "projectTab",
];

/// The HTTP outcome of a `/command` POST: a status, a JSON body, and whether the structure
/// changed (so the caller fires the coalesced `state` ping).
pub struct DispatchResult {
    pub status: u16,
    pub body: Value,
    pub notify_state: bool,
    /// The command touched the project registry (`projects.json`) off the UI thread — a
    /// project-opening `newPane` bumps a project's recency. The route layer maps this to
    /// `Shared::mark_projects_dirty` so the GUI sidebar rail refreshes live.
    pub projects_dirty: bool,
}

impl DispatchResult {
    fn err(status: u16, message: &str) -> Self {
        DispatchResult {
            status,
            body: json!({ "error": message }),
            notify_state: false,
            projects_dirty: false,
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
            projects_dirty: false,
        }
    }
}

/// Execute a `/command`, mirroring the TS `/command` handler + `applyControlCommand`.
/// `control_file` is the discovery path injected into spawned panes' env (suppressed by
/// `build_env` when a scoped token rides in the spec's env). `speech` is the shared
/// per-pane "talk" service (settings + engine) for the `setSpeech*` commands below.
pub fn handle_command(
    model: &mut ReadModel,
    sessions: &SessionManager,
    control_file: Option<&str>,
    scope: Option<&Scope>,
    cmd: &Value,
    speech: &SpeechService,
) -> DispatchResult {
    let ty = match cmd.get("type").and_then(Value::as_str) {
        Some(t) => t,
        None => return DispatchResult::err(400, "expected { type: string, … }"),
    };

    // Scope gate on the command's target (pane > tab > window).
    if let Some(denied) = command_scope_error(scope, cmd, model) {
        return DispatchResult::err(403, &denied);
    }

    // `queuePrompt` targets a Claude *session*, not a pane/window — handle it before
    // window resolution (the queue is file-backed; delivery finds the pane later).
    if ty == "queuePrompt" {
        let (session_id, text) = match (
            cmd.get("sessionId").and_then(Value::as_str),
            cmd.get("text").and_then(Value::as_str),
        ) {
            (Some(s), Some(t)) => (s, t),
            _ => return DispatchResult::err(400, "queuePrompt needs sessionId and text"),
        };
        return match crate::resume_queue::enqueue(session_id, text) {
            Ok(()) => DispatchResult::ok(None, false),
            Err(e) => DispatchResult::err(400, &e),
        };
    }

    // `setSpeechMuted`/`setSpeechFocusedOnly` are global speech-engine settings, not
    // targeted at a pane/window — handle them before window resolution, same as
    // `queuePrompt` above.
    if ty == "setSpeechMuted" {
        return match cmd.get("muted").and_then(Value::as_bool) {
            Some(muted) => {
                speech.set_muted(muted);
                DispatchResult::ok(None, false)
            }
            None => DispatchResult::err(400, "setSpeechMuted needs a boolean muted"),
        };
    }
    if ty == "setSpeechFocusedOnly" {
        return match cmd.get("focusedOnly").and_then(Value::as_bool) {
            Some(focused_only) => {
                speech.set_focused_only(focused_only);
                DispatchResult::ok(None, false)
            }
            None => DispatchResult::err(400, "setSpeechFocusedOnly needs a boolean focusedOnly"),
        };
    }
    // One-shot global stop: kill the in-flight utterance and drop the backlog, leaving
    // the persisted mute/focused settings untouched (mute is the sticky variant).
    if ty == "stopSpeech" {
        speech.stop_all();
        return DispatchResult::ok(None, false);
    }

    // `setLayout` validates its own target + layout name up front so an unknown tab or layout
    // is an error rather than a silently-acknowledged no-op.
    if ty == "setLayout" {
        return match resolve_set_layout(cmd, model) {
            Ok((tab_id, layout)) => {
                model.set_layout(&tab_id, &layout);
                DispatchResult::ok(None, true)
            }
            Err((status, msg)) => DispatchResult::err(status, &msg),
        };
    }

    // `renameTab` / `recolorTab` carry only a tabId, so they are handled before the generic
    // window resolution (which would answer 400 "needs a paneId or windowId").
    if ty == "renameTab" {
        return match resolve_rename_tab(cmd, model) {
            Ok((tab_id, title)) => {
                model.rename_tab(&tab_id, &title);
                DispatchResult::ok(None, true)
            }
            Err((status, msg)) => DispatchResult::err(status, &msg),
        };
    }
    if ty == "recolorTab" {
        return match resolve_recolor_tab(cmd, model) {
            Ok((tab_id, color)) => {
                model.recolor_tab(&tab_id, color);
                DispatchResult::ok(None, true)
            }
            Err((status, msg)) => DispatchResult::err(status, &msg),
        };
    }

    // `newPane {tabId}` spawns straight into that tab (no open-then-move flash): the tab must
    // exist, and an explicit windowId must be the tab's own window. `attach {as:"panes",tabId}`
    // merges its panes into that tab the same way; tabId is meaningless for new tabs.
    if ty == "newPane" || ty == "attach" {
        if let Err(msg) = validate_new_pane_tab(cmd, model) {
            return DispatchResult::err(400, &msg);
        }
    }
    if ty == "attach"
        && cmd.get("tabId").is_some_and(|v| !v.is_null())
        && cmd.get("as").and_then(Value::as_str) != Some("panes")
    {
        return DispatchResult::err(400, "attach tabId needs \"as\":\"panes\"");
    }

    // Resolve a target window: explicit windowId (number or numeric string), else the pane's
    // window, else (newPane) the target tab's window.
    let window_id = window_id_field(cmd)
        .or_else(|| {
            cmd.get("paneId")
                .and_then(Value::as_str)
                .and_then(|p| model.coords_of(p).map(|c| c.window_id))
        })
        .or_else(|| new_pane_tab_id(cmd).and_then(|t| model.tab_window(t)));
    if window_id.is_none() {
        return DispatchResult::err(400, "command needs a paneId or windowId");
    }
    let window_id = window_id.unwrap();

    // `newPane` spawn spec must be nested under `pane`; a flat top-level spec
    // (a common hand-authored mistake) would otherwise fall through to the
    // `unwrap_or_else(|| json!({}))` default in `exec` and silently spawn the
    // default shell instead of the caller's intended command.
    if ty == "newPane" {
        if let Some(pane) = cmd.get("pane") {
            if !pane.is_object() {
                return DispatchResult::err(
                    400,
                    "newPane \"pane\" must be an object, e.g. { \"type\": \"newPane\", \"windowId\": 0, \"pane\": { \"command\": … } }",
                );
            }
        } else if NEW_PANE_SPEC_KEYS.iter().any(|k| cmd.get(*k).is_some()) {
            return DispatchResult::err(
                400,
                "newPane spec fields (command/args/cwd/label/subtitle/color/shell/env/meta/project) must be nested under \"pane\", not top-level, e.g. { \"type\": \"newPane\", \"windowId\": 0, \"pane\": { \"command\": … } }",
            );
        }
    }

    // `attach` group colours are validated up front so a bad one fails with 400 before any pane
    // is spawned (the same rule as `recolorTab`).
    if ty == "attach" {
        if let Err(msg) = validate_attach_colors(cmd) {
            return DispatchResult::err(400, &msg);
        }
    }

    match exec(ty, cmd, model, sessions, control_file, window_id) {
        Ok((result, notify)) => {
            let mut r = DispatchResult::ok(result, notify);
            // A successful newPane that named a project bumped its recency in the registry.
            if ty == "newPane"
                && cmd
                    .pointer("/pane/project")
                    .and_then(Value::as_str)
                    .is_some()
            {
                r.projects_dirty = true;
            }
            r
        }
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
            let mut spec = cmd.get("pane").cloned().unwrap_or_else(|| json!({}));
            // `pane.project` (a project id or name) opens the pane in that remembered project:
            // default its cwd + frame color from the registry (explicit cwd/color still win) and
            // bump the project's recency, mirroring the GUI sidebar's "open project". An unknown
            // handle fails the command rather than silently spawning a homeless pane.
            resolve_project_into_spec(&mut spec)?;
            let pane = spawn_pane(sessions, control_file, &spec)?;
            let pane_id = pane.id.clone();
            // `tabId` (validated in `handle_command`) places it in that tab; else the active one.
            let placed = match new_pane_tab_id(cmd) {
                Some(tab_id) => model.insert_pane_in_tab(tab_id, pane),
                None => model.insert_pane(window_id, pane),
            };
            if !placed {
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
                // `tabId` (validated in `handle_command`) merges into that tab; else the active one.
                let tab_id = new_pane_tab_id(cmd);
                let mut pane_ids = Vec::new();
                for g in &groups {
                    if let Some(panes) = g.get("panes").and_then(Value::as_array) {
                        for ps in panes {
                            let pane = spawn_pane(sessions, control_file, ps)?;
                            pane_ids.push(Value::String(pane.id.clone()));
                            let placed = match tab_id {
                                Some(t) => model.insert_pane_in_tab(t, pane),
                                None => model.insert_pane(window_id, pane),
                            };
                            if !placed {
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
                        .unwrap_or("")
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
                    // Validated by `validate_attach_colors`; null or absent = no manual colour.
                    let color = g
                        .get("color")
                        .and_then(Value::as_str)
                        .and_then(normalize_hex_color);
                    let tab = TabInfo {
                        id: tab_id.clone(),
                        title,
                        color,
                        layout,
                        panes,
                        pane_group: None,
                    };
                    // Pending until the GUI adopts its panes into a tab under this id, so the
                    // returned TAB id stays addressable across any GUI republish before that.
                    if !model.insert_pending_tab(window_id, tab) {
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
                crate::persistence::session_log::record(
                    "kill-request",
                    &uid,
                    "control:closePane",
                    Some(&pane_id),
                );
                sessions.kill(&uid);
            }
            Ok((None, true))
        }
        "restartPane" => {
            let pane_id = str_field(cmd, "paneId")?;
            let pane = model
                .pane(&pane_id)
                .cloned()
                .ok_or_else(|| format!("no such pane: {pane_id}"))?;
            // `resume:true`: after the respawn, type `cd + claude --resume` for the
            // conversation this pane was hosting, per its SessionStart marker — read
            // BEFORE the kill (SessionEnd may remove it). An agent may target its OWN
            // pane: the command returns before the kill lands on its process.
            let resume = matches!(cmd.get("resume"), Some(Value::Bool(true)));
            if !resume {
                let old_uid = pane.session_uid.clone();
                let new_uid = new_id();
                // Optional `env` override, layered over the base spawn env (same shape as
                // `openPane`).
                let env_override = cmd.get("env").and_then(Value::as_object).map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect::<EnvMap>()
                });
                // Keep the env the pane was opened with; the restart env layers on top.
                let merged_env = merge_env(pane.env.as_ref(), env_override);
                let spawn_env = crate::session_manager::stamp_unattended(
                    merged_env.clone(),
                    pane.meta.as_ref(),
                );
                let opts = SpawnOptions {
                    uid: new_uid.clone(),
                    shell: pane.shell.clone(),
                    args: pane.args.clone(),
                    command: pane.command.clone(),
                    cwd: pane.cwd.clone(),
                    env: spawn_env,
                    cols: None,
                    rows: None,
                    pane_id: Some(pane_id.clone()),
                    integration: None,
                    control_file: control_file.map(str::to_string),
                };
                crate::persistence::session_log::record(
                    "kill-request",
                    &old_uid,
                    "control:restartPane",
                    Some(&pane_id),
                );
                sessions.kill(&old_uid);
                sessions.create(opts).map_err(|e| e.to_string())?;
                model.respawn_pane(&pane_id, &new_uid);
                model.set_pane_env(&pane_id, merged_env);
                return Ok((None, true));
            }
            let marker = crate::claude_panes::read_pane_session(&pane_id).ok_or_else(|| {
                format!("resume requested but no live Claude marker for pane {pane_id}")
            })?;
            let target = ResumeTarget::from_marker(&marker);
            respawn_resuming(
                model,
                sessions,
                control_file,
                &pane,
                &target,
                cmd.get("env"),
                cmd.get("prompt").and_then(Value::as_str),
            )?;
            Ok((None, true))
        }
        "recoverPane" => {
            let pane_id = str_field(cmd, "paneId")?;
            let pane = model
                .pane(&pane_id)
                .cloned()
                .ok_or_else(|| format!("no such pane: {pane_id}"))?;
            let action = cmd
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("inspect");
            match action {
                "inspect" => Ok((Some(recover_inspect(sessions, &pane)?), false)),
                "repair" => Ok((Some(recover_repair(sessions, &pane, cmd)?), false)),
                "resume" => {
                    recover_resume(model, sessions, control_file, &pane, cmd)?;
                    Ok((None, true))
                }
                other => Err(format!("unknown recoverPane action: {other}")),
            }
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
        "setTalk" => {
            let pane_id = str_field(cmd, "paneId")?;
            let enabled = cmd
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or("missing boolean field: enabled")?;
            if !model.set_talk(&pane_id, enabled) {
                return Err(format!("no such pane: {pane_id}"));
            }
            Ok((None, false))
        }
        other => Err(format!("unknown command type: {other}")),
    }
}

// ===================== recoverPane =====================

/// How much of a pane's (ANSI-stripped) scrollback [`pane_tail`] reads for detection — "the
/// last few KB", per `docs/agent-recovery.md`. Large enough to catch a multi-line `API Error`
/// plus surrounding context; small enough to keep every `recoverPane` call cheap.
const RECOVER_TAIL_BYTES: usize = 8 * 1024;

/// Read a pane's live output, ANSI-strip it (same helper `GET /panes/:id/output?strip=1`
/// uses), and keep only the last [`RECOVER_TAIL_BYTES`] bytes — the pane-tail text
/// [`crate::claude_recovery::detect_api_error`] scans.
fn pane_tail(sessions: &SessionManager, uid: &str) -> String {
    let (raw, _) = sessions.replay_with_cursor(uid).unwrap_or_default();
    let stripped = crate::ansi_strip::strip_ansi(&raw);
    tail_bytes(&stripped, RECOVER_TAIL_BYTES).to_string()
}

/// Keep the last `max` bytes of `s`, walking forward to the nearest char boundary so the
/// slice is always valid UTF-8 (never splits a multi-byte character).
fn tail_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn error_class_str(c: ErrorClass) -> &'static str {
    match c {
        ErrorClass::Transient => "transient",
        ErrorClass::AccountLimit => "account-limit",
        ErrorClass::Poisoned => "poisoned",
        ErrorClass::Unknown => "unknown",
    }
}

/// `action: "inspect"` — read-only: current activity, the last `API Error` sighting (if
/// any) and its class, and the best session this pane could be resumed/repaired against
/// (a live marker, or every same-cwd scan candidate when there's no marker). Never mutates
/// anything, so it's always safe to call speculatively before `repair`/`resume`.
fn recover_inspect(sessions: &SessionManager, pane: &PaneInfo) -> Result<Value, String> {
    let activity = crate::control::server::activity_for(
        sessions,
        crate::control::server::IDLE_THRESHOLD_MS,
        &pane.session_uid,
        pane.status,
    );
    let tail = pane_tail(sessions, &pane.session_uid);
    let sighting = claude_recovery::detect_api_error(&tail);
    let class = sighting.as_ref().map(claude_recovery::classify_error);
    let api_error = sighting
        .as_ref()
        .map(|s| json!({ "code": s.code, "detail": s.detail, "retrying": s.retrying }));

    let session = if let Some(marker) = crate::claude_panes::read_pane_session(&pane.id) {
        json!({
            "source": "marker",
            "sessionId": marker.session_id,
            "configDir": (!marker.config_dir.is_empty()).then(|| marker.config_dir.clone()),
            "cwd": marker.cwd,
        })
    } else {
        let candidates = pane
            .cwd
            .as_deref()
            .map(|cwd| claude_recovery::resolve_session_candidates(Path::new(cwd)))
            .unwrap_or_default();
        json!({
            "source": "scan",
            "candidates": candidates
                .into_iter()
                .map(|c| json!({
                    "sessionId": c.session_id,
                    "configDir": c.config_dir.map(|d| d.display().to_string()),
                    "path": c.path.display().to_string(),
                    "mtimeMs": c.modified_at,
                }))
                .collect::<Vec<_>>(),
        })
    };

    Ok(json!({
        "activity": activity.as_str(),
        "apiError": api_error,
        "class": class.map(error_class_str),
        "session": session,
    }))
}

/// A transcript resolved for `repair`/`resume`, however it was found. Precedence (per
/// `docs/agent-recovery.md`'s "Session resolution"): an explicit `sessionId` override wins
/// over both; else the pane's live marker; else the newest same-cwd scan candidate across
/// every account's store.
struct RecoverTarget {
    session_id: String,
    path: PathBuf,
    cwd: String,
    /// `None` = the default account (`~/.claude`).
    config_dir: Option<String>,
}

fn resolve_recover_target(pane: &PaneInfo, cmd: &Value) -> Result<RecoverTarget, String> {
    let cwd = pane
        .cwd
        .clone()
        .ok_or_else(|| "pane has no cwd to resolve a session against".to_string())?;

    if let Some(explicit) = cmd.get("sessionId").and_then(Value::as_str) {
        let hit = claude_recovery::resolve_session_candidates(Path::new(&cwd))
            .into_iter()
            .find(|c| c.session_id == explicit)
            .ok_or_else(|| {
                format!("no transcript found for sessionId {explicit} under cwd {cwd}")
            })?;
        return Ok(RecoverTarget {
            session_id: hit.session_id,
            path: hit.path,
            cwd,
            config_dir: hit.config_dir.map(|d| d.display().to_string()),
        });
    }

    if let Some(marker) = crate::claude_panes::read_pane_session(&pane.id) {
        let config_dir = (!marker.config_dir.is_empty()).then(|| marker.config_dir.clone());
        let projects_root = match &config_dir {
            Some(dir) => PathBuf::from(dir).join("projects"),
            None => crate::claude_history::claude_projects_root()
                .ok_or_else(|| "no default ~/.claude/projects store found".to_string())?,
        };
        let path = projects_root
            .join(crate::claude_history::encode_project_dir(Path::new(
                &marker.cwd,
            )))
            .join(format!("{}.jsonl", marker.session_id));
        return Ok(RecoverTarget {
            session_id: marker.session_id,
            path,
            cwd: marker.cwd,
            config_dir,
        });
    }

    let newest = claude_recovery::resolve_session_candidates(Path::new(&cwd))
        .into_iter()
        .next()
        .ok_or_else(|| {
            format!("no transcript found for pane cwd {cwd} (no marker, no scan candidates)")
        })?;
    Ok(RecoverTarget {
        session_id: newest.session_id,
        path: newest.path,
        cwd,
        config_dir: newest.config_dir.map(|d| d.display().to_string()),
    })
}

/// `action: "repair"` — classify from the pane tail (NOT the transcript itself: the tail is
/// the only signal that a repair is actually warranted); refuse unless the class is
/// `Poisoned` or the caller passed `force:true` — guessing at an unrecognized error risks
/// masking a real, unrelated problem. A healthy/already-repaired transcript is a no-op
/// (`dropped: []`, no backup written); repairing one is byte-preserving except for the
/// dropped lines, and always backed up first.
fn recover_repair(
    sessions: &SessionManager,
    pane: &PaneInfo,
    cmd: &Value,
) -> Result<Value, String> {
    let force = matches!(cmd.get("force"), Some(Value::Bool(true)));
    let tail = pane_tail(sessions, &pane.session_uid);
    let class =
        claude_recovery::detect_api_error(&tail).map(|s| claude_recovery::classify_error(&s));
    if !force && class != Some(ErrorClass::Poisoned) {
        return Err(format!(
            "refusing to repair: class is {} (not poisoned) — pass force:true to override",
            class.map(error_class_str).unwrap_or("none")
        ));
    }

    let target = resolve_recover_target(pane, cmd)?;
    let (dropped, backup) = apply_repair_to_disk(&target.path)?;

    Ok(json!({
        "sessionId": target.session_id,
        "path": target.path.display().to_string(),
        "dropped": dropped,
        "backup": backup,
    }))
}

/// Repair the transcript at `path` in place: read it, run
/// [`claude_recovery::repair_poisoned_transcript`], and — only when it actually dropped
/// something — back up the original to `<path>.bak-<epoch-ms>` before overwriting `path`
/// with the repaired content. An already-healthy transcript is untouched (empty
/// `dropped`, no backup file). Factored out of [`recover_repair`] so this disk-mutation
/// contract (byte-preserving repair, timestamped backup, idempotency) is testable against
/// a plain temp file, independent of [`resolve_recover_target`]'s session resolution
/// (which, in production, reads real per-account transcript stores).
fn apply_repair_to_disk(path: &Path) -> Result<(Vec<usize>, Option<String>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("reading transcript {}: {e}", path.display()))?;
    let result = claude_recovery::repair_poisoned_transcript(&content);

    if result.dropped_lines.is_empty() {
        return Ok((Vec::new(), None));
    }

    let epoch_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let backup_path = format!("{}.bak-{epoch_ms}", path.display());
    std::fs::copy(path, &backup_path)
        .map_err(|e| format!("backing up {} to {backup_path}: {e}", path.display()))?;
    std::fs::write(path, &result.repaired)
        .map_err(|e| format!("writing repaired transcript {}: {e}", path.display()))?;

    Ok((result.dropped_lines, Some(backup_path)))
}

/// `action: "resume"` — resolves the target the same way `repair` does, refuses an
/// `Unknown` class without `force:true` (a `poisoned` transcript is deliberately NOT refused
/// here: the caller is expected to `repair` first per the class-policy table, but `resume`
/// doesn't second-guess that — it only hard-refuses the one class recovery can't reason
/// about at all), then shares its respawn mechanics with `restartPane { resume: true }` via
/// [`respawn_resuming`].
fn recover_resume(
    model: &mut ReadModel,
    sessions: &SessionManager,
    control_file: Option<&str>,
    pane: &PaneInfo,
    cmd: &Value,
) -> Result<(), String> {
    let force = matches!(cmd.get("force"), Some(Value::Bool(true)));
    let tail = pane_tail(sessions, &pane.session_uid);
    let class =
        claude_recovery::detect_api_error(&tail).map(|s| claude_recovery::classify_error(&s));
    if !force && class == Some(ErrorClass::Unknown) {
        return Err(
            "refusing to resume: class is unknown — pass force:true to override".to_string(),
        );
    }

    let target = resolve_recover_target(pane, cmd)?;
    let resume_target = ResumeTarget {
        session_id: target.session_id,
        cwd: target.cwd,
        config_dir: target.config_dir,
    };
    respawn_resuming(
        model,
        sessions,
        control_file,
        pane,
        &resume_target,
        cmd.get("env"),
        cmd.get("prompt").and_then(Value::as_str),
    )
}

/// A resolved Claude conversation to relaunch a pane against, abstracting over where it came
/// from (a live `SessionStart` marker vs. a same-cwd transcript found by scanning every
/// account's store) so [`respawn_resuming`] doesn't care which.
struct ResumeTarget {
    session_id: String,
    cwd: String,
    /// `None` = the default account (`~/.claude`).
    config_dir: Option<String>,
}

impl ResumeTarget {
    fn from_marker(m: &crate::claude_panes::PaneClaudeSession) -> Self {
        ResumeTarget {
            session_id: m.session_id.clone(),
            cwd: m.cwd.clone(),
            config_dir: (!m.config_dir.is_empty()).then(|| m.config_dir.clone()),
        }
    }
}

/// Kill `pane`'s current session and respawn it resuming `target`'s conversation — the
/// mechanics shared by `restartPane { resume: true }` (marker-sourced target) and
/// `recoverPane { action: "resume" }` (marker-or-scan-fallback target), factored out so
/// there's exactly one place that knows how to rebuild a resumed launch.
fn respawn_resuming(
    model: &mut ReadModel,
    sessions: &SessionManager,
    control_file: Option<&str>,
    pane: &PaneInfo,
    target: &ResumeTarget,
    env_field: Option<&Value>,
    prompt_field: Option<&str>,
) -> Result<(), String> {
    let old_uid = pane.session_uid.clone();
    let new_uid = new_id();
    let pane_id = pane.id.as_str();

    // Optional `env` override, layered over the base spawn env (same shape as `openPane`).
    // The account-rotation path uses this to respawn a pane under a different
    // `CLAUDE_CONFIG_DIR` when its Claude account hits a limit, while still resuming the
    // conversation (transcripts are on a shared store).
    let explicit_env = env_field.and_then(Value::as_object).map(|o| {
        o.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<EnvMap>()
    });
    // Restore the account (CLAUDE_CONFIG_DIR) the conversation was saved/found under, so
    // `claude --resume` finds it in the right per-account transcript store.
    let resume_config_dir = target
        .config_dir
        .as_deref()
        .filter(|d| crate::claude_panes::valid_config_dir(d));
    let env_override = resume_env(pane.env.as_ref(), explicit_env, resume_config_dir);
    // The account the effective env resolved to (explicit `env` override wins over the
    // target's) — captured before `env_override` moves into `SpawnOptions`, for the typed
    // shell-pane resume line below. Empty ⇒ the default account.
    let resume_cfg_prefix = env_override
        .as_ref()
        .and_then(|e| e.get("CLAUDE_CONFIG_DIR"))
        .filter(|d| crate::claude_panes::valid_config_dir(d))
        .map(|d| format!("CLAUDE_CONFIG_DIR='{d}' "))
        .unwrap_or_default();
    // A directly-launched claude pane is resumed by RE-LAUNCHING its original command with
    // `--resume <id>` appended (see [`resume_command`]) — so the resumed session keeps every
    // flag it was born with (`--mcp-config`, `--append-system-prompt-file`,
    // `--dangerously-skip-permissions`, `--model`). Typing a bare `claude --resume <id>`
    // instead (the shell-pane path below) would drop all of them: the agent would come back
    // tool-less, prompt-wedged, and persona-less. When we rebuild the launch we also anchor
    // the pane at the conversation's own cwd and skip the typed line.
    let resume_launch = resume_launch(
        pane.command.as_deref(),
        pane.args.as_deref(),
        &target.session_id,
    );
    let resume_cwd = crate::claude_panes::valid_resume_cwd(&target.cwd).then(|| target.cwd.clone());
    let opts = SpawnOptions {
        uid: new_uid.clone(),
        shell: pane.shell.clone(),
        args: match &resume_launch {
            Some(l) => l.args.clone(),
            None => pane.args.clone(),
        },
        command: resume_launch
            .as_ref()
            .map(|l| l.command.clone())
            .or_else(|| pane.command.clone()),
        cwd: if resume_launch.is_some() {
            resume_cwd.clone().or_else(|| pane.cwd.clone())
        } else {
            pane.cwd.clone()
        },
        env: crate::session_manager::stamp_unattended(env_override.clone(), pane.meta.as_ref()),
        cols: None,
        rows: None,
        pane_id: Some(pane_id.to_string()),
        integration: None,
        control_file: control_file.map(str::to_string),
    };
    crate::persistence::session_log::record(
        "kill-request",
        &old_uid,
        "control:restartPane(resume)",
        Some(pane_id),
    );
    sessions.kill(&old_uid);
    sessions.create(opts).map_err(|e| e.to_string())?;
    model.respawn_pane(pane_id, &new_uid);
    model.set_pane_env(pane_id, env_override);

    // Shell-hosted pane (no direct claude command to rebuild): fall back to typing the
    // resume line into the fresh shell. Direct claude panes already relaunched with
    // `--resume` baked in, so skip the typed line for them.
    if resume_launch.is_none() {
        // Prefix the resolved account so the typed resume runs against the right per-account
        // transcript store (captured above, pre-move). Empty ⇒ default.
        let prefix = &resume_cfg_prefix;
        let line = if crate::claude_panes::valid_resume_cwd(&target.cwd) {
            format!(
                "cd '{}' && {prefix}claude --resume {}\r",
                target.cwd, target.session_id
            )
        } else {
            format!("{prefix}claude --resume {}\r", target.session_id)
        };
        sessions.write(&new_uid, &line);
    }
    if let Some(p) = prompt_field {
        crate::resume_queue::enqueue(&target.session_id, p)?;
    }
    Ok(())
}

/// The env a resumed pane spawns with. Precedence, highest first: the caller's explicit `env`
/// (the account-rotation path deliberately moves the pane to a DIFFERENT account whose
/// shared-store transcript it can still resume), then the resume target's recorded
/// `CLAUDE_CONFIG_DIR` (only when the caller set none), then the env the pane was spawned with.
/// Every stored key the higher layers don't name survives.
fn resume_env(
    stored: Option<&EnvMap>,
    explicit: Option<EnvMap>,
    target_config_dir: Option<&str>,
) -> Option<EnvMap> {
    let mut over = explicit;
    if let Some(dir) = target_config_dir {
        let already_set = over
            .as_ref()
            .is_some_and(|e| e.contains_key("CLAUDE_CONFIG_DIR"));
        if !already_set {
            over.get_or_insert_with(EnvMap::default)
                .insert("CLAUDE_CONFIG_DIR".to_string(), dir.to_string());
        }
    }
    merge_env(stored, over)
}

/// Layer a restart's `env` over the env the pane was spawned with (restart keys win). `None`
/// only when neither side has any env.
fn merge_env(stored: Option<&EnvMap>, over: Option<EnvMap>) -> Option<EnvMap> {
    match (stored, over) {
        (None, o) => o,
        (Some(s), None) => Some(s.clone()),
        (Some(s), Some(o)) => {
            let mut m = s.clone();
            m.extend(o);
            Some(m)
        }
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
        env: crate::session_manager::stamp_unattended(env.clone(), meta.as_ref()),
        cols: None,
        rows: None,
        pane_id: Some(pane_id.clone()),
        integration,
        control_file: control_file.map(str::to_string),
    };
    sessions.create(opts).map_err(|e| e.to_string())?;
    // The header's second line (e.g. a launcher's `<team>/<session> · <state>`); blank = none.
    let subtitle = spec
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok(PaneInfo {
        id: pane_id,
        session_uid,
        label,
        subtitle,
        color,
        command,
        args,
        cwd,
        shell,
        status: PaneStatus::Running,
        exit_code: None,
        meta: meta.filter(|m| !m.is_empty()),
        talk: false,
        env,
    })
}

/// If a `newPane` spec names a `project` (a project id or name), resolve it from the registry
/// and fill the pane's `cwd` + frame `color` from the project — without clobbering values the
/// caller set explicitly — then bump the project's recency so opening via the control plane
/// reorders the sidebar rail exactly like the GUI's "open project". An unknown handle is an
/// error (the `newPane` fails rather than spawning a homeless pane). A spec with no `project`
/// field is left untouched.
fn resolve_project_into_spec(spec: &mut Value) -> Result<(), String> {
    let handle = match spec.get("project").and_then(Value::as_str) {
        Some(h) => h.to_string(),
        None => return Ok(()),
    };
    let project = crate::persistence::projects::resolve_checked(&handle)?
        .ok_or_else(|| format!("unknown project: {handle}"))?;
    if let Value::Object(map) = spec {
        if !map.get("cwd").map(Value::is_string).unwrap_or(false) {
            map.insert("cwd".into(), Value::String(project.path.clone()));
        }
        if !map.get("color").map(Value::is_string).unwrap_or(false) {
            map.insert("color".into(), Value::String(project.color.clone()));
        }
    }
    crate::persistence::projects::upsert_project_by_root(&project.path);
    Ok(())
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
    if let Some(window_id) = window_id_field(cmd) {
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

/// Max length (chars) of a tab title set over the control API.
const MAX_TAB_TITLE_CHARS: usize = 80;

fn existing_tab_id(cmd: &Value, model: &ReadModel, needs: &str) -> Result<String, (u16, String)> {
    let tab_id = cmd
        .get("tabId")
        .and_then(Value::as_str)
        .ok_or((400, needs.to_string()))?;
    if model.tab_window(tab_id).is_none() {
        return Err((404, format!("no such tab: {tab_id}")));
    }
    Ok(tab_id.to_string())
}

/// Resolve a `renameTab` command to `(tab id, trimmed title)`, or `(http status, message)`.
fn resolve_rename_tab(cmd: &Value, model: &ReadModel) -> Result<(String, String), (u16, String)> {
    const NEEDS: &str = "renameTab needs a tabId and a title";
    if cmd.get("tabId").and_then(Value::as_str).is_none() {
        return Err((400, NEEDS.to_string()));
    }
    let title = cmd
        .get("title")
        .and_then(Value::as_str)
        .ok_or((400, NEEDS.to_string()))?
        .trim();
    if title.is_empty() {
        return Err((400, NEEDS.to_string()));
    }
    if title.chars().count() > MAX_TAB_TITLE_CHARS {
        return Err((
            400,
            format!("tab title too long (max {MAX_TAB_TITLE_CHARS} chars)"),
        ));
    }
    let tab_id = existing_tab_id(cmd, model, NEEDS)?;
    Ok((tab_id, title.to_string()))
}

/// Normalise `#rgb` / `#rrggbb` (either case) to lowercase `#rrggbb`.
pub(crate) fn normalize_hex_color(s: &str) -> Option<String> {
    let h = s.strip_prefix('#')?;
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        6 => Some(format!("#{}", h.to_ascii_lowercase())),
        3 => {
            let mut out = String::from("#");
            for c in h.chars() {
                let c = c.to_ascii_lowercase();
                out.push(c);
                out.push(c);
            }
            Some(out)
        }
        _ => None,
    }
}

/// Check every `attach` group's optional tab `color`: absent or null is fine, a string must be
/// `#rgb` / `#rrggbb` (as for `recolorTab`), anything else is an error naming the group.
fn validate_attach_colors(cmd: &Value) -> Result<(), String> {
    let Some(groups) = cmd.get("groups").and_then(Value::as_array) else {
        return Ok(());
    };
    for (i, g) in groups.iter().enumerate() {
        match g.get("color") {
            None | Some(Value::Null) => {}
            Some(Value::String(s)) if normalize_hex_color(s).is_some() => {}
            Some(v) => {
                let shown = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Err(format!(
                    "invalid color \"{shown}\" in groups[{i}] (expected #rrggbb, or null for none)"
                ));
            }
        }
    }
    Ok(())
}

/// Resolve a `recolorTab` command to `(tab id, colour or None to clear)`.
fn resolve_recolor_tab(
    cmd: &Value,
    model: &ReadModel,
) -> Result<(String, Option<String>), (u16, String)> {
    const NEEDS: &str = "recolorTab needs a tabId and a color";
    if cmd.get("tabId").and_then(Value::as_str).is_none() {
        return Err((400, NEEDS.to_string()));
    }
    let color = match cmd.get("color") {
        Some(Value::Null) => None,
        Some(v) => match v.as_str().and_then(normalize_hex_color) {
            Some(c) => Some(c),
            None => {
                let shown = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                return Err((
                    400,
                    format!("invalid color \"{shown}\" (expected #rrggbb, or null to clear)"),
                ));
            }
        },
        None => {
            return Err((
                400,
                "invalid color \"undefined\" (expected #rrggbb, or null to clear)".to_string(),
            ))
        }
    };
    let tab_id = existing_tab_id(cmd, model, NEEDS)?;
    Ok((tab_id, color))
}

/// The layout names a tab can take (the GUI's `layout_name` set).
pub const LAYOUT_NAMES: [&str; 6] = ["auto", "single", "columns", "rows", "grid", "main-stack"];

/// Resolve a `setLayout` command to `(tab id, layout name)`, or `(http status, message)`.
/// The tab is `tabId`, else the active tab of `windowId`, else the tab holding `paneId`.
fn resolve_set_layout(cmd: &Value, model: &ReadModel) -> Result<(String, String), (u16, String)> {
    const NEEDS: &str = "setLayout needs a tabId (or a windowId, which targets its active tab) \
         and a layout";
    let layout = match cmd.get("layout").and_then(Value::as_str) {
        Some(l) => l.to_string(),
        None => return Err((400, NEEDS.to_string())),
    };
    if !LAYOUT_NAMES.contains(&layout.as_str()) {
        return Err((
            400,
            format!(
                "unknown layout \"{layout}\" (expected one of: {})",
                LAYOUT_NAMES.join(", ")
            ),
        ));
    }
    let tab_id = if let Some(t) = cmd.get("tabId").and_then(Value::as_str) {
        t.to_string()
    } else if let Some(w) = window_id_field(cmd) {
        model
            .active_tab_id(w)
            .ok_or((404, format!("no such window or tab: windowId {w}")))?
    } else if let Some(c) = cmd
        .get("paneId")
        .and_then(Value::as_str)
        .and_then(|p| model.coords_of(p))
    {
        c.tab_id
    } else {
        return Err((400, NEEDS.to_string()));
    };
    if model.tab_window(&tab_id).is_none() {
        return Err((404, format!("no such tab: {tab_id}")));
    }
    Ok((tab_id, layout))
}

fn str_field(cmd: &Value, key: &str) -> Result<String, String> {
    cmd.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field: {key}"))
}

/// Read `windowId` from a command, accepting either a JSON number OR a numeric string. Clients
/// that build the request by hand (e.g. `jq -r '.windows[0].windowId'` → the string `"0"`) would
/// otherwise fail the strict `as_i64` and hit the misleading "needs a paneId or windowId".
/// `newPane`'s optional top-level `tabId`: the tab to spawn into instead of the active one.
fn new_pane_tab_id(cmd: &Value) -> Option<&str> {
    cmd.get("tabId").and_then(Value::as_str)
}

/// Reject a malformed or unknown `newPane` / `attach` `tabId`, or one outside an explicit
/// `windowId`.
fn validate_new_pane_tab(cmd: &Value, model: &ReadModel) -> Result<(), String> {
    let Some(v) = cmd.get("tabId").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let Some(tab_id) = v.as_str() else {
        return Err("tabId must be a string".into());
    };
    let Some(win) = model.tab_window(tab_id) else {
        return Err(format!("no such tab: {tab_id}"));
    };
    match window_id_field(cmd) {
        Some(w) if w != win => Err(format!("tabId {tab_id} is not in window {w}")),
        _ => Ok(()),
    }
}

fn window_id_field(cmd: &Value) -> Option<i64> {
    cmd.get("windowId").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
}

/// The launch a resumed claude pane is respawned with: the original program and argv, minus any
/// earlier session-selection flags, plus `--resume <session>`.
/// A claude launch rebuilt for a resume (see [`resume_launch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeLaunch {
    /// The program (direct argv) or the whole command line (command-string launch).
    pub command: String,
    /// The argv for a direct launch; `None` for a command-string launch.
    pub args: Option<Vec<String>>,
}

/// Claude Code flags that pick which conversation a launch attaches to. A resume replaces them
/// all with a single `--resume <session>`. Each entry is `(flag, takes_value)`; the value is
/// optional for `--resume`/`-r`/`--from-pr`/`--teleport` (consumed only when the next token is
/// not itself a flag) and required for `--session-id`.
const SESSION_FLAGS: &[(&str, bool)] = &[
    ("--resume", true),
    ("-r", true),
    ("--continue", false),
    ("-c", false),
    ("--session-id", true),
    ("--from-pr", true),
    ("--teleport", true),
    ("--fork-session", false),
];

/// Which of `tokens` survive a resume: the session-selection flags (and their values) are
/// removed, and so are the positional tokens (the launch's initial prompt) — a resume
/// continues the conversation, it does not re-send the first message. When the classifier
/// meets an unknown flag it cannot tell a prompt from a flag value, so only the session flags
/// go (conservative). `shell_syntax` marks a command string: a token holding unquoted shell
/// syntax (`|`, `;`, `$(…)`, …) is not a claude argument, so nothing positional is dropped.
fn keep_for_resume(tokens: &[&str], shell_syntax: bool) -> Vec<bool> {
    let mut keep = keep_after_stripping_session_flags(tokens);
    let c = crate::claude_argv::classify(tokens);
    let shelly = shell_syntax
        && tokens.iter().any(|t| {
            !t.starts_with(['\'', '"']) && t.contains(['|', '&', ';', '<', '>', '(', ')', '`', '$'])
        });
    if !c.has_unknown && !shelly {
        for (k, kind) in keep.iter_mut().zip(&c.kinds) {
            if matches!(
                kind,
                crate::claude_argv::TokenKind::Positional
                    | crate::claude_argv::TokenKind::Separator
            ) {
                *k = false;
            }
        }
    }
    keep
}

/// Which of `tokens` survive removing the session-selection flags (and their values).
fn keep_after_stripping_session_flags(tokens: &[&str]) -> Vec<bool> {
    let mut keep = vec![true; tokens.len()];
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        // `--flag=value` form.
        if let Some((flag, _)) = tok.split_once('=') {
            if flag.starts_with("--") && SESSION_FLAGS.iter().any(|(f, _)| *f == flag) {
                keep[i] = false;
            }
        } else if let Some((_, takes_value)) = SESSION_FLAGS.iter().find(|(f, _)| *f == tok) {
            keep[i] = false;
            if *takes_value {
                if let Some(next) = tokens.get(i + 1) {
                    if !next.starts_with('-') {
                        keep[i + 1] = false;
                        i += 1;
                    }
                }
            }
        }
        i += 1;
    }
    keep
}

/// Byte spans of the whitespace-separated tokens of `s`, treating a `'…'` or `"…"` run as part
/// of its token so a quoted path with spaces stays whole.
fn shell_token_spans(s: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut quote: Option<char> = None;
    for (i, ch) in s.char_indices() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                start.get_or_insert(i);
            }
            None if ch.is_whitespace() => {
                if let Some(st) = start.take() {
                    spans.push((st, i));
                }
            }
            None => {
                start.get_or_insert(i);
            }
        }
    }
    if let Some(st) = start {
        spans.push((st, s.len()));
    }
    spans
}

/// `tokens` (the argv after the program) with the session-selection flags and their values
/// removed — the flags a resume replaces with its own `--resume <id>`.
pub fn strip_session_flag_tokens(tokens: &[String]) -> Vec<String> {
    let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let keep = keep_after_stripping_session_flags(&refs);
    tokens
        .iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(t, _)| t.clone())
        .collect()
}

fn is_claude_program(tok: &str) -> bool {
    matches!(
        tok.rsplit(['/', '\\']).next(),
        Some("claude") | Some("claude.exe")
    )
}

/// Rebuild a claude launch for a resume: keep the original program and every original argument
/// (`--model`, `--settings`, `--mcp-config`, `--append-system-prompt-file`, permission flags),
/// drop earlier session-selection flags (`--resume <id>`, `--resume=<id>`, `-r <id>`,
/// `--continue`, `-c`, `--session-id <id>`, `--from-pr`, `--teleport`, `--fork-session`) and add
/// `--resume <session_id>` once. The launch's positional prompt is dropped too (a resume
/// continues the conversation; `restartPane`'s explicit `prompt` field is how a message is sent
/// on resume), unless the argv holds a flag this build does not know.
///
/// Handles both launch shapes: a direct argv (`command` is the program, `args` its argv) and a
/// flags-in-the-command-string launch (`command` = `claude --model m …`, no args), where every
/// byte of the string outside the removed flags is preserved. Returns `None` when the launch is
/// not a claude one (a shell-hosted pane — the caller types a bare `claude --resume` into it).
pub fn resume_launch(
    command: Option<&str>,
    args: Option<&[String]>,
    session_id: &str,
) -> Option<ResumeLaunch> {
    let command = command?.trim();
    // Direct argv: the program is the whole `command`, the flags live in `args`.
    if let Some(args) = args.filter(|a| !a.is_empty()) {
        if !is_claude_program(command) {
            return None;
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let keep = keep_for_resume(&refs, false);
        let mut out: Vec<String> = args
            .iter()
            .zip(keep)
            .filter(|(_, k)| *k)
            .map(|(a, _)| a.clone())
            .collect();
        out.push("--resume".to_string());
        out.push(session_id.to_string());
        return Some(ResumeLaunch {
            command: command.to_string(),
            args: Some(out),
        });
    }
    // Command string: only rewrite a direct claude invocation; never append flags to a shell.
    let spans = shell_token_spans(command);
    let toks: Vec<&str> = spans.iter().map(|&(a, b)| &command[a..b]).collect();
    let prog = toks.iter().position(|t| is_claude_program(t))?;
    // Only the tokens after the program are flags (an env prefix or wrapper before it is kept).
    let keep = keep_for_resume(&toks[prog + 1..], true);
    let mut out = String::new();
    let mut last_end = 0;
    for (idx, &(a, b)) in spans.iter().enumerate() {
        let removed = idx > prog && !keep[idx - prog - 1];
        if removed {
            last_end = b;
            continue;
        }
        out.push_str(&command[last_end..a]);
        out.push_str(&command[a..b]);
        last_end = b;
    }
    out.push_str(&format!(" --resume {session_id}"));
    Some(ResumeLaunch {
        command: out,
        args: None,
    })
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::readmodel::{Activity, TabInfo, WindowInfo};
    use tokio::sync::mpsc::unbounded_channel;

    fn model_one_window() -> ReadModel {
        let mut m = ReadModel::new();
        m.add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab 1".into(),
                color: None,
                layout: "auto".into(),
                panes: vec![],
                pane_group: None,
            }],
        });
        m
    }

    fn sessions() -> SessionManager {
        let (tx, _rx) = unbounded_channel();
        SessionManager::new(tx)
    }

    /// A fresh `SpeechService` backed by a scratch settings path unique to the calling
    /// test, so `cargo test` never touches (or races on) the developer's real speech.json.
    fn speech() -> SpeechService {
        let path = std::env::temp_dir().join(format!(
            "hp-dispatch-speech-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        SpeechService::new(path)
    }

    // newPane needs a tokio runtime (SessionManager::create spawns a driver task).
    // ---- restart keeps the pane's spawn env (hyperpanes#43) ----

    #[test]
    fn merge_env_layers_override_over_stored() {
        let stored: EnvMap = [("A", "1"), ("B", "2")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let over: EnvMap = [("B", "9"), ("C", "3")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let m = merge_env(Some(&stored), Some(over)).unwrap();
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "9");
        assert_eq!(m["C"], "3");
        assert!(merge_env(None, None).is_none());
        assert_eq!(merge_env(Some(&stored), None).unwrap(), stored);
    }

    #[test]
    fn resume_env_rotates_the_account_and_keeps_every_other_stored_key() {
        let env = |pairs: &[(&str, &str)]| -> EnvMap {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let stored = env(&[("CLAUDE_CONFIG_DIR", "/acct-a"), ("PERSONA_DIR", "/p")]);
        // Plain resume: the target's recorded account beats the stored one.
        let e = resume_env(Some(&stored), None, Some("/acct-b")).unwrap();
        assert_eq!(e["CLAUDE_CONFIG_DIR"], "/acct-b");
        assert_eq!(e["PERSONA_DIR"], "/p");
        // Rotation: an explicit env beats both, and the other stored keys survive.
        let rot = env(&[("CLAUDE_CONFIG_DIR", "/acct-c")]);
        let e = resume_env(Some(&stored), Some(rot), Some("/acct-b")).unwrap();
        assert_eq!(e["CLAUDE_CONFIG_DIR"], "/acct-c");
        assert_eq!(e["PERSONA_DIR"], "/p");
        // No target dir, no explicit env: the stored env is reused as-is.
        assert_eq!(resume_env(Some(&stored), None, None).unwrap(), stored);
        // Nothing stored (a GUI-native pane): only the target's account is set.
        let e = resume_env(None, None, Some("/acct-b")).unwrap();
        assert_eq!(e, env(&[("CLAUDE_CONFIG_DIR", "/acct-b")]));
    }

    async fn wait_for_output(s: &SessionManager, uid: &str, needle: &str) -> String {
        for _ in 0..100 {
            let t = pane_tail(s, uid);
            if t.contains(needle) {
                return t;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        pane_tail(s, uid)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restart_pane_keeps_original_env_and_layers_restart_env() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": {
            "command": "echo \"mark:$MY_VAR:$OTHER:\"; sleep 30",
            "env": { "MY_VAR": "x" } } });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        let id = r.body["result"].as_str().unwrap().to_string();

        let r = handle_command(
            &mut m,
            &s,
            None,
            None,
            &json!({ "type": "restartPane", "paneId": id, "env": { "OTHER": "y" } }),
            &speech(),
        );
        assert_eq!(r.status, 200);
        let uid = m.pane(&id).unwrap().session_uid.clone();
        let out = wait_for_output(&s, &uid, "mark:").await;
        assert!(
            out.contains("mark:x:y:"),
            "restart env lost MY_VAR: {out:?}"
        );

        // The stored env is now the merged env: a second restart that overrides OTHER again
        // still keeps MY_VAR.
        handle_command(
            &mut m,
            &s,
            None,
            None,
            &json!({ "type": "restartPane", "paneId": id, "env": { "OTHER": "z" } }),
            &speech(),
        );
        let uid = m.pane(&id).unwrap().session_uid.clone();
        let out = wait_for_output(&s, &uid, "mark:").await;
        assert!(out.contains("mark:x:z:"), "second restart: {out:?}");
        // The env must never leak through the serialized state.
        let state = serde_json::to_string(&m.state_for_scope(None, &|_| Activity::Idle)).unwrap();
        assert!(
            !state.contains("\"env\"") && !state.contains("\"MY_VAR\""),
            "{state}"
        );
    }

    /// restartPane kills the old session and spawns the same command right away. A program
    /// of the old session that was still shutting down raced the new one: a launcher that
    /// refuses to run beside its own previous run exited, and the pane closed. The new
    /// session now starts only once every process of the old one is gone. Here the old
    /// session leaves a child that ignores the hang-up and lives 1.5 s more.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn restart_pane_starts_the_new_session_only_once_the_old_one_has_exited() {
        let dir = std::env::temp_dir().join(format!(
            "hp-respawn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = r#"if [ "$PHASE" = 2 ]; then
  if [ -e "$D/old-gone" ]; then echo "phase2:waited"; else echo "phase2:raced"; fi; sleep 30
else
  (trap '' HUP; sleep 1.5; touch "$D/old-gone") & echo "phase1:up"; wait
fi"#;
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": {
            "command": script, "env": { "D": dir.to_str().unwrap() } } });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        let id = r.body["result"].as_str().unwrap().to_string();
        let old = m.pane(&id).unwrap().session_uid.clone();
        assert!(wait_for_output(&s, &old, "phase1:up")
            .await
            .contains("phase1:up"));

        let restarted = std::time::Instant::now();
        let r = handle_command(
            &mut m,
            &s,
            None,
            None,
            &json!({ "type": "restartPane", "paneId": id, "env": { "PHASE": "2" } }),
            &speech(),
        );
        assert_eq!(r.status, 200);
        let uid = m.pane(&id).unwrap().session_uid.clone();
        assert_ne!(uid, old);
        let out = wait_for_output(&s, &uid, "phase2:").await;
        let took = restarted.elapsed();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            out.contains("phase2:waited"),
            "the new session ran before the old one exited: {out:?}"
        );
        assert!(
            took < std::time::Duration::from_millis(4_500),
            "it spawned when the old session exited, not at the 5 s bound: {took:?}"
        );
        handle_command(
            &mut m,
            &s,
            None,
            None,
            &json!({ "type": "closePane", "paneId": id }),
            &speech(),
        );
    }

    #[tokio::test]
    async fn new_pane_spawns_inserts_and_returns_the_pane_id() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": { "label": "w", "command": "echo hi" } });
        let r = handle_command(&mut m, &s, Some("C:/control.json"), None, &cmd, &speech());
        assert_eq!(r.status, 200);
        assert!(r.notify_state);
        let id = r.body["result"].as_str().unwrap().to_string();
        assert!(m.pane(&id).is_some());
        assert_eq!(m.pane(&id).unwrap().label, "w");
        assert_eq!(m.pane(&id).unwrap().subtitle, None);
    }

    #[tokio::test]
    async fn new_pane_tab_id_lets_a_tab_scoped_token_spawn_into_its_background_tab() {
        let mut m = model_one_window();
        assert!(m.insert_tab(
            1,
            TabInfo {
                id: "t2".into(),
                title: "Mine".into(),
                color: None,
                layout: "auto".into(),
                panes: vec![],
                pane_group: None,
            }
        ));
        let s = sessions();
        let mine = Scope {
            tab_ids: Some(vec!["t2".into()]),
            ..Default::default()
        };
        // t1 is active, so without tabId a t2-scoped token cannot spawn at all.
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": { "command": "echo hi" } });
        let r = handle_command(&mut m, &s, None, Some(&mine), &cmd, &speech());
        assert_eq!(r.status, 403, "{}", r.body);
        // With tabId it lands in t2 and the active tab is untouched.
        let cmd = json!({ "type": "newPane", "tabId": "t2", "pane": { "command": "echo hi" } });
        let r = handle_command(&mut m, &s, None, Some(&mine), &cmd, &speech());
        assert_eq!(r.status, 200, "{}", r.body);
        let id = r.body["result"].as_str().unwrap();
        assert_eq!(m.coords_of(id).unwrap().tab_id, "t2");
        assert_eq!(m.active_tab_id(1).as_deref(), Some("t1"));
        // Another tab stays out of scope.
        let cmd = json!({ "type": "newPane", "tabId": "t1", "pane": { "command": "echo hi" } });
        let r = handle_command(&mut m, &s, None, Some(&mine), &cmd, &speech());
        assert_eq!(r.status, 403, "{}", r.body);
    }

    #[tokio::test]
    async fn attach_panes_tab_id_merges_into_that_tab_not_the_active_one() {
        let mut m = model_one_window();
        assert!(m.insert_tab(
            1,
            TabInfo {
                id: "t2".into(),
                title: "Mine".into(),
                color: None,
                layout: "auto".into(),
                panes: vec![],
                pane_group: None,
            }
        ));
        let s = sessions();
        let groups = json!([{ "panes": [{ "command": "echo a" }, { "command": "echo b" }] }]);
        // t1 is active; tabId puts both panes in t2 and leaves the active tab alone.
        let cmd = json!({ "type": "attach", "windowId": 1, "tabId": "t2", "as": "panes", "groups": groups });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 200, "{}", r.body);
        let ids = r.body["result"].as_array().unwrap();
        assert_eq!(ids.len(), 2);
        for id in ids {
            assert_eq!(m.coords_of(id.as_str().unwrap()).unwrap().tab_id, "t2");
        }
        assert_eq!(m.active_tab_id(1).as_deref(), Some("t1"));
        // windowId is optional with tabId.
        let cmd = json!({ "type": "attach", "tabId": "t2", "as": "panes", "groups": groups });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 200, "{}", r.body);
        // Without tabId: the active tab, as before.
        let cmd = json!({ "type": "attach", "windowId": 1, "as": "panes", "groups": groups });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        let id = r.body["result"][0].as_str().unwrap();
        assert_eq!(m.coords_of(id).unwrap().tab_id, "t1");
        // A tab-scoped token may merge into its own background tab, not another one.
        let mine = Scope {
            tab_ids: Some(vec!["t2".into()]),
            ..Default::default()
        };
        let cmd = json!({ "type": "attach", "tabId": "t2", "as": "panes", "groups": groups });
        let r = handle_command(&mut m, &s, None, Some(&mine), &cmd, &speech());
        assert_eq!(r.status, 200, "{}", r.body);
        let cmd = json!({ "type": "attach", "tabId": "t1", "as": "panes", "groups": groups });
        let r = handle_command(&mut m, &s, None, Some(&mine), &cmd, &speech());
        assert_eq!(r.status, 403, "{}", r.body);
    }

    #[tokio::test]
    async fn attach_tab_id_is_validated_and_spawns_nothing_when_bad() {
        let mut m = model_one_window();
        let s = sessions();
        let groups = json!([{ "panes": [{ "command": "echo a" }] }]);
        let before = m.panes().len();
        for cmd in [
            json!({ "type": "attach", "windowId": 1, "tabId": "nope", "as": "panes", "groups": groups }),
            json!({ "type": "attach", "windowId": 1, "tabId": 7, "as": "panes", "groups": groups }),
            json!({ "type": "attach", "windowId": 2, "tabId": "t1", "as": "panes", "groups": groups }),
            json!({ "type": "attach", "windowId": 1, "tabId": "t1", "groups": groups }),
            json!({ "type": "attach", "windowId": 1, "tabId": "t1", "as": "tab", "groups": groups }),
        ] {
            let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
            assert_eq!(r.status, 400, "{cmd}: {}", r.body);
        }
        assert_eq!(m.panes().len(), before);
    }

    #[tokio::test]
    async fn new_pane_keeps_its_subtitle() {
        let mut m = model_one_window();
        let s = sessions();
        for (given, want) in [
            (
                json!("team/member-a · ready"),
                Some("team/member-a · ready"),
            ),
            (json!("   "), None),
        ] {
            let cmd = json!({ "type": "newPane", "windowId": 1, "pane": { "label": "w", "command": "echo hi", "subtitle": given } });
            let r = handle_command(&mut m, &s, Some("C:/control.json"), None, &cmd, &speech());
            assert_eq!(r.status, 200);
            let id = r.body["result"].as_str().unwrap().to_string();
            assert_eq!(m.pane(&id).unwrap().subtitle.as_deref(), want);
        }
    }

    #[tokio::test]
    async fn new_pane_with_flat_spec_fields_is_400() {
        let mut m = model_one_window();
        let s = sessions();
        // Spawn spec fields at the top level instead of nested under "pane" — a common
        // hand-authored mistake that must be rejected, not silently spawn a default shell.
        let cmd = json!({ "type": "newPane", "windowId": 1, "command": "claude" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 400);
        assert!(r.body["error"].as_str().unwrap().contains("pane"));
        assert!(m.panes().is_empty());
    }

    #[tokio::test]
    async fn new_pane_with_non_object_pane_is_400() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": "claude" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 400);
        assert!(r.body["error"].as_str().unwrap().contains("pane"));
        assert!(m.panes().is_empty());
    }

    #[tokio::test]
    async fn new_pane_with_empty_pane_object_still_spawns_default_shell() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": {} });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 200);
        let id = r.body["result"].as_str().unwrap().to_string();
        assert!(m.pane(&id).is_some());
    }

    #[tokio::test]
    async fn new_pane_with_no_pane_key_at_all_still_spawns_default_shell() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": 1 });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 200);
        let id = r.body["result"].as_str().unwrap().to_string();
        assert!(m.pane(&id).is_some());
    }

    #[tokio::test]
    async fn set_meta_echoes_true_merged_synchronously() {
        let mut m = model_one_window();
        let s = sessions();
        // Spawn a pane to target.
        let open = json!({ "type": "newPane", "windowId": 1, "pane": {} });
        let id = handle_command(&mut m, &s, None, None, &open, &speech()).body["result"]
            .as_str()
            .unwrap()
            .to_string();
        let cmd =
            json!({ "type": "setMeta", "paneId": id, "meta": { "role": "worker", "task": "x" } });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 200);
        assert_eq!(r.body["result"]["role"], json!("worker"));
        assert_eq!(r.body["result"]["task"], json!("x"));
        // Delete a key — echoed merged drops it.
        let del = json!({ "type": "setMeta", "paneId": id, "meta": { "task": null } });
        let r2 = handle_command(&mut m, &s, None, None, &del, &speech());
        assert!(r2.body["result"].get("task").is_none());
        assert_eq!(r2.body["result"]["role"], json!("worker"));
    }

    #[tokio::test]
    async fn new_pane_with_unknown_project_is_500_and_spawns_nothing() {
        let mut m = model_one_window();
        let s = sessions();
        // A handle that matches no remembered project fails the command (no homeless pane).
        // Uses a uuid so it can never collide with a real project on the test machine — and
        // because resolution fails first, this never writes to the registry.
        let bogus = format!("no-such-project-{}", uuid::Uuid::new_v4());
        let cmd = json!({ "type": "newPane", "windowId": 1, "pane": { "project": bogus } });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 500);
        assert_eq!(r.body["error"], json!(format!("unknown project: {bogus}")));
        assert!(!r.projects_dirty);
        // Nothing landed in the model.
        assert!(m.panes().is_empty());
    }

    #[test]
    fn resolve_project_into_spec_is_noop_without_a_project_field() {
        // No `project` key → spec untouched, no registry read/write.
        let mut spec = json!({ "label": "x", "command": "echo hi" });
        let before = spec.clone();
        assert!(resolve_project_into_spec(&mut spec).is_ok());
        assert_eq!(spec, before);
    }

    #[test]
    fn missing_type_is_400() {
        let mut m = model_one_window();
        let s = sessions();
        let r = handle_command(&mut m, &s, None, None, &json!({ "paneId": "p" }), &speech());
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
            &speech(),
        );
        assert_eq!(r.status, 400);
        let msg = r.body["error"].as_str().unwrap();
        assert!(msg.contains("tabId") && msg.contains("windowId"), "{msg}");
    }

    fn set_layout(m: &mut ReadModel, cmd: Value) -> DispatchResult {
        let s = sessions();
        handle_command(m, &s, None, None, &cmd, &speech())
    }

    #[test]
    fn set_layout_by_tab_id_or_window_id() {
        let mut m = model_one_window();
        let r = set_layout(
            &mut m,
            json!({"type":"setLayout","tabId":"t1","layout":"single"}),
        );
        assert_eq!(r.status, 200);
        assert_eq!(m.tabs().next().unwrap().1.layout, "single");
        let r = set_layout(
            &mut m,
            json!({"type":"setLayout","windowId":1,"layout":"grid"}),
        );
        assert_eq!(r.status, 200);
        assert_eq!(m.tabs().next().unwrap().1.layout, "grid");
    }

    #[test]
    fn set_layout_unknown_tab_or_layout_is_an_error() {
        let mut m = model_one_window();
        let r = set_layout(
            &mut m,
            json!({"type":"setLayout","tabId":"nope","layout":"single"}),
        );
        assert_eq!(r.status, 404);
        let r = set_layout(
            &mut m,
            json!({"type":"setLayout","tabId":"t1","layout":"bogus"}),
        );
        assert_eq!(r.status, 400);
        assert!(r.body["error"].as_str().unwrap().contains("main-stack"));
        assert_eq!(m.tabs().next().unwrap().1.layout, "auto");
    }

    fn tab_cmd(m: &mut ReadModel, cmd: Value) -> DispatchResult {
        set_layout(m, cmd)
    }

    fn first_tab(m: &ReadModel) -> &TabInfo {
        m.tabs().next().unwrap().1
    }

    fn tab_by_id<'a>(m: &'a ReadModel, id: &str) -> &'a TabInfo {
        m.tabs().find(|(_, t)| t.id == id).unwrap().1
    }

    #[test]
    fn attach_tab_takes_a_normalised_manual_color() {
        let mut m = model_one_window();
        let r = tab_cmd(
            &mut m,
            json!({"type":"attach","windowId":1,"as":"tab","groups":[
                {"title":"a","color":"#ABC","panes":[]},
                {"title":"b","color":null,"panes":[]},
                {"title":"c","panes":[]}
            ]}),
        );
        assert_eq!(r.status, 200, "{:?}", r.body);
        let ids: Vec<String> = r.body["result"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(tab_by_id(&m, &ids[0]).color.as_deref(), Some("#aabbcc"));
        assert_eq!(tab_by_id(&m, &ids[1]).color, None);
        assert_eq!(tab_by_id(&m, &ids[2]).color, None);
        // The manual colour is the same field recolorTab sets and clears.
        let r = tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":ids[0],"color":null}),
        );
        assert_eq!(r.status, 200);
        assert_eq!(tab_by_id(&m, &ids[0]).color, None);
    }

    #[test]
    fn attach_bad_group_color_is_400_and_adds_no_tab() {
        for bad in [json!("red"), json!("#12345"), json!(7)] {
            let mut m = model_one_window();
            let before = m.tabs().count();
            let r = tab_cmd(
                &mut m,
                json!({"type":"attach","windowId":1,"groups":[
                    {"title":"ok","panes":[]},
                    {"title":"bad","color":bad,"panes":[]}
                ]}),
            );
            assert_eq!(r.status, 400, "{bad}: {:?}", r.body);
            let msg = r.body["error"].as_str().unwrap();
            assert!(msg.contains("groups[1]"), "{msg}");
            assert_eq!(
                m.tabs().count(),
                before,
                "no tab may be added on a bad colour"
            );
        }
    }

    #[test]
    fn rename_tab_sets_and_trims_title() {
        let mut m = model_one_window();
        let r = tab_cmd(
            &mut m,
            json!({"type":"renameTab","tabId":"t1","title":"  build  "}),
        );
        assert_eq!(r.status, 200);
        assert!(r.notify_state);
        assert_eq!(first_tab(&m).title, "build");
    }

    #[test]
    fn rename_tab_rejects_bad_input() {
        let mut m = model_one_window();
        let before = first_tab(&m).title.clone();
        let long = "x".repeat(81);
        for cmd in [
            json!({"type":"renameTab","tabId":"t1","title":""}),
            json!({"type":"renameTab","tabId":"t1","title":"   "}),
            json!({"type":"renameTab","tabId":"t1","title":long}),
            json!({"type":"renameTab","tabId":"t1","title":5}),
            json!({"type":"renameTab","tabId":7,"title":"x"}),
            json!({"type":"renameTab","title":"x"}),
        ] {
            assert_eq!(tab_cmd(&mut m, cmd).status, 400);
        }
        let r = tab_cmd(
            &mut m,
            json!({"type":"renameTab","tabId":"t1","title":"x".repeat(81)}),
        );
        assert_eq!(r.body["error"], json!("tab title too long (max 80 chars)"));
        assert_eq!(first_tab(&m).title, before);
        let ok80 = "x".repeat(80);
        let r = tab_cmd(
            &mut m,
            json!({"type":"renameTab","tabId":"t1","title":ok80}),
        );
        assert_eq!(r.status, 200);
    }

    #[test]
    fn recolor_tab_sets_normalises_and_clears() {
        let mut m = model_one_window();
        let r = tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"t1","color":"#ABC"}),
        );
        assert_eq!(r.status, 200);
        assert!(r.notify_state);
        assert_eq!(first_tab(&m).color.as_deref(), Some("#aabbcc"));
        tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"t1","color":"#A1B2C3"}),
        );
        assert_eq!(first_tab(&m).color.as_deref(), Some("#a1b2c3"));
        let r = tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"t1","color":null}),
        );
        assert_eq!(r.status, 200);
        assert_eq!(first_tab(&m).color, None);
    }

    #[test]
    fn recolor_tab_rejects_bad_colours_and_leaves_model_unchanged() {
        let mut m = model_one_window();
        tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"t1","color":"#112233"}),
        );
        for cmd in [
            json!({"type":"recolorTab","tabId":"t1","color":"red"}),
            json!({"type":"recolorTab","tabId":"t1","color":"#12345"}),
            json!({"type":"recolorTab","tabId":"t1","color":"#12345g"}),
            json!({"type":"recolorTab","tabId":"t1","color":42}),
            json!({"type":"recolorTab","tabId":"t1"}),
            json!({"type":"recolorTab","color":"#fff"}),
        ] {
            assert_eq!(tab_cmd(&mut m, cmd).status, 400);
        }
        let r = tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"t1","color":"red"}),
        );
        assert_eq!(
            r.body["error"],
            json!("invalid color \"red\" (expected #rrggbb, or null to clear)")
        );
        assert_eq!(first_tab(&m).color.as_deref(), Some("#112233"));
    }

    #[test]
    fn tab_commands_unknown_tab_is_404() {
        let mut m = model_one_window();
        let r = tab_cmd(
            &mut m,
            json!({"type":"renameTab","tabId":"nope","title":"x"}),
        );
        assert_eq!(r.status, 404);
        assert_eq!(r.body["error"], json!("no such tab: nope"));
        let r = tab_cmd(
            &mut m,
            json!({"type":"recolorTab","tabId":"nope","color":"#fff"}),
        );
        assert_eq!(r.status, 404);
    }

    #[test]
    fn tab_commands_respect_scope() {
        let mut m = model_one_window();
        let s = sessions();
        let scope = Scope {
            window_ids: Some(vec![999]),
            ..Default::default()
        };
        for cmd in [
            json!({"type":"renameTab","tabId":"t1","title":"x"}),
            json!({"type":"recolorTab","tabId":"t1","color":"#fff"}),
        ] {
            let r = handle_command(&mut m, &s, None, Some(&scope), &cmd, &speech());
            assert_eq!(r.status, 403);
        }
        assert_eq!(first_tab(&m).color, None);
    }

    #[tokio::test]
    async fn window_id_accepts_a_numeric_string() {
        // A hand-built request (e.g. `jq -r` stringifies windowId) must still resolve, not hit
        // the misleading "needs a paneId or windowId".
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "newPane", "windowId": "1", "pane": {} });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(
            r.status, 200,
            "string windowId should resolve, got {:?}",
            r.body
        );
        assert!(r.body["result"].is_string());
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
        let r = handle_command(&mut m, &s, None, Some(&scope), &cmd, &speech());
        assert_eq!(r.status, 403);
        assert_eq!(r.body["error"], json!("windowId 1 is out of scope"));
    }

    #[tokio::test]
    async fn set_talk_enables_and_disables_a_pane() {
        let mut m = model_one_window();
        let s = sessions();
        let open = json!({ "type": "newPane", "windowId": 1, "pane": {} });
        let id = handle_command(&mut m, &s, None, None, &open, &speech()).body["result"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(!m.pane(&id).unwrap().talk);

        let on = json!({ "type": "setTalk", "paneId": id, "enabled": true });
        let r = handle_command(&mut m, &s, None, None, &on, &speech());
        assert_eq!(r.status, 200);
        assert!(m.pane(&id).unwrap().talk);

        let off = json!({ "type": "setTalk", "paneId": id, "enabled": false });
        let r = handle_command(&mut m, &s, None, None, &off, &speech());
        assert_eq!(r.status, 200);
        assert!(!m.pane(&id).unwrap().talk);
    }

    #[test]
    fn set_talk_unknown_pane_is_500() {
        let mut m = model_one_window();
        let s = sessions();
        // An explicit windowId gets past the generic "needs a paneId or windowId" gate
        // (which can't resolve "ghost" to a window on its own) so this actually exercises
        // setTalk's own "no such pane" check inside `exec`.
        let cmd = json!({ "type": "setTalk", "windowId": 1, "paneId": "ghost", "enabled": true });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 500);
        assert_eq!(r.body["error"], json!("no such pane: ghost"));
    }

    #[test]
    fn set_talk_missing_enabled_is_500() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "setTalk", "windowId": 1, "paneId": "p" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 500);
        assert_eq!(r.body["error"], json!("missing boolean field: enabled"));
    }

    #[test]
    fn set_speech_muted_toggles_the_service_and_is_global_not_pane_scoped() {
        let mut m = model_one_window();
        let s = sessions();
        let svc = speech();
        // No paneId/windowId on this command at all — it must not hit the "needs a
        // paneId or windowId" 400 that every pane/window command does.
        let cmd = json!({ "type": "setSpeechMuted", "muted": true });
        let r = handle_command(&mut m, &s, None, None, &cmd, &svc);
        assert_eq!(r.status, 200);
        assert!(svc.status().muted);

        let cmd = json!({ "type": "setSpeechMuted", "muted": false });
        let r = handle_command(&mut m, &s, None, None, &cmd, &svc);
        assert_eq!(r.status, 200);
        assert!(!svc.status().muted);
    }

    #[test]
    fn stop_speech_is_global_and_leaves_mute_untouched() {
        let mut m = model_one_window();
        let s = sessions();
        let svc = speech();
        svc.set_muted(true);
        // Global like setSpeechMuted: no paneId/windowId, must not 400.
        let cmd = json!({ "type": "stopSpeech" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &svc);
        assert_eq!(r.status, 200);
        // A one-shot stop is not a mode switch — mute stays as it was.
        assert!(svc.status().muted);
    }

    #[test]
    fn set_speech_muted_needs_a_boolean() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "setSpeechMuted" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 400);
        assert_eq!(
            r.body["error"],
            json!("setSpeechMuted needs a boolean muted")
        );
    }

    #[test]
    fn set_speech_focused_only_toggles_the_service() {
        let mut m = model_one_window();
        let s = sessions();
        let svc = speech();
        let cmd = json!({ "type": "setSpeechFocusedOnly", "focusedOnly": true });
        let r = handle_command(&mut m, &s, None, None, &cmd, &svc);
        assert_eq!(r.status, 200);
        assert!(svc.status().focused_only);
    }

    #[test]
    fn set_speech_focused_only_needs_a_boolean() {
        let mut m = model_one_window();
        let s = sessions();
        let cmd = json!({ "type": "setSpeechFocusedOnly" });
        let r = handle_command(&mut m, &s, None, None, &cmd, &speech());
        assert_eq!(r.status, 400);
        assert_eq!(
            r.body["error"],
            json!("setSpeechFocusedOnly needs a boolean focusedOnly")
        );
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

    fn sv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resume_launch_table() {
        // (command, args, expected command, expected args)
        type Case = (
            &'static str,
            Option<Vec<String>>,
            &'static str,
            Option<Vec<String>>,
        );
        let cases: Vec<Case> = vec![
            // Flags in args survive; --resume added once.
            (
                "claude",
                Some(sv(&["--model", "sonnet", "--settings", "/path/s.json"])),
                "claude",
                Some(sv(&[
                    "--model",
                    "sonnet",
                    "--settings",
                    "/path/s.json",
                    "--resume",
                    "S",
                ])),
            ),
            // Every earlier session flag form is removed.
            (
                "claude",
                Some(sv(&["--model", "m", "--resume", "old"])),
                "claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            (
                "claude",
                Some(sv(&["--resume=old", "--model", "m"])),
                "claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            (
                "claude",
                Some(sv(&["-r", "old", "--model", "m"])),
                "claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            (
                "claude",
                Some(sv(&["--continue", "--model", "m", "-c"])),
                "claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            (
                "claude",
                Some(sv(&[
                    "--session-id",
                    "u-1",
                    "--session-id=u-2",
                    "--model",
                    "m",
                ])),
                "claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            // A bare --resume followed by another flag does not eat that flag.
            (
                "/usr/bin/claude",
                Some(sv(&["--resume", "--model", "m"])),
                "/usr/bin/claude",
                Some(sv(&["--model", "m", "--resume", "S"])),
            ),
            // Args with spaces stay single elements.
            (
                "claude",
                Some(sv(&["--settings", "/my dir/s.json"])),
                "claude",
                Some(sv(&["--settings", "/my dir/s.json", "--resume", "S"])),
            ),
            // Flags inside the command string.
            (
                "claude --model sonnet --resume old --settings /p/s.json",
                None,
                "claude --model sonnet --settings /p/s.json --resume S",
                None,
            ),
            (
                "/usr/bin/claude --model m -c",
                None,
                "/usr/bin/claude --model m --resume S",
                None,
            ),
            (
                "claude --settings '/my dir/s.json' --resume=old",
                None,
                "claude --settings '/my dir/s.json' --resume S",
                None,
            ),
            ("claude", None, "claude --resume S", None),
            ("claude", Some(vec![]), "claude --resume S", None),
        ];
        for (cmd, args, want_cmd, want_args) in cases {
            let got = resume_launch(Some(cmd), args.as_deref(), "S")
                .unwrap_or_else(|| panic!("no launch for {cmd:?} {args:?}"));
            assert_eq!(got.command, want_cmd, "command for {cmd:?} {args:?}");
            assert_eq!(got.args, want_args, "args for {cmd:?} {args:?}");
        }
    }

    #[test]
    fn resume_launch_drops_positional_prompt_from_argv() {
        let l = resume_launch(
            Some("claude"),
            Some(&sv(&[
                "--model",
                "m",
                "fix the build",
                "--add-dir",
                "/a",
                "/b",
                "--verbose",
            ])),
            "S",
        )
        .unwrap();
        assert_eq!(
            l.args,
            Some(sv(&[
                "--model",
                "m",
                "--add-dir",
                "/a",
                "/b",
                "--verbose",
                "--resume",
                "S"
            ]))
        );
        // `--` and what follows it are positional too.
        let l = resume_launch(
            Some("claude"),
            Some(&sv(&["--model", "m", "--", "-x", "go"])),
            "S",
        )
        .unwrap();
        assert_eq!(l.args, Some(sv(&["--model", "m", "--resume", "S"])));
    }

    #[test]
    fn resume_launch_drops_positional_prompt_from_command_string() {
        let l = resume_launch(
            Some("claude --model m   'fix the \"build\" now'  --verbose"),
            None,
            "S",
        )
        .unwrap();
        assert_eq!(l.command, "claude --model m  --verbose --resume S");
        let l = resume_launch(Some("claude 'first' --resume old"), None, "S").unwrap();
        assert_eq!(l.command, "claude --resume S");
        // Shell syntax after the program: nothing positional is touched.
        let l = resume_launch(Some("claude --model m | tee out.log"), None, "S").unwrap();
        assert_eq!(l.command, "claude --model m | tee out.log --resume S");
    }

    #[test]
    fn resume_launch_keeps_positional_when_a_flag_is_unknown() {
        let l = resume_launch(
            Some("claude"),
            Some(&sv(&["--future-flag", "--resume", "old", "hello"])),
            "S",
        )
        .unwrap();
        assert_eq!(
            l.args,
            Some(sv(&["--future-flag", "hello", "--resume", "S"]))
        );
        let l = resume_launch(Some("claude --future-flag hello -c"), None, "S").unwrap();
        assert_eq!(l.command, "claude --future-flag hello --resume S");
    }

    #[cfg(unix)]
    #[tokio::test]
    // The guard serialises the process-global queue path; the await is the output poll.
    #[allow(clippy::await_holding_lock)]
    async fn restart_resume_with_prompt_relaunches_without_positional_and_enqueues_once() {
        use std::os::unix::fs::PermissionsExt;
        let _g = crate::resume_queue::tests::lock();
        crate::resume_queue::tests::use_scratch_queue();
        // A stand-in `claude` that echoes the argv it was relaunched with.
        let dir = std::env::temp_dir().join(format!("hp-fake-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude");
        std::fs::write(&bin, "#!/bin/sh\necho \"argv:[$*]\"\nsleep 30\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut m = model_one_window();
        let s = sessions();
        let pane = PaneInfo {
            id: "p1".into(),
            session_uid: "u1".into(),
            label: "a".into(),
            subtitle: None,
            color: "#888888".into(),
            command: Some(bin.to_string_lossy().into_owned()),
            args: Some(sv(&["--model", "m", "original launch prompt"])),
            cwd: None,
            shell: None,
            status: crate::control::readmodel::PaneStatus::Running,
            exit_code: None,
            meta: None,
            talk: false,
            env: None,
        };
        assert!(m.insert_pane(1, pane.clone()));
        let target = ResumeTarget {
            session_id: "0198c4a2-1f2e-4d3c-8a5b-9e7f6c5d4b3a".into(),
            cwd: String::new(),
            config_dir: None,
        };
        respawn_resuming(&mut m, &s, None, &pane, &target, None, Some("new message")).unwrap();
        let uid = m.pane("p1").unwrap().session_uid.clone();
        let out = wait_for_output(&s, &uid, "argv:[").await;
        assert!(
            out.contains(&format!("argv:[--model m --resume {}]", target.session_id)),
            "relaunch argv: {out:?}"
        );
        assert!(!out.contains("original launch prompt"), "{out:?}");
        let queued = crate::resume_queue::take_for(&target.session_id);
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].text, "new message");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resume_launch_accepts_windows_claude_exe() {
        let l = resume_launch(
            Some(r"C:\tools\claude.exe"),
            Some(&sv(&["--model", "m", "--resume", "old"])),
            "S",
        )
        .unwrap();
        assert_eq!(l.args, Some(sv(&["--model", "m", "--resume", "S"])));
        let l = resume_launch(Some("claude.exe -c --model m"), None, "S").unwrap();
        assert_eq!(l.command, "claude.exe --model m --resume S");
    }

    #[test]
    fn resume_launch_leaves_non_claude_alone() {
        assert_eq!(resume_launch(Some("bash"), Some(&sv(&["-l"])), "S"), None);
        assert_eq!(resume_launch(Some("bash -l"), None, "S"), None);
        assert_eq!(resume_launch(Some("  "), None, "S"), None);
        assert_eq!(resume_launch(None, None, "S"), None);
        assert_eq!(resume_launch(Some("echo declaude"), None, "S"), None);
    }

    #[test]
    fn restart_resume_keeps_original_args_and_adds_resume_once() {
        let mut m = model_one_window();
        let pane = PaneInfo {
            id: "p1".into(),
            session_uid: "u1".into(),
            label: "a".into(),
            subtitle: None,
            color: "#888888".into(),
            command: Some("claude".into()),
            args: Some(sv(&[
                "--model",
                "sonnet",
                "--resume",
                "old",
                "--settings",
                "/p/s.json",
            ])),
            cwd: None,
            shell: None,
            status: crate::control::readmodel::PaneStatus::Running,
            exit_code: None,
            meta: None,
            talk: false,
            env: None,
        };
        assert!(m.insert_pane(1, pane.clone()));
        let l = resume_launch(pane.command.as_deref(), pane.args.as_deref(), "new").unwrap();
        let args = l.args.unwrap();
        assert_eq!(args.iter().filter(|a| *a == "--resume").count(), 1);
        assert_eq!(
            args,
            sv(&[
                "--model",
                "sonnet",
                "--settings",
                "/p/s.json",
                "--resume",
                "new"
            ])
        );
        assert_eq!(l.command, "claude");
    }

    // ---- recoverPane ----

    #[tokio::test]
    async fn recover_pane_inspect_reports_null_api_error_when_tail_is_healthy() {
        let mut m = model_one_window();
        let s = sessions();
        let open = json!({ "type": "newPane", "windowId": 1, "pane": { "command": "echo hi" } });
        let pane_id = handle_command(&mut m, &s, None, None, &open, &speech()).body["result"]
            .as_str()
            .unwrap()
            .to_string();

        let inspect = json!({ "type": "recoverPane", "paneId": pane_id, "action": "inspect" });
        let r = handle_command(&mut m, &s, None, None, &inspect, &speech());
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert_eq!(r.body["result"]["apiError"], Value::Null);
        assert_eq!(r.body["result"]["class"], Value::Null);
        assert!(r.body["result"]["activity"].is_string());
        assert!(r.body["result"]["session"].is_object());
    }

    #[tokio::test]
    // Spawns `printf …; exec cat` to plant the API-Error tail — POSIX shell only. The
    // classification it pins is platform-agnostic and covered by classification_table.
    #[cfg(unix)]
    async fn recover_pane_resume_refuses_unknown_class_without_force() {
        let mut m = model_one_window();
        let s = sessions();
        // Classifies as Unknown per the classification table (a 418 matches no known
        // transient/account-limit/poisoned pattern).
        let open = json!({
            "type": "newPane",
            "windowId": 1,
            "pane": { "command": "printf 'API Error: 418 I am a teapot\\r\\n'; exec cat" }
        });
        let pane_id = handle_command(&mut m, &s, None, None, &open, &speech()).body["result"]
            .as_str()
            .unwrap()
            .to_string();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resume = json!({ "type": "recoverPane", "paneId": pane_id, "action": "resume" });
        let r = handle_command(&mut m, &s, None, None, &resume, &speech());
        assert_eq!(r.status, 500);
        assert!(
            r.body["error"].as_str().unwrap().contains("unknown"),
            "{:?}",
            r.body
        );
    }

    // `recover_repair`'s own session resolution (marker file / same-cwd scan) reads real,
    // user-global directories (`~/.claude/projects`, per-account config dirs, the pane
    // marker dir) that other concurrent processes on this machine also touch — exercising
    // that live in a unit test is exactly the flakiness/pollution risk
    // `scripts/g3-recovery-demo.sh` is designed to absorb instead (it owns its own isolated
    // headless instance + a namespaced temp project). So this test drives the disk-mutation
    // half directly through `apply_repair_to_disk`, on a plain temp file dispatch doesn't
    // otherwise share with anything: byte-exact drop of line 9, a real backup file, and a
    // no-op idempotent second pass.
    #[test]
    fn apply_repair_to_disk_drops_the_orphaned_fixture_line_and_is_idempotent() {
        const FIXTURE: &str = include_str!("../../tests/fixtures/g2-poisoned-transcript.jsonl");
        let tmp = std::env::temp_dir().join(format!("hp-recover-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        let transcript_path = tmp.join("session.jsonl");
        std::fs::write(&transcript_path, FIXTURE).unwrap();

        let (dropped, backup) = apply_repair_to_disk(&transcript_path).unwrap();
        assert_eq!(dropped, vec![9]);
        let backup = backup.expect("a dropped-line repair must write a backup");
        assert!(
            Path::new(&backup).exists(),
            "backup file must exist: {backup}"
        );
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            FIXTURE,
            "backup must be the untouched original"
        );

        // Idempotent: repairing the now-healthy transcript a second time is a no-op —
        // nothing dropped, no new backup written.
        let (dropped2, backup2) = apply_repair_to_disk(&transcript_path).unwrap();
        assert!(dropped2.is_empty());
        assert!(backup2.is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
