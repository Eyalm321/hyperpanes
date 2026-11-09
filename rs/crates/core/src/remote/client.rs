//! HTTP + WebSocket client for a remote hyperpanes control server.
//!
//! Layers, bottom up:
//! 1. [`RemoteClient`] — stateless typed HTTP calls (`/state`, `/panes/{id}/output`,
//!    `/panes/{id}/input`). Token goes in `Authorization: Bearer`.
//! 2. [`RemoteSession`] — one background task owning the `/events` WebSocket: gapless
//!    attach, reconnect with capped exponential backoff + jitter, delta resync, state polling.
//!    Consumers read [`RemoteEvent`]s from the receiver returned by
//!    [`RemoteSession::start`].
//! 3. [`InputSender`] — per-pane ordered keystroke queue (one POST in flight, queued bytes
//!    coalesced into the next POST).
//!
//! The token never appears in `Debug`/`Display`/errors: reqwest errors are stripped of their
//! URL and every error string is scrubbed of the token before it leaves this module.
//!
//! Notes for embedders:
//! * Cursors are UTF-16 code units (JS `.length`); only ever compared numerically here.
//! * Input `data` is sent verbatim as the JSON string — no newline translation and no
//!   `submit` flag. (A Windows host itself collapses `\n`→`\r` in `data`; send `\r` for Enter.)
//! * The event channel is unbounded: drain it promptly.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

use super::pairing::{pct_encode, RemoteTarget};

// ---------------------------------------------------------------------------------------------
// Typed /state model (lenient: every field defaulted, unknown fields ignored)
// ---------------------------------------------------------------------------------------------

/// `GET /state`: windows → tabs → panes.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteState {
    pub windows: Vec<RemoteWindow>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteWindow {
    pub window_id: i64,
    pub active_tab_id: Option<String>,
    pub tabs: Vec<RemoteTab>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteTab {
    pub id: String,
    pub title: String,
    pub layout: String,
    pub panes: Vec<RemotePane>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemotePane {
    pub id: String,
    pub session_uid: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub color: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    /// `running` | `exited`.
    pub status: String,
    pub exit_code: Option<i32>,
    /// `busy` | `idle` | `exited` (legacy activity; precise liveness arrives as events).
    pub activity: String,
    /// Host grid. A terminal stream is only valid at the width it was produced for — emulate
    /// at exactly this size. `None` when the host doesn't report it.
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub meta: BTreeMap<String, String>,
}

impl RemotePane {
    pub fn exited(&self) -> bool {
        self.status == "exited"
    }
}

impl RemoteState {
    /// All panes, window/tab order.
    pub fn panes(&self) -> impl Iterator<Item = &RemotePane> {
        self.windows
            .iter()
            .flat_map(|w| w.tabs.iter())
            .flat_map(|t| t.panes.iter())
    }

    pub fn pane(&self, id: &str) -> Option<&RemotePane> {
        self.panes().find(|p| p.id == id)
    }
}

/// `GET /panes/{id}/output` result.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct OutputChunk {
    pub output: String,
    /// Monotonic UTF-16-code-unit cursor after `output`.
    pub cursor: u64,
    /// Only meaningful for `since` reads: the gap predates the host's replay buffer, so
    /// `output` is the full replay rather than a delta.
    pub truncated: bool,
    pub status: String,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// 401 — bad/revoked/expired token.
    Unauthorized,
    /// 403 — e.g. the host has `allowInput` off.
    Forbidden(String),
    /// 404 — no such pane.
    NotFound,
    /// Any other HTTP status.
    Status { code: u16, message: String },
    /// Connect/timeout/transport failure.
    Network(String),
    /// Response couldn't be decoded.
    Protocol(String),
}

impl RemoteError {
    /// HTTP status if this error came from a response (0 otherwise).
    pub fn status_code(&self) -> u16 {
        match self {
            RemoteError::Unauthorized => 401,
            RemoteError::Forbidden(_) => 403,
            RemoteError::NotFound => 404,
            RemoteError::Status { code, .. } => *code,
            _ => 0,
        }
    }
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Unauthorized => write!(f, "unauthorized (bad or revoked token)"),
            RemoteError::Forbidden(m) => write!(f, "forbidden: {m}"),
            RemoteError::NotFound => write!(f, "not found"),
            RemoteError::Status { code, message } => write!(f, "HTTP {code}: {message}"),
            RemoteError::Network(m) => write!(f, "network error: {m}"),
            RemoteError::Protocol(m) => write!(f, "protocol error: {m}"),
        }
    }
}

impl std::error::Error for RemoteError {}

fn scrub(msg: &str, token: &str) -> String {
    if token.is_empty() {
        return msg.to_string();
    }
    msg.replace(token, "***").replace(&pct_encode(token), "***")
}

// ---------------------------------------------------------------------------------------------
// RemoteClient (HTTP)
// ---------------------------------------------------------------------------------------------

/// Stateless typed HTTP client. Cheap to clone.
#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    base: String,
    token: Arc<str>,
}

impl fmt::Debug for RemoteClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteClient")
            .field("base", &self.base)
            .field("token", &"***")
            .finish()
    }
}

impl RemoteClient {
    pub fn new(target: &RemoteTarget) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .no_proxy() // the host is a peer on the VPN/LAN; never route via an HTTP proxy
            .build()
            .expect("reqwest client");
        Self {
            http,
            base: target.http_base(),
            token: Arc::from(target.token()),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn net_err(&self, e: reqwest::Error) -> RemoteError {
        RemoteError::Network(scrub(&e.without_url().to_string(), &self.token))
    }

    async fn check(&self, resp: reqwest::Response) -> Result<reqwest::Response, RemoteError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let body = resp.text().await.unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
            .unwrap_or_else(|| body.chars().take(200).collect());
        let msg = scrub(&msg, &self.token);
        Err(match status.as_u16() {
            401 => RemoteError::Unauthorized,
            403 => RemoteError::Forbidden(msg),
            404 => RemoteError::NotFound,
            code => RemoteError::Status { code, message: msg },
        })
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T, RemoteError> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&*self.token)
            .send()
            .await
            .map_err(|e| self.net_err(e))?;
        let resp = self.check(resp).await?;
        resp.json::<T>()
            .await
            .map_err(|e| RemoteError::Protocol(scrub(&e.without_url().to_string(), &self.token)))
    }

    async fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<(), RemoteError> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&*self.token)
            .json(body)
            .send()
            .await
            .map_err(|e| self.net_err(e))?;
        self.check(resp).await.map(|_| ())
    }

    pub async fn get_state(&self) -> Result<RemoteState, RemoteError> {
        self.get_json("/state").await
    }

    /// Full replay (`since: None`) or the delta since `since`.
    pub async fn get_output(
        &self,
        pane_id: &str,
        since: Option<u64>,
    ) -> Result<OutputChunk, RemoteError> {
        let mut path = format!("/panes/{}/output", pct_encode(pane_id));
        if let Some(s) = since {
            path.push_str(&format!("?since={s}"));
        }
        self.get_json(&path).await
    }

    /// POST raw input. NOT ordered across concurrent calls — use [`InputSender`] for typing.
    pub async fn send_input(&self, pane_id: &str, data: &str) -> Result<(), RemoteError> {
        self.post_json(
            &format!("/panes/{}/input", pct_encode(pane_id)),
            &serde_json::json!({ "data": data }),
        )
        .await
    }

    /// POST named keys (`enter`, `ctrl+c`, `up`, …). Same ordering caveat as `send_input`.
    pub async fn send_keys(&self, pane_id: &str, keys: &[String]) -> Result<(), RemoteError> {
        self.post_json(
            &format!("/panes/{}/input", pct_encode(pane_id)),
            &serde_json::json!({ "keys": keys }),
        )
        .await
    }
}

// ---------------------------------------------------------------------------------------------
// Session events
// ---------------------------------------------------------------------------------------------

/// What a [`RemoteSession`] reports to its consumer.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    /// WS up and greeted by the host (`version` = host build).
    Connected { version: String },
    /// The `/state` tree changed (emitted only on change; first fetch always emits).
    State(RemoteState),
    /// (Re)seed the pane's emulator: clear it, size it to `cols`×`rows` (if known) and feed
    /// `data`. Sent on attach, and when a delta isn't possible (truncated / host restarted /
    /// pane respawned).
    Reset {
        pane_id: String,
        data: String,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    /// Append-only live (or catch-up delta) output. Never duplicates what Reset delivered.
    Data { pane_id: String, data: String },
    /// Pane exited, or vanished from the host (`code: None`). Attachment is dropped for
    /// vanished panes.
    Exit { pane_id: String, code: Option<i32> },
    /// Precise agent liveness: `working | awaiting-input | done | exited`.
    Liveness { pane_id: String, state: String },
    /// Legacy activity: `busy | idle | exited`.
    Activity { pane_id: String, activity: String },
    /// Connection lost / not establishable. The session keeps retrying on its own.
    Disconnected { reason: String },
    /// 401: the token is bad/revoked. Retrying has STOPPED; call `reconnect_now()` after
    /// fixing it (or build a new session).
    AuthFailed,
    /// An input POST was refused (`403` host allowInput off, `423` pane locked, `404`, or
    /// `0` for a transport failure). The session stays up.
    InputRejected {
        pane_id: String,
        status: u16,
        reason: String,
    },
}

/// Tunables; `Default` is the production profile.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// `/state` refetch cadence while ≥1 pane is attached.
    pub state_poll: Duration,
    /// Coalescing window for `state` pings.
    pub state_debounce: Duration,
    pub ws_connect_timeout: Duration,
    /// WS ping cadence; the link is declared dead after `3×` this with no inbound traffic.
    pub ping_every: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            state_poll: Duration::from_secs(2),
            state_debounce: Duration::from_millis(100),
            ws_connect_timeout: Duration::from_secs(10),
            ping_every: Duration::from_secs(10),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Ordered input
// ---------------------------------------------------------------------------------------------

enum InputItem {
    Data(String),
    Keys(Vec<String>),
}

/// Ordered keystroke sink for ONE pane. Clone freely; every clone feeds the same queue.
/// Exactly one POST is in flight per pane; bytes queued meanwhile are coalesced into the next
/// POST. `send*` never blocks and returns `false` only once the session is gone.
#[derive(Clone)]
pub struct InputSender {
    tx: UnboundedSender<InputItem>,
}

impl InputSender {
    /// Queue raw bytes (as a string) verbatim — escape sequences, bracketed paste, `\r`, `\x03`…
    pub fn send(&self, data: impl Into<String>) -> bool {
        self.tx.send(InputItem::Data(data.into())).is_ok()
    }

    /// Queue named keys, ordered relative to `send`.
    pub fn send_keys(&self, keys: Vec<String>) -> bool {
        self.tx.send(InputItem::Keys(keys)).is_ok()
    }
}

async fn input_loop(
    client: RemoteClient,
    pane_id: String,
    mut rx: UnboundedReceiver<InputItem>,
    ev_tx: UnboundedSender<RemoteEvent>,
    cmd_tx: UnboundedSender<Cmd>,
) {
    while let Some(first) = rx.recv().await {
        let mut items = vec![first];
        while let Ok(more) = rx.try_recv() {
            items.push(more);
        }
        // Merge adjacent Data runs (order preserved; Keys are boundaries).
        let mut merged: Vec<InputItem> = Vec::new();
        for it in items {
            match (merged.last_mut(), it) {
                (Some(InputItem::Data(acc)), InputItem::Data(d)) => acc.push_str(&d),
                (_, it) => merged.push(it),
            }
        }
        for it in merged {
            let res = match &it {
                InputItem::Data(d) => client.send_input(&pane_id, d).await,
                InputItem::Keys(k) => client.send_keys(&pane_id, k).await,
            };
            match res {
                Ok(()) => {}
                Err(RemoteError::Unauthorized) => {
                    let _ = cmd_tx.send(Cmd::AuthFailed);
                }
                Err(e) => {
                    let _ = ev_tx.send(RemoteEvent::InputRejected {
                        pane_id: pane_id.clone(),
                        status: e.status_code(),
                        reason: e.to_string(),
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// WS frames
// ---------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Frame {
    Hello {
        #[serde(default)]
        version: String,
    },
    #[serde(rename_all = "camelCase")]
    Output {
        pane_id: Option<String>,
        data: String,
        cursor: u64,
    },
    #[serde(rename_all = "camelCase")]
    Exit {
        pane_id: Option<String>,
        code: i32,
    },
    #[serde(rename_all = "camelCase")]
    Activity {
        pane_id: String,
        activity: String,
    },
    #[serde(rename_all = "camelCase")]
    Liveness {
        pane_id: String,
        state: String,
    },
    State,
    #[serde(other)]
    Other,
}

fn utf16_len(s: &str) -> u64 {
    s.encode_utf16().count() as u64
}

/// Drop the first `n` UTF-16 code units of `s`.
fn skip_utf16(s: &str, n: u64) -> String {
    let units: Vec<u16> = s.encode_utf16().skip(n as usize).collect();
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------------------------------------
// RemoteSession
// ---------------------------------------------------------------------------------------------

enum Cmd {
    Attach(String),
    Detach(String),
    ReconnectNow,
    AuthFailed,
}

enum Internal {
    Snapshot {
        gen: u64,
        pane: String,
        req: u64,
        since: Option<u64>,
        out: Result<OutputChunk, RemoteError>,
        state: Option<RemoteState>,
    },
    State {
        gen: u64,
        res: Result<RemoteState, RemoteError>,
    },
}

/// Handle to the background session task. Dropping it stops the task (and the WS).
pub struct RemoteSession {
    client: RemoteClient,
    cmd_tx: UnboundedSender<Cmd>,
    ev_tx: UnboundedSender<RemoteEvent>,
    handle: tokio::runtime::Handle,
    inputs: Mutex<HashMap<String, InputSender>>,
    task: JoinHandle<()>,
}

impl fmt::Debug for RemoteSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteSession")
            .field("client", &self.client)
            .finish()
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RemoteSession {
    /// Start on the current tokio runtime. Panics outside one — use [`start_on`](Self::start_on).
    pub fn start(
        target: RemoteTarget,
        cfg: SessionConfig,
    ) -> (RemoteSession, UnboundedReceiver<RemoteEvent>) {
        Self::start_on(&tokio::runtime::Handle::current(), target, cfg)
    }

    /// Start on `handle`'s runtime. Returns immediately; the first event is `Connected` or
    /// `Disconnected`/`AuthFailed`.
    pub fn start_on(
        handle: &tokio::runtime::Handle,
        target: RemoteTarget,
        cfg: SessionConfig,
    ) -> (RemoteSession, UnboundedReceiver<RemoteEvent>) {
        let client = RemoteClient::new(&target);
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (ev_tx, ev_rx) = unbounded_channel();
        let (int_tx, int_rx) = unbounded_channel();
        let driver = Driver {
            client: client.clone(),
            events_url: target.events_url(),
            token: target.token().to_string(),
            cfg,
            cmd_rx,
            int_rx,
            int_tx,
            ev_tx: ev_tx.clone(),
            attached: HashMap::new(),
            last_state: None,
            gen: 0,
            req_seq: 0,
            state_inflight: false,
            state_again: false,
        };
        let task = handle.spawn(driver.run());
        (
            RemoteSession {
                client,
                cmd_tx,
                ev_tx,
                handle: handle.clone(),
                inputs: Mutex::new(HashMap::new()),
                task,
            },
            ev_rx,
        )
    }

    /// The HTTP client (for one-off calls such as `/state` before the session connects).
    pub fn client(&self) -> &RemoteClient {
        &self.client
    }

    /// Start streaming `pane_id`: yields a `Reset` (full replay) and then gapless `Data`.
    /// Attaching an already-attached pane re-seeds it with a fresh `Reset`.
    pub fn attach(&self, pane_id: &str) {
        let _ = self.cmd_tx.send(Cmd::Attach(pane_id.to_string()));
    }

    /// Stop streaming `pane_id` (frames for it are dropped).
    pub fn detach(&self, pane_id: &str) {
        let _ = self.cmd_tx.send(Cmd::Detach(pane_id.to_string()));
    }

    /// Skip the backoff wait and reconnect now. If currently connected the link is torn down
    /// and re-established (the right response to a laptop wake with a zombie socket). After
    /// `AuthFailed` this re-arms the session (e.g. once the host re-pairs the same token).
    pub fn reconnect_now(&self) {
        let _ = self.cmd_tx.send(Cmd::ReconnectNow);
    }

    /// The ordered input queue for `pane_id` (one per pane, created lazily).
    pub fn input_sender(&self, pane_id: &str) -> InputSender {
        let mut map = self.inputs.lock().unwrap();
        if let Some(s) = map.get(pane_id) {
            if !s.tx.is_closed() {
                return s.clone();
            }
        }
        let (tx, rx) = unbounded_channel();
        self.handle.spawn(input_loop(
            self.client.clone(),
            pane_id.to_string(),
            rx,
            self.ev_tx.clone(),
            self.cmd_tx.clone(),
        ));
        let sender = InputSender { tx };
        map.insert(pane_id.to_string(), sender.clone());
        sender
    }

    /// Stop the session (same as dropping it).
    pub fn close(self) {}
}

// ---------------------------------------------------------------------------------------------
// Driver (the background task)
// ---------------------------------------------------------------------------------------------

enum Phase {
    /// No live WS (or not yet synced on this connection).
    Idle,
    /// Snapshot GET in flight; WS output frames buffered meanwhile.
    Fetching {
        buffered: Vec<(u64, String)>,
    },
    Live,
}

struct PaneSync {
    /// Cursor of the last output delivered to the consumer (`None` = never seeded).
    cursor: Option<u64>,
    uid: Option<String>,
    phase: Phase,
    /// Identifies the current snapshot request; stale results are discarded.
    req: u64,
}

enum Ended {
    /// Connection lost (reason already scrubbed).
    Lost(String),
    Auth,
    /// `reconnect_now` while connected: go again immediately.
    Again,
}

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Driver {
    client: RemoteClient,
    events_url: String,
    token: String,
    cfg: SessionConfig,
    cmd_rx: UnboundedReceiver<Cmd>,
    int_rx: UnboundedReceiver<Internal>,
    int_tx: UnboundedSender<Internal>,
    ev_tx: UnboundedSender<RemoteEvent>,
    attached: HashMap<String, PaneSync>,
    last_state: Option<RemoteState>,
    /// Connection generation: bumps on every (dis)connect so stale async results are ignored.
    gen: u64,
    req_seq: u64,
    state_inflight: bool,
    state_again: bool,
}

enum Conn {
    Up(Box<WsStream>),
    Auth,
    Failed(String),
}

impl Driver {
    fn emit(&self, ev: RemoteEvent) {
        let _ = self.ev_tx.send(ev);
    }

    fn jitter(&self, base: Duration) -> Duration {
        use rand::Rng;
        let half = base / 2;
        let extra = rand::thread_rng().gen_range(0.0..=1.0);
        half + half.mul_f64(extra)
    }

    async fn run(mut self) {
        let mut backoff = self.cfg.backoff_min;
        let mut announced_down = false;
        loop {
            match self.connect().await {
                Conn::Up(ws) => {
                    backoff = self.cfg.backoff_min;
                    announced_down = false;
                    match self.connected(*ws).await {
                        Ended::Again => continue,
                        Ended::Auth => {
                            self.emit(RemoteEvent::AuthFailed);
                            if !self.idle_after_auth().await {
                                return;
                            }
                            continue;
                        }
                        Ended::Lost(reason) => {
                            self.reset_phases();
                            self.emit(RemoteEvent::Disconnected { reason });
                            announced_down = true;
                        }
                    }
                }
                Conn::Auth => {
                    self.emit(RemoteEvent::AuthFailed);
                    if !self.idle_after_auth().await {
                        return;
                    }
                    continue;
                }
                Conn::Failed(reason) => {
                    if !announced_down {
                        self.emit(RemoteEvent::Disconnected { reason });
                        announced_down = true;
                    }
                }
            }
            let wait = self.jitter(backoff);
            backoff = (backoff * 2).min(self.cfg.backoff_max);
            if !self.wait(wait).await {
                return;
            }
        }
    }

    /// Offline command handling shared by the idle/backoff waits.
    fn apply_offline(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Attach(p) => {
                self.req_seq += 1;
                self.attached.insert(
                    p,
                    PaneSync {
                        cursor: None,
                        uid: None,
                        phase: Phase::Idle,
                        req: self.req_seq,
                    },
                );
            }
            Cmd::Detach(p) => {
                self.attached.remove(&p);
            }
            Cmd::ReconnectNow | Cmd::AuthFailed => {}
        }
    }

    /// Sleep `d` (backoff); false if the session was closed. `reconnect_now` cuts it short.
    async fn wait(&mut self, d: Duration) -> bool {
        let sleep = tokio::time::sleep(d);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                _ = &mut sleep => return true,
                cmd = self.cmd_rx.recv() => match cmd {
                    None => return false,
                    Some(Cmd::ReconnectNow) => return true,
                    Some(c) => self.apply_offline(c),
                },
                // Stale async results from the dead connection.
                _ = self.int_rx.recv() => {}
            }
        }
    }

    /// After AuthFailed: no retries until `reconnect_now`. False if closed.
    async fn idle_after_auth(&mut self) -> bool {
        self.reset_phases();
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    None => return false,
                    Some(Cmd::ReconnectNow) => return true,
                    Some(c) => self.apply_offline(c),
                },
                _ = self.int_rx.recv() => {}
            }
        }
    }

    fn reset_phases(&mut self) {
        self.gen += 1;
        self.state_inflight = false;
        self.state_again = false;
        for p in self.attached.values_mut() {
            p.phase = Phase::Idle;
        }
    }

    async fn connect(&mut self) -> Conn {
        let attempt = tokio::time::timeout(
            self.cfg.ws_connect_timeout,
            tokio_tungstenite::connect_async(self.events_url.as_str()),
        )
        .await;
        match attempt {
            Err(_) => Conn::Failed("connect timed out".into()),
            Ok(Ok((ws, _resp))) => Conn::Up(Box::new(ws)),
            Ok(Err(tokio_tungstenite::tungstenite::Error::Http(resp)))
                if resp.status().as_u16() == 401 =>
            {
                Conn::Auth
            }
            Ok(Err(e)) => Conn::Failed(scrub(&e.to_string(), &self.token)),
        }
    }

    // ---- connected state ----

    async fn connected(&mut self, ws: WsStream) -> Ended {
        self.gen += 1;
        let (mut sink, mut stream) = ws.split();

        let panes: Vec<String> = self.attached.keys().cloned().collect();
        for p in panes {
            self.start_sync(&p, true);
        }
        self.request_state();

        let mut ping = tokio::time::interval(self.cfg.ping_every);
        ping.reset();
        let mut poll = tokio::time::interval(self.cfg.state_poll);
        poll.reset();
        let mut last_rx = Instant::now();
        let mut debounce: Option<Instant> = None;

        loop {
            let debounce_fut = async {
                match debounce {
                    Some(t) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            let ended: Option<Ended> = tokio::select! {
                msg = stream.next() => match msg {
                    None => Some(Ended::Lost("connection closed".into())),
                    Some(Err(e)) => Some(Ended::Lost(scrub(&e.to_string(), &self.token))),
                    Some(Ok(m)) => {
                        last_rx = Instant::now();
                        match m {
                            Message::Text(t) => self.on_frame(t.as_str(), &mut debounce),
                            Message::Close(_) => Some(Ended::Lost("closed by host".into())),
                            _ => None,
                        }
                    }
                },
                cmd = self.cmd_rx.recv() => match cmd {
                    // Session handle dropped: stop silently (the task is normally aborted first).
                    None => futures_util::future::pending().await,
                    Some(c) => self.on_cmd(c),
                },
                int = self.int_rx.recv() => match int {
                    Some(i) => self.on_internal(i),
                    None => None,
                },
                _ = debounce_fut => {
                    debounce = None;
                    self.request_state();
                    None
                }
                _ = poll.tick(), if !self.attached.is_empty() => {
                    self.request_state();
                    None
                }
                _ = ping.tick() => {
                    if last_rx.elapsed() > self.cfg.ping_every * 3 {
                        Some(Ended::Lost("host stopped responding".into()))
                    } else if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                        Some(Ended::Lost("send failed".into()))
                    } else {
                        None
                    }
                }
            };
            if let Some(e) = ended {
                let _ = sink.close().await;
                return e;
            }
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) -> Option<Ended> {
        match cmd {
            Cmd::Attach(p) => {
                self.attached.remove(&p);
                self.attached.insert(
                    p.clone(),
                    PaneSync {
                        cursor: None,
                        uid: None,
                        phase: Phase::Idle,
                        req: 0,
                    },
                );
                self.start_sync(&p, false);
                None
            }
            Cmd::Detach(p) => {
                self.attached.remove(&p);
                None
            }
            Cmd::ReconnectNow => Some(Ended::Again),
            Cmd::AuthFailed => Some(Ended::Auth),
        }
    }

    /// Begin (or restart) the snapshot for `pane`. `resume` = use `since=cursor` when we have
    /// one (reconnect catch-up); otherwise force a full replay (`Reset`).
    fn start_sync(&mut self, pane: &str, resume: bool) {
        self.req_seq += 1;
        let req = self.req_seq;
        let Some(p) = self.attached.get_mut(pane) else {
            return;
        };
        if !resume {
            p.cursor = None;
        }
        let since = p.cursor;
        p.req = req;
        p.phase = Phase::Fetching { buffered: vec![] };
        let client = self.client.clone();
        let tx = self.int_tx.clone();
        let gen = self.gen;
        let pane = pane.to_string();
        tokio::spawn(async move {
            let (out, state) = tokio::join!(client.get_output(&pane, since), client.get_state());
            let _ = tx.send(Internal::Snapshot {
                gen,
                pane,
                req,
                since,
                out,
                state: state.ok(),
            });
        });
    }

    fn request_state(&mut self) {
        if self.state_inflight {
            self.state_again = true;
            return;
        }
        self.state_inflight = true;
        let client = self.client.clone();
        let tx = self.int_tx.clone();
        let gen = self.gen;
        tokio::spawn(async move {
            let res = client.get_state().await;
            let _ = tx.send(Internal::State { gen, res });
        });
    }

    fn on_frame(&mut self, text: &str, debounce: &mut Option<Instant>) -> Option<Ended> {
        let Ok(frame) = serde_json::from_str::<Frame>(text) else {
            return None;
        };
        match frame {
            Frame::Hello { version } => self.emit(RemoteEvent::Connected { version }),
            Frame::State => {
                if debounce.is_none() {
                    *debounce = Some(Instant::now() + self.cfg.state_debounce);
                }
            }
            Frame::Output {
                pane_id: Some(pane),
                data,
                cursor,
            } => {
                let p = self.attached.get_mut(&pane)?;
                match &mut p.phase {
                    Phase::Idle => {}
                    Phase::Fetching { buffered } => buffered.push((cursor, data)),
                    Phase::Live => {
                        let c = p.cursor.unwrap_or(0);
                        if let Some(d) = splice(c, cursor, data) {
                            p.cursor = Some(cursor);
                            self.emit(RemoteEvent::Data {
                                pane_id: pane,
                                data: d,
                            });
                        }
                    }
                }
            }
            Frame::Exit {
                pane_id: Some(pane_id),
                code,
            } => {
                if self.attached.contains_key(&pane_id) {
                    self.emit(RemoteEvent::Exit {
                        pane_id,
                        code: Some(code),
                    });
                }
            }
            Frame::Activity { pane_id, activity } => {
                self.emit(RemoteEvent::Activity { pane_id, activity });
            }
            Frame::Liveness { pane_id, state } => {
                self.emit(RemoteEvent::Liveness { pane_id, state });
            }
            _ => {}
        }
        None
    }

    fn on_internal(&mut self, int: Internal) -> Option<Ended> {
        match int {
            Internal::State { gen, res } => {
                if gen != self.gen {
                    return None;
                }
                self.state_inflight = false;
                let again = std::mem::take(&mut self.state_again);
                match res {
                    Ok(s) => self.apply_state(s),
                    Err(RemoteError::Unauthorized) => return Some(Ended::Auth),
                    Err(e) => return Some(Ended::Lost(e.to_string())),
                }
                if again {
                    self.request_state();
                }
                None
            }
            Internal::Snapshot {
                gen,
                pane,
                req,
                since,
                out,
                state,
            } => {
                if gen != self.gen {
                    return None;
                }
                if let Some(s) = state {
                    self.apply_state(s);
                }
                let current = matches!(
                    self.attached.get(&pane),
                    Some(p) if p.req == req && matches!(p.phase, Phase::Fetching { .. })
                );
                if !current {
                    return None;
                }
                self.finish_snapshot(&pane, since, out)
            }
        }
    }

    fn finish_snapshot(
        &mut self,
        pane: &str,
        since: Option<u64>,
        out: Result<OutputChunk, RemoteError>,
    ) -> Option<Ended> {
        let chunk = match out {
            Ok(c) => c,
            Err(RemoteError::Unauthorized) => return Some(Ended::Auth),
            Err(RemoteError::NotFound) => {
                self.attached.remove(pane);
                self.emit(RemoteEvent::Exit {
                    pane_id: pane.to_string(),
                    code: None,
                });
                return None;
            }
            Err(e) => return Some(Ended::Lost(e.to_string())),
        };
        if let Some(s) = since {
            if chunk.cursor < s {
                // Host restarted (cursor went backwards): the delta is meaningless → full reset.
                self.start_sync(pane, false);
                return None;
            }
        }
        let (cols, rows) = self
            .last_state
            .as_ref()
            .and_then(|s| s.pane(pane))
            .map(|p| (p.cols, p.rows))
            .unwrap_or((None, None));
        let delta = since.is_some() && !chunk.truncated;
        if delta {
            if !chunk.output.is_empty() {
                self.emit(RemoteEvent::Data {
                    pane_id: pane.to_string(),
                    data: chunk.output,
                });
            }
        } else {
            self.emit(RemoteEvent::Reset {
                pane_id: pane.to_string(),
                data: chunk.output,
                cols,
                rows,
            });
        }
        let mut events = Vec::new();
        if let Some(p) = self.attached.get_mut(pane) {
            let mut c = chunk.cursor;
            if let Phase::Fetching { buffered } = std::mem::replace(&mut p.phase, Phase::Live) {
                for (fc, data) in buffered {
                    if let Some(d) = splice(c, fc, data) {
                        c = fc;
                        events.push(RemoteEvent::Data {
                            pane_id: pane.to_string(),
                            data: d,
                        });
                    }
                }
            }
            p.cursor = Some(c);
        }
        for e in events {
            self.emit(e);
        }
        None
    }

    fn apply_state(&mut self, new: RemoteState) {
        let changed = self.last_state.as_ref() != Some(&new);
        // Vanished panes → Exit + detach; respawned panes (new session uid) → fresh Reset.
        let ids: Vec<String> = self.attached.keys().cloned().collect();
        let mut resync = Vec::new();
        for id in ids {
            match new.pane(&id) {
                None => {
                    self.attached.remove(&id);
                    self.emit(RemoteEvent::Exit {
                        pane_id: id,
                        code: None,
                    });
                }
                Some(hp) => {
                    if let Some(p) = self.attached.get_mut(&id) {
                        match &p.uid {
                            None => p.uid = Some(hp.session_uid.clone()),
                            Some(u) if *u != hp.session_uid => {
                                p.uid = Some(hp.session_uid.clone());
                                resync.push(id.clone());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        if changed {
            self.last_state = Some(new.clone());
            self.emit(RemoteEvent::State(new));
        }
        for id in resync {
            self.start_sync(&id, false);
        }
    }
}

/// Given the cursor `c` already delivered, decide what of a frame ending at `end` is new.
/// Whole-frame overlap → `None`; partial overlap (shouldn't happen: cursors sit on batch
/// boundaries) → the unseen suffix.
pub(crate) fn splice(c: u64, end: u64, data: String) -> Option<String> {
    if end <= c || data.is_empty() {
        return None;
    }
    let len = utf16_len(&data);
    let start = end.saturating_sub(len);
    if start >= c {
        Some(data)
    } else {
        Some(skip_utf16(&data, c - start))
    }
}
