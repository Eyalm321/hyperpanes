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
use crate::control::output::{
    detect_awaiting_input, next_poll_delay, slice_since, wait_decision, WaitVerdict,
    DEFAULT_SETTLE_MS, DEFAULT_WAIT_TIMEOUT_MS,
};
use crate::control::readmodel::PaneStatus;
use crate::control::scope::{check_mintable, coerce_scope, pane_in_scope, queue_in_scope, Scope};
use crate::control::server::{events_url, notify_state, now_ms, Shared};
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

/// A resolved, in-scope pane: its session uid. 404 (no such pane) / 403 (out of scope).
struct FoundPane {
    uid: String,
}

// pre-existing; deferred per repo lint policy (test.yml)
#[allow(clippy::result_large_err)]
fn find_pane_scoped(
    shared: &Arc<Shared>,
    scope: Option<&Scope>,
    pane_id: &str,
) -> Result<FoundPane, Response> {
    let m = shared.model.lock().unwrap();
    match m.coords_of(pane_id) {
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
                .pane(pane_id)
                .map(|p| p.session_uid.clone())
                .unwrap_or_default();
            Ok(FoundPane { uid })
        }
    }
}

// ---- /health ------------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthOut {
    ok: bool,
    app: &'static str,
    pid: u32,
    version: String,
    allow_input: bool,
}

async fn health(State(shared): State<Arc<Shared>>) -> Response {
    ok_json(HealthOut {
        ok: true,
        app: "hyperpanes",
        pid: shared.pid,
        version: shared.version.clone(),
        allow_input: shared.allow_input(),
    })
}

// ---- /state -------------------------------------------------------------------------------

async fn state(State(shared): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let info = match authorize(&shared, &headers) {
        Ok(i) => i,
        Err(e) => return e,
    };
    let m = shared.model.lock().unwrap();
    let out = m.state_for_scope_with_dims(
        info.scope.as_ref(),
        &|p| shared.compute_activity(p),
        &|p| shared.sessions.dims(&p.session_uid),
    );
    ok_json(out)
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
    match shared.sessions.render_screen(uid) {
        Some(full) => {
            // Prompt detection runs on the FULL screen so a clipping `tail` can't hide a blocked
            // prompt above the tail window.
            let awaiting = detect_awaiting_input(&full);
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
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let after = non_neg_num(q.get("after"))
        .filter(|&a| a > 0)
        .map(|a| a as u64)
        .unwrap_or(0);
    let inbox = shared.inbox.lock().unwrap();
    let out = MessagesOut {
        messages: inbox.read(&id, after),
        dropped: inbox.dropped_count(&id),
        latest_seq: inbox.latest_seq(&id),
        pane_id: id,
    };
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
    if let Err(e) = find_pane_scoped(&shared, info.scope.as_ref(), &id) {
        return e;
    }
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let from = b
        .get("from")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let msg_body = match b.get("body").and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None => return jstatus(400, json!({ "error": "expected { from?, body: string }" })),
    };
    let msg = shared
        .inbox
        .lock()
        .unwrap()
        .post(&id, &from, &msg_body, now_ms());
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
    jstatus(200, json!({ "ok": true, "seq": msg.seq }))
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
                json!({ "error": "expected { payload: string, kind?, title?, priority?, maxAttempts?, visibilityTimeoutMs?, delayMs?|availableAt?, dedupeKey? }" }),
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
                json!({ "error": "expected { worker: string, leaseMs?, count? }" }),
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
    let now = now_ms();
    let mut tasks = Vec::new();
    {
        let mut wq = shared.work.lock().unwrap();
        for _ in 0..count {
            match wq.claim(&queue, &worker, lease_ms, now) {
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
            return jstatus(403, serde_json::json!({"error": "restartApp needs a root token"}));
        }
        let scope = match cmd.get("scope").and_then(Value::as_str).unwrap_or("gui") {
            "gui" => 1u8,
            "full" => 2u8,
            other => return jstatus(400, serde_json::json!({"error": format!("unknown scope: {other}")})),
        };
        if let (Some(sid), Some(text)) = (
            cmd.get("sessionId").and_then(Value::as_str),
            cmd.get("prompt").and_then(Value::as_str),
        ) {
            if let Err(e) = crate::resume_queue::enqueue(sid, text) {
                return jstatus(400, serde_json::json!({"error": e}));
            }
        }
        shared.restart_app.store(scope, std::sync::atomic::Ordering::SeqCst);
        return jstatus(200, serde_json::json!({"ok": true, "scope": if scope == 1 {"gui"} else {"full"}}));
    }
    let control_file = shared.control_file.to_str().map(str::to_string);
    let result = {
        let mut m = shared.model.lock().unwrap();
        dispatch::handle_command(
            &mut m,
            &shared.sessions,
            control_file.as_deref(),
            info.scope.as_ref(),
            &cmd,
        )
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
    use super::router;
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
        let shared = Shared::new(sessions, allow_input, "0.1.8", control_file);
        shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab 1".into(),
                layout: "auto".into(),
                panes: vec![],
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
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
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
            .get(format!("{}/fs/read?path={}/nope.txt", s.base, dir.display()))
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
                r#"{{"ok":true,"app":"hyperpanes","pid":{pid},"version":"0.1.8","allowInput":true}}"#
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
        assert!(r.text().await.unwrap().contains(r#""allowInput":false"#));
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
        assert_eq!(
            r.text().await.unwrap(),
            r#"{"windows":[{"windowId":1,"activeTabId":"t1","tabs":[{"id":"t1","title":"Tab 1","layout":"auto","panes":[]}]}]}"#
        );
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
        assert_eq!(post.text().await.unwrap(), r#"{"ok":true,"seq":1}"#);
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
}
