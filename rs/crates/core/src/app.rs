//! The headless app wiring — the one place every subsystem meets. Builds the central
//! `SessionManager`; the `AiService` (default-off, settings/memory paths from
//! `persistence::paths`, run on its own thread because it is `!Send`); and the `ControlServer`
//! (discovery file from `persistence::paths`, gated by `persistence::control_settings`). Runs the
//! `single_instance` gate (headless-salted so it never collides with the Electron app) and routes
//! a second-instance argv via `cli::routing`; resolves the launch workspace via `workspace::launch`.
//! Exposes `run()` for the headless daemon bin (and, later, for the Slint app to embed).
//!
//! Env overrides (for the MCP acceptance gate against an isolated userData dir):
//!   * `HYPERPANES_CONTROL_FILE`  — discovery file path (also injected into spawned panes).
//!   * `HYPERPANES_ALLOW_INPUT`   — `1`/`true`/`yes` forces `allowInput` on (else from settings).

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::ai::ollama::{OllamaClient, OllamaConfig};
use crate::ai::service::{AiService, AiSettings, OnStatus, PushMeta};
use crate::control::readmodel::{PaneInfo, PaneStatus, ReadModel, TabInfo, WindowInfo};
use crate::control::server::{self, now_ms, notify_state, Shared};
use crate::persistence::{control_settings, paths};
use crate::session::spawn::EnvMap;
use crate::session_manager::{SessionEvent, SessionManager, SpawnOptions};
use crate::single_instance::{self, HandoffMessage, Instance};
use crate::workspace::io::windows_of;
use crate::workspace::launch::resolve_launch_workspace;
use crate::workspace::model::{PaneSpec, WindowSpec};

/// Version stamped into `/health` + `control.json` (the Electron app's `package.json` version).
pub const VERSION: &str = "0.1.8";

/// Default pane frame color for seeded panes that don't specify one.
const DEFAULT_PANE_COLOR: &str = "#3b82f6";

/// One AI subtitle push, forwarded from the (`!Send`) AI thread to the control loop.
struct MetaUpdate {
    pane_id: String,
    pairs: Vec<(String, String)>,
}

/// Run the headless daemon: wire the engine + control server + AI + single-instance gate, seed the
/// launch workspace, and serve the loopback control API until the process exits.
pub async fn run() -> io::Result<()> {
    let control_file = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(paths::control_json);

    let settings = control_settings::load();
    let allow_input = settings.allow_input || env_truthy("HYPERPANES_ALLOW_INPUT");

    // The central engine. All PTYs live here; panes/windows just reference uids.
    let (etx, erx) = unbounded_channel::<SessionEvent>();
    let sessions = Arc::new(SessionManager::new(etx));

    let shared = Shared::new(Arc::clone(&sessions), allow_input, VERSION, control_file.clone());

    // Seed the read-model from the launch workspace (argv only — no last-session restore, so the
    // daemon starts clean and deterministic). Always leaves ≥1 window for open_pane to target.
    let argv: Vec<String> = std::env::args().collect();
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let file = resolve_launch_workspace(&argv, &cwd);
    let windows = windows_of(file.as_ref());
    seed_windows(&shared, windows, 1);

    // Ambient AI (default-off) on its own thread: it is `!Send` (Rc/RefCell), so it cannot share
    // the multi-thread runtime. It drains a tee of the session events; subtitle pushes flow back
    // over `meta_rx` and merge into the read-model.
    let (ai_tx, ai_rx) = unbounded_channel::<SessionEvent>();
    let (meta_tx, meta_rx) = unbounded_channel::<MetaUpdate>();
    spawn_ai_thread(paths::ai_settings_json(), paths::ai_memory_json(), ai_rx, meta_tx);
    {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move { apply_meta_updates(shared, meta_rx).await });
    }

    // Forward live session events to the control plane, teeing a clone to the AI thread.
    {
        let shared = Arc::clone(&shared);
        tokio::spawn(async move { forward_loop(shared, erx, ai_tx).await });
    }

    // Single-instance gate, headless-salted so it never collides with the Electron build's lock.
    // A second headless invocation forwards its argv (new windows) and exits.
    let salt = format!("{}-headless", single_instance::user_salt());
    match single_instance::acquire(&salt) {
        Ok(Instance::Secondary(sec)) => {
            let _ = sec.forward(&HandoffMessage { argv, cwd }).await;
            return Ok(());
        }
        Ok(Instance::Primary(primary)) => {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move {
                let _ = primary
                    .run_server(move |msg| handle_handoff(&shared, msg))
                    .await;
            });
        }
        Err(_) => { /* unsupported platform → run standalone (no gate) */ }
    }

    // The activity ticker is a separate task from `run_server` now (so the GUI host can abort it
    // independently); the headless daemon runs it for the whole process lifetime.
    tokio::spawn(server::run_activity_ticker(Arc::clone(&shared)));

    // Serve the loopback control API. (The headless daemon always serves — that is its purpose;
    // the enabled/allowInput toggles still gate input and are reflected in /health.)
    server::run_server(shared).await
}

fn env_truthy(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

// ---- read-model seeding -------------------------------------------------------------------

/// Seed the read-model with the given windows, spawning a pty per pane. If `windows` is empty,
/// seeds a single empty window so the daemon always has a target for `open_pane`.
fn seed_windows(shared: &Arc<Shared>, windows: Vec<WindowSpec>, start_id: i64) {
    let control_file = shared.control_file.to_str().map(str::to_string);
    let mut model = shared.model.lock().unwrap();
    if windows.is_empty() {
        let tab_id = new_id();
        model.add_window(WindowInfo {
            window_id: start_id,
            active_tab_id: Some(tab_id.clone()),
            tabs: vec![TabInfo { id: tab_id, title: "Tab 1".into(), layout: "auto".into(), panes: vec![] }],
        });
        return;
    }
    for (window_id, ws) in (start_id..).zip(windows) {
        let groups = if ws.groups.is_empty() {
            vec![Default::default()]
        } else {
            ws.groups
        };
        let active_index = ws.active.unwrap_or(0) as usize;
        let mut tabs = Vec::new();
        let mut active_tab_id = None;
        for (i, g) in groups.into_iter().enumerate() {
            let tab_id = new_id();
            if i == active_index {
                active_tab_id = Some(tab_id.clone());
            }
            let title = g.title.unwrap_or_else(|| "Tab".into());
            let layout = g.layout.unwrap_or_else(|| "auto".into());
            let panes = g
                .panes
                .iter()
                .map(|ps| spawn_seed_pane(&shared.sessions, control_file.as_deref(), ps))
                .collect();
            tabs.push(TabInfo { id: tab_id, title, layout, panes });
        }
        if active_tab_id.is_none() {
            active_tab_id = tabs.first().map(|t| t.id.clone());
        }
        model.add_window(WindowInfo { window_id, active_tab_id, tabs });
    }
}

/// Spawn a pty for a workspace `PaneSpec` and return its `PaneInfo` (not yet inserted).
fn spawn_seed_pane(sessions: &SessionManager, control_file: Option<&str>, ps: &PaneSpec) -> PaneInfo {
    let pane_id = new_id();
    let session_uid = new_id();
    let label = ps
        .label
        .clone()
        .or_else(|| ps.command.clone())
        .unwrap_or_else(|| "shell".to_string());
    let color = ps.color.clone().unwrap_or_else(|| DEFAULT_PANE_COLOR.to_string());
    let args = ps.args.clone().filter(|a| !a.is_empty());
    let opts = SpawnOptions {
        uid: session_uid.clone(),
        shell: ps.shell.clone(),
        args: args.clone(),
        command: ps.command.clone(),
        cwd: ps.cwd.clone(),
        env: None::<EnvMap>,
        cols: None,
        rows: None,
        pane_id: Some(pane_id.clone()),
        integration: None,
        control_file: control_file.map(str::to_string),
    };
    let _ = sessions.create(opts);
    PaneInfo {
        id: pane_id,
        session_uid,
        label,
        subtitle: None,
        color,
        command: ps.command.clone(),
        args,
        cwd: ps.cwd.clone(),
        shell: ps.shell.clone(),
        status: PaneStatus::Running,
        exit_code: None,
        meta: ps.meta.clone().filter(|m| !m.is_empty()),
    }
}

/// A second headless invocation: open its windows alongside the existing ones.
fn handle_handoff(shared: &Arc<Shared>, msg: HandoffMessage) {
    let si = crate::cli::routing::resolve_second_instance_windows(&msg.argv, &msg.cwd);
    let next_id = {
        let model = shared.model.lock().unwrap();
        model.first_window_id().map(|_| max_window_id(&model) + 1).unwrap_or(1)
    };
    seed_windows(shared, si.windows, next_id);
    notify_state(shared);
}

fn max_window_id(model: &ReadModel) -> i64 {
    model.panes().iter().map(|p| p.coords.window_id).max().unwrap_or(0)
}

// ---- live event plumbing ------------------------------------------------------------------

async fn forward_loop(
    shared: Arc<Shared>,
    mut erx: UnboundedReceiver<SessionEvent>,
    ai_tx: UnboundedSender<SessionEvent>,
) {
    while let Some(ev) = erx.recv().await {
        let _ = ai_tx.send(ev.clone());
        server::process_session_event(&shared, ev);
    }
}

async fn apply_meta_updates(shared: Arc<Shared>, mut rx: UnboundedReceiver<MetaUpdate>) {
    while let Some(u) = rx.recv().await {
        let mut patch: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (k, v) in u.pairs {
            patch.insert(k, Some(v));
        }
        let changed = shared.model.lock().unwrap().set_meta(&u.pane_id, &patch).is_some();
        if changed {
            notify_state(&shared);
        }
    }
}

// ---- ambient AI thread (!Send, so it owns its own current-thread runtime) ------------------

fn spawn_ai_thread(
    settings_path: PathBuf,
    memory_path: PathBuf,
    ai_rx: UnboundedReceiver<SessionEvent>,
    meta_tx: UnboundedSender<MetaUpdate>,
) {
    std::thread::Builder::new()
        .name("ambient-ai".to_string())
        .spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
                return;
            };
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, async move {
                ai_loop(settings_path, memory_path, ai_rx, meta_tx).await;
            });
        })
        .ok();
}

async fn ai_loop(
    settings_path: PathBuf,
    memory_path: PathBuf,
    mut ai_rx: UnboundedReceiver<SessionEvent>,
    meta_tx: UnboundedSender<MetaUpdate>,
) {
    let defaults = AiSettings::default();
    let client = OllamaClient::new(OllamaConfig {
        endpoint: defaults.endpoint.clone(),
        model: defaults.model.clone(),
        timeout_ms: None,
    });
    let push_meta: PushMeta = Box::new(move |_window_id, pane_id, pairs| {
        let _ = meta_tx.send(MetaUpdate { pane_id: pane_id.to_string(), pairs });
    });
    let on_status: OnStatus = Box::new(|_status| {});
    let mut ai = AiService::new(settings_path, memory_path, client, push_meta, on_status);
    ai.init(); // default-off ⇒ a no-op until enabled in ai-settings.json

    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    let mut last = now_ms();
    loop {
        tokio::select! {
            maybe = ai_rx.recv() => match maybe {
                Some(SessionEvent::Data { uid, data }) => ai.on_data(&uid, &data),
                Some(SessionEvent::Cwd { uid, cwd }) => ai.on_cwd(&uid, &cwd, None),
                Some(SessionEvent::Exit { uid, .. }) => ai.on_session_exit(&uid),
                // Phase-4 semantic markers are not part of the AI tap's input model.
                Some(_) => {}
                None => break,
            },
            _ = ticker.tick() => {
                let now = now_ms();
                ai.tick(now - last);
                last = now;
                while let Some(uid) = ai.next_due() {
                    let result = ai.run_job(&uid).await;
                    ai.complete_job(&uid, result);
                }
            }
        }
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
