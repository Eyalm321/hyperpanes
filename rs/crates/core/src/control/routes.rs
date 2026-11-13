//! All HTTP route handlers (axum) — the byte-compatible surface the MCP server depends on.
//! Ports the route table from `src/main/control-server.ts` EXACTLY:
//!   GET  /health                 (the ONLY unauthenticated route)
//!   GET  /state                  scope-filtered windows tree (readmodel)
//!   POST /tokens                 mint scoped token (tokens + scope::check_mintable)
//!   GET  /panes/{id}/output      mode=screen|raw, tail, strip, since, waitForIdle/settleMs/timeoutMs
//!                                (control::output cores); cursor ALWAYS present
//!   POST /panes/{id}/input       allowInput gate (403); data|keys (control::input); submit; lock 423
//!   GET|POST /panes/{id}/messages durable inbox (control::inbox)
//!   POST|DELETE /panes/{id}/lock  advisory lock (control::lock)
//!   POST /command                dispatch
//!   GET  /events                 WS upgrade (token via header or ?token=)
//!   + 401 unauthorized / 404 {error,path} / 405 method-not-allowed fallbacks
//!
//! Bearer via `Authorization: Bearer` or `?token=` (WS only). Every body shape matches the TS
//! source (omit-when-unset; ordered structs where field order is observable).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{json, Value};

use crate::ansi_strip::strip_ansi;
use crate::control::dispatch;
use crate::control::events::ControlEvent;
use crate::control::input::{keys_to_bytes, submit_newlines, KeysResult, SUBMIT_DELAY_MS};
use crate::control::nudge;
use crate::control::output::{
    detect_awaiting_input, next_poll_delay, slice_since, wait_decision, WaitVerdict,
    DEFAULT_SETTLE_MS, DEFAULT_WAIT_TIMEOUT_MS,
};
use crate::control::readmodel::{Activity, PaneStatus, WindowOut};
use crate::control::scope::{check_mintable, coerce_scope, pane_in_scope, queue_in_scope, Scope};
use crate::control::server::{
    events_url, notify_state, now_ms, MoveTarget, PaneGroupRequest, Shared,
};
use crate::control::tokens::TokenInfo;
use crate::control::work::{
    Counts, EnqueueOpts, LeaseOutcome, ListFilter, NackOpts, QueueSummary, Task, TaskState,
};
use crate::persistence::projects;

/// Build the full router with the shared state baked in.
pub fn router(shared: Arc<Shared>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/state", get(state))
        .route("/tokens", post(tokens))
        .route(
            "/devices",
            get(devices_list).post(devices_mint).delete(devices_revoke),
        )
        .route("/command", post(command))
        .route("/projects", get(projects_list).post(projects_add))
        .route(
            "/projects/{id}",
            patch(projects_patch).delete(projects_delete),
        )
        .route("/panes/{id}/output", get(output))
        .route("/panes/{id}/input", post(input))
        .route(
            "/panes/{id}/messages",
            get(messages_get).post(messages_post),
        )
        .route("/panes/{id}/lock", post(lock_post).delete(lock_delete))
        // ---- work queue (worker-pool phase-2/3) ----
        .route("/queues", get(queues_list))
        .route("/queues/{queue}/tasks", post(task_enqueue).get(tasks_list))
        .route("/queues/{queue}/claim", post(task_claim))
        .route("/queues/{queue}/purge", post(queue_purge))
        .route("/tasks/{id}", get(task_get))
        .route("/tasks/{id}/ack", post(task_ack))
        .route("/tasks/{id}/nack", post(task_nack))
        .route("/tasks/{id}/extend", post(task_extend))
        .route("/fs/read", get(fs_read))
        .route("/events", get(events_ws))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(shared)
}

// ---- response helpers ---------------------------------------------------------------------

fn jstatus(code: u16, body: Value) -> Response {
    (
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(body),
    )
        .into_response()
}

fn ok_json<T: Serialize>(body: T) -> Response {
    Json(body).into_response()
}

// ---- auth ---------------------------------------------------------------------------------

fn bearer_header(headers: &HeaderMap) -> Option<String> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::to_string)
}

/// Resolve the presented bearer (HTTP header only — `?token=` is WS-only, per TS). 401 on failure.
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
fn authorize(shared: &Arc<Shared>, headers: &HeaderMap) -> Result<TokenInfo, Response> {
    let token = bearer_header(headers);
    shared
        .tokens
        .lock()
        .unwrap()
        .resolve(token.as_deref(), now_ms())
        .ok_or_else(|| jstatus(401, json!({ "error": "unauthorized" })))
}

/// A resolved, in-scope pane: its session uid plus the canonical pane id (the caller may have
/// addressed it by an alias — see `ReadModel::resolve_pane_id`). 404 (no such pane) / 403 (out
/// of scope).
struct FoundPane {
    uid: String,
    pane_id: String,
}

// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
fn find_pane_scoped(
    shared: &Arc<Shared>,
    scope: Option<&Scope>,
    pane_id: &str,
) -> Result<FoundPane, Response> {
    let m = shared.model.lock().unwrap();
    let canonical = match m.resolve_pane_id(pane_id) {
        None => {
            return Err(jstatus(
                404,
                json!({ "error": "no such pane", "paneId": pane_id }),
            ))
        }
        Some(c) => c,
    };
    match m.coords_of(&canonical) {
        None => Err(jstatus(
            404,
            json!({ "error": "no such pane", "paneId": pane_id }),
        )),
        Some(coords) => {
            if !pane_in_scope(scope, &coords) {
                return Err(jstatus(
                    403,
                    json!({ "error": "pane out of scope", "paneId": pane_id }),
                ));
            }
            let uid = m
                .pane(&canonical)
                .map(|p| p.session_uid.clone())
                .unwrap_or_default();
            Ok(FoundPane {
                uid,
                pane_id: canonical,
            })
        }
    }
}

// ---- /health ------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthOut {
    ok: bool,
    app: &'static str,
    /// Only for a caller presenting a VALID bearer token (#131): `/health` is unauthenticated
    /// and may be reachable over a remote endpoint, so it must not leak the app pid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    version: String,
    allow_input: bool,
    /// Additive, stable: `"durable"` (this instance owns the on-disk work queue),
    /// `"refused"` (another live instance owns `work.db`; this one runs an in-memory queue,
    /// no boot recovery, no reaper on the DB) or `"memory"` (never attached / open failed).
    /// Deliberately no paths or pids — `/health` is unauthenticated.
    work_queue: &'static str,
    /// Additive: `durable` | `memory` | `refused` — which pane inbox this instance runs.
    inbox: &'static str,
    /// Additive: optional sanitized build label; omitted when unset (public app).
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
}

async fn health(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    // An absent or invalid token is NOT a 401 here: /health stays reachable without auth, it
    // just omits the pid.
    let authed = shared
        .tokens
        .lock()
        .unwrap()
        .resolve(bearer_header(&headers).as_deref(), now_ms())
        .is_some();
    ok_json(HealthOut {
        ok: true,
        app: "hyperpanes",
        pid: authed.then_some(shared.pid),
        version: shared.version.clone(),
        allow_input: shared.allow_input(),
        work_queue: shared.work_queue_mode().as_str(),
        inbox: shared.inbox_mode().as_str(),
        build: shared.build(),
    })
}

// ---- /state -------------------------------------------------------------------------------

/// `/state`'s top-level, additive `speech` field: the per-pane "talk" engine's status,
/// reported even before the engine has been lazily spawned.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SpeechOut {
    muted: bool,
    focused_only: bool,
    backend: String,
    speaking_pane: Option<String>,
}

#[derive(Serialize)]
struct AppOut {
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    build: Option<String>,
}

#[derive(Serialize)]
struct StateWithSpeechOut {
    app: AppOut,
    windows: Vec<WindowOut>,
    speech: SpeechOut,
}

async fn state(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let out = {
        let m = shared.model.lock().unwrap();
        m.state_for_scope_with_dims(info.scope.as_ref(), &|p| shared.compute_activity(p), &|p| {
            shared.sessions.dims(&p.session_uid)
        })
    };
    let status = shared.speech.status();
    ok_json(StateWithSpeechOut {
        app: AppOut {
            version: shared.version.clone(),
            build: shared.build(),
        },
        windows: out.windows,
        speech: SpeechOut {
            muted: status.muted,
            focused_only: status.focused_only,
            backend: status.backend,
            speaking_pane: status.speaking_pane,
        },
    })
}

// ---- /tokens ------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MintOut {
    ok: bool,
    token: String,
    scope: Scope,
    expires_at: Option<i64>,
    port: Option<u16>,
    events: Value,
}

async fn tokens(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    let scope_val = parsed
        .as_ref()
        .and_then(|b| b.get("scope"))
        .cloned()
        .unwrap_or(Value::Null);
    let requested = match coerce_scope(&scope_val) {
        Some(s) => s,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { scope: { windowIds?|tabIds?|paneIds? }, ttlMs? }" }),
            )
        }
    };
    // No-escalation: the requested scope must sit within the minter's authority + name real ids.
    let problem = {
        let m = shared.model.lock().unwrap();
        check_mintable(info.scope.as_ref(), &requested, &*m)
    };
    if let Some(p) = problem {
        return jstatus(403, json!({ "error": p }));
    }
    let ttl = parsed
        .as_ref()
        .and_then(|b| b.get("ttlMs"))
        .and_then(Value::as_i64)
        .filter(|&t| t > 0);
    let (token, expires_at) = shared
        .tokens
        .lock()
        .unwrap()
        .mint(requested.clone(), ttl, now_ms());
    let port = shared.port();
    let (port_field, events) = if port != 0 {
        (
            Some(port),
            Value::String(events_url(&shared.advertised_host(), port, &token)),
        )
    } else {
        (None, Value::Null)
    };
    ok_json(MintOut {
        ok: true,
        token,
        scope: requested,
        expires_at,
        port: port_field,
        events,
    })
}

// ---- /devices (paired mobile clients: persisted, revocable, full-authority) ----------------
//
// A device token is unscoped (a phone is a full remote head, and a scope is a whitelist of
// TODAY's panes — it would go blind the moment a new pane opens), persisted to
// `device-tokens.json` so pairing survives a host restart, and individually revocable by label.
// `hyperpanes pair` POSTs here so the MASTER token never leaves the machine; `devices`/`revoke`
// list and drop. All three are MASTER-only — a scoped agent token must not mint device creds.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceMintOut {
    ok: bool,
    label: String,
    token: String,
    expires_at: Option<i64>,
    port: Option<u16>,
    events: Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceListItem {
    label: String,
    expires_at: Option<i64>,
}

/// Rewrite `device-tokens.json` from the live store (best-effort — the in-memory table is
/// authoritative for this process; the file only has to be right for the next start). Kept beside
/// the active `control.json` so tests/embeds with a custom control file stay self-consistent.
fn persist_devices(shared: &Arc<Shared>) {
    let records: Vec<_> = shared
        .tokens
        .lock()
        .unwrap()
        .list_devices()
        .into_iter()
        .map(
            |(token, label, expires_at)| crate::persistence::device_tokens::DeviceRecord {
                label,
                token,
                expires_at,
            },
        )
        .collect();
    let path = shared.control_file.with_file_name("device-tokens.json");
    if let Err(e) = crate::persistence::device_tokens::save_to(&path, &records) {
        eprintln!("[control] persist device-tokens.json failed: {e}");
    }
}

/// POST /devices — mint a device token. Body: `{ label?, ttlMs? }` (label defaults to `device`,
/// ttl omitted = never expires). Returns the token + reachable port/events for the QR.
async fn devices_mint(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let parsed: Option<Value> = serde_json::from_slice(&body).ok();
    let label = parsed
        .as_ref()
        .and_then(|b| b.get("label"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("device")
        .to_string();
    let ttl = parsed
        .as_ref()
        .and_then(|b| b.get("ttlMs"))
        .and_then(Value::as_i64)
        .filter(|&t| t > 0);
    let (token, expires_at) =
        shared
            .tokens
            .lock()
            .unwrap()
            .mint_device(label.clone(), ttl, now_ms());
    persist_devices(&shared);
    let port = shared.port();
    let (port_field, events) = if port != 0 {
        (
            Some(port),
            Value::String(events_url(&shared.advertised_host(), port, &token)),
        )
    } else {
        (None, Value::Null)
    };
    ok_json(DeviceMintOut {
        ok: true,
        label,
        token,
        expires_at,
        port: port_field,
        events,
    })
}

/// GET /devices — list paired devices (label + expiry only; tokens are never echoed back).
async fn devices_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let mut items: Vec<DeviceListItem> = shared
        .tokens
        .lock()
        .unwrap()
        .list_devices()
        .into_iter()
        .map(|(_token, label, expires_at)| DeviceListItem { label, expires_at })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    ok_json(json!({ "ok": true, "devices": items }))
}

/// DELETE /devices?label=<label> — revoke every device carrying `label` (a query param so labels
/// may hold spaces or slashes). `revoked` = how many were dropped.
async fn devices_revoke(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let Some(label) = q.get("label").filter(|l| !l.is_empty()) else {
        return jstatus(400, json!({ "error": "missing label" }));
    };
    let removed = shared.tokens.lock().unwrap().revoke_device(label);
    if removed > 0 {
        persist_devices(&shared);
    }
    ok_json(json!({ "ok": true, "revoked": removed }))
}

// ---- /panes/{id}/output -------------------------------------------------------------------

async fn output(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let found = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let uid = found.uid;

    if q.get("waitForIdle").map(|v| v == "1").unwrap_or(false) {
        let settle = pos_num(q.get("settleMs")).unwrap_or(DEFAULT_SETTLE_MS);
        let timeout = pos_num(q.get("timeoutMs")).unwrap_or(DEFAULT_WAIT_TIMEOUT_MS);
        let since = non_neg_num(q.get("since"));
        let (settled, timed_out) = wait_for_quiet(&shared, &uid, settle, timeout, since).await;
        let mut body = build_output_body(&shared, &id, &uid, &q);
        if let Value::Object(map) = &mut body {
            map.insert("waited".into(), json!(true));
            map.insert("settled".into(), json!(settled));
            map.insert("timedOut".into(), json!(timed_out));
        }
        return ok_json(body);
    }
    ok_json(build_output_body(&shared, &id, &uid, &q))
}

// ---- /fs/read (mobile clickable paths) ------------------------------------------------------

/// Read a text file off the host disk for a remote viewer (the mobile app's tap-a-path
/// feature). MASTER token only: scoped tokens are for sandboxed agents and must not
/// grow a filesystem read primitive. Query: `path` (absolute or `~/...`), optional
/// `maxBytes` (default 256 KiB, capped 2 MiB). Non-UTF-8 content 415s rather than
/// mangling bytes; oversized files return the head with `truncated: true`.
async fn fs_read(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if info.scope.is_some() {
        return jstatus(403, json!({ "error": "master token required" }));
    }
    let Some(raw_path) = q.get("path").filter(|p| !p.is_empty()) else {
        return jstatus(400, json!({ "error": "missing path" }));
    };
    // `~` expansion matches the desktop clickable-paths behaviour.
    let path = if let Some(rest) = raw_path.strip_prefix("~/") {
        match std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => return jstatus(400, json!({ "error": "cannot expand ~ (no HOME)" })),
        }
    } else {
        std::path::PathBuf::from(raw_path)
    };
    if !path.is_absolute() {
        return jstatus(400, json!({ "error": "path must be absolute (or ~/...)" }));
    }
    let max_bytes = q
        .get("maxBytes")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(256 * 1024)
        .min(2 * 1024 * 1024);
    let meta = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(_) => return jstatus(404, json!({ "error": "not found" })),
    };
    if !meta.is_file() {
        return jstatus(400, json!({ "error": "not a regular file" }));
    }
    let size = meta.len();
    let bytes = {
        use std::io::Read;
        let mut f = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(_) => return jstatus(403, json!({ "error": "cannot open" })),
        };
        let mut buf = vec![0u8; max_bytes.min(size as usize)];
        let mut read = 0;
        while read < buf.len() {
            match f.read(&mut buf[read..]) {
                Ok(0) => break,
                Ok(n) => read += n,
                Err(_) => return jstatus(500, json!({ "error": "read failed" })),
            }
        }
        buf.truncate(read);
        buf
    };
    let truncated = (bytes.len() as u64) < size;
    // Lop a torn trailing UTF-8 sequence off a truncated read before validating.
    let content = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) if truncated => {
            let valid = e.utf8_error().valid_up_to();
            let mut b = e.into_bytes();
            b.truncate(valid);
            match String::from_utf8(b) {
                Ok(s) => s,
                Err(_) => return jstatus(415, json!({ "error": "not a text file" })),
            }
        }
        Err(_) => return jstatus(415, json!({ "error": "not a text file" })),
    };
    ok_json(json!({
        "path": path.display().to_string(),
        "size": size,
        "truncated": truncated,
        "content": content,
    }))
}

fn pane_status_str(shared: &Arc<Shared>, pane_id: &str) -> &'static str {
    shared
        .model
        .lock()
        .unwrap()
        .pane(pane_id)
        .map(|p| p.status.as_str())
        .unwrap_or(PaneStatus::Exited.as_str())
}

fn build_output_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    if q.get("mode").map(|m| m == "screen").unwrap_or(false) {
        build_screen_body(shared, pane_id, uid, q)
    } else {
        read_output_body(shared, pane_id, uid, q)
    }
}

fn read_output_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    // Atomic pair: a torn replay/cursor read would drop or duplicate bytes when a
    // remote client splices the live `output` frame stream onto this snapshot.
    let (raw, total) = shared
        .sessions
        .replay_with_cursor(uid)
        .map(|(r, c)| (r, c as i64))
        .unwrap_or_default();
    let since = non_neg_num(q.get("since"));
    let mut text = raw.clone();
    let mut cursor = total;
    let mut truncated = false;
    if let Some(s) = since {
        let sl = slice_since(&raw, total, s);
        text = sl.output;
        cursor = sl.cursor;
        truncated = sl.truncated;
    }
    let strip = q.get("strip").map(|v| v == "1").unwrap_or(false);
    if strip {
        text = strip_ansi(&text);
    }
    if let Some(tail) = tail_num(q.get("tail")) {
        text = tail_lines(&text, tail);
    }
    let status = pane_status_str(shared, pane_id);
    let mut body = json!({
        "paneId": pane_id,
        "status": status,
        "stripped": strip,
        "output": text,
        "cursor": cursor,
    });
    if let Some(s) = since {
        if let Value::Object(map) = &mut body {
            map.insert("since".into(), json!(s));
            map.insert("truncated".into(), json!(truncated));
        }
    }
    body
}

fn build_screen_body(
    shared: &Arc<Shared>,
    pane_id: &str,
    uid: &str,
    q: &HashMap<String, String>,
) -> Value {
    let cursor = shared
        .sessions
        .output_bytes(uid)
        .map(|b| b as i64)
        .unwrap_or(0);
    // The content cursor BEFORE rendering keys the cached classification (see
    // `server::classify_screen`), so the activity read-out reuses this read's verdict.
    let content_cursor = shared
        .sessions
        .activity_signal(uid)
        .map(|sig| sig.content_cursor);
    match shared.sessions.render_screen_read(uid) {
        Some(read) => {
            let full = read.text;
            // Prompt detection runs on the FULL screen so a clipping `tail` can't hide a blocked
            // prompt above the tail window.
            let awaiting = detect_awaiting_input(&full);
            if let Some(c) = content_cursor {
                shared.sessions.set_awaiting(uid, c, awaiting);
            }
            let mut text = full;
            if let Some(tail) = tail_num(q.get("tail")) {
                text = tail_lines(&text, tail);
            }
            json!({
                "paneId": pane_id,
                "status": pane_status_str(shared, pane_id),
                "mode": "screen",
                "output": text,
                "cursor": cursor,
                "awaitingInput": awaiting,
                // The Claude-Code-style prompt box, or null: `kind` says whether its text was
                // typed or is the agent's own faint suggestion (never someone's input).
                "inputLine": read.input_line,
            })
        }
        None => {
            // No screen (pane gone / wedged): fall back to the raw replay, flagged.
            let mut body = read_output_body(shared, pane_id, uid, q);
            if let Value::Object(map) = &mut body {
                map.insert("mode".into(), json!("raw"));
                map.insert("screenUnavailable".into(), json!(true));
            }
            body
        }
    }
}

/// Block until the pane has been output-quiet for `settle_ms` (or `timeout_ms` elapses), driving
/// an adaptive poll over the live tracking maps (pure `wait_decision`/`next_poll_delay`).
async fn wait_for_quiet(
    shared: &Arc<Shared>,
    uid: &str,
    settle_ms: i64,
    timeout_ms: i64,
    since: Option<i64>,
) -> (bool, bool) {
    let start = now_ms();
    loop {
        let now = now_ms();
        let last = shared.sessions.last_output_at(uid).map(|t| t as i64);
        let total = shared
            .sessions
            .output_bytes(uid)
            .map(|b| b as i64)
            .unwrap_or(0);
        match wait_decision(last, total, since, now, start, settle_ms, timeout_ms) {
            WaitVerdict::Settled => return (true, false),
            WaitVerdict::Timeout => return (false, true),
            WaitVerdict::Wait => {
                let d = next_poll_delay(last, now, start, settle_ms, timeout_ms).max(1) as u64;
                tokio::time::sleep(Duration::from_millis(d)).await;
            }
        }
    }
}

// ---- /panes/{id}/input --------------------------------------------------------------------

async fn input(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let found = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f,
        Err(e) => return e,
    };
    if !shared.allow_input() {
        return jstatus(403, json!({ "error": "input not allowed" }));
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let has_data = b.get("data").and_then(Value::as_str).is_some();
    let has_keys = b.get("keys").map(Value::is_array).unwrap_or(false);
    if !has_data && !has_keys {
        return jstatus(
            400,
            json!({ "error": "expected { data: string } or { keys: string[] }" }),
        );
    }
    // Advisory write lock (H): if someone else holds it, refuse.
    let owner = b.get("owner").and_then(Value::as_str);
    let holder = shared.locks.lock().unwrap().holder(&id, now_ms());
    if let Some(h) = &holder {
        if Some(h.as_str()) != owner {
            return jstatus(423, json!({ "error": "pane locked", "owner": h }));
        }
    }
    let uid = found.uid;

    if has_keys {
        let keys: Vec<String> = b
            .get("keys")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        return match keys_to_bytes(&refs) {
            KeysResult::Ok { bytes } => {
                shared.sessions.write(&uid, &bytes);
                jstatus(200, json!({ "ok": true, "keys": keys }))
            }
            KeysResult::Err { unknown } => jstatus(
                400,
                json!({ "error": "unknown key(s)", "unknown": unknown }),
            ),
        };
    }

    let data = b.get("data").and_then(Value::as_str).unwrap_or("");
    let platform = if cfg!(windows) { "win32" } else { "linux" };
    shared
        .sessions
        .write(&uid, &submit_newlines(data, platform));
    // submit (A1): a bare CR as a SEPARATE pty write a beat later, so bracketed-paste TUIs read it
    // as Enter, not pasted content.
    if b.get("submit").and_then(Value::as_bool) == Some(true) {
        let delay = b
            .get("submitDelayMs")
            .and_then(Value::as_i64)
            .filter(|&d| d >= 0)
            .map(|d| d as u64)
            .unwrap_or(SUBMIT_DELAY_MS);
        let sessions = Arc::clone(&shared.sessions);
        let uid2 = uid.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            sessions.write(&uid2, "\r");
        });
    }
    jstatus(200, json!({ "ok": true }))
}

// ---- /panes/{id}/messages -----------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MessagesOut {
    pane_id: String,
    messages: Vec<crate::control::inbox::PaneMessage>,
    dropped: usize,
    latest_seq: u64,
    /// This inbox's id. Stable across restarts for a durable inbox (it lives with the DB);
    /// an in-memory inbox mints a new one each app start.
    epoch: u64,
    /// The `after` cursor was from an earlier lifetime: `messages` is the whole backlog.
    reset: bool,
}

async fn messages_get(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    // Canonical id: an inbox is keyed by the read-model's pane id, so an alias-addressed read
    // (`pane-<uuid>` vs bare `<uuid>`) must land on the same queue the writer posted to.
    let id = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f.pane_id,
        Err(e) => return e,
    };
    let after = non_neg_num(q.get("after"))
        .filter(|&a| a > 0)
        .map(|a| a as u64)
        .unwrap_or(0);
    let out = {
        let inbox = shared.inbox.lock().unwrap();
        let read = inbox.read_cursor(&id, after);
        MessagesOut {
            messages: read.messages,
            dropped: inbox.dropped_count(&id),
            latest_seq: inbox.latest_seq(&id),
            epoch: inbox.epoch(),
            reset: read.reset,
            pane_id: id,
        }
    };
    // The reader now holds every message up to the newest one returned (or up to its own cursor
    // when nothing newer was there): a pending inbox nudge for those would wake the pane for mail
    // it already has (t39). The inbox lock is released first; the ledger lock is never nested.
    let cursor = if out.reset { 0 } else { after };
    let through = out.messages.last().map_or(cursor, |m| m.seq.max(cursor));
    shared
        .nudges
        .lock()
        .unwrap()
        .mark_read(&out.pane_id, through);
    ok_json(out)
}

async fn messages_post(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    // Canonical id: the inbox is keyed by the read-model's pane id, so an alias-addressed post
    // (`pane-<uuid>` vs bare `<uuid>`) lands on the queue the target actually reads.
    let id = match find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        Ok(f) => f.pane_id,
        Err(e) => return e,
    };
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let from = b
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let msg_body = match b.get("body").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { from?, body: string, nudge?: boolean }" }),
            )
        }
    };
    // `nudge:false` stores the message without typing the inbox nudge into the pane: for a
    // sender that already wakes the receiver its own way, so one send costs the receiver one
    // turn, not two. Anything but an explicit `false` keeps the default (nudge).
    let nudge = !matches!(b.get("nudge"), Some(Value::Bool(false)));
    let (msg, epoch) = {
        let mut inbox = shared.inbox.lock().unwrap();
        let msg = inbox.post(&id, &from, &msg_body, now_ms());
        (msg, inbox.epoch())
    };
    // Wake an opted-in agent pane (goals org): the durable read stays the source of truth, but a
    // TUI agent parked at its prompt would never perform that read on its own.
    if nudge {
        arm_inbox_nudge(&shared, &id, msg.seq);
    }
    // Nudge live, in-scope clients (the durable read remains the source of truth).
    let coords = shared.model.lock().unwrap().coords_of(&id);
    shared.events.broadcast_for_pane(
        coords.as_ref(),
        &ControlEvent::Message {
            to: id,
            from,
            seq: msg.seq,
            body: msg_body,
        },
    );
    jstatus(
        200,
        json!({ "ok": true, "seq": msg.seq, "epoch": epoch, "nudged": nudge }),
    )
}

/// Whether inbox nudges are enabled at all — `HYPERPANES_MSG_NUDGE=0` (or `false`/`off`) in the
/// app's environment turns the whole mechanism off, leaving the bus pull-only as before.
fn nudges_enabled() -> bool {
    !matches!(
        std::env::var("HYPERPANES_MSG_NUDGE").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Arm (and, if needed, spawn) the waiter that types a one-line "you have mail" into `pane_id`
/// once it goes quiet. See `control::nudge` for the policy: role opt-in, coalescing,
/// never-mid-turn, rate limit.
fn arm_inbox_nudge(shared: &Arc<Shared>, pane_id: &str, seq: u64) {
    if !nudges_enabled() {
        return;
    }
    let (wants, uid, status) = {
        let m = shared.model.lock().unwrap();
        match m.pane(pane_id) {
            None => return,
            Some(p) => (
                nudge::wants_nudge(p.meta.as_ref()),
                p.session_uid.clone(),
                p.status,
            ),
        }
    };
    if !wants || status == PaneStatus::Exited {
        return;
    }
    if !shared.nudges.lock().unwrap().arm(pane_id, seq, now_ms()) {
        return; // a waiter is already in flight for this pane; it will pick the batch up
    }
    let shared = Arc::clone(shared);
    let pane_id = pane_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(nudge::SETTLE_MS)).await;
        loop {
            // Busy = mid-turn (a running command, or output within the idle threshold): typing
            // now would land in a live prompt, so wait for the pane to come to rest, however long
            // that takes (the ledger never gives up on a pending batch). A pane that closed or
            // exited meanwhile can read nothing: forget its counters and retire the waiter.
            let watched = {
                let m = shared.model.lock().unwrap();
                match m.pane(&pane_id) {
                    Some(p) if p.status != PaneStatus::Exited => Some((
                        shared.compute_activity(p) == Activity::Busy,
                        nudge::nudge_form(p.meta.as_ref()),
                    )),
                    _ => None,
                }
            };
            let Some((busy, form)) = watched else {
                shared.nudges.lock().unwrap().drop_pane(&pane_id);
                return;
            };
            // Never type into a decision prompt (a permission dialog, a y/n question): the
            // nudge's Enter would answer it. Such a pane reads idle once its output is only
            // cosmetic (a blinking indicator), so check the screen itself before typing, with
            // the same predicate the queued-prompt and goal deliveries use.
            let busy = busy || {
                let sessions = Arc::clone(&shared.sessions);
                let owned = uid.clone();
                // A failed check (the blocking task panicked) must not type: treat it as busy.
                tokio::task::spawn_blocking(move || {
                    crate::control::server::screen_blocks_typing(&sessions, &owned)
                })
                .await
                .unwrap_or(true)
            };
            let step = shared
                .nudges
                .lock()
                .unwrap()
                .poll(&pane_id, busy, now_ms(), form);
            match step {
                nudge::Step::Stop => return,
                nudge::Step::Wait => {
                    tokio::time::sleep(Duration::from_millis(nudge::POLL_MS)).await;
                }
                nudge::Step::Send(text) => {
                    // Same cadence the goal/resume delivery uses: text, gap, CR, insurance CR —
                    // a bracketed-paste TUI reads text+CR in one read as a paste otherwise.
                    let sessions = Arc::clone(&shared.sessions);
                    sessions.write(&uid, &text);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    sessions.write(&uid, "\r");
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    sessions.write(&uid, "\r");
                    return;
                }
            }
        }
    });
}

// ---- /panes/{id}/lock ---------------------------------------------------------------------

async fn lock_post(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let owner = match b.get("owner").and_then(Value::as_str) {
        Some(o) => o,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { owner: string, ttlMs? }" }),
            )
        }
    };
    let ttl = b
        .get("ttlMs")
        .and_then(Value::as_i64)
        .filter(|&t| t > 0)
        .unwrap_or(30_000);
    let r = shared
        .locks
        .lock()
        .unwrap()
        .acquire(&id, owner, now_ms(), ttl);
    if r.ok {
        jstatus(
            200,
            json!({ "ok": true, "owner": r.owner, "expiresAt": r.expires_at }),
        )
    } else {
        jstatus(
            423,
            json!({ "ok": false, "owner": r.owner, "expiresAt": r.expires_at, "error": "held" }),
        )
    }
}

async fn lock_delete(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let owner = match b.get("owner").and_then(Value::as_str) {
        Some(o) => o,
        None => return jstatus(400, json!({ "error": "expected { owner: string }" })),
    };
    let ok = shared.locks.lock().unwrap().release(&id, owner, now_ms());
    if ok {
        jstatus(200, json!({ "ok": true }))
    } else {
        jstatus(423, json!({ "ok": false, "error": "not the lock holder" }))
    }
}

// ---- work queue: /queues + /tasks ---------------------------------------------------------
//
// Same spine as every existing route: `authorize → 401`; a queue-scope gate → 403
// (`queue_in_scope`, master = any); camelCase JSON via `ok_json`/`jstatus`; bodies parsed
// with `serde_json::from_slice(..).unwrap_or(Value::Null)` and validated with the same
// `expected { … }` 400 style. One NEW status — `409 Conflict` for a stale lease (a wrong
// `fencingToken`, the optimistic-concurrency failure) — distinct from the lock module's
// `423 Locked`. The serialized `Task` IS the canonical wire format (camelCase, epoch-ms,
// flattened lease fields claimedBy/fencingToken/visibilityDeadline), so claim/get return it
// verbatim and the controller presents the task's own `fencingToken` back on ack/nack/extend.

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnqueueOut {
    ok: bool,
    id: String,
    seq: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimOut {
    ok: bool,
    tasks: Vec<Task>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TasksListOut {
    queue: String,
    tasks: Vec<Task>,
    counts: Counts,
    latest_seq: u64,
}

#[derive(Serialize)]
struct QueuesOut {
    queues: Vec<QueueSummary>,
}

/// Map a lease-guarded outcome (`ack`/`nack`/`extend`) to the byte-exact response: the
/// `ok_body` closure builds the 200 body from the resulting task; `Conflict → 409` (stale
/// `fencingToken`); `NotFound → 404`.
fn lease_response(
    outcome: LeaseOutcome,
    id: &str,
    ok_body: impl FnOnce(&Task) -> Value,
) -> Response {
    match outcome {
        LeaseOutcome::Ok(task) => jstatus(200, ok_body(&task)),
        LeaseOutcome::Conflict => jstatus(409, json!({ "error": "stale lease", "taskId": id })),
        LeaseOutcome::NotFound => jstatus(404, json!({ "error": "no such task", "taskId": id })),
    }
}

/// The shared queue-scope gate: 403 unless `queue_in_scope` (master passes).
fn queue_scope_gate(scope: Option<&Scope>, queue: &str) -> Option<Response> {
    if queue_in_scope(scope, queue) {
        None
    } else {
        Some(jstatus(
            403,
            json!({ "error": "queue out of scope", "queue": queue }),
        ))
    }
}

// POST /queues/{queue}/tasks — enqueue
async fn task_enqueue(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let payload = match b.get("payload").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { payload: string, kind?, title?, priority?, maxAttempts?, visibilityTimeoutMs?, delayMs?|availableAt?, dedupeKey?, goalId?, dependsOn?: string[] }" }),
            )
        }
    };
    let mut opts = EnqueueOpts::default();
    if let Some(k) = b.get("kind").and_then(Value::as_str) {
        opts.kind = k.to_string();
    }
    if let Some(t) = b.get("title").and_then(Value::as_str) {
        opts.title = t.to_string();
    }
    if let Some(p) = b.get("priority").and_then(Value::as_i64) {
        opts.priority = p;
    }
    if let Some(m) = b
        .get("maxAttempts")
        .and_then(Value::as_i64)
        .filter(|&m| m > 0)
    {
        opts.max_attempts = m as u32;
    }
    if let Some(v) = b
        .get("visibilityTimeoutMs")
        .and_then(Value::as_i64)
        .filter(|&v| v > 0)
    {
        opts.visibility_timeout_ms = v;
    }
    let now = now_ms();
    // `availableAt` (absolute ms) wins; else `delayMs` schedules `now + delay`.
    if let Some(a) = b.get("availableAt").and_then(Value::as_i64) {
        opts.available_at = Some(a);
    } else if let Some(d) = b.get("delayMs").and_then(Value::as_i64).filter(|&d| d > 0) {
        opts.available_at = Some(now + d);
    }
    if let Some(k) = b.get("dedupeKey").and_then(Value::as_str) {
        opts.dedupe_key = Some(k.to_string());
    }
    // Goals system: `goalId` tags the task to a goal; `dependsOn` (array of task ids) gates the
    // task's claim until every listed task is `done` (see WorkQueue::claim).
    if let Some(g) = b.get("goalId").and_then(Value::as_str) {
        opts.goal_id = Some(g.to_string());
    }
    if let Some(deps) = b.get("dependsOn").and_then(Value::as_array) {
        opts.depends_on = Some(
            deps.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
        );
    }
    let task = shared
        .work
        .lock()
        .unwrap()
        .enqueue(&queue, &payload, opts, now);
    ok_json(EnqueueOut {
        ok: true,
        id: task.id,
        seq: task.seq,
    })
}

// GET /queues/{queue}/tasks — list/inspect (cursor `after`, optional `state`, `limit`)
async fn tasks_list(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let after = non_neg_num(q.get("after"))
        .filter(|&a| a > 0)
        .map(|a| a as u64)
        .unwrap_or(0);
    let limit = pos_num(q.get("limit")).unwrap_or(100).min(1000) as usize;
    // Only a state string that round-trips is a real filter (an unknown value is ignored,
    // never silently coerced to `queued`).
    let state = q.get("state").and_then(|s| {
        let st = TaskState::from_wire(s);
        (st.as_str() == s).then_some(st)
    });
    let wq = shared.work.lock().unwrap();
    let tasks = wq.list(&queue, ListFilter { state }, after, limit);
    let counts = wq.counts(&queue);
    let latest_seq = tasks.iter().map(|t| t.seq).max().unwrap_or(after);
    ok_json(TasksListOut {
        queue,
        tasks,
        counts,
        latest_seq,
    })
}

// POST /queues/{queue}/claim — claim the next task(s) (competing consumers)
async fn task_claim(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let worker = match b
        .get("worker")
        .and_then(Value::as_str)
        .filter(|w| !w.is_empty())
    {
        Some(w) => w.to_string(),
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { worker: string, leaseMs?, count?, holderPid? }" }),
            )
        }
    };
    // `leaseMs <= 0` ⇒ the queue falls back to the task's own visibility timeout.
    let lease_ms = b
        .get("leaseMs")
        .and_then(Value::as_i64)
        .filter(|&v| v > 0)
        .unwrap_or(0);
    let count = b
        .get("count")
        .and_then(Value::as_i64)
        .filter(|&c| c > 0)
        .map(|c| c.min(100) as usize)
        .unwrap_or(1);
    // `holderPid`: the claiming process (the `hyperpanes worker` runner sends its own pid).
    // Recorded only if it is a live LOCAL process; boot recovery then keeps this claim, fence
    // and all, for as long as that process lives (a GUI relaunch with the session daemon on).
    // Absent / not a live local pid ⇒ no holder ⇒ the claim is requeued on the next boot.
    let holder = b
        .get("holderPid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .and_then(crate::control::holder::identify);
    let now = now_ms();
    let mut tasks = Vec::new();
    {
        let mut wq = shared.work.lock().unwrap();
        for _ in 0..count {
            match wq.claim_held(&queue, &worker, holder.as_ref(), lease_ms, now) {
                Some(c) => tasks.push(c.task),
                None => break, // queue drained — an EMPTY claim is 200 {tasks:[]}, never 204
            }
        }
    }
    ok_json(ClaimOut { ok: true, tasks })
}

// POST /queues/{queue}/purge — drop terminal tasks (retention/cleanup)
async fn queue_purge(
    State(shared): State<Arc<Shared>>,
    Path(queue): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let now = now_ms();
    // `olderThan` = absolute ms cutoff; `olderThanMs` = an age (cutoff = now - age);
    // neither ⇒ purge every currently-terminal task.
    let older_than = if let Some(abs) = b.get("olderThan").and_then(Value::as_i64) {
        abs
    } else if let Some(age) = b.get("olderThanMs").and_then(Value::as_i64) {
        now - age
    } else {
        now
    };
    let removed = shared.work.lock().unwrap().purge(&queue, older_than);
    jstatus(200, json!({ "ok": true, "removed": removed }))
}

// GET /queues — every queue + its depth (scope-filtered)
async fn queues_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let all = shared.work.lock().unwrap().queues();
    let queues = all
        .into_iter()
        .filter(|qs| queue_in_scope(info.scope.as_ref(), &qs.queue))
        .collect();
    ok_json(QueuesOut { queues })
}

// GET /tasks/{id} — fetch one task (scope resolved from its queue)
async fn task_get(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let task = shared.work.lock().unwrap().get(&id);
    match task {
        None => jstatus(404, json!({ "error": "no such task", "taskId": id })),
        Some(task) => {
            if let Some(e) = queue_scope_gate(info.scope.as_ref(), &task.queue) {
                return e;
            }
            ok_json(task)
        }
    }
}

/// Resolve `fencingToken` + the task's queue-scope for a lease op, or the error response.
/// Returns `(fencing_token, body)` on success.
// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
fn lease_op_preamble(
    shared: &Arc<Shared>,
    info: &TokenInfo,
    id: &str,
    body: &Bytes,
) -> Result<(u64, Value), Response> {
    let b: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let token = match b.get("fencingToken").and_then(Value::as_u64) {
        Some(t) => t,
        None => {
            return Err(jstatus(
                400,
                json!({ "error": "expected { fencingToken: number, … }" }),
            ))
        }
    };
    // Resolve the task → its queue for the scope gate; 404 if it's gone.
    let queue = match shared.work.lock().unwrap().get(id) {
        Some(t) => t.queue,
        None => {
            return Err(jstatus(
                404,
                json!({ "error": "no such task", "taskId": id }),
            ))
        }
    };
    if let Some(e) = queue_scope_gate(info.scope.as_ref(), &queue) {
        return Err(e);
    }
    Ok((token, b))
}

// POST /tasks/{id}/ack — complete a claimed task (lease-guarded)
async fn task_ack(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let result = b.get("result").and_then(Value::as_str);
    let outcome = shared
        .work
        .lock()
        .unwrap()
        .ack(&id, token, result, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "state": t.state.as_str() }),
    )
}

// POST /tasks/{id}/nack — fail/retry a claimed task (lease-guarded)
async fn task_nack(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let opts = NackOpts {
        // default requeue=true (retry with backoff); requeue=false ⇒ give up now (Failed)
        requeue: b.get("requeue").and_then(Value::as_bool).unwrap_or(true),
        error: b.get("error").and_then(Value::as_str).map(str::to_string),
        delay_ms: b.get("delayMs").and_then(Value::as_i64),
    };
    let outcome = shared.work.lock().unwrap().nack(&id, token, opts, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "state": t.state.as_str() }),
    )
}

// POST /tasks/{id}/extend — heartbeat: extend the lease (lease-guarded)
async fn task_extend(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let (token, b) = match lease_op_preamble(&shared, &info, &id, &body) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let extra_ms = match b.get("extraMs").and_then(Value::as_i64).filter(|&x| x > 0) {
        Some(x) => x,
        None => {
            return jstatus(
                400,
                json!({ "error": "expected { fencingToken: number, extraMs: number }" }),
            )
        }
    };
    let outcome = shared
        .work
        .lock()
        .unwrap()
        .extend(&id, token, extra_ms, now_ms());
    lease_response(
        outcome,
        &id,
        |t| json!({ "ok": true, "visibilityDeadline": t.visibility_deadline }),
    )
}

// ---- /command -----------------------------------------------------------------------------

async fn command(State(shared): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let cmd: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    // `restartApp` is handled here, not in dispatch: it needs `Shared` (the GUI host polls
    // the flag each tick and performs the teardown on the UI thread). An optional
    // `sessionId` + `prompt` pair pre-queues a speak-first message for a conversation the
    // restore will resurrect. Root scope only — this takes the whole app down.
    if cmd.get("type").and_then(Value::as_str) == Some("restartApp") {
        if info.scope.is_some() {
            return jstatus(
                403,
                serde_json::json!({"error": "restartApp needs a root token"}),
            );
        }
        let scope = match cmd.get("scope").and_then(Value::as_str).unwrap_or("gui") {
            "gui" => 1u8,
            "full" => 2u8,
            other => {
                return jstatus(
                    400,
                    serde_json::json!({"error": format!("unknown scope: {other}")}),
                )
            }
        };
        if let (Some(sid), Some(text)) = (
            cmd.get("sessionId").and_then(Value::as_str),
            cmd.get("prompt").and_then(Value::as_str),
        ) {
            if let Err(e) = crate::resume_queue::enqueue(sid, text) {
                return jstatus(400, serde_json::json!({"error": e}));
            }
        }
        shared
            .restart_app
            .store(scope, std::sync::atomic::Ordering::SeqCst);
        return jstatus(
            200,
            serde_json::json!({"ok": true, "scope": if scope == 1 {"gui"} else {"full"}}),
        );
    }
    // `connectRemote` / `remoteScreen` are local (client-side) GUI commands: the work happens on
    // the GUI thread via queues on `Shared`. Root token only; the token/url is never echoed.
    if let Some(t @ ("connectRemote" | "remoteScreen")) = cmd.get("type").and_then(Value::as_str) {
        if info.scope.is_some() {
            return jstatus(
                403,
                serde_json::json!({"error": format!("{t} needs a root token")}),
            );
        }
        return remote_command(&shared, t, &cmd);
    }
    if let Some(t @ ("groupPanes" | "ungroupPanes")) = cmd.get("type").and_then(Value::as_str) {
        return pane_group_command(&shared, &info, t, &cmd);
    }
    if cmd.get("type").and_then(Value::as_str) == Some("movePane") {
        return move_pane_command(&shared, &info, &cmd);
    }
    // `newPane` with `pane.projectTab: true` lands in its project's tab (reused or created)
    // instead of the caller's. Validate up front; see `place_project_tab` for how it lands.
    let project_tab = match new_pane_project_tab(&cmd) {
        Ok(v) => v,
        Err(msg) => return jstatus(400, json!({ "error": msg })),
    };
    let mut cmd = cmd;
    let control_file = shared.control_file.to_str().map(str::to_string);
    let result = {
        // ONE model lock across placement → dispatch → queued move, so the GUI sync tick never
        // sees the new pane without its move (it would host it for a frame in the active tab).
        let mut m = shared.model.lock().unwrap();
        let deferred = project_tab
            .as_deref()
            .and_then(|h| place_project_tab(&m, info.scope.as_ref(), &mut cmd, h));
        let result = dispatch::handle_command(
            &mut m,
            &shared.sessions,
            control_file.as_deref(),
            info.scope.as_ref(),
            &cmd,
            &shared.speech,
        );
        if let (Some(handle), 200) = (deferred, result.status) {
            queue_project_tab_move(&shared, &m, &handle, &result.body);
        }
        result
    };
    // Phase-5: keep supervisor policies in lockstep with pane meta (setMeta flips
    // hp.supervise; newPane carries meta; closePane removes a pane). Cheap + idempotent.
    shared.reconcile_policies();
    if result.notify_state {
        notify_state(&shared);
    }
    // A project-opening newPane bumped the registry's recency off-thread → tell the GUI host.
    if result.projects_dirty {
        shared.mark_projects_dirty();
    }
    jstatus(result.status, result.body)
}

/// `groupPanes {paneIds,title?,project?}` → validate against the read model, queue a GUI-side
/// group, answer `{tabId}`; `ungroupPanes {tabId}` → queue an ungroup. The GUI host executes
/// both (`Shared::take_pane_group_requests`); a headless server just leaves them queued.
fn pane_group_command(shared: &Arc<Shared>, info: &TokenInfo, kind: &str, cmd: &Value) -> Response {
    use crate::control::scope::tab_in_scope;
    use crate::control::server::PaneGroupRequest;
    let bad = |msg: String| jstatus(400, json!({ "error": msg }));
    let m = shared.model.lock().unwrap();
    if kind == "ungroupPanes" {
        let Some(tab_id) = cmd.get("tabId").and_then(Value::as_str) else {
            return bad("ungroupPanes needs a tabId".into());
        };
        let Some(win) = m.tab_window(tab_id) else {
            return bad(format!("no such tab: {tab_id}"));
        };
        if !tab_in_scope(info.scope.as_ref(), tab_id, win) {
            return jstatus(
                403,
                json!({ "error": format!("tabId {tab_id} is out of scope") }),
            );
        }
        let is_group = m
            .tabs()
            .any(|(_, t)| t.id == tab_id && t.pane_group.is_some());
        if !is_group {
            return bad(format!("tab {tab_id} is not a pane group"));
        }
        drop(m);
        shared.queue_pane_group_request(PaneGroupRequest::Ungroup {
            tab_id: tab_id.to_string(),
        });
        return jstatus(200, json!({ "ok": true }));
    }
    // groupPanes
    let Some(raw) = cmd.get("paneIds").and_then(Value::as_array) else {
        return bad("groupPanes needs a paneIds array".into());
    };
    let mut pane_ids: Vec<String> = Vec::new();
    let mut window: Option<i64> = None;
    for v in raw {
        let Some(id) = v.as_str() else {
            return bad("paneIds must be strings".into());
        };
        // Remote panes are not in the local model, so they are "no such pane" here.
        let Some(c) = m.coords_of(id) else {
            return bad(format!("no such pane: {id}"));
        };
        if !pane_in_scope(info.scope.as_ref(), &c) {
            return jstatus(
                403,
                json!({ "error": format!("paneId {id} is out of scope") }),
            );
        }
        match window {
            None => window = Some(c.window_id),
            Some(w) if w != c.window_id => {
                return bad("groupPanes panes must all be in one window".into())
            }
            _ => {}
        }
        if pane_ids.iter().any(|p| p == id) {
            return bad(format!("duplicate pane in paneIds: {id}"));
        }
        pane_ids.push(id.to_string());
    }
    let Some(window_id) = window.filter(|_| pane_ids.len() >= 2) else {
        return bad("groupPanes needs at least 2 distinct panes".into());
    };
    drop(m);
    let project = match cmd.get("project") {
        None | Some(Value::Null) => None,
        Some(Value::String(h)) => match projects::resolve_checked(h) {
            Ok(Some(p)) => Some(p.id),
            Ok(None) => return bad(format!("unknown project: {h}")),
            Err(e) => return bad(e),
        },
        Some(_) => return bad("project must be a string".into()),
    };
    let title = cmd
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty());
    let tab_id = uuid::Uuid::new_v4().to_string();
    shared.queue_pane_group_request(PaneGroupRequest::Group {
        tab_id: tab_id.clone(),
        window_id,
        pane_ids,
        title,
        project,
    });
    jstatus(
        200,
        json!({ "ok": true, "tabId": tab_id, "result": { "tabId": tab_id } }),
    )
}

/// Where `movePane` (or a `newPane` with `projectTab`) sends a pane, resolved against the read
/// model: the target window and the queued [`MoveTarget`]. `(status, message)` on a bad target.
fn resolve_move_target(
    m: &crate::control::readmodel::ReadModel,
    scope: Option<&Scope>,
    cmd: &Value,
    src_window: i64,
) -> Result<(i64, MoveTarget), (u16, String)> {
    use crate::control::scope::{tab_in_scope, window_in_scope};
    let bad = |msg: String| Err((400, msg));
    let has = |k: &str| cmd.get(k).is_some_and(|v| !v.is_null());
    let targets = ["tabId", "newTab", "project"]
        .iter()
        .filter(|k| has(k))
        .count();
    if targets != 1 {
        return bad("movePane needs exactly one of tabId, newTab, project".into());
    }
    if let Some(tab_id) = cmd.get("tabId").filter(|v| !v.is_null()) {
        let Some(tab_id) = tab_id.as_str() else {
            return bad("tabId must be a string".into());
        };
        let Some(win) = m.tab_window(tab_id) else {
            return bad(format!("no such tab: {tab_id}"));
        };
        if !tab_in_scope(scope, tab_id, win) {
            return Err((403, format!("tabId {tab_id} is out of scope")));
        }
        return Ok((
            win,
            MoveTarget::Tab {
                tab_id: tab_id.to_string(),
            },
        ));
    }
    // newTab / project: in `windowId` when given, else the pane's own window.
    let win = match cmd.get("windowId").filter(|v| !v.is_null()) {
        None => src_window,
        Some(v) => match v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        {
            Some(w) => w,
            None => return bad("windowId must be a number".into()),
        },
    };
    if !m.tabs().any(|(w, _)| w == win) {
        return bad(format!("no such window: {win}"));
    }
    if !window_in_scope(scope, win) {
        return Err((403, format!("windowId {win} is out of scope")));
    }
    if let Some(nt) = cmd.get("newTab").filter(|v| !v.is_null()) {
        if !nt.is_object() {
            return bad("newTab must be an object {title?, color?}".into());
        }
        let title = nt.get("title").and_then(Value::as_str).map(str::to_string);
        let color = match nt.get("color") {
            None | Some(Value::Null) => None,
            Some(Value::String(c)) if is_hex_color(c) => Some(c.clone()),
            Some(_) => return bad("newTab.color must be \"#rrggbb\" or \"#rgb\"".into()),
        };
        return Ok((
            win,
            MoveTarget::NewTab {
                tab_id: uuid::Uuid::new_v4().to_string(),
                title,
                color,
            },
        ));
    }
    let Some(handle) = cmd.get("project").and_then(Value::as_str) else {
        return bad("project must be a string".into());
    };
    let p = match projects::resolve_checked(handle) {
        Ok(Some(p)) => p,
        Ok(None) => return bad(format!("unknown project: {handle}")),
        Err(e) => return bad(e),
    };
    // Reuse the window's existing tab for the project when the model already shows one.
    if let Some(tab_id) = existing_project_tab(m, win, &p.id) {
        if !tab_in_scope(scope, &tab_id, win) {
            return Err((403, format!("tabId {tab_id} is out of scope")));
        }
        return Ok((win, MoveTarget::Tab { tab_id }));
    }
    Ok((
        win,
        MoveTarget::Project {
            tab_id: uuid::Uuid::new_v4().to_string(),
            project_id: p.id,
            title: p.name,
            color: Some(p.color).filter(|c| is_hex_color(c)),
        },
    ))
}

/// `#rgb` / `#rrggbb`.
fn is_hex_color(c: &str) -> bool {
    let Some(h) = c.strip_prefix('#') else {
        return false;
    };
    matches!(h.len(), 3 | 6) && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// The tab id a queued move lands in (what the API answers).
fn move_target_tab_id(t: &MoveTarget) -> &str {
    match t {
        MoveTarget::Tab { tab_id }
        | MoveTarget::NewTab { tab_id, .. }
        | MoveTarget::Project { tab_id, .. } => tab_id,
    }
}

/// `movePane {paneId, tabId | newTab:{title?,color?} | project, windowId?}` → validate against
/// the read model, queue a GUI-side move, answer `{tabId}`. Only the GUI can re-host a pane in
/// another tab, so (like `groupPanes`) a headless server leaves the request queued, unapplied.
fn move_pane_command(shared: &Arc<Shared>, info: &TokenInfo, cmd: &Value) -> Response {
    let bad = |status: u16, msg: String| jstatus(status, json!({ "error": msg }));
    let mut m = shared.model.lock().unwrap();
    let Some(pane_id) = cmd.get("paneId").and_then(Value::as_str) else {
        return bad(400, "movePane needs a paneId".into());
    };
    // Remote panes are not in the local model, so they are "no such pane" here.
    let Some(c) = m.coords_of(pane_id) else {
        return bad(400, format!("no such pane: {pane_id}"));
    };
    if !pane_in_scope(info.scope.as_ref(), &c) {
        return bad(403, format!("paneId {pane_id} is out of scope"));
    }
    let (window_id, to) = match resolve_move_target(&m, info.scope.as_ref(), cmd, c.window_id) {
        Ok(v) => v,
        Err((status, msg)) => return bad(status, msg),
    };
    let tab_id = move_target_tab_id(&to).to_string();
    if tab_id == c.tab_id {
        // Already there: nothing to move.
        return jstatus(
            200,
            json!({ "ok": true, "tabId": tab_id, "result": { "tabId": tab_id } }),
        );
    }
    // The window's only pane can't leave: that would strand an empty window.
    let in_window = m
        .tabs()
        .filter(|(w, _)| *w == c.window_id)
        .map(|(_, t)| t.panes.len())
        .sum::<usize>();
    if in_window <= 1 {
        return bad(
            409,
            format!("pane {pane_id} is the only pane in its window; open another first"),
        );
    }
    // A new tab (`newTab`, or a project tab the window doesn't have) is created in the read
    // model right here, under the same model lock as the queued move: the id we answer with is
    // addressable at once (an immediate `movePane {tabId}` / `newPane {tabId}` to it works), and
    // the GUI sync tick, which holds this lock across apply → republish, sees the tab and its
    // move together. The GUI makes the tab under this same id; until it publishes it, the model
    // keeps the tab as pending ([`ReadModel::insert_pending_tab`]).
    let created = pending_move_tab(&to).is_some_and(|tab| m.insert_pending_tab(window_id, tab));
    shared.queue_pane_group_request(PaneGroupRequest::Move {
        pane_id: pane_id.to_string(),
        window_id,
        to,
    });
    drop(m);
    if created {
        notify_state(shared);
    }
    jstatus(
        200,
        json!({ "ok": true, "tabId": tab_id, "result": { "tabId": tab_id } }),
    )
}

/// The read-model tab a queued move creates, mirroring what the GUI will make for it
/// (`apply_move_request`): a `newTab` carries its manual colour, a project tab its pane-group
/// marker. `None` for a move into an existing tab.
fn pending_move_tab(to: &MoveTarget) -> Option<crate::control::readmodel::TabInfo> {
    use crate::control::readmodel::{PaneGroupInfo, TabInfo};
    let (id, title, color, pane_group) = match to {
        MoveTarget::Tab { .. } => return None,
        MoveTarget::NewTab {
            tab_id,
            title,
            color,
        } => (
            tab_id,
            title.clone().unwrap_or_default(),
            color
                .as_deref()
                .and_then(crate::control::dispatch::normalize_hex_color),
            None,
        ),
        MoveTarget::Project {
            tab_id,
            project_id,
            title,
            color,
        } => (
            tab_id,
            title.clone(),
            None,
            Some(PaneGroupInfo {
                project: Some(project_id.clone()),
                color: color
                    .as_deref()
                    .and_then(crate::control::dispatch::normalize_hex_color),
            }),
        ),
    };
    Some(TabInfo {
        id: id.clone(),
        title,
        color,
        layout: "auto".into(),
        panes: Vec::new(),
        pane_group,
    })
}

/// `newPane`'s opt-in `pane.projectTab`: `Ok(Some(project handle))` when set (it needs
/// `pane.project`), `Ok(None)` when absent/false, `Err` on a malformed value.
fn new_pane_project_tab(cmd: &Value) -> Result<Option<String>, String> {
    if cmd.get("type").and_then(Value::as_str) != Some("newPane") {
        return Ok(None);
    }
    match cmd.pointer("/pane/projectTab") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(None),
        Some(Value::Bool(true)) if cmd.get("tabId").is_some_and(|v| !v.is_null()) => {
            Err("newPane takes tabId or pane.projectTab, not both".into())
        }
        Some(Value::Bool(true)) => match cmd.pointer("/pane/project").and_then(Value::as_str) {
            Some(h) => Ok(Some(h.to_string())),
            None => Err("pane.projectTab needs pane.project".into()),
        },
        Some(_) => Err("pane.projectTab must be a boolean".into()),
    }
}

/// Where a `projectTab` `newPane` goes. When the window already has the project's tab (and the
/// caller may act on it), the pane is spawned straight into it: `tabId` is set on `cmd` and
/// `None` is returned, so nothing moves. Otherwise the project tab must be created by the GUI,
/// so the handle comes back for [`queue_project_tab_move`] to run once the pane exists.
fn place_project_tab(
    m: &crate::control::readmodel::ReadModel,
    scope: Option<&Scope>,
    cmd: &mut Value,
    handle: &str,
) -> Option<String> {
    let deferred = Some(handle.to_string());
    let win = match cmd.get("windowId") {
        None | Some(Value::Null) => m.first_window_id(),
        Some(v) => v
            .as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok())),
    };
    let Some(win) = win else { return deferred };
    let Some(p) = projects::resolve(handle) else {
        return deferred;
    };
    match project_tab_target(m, scope, win, &p.id) {
        Some(tab_id) => {
            let obj = cmd.as_object_mut()?;
            obj.insert("tabId".into(), Value::String(tab_id));
            None
        }
        None => deferred,
    }
}

/// The window's existing tab for project `project_id`, if the caller may spawn into it.
fn project_tab_target(
    m: &crate::control::readmodel::ReadModel,
    scope: Option<&Scope>,
    win: i64,
    project_id: &str,
) -> Option<String> {
    existing_project_tab(m, win, project_id)
        .filter(|t| crate::control::scope::tab_in_scope(scope, t, win))
}

/// The id of window `win`'s tab grouped under project `project_id`, as the read model shows it.
fn existing_project_tab(
    m: &crate::control::readmodel::ReadModel,
    win: i64,
    project_id: &str,
) -> Option<String> {
    m.tabs()
        .find(|(w, t)| {
            *w == win
                && t.pane_group
                    .as_ref()
                    .is_some_and(|g| g.project.as_deref() == Some(project_id))
        })
        .map(|(_, t)| t.id.clone())
}

/// After a successful `projectTab` `newPane` whose project tab does not exist yet, queue the
/// move of the new pane into it. The caller still holds the model lock from dispatch, so the
/// GUI adopts and moves the pane in the same tick. Best effort: on any miss the pane simply
/// stays where `newPane` put it.
fn queue_project_tab_move(
    shared: &Arc<Shared>,
    m: &crate::control::readmodel::ReadModel,
    handle: &str,
    body: &Value,
) {
    let Some(pane_id) = body.get("result").and_then(Value::as_str) else {
        return;
    };
    let Some(c) = m.coords_of(pane_id) else {
        return;
    };
    let cmd = json!({ "project": handle });
    let Ok((window_id, to)) = resolve_move_target(m, None, &cmd, c.window_id) else {
        return;
    };
    if move_target_tab_id(&to) == c.tab_id {
        return;
    }
    shared.queue_pane_group_request(PaneGroupRequest::Move {
        pane_id: pane_id.to_string(),
        window_id,
        to,
    });
}

/// `connectRemote {url, label?, remember?}` → queue a GUI-side attach, answer `{hostId}`;
/// `remoteScreen` → the GUI's published snapshot. Errors never include the url (it holds a token).
fn remote_command(shared: &Arc<Shared>, kind: &str, cmd: &Value) -> Response {
    if kind == "remoteScreen" {
        return jstatus(
            200,
            serde_json::json!({"ok": true, "result": shared.remote_screens()}),
        );
    }
    let Some(url) = cmd.get("url").and_then(Value::as_str) else {
        return jstatus(
            400,
            serde_json::json!({"error": "connectRemote needs a url"}),
        );
    };
    let target = match crate::remote::RemoteTarget::parse(url) {
        Ok(t) => t,
        Err(e) => return jstatus(400, serde_json::json!({"error": format!("bad url: {e}")})),
    };
    let host_id = crate::remote::next_host_id();
    shared.queue_remote_connect(crate::remote::ConnectRequest {
        host_id,
        target,
        label: cmd
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|l| !l.trim().is_empty()),
        remember: cmd
            .get("remember")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    });
    jstatus(
        200,
        serde_json::json!({"ok": true, "result": {"hostId": host_id}}),
    )
}

// ---- /projects ----------------------------------------------------------------------------
// The project registry (`projects.json`): the directories the app remembers, shared by the
// GUI sidebar rail. These are global, not pane-scoped — any authorized token may use them. The
// `core::persistence::projects` layer owns the file; a write marks the GUI host's dirty flag so
// the rail refreshes live (`ControlHost::sync`).

async fn projects_list(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    ok_json(json!({ "projects": projects::list_projects() }))
}

async fn projects_add(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let dir = match b.get("dir").and_then(Value::as_str) {
        Some(d) if !d.trim().is_empty() => d.trim(),
        _ => return jstatus(400, json!({ "error": "expected { dir: string }" })),
    };
    // Mirror the GUI Add-Project dialog: the path must exist and be a directory (a git repo is
    // NOT required — `add_project_explicit` happily tracks any folder).
    if !std::path::Path::new(dir).is_dir() {
        return jstatus(
            400,
            json!({ "error": "path doesn't exist or isn't a directory", "dir": dir }),
        );
    }
    let (project, added) = projects::add_project_explicit(dir);
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true, "added": added, "project": project }))
}

async fn projects_patch(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let name = b
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let color = b
        .get("color")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if name.is_none() && color.is_none() {
        return jstatus(
            400,
            json!({ "error": "expected { name?: string, color?: string }" }),
        );
    }
    if let Some(name) = name {
        projects::rename_project(&id, name);
    }
    if let Some(color) = color {
        projects::set_project_color(&id, color);
    }
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true }))
}

async fn projects_delete(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authorize(&shared, &headers) {
        return e;
    }
    projects::remove_project(&id);
    shared.mark_projects_dirty();
    ok_json(json!({ "ok": true }))
}

// ---- /events (WebSocket) ------------------------------------------------------------------

async fn events_ws(
    State(shared): State<Arc<Shared>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    // Token from the `?token=` query (WS clients can't set Authorization reliably) or a Bearer header.
    let token = q.get("token").cloned().or_else(|| bearer_header(&headers));
    let info = shared
        .tokens
        .lock()
        .unwrap()
        .resolve(token.as_deref(), now_ms());
    let Some(info) = info else {
        return (StatusCode::UNAUTHORIZED, "").into_response();
    };
    let scope = info.scope;
    let shared2 = Arc::clone(&shared);
    ws.on_upgrade(move |socket| handle_ws(shared2, socket, scope))
}

async fn handle_ws(shared: Arc<Shared>, mut socket: WebSocket, scope: Option<Scope>) {
    let (id, mut rx) = shared.events.add_client(scope);
    // Greet first (this frame is queued ahead of any fan-out).
    shared.events.send_to(
        id,
        &ControlEvent::Hello {
            pid: shared.pid,
            version: shared.version.clone(),
        },
    );
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                None => break,
            },
            inbound = socket.recv() => match inbound {
                Some(Ok(Message::Close(_))) | None => break,
                Some(Ok(_)) => {}
                Some(Err(_)) => break,
            },
        }
    }
    shared.events.remove_client(id);
}

// ---- fallbacks ----------------------------------------------------------------------------

async fn method_not_allowed() -> Response {
    jstatus(405, json!({ "error": "method not allowed" }))
}

async fn not_found(uri: Uri) -> Response {
    jstatus(404, json!({ "error": "not found", "path": uri.path() }))
}

// ---- query-param parsing (mirrors the TS posNum / nonNegNum / Number(tail)) ----------------

fn pos_num(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as i64)
}

fn non_neg_num(v: Option<&String>) -> Option<i64> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as i64)
}

fn tail_num(v: Option<&String>) -> Option<usize> {
    v.and_then(|s| s.parse::<f64>().ok())
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as usize)
}

fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn num_parsers_match_ts_semantics() {
        assert_eq!(pos_num(Some(&"600".to_string())), Some(600));
        assert_eq!(pos_num(Some(&"0".to_string())), None);
        assert_eq!(pos_num(Some(&"-5".to_string())), None);
        assert_eq!(pos_num(None), None);
        assert_eq!(non_neg_num(Some(&"0".to_string())), Some(0));
        assert_eq!(non_neg_num(Some(&"-1".to_string())), None);
        assert_eq!(tail_num(Some(&"3".to_string())), Some(3));
        assert_eq!(tail_num(Some(&"0".to_string())), None);
    }

    #[test]
    fn tail_lines_keeps_last_n() {
        assert_eq!(tail_lines("a\nb\nc\nd", 2), "c\nd");
        assert_eq!(tail_lines("a\nb", 5), "a\nb");
    }
}

/// Golden-JSON parity: boot the REAL axum stack and assert byte-exact response bodies for every
/// route + error shape (modulo port/token/pid), against the `src/main/control-server.ts` oracle.
/// Runs the full socket round-trip with `reqwest` (a lib dependency, available to unit tests).
#[cfg(test)]
mod golden {
    use super::{project_tab_target, router, Scope};
    use crate::control::readmodel::{PaneInfo, PaneStatus, TabInfo, WindowInfo};
    use crate::control::server::Shared;
    use crate::session_manager::SessionManager;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    struct Server {
        shared: Arc<Shared>,
        base: String,
        token: String,
    }

    async fn boot(allow_input: bool) -> Server {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let sessions = Arc::new(SessionManager::new(tx));
        let control_file = std::env::temp_dir().join("hp-golden-control.json");
        let speech_settings = std::env::temp_dir().join("hp-golden-speech.json");
        let shared = Shared::new(
            sessions,
            allow_input,
            "0.1.8",
            control_file,
            speech_settings,
        );
        shared.model.lock().unwrap().add_window(WindowInfo {
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
        let token = "tok-master".to_string();
        shared.tokens.lock().unwrap().set_master(token.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        shared.port.store(port, Ordering::SeqCst);
        let app = router(Arc::clone(&shared));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server {
            shared,
            base: format!("http://127.0.0.1:{port}"),
            token,
        }
    }

    fn pane(id: &str, uid: &str) -> PaneInfo {
        PaneInfo {
            id: id.into(),
            session_uid: uid.into(),
            label: "shell".into(),
            subtitle: None,
            color: "#3b82f6".into(),
            command: None,
            args: None,
            cwd: None,
            shell: None,
            status: PaneStatus::Running,
            exit_code: None,
            meta: None,
            talk: false,
            env: None,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn post_cmd(s: &Server, body: Value) -> (u16, Value) {
        let r = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&body)
            .send()
            .await
            .unwrap();
        let st = r.status().as_u16();
        (st, r.json().await.unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn group_and_ungroup_panes_queue_gui_requests() {
        use crate::control::readmodel::{PaneGroupInfo, WindowInfo};
        use crate::control::server::PaneGroupRequest;
        let s = boot(true).await;
        {
            let mut m = s.shared.model.lock().unwrap();
            m.insert_pane(1, pane("p1", "u1"));
            m.insert_pane(1, pane("p2", "u2"));
            m.add_window(WindowInfo {
                window_id: 2,
                active_tab_id: Some("w2t".into()),
                tabs: vec![TabInfo {
                    id: "w2t".into(),
                    title: "T".into(),
                    color: None,
                    layout: "auto".into(),
                    panes: vec![],
                    pane_group: None,
                }],
            });
            m.insert_pane(2, pane("p3", "u3"));
            m.add_window(WindowInfo {
                window_id: 3,
                active_tab_id: Some("g1".into()),
                tabs: vec![TabInfo {
                    id: "g1".into(),
                    title: "Group".into(),
                    color: None,
                    layout: "auto".into(),
                    panes: vec![],
                    pane_group: Some(PaneGroupInfo::default()),
                }],
            });
        }
        // Happy path: exactly one Group request queued, tabId returned.
        let (st, v) = post_cmd(
            &s,
            json!({"type":"groupPanes","paneIds":["p1","p2"],"title":"G"}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        let tab_id = v["tabId"].as_str().unwrap().to_string();
        assert_eq!(v["result"]["tabId"], json!(tab_id));
        let q = s.shared.take_pane_group_requests();
        assert_eq!(
            q,
            vec![PaneGroupRequest::Group {
                tab_id,
                window_id: 1,
                pane_ids: vec!["p1".into(), "p2".into()],
                title: Some("G".into()),
                project: None,
            }]
        );
        // Each 400 case queues nothing.
        for body in [
            json!({"type":"groupPanes","paneIds":["p1"]}),
            json!({"type":"groupPanes","paneIds":["p1","p1"]}),
            json!({"type":"groupPanes","paneIds":[]}),
            json!({"type":"groupPanes"}),
            json!({"type":"groupPanes","paneIds":["p1","nope"]}),
            json!({"type":"groupPanes","paneIds":["p1","p3"]}),
            json!({"type":"groupPanes","paneIds":["p1","p2"],"project":"no-such-project-xyz"}),
            json!({"type":"ungroupPanes","tabId":"t1"}),
            json!({"type":"ungroupPanes","tabId":"nope"}),
            json!({"type":"ungroupPanes"}),
        ] {
            let (st, v) = post_cmd(&s, body.clone()).await;
            assert_eq!(st, 400, "{body}: {v}");
            assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        assert!(s.shared.take_pane_group_requests().is_empty());
        // Ungroup of a real group tab queues one request.
        let (st, _) = post_cmd(&s, json!({"type":"ungroupPanes","tabId":"g1"})).await;
        assert_eq!(st, 200);
        assert_eq!(
            s.shared.take_pane_group_requests(),
            vec![PaneGroupRequest::Ungroup {
                tab_id: "g1".into()
            }]
        );
    }

    #[tokio::test]
    async fn move_pane_validates_and_queues_a_gui_request() {
        use crate::control::readmodel::WindowInfo;
        use crate::control::server::{MoveTarget, PaneGroupRequest};
        let s = boot(true).await;
        {
            let mut m = s.shared.model.lock().unwrap();
            m.insert_pane(1, pane("p1", "u1"));
            m.insert_pane(1, pane("p2", "u2"));
            m.add_window(WindowInfo {
                window_id: 2,
                active_tab_id: Some("w2t".into()),
                tabs: vec![TabInfo {
                    id: "w2t".into(),
                    title: "T".into(),
                    color: None,
                    layout: "auto".into(),
                    panes: vec![],
                    pane_group: None,
                }],
            });
            m.insert_pane(2, pane("solo", "u3"));
        }
        // To an existing tab in another window.
        let (st, v) = post_cmd(&s, json!({"type":"movePane","paneId":"p1","tabId":"w2t"})).await;
        assert_eq!(st, 200, "{v}");
        assert_eq!(v["tabId"], json!("w2t"));
        assert_eq!(v["result"]["tabId"], json!("w2t"));
        assert_eq!(
            s.shared.take_pane_group_requests(),
            vec![PaneGroupRequest::Move {
                pane_id: "p1".into(),
                window_id: 2,
                to: MoveTarget::Tab {
                    tab_id: "w2t".into()
                },
            }]
        );
        // To a new tab: defaults to the pane's own window; the returned id is the new tab's.
        let (st, v) = post_cmd(
            &s,
            json!({"type":"movePane","paneId":"p2","newTab":{"title":"proj-a","color":"#a855f7"}}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        let tab_id = v["tabId"].as_str().unwrap().to_string();
        assert_eq!(
            s.shared.take_pane_group_requests(),
            vec![PaneGroupRequest::Move {
                pane_id: "p2".into(),
                window_id: 1,
                to: MoveTarget::NewTab {
                    tab_id,
                    title: Some("proj-a".into()),
                    color: Some("#a855f7".into()),
                },
            }]
        );
        // A new tab in another window.
        let (st, v) = post_cmd(
            &s,
            json!({"type":"movePane","paneId":"p2","newTab":{},"windowId":2}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        assert!(matches!(
            s.shared.take_pane_group_requests().as_slice(),
            [PaneGroupRequest::Move {
                window_id: 2,
                to: MoveTarget::NewTab { .. },
                ..
            }]
        ));
        // Already in the target tab: ok, nothing queued.
        let (st, v) = post_cmd(&s, json!({"type":"movePane","paneId":"p1","tabId":"t1"})).await;
        assert_eq!(st, 200, "{v}");
        assert!(s.shared.take_pane_group_requests().is_empty());
        // The window's only pane can't leave it.
        let (st, v) = post_cmd(&s, json!({"type":"movePane","paneId":"solo","tabId":"t1"})).await;
        assert_eq!(st, 409, "{v}");
        // Each 400 case queues nothing.
        for body in [
            json!({"type":"movePane","tabId":"t1"}),
            json!({"type":"movePane","paneId":"nope","tabId":"t1"}),
            json!({"type":"movePane","paneId":"p1"}),
            json!({"type":"movePane","paneId":"p1","tabId":"t1","newTab":{}}),
            json!({"type":"movePane","paneId":"p1","tabId":"nope"}),
            json!({"type":"movePane","paneId":"p1","newTab":"x"}),
            json!({"type":"movePane","paneId":"p1","newTab":{"color":"red"}}),
            json!({"type":"movePane","paneId":"p1","newTab":{},"windowId":99}),
            json!({"type":"movePane","paneId":"p1","project":"no-such-project-xyz"}),
            json!({"type":"newPane","windowId":1,"pane":{"projectTab":true}}),
            json!({"type":"newPane","windowId":1,"pane":{"project":"x","projectTab":"yes"}}),
        ] {
            let (st, v) = post_cmd(&s, body.clone()).await;
            assert_eq!(st, 400, "{body}: {v}");
            assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        assert!(s.shared.take_pane_group_requests().is_empty());
    }

    /// Issue #125: the `tabId` a `movePane {newTab}` answers with is addressable at once (a
    /// second `movePane {tabId}` right after succeeds), and the tab stays in the model across GUI
    /// republishes until the GUI publishes it under that id.
    #[tokio::test]
    async fn move_pane_new_tab_id_is_addressable_at_once() {
        use crate::control::server::{MoveTarget, PaneGroupRequest};
        let s = boot(true).await;
        {
            let mut m = s.shared.model.lock().unwrap();
            m.insert_pane(1, pane("a", "ua"));
            m.insert_pane(1, pane("b", "ub"));
            m.insert_pane(1, pane("c", "uc"));
        }
        let (st, v) = post_cmd(
            &s,
            json!({"type":"movePane","paneId":"a","newTab":{"title":"x","color":"#A5F"}}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        let tab_id = v["tabId"].as_str().unwrap().to_string();
        // At once, before any GUI tick.
        let (st, v) = post_cmd(&s, json!({"type":"movePane","paneId":"b","tabId":tab_id})).await;
        assert_eq!(st, 200, "an immediate movePane into the returned tab: {v}");
        assert_eq!(v["tabId"], json!(tab_id));
        // The tab is in /state, as the GUI will make it (title, normalized colour).
        {
            let m = s.shared.model.lock().unwrap();
            assert_eq!(m.tab_window(&tab_id), Some(1));
            let t = m.tabs().find(|(_, t)| t.id == tab_id).unwrap().1;
            assert_eq!(t.title, "x");
            assert_eq!(t.color.as_deref(), Some("#aa55ff"));
            assert!(m.is_pending_tab(&tab_id));
        }
        // Both moves queued in order; the GUI makes the tab with the first, fills it with the second.
        assert_eq!(
            s.shared.take_pane_group_requests(),
            vec![
                PaneGroupRequest::Move {
                    pane_id: "a".into(),
                    window_id: 1,
                    to: MoveTarget::NewTab {
                        tab_id: tab_id.clone(),
                        title: Some("x".into()),
                        color: Some("#A5F".into()),
                    },
                },
                PaneGroupRequest::Move {
                    pane_id: "b".into(),
                    window_id: 1,
                    to: MoveTarget::Tab {
                        tab_id: tab_id.clone()
                    },
                },
            ]
        );
        // A newPane straight into the returned tab also works at once.
        let (st, v) = post_cmd(
            &s,
            json!({"type":"newPane","tabId":tab_id,"pane":{"command":"true"}}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        let new_id = v["result"].as_str().unwrap().to_string();

        // A GUI republish that doesn't show the tab yet keeps it (and the not-yet-adopted pane in it).
        let gui_tree = |tabs: Vec<TabInfo>| {
            vec![WindowInfo {
                window_id: 1,
                active_tab_id: Some("t1".into()),
                tabs,
            }]
        };
        let t1 = |panes: Vec<PaneInfo>| TabInfo {
            id: "t1".into(),
            title: "Tab 1".into(),
            color: None,
            layout: "auto".into(),
            panes,
            pane_group: None,
        };
        let hosted: std::collections::HashSet<String> =
            ["ua", "ub", "uc"].iter().map(|u| u.to_string()).collect();
        {
            let mut m = s.shared.model.lock().unwrap();
            m.publish_replace(
                &[1],
                gui_tree(vec![t1(vec![
                    pane("a", "ua"),
                    pane("b", "ub"),
                    pane("c", "uc"),
                ])]),
                &hosted,
            );
            assert_eq!(
                m.tab_window(&tab_id),
                Some(1),
                "pending tab survives a republish"
            );
            assert_eq!(m.coords_of(&new_id).unwrap().tab_id, tab_id);
        }
        let (st, v) = post_cmd(&s, json!({"type":"movePane","paneId":"c","tabId":tab_id})).await;
        assert_eq!(st, 200, "still addressable after a republish: {v}");
        // The GUI publishes the tab under the same id: it is no longer pending.
        {
            let mut m = s.shared.model.lock().unwrap();
            let mut made = t1(vec![pane("c", "uc")]);
            made.id = tab_id.clone();
            made.panes = vec![pane("a", "ua"), pane("b", "ub")];
            m.publish_replace(
                &[1],
                gui_tree(vec![t1(vec![pane("c", "uc")]), made]),
                &hosted,
            );
            assert!(!m.is_pending_tab(&tab_id));
            assert_eq!(m.tab_window(&tab_id), Some(1));
            // ... so once the GUI drops it, so does the model.
            m.publish_replace(&[1], gui_tree(vec![t1(vec![pane("c", "uc")])]), &hosted);
            assert_eq!(m.tab_window(&tab_id), None);
        }
    }

    /// `attach` answers with TAB ids; each stays addressable across GUI republishes until the GUI
    /// adopts the group's panes into a tab under that id.
    #[tokio::test]
    async fn attach_tab_ids_survive_a_republish_before_adoption() {
        let s = boot(true).await;
        let (st, v) = post_cmd(
            &s,
            json!({"type":"attach","windowId":1,"groups":[{"title":"g","panes":[{"command":"true"}]}]}),
        )
        .await;
        assert_eq!(st, 200, "{v}");
        let tab_id = v["result"][0].as_str().unwrap().to_string();
        let mut m = s.shared.model.lock().unwrap();
        let pane_id = m.tabs().find(|(_, t)| t.id == tab_id).unwrap().1.panes[0]
            .id
            .clone();
        m.publish_replace(
            &[1],
            vec![WindowInfo {
                window_id: 1,
                active_tab_id: Some("t1".into()),
                tabs: vec![bare_tab("t1", None)],
            }],
            &std::collections::HashSet::new(),
        );
        assert_eq!(m.tab_window(&tab_id), Some(1));
        assert_eq!(
            m.coords_of(&pane_id).unwrap().tab_id,
            tab_id,
            "the not-yet-adopted pane is carried back into its own tab"
        );
    }

    fn bare_tab(id: &str, pane_group: Option<crate::control::readmodel::PaneGroupInfo>) -> TabInfo {
        TabInfo {
            id: id.into(),
            title: id.into(),
            color: None,
            layout: "auto".into(),
            panes: vec![],
            pane_group,
        }
    }

    #[tokio::test]
    async fn new_pane_with_tab_id_spawns_straight_into_that_tab() {
        let s = boot(true).await;
        // t1 stays the ACTIVE tab; t2 is a background tab.
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_tab(1, bare_tab("t2", None));
        for body in [
            json!({"type":"newPane","windowId":1,"tabId":"t2","pane":{"command":"true"}}),
            // windowId is optional: the tab names its window.
            json!({"type":"newPane","tabId":"t2","pane":{"command":"true"}}),
        ] {
            let (st, v) = post_cmd(&s, body.clone()).await;
            assert_eq!(st, 200, "{body}: {v}");
            let id = v["result"].as_str().unwrap();
            let c = s.shared.model.lock().unwrap().coords_of(id).unwrap();
            assert_eq!((c.window_id, c.tab_id.as_str()), (1, "t2"), "{body}");
        }
        // Placed by the model, not moved by the GUI: nothing queued.
        assert!(s.shared.take_pane_group_requests().is_empty());
        // Each 400 case spawns nothing.
        let before = s
            .shared
            .model
            .lock()
            .unwrap()
            .tabs()
            .map(|(_, t)| t.panes.len())
            .sum::<usize>();
        for body in [
            json!({"type":"newPane","windowId":1,"tabId":"nope","pane":{"command":"true"}}),
            json!({"type":"newPane","windowId":1,"tabId":7,"pane":{"command":"true"}}),
            json!({"type":"newPane","windowId":2,"tabId":"t2","pane":{"command":"true"}}),
            json!({"type":"newPane","windowId":1,"tabId":"t2","pane":{"project":"x","projectTab":true}}),
        ] {
            let (st, v) = post_cmd(&s, body.clone()).await;
            assert_eq!(st, 400, "{body}: {v}");
            assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()));
        }
        let after = s
            .shared
            .model
            .lock()
            .unwrap()
            .tabs()
            .map(|(_, t)| t.panes.len())
            .sum::<usize>();
        assert_eq!(before, after);
        assert!(s.shared.take_pane_group_requests().is_empty());
    }

    #[test]
    fn project_tab_target_finds_the_windows_project_tab_in_scope() {
        use crate::control::readmodel::{PaneGroupInfo, ReadModel, WindowInfo};
        let group = |p: &str| {
            Some(PaneGroupInfo {
                project: Some(p.into()),
                ..Default::default()
            })
        };
        let mut m = ReadModel::new();
        m.add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("lead".into()),
            tabs: vec![bare_tab("lead", None), bare_tab("pa", group("proj-a"))],
        });
        m.add_window(WindowInfo {
            window_id: 2,
            active_tab_id: Some("pb".into()),
            tabs: vec![bare_tab("pb", group("proj-b"))],
        });
        assert_eq!(
            project_tab_target(&m, None, 1, "proj-a").as_deref(),
            Some("pa")
        );
        // Another window's project tab is not reused; a missing one is created by the GUI.
        assert_eq!(project_tab_target(&m, None, 1, "proj-b"), None);
        assert_eq!(project_tab_target(&m, None, 1, "proj-c"), None);
        // A scoped caller only lands directly in a tab it may act on.
        let own = Scope {
            tab_ids: Some(vec!["pa".into()]),
            ..Default::default()
        };
        let other = Scope {
            tab_ids: Some(vec!["lead".into()]),
            ..Default::default()
        };
        assert_eq!(
            project_tab_target(&m, Some(&own), 1, "proj-a").as_deref(),
            Some("pa")
        );
        assert_eq!(project_tab_target(&m, Some(&other), 1, "proj-a"), None);
    }

    #[tokio::test]
    async fn fs_read_serves_text_master_only() {
        let s = boot(true).await;
        let dir = std::env::temp_dir().join(format!("hp-fsread-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hello.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let path = file.display().to_string();

        // Master token reads the file.
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let v: Value = r.json().await.unwrap();
        assert_eq!(v["content"], json!("fn main() {}\n"));
        assert_eq!(v["truncated"], json!(false));

        // Missing file → 404; relative path → 400; no auth → 401.
        let r = client()
            .get(format!(
                "{}/fs/read?path={}/nope.txt",
                s.base,
                dir.display()
            ))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        let r = client()
            .get(format!("{}/fs/read?path=relative.txt", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);

        // A scoped token is refused outright.
        let mint: Value = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({"scope": {"tabIds": ["t1"]}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let scoped = mint["token"].as_str().unwrap();
        let r = client()
            .get(format!("{}/fs/read?path={}", s.base, path))
            .header("authorization", format!("Bearer {scoped}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn connect_remote_and_remote_screen_commands() {
        let s = boot(true).await;
        let secret = "SUPERSECRETTOKEN123";
        let url = format!("hp://127.0.0.1:9/?token={secret}&v=1");
        let post = |tok: String, body: Value| {
            let base = s.base.clone();
            async move {
                let r = client()
                    .post(format!("{base}/command"))
                    .header("authorization", format!("Bearer {tok}"))
                    .json(&body)
                    .send()
                    .await
                    .unwrap();
                let st = r.status().as_u16();
                (st, r.text().await.unwrap())
            }
        };

        // Master: accepted, hostId returned, the token/url is never echoed.
        let (st, body) = post(
            s.token.clone(),
            json!({"type": "connectRemote", "url": url, "label": "x"}),
        )
        .await;
        assert_eq!(st, 200);
        assert!(!body.contains(secret), "token echoed: {body}");
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], json!(true));
        let id = v["result"]["hostId"].as_u64().unwrap() as u32;
        let q = s.shared.take_remote_connects();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].host_id, id);
        assert_eq!(q[0].label.as_deref(), Some("x"));
        assert!(!q[0].remember);
        assert_eq!(q[0].target.token(), secret);
        assert!(!format!("{:?}", q[0]).contains(secret));

        // A malformed url is a 400 whose error does not echo the input.
        let (st, body) = post(
            s.token.clone(),
            json!({"type": "connectRemote", "url": format!("ftp://h/?token={secret}")}),
        )
        .await;
        assert_eq!(st, 400);
        assert!(!body.contains(secret));
        assert!(s.shared.take_remote_connects().is_empty());

        // remoteScreen serves the GUI's published snapshot.
        s.shared
            .set_remote_screens(json!([{"hostId": 1, "screenText": "hello"}]));
        let (st, body) = post(s.token.clone(), json!({"type": "remoteScreen"})).await;
        assert_eq!(st, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["result"][0]["screenText"], json!("hello"));

        // A scoped token is refused for both.
        let scoped = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "scope": { "tabIds": ["t1"] } }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        for cmd in [
            json!({"type": "connectRemote", "url": url}),
            json!({"type": "remoteScreen"}),
        ] {
            let (st, body) = post(scoped.clone(), cmd).await;
            assert_eq!(st, 403, "{body}");
            assert!(!body.contains(secret));
        }
        assert!(s.shared.take_remote_connects().is_empty());
    }

    #[tokio::test]
    async fn devices_mint_list_revoke_full_authority_and_master_only() {
        let s = boot(true).await;

        // Master mints a device token; it comes back and carries full (unscoped) authority.
        let mint: Value = client()
            .post(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "label": "iphone" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mint["ok"], json!(true));
        assert_eq!(mint["label"], json!("iphone"));
        let device = mint["token"].as_str().unwrap().to_string();

        // The device token authenticates like the master (e.g. reads /state).
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {device}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);

        // GET /devices lists it by label and NEVER echoes the token back.
        let list: Value = client()
            .get(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["devices"][0]["label"], json!("iphone"));
        assert!(list["devices"][0].get("token").is_none());

        // A scoped token can neither mint nor list device credentials.
        let scoped = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .json(&json!({ "scope": { "tabIds": ["t1"] } }))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        for method in ["post", "get"] {
            let req = if method == "post" {
                client().post(format!("{}/devices", s.base))
            } else {
                client().get(format!("{}/devices", s.base))
            };
            let r = req
                .header("authorization", format!("Bearer {scoped}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status().as_u16(), 403);
        }

        // Revoke by label drops it; the device token is then rejected, and the list is empty.
        let revoked: Value = client()
            .delete(format!("{}/devices?label=iphone", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(revoked["revoked"], json!(1));
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {device}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        let list: Value = client()
            .get(format!("{}/devices", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(list["devices"].as_array().unwrap().len(), 0);

        let _ = std::fs::remove_file(s.shared.control_file.with_file_name("device-tokens.json"));
    }

    #[tokio::test]
    async fn health_is_byte_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/health", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body = r.text().await.unwrap();
        let pid = std::process::id();
        assert_eq!(
            body,
            format!(
                r#"{{"ok":true,"app":"hyperpanes","pid":{pid},"version":"0.1.8","allowInput":true,"workQueue":"memory","inbox":"memory"}}"#
            )
        );
    }

    #[tokio::test]
    async fn health_needs_no_auth() {
        let s = boot(false).await;
        let r = client()
            .get(format!("{}/health", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body = r.text().await.unwrap();
        assert!(body.contains(r#""allowInput":false"#));
        // #131: no pid without a valid token.
        assert!(!body.contains("\"pid\""), "{body}");
        assert_eq!(
            body,
            r#"{"ok":true,"app":"hyperpanes","version":"0.1.8","allowInput":false,"workQueue":"memory","inbox":"memory"}"#
        );
        // An INVALID token is treated as unauthenticated: still 200, still no pid.
        let r = client()
            .get(format!("{}/health", s.base))
            .header("authorization", "Bearer not-a-real-token")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        assert!(!r.text().await.unwrap().contains("\"pid\""));
    }

    #[tokio::test]
    async fn state_empty_window_is_byte_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: Value = serde_json::from_str(&r.text().await.unwrap()).unwrap();
        // `windows` stays byte-exact (the frozen legacy shape); `speech` is additive — its
        // `backend` is environment-dependent (whatever TTS the test machine has on PATH),
        // so only its shape is asserted, not the exact backend name.
        assert_eq!(
            body["windows"],
            json!([{"windowId":1,"activeTabId":"t1","tabs":[{"id":"t1","title":"Tab 1","color":null,"layout":"auto","status":"idle","panes":[],"paneGroup":null}]}])
        );
        assert_eq!(body["speech"]["muted"], json!(false));
        assert_eq!(body["speech"]["focusedOnly"], json!(false));
        assert!(body["speech"]["backend"].is_string());
        assert_eq!(body["speech"]["speakingPane"], Value::Null);
        // `app` is additive and first; public (no build label) has no `build` key.
        assert_eq!(body["app"], json!({"version": "0.1.8"}));
        assert!(body["app"].as_object().unwrap().get("build").is_none());
    }

    #[tokio::test]
    async fn state_and_health_report_build_label_when_set() {
        let s = boot(true).await;
        s.shared.set_build(Some("private build, abc1234"));
        let body = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.starts_with(
            r#"{"app":{"version":"0.1.8","build":"private build, abc1234"},"windows""#
        ));
        let h = client()
            .get(format!("{}/health", s.base))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(h.ends_with(
            r#""workQueue":"memory","inbox":"memory","build":"private build, abc1234"}"#
        ));
    }

    #[tokio::test]
    async fn state_with_pane_omits_unset_optionals_and_keeps_field_order() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let body = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // A never-output running pane reads `busy`; optionals omitted; field order preserved.
        assert!(body.contains(
            r##""panes":[{"id":"p1","sessionUid":"u1","label":"shell","color":"#3b82f6","status":"running","activity":"busy"}]"##
        ));
    }

    #[tokio::test]
    async fn unauthorized_is_401_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/state", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn no_such_pane_is_404_exact() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/panes/ghost/output", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"no such pane","paneId":"ghost"}"#
        );
    }

    #[tokio::test]
    async fn unknown_path_is_404_not_found_with_path() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/nope", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 404);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"not found","path":"/nope"}"#
        );
    }

    #[tokio::test]
    async fn output_of_a_sessionless_pane_is_byte_exact() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .get(format!("{}/panes/p1/output", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        // serde_json::Value object → keys sorted; cursor ALWAYS present.
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"cursor":0,"output":"","paneId":"p1","status":"running","stripped":false}"#
        );
    }

    #[tokio::test]
    async fn input_blocked_when_allow_input_off_is_403() {
        let s = boot(false).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let r = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"data":"hi"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 403);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"input not allowed"}"#);
    }

    #[tokio::test]
    async fn messages_post_then_get_roundtrip() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let post = client()
            .post(format!("{}/panes/p1/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"from":"mgr","body":"go"}"#)
            .send()
            .await
            .unwrap();
        let post: serde_json::Value = post.json().await.unwrap();
        assert_eq!(post["ok"], serde_json::json!(true));
        assert_eq!(post["seq"], serde_json::json!(1));
        assert_eq!(post["nudged"], serde_json::json!(true));
        assert!(post["epoch"].as_u64().is_some_and(|e| e > 0));
        let get: serde_json::Value = client()
            .get(format!("{}/panes/p1/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        // ts is wall-clock; assert every other field byte-for-byte via structural compare.
        assert_eq!(get["paneId"], serde_json::json!("p1"));
        assert_eq!(get["dropped"], serde_json::json!(0));
        assert_eq!(get["latestSeq"], serde_json::json!(1));
        let m = &get["messages"][0];
        assert_eq!(m["seq"], serde_json::json!(1));
        assert_eq!(m["to"], serde_json::json!("p1"));
        assert_eq!(m["from"], serde_json::json!("mgr"));
        assert_eq!(m["body"], serde_json::json!("go"));
        assert!(m["ts"].is_number());
    }

    /// A goals-org pane (role goals-orch): a post normally arms the inbox nudge.
    fn orch_pane(id: &str, uid: &str) -> PaneInfo {
        let mut p = pane(id, uid);
        p.meta = Some(
            [("role".to_string(), "goals-orch".to_string())]
                .into_iter()
                .collect(),
        );
        p
    }

    async fn post_message(s: &Server, pane: &str, body: &str) -> serde_json::Value {
        client()
            .post(format!("{}/panes/{pane}/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    // t36: a sender that wakes the receiver itself posts with nudge:false, so one
    // relay costs the receiver one turn. The message is still stored for read_messages.
    #[tokio::test]
    async fn messages_post_with_nudge_false_stores_but_does_not_arm_the_nudge() {
        if !super::nudges_enabled() {
            return; // HYPERPANES_MSG_NUDGE=0 in this environment: nothing to tell apart
        }
        let s = boot(true).await;
        {
            let mut m = s.shared.model.lock().unwrap();
            m.insert_pane(1, orch_pane("quiet", "u1"));
            m.insert_pane(1, orch_pane("loud", "u2"));
        }
        let quiet = post_message(
            &s,
            "quiet",
            r#"{"from":"lead","body":"relay","nudge":false}"#,
        )
        .await;
        assert_eq!(quiet["nudged"], serde_json::json!(false));
        let loud = post_message(&s, "loud", r#"{"from":"lead","body":"relay"}"#).await;
        assert_eq!(loud["nudged"], serde_json::json!(true));
        {
            let mut ledger = s.shared.nudges.lock().unwrap();
            // `arm` answers true only when no waiter is in flight for that pane.
            assert!(
                ledger.arm("quiet", 99, 0),
                "nudge:false must not arm a waiter"
            );
            assert!(!ledger.arm("loud", 99, 0), "the default post arms a waiter");
        }
        let get: serde_json::Value = client()
            .get(format!("{}/panes/quiet/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(get["messages"][0]["body"], serde_json::json!("relay"));
    }

    // t39: three relays land for an orchestrator, and it reads them during the turn some other
    // wake started. The nudge still waiting for it to go idle must not fire for that mail.
    #[tokio::test]
    async fn messages_get_drops_the_pending_nudge_for_what_it_returned() {
        if !super::nudges_enabled() {
            return; // HYPERPANES_MSG_NUDGE=0 in this environment: nothing is ever armed
        }
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, orch_pane("orch", "u1"));
        for body in ["T37", "T38", "note"] {
            let post = format!(r#"{{"from":"lead","body":"{body}"}}"#);
            assert_eq!(
                post_message(&s, "orch", &post).await["nudged"],
                serde_json::json!(true)
            );
        }
        assert_eq!(s.shared.nudges.lock().unwrap().pending("orch"), 3);
        let read = |after: u64| {
            let url = format!("{}/panes/orch/messages?after={after}", s.base);
            let token = s.token.clone();
            async move {
                client()
                    .get(url)
                    .header("authorization", format!("Bearer {token}"))
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()
            }
        };
        let got = read(0).await;
        assert_eq!(got["messages"].as_array().map(Vec::len), Some(3));
        assert_eq!(s.shared.nudges.lock().unwrap().pending("orch"), 0);
        // Once the pane goes idle the waiter finds nothing unread and types nothing.
        let later = super::now_ms() + 60_000;
        assert_eq!(
            s.shared.nudges.lock().unwrap().poll(
                "orch",
                false,
                later,
                super::nudge::NudgeForm::Mcp
            ),
            super::nudge::Step::Stop
        );
        // New mail after the read is pending again. A read at the newest cursor (an empty reply:
        // the reader already holds seq 4) clears it too.
        post_message(&s, "orch", r#"{"from":"lead","body":"late"}"#).await;
        assert_eq!(s.shared.nudges.lock().unwrap().pending("orch"), 1);
        let empty = read(4).await;
        assert_eq!(empty["messages"], serde_json::json!([]));
        assert_eq!(empty["reset"], serde_json::json!(false));
        assert_eq!(s.shared.nudges.lock().unwrap().pending("orch"), 0);
        // A stale cursor (an earlier inbox lifetime) gets the backlog back and counts as a read.
        post_message(&s, "orch", r#"{"from":"lead","body":"after restart"}"#).await;
        let stale = read(48).await;
        assert_eq!(stale["reset"], serde_json::json!(true));
        assert_eq!(s.shared.nudges.lock().unwrap().pending("orch"), 0);
    }

    // t36: an in-memory inbox restarts at seq 1, so after an app restart a reader's old cursor (after:48) is
    // past every seq. It must get the backlog back flagged reset, not an empty list.
    #[tokio::test]
    async fn messages_get_with_a_stale_cursor_returns_everything_flagged_reset() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        post_message(&s, "p1", r#"{"from":"lead","body":"T36 goal"}"#).await;
        let read = |after: u64| {
            let url = format!("{}/panes/p1/messages?after={after}", s.base);
            let token = s.token.clone();
            async move {
                client()
                    .get(url)
                    .header("authorization", format!("Bearer {token}"))
                    .send()
                    .await
                    .unwrap()
                    .json::<serde_json::Value>()
                    .await
                    .unwrap()
            }
        };
        let stale = read(48).await;
        assert_eq!(stale["reset"], serde_json::json!(true));
        assert_eq!(stale["messages"][0]["body"], serde_json::json!("T36 goal"));
        assert_eq!(stale["latestSeq"], serde_json::json!(1));
        let current = read(1).await;
        assert_eq!(current["reset"], serde_json::json!(false));
        assert_eq!(current["messages"], serde_json::json!([]));
        assert_eq!(current["epoch"], stale["epoch"]);
    }

    /// od2-A: after a full restore a pane has a new id. Mail posted to the OLD id once the alias
    /// is registered must land on the live pane and be readable under its new id.
    #[tokio::test]
    async fn mail_to_an_old_pane_id_lands_on_the_aliased_live_pane() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("pane-new", "u9"));
        assert!(s.shared.register_pane_alias("pane-old", "pane-new"));
        assert!(!s.shared.register_pane_alias("pane-x", "pane-x"));
        for old in ["pane-old", "old"] {
            let post = client()
                .post(format!("{}/panes/{old}/messages", s.base))
                .header("authorization", format!("Bearer {}", s.token))
                .header("content-type", "application/json")
                .body(r#"{"from":"impl","body":"via old id"}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(post.status().as_u16(), 200, "{old}");
        }
        let get: serde_json::Value = client()
            .get(format!("{}/panes/pane-new/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(get["paneId"], serde_json::json!("pane-new"));
        assert_eq!(get["messages"].as_array().unwrap().len(), 2);
        assert_eq!(get["messages"][0]["body"], serde_json::json!("via old id"));
        // an unaliased unknown id still 404s
        let miss = client()
            .get(format!("{}/panes/pane-nope/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(miss.status().as_u16(), 404);
    }

    /// od2-A: the durable inbox attaches only for a work.db owner; mail and aliases survive a
    /// fresh `Shared` over the same file; `resolve_pane_id` resolves an old id.
    #[tokio::test]
    async fn durable_inbox_attach_modes_and_survival() {
        use crate::control::server::{InboxMode, WorkAttach};
        let dir = std::env::temp_dir().join(format!("hp-inbox-attach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("messages.db");
        let owner = WorkAttach::Durable {
            recovered: 0,
            kept: 0,
        };
        {
            let s = boot(true).await;
            assert_eq!(s.shared.inbox_mode(), InboxMode::Memory);
            // a non-owner whose work queue never opened stays in-memory and writes nothing
            let failed = WorkAttach::OpenFailed("x".into());
            assert_eq!(
                s.shared.attach_durable_inbox_at(&db, &failed),
                InboxMode::Memory
            );
            assert!(!db.exists());
            // an owner whose store cannot open (a directory) stays in-memory
            assert_eq!(
                s.shared.attach_durable_inbox_at(&dir, &owner),
                InboxMode::Memory
            );
            assert_eq!(
                s.shared.attach_durable_inbox_at(&db, &owner),
                InboxMode::Durable
            );
            s.shared.inbox.lock().unwrap().post(
                "pane-old",
                "lead",
                "kept",
                crate::control::server::now_ms(),
            );
            assert!(s.shared.register_pane_alias("pane-old", "pane-new"));
        }
        let s = boot(true).await;
        assert_eq!(
            s.shared.attach_durable_inbox_at(&db, &owner),
            InboxMode::Durable
        );
        assert_eq!(s.shared.inbox.lock().unwrap().read("pane-new", 0).len(), 1);
        let mut m = s.shared.model.lock().unwrap();
        m.insert_pane(1, pane("pane-new", "u7"));
        assert_eq!(m.resolve_pane_id("old").as_deref(), Some("pane-new"));
        assert_eq!(m.resolve_pane_id("pane-old").as_deref(), Some("pane-new"));
        assert_eq!(m.resolve_pane_id("pane-zzz"), None);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The two pane-id spellings in the wild (`pane-<uuid>` from the app, bare `<uuid>` from the
    /// control API) must address the SAME inbox: an agent handed one form and reconstructing the
    /// other used to get `404 no such pane`, silently breaking the reply direction of the bus.
    #[tokio::test]
    async fn messages_are_addressable_by_either_pane_id_spelling() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("pane-abc", "u1"));
        // Post to the bare-uuid alias…
        let post = client()
            .post(format!("{}/panes/abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"from":"impl","body":"done"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(post.status().as_u16(), 200);
        // …and read it back under the canonical id.
        let get: serde_json::Value = client()
            .get(format!("{}/panes/pane-abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(get["paneId"], serde_json::json!("pane-abc"));
        assert_eq!(get["messages"][0]["body"], serde_json::json!("done"));
        // The reverse spelling reads the same queue and reports the canonical id.
        let alias: serde_json::Value = client()
            .get(format!("{}/panes/abc/messages", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(alias["paneId"], serde_json::json!("pane-abc"));
        assert_eq!(alias["messages"][0]["body"], serde_json::json!("done"));
    }

    #[tokio::test]
    async fn lock_acquire_then_nonowner_input_is_423() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        let lock: serde_json::Value = client()
            .post(format!("{}/panes/p1/lock", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"owner":"mgrA","ttlMs":60000}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(lock["ok"], serde_json::json!(true));
        assert_eq!(lock["owner"], serde_json::json!("mgrA"));
        assert!(lock["expiresAt"].is_number());
        // A different writer is refused 423 with the holder named.
        let blocked = client()
            .post(format!("{}/panes/p1/input", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"data":"x","owner":"mgrB"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status().as_u16(), 423);
        assert_eq!(
            blocked.text().await.unwrap(),
            r#"{"error":"pane locked","owner":"mgrA"}"#
        );
    }

    #[tokio::test]
    async fn tokens_mint_and_no_escalation() {
        let s = boot(true).await;
        s.shared
            .model
            .lock()
            .unwrap()
            .insert_pane(1, pane("p1", "u1"));
        // Master mints a pane-scoped token.
        let minted: serde_json::Value = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(r#"{"scope":{"paneIds":["p1"]}}"#)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(minted["ok"], serde_json::json!(true));
        let scoped = minted["token"].as_str().unwrap().to_string();
        assert_eq!(minted["token"].as_str().unwrap().len(), 64);
        assert_eq!(minted["scope"], serde_json::json!({ "paneIds": ["p1"] }));
        assert!(minted["events"]
            .as_str()
            .unwrap()
            .contains("/events?token="));
        // The scoped token cannot escalate to the whole window → 403.
        let esc = client()
            .post(format!("{}/tokens", s.base))
            .header("authorization", format!("Bearer {}", scoped))
            .header("content-type", "application/json")
            .body(r#"{"scope":{"windowIds":[1]}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(esc.status().as_u16(), 403);
        assert!(esc
            .text()
            .await
            .unwrap()
            .contains("outside the minting token's scope"));
    }

    // ---- /projects -----------------------------------------------------------------------
    // Only the side-effect-FREE paths are golden-tested here: the persistence layer writes the
    // real `projects.json` (no store injection), so exercising add/patch/delete over HTTP would
    // clobber the dev machine's actual registry and race parallel tests. The write paths are
    // covered by `persistence::projects` unit tests (`add_project_explicit_in`, …); these assert
    // auth + validation + the read-only GET contract the MCP depends on.

    #[tokio::test]
    async fn projects_list_is_authorized_and_returns_an_array() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 200);
        let body: serde_json::Value = r.json().await.unwrap();
        assert!(
            body["projects"].is_array(),
            "expected a projects array, got {body}"
        );
    }

    #[tokio::test]
    async fn projects_list_unauthorized_is_401() {
        let s = boot(true).await;
        let r = client()
            .get(format!("{}/projects", s.base))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn projects_add_without_dir_is_400() {
        let s = boot(true).await;
        let r = client()
            .post(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"expected { dir: string }"}"#
        );
    }

    #[tokio::test]
    async fn projects_add_nonexistent_dir_is_400() {
        let s = boot(true).await;
        // A path guaranteed not to exist → 400 before any registry write.
        let dir = format!("/nonexistent-hp-test-{}/repo", std::process::id());
        let r = client()
            .post(format!("{}/projects", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(format!(r#"{{"dir":"{dir}"}}"#))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        let body: serde_json::Value = r.json().await.unwrap();
        assert_eq!(
            body["error"],
            serde_json::json!("path doesn't exist or isn't a directory")
        );
    }

    #[tokio::test]
    async fn projects_patch_without_fields_is_400() {
        let s = boot(true).await;
        let r = client()
            .patch(format!("{}/projects/whatever", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 400);
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"error":"expected { name?: string, color?: string }"}"#
        );
    }

    #[tokio::test]
    async fn new_pane_command_returns_id_and_lands_in_state() {
        let s = boot(true).await;
        let resp: serde_json::Value = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(
                r#"{"type":"newPane","windowId":1,"pane":{"label":"worker","command":"echo hi"}}"#,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp["ok"], serde_json::json!(true));
        let pane_id = resp["result"].as_str().unwrap().to_string();
        // The pane is immediately present in /state (no debounce — synchronous in-process).
        let state = client()
            .get(format!("{}/state", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(state.contains(&format!(r#""id":"{pane_id}""#)));
        assert!(state.contains(r#""label":"worker""#));
        // Clean up the spawned pty.
        let _ = client()
            .post(format!("{}/command", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .header("content-type", "application/json")
            .body(format!(r#"{{"type":"closePane","paneId":"{pane_id}"}}"#))
            .send()
            .await;
    }

    // ---- work queue routes -----------------------------------------------------------------

    use serde_json::{json, Value};

    async fn post(s: &Server, path: &str, token: &str, body: &str) -> reqwest::Response {
        client()
            .post(format!("{}{}", s.base, path))
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .unwrap()
    }

    /// Enqueue one task into `queue` and claim it as `worker`; return `(id, fencingToken)`.
    async fn enqueue_and_claim(s: &Server, queue: &str, worker: &str) -> (String, u64) {
        let enq: Value = post(
            s,
            &format!("/queues/{queue}/tasks"),
            &s.token,
            r#"{"payload":"{}"}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        let id = enq["id"].as_str().unwrap().to_string();
        let claim: Value = post(
            s,
            &format!("/queues/{queue}/claim"),
            &s.token,
            &format!(r#"{{"worker":"{worker}"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        let fencing = claim["tasks"][0]["fencingToken"].as_u64().unwrap();
        (id, fencing)
    }

    #[tokio::test]
    async fn queue_enqueue_claim_ack_happy_path() {
        let s = boot(true).await;
        let enq: Value = post(
            &s,
            "/queues/build/tasks",
            &s.token,
            r#"{"payload":"{\"prompt\":\"do it\"}","kind":"manual","title":"T","priority":7}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(enq["ok"], json!(true));
        assert_eq!(enq["seq"], json!(1));
        let id = enq["id"].as_str().unwrap().to_string();

        // claim → a canonical Task carrying the fencing token + opaque payload verbatim
        let claim: Value = post(&s, "/queues/build/claim", &s.token, r#"{"worker":"wkr-1"}"#)
            .await
            .json()
            .await
            .unwrap();
        let t = &claim["tasks"][0];
        assert_eq!(t["id"], json!(id));
        assert_eq!(t["state"], json!("claimed"));
        assert_eq!(t["claimedBy"], json!("wkr-1"));
        assert_eq!(t["attempts"], json!(1));
        assert_eq!(t["fencingToken"], json!(1));
        assert_eq!(t["payload"], json!(r#"{"prompt":"do it"}"#));
        let fencing = t["fencingToken"].as_u64().unwrap();

        // ack with the fencing token → done
        let ack: Value = post(
            &s,
            &format!("/tasks/{id}/ack"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"result":"artifact://x"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(ack, json!({ "ok": true, "state": "done" }));

        // GET reflects the terminal state + recorded result
        let got: Value = client()
            .get(format!("{}/tasks/{id}", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(got["state"], json!("done"));
        assert_eq!(got["result"], json!("artifact://x"));
    }

    #[tokio::test]
    async fn queue_claim_empty_is_200_empty_tasks_byte_exact() {
        let s = boot(true).await;
        let r = post(&s, "/queues/nothing/claim", &s.token, r#"{"worker":"w"}"#).await;
        assert_eq!(r.status().as_u16(), 200);
        assert_eq!(r.text().await.unwrap(), r#"{"ok":true,"tasks":[]}"#);
    }

    #[tokio::test]
    async fn queue_ack_with_stale_fencing_token_is_409() {
        let s = boot(true).await;
        let (id, fencing) = enqueue_and_claim(&s, "build", "wkr-1").await;
        let r = post(
            &s,
            &format!("/tasks/{id}/ack"),
            &s.token,
            &format!(r#"{{"fencingToken":{}}}"#, fencing + 1),
        )
        .await;
        assert_eq!(r.status().as_u16(), 409);
        assert_eq!(
            r.text().await.unwrap(),
            format!(r#"{{"error":"stale lease","taskId":"{id}"}}"#)
        );
    }

    #[tokio::test]
    async fn queue_task_wire_shape_is_camelcase_with_flattened_lease() {
        let s = boot(true).await;
        let (id, _fencing) = enqueue_and_claim(&s, "build", "wkr").await;
        let body = client()
            .get(format!("{}/tasks/{id}", s.base))
            .header("authorization", format!("Bearer {}", s.token))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        // camelCase columns + epoch-ms numbers
        assert!(body.contains(r#""maxAttempts":5"#));
        assert!(body.contains(r#""availableAt":"#));
        assert!(body.contains(r#""createdAt":"#));
        // FLATTENED lease fields (claimedBy/fencingToken/visibilityDeadline) — never a nested object
        assert!(body.contains(r#""claimedBy":"wkr""#));
        assert!(body.contains(r#""fencingToken":1"#));
        assert!(body.contains(r#""visibilityDeadline":"#));
        assert!(!body.contains(r#""lease""#)); // not nested
        assert!(!body.contains("claimed_by")); // not snake_case
    }

    #[tokio::test]
    async fn queue_nack_requeue_then_extend_heartbeat() {
        let s = boot(true).await;
        let (id, fencing) = enqueue_and_claim(&s, "build", "wkr").await;
        // extend (heartbeat) bumps the visibility deadline
        let ext: Value = post(
            &s,
            &format!("/tasks/{id}/extend"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"extraMs":5000}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(ext["ok"], json!(true));
        assert!(ext["visibilityDeadline"].is_number());
        // nack(requeue=true) returns it to queued
        let nack: Value = post(
            &s,
            &format!("/tasks/{id}/nack"),
            &s.token,
            &format!(r#"{{"fencingToken":{fencing},"requeue":true,"error":"boom"}}"#),
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(nack, json!({ "ok": true, "state": "queued" }));
    }

    #[tokio::test]
    async fn queue_enqueue_requires_auth_401() {
        let s = boot(true).await;
        let r = client()
            .post(format!("{}/queues/build/tasks", s.base))
            .header("content-type", "application/json")
            .body(r#"{"payload":"x"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status().as_u16(), 401);
        assert_eq!(r.text().await.unwrap(), r#"{"error":"unauthorized"}"#);
    }

    #[tokio::test]
    async fn queue_scoped_token_is_gated_to_its_queue() {
        let s = boot(true).await;
        // master mints a token scoped to queue "build" only
        let minted: Value = post(
            &s,
            "/tokens",
            &s.token,
            r#"{"scope":{"queueIds":["build"]}}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        assert_eq!(minted["ok"], json!(true));
        assert_eq!(minted["scope"], json!({ "queueIds": ["build"] }));
        let scoped = minted["token"].as_str().unwrap().to_string();
        // it CAN enqueue to its own queue
        let ok = post(&s, "/queues/build/tasks", &scoped, r#"{"payload":"x"}"#).await;
        assert_eq!(ok.status().as_u16(), 200);
        // but a foreign queue is 403
        let denied = post(&s, "/queues/deploy/tasks", &scoped, r#"{"payload":"x"}"#).await;
        assert_eq!(denied.status().as_u16(), 403);
        assert_eq!(
            denied.text().await.unwrap(),
            r#"{"error":"queue out of scope","queue":"deploy"}"#
        );
    }

    /// Put `queue` into the stranded state: one claimed task, boot recovery requeues it
    /// (no holder), nobody claims, and the grace elapses.
    fn strand(s: &Server, queue: &str) {
        let mut wq = s.shared.work.lock().unwrap();
        wq.enqueue(queue, "x", crate::control::work::EnqueueOpts::default(), 0);
        wq.claim(queue, "w", 60_000, 1).unwrap();
        wq.recover_in_flight_with(10, &|_, _| false);
        assert_eq!(wq.check_stranded(10 + 120_000, 120_000).len(), 1);
    }

    #[tokio::test]
    async fn queues_overview_stranded_respects_queue_scope() {
        let s = boot(true).await;
        strand(&s, "build");
        strand(&s, "deploy");
        let get = |tok: String| {
            let url = format!("{}/queues", s.base);
            async move {
                client()
                    .get(url)
                    .header("authorization", format!("Bearer {tok}"))
                    .send()
                    .await
                    .unwrap()
                    .json::<Value>()
                    .await
                    .unwrap()
            }
        };
        // master sees both, each with its stranded report
        let all = get(s.token.clone()).await;
        let qs = all["queues"].as_array().unwrap();
        assert_eq!(qs.len(), 2);
        assert_eq!(qs[0]["stranded"]["tasks"], json!(1));
        assert_eq!(qs[0]["stranded"]["since"], json!(10));
        // a token scoped to "build" sees only its own entry (and its stranded info)
        let minted: Value = post(
            &s,
            "/tokens",
            &s.token,
            r#"{"scope":{"queueIds":["build"]}}"#,
        )
        .await
        .json()
        .await
        .unwrap();
        let scoped = get(minted["token"].as_str().unwrap().to_string()).await;
        let qs = scoped["queues"].as_array().unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0]["queue"], json!("build"));
        assert!(qs[0]["stranded"].is_object());
        assert!(!scoped.to_string().contains("deploy"));
    }
}
