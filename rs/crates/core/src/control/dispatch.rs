//! In-process command execution — replaces the Electron renderer round-trip + correlationId.
//! POST /command mutates the central `readmodel` directly and returns `{ok, result}`
//! SYNCHRONOUSLY (the set_meta echo race is now structurally impossible). Commands:
//! newPane (→ returns new paneId) / closePane / setLayout / renamePane / recolorPane / setMeta /
//! focusPane / openTab(attach) / restartPane / readScreen / recoverPane. PRESERVE the response
//! shapes + status mapping byte-for-byte: 500 on action error, 404 window-not-found, 400
//! missing-type/target, 403 scope error. `readScreen` serializes the central `alacritty_terminal`
//! Term via `session::screen` (`SessionManager::render_screen`).
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
    "command", "args", "cwd", "label", "subtitle", "color", "shell", "env", "meta", "project",
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

    // Resolve a target window: explicit windowId (number or numeric string), else the pane's window.
    let window_id = window_id_field(cmd).or_else(|| {
        cmd.get("paneId")
            .and_then(Value::as_str)
            .and_then(|p| model.coords_of(p).map(|c| c.window_id))
    });
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
                let opts = SpawnOptions {
                    uid: new_uid.clone(),
                    shell: pane.shell.clone(),
                    args: pane.args.clone(),
                    command: pane.command.clone(),
                    cwd: pane.cwd.clone(),
                    env: env_override,
                    cols: None,
                    rows: None,
                    pane_id: Some(pane_id.clone()),
                    integration: None,
                    control_file: control_file.map(str::to_string),
                };
                sessions.kill(&old_uid);
                sessions.create(opts).map_err(|e| e.to_string())?;
                model.respawn_pane(&pane_id, &new_uid);
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
    let mut env_override = env_field.and_then(Value::as_object).map(|o| {
        o.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect::<EnvMap>()
    });
    // Restore the account (CLAUDE_CONFIG_DIR) the conversation was saved/found under, so
    // `claude --resume` finds it in the right per-account transcript store. An explicit
    // `env` override wins (the account-rotation path deliberately moves the pane to a
    // DIFFERENT account with the same shared-store transcript), so only fill it in when the
    // caller didn't set it and the target recorded a valid dir.
    let resume_config_dir = target
        .config_dir
        .as_deref()
        .filter(|d| crate::claude_panes::valid_config_dir(d));
    if let Some(dir) = resume_config_dir {
        let already_set = env_override
            .as_ref()
            .is_some_and(|e| e.contains_key("CLAUDE_CONFIG_DIR"));
        if !already_set {
            env_override
                .get_or_insert_with(EnvMap::default)
                .insert("CLAUDE_CONFIG_DIR".to_string(), dir.to_string());
        }
    }
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
    let resume_launch = resume_command(pane.command.as_deref(), &target.session_id);
    let resume_cwd = crate::claude_panes::valid_resume_cwd(&target.cwd).then(|| target.cwd.clone());
    let opts = SpawnOptions {
        uid: new_uid.clone(),
        shell: pane.shell.clone(),
        args: pane.args.clone(),
        command: resume_launch.clone().or_else(|| pane.command.clone()),
        cwd: if resume_launch.is_some() {
            resume_cwd.clone().or_else(|| pane.cwd.clone())
        } else {
            pane.cwd.clone()
        },
        env: env_override,
        cols: None,
        rows: None,
        pane_id: Some(pane_id.to_string()),
        integration: None,
        control_file: control_file.map(str::to_string),
    };
    sessions.kill(&old_uid);
    sessions.create(opts).map_err(|e| e.to_string())?;
    model.respawn_pane(pane_id, &new_uid);

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
        talk: false,
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
    let project = crate::persistence::projects::resolve(&handle)
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

fn str_field(cmd: &Value, key: &str) -> Result<String, String> {
    cmd.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string field: {key}"))
}

/// Read `windowId` from a command, accepting either a JSON number OR a numeric string. Clients
/// that build the request by hand (e.g. `jq -r '.windows[0].windowId'` → the string `"0"`) would
/// otherwise fail the strict `as_i64` and hit the misleading "needs a paneId or windowId".
fn window_id_field(cmd: &Value) -> Option<i64> {
    cmd.get("windowId").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })
}

/// Rebuild a directly-launched claude command so a resumed pane keeps every flag it
/// was born with (`--mcp-config`, `--append-system-prompt-file`,
/// `--dangerously-skip-permissions`, `--model`) by appending `--resume <session_id>` to
/// its original launch. Returns `None` when `base` doesn't launch claude directly (a
/// shell-hosted pane — the caller keeps typing a bare `claude --resume` into its shell).
///
/// `session_id` is UUID-shaped (it comes from a SessionStart marker, gated on write), so
/// it needs no shell-quoting here. `respawn_pane` never bakes the resume flag back into
/// the stored command, so `base` is always the pristine original — but we still guard
/// against a pre-existing `--resume` to stay idempotent if that ever changes.
fn resume_command(base: Option<&str>, session_id: &str) -> Option<String> {
    let base = base?.trim();
    // Only rewrite a direct claude invocation; never append flags to a plain shell.
    let launches_claude = base
        .split_whitespace()
        .any(|tok| tok == "claude" || tok.rsplit('/').next() == Some("claude"));
    if !launches_claude || base.contains("--resume ") {
        return launches_claude.then(|| base.to_string());
    }
    Some(format!("{base} --resume {session_id}"))
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
        assert_eq!(r.body["error"], json!("command needs a paneId or windowId"));
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

    #[test]
    fn resume_command_appends_flags_to_direct_claude() {
        let base = "claude --dangerously-skip-permissions                     --append-system-prompt-file /x/SPEC.md --model m --mcp-config /x/mcp.json";
        let got = resume_command(Some(base), "abc-123").unwrap();
        // Every original flag survives, and the resume id is appended.
        assert!(got.starts_with(base));
        assert!(got.ends_with(" --resume abc-123"));
        assert!(got.contains("--mcp-config /x/mcp.json"));
        assert!(got.contains("--append-system-prompt-file /x/SPEC.md"));
        assert!(got.contains("--dangerously-skip-permissions"));
    }

    #[test]
    fn resume_command_handles_absolute_claude_path() {
        let got = resume_command(Some("/usr/bin/claude --model m"), "id9").unwrap();
        assert_eq!(got, "/usr/bin/claude --model m --resume id9");
    }

    #[test]
    fn resume_command_skips_non_claude_and_empty() {
        // Shell-hosted pane → None (caller keeps the typed-`claude --resume` path).
        assert_eq!(resume_command(Some("bash -l"), "id"), None);
        assert_eq!(resume_command(Some("  "), "id"), None);
        assert_eq!(resume_command(None, "id"), None);
        // A word merely containing "claude" must not trip the direct-launch check.
        assert_eq!(resume_command(Some("echo declaude"), "id"), None);
    }

    #[test]
    fn resume_command_is_idempotent_when_already_resuming() {
        let base = "claude --model m --resume old-id";
        // Never double-append; return the command unchanged.
        assert_eq!(resume_command(Some(base), "new-id").unwrap(), base);
    }

    // ---- recoverPane ----

    #[tokio::test]
    async fn recover_pane_inspect_reports_null_api_error_when_tail_is_healthy() {
        let mut m = model_one_window();
        let s = sessions();
        let open = json!({ "type": "newPane", "windowId": 1, "pane": { "command": "echo hi" } });
        let pane_id = handle_command(&mut m, &s, None, None, &open).body["result"]
            .as_str()
            .unwrap()
            .to_string();

        let inspect = json!({ "type": "recoverPane", "paneId": pane_id, "action": "inspect" });
        let r = handle_command(&mut m, &s, None, None, &inspect);
        assert_eq!(r.status, 200, "{:?}", r.body);
        assert_eq!(r.body["result"]["apiError"], Value::Null);
        assert_eq!(r.body["result"]["class"], Value::Null);
        assert!(r.body["result"]["activity"].is_string());
        assert!(r.body["result"]["session"].is_object());
    }

    #[tokio::test]
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
        let pane_id = handle_command(&mut m, &s, None, None, &open).body["result"]
            .as_str()
            .unwrap()
            .to_string();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let resume = json!({ "type": "recoverPane", "paneId": pane_id, "action": "resume" });
        let r = handle_command(&mut m, &s, None, None, &resume);
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
