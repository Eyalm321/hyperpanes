//! Ambient-AI bridge (Phase 5): runs the ported `core::ai::service::AiService` engine
//! in-process and wires it to the GUI **without ever blocking the Slint UI thread**.
//!
//! `AiService` is `!Send` (it holds `Rc`/`RefCell` internally), so — exactly like the
//! headless `core::app` daemon does — it lives on its **own OS thread** with a private
//! current-thread Tokio runtime. The UI thread talks to it over channels:
//!
//!   * UI → engine: [`AiMsg`] (live pty data / cwd / session-exit taps, the per-window
//!     pane-context publish, and the Preferences enable/configure controls).
//!   * engine → UI: [`MetaUpdate`] (a produced `ai.subtitle` for a pane) and [`AiStatus`]
//!     (running / online / last-error, for the Preferences status line).
//!
//! The engine is **default-OFF**: it loads `ai-settings.json` on its thread and does
//! nothing until enabled from Preferences. The bridge gates the heavy per-pane data tap on
//! the enabled flag so the default path stays effectively zero-cost.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Instant;

use hyperpanes_core::ai::ollama::{OllamaClient, OllamaConfig};
use hyperpanes_core::ai::service::{
    AiPanePublish, AiProjectRef, AiService, AiSettings, AiSettingsPatch, AiStatus, JobOutcome,
    JobStep, OnStatus, PushMeta,
};
use hyperpanes_core::persistence::paths;

use tokio::sync::mpsc::{
    channel, unbounded_channel, Receiver, Sender, UnboundedReceiver, UnboundedSender,
};

/// Capacity of the bounded UI→engine *data* tap. Each send is a full
/// rendered-screen clone, produced only on change + debounced in `pump_ai`. If
/// the engine stalls in a slow Ollama call these would otherwise pile up
/// unbounded, so the tap is `try_send` drop-on-full (the next debounce tick
/// re-feeds the latest screen anyway — latest-wins). Control messages take a
/// separate unbounded channel so a dropped data tap never loses a Mute/Disable.
const DATA_CHANNEL_CAP: usize = 256;

/// A message from the UI thread to the ambient-AI engine thread.
pub enum AiMsg {
    /// Live pty output for a session (fed to the pane buffer + quiescence scheduler).
    Data { uid: String, data: String },
    /// A pane's reported working directory + (if a git repo) its project.
    Cwd {
        uid: String,
        cwd: String,
        project: Option<AiProjectRef>,
    },
    /// A session exited — forget its buffer/context.
    Exit { uid: String },
    /// A window's current set of watched panes (label + mute), reconciled per window.
    PaneContext {
        window_id: i64,
        panes: Vec<AiPanePublish>,
    },
    /// A window closed — drop its panes.
    DropWindow { window_id: i64 },
    /// The Preferences master toggle.
    SetEnabled(bool),
    /// A Preferences field change (endpoint/model/cadence).
    Configure(AiSettingsPatch),
}

/// One produced `ai.subtitle` push, forwarded from the engine thread to the UI thread.
pub struct MetaUpdate {
    pub window_id: i64,
    pub pane_id: String,
    pub pairs: Vec<(String, String)>,
}

/// The UI-thread handle to the ambient-AI engine thread: a sender for taps/controls plus
/// the two receivers the UI pump drains each tick. Holds the latest [`AiStatus`] so the
/// Preferences dialog can render it, and caches the enabled flag so the (hot) data tap can
/// skip sending when the engine is off.
pub struct AiBridge {
    tx: UnboundedSender<AiMsg>,
    data_tx: Sender<AiMsg>,
    meta_rx: RefCell<UnboundedReceiver<MetaUpdate>>,
    status_rx: RefCell<UnboundedReceiver<AiStatus>>,
    status: RefCell<AiStatus>,
    enabled: Cell<bool>,
}

impl AiBridge {
    /// Spawn the engine thread (default-OFF) and return its UI-side handle.
    pub fn spawn() -> AiBridge {
        let (tx, rx) = unbounded_channel::<AiMsg>();
        let (data_tx, data_rx) = channel::<AiMsg>(DATA_CHANNEL_CAP);
        let (meta_tx, meta_rx) = unbounded_channel::<MetaUpdate>();
        let (status_tx, status_rx) = unbounded_channel::<AiStatus>();

        let settings_path = paths::ai_settings_json();
        let memory_path = paths::ai_memory_json();
        std::thread::Builder::new()
            .name("ambient-ai".to_string())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build()
                else {
                    return;
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&rt, async move {
                    ai_loop(settings_path, memory_path, rx, data_rx, meta_tx, status_tx).await;
                });
            })
            .ok();

        AiBridge {
            tx,
            data_tx,
            meta_rx: RefCell::new(meta_rx),
            status_rx: RefCell::new(status_rx),
            status: RefCell::new(AiStatus {
                enabled: false,
                online: false,
                endpoint: AiSettings::default().endpoint,
                model: AiSettings::default().model,
                last_error: None,
            }),
            enabled: Cell::new(false),
        }
    }

    /// Whether the engine is currently enabled (cached from the latest status). Gates the
    /// per-tick rendered-screen scan so the default-OFF path does no work.
    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    /// Send a message to the engine thread (no-op if the thread has gone).
    pub fn send(&self, msg: AiMsg) {
        let _ = self.tx.send(msg);
    }

    /// Send a live-data tap only when the engine is enabled (keeps the default-OFF path cheap).
    /// Bounded + `try_send`: if the engine is stalled (e.g. a slow Ollama job) the tap is
    /// dropped rather than queued unbounded — the next debounced `pump_ai` tick re-feeds the
    /// latest screen, so dropping is latest-wins, not data loss.
    pub fn feed_data(&self, uid: &str, data: &str) {
        if self.enabled.get() {
            let _ = self.data_tx.try_send(AiMsg::Data {
                uid: uid.to_string(),
                data: data.to_string(),
            });
        }
    }

    /// Drain any produced subtitle pushes the UI pump should apply this tick.
    pub fn drain_meta(&self) -> Vec<MetaUpdate> {
        let mut out = Vec::new();
        let mut rx = self.meta_rx.borrow_mut();
        while let Ok(u) = rx.try_recv() {
            out.push(u);
        }
        out
    }

    /// Drain status transitions, updating the cached status + enabled flag. Returns `true`
    /// if anything changed (so the caller can refresh the Preferences projection).
    pub fn drain_status(&self) -> bool {
        let mut changed = false;
        let mut rx = self.status_rx.borrow_mut();
        while let Ok(st) = rx.try_recv() {
            self.enabled.set(st.enabled);
            *self.status.borrow_mut() = st;
            changed = true;
        }
        changed
    }

    /// The latest status (for the Preferences dialog).
    pub fn status(&self) -> AiStatus {
        self.status.borrow().clone()
    }
}

/// The engine's own event loop, running on its dedicated current-thread runtime. Mirrors
/// `core::app::ai_loop`, extended with the pane-context publish + the enable/configure
/// controls the GUI Preferences drives.
async fn ai_loop(
    settings_path: PathBuf,
    memory_path: PathBuf,
    mut rx: UnboundedReceiver<AiMsg>,
    mut data_rx: Receiver<AiMsg>,
    meta_tx: UnboundedSender<MetaUpdate>,
    status_tx: UnboundedSender<AiStatus>,
) {
    let defaults = AiSettings::default();
    let client = OllamaClient::new(OllamaConfig {
        endpoint: defaults.endpoint.clone(),
        model: defaults.model.clone(),
        timeout_ms: None,
    });
    let push_meta: PushMeta = Box::new(move |window_id, pane_id, pairs| {
        let _ = meta_tx.send(MetaUpdate {
            window_id,
            pane_id: pane_id.to_string(),
            pairs,
        });
    });
    let on_status: OnStatus = Box::new(move |st| {
        let _ = status_tx.send(st.clone());
    });
    let mut ai = AiService::new(settings_path, memory_path, client, push_meta, on_status);
    ai.init(); // default-off ⇒ a no-op until enabled in ai-settings.json

    // Off-loop completion reports. The HTTP calls (summaries + reachability pings) are
    // `spawn_local`'d onto this LocalSet so a slow/hung Ollama call (up to 12s) NEVER
    // blocks the `select!`: incoming taps and Mute/Disable/Configure stay serviced, and a
    // Mute that lands mid-summary takes effect at once (the stale result is just applied —
    // or, for a now-muted pane, harmlessly dropped — when it finally lands).
    let (done_tx, mut done_rx) = unbounded_channel::<JobOutcome>();
    let (ping_tx, mut ping_rx) = unbounded_channel::<bool>();

    let start = Instant::now();
    let mut last_ms: i64 = 0;
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(100));
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(AiMsg::Cwd { uid, cwd, project }) => ai.on_cwd(&uid, &cwd, project),
                Some(AiMsg::Exit { uid }) => ai.on_session_exit(&uid),
                Some(AiMsg::PaneContext { window_id, panes }) => ai.on_pane_context(window_id, &panes),
                Some(AiMsg::DropWindow { window_id }) => ai.drop_window(window_id),
                Some(AiMsg::SetEnabled(on)) => {
                    ai.set_enabled(on);
                    spawn_ping(&ai, &ping_tx);
                }
                Some(AiMsg::Configure(patch)) => {
                    ai.configure(patch);
                    spawn_ping(&ai, &ping_tx);
                }
                Some(AiMsg::Data { .. }) => {} // data flows on `data_rx`; ignore here
                None => break, // UI side dropped — shut the engine down
            },
            maybe = data_rx.recv() => match maybe {
                Some(AiMsg::Data { uid, data }) => ai.on_data(&uid, &data),
                Some(_) => {}
                // The data and control channels share the bridge's lifetime, so a closed
                // data channel means the UI is gone — fall through to the same shutdown.
                None => break,
            },
            Some(outcome) = done_rx.recv() => ai.finish_job(outcome),
            Some(ok) = ping_rx.recv() => ai.apply_ping(ok),
            _ = ticker.tick() => {
                let now = start.elapsed().as_millis() as i64;
                ai.tick(now - last_ms);
                last_ms = now;
                while let Some(uid) = ai.next_due() {
                    match ai.prepare_job(&uid) {
                        // Skip/dedup decided synchronously — report straight back.
                        JobStep::Done(result) => {
                            crate::dbg_log(&format!("[ai] job uid={uid} -> {result:?} (no call)"));
                            ai.complete_job(&uid, result);
                        }
                        // Real Ollama call: run it off-loop and report via `done_rx`. The
                        // scheduler already marked the uid in-flight, so concurrency is
                        // honoured; `finish_job` calls `complete_job` when it lands.
                        JobStep::Run(job) => {
                            let done_tx = done_tx.clone();
                            tokio::task::spawn_local(async move {
                                let _ = done_tx.send(job.run().await);
                            });
                        }
                    }
                }
            }
        }
    }
}

/// Spawn an off-loop reachability ping (up to 3s) so enable/configure never block the
/// control loop; the result lands on `ping_tx` and is applied via `AiService::apply_ping`.
fn spawn_ping(ai: &AiService<OllamaClient>, ping_tx: &UnboundedSender<bool>) {
    let client = ai.ping_client();
    let ping_tx = ping_tx.clone();
    tokio::task::spawn_local(async move {
        let _ = ping_tx.send(client.ping().await);
    });
}
