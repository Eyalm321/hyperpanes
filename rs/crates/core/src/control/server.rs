//! The control server: bind `127.0.0.1` on an EPHEMERAL port, build the axum router (routes +
//! the `/events` WS upgrade with token auth via header or `?token=`), and write the `control.json`
//! discovery file under `persistence::paths` — exact fields + 2-space pretty JSON:
//! `{ port, token, pid, version, events: "ws://127.0.0.1:<port>/events?token=<token>" }` —
//! removing it on shutdown. Owns/holds the `SessionManager` + `readmodel` + `tokens` + inbox +
//! locks. Toggled by control-settings (enabled / allowInput), default OFF.
//!
//! This is the single place the cores meet at runtime. It also runs the central activity ticker
//! (busy⇄idle flips → `activity` frames, computed from `session_manager.last_output_at` at the
//! `idleAlertSeconds` threshold) and forwards live `SessionEvent`s to the read-model + WS clients.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::control::events::{ControlEvent, EventHub};
use crate::control::inbox::MessageInbox;
use crate::control::lock::PaneLocks;
use crate::control::readmodel::{Activity, PaneInfo, PaneRef, PaneStatus, ReadModel};
use crate::control::routes;
use crate::control::tokens::{random_token, TokenStore};
use crate::session_manager::{SessionEvent, SessionManager};

/// The renderer idle threshold (`useSettings.idleAlertSeconds`, default 10s): a running pane with
/// no pty output for at least this long reads as `idle`, else `busy`.
pub const IDLE_THRESHOLD_MS: i64 = 10_000;

/// How often the activity ticker re-evaluates liveness and fans out busy⇄idle flips.
const ACTIVITY_TICK_MS: u64 = 500;

/// Coalescing window for structure-only `state` pings (TS `notifyState`).
const STATE_COALESCE_MS: u64 = 100;

/// Everything the running control server owns, behind cheap component locks. Cloned-shared via
/// `Arc` into every axum handler + the background tasks.
pub struct Shared {
    pub model: Mutex<ReadModel>,
    pub tokens: Mutex<TokenStore>,
    pub inbox: Mutex<MessageInbox>,
    pub locks: Mutex<PaneLocks>,
    pub events: EventHub,
    pub sessions: Arc<SessionManager>,
    pub allow_input: AtomicBool,
    pub pid: u32,
    pub version: String,
    pub port: AtomicU16,
    pub idle_threshold_ms: i64,
    pub control_file: PathBuf,
    state_scheduled: AtomicBool,
}

impl Shared {
    /// Build the shared state. `allow_input` mirrors control-settings; `control_file` is both the
    /// path written on start and the `HYPERPANES_CONTROL_FILE` injected into spawned panes.
    pub fn new(
        sessions: Arc<SessionManager>,
        allow_input: bool,
        version: impl Into<String>,
        control_file: PathBuf,
    ) -> Arc<Self> {
        Arc::new(Shared {
            model: Mutex::new(ReadModel::new()),
            tokens: Mutex::new(TokenStore::new()),
            inbox: Mutex::new(MessageInbox::new()),
            locks: Mutex::new(PaneLocks::new()),
            events: EventHub::new(),
            sessions,
            allow_input: AtomicBool::new(allow_input),
            pid: std::process::id(),
            version: version.into(),
            port: AtomicU16::new(0),
            idle_threshold_ms: IDLE_THRESHOLD_MS,
            control_file,
            state_scheduled: AtomicBool::new(false),
        })
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    pub fn allow_input(&self) -> bool {
        self.allow_input.load(Ordering::SeqCst)
    }

    /// Resolve a pane's liveness from the session manager + the idle threshold. Mirrors the TS
    /// `status==='exited' ? 'exited' : idle ? 'idle' : 'busy'` with idle = "no output for the
    /// threshold"; a never-output pane reads `busy` (the renderer's `markActivity` only fires on
    /// output).
    pub fn compute_activity(&self, pane: &PaneInfo) -> Activity {
        self.activity_for(&pane.session_uid, pane.status)
    }

    fn activity_for(&self, uid: &str, status: PaneStatus) -> Activity {
        if status == PaneStatus::Exited {
            return Activity::Exited;
        }
        match self.sessions.last_output_at(uid) {
            Some(t) if now_ms() - (t as i64) >= self.idle_threshold_ms => Activity::Idle,
            _ => Activity::Busy,
        }
    }
}

/// Current epoch-ms (the TS `Date.now()`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Coalesce structure changes into one "re-fetch /state" ping per ~100ms tick (TS `notifyState`).
pub fn notify_state(shared: &Arc<Shared>) {
    if !shared.events.has_clients() {
        return;
    }
    // First caller in the window arms the timer; the rest are no-ops.
    if shared
        .state_scheduled
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let shared = Arc::clone(shared);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(STATE_COALESCE_MS)).await;
        shared.state_scheduled.store(false, Ordering::SeqCst);
        shared.events.broadcast(&ControlEvent::State);
    });
}

/// The control.json discovery shape — field order + 2-space pretty JSON match TS `writeDiscovery`.
#[derive(Serialize)]
struct Discovery<'a> {
    port: u16,
    token: &'a str,
    pid: u32,
    version: &'a str,
    events: String,
}

/// Build the WS events URL for a port + token (also used by `POST /tokens`).
pub fn events_url(port: u16, token: &str) -> String {
    format!("ws://127.0.0.1:{port}/events?token={token}")
}

fn write_discovery(shared: &Arc<Shared>) -> io::Result<()> {
    let port = shared.port();
    let token = shared.tokens.lock().unwrap().master().map(str::to_string);
    let Some(token) = token else {
        return Ok(());
    };
    let discovery = Discovery {
        port,
        token: &token,
        pid: shared.pid,
        version: &shared.version,
        events: events_url(port, &token),
    };
    let json = serde_json::to_string_pretty(&discovery)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    crate::persistence::paths::write_atomic(&shared.control_file, json.as_bytes())
}

/// Remove the discovery file (best-effort), so a stale `control.json` never points at a dead port.
pub fn remove_discovery(shared: &Arc<Shared>) {
    let _ = std::fs::remove_file(&shared.control_file);
}

/// Bind loopback on an ephemeral port, mint the master token, write `control.json`, start the
/// activity ticker, and serve until the process exits. Never returns under normal operation.
pub async fn run_server(shared: Arc<Shared>) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    shared.port.store(port, Ordering::SeqCst);

    shared.tokens.lock().unwrap().set_master(random_token());
    write_discovery(&shared)?;

    tokio::spawn(run_activity_ticker(Arc::clone(&shared)));

    let app = routes::router(Arc::clone(&shared));
    axum::serve(listener, app).await
}

/// Recompute each pane's activity every tick and broadcast a scope-filtered `activity` frame on
/// each flip of a KNOWN pane (a freshly-seen pane seeds its baseline silently — it rides the
/// `state` ping). A pure busy⇄idle flip does NOT trigger a `state` ping (TS #13).
async fn run_activity_ticker(shared: Arc<Shared>) {
    let mut interval = tokio::time::interval(Duration::from_millis(ACTIVITY_TICK_MS));
    let mut last: HashMap<String, Activity> = HashMap::new();
    loop {
        interval.tick().await;
        let panes: Vec<PaneRef> = shared.model.lock().unwrap().panes();
        let mut seen = std::collections::HashSet::new();
        for pr in &panes {
            seen.insert(pr.pane_id.clone());
            let act = shared.activity_for(&pr.session_uid, pr.status);
            let prev = last.get(&pr.pane_id).copied();
            if prev != Some(act) {
                // Only emit on a flip of an already-tracked pane while someone is streaming.
                if prev.is_some() && shared.events.has_clients() {
                    shared.events.broadcast_for_pane(
                        Some(&pr.coords),
                        &ControlEvent::Activity {
                            pane_id: pr.pane_id.clone(),
                            activity: act.as_str().to_string(),
                        },
                    );
                }
                last.insert(pr.pane_id.clone(), act);
            }
        }
        last.retain(|id, _| seen.contains(id));
    }
}

/// Apply one live `SessionEvent` to the read-model and fan out the matching `/events` frame.
/// `note_output` (byte cursor + last_output_at) is already done inside `SessionManager` BEFORE
/// any subscriber guard, so `since`/`waitForIdle` work with zero clients (the ordering invariant).
pub fn process_session_event(shared: &Arc<Shared>, ev: SessionEvent) {
    match ev {
        SessionEvent::Data { uid, data } => {
            if !shared.events.has_clients() {
                return;
            }
            let (pane_id, coords) = {
                let m = shared.model.lock().unwrap();
                let pid = m.uid_to_pane(&uid);
                let coords = pid.as_deref().and_then(|p| m.coords_of(p));
                (pid, coords)
            };
            shared.events.broadcast_for_pane(
                coords.as_ref(),
                &ControlEvent::Output { session_uid: uid, pane_id, data },
            );
        }
        SessionEvent::Cwd { uid, cwd } => {
            shared.model.lock().unwrap().set_cwd(&uid, &cwd);
        }
        SessionEvent::Exit { uid, code } => {
            let marked = shared.model.lock().unwrap().mark_exited(&uid, code);
            match marked {
                Some((pane_id, coords)) => {
                    shared.events.broadcast_for_pane(
                        Some(&coords),
                        &ControlEvent::Exit {
                            session_uid: uid,
                            pane_id: Some(pane_id.clone()),
                            code,
                        },
                    );
                    shared.events.broadcast_for_pane(
                        Some(&coords),
                        &ControlEvent::Activity { pane_id, activity: "exited".to_string() },
                    );
                    notify_state(shared);
                }
                None => {
                    if shared.events.has_clients() {
                        shared.events.broadcast_for_pane(
                            None,
                            &ControlEvent::Exit { session_uid: uid, pane_id: None, code },
                        );
                    }
                }
            }
        }
    }
}

/// Drain the session-event channel forever, applying each event. (app.rs uses this when it has no
/// extra taps; otherwise it runs its own loop calling [`process_session_event`].)
pub async fn forward_session_events(shared: Arc<Shared>, mut rx: UnboundedReceiver<SessionEvent>) {
    while let Some(ev) = rx.recv().await {
        process_session_event(&shared, ev);
    }
}

/// Test/embedding helper: build a `Shared` with a fresh engine, install `master_token`, and serve
/// the router on an ephemeral loopback port in a background thread (with its own runtime). Returns
/// the shared state (so the caller can seed the read-model) + the bound port. Session events are
/// drained. Used by the std-only `tests/control_parity.rs` integration test and embedders that
/// want a control server without driving the full `app::run` loop.
pub fn serve_for_test(
    control_file: PathBuf,
    allow_input: bool,
    master_token: &str,
) -> io::Result<(Arc<Shared>, u16)> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SessionEvent>();
    let sessions = Arc::new(SessionManager::new(tx));
    let shared = Shared::new(sessions, allow_input, env!("CARGO_PKG_VERSION"), control_file);
    shared.tokens.lock().unwrap().set_master(master_token.to_string());

    let std_listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = std_listener.local_addr()?.port();
    shared.port.store(port, Ordering::SeqCst);

    let serve_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("control-test-server".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            rt.block_on(async move {
                tokio::spawn(async move { while rx.recv().await.is_some() {} });
                if std_listener.set_nonblocking(true).is_err() {
                    return;
                }
                let Ok(listener) = tokio::net::TcpListener::from_std(std_listener) else {
                    return;
                };
                let _ = axum::serve(listener, routes::router(serve_shared)).await;
            });
        })?;

    Ok((shared, port))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::readmodel::{TabInfo, WindowInfo};
    use tokio::sync::mpsc::unbounded_channel;

    fn shared_with_pane() -> (Arc<Shared>, String) {
        let (tx, _rx) = unbounded_channel();
        let sm = Arc::new(SessionManager::new(tx));
        let shared = Shared::new(sm, true, "0.0.0-test", std::env::temp_dir().join("control-test.json"));
        let pane = PaneInfo {
            id: "p1".into(),
            session_uid: "u1".into(),
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
        };
        shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo { id: "t1".into(), title: "Tab".into(), layout: "auto".into(), panes: vec![pane] }],
        });
        (shared, "u1".to_string())
    }

    #[test]
    fn activity_is_busy_for_a_never_output_running_pane() {
        let (shared, _uid) = shared_with_pane();
        let pane = shared.model.lock().unwrap().pane("p1").unwrap().clone();
        // No session/output ⇒ busy (matches useIdle: markActivity only fires on output).
        assert_eq!(shared.compute_activity(&pane), Activity::Busy);
    }

    #[test]
    fn discovery_url_shape() {
        assert_eq!(
            events_url(54321, "abc"),
            "ws://127.0.0.1:54321/events?token=abc"
        );
    }

    #[tokio::test]
    async fn exit_event_marks_pane_and_fans_out() {
        let (shared, uid) = shared_with_pane();
        let (_id, mut rx) = shared.events.add_client(None);
        process_session_event(&shared, SessionEvent::Exit { uid: uid.clone(), code: 7 });
        // The pane is now exited in the model.
        assert_eq!(shared.model.lock().unwrap().pane("p1").unwrap().status, PaneStatus::Exited);
        // Two pane-addressed frames went out: exit then activity:exited.
        let f1 = rx.try_recv().unwrap();
        assert!(f1.contains(r#""type":"exit""#) && f1.contains(r#""code":7"#));
        let f2 = rx.try_recv().unwrap();
        assert!(f2.contains(r#""type":"activity""#) && f2.contains(r#""activity":"exited""#));
    }
}
