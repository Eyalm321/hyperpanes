//! Remote-attach in the desktop app: another machine's hyperpanes panes as local "remote tabs".
//!
//! The protocol layer is `hyperpanes_core::remote` (GUI-free). This module is the glue:
//!
//! * **uids** — a remote pane is an ordinary [`PaneState`] whose uid is
//!   `remote:<hostKey>:<hostPaneId>` (`hostKey` is a small process-wide counter, NEVER the token).
//! * **one write choke point** — [`write`] / [`kill`] replace every `mgr.write` / `mgr.kill`
//!   call site: remote uids go to the host's ordered `InputSender` (or detach), local uids to the
//!   `SessionManager` untouched.
//! * **[`Hub`]** — owned by the `App`, drained NON-BLOCKINGLY from the pump tick. Sessions run on
//!   the app's tokio runtime; nothing here blocks or touches the network on the UI thread.
//! * **pure mapping** — [`plan_tabs`] turns a host `/state` into the tab model; [`apply_plan`]
//!   merges it into a window's [`State`].
//!
//! Remote panes are excluded from: emulator reply write-back, startup commands, pty resize,
//! kill, the local control read model, session persistence and the ambient-AI feed.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use hyperpanes_core::remote::{
    ConnectRequest, RemoteEvent, RemotePane, RemoteSession, RemoteState, RemoteTarget,
    SessionConfig,
};
use hyperpanes_core::session_manager::SessionManager;
use hyperpanes_terminal_widget::{SoftwareRenderer, TerminalPane};
use serde_json::json;
use tokio::runtime::Handle;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::Window;
use crate::remote_hosts::{self, SavedHost};
use crate::state::{parse_hex, PaneState, State, Tab};
use crate::theme;

/// Toast for an image paste into a remote pane (requires local files / clipboard on the host).
pub const IMAGE_PASTE_UNSUPPORTED: &str = "not supported on remote panes";
/// Visible marker prefixed to every remote tab title.
pub const REMOTE_MARK: &str = "⇄ ";

// ---------------------------------------------------------------------------------------------
// uids
// ---------------------------------------------------------------------------------------------

const UID_PREFIX: &str = "remote:";

pub fn is_remote_uid(uid: &str) -> bool {
    uid.starts_with(UID_PREFIX)
}

pub fn make_uid(host_key: u32, host_pane_id: &str) -> String {
    format!("{UID_PREFIX}{host_key}:{host_pane_id}")
}

/// `remote:<hostKey>:<hostPaneId>` → `(hostKey, hostPaneId)`. The pane id may itself contain `:`.
pub fn parse_uid(uid: &str) -> Option<(u32, &str)> {
    let rest = uid.strip_prefix(UID_PREFIX)?;
    let (key, pane) = rest.split_once(':')?;
    if pane.is_empty() {
        return None;
    }
    Some((key.parse().ok()?, pane))
}

// ---------------------------------------------------------------------------------------------
// registry + write choke point
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Registry {
    hosts: HashMap<u32, Arc<RemoteSession>>,
    labels: HashMap<u32, String>,
    /// Remote uids the user closed locally (detach only): the host state must not re-add them.
    detached: HashSet<String>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

fn session_for(host_key: u32) -> Option<Arc<RemoteSession>> {
    registry().lock().unwrap().hosts.get(&host_key).cloned()
}

pub fn is_detached(uid: &str) -> bool {
    registry().lock().unwrap().detached.contains(uid)
}

/// `(hostKey, label)` of every live connection, key order (palette: reconnect / disconnect).
pub fn live_hosts() -> Vec<(u32, String)> {
    let r = registry().lock().unwrap();
    let mut v: Vec<(u32, String)> = r.labels.iter().map(|(k, l)| (*k, l.clone())).collect();
    v.sort();
    v
}

/// The ONE write path for pane input: remote uids → the host's ordered input queue (never
/// blocks), local uids → the session manager.
pub fn write(mgr: &SessionManager, uid: &str, data: &str) {
    match parse_uid(uid) {
        Some((key, pane)) => {
            if let Some(s) = session_for(key) {
                s.input_sender(pane).send(data);
            }
        }
        None => mgr.write(uid, data),
    }
}

/// Closing a remote pane DETACHES it (the host pane keeps running); local uids are killed.
pub fn kill(mgr: &SessionManager, uid: &str) {
    match parse_uid(uid) {
        Some((key, pane)) => {
            let mut r = registry().lock().unwrap();
            r.detached.insert(uid.to_string());
            if let Some(s) = r.hosts.get(&key) {
                s.detach(pane);
            }
        }
        None => mgr.kill(uid),
    }
}

// ---------------------------------------------------------------------------------------------
// model types carried on Tab / PaneState
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTabInfo {
    pub host_key: u32,
    pub host_tab_id: String,
    pub host_label: String,
    pub base_title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePaneInfo {
    pub host_key: u32,
    pub host_pane_id: String,
    pub cols: u16,
    pub rows: u16,
    pub exited: bool,
    pub exit_code: Option<i32>,
    /// Vanished from the host's state (kept, not auto-closed).
    pub gone: bool,
    pub host_subtitle: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnStatus {
    Connecting,
    Connected,
    Disconnected,
    AuthFailed,
}

pub fn tab_title(label: &str, base: &str, conn: ConnStatus) -> String {
    let suffix = match conn {
        ConnStatus::Disconnected => " (disconnected)",
        ConnStatus::AuthFailed => " (auth failed — re-pair)",
        _ => "",
    };
    format!("{REMOTE_MARK}{label} · {base}{suffix}")
}

pub fn compose_subtitle(
    conn: ConnStatus,
    gone: bool,
    exited: bool,
    exit_code: Option<i32>,
    host_sub: Option<&str>,
) -> Option<String> {
    let status = match (conn, gone, exited) {
        (ConnStatus::AuthFailed, _, _) => Some("auth failed — re-pair".to_string()),
        (ConnStatus::Disconnected, _, _) => Some("disconnected".to_string()),
        (_, true, _) => Some("gone".to_string()),
        (_, _, true) => Some(match exit_code {
            Some(c) => format!("exited ({c})"),
            None => "exited".to_string(),
        }),
        _ => None,
    };
    match (status, host_sub.filter(|s| !s.is_empty())) {
        (Some(s), Some(h)) => Some(format!("{s} · {h}")),
        (Some(s), None) => Some(s),
        (None, Some(h)) => Some(h.to_string()),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------------------------
// pure State → tab-model mapping
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedPane {
    pub uid: String,
    pub host_pane_id: String,
    pub label: String,
    pub subtitle: Option<String>,
    pub color: String,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    pub exited: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlannedTab {
    pub host_tab_id: String,
    pub base_title: String,
    pub layout: String,
    pub panes: Vec<PlannedPane>,
}

fn planned_pane(host_key: u32, p: &RemotePane) -> PlannedPane {
    PlannedPane {
        uid: make_uid(host_key, &p.id),
        host_pane_id: p.id.clone(),
        // Label only — never `meta` (it can carry tokens / orchestration secrets).
        label: p.label.clone(),
        subtitle: p.subtitle.clone(),
        color: p.color.clone(),
        cols: p.cols,
        rows: p.rows,
        exited: p.exited(),
        exit_code: p.exit_code,
    }
}

/// One local tab per host tab (all host windows flattened, host order).
pub fn plan_tabs(host_key: u32, state: &RemoteState) -> Vec<PlannedTab> {
    state
        .windows
        .iter()
        .flat_map(|w| w.tabs.iter())
        .map(|t| PlannedTab {
            host_tab_id: t.id.clone(),
            base_title: t.title.clone(),
            layout: t.layout.clone(),
            panes: t.panes.iter().map(|p| planned_pane(host_key, p)).collect(),
        })
        .collect()
}

fn clamp_dims(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, 500), rows.clamp(1, 300))
}

fn refresh_subtitle(p: &mut PaneState, conn: ConnStatus) {
    if let Some(r) = &p.remote {
        p.subtitle = compose_subtitle(
            conn,
            r.gone,
            r.exited,
            r.exit_code,
            r.host_subtitle.as_deref(),
        )
        .map(Into::into);
    }
}

fn pin_dims(p: &mut PaneState, cols: u16, rows: u16) {
    let (c, r) = clamp_dims(cols, rows);
    if let Some(info) = &mut p.remote {
        info.cols = c;
        info.rows = r;
    }
    if p.applied != (c as usize, r as usize) {
        p.pane.resize(c as usize, r as usize);
        p.applied = (c as usize, r as usize);
        p.pane.search_reflow();
    }
}

/// Merge a planned host state into `st`. Returns the host pane ids to `attach` (newly created
/// or reappeared). Panes the host no longer lists are marked gone, never removed.
pub fn apply_plan(
    st: &mut State,
    host_key: u32,
    label: &str,
    conn: ConnStatus,
    plan: &[PlannedTab],
    activate: &mut bool,
) -> Vec<String> {
    let mut attach = Vec::new();
    let mut planned_uids: HashSet<&str> = HashSet::new();
    for pt in plan {
        for p in &pt.panes {
            planned_uids.insert(p.uid.as_str());
        }
        let live: Vec<&PlannedPane> = pt.panes.iter().filter(|p| !is_detached(&p.uid)).collect();
        let ti = st.tabs.iter().position(|t| {
            t.remote
                .as_ref()
                .is_some_and(|r| r.host_key == host_key && r.host_tab_id == pt.host_tab_id)
        });
        let ti = match ti {
            Some(i) => i,
            None => {
                if live.is_empty() {
                    continue;
                }
                let mut tab = Tab::empty(tab_title(label, &pt.base_title, conn).into());
                tab.remote = Some(RemoteTabInfo {
                    host_key,
                    host_tab_id: pt.host_tab_id.clone(),
                    host_label: label.to_string(),
                    base_title: pt.base_title.clone(),
                });
                let ti = st.push_remote_tab(tab, &pt.layout);
                // A user-initiated connect switches to its first remote tab (autoconnect at
                // startup must not steal focus from the restored local tab).
                if std::mem::take(activate) {
                    st.active = ti;
                    st.editing_tab = -1;
                }
                ti
            }
        };
        if let Some(r) = st.tabs[ti].remote.as_mut() {
            r.base_title = pt.base_title.clone();
            r.host_label = label.to_string();
        }
        for pp in live {
            let color = (!pp.color.is_empty()).then(|| parse_hex(&pp.color));
            if let Some((eti, epi)) = st.find_pane(&pp.uid) {
                let p = &mut st.tabs[eti].panes[epi];
                let reappeared = p.remote.as_ref().is_some_and(|r| r.gone);
                if p.title.as_str() != pp.label {
                    p.title = pp.label.clone().into();
                }
                if let Some(c) = color {
                    p.accent = c;
                    p.pinned_accent = Some(c);
                }
                if let Some(info) = &mut p.remote {
                    info.gone = false;
                    info.exited = pp.exited;
                    info.exit_code = pp.exit_code;
                    info.host_subtitle = pp.subtitle.clone();
                }
                if let (Some(c), Some(r)) = (pp.cols, pp.rows) {
                    pin_dims(p, c, r);
                }
                refresh_subtitle(p, conn);
                if reappeared {
                    attach.push(pp.host_pane_id.clone());
                }
            } else {
                let (cols, rows) = clamp_dims(pp.cols.unwrap_or(80), pp.rows.unwrap_or(24));
                let info = RemotePaneInfo {
                    host_key,
                    host_pane_id: pp.host_pane_id.clone(),
                    cols,
                    rows,
                    exited: pp.exited,
                    exit_code: pp.exit_code,
                    gone: false,
                    host_subtitle: pp.subtitle.clone(),
                };
                let idx = st.tabs[ti].panes.len();
                let mut ps = st.new_remote_pane(pp.uid.clone(), info, &pp.label, idx, color);
                refresh_subtitle(&mut ps, conn);
                st.push_remote_pane(ti, ps);
                attach.push(pp.host_pane_id.clone());
            }
        }
    }
    // Vanished panes: keep them, show "gone".
    for t in st.tabs.iter_mut() {
        if !t.remote.as_ref().is_some_and(|r| r.host_key == host_key) {
            continue;
        }
        for p in t.panes.iter_mut() {
            if planned_uids.contains(p.uid.as_str()) {
                continue;
            }
            if let Some(info) = &mut p.remote {
                if !info.gone {
                    info.gone = true;
                    refresh_subtitle(p, conn);
                }
            }
        }
    }
    retitle(st, host_key, label, conn);
    st.dirty = true;
    attach
}

/// Re-derive tab titles + pane subtitles for `host_key` after a connection-state change.
pub fn retitle(st: &mut State, host_key: u32, label: &str, conn: ConnStatus) {
    for t in st.tabs.iter_mut() {
        let Some(r) = t.remote.as_mut().filter(|r| r.host_key == host_key) else {
            continue;
        };
        r.host_label = label.to_string();
        t.title = tab_title(label, &r.base_title, conn).into();
        for p in t.panes.iter_mut() {
            refresh_subtitle(p, conn);
        }
    }
    st.dirty = true;
}

fn fresh_pane(st: &State, cols: u16, rows: u16) -> TerminalPane {
    let mut pane = TerminalPane::new(
        cols as usize,
        rows as usize,
        Box::new(SoftwareRenderer::new()),
    );
    pane.set_palette(theme::terminal_theme(st.settings.terminal_theme));
    pane
}

/// `Reset`: recreate the emulator (clean modes + scrollback), size it to the host grid, feed.
fn apply_reset(st: &mut State, uid: &str, data: &str, cols: Option<u16>, rows: Option<u16>) {
    let Some((ti, pi)) = st.find_pane(uid) else {
        return;
    };
    let (cur_c, cur_r) = {
        let p = &st.tabs[ti].panes[pi];
        p.remote
            .as_ref()
            .map(|r| (r.cols, r.rows))
            .unwrap_or((80, 24))
    };
    let (c, r) = clamp_dims(cols.unwrap_or(cur_c), rows.unwrap_or(cur_r));
    let fresh = fresh_pane(st, c, r);
    let p = &mut st.tabs[ti].panes[pi];
    p.pane = fresh;
    p.applied = (c as usize, r as usize);
    if let Some(info) = &mut p.remote {
        info.cols = c;
        info.rows = r;
    }
    p.pane.feed(data);
    // The host's own emulator answers DSR/DA; never write replies back.
    let _ = p.pane.take_replies();
    st.dirty = true;
}

fn apply_data(st: &mut State, uid: &str, data: &str) {
    if let Some((ti, pi)) = st.find_pane(uid) {
        let p = &mut st.tabs[ti].panes[pi];
        if let Some(title) = crate::glow::sniff_osc_title(data) {
            p.shell_title = title;
        }
        p.pane.feed(data);
        let _ = p.pane.take_replies();
    }
}

fn apply_exit(st: &mut State, uid: &str, code: Option<i32>, conn: ConnStatus) {
    if let Some((ti, pi)) = st.find_pane(uid) {
        let p = &mut st.tabs[ti].panes[pi];
        if let Some(info) = &mut p.remote {
            if code.is_some() {
                info.exited = true;
                info.exit_code = code;
            } else {
                info.gone = true;
            }
        }
        refresh_subtitle(p, conn);
        st.dirty = true;
    }
}

/// Toast on the best pane for `host_key`: the focused pane if it is that host's, else its first.
fn toast_host(st: &mut State, host_key: u32, msg: &str) {
    let mine = |p: &PaneState| p.remote.as_ref().is_some_and(|r| r.host_key == host_key);
    let t = &st.tabs[st.active.min(st.tabs.len() - 1)];
    if let Some(p) = t.panes.get(t.focused).filter(|p| mine(p)) {
        let uid = p.uid.clone();
        if let Some((ti, pi)) = st.find_pane(&uid) {
            st.tabs[ti].panes[pi].pane.set_toast(msg);
        }
        return;
    }
    for t in st.tabs.iter_mut() {
        if let Some(p) = t.panes.iter_mut().find(|p| mine(p)) {
            p.pane.set_toast(msg);
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Hub (UI thread)
// ---------------------------------------------------------------------------------------------

const TOAST_EVERY: Duration = Duration::from_secs(8);
const SCREENS_EVERY: Duration = Duration::from_millis(300);
/// Bound the events applied per host per tick so a flood can't starve the UI.
const MAX_EVENTS_PER_TICK: usize = 256;

struct HostConn {
    key: u32,
    label: String,
    window_id: usize,
    conn: ConnStatus,
    rx: UnboundedReceiver<RemoteEvent>,
    session: Arc<RemoteSession>,
    /// Switch to the host's first remote tab when it appears (user-initiated connects only).
    activate: bool,
    last_input_toast: Option<Instant>,
}

pub struct Hub {
    handle: Handle,
    hosts: std::cell::RefCell<Vec<HostConn>>,
    autoconnect_done: std::cell::Cell<bool>,
    last_screens: std::cell::Cell<Option<Instant>>,
    screens_nonempty: std::cell::Cell<bool>,
}

impl Hub {
    /// Must be created inside the tokio runtime guard (like `ControlHost`).
    pub fn new() -> Self {
        Hub {
            handle: Handle::current(),
            hosts: Default::default(),
            autoconnect_done: Default::default(),
            last_screens: Default::default(),
            screens_nonempty: Default::default(),
        }
    }

    /// Start a session on the runtime — returns immediately; no I/O on this thread.
    pub fn connect(
        &self,
        window_id: usize,
        key: u32,
        target: RemoteTarget,
        label: &str,
        activate: bool,
    ) {
        let (session, rx) = RemoteSession::start_on(&self.handle, target, SessionConfig::default());
        let session = Arc::new(session);
        {
            let mut r = registry().lock().unwrap();
            r.hosts.insert(key, session.clone());
            r.labels.insert(key, label.to_string());
        }
        self.hosts.borrow_mut().push(HostConn {
            key,
            label: label.to_string(),
            window_id,
            conn: ConnStatus::Connecting,
            rx,
            session,
            activate,
            last_input_toast: None,
        });
    }

    /// Connect exactly like the dialog; optionally persist. Returns the host key.
    pub fn connect_request(&self, window_id: usize, req: ConnectRequest, autoconnect: bool) -> u32 {
        let label = req
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| remote_hosts::default_label(&req.target));
        if req.remember {
            let _ =
                remote_hosts::remember(SavedHost::from_target(&label, &req.target, autoconnect));
        }
        self.connect(window_id, req.host_id, req.target, &label, true);
        req.host_id
    }

    pub fn reconnect(&self, key: u32) {
        if let Some(h) = self.hosts.borrow().iter().find(|h| h.key == key) {
            h.session.reconnect_now();
        }
    }

    /// Drop the session and close its remote tabs (detach only).
    pub fn disconnect(&self, windows: &[Rc<Window>], key: u32) {
        self.hosts.borrow_mut().retain(|h| h.key != key);
        {
            let mut r = registry().lock().unwrap();
            r.hosts.remove(&key);
            r.labels.remove(&key);
            r.detached
                .retain(|u| parse_uid(u).map(|(k, _)| k) != Some(key));
        }
        for w in windows {
            w.state.borrow_mut().remove_remote_tabs(key);
        }
    }

    pub fn disconnect_label(&self, windows: &[Rc<Window>], label: &str) {
        let keys: Vec<u32> = self
            .hosts
            .borrow()
            .iter()
            .filter(|h| h.label == label)
            .map(|h| h.key)
            .collect();
        for k in keys {
            self.disconnect(windows, k);
        }
    }

    /// Connect every saved host flagged `autoconnect` (once; async, never blocks startup).
    pub fn autoconnect_once(&self, windows: &[Rc<Window>]) {
        if self.autoconnect_done.replace(true) {
            return;
        }
        let Some(w) = windows.first() else { return };
        for h in remote_hosts::load().into_iter().filter(|h| h.autoconnect) {
            if let Ok(t) = h.target() {
                self.connect(
                    w.id,
                    hyperpanes_core::remote::next_host_id(),
                    t,
                    &h.label,
                    false,
                );
            }
        }
    }

    /// Drain every host's event channel (try_recv only) into its window. Returns events applied.
    pub fn pump(&self, windows: &[Rc<Window>]) -> usize {
        let mut applied = 0;
        let mut orphaned = Vec::new();
        let mut hosts = self.hosts.borrow_mut();
        for h in hosts.iter_mut() {
            let mut evs = Vec::new();
            while evs.len() < MAX_EVENTS_PER_TICK {
                match h.rx.try_recv() {
                    Ok(e) => evs.push(e),
                    Err(_) => break,
                }
            }
            if evs.is_empty() {
                continue;
            }
            let Some(w) = windows.iter().find(|w| w.id == h.window_id) else {
                orphaned.push(h.key);
                continue;
            };
            applied += evs.len();
            let mut st = w.state.borrow_mut();
            for ev in evs {
                handle_event(&mut st, h, ev);
            }
        }
        // Hosts whose window closed: detach (windows must be re-borrowed after the hosts guard).
        let gone_window: Vec<u32> = hosts
            .iter()
            .filter(|h| !windows.iter().any(|w| w.id == h.window_id))
            .map(|h| h.key)
            .chain(orphaned)
            .collect();
        drop(hosts);
        for k in gone_window {
            self.disconnect(windows, k);
        }
        applied
    }

    /// `remoteScreen` snapshot, throttled; `None` when there's nothing new to publish.
    pub fn screens(&self, windows: &[Rc<Window>]) -> Option<serde_json::Value> {
        let hosts = self.hosts.borrow();
        if hosts.is_empty() {
            return self.screens_nonempty.replace(false).then(|| json!([]));
        }
        let now = Instant::now();
        if self
            .last_screens
            .get()
            .is_some_and(|t| now.duration_since(t) < SCREENS_EVERY)
        {
            return None;
        }
        self.last_screens.set(Some(now));
        self.screens_nonempty.set(true);
        let mut out = Vec::new();
        for h in hosts.iter() {
            let Some(w) = windows.iter().find(|w| w.id == h.window_id) else {
                continue;
            };
            let st = w.state.borrow();
            for t in st.tabs.iter() {
                for p in &t.panes {
                    let Some(r) = p.remote.as_ref().filter(|r| r.host_key == h.key) else {
                        continue;
                    };
                    out.push(json!({
                        "hostId": h.key,
                        "hostPaneId": r.host_pane_id,
                        "cols": r.cols,
                        "rows": r.rows,
                        "connected": h.conn == ConnStatus::Connected,
                        "exited": r.exited,
                        "screenText": p.pane.screen_text(),
                    }));
                }
            }
        }
        Some(json!(out))
    }
}

fn handle_event(st: &mut State, h: &mut HostConn, ev: RemoteEvent) {
    match ev {
        RemoteEvent::Connected { .. } => {
            h.conn = ConnStatus::Connected;
            retitle(st, h.key, &h.label, h.conn);
        }
        RemoteEvent::State(s) => {
            // A state arriving implies a live link even if `Connected` raced behind it.
            if h.conn == ConnStatus::Connecting || h.conn == ConnStatus::Disconnected {
                h.conn = ConnStatus::Connected;
            }
            let plan = plan_tabs(h.key, &s);
            let mut activate = h.activate;
            let attach = apply_plan(st, h.key, &h.label, h.conn, &plan, &mut activate);
            h.activate = activate;
            for pane_id in attach {
                h.session.attach(&pane_id);
            }
        }
        RemoteEvent::Reset {
            pane_id,
            data,
            cols,
            rows,
        } => apply_reset(st, &make_uid(h.key, &pane_id), &data, cols, rows),
        RemoteEvent::Data { pane_id, data } => apply_data(st, &make_uid(h.key, &pane_id), &data),
        RemoteEvent::Exit { pane_id, code } => {
            apply_exit(st, &make_uid(h.key, &pane_id), code, h.conn)
        }
        RemoteEvent::Liveness { .. } | RemoteEvent::Activity { .. } => {}
        RemoteEvent::Disconnected { .. } => {
            h.conn = ConnStatus::Disconnected;
            retitle(st, h.key, &h.label, h.conn);
        }
        RemoteEvent::AuthFailed => {
            h.conn = ConnStatus::AuthFailed;
            retitle(st, h.key, &h.label, h.conn);
            toast_host(st, h.key, "remote auth failed — re-pair the host");
        }
        RemoteEvent::InputRejected { status, .. } => {
            let msg = match status {
                403 => "host disallows input",
                423 => "remote pane is locked",
                _ => return,
            };
            let now = Instant::now();
            if h.last_input_toast
                .is_some_and(|t| now.duration_since(t) < TOAST_EVERY)
            {
                return;
            }
            h.last_input_toast = Some(now);
            toast_host(st, h.key, msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperpanes_core::remote::{RemoteTab, RemoteWindow};

    #[test]
    fn uid_roundtrip_and_rejects() {
        let u = make_uid(7, "pane-3");
        assert_eq!(u, "remote:7:pane-3");
        assert!(is_remote_uid(&u));
        assert_eq!(parse_uid(&u), Some((7, "pane-3")));
        // Pane ids containing ':' survive.
        assert_eq!(parse_uid("remote:2:a:b"), Some((2, "a:b")));
        assert_eq!(parse_uid("pane-3"), None);
        assert_eq!(parse_uid("remote:x:p"), None);
        assert_eq!(parse_uid("remote:1:"), None);
        assert!(!is_remote_uid("pane-3"));
    }

    #[test]
    fn write_routes_local_to_manager_and_remote_to_session() {
        // A remote uid with no registered host is dropped silently (never reaches the manager);
        // a local uid reaches the manager (no session → no-op, but must not panic).
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let mgr = SessionManager::new(tx);
        write(&mgr, "remote:9999:p", "x");
        write(&mgr, "pane-1", "x");
        kill(&mgr, "remote:9999:p");
        assert!(is_detached("remote:9999:p"));
    }

    fn rp(id: &str, label: &str, status: &str) -> RemotePane {
        RemotePane {
            id: id.into(),
            session_uid: format!("s-{id}"),
            label: label.into(),
            subtitle: Some("sub".into()),
            color: "#89b4fa".into(),
            status: status.into(),
            cols: Some(100),
            rows: Some(30),
            meta: [("secret".to_string(), "TOKEN123".to_string())].into(),
            ..Default::default()
        }
    }

    #[test]
    fn plan_maps_tabs_and_never_carries_meta() {
        let s = RemoteState {
            windows: vec![
                RemoteWindow {
                    window_id: 1,
                    active_tab_id: None,
                    tabs: vec![RemoteTab {
                        id: "1:0".into(),
                        title: "build".into(),
                        layout: "grid".into(),
                        panes: vec![rp("a", "shell", "running"), rp("b", "claude", "exited")],
                    }],
                },
                RemoteWindow {
                    window_id: 2,
                    active_tab_id: None,
                    tabs: vec![RemoteTab {
                        id: "2:0".into(),
                        title: "other".into(),
                        layout: "auto".into(),
                        panes: vec![rp("c", "c", "running")],
                    }],
                },
            ],
        };
        let plan = plan_tabs(4, &s);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].host_tab_id, "1:0");
        assert_eq!(plan[0].panes[0].uid, "remote:4:a");
        assert_eq!(plan[0].panes[0].cols, Some(100));
        assert!(!plan[0].panes[0].exited);
        assert!(plan[0].panes[1].exited);
        assert!(!format!("{plan:?}").contains("TOKEN123"));
    }

    #[test]
    fn titles_and_subtitles_show_connection_state() {
        assert_eq!(
            tab_title("laptop", "build", ConnStatus::Connected),
            "⇄ laptop · build"
        );
        assert!(tab_title("l", "b", ConnStatus::Disconnected).ends_with("(disconnected)"));
        assert!(tab_title("l", "b", ConnStatus::AuthFailed).contains("auth failed"));
        assert_eq!(
            compose_subtitle(ConnStatus::Connected, false, false, None, Some("x")).as_deref(),
            Some("x")
        );
        assert_eq!(
            compose_subtitle(ConnStatus::Connected, true, false, None, None).as_deref(),
            Some("gone")
        );
        assert_eq!(
            compose_subtitle(ConnStatus::Connected, false, true, Some(2), Some("x")).as_deref(),
            Some("exited (2) · x")
        );
        assert_eq!(
            compose_subtitle(ConnStatus::Disconnected, false, false, None, None).as_deref(),
            Some("disconnected")
        );
        assert_eq!(
            compose_subtitle(ConnStatus::AuthFailed, false, false, None, None).as_deref(),
            Some("auth failed — re-pair")
        );
    }

    fn state_with(panes: Vec<RemotePane>) -> RemoteState {
        RemoteState {
            windows: vec![RemoteWindow {
                window_id: 1,
                active_tab_id: None,
                tabs: vec![RemoteTab {
                    id: "1:0".into(),
                    title: "build".into(),
                    layout: "auto".into(),
                    panes,
                }],
            }],
        }
    }

    fn fresh() -> State {
        State::new(theme::load_font(1.0))
    }

    #[test]
    fn apply_plan_creates_pins_marks_gone_and_stays_out_of_the_session_file() {
        let mut st = fresh();
        let local_tabs = st.tabs.len();
        let plan = plan_tabs(
            71,
            &state_with(vec![rp("a", "shell", "running"), rp("b", "x", "running")]),
        );
        let mut activate = true;
        let attach = apply_plan(
            &mut st,
            71,
            "laptop",
            ConnStatus::Connected,
            &plan,
            &mut activate,
        );
        assert_eq!(attach, vec!["a".to_string(), "b".to_string()]);
        assert!(!activate, "activation consumed");
        assert_eq!(st.tabs.len(), local_tabs + 1);
        assert_eq!(st.active, local_tabs, "first remote tab activated");
        let t = &st.tabs[local_tabs];
        assert_eq!(t.title.as_str(), "⇄ laptop · build");
        assert!(t.remote.is_some());
        let p = &t.panes[0];
        assert_eq!(p.uid, "remote:71:a");
        assert_eq!(p.pane.grid_size(), (100, 30), "grid pinned to host dims");
        assert_eq!(p.subtitle.as_ref().map(|s| s.as_str()), Some("sub"));

        // Remote tabs are never persisted.
        let file = st.to_session_file();
        let uids: Vec<String> = file
            .groups
            .unwrap_or_default()
            .iter()
            .flat_map(|g| g.panes.iter().filter_map(|p| p.uid.clone()))
            .collect();
        assert!(uids.iter().all(|u| !is_remote_uid(u)), "{uids:?}");
        assert!(st.session_uids().iter().all(|u| !is_remote_uid(u)));

        // Host dims change → grid follows; vanished pane stays, marked gone.
        let mut a = rp("a", "shell", "running");
        a.cols = Some(90);
        a.rows = Some(20);
        let plan = plan_tabs(71, &state_with(vec![a]));
        let mut no = false;
        let attach = apply_plan(&mut st, 71, "laptop", ConnStatus::Connected, &plan, &mut no);
        assert!(attach.is_empty());
        let t = &st.tabs[local_tabs];
        assert_eq!(t.panes.len(), 2, "vanished pane is kept");
        assert_eq!(t.panes[0].pane.grid_size(), (90, 20));
        assert!(t.panes[1].remote.as_ref().unwrap().gone);
        assert_eq!(
            t.panes[1].subtitle.as_ref().map(|s| s.as_str()),
            Some("gone · sub")
        );

        // Disconnected → visible marker, tabs kept.
        retitle(&mut st, 71, "laptop", ConnStatus::Disconnected);
        assert!(st.tabs[local_tabs].title.ends_with("(disconnected)"));

        // Closing a remote pane detaches it and it is not re-added by the next state.
        let uid = "remote:71:a".to_string();
        let (m_tx, _m_rx) = tokio::sync::mpsc::unbounded_channel();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _g = rt.enter();
        let mgr = SessionManager::new(m_tx);
        kill(&mgr, &uid);
        let (ti, pi) = st.find_pane(&uid).unwrap();
        st.close_pane_in(ti, pi, &mgr);
        assert!(st.find_pane(&uid).is_none());
        let plan = plan_tabs(71, &state_with(vec![rp("a", "shell", "running")]));
        apply_plan(&mut st, 71, "laptop", ConnStatus::Connected, &plan, &mut no);
        assert!(
            st.find_pane(&uid).is_none(),
            "detached pane must not reappear"
        );

        // Disconnect removes the host's tabs.
        st.remove_remote_tabs(71);
        assert!(st.tabs.iter().all(|t| t.remote.is_none()));
    }

    #[test]
    fn reset_recreates_the_emulator_and_data_appends_without_replies() {
        let mut st = fresh();
        let plan = plan_tabs(72, &state_with(vec![rp("a", "s", "running")]));
        let mut act = false;
        apply_plan(&mut st, 72, "h", ConnStatus::Connected, &plan, &mut act);
        apply_reset(&mut st, "remote:72:a", "hello world", Some(60), Some(10));
        let (ti, pi) = st.find_pane("remote:72:a").unwrap();
        assert_eq!(st.tabs[ti].panes[pi].pane.grid_size(), (60, 10));
        assert!(st.tabs[ti].panes[pi]
            .pane
            .screen_text()
            .contains("hello world"));
        apply_data(&mut st, "remote:72:a", "\r\nmore");
        assert!(st.tabs[ti].panes[pi].pane.screen_text().contains("more"));
        // A DSR request must NOT leave a reply queued for write-back.
        apply_data(&mut st, "remote:72:a", "\x1b[6n");
        assert!(st.tabs[ti].panes[pi].pane.take_replies().is_empty());
        // A second Reset clears what was there.
        apply_reset(&mut st, "remote:72:a", "fresh", None, None);
        let txt = st.tabs[ti].panes[pi].pane.screen_text();
        assert!(txt.contains("fresh") && !txt.contains("hello"));
        apply_exit(&mut st, "remote:72:a", Some(3), ConnStatus::Connected);
        assert_eq!(
            st.tabs[ti].panes[pi].subtitle.as_ref().map(|s| s.as_str()),
            Some("exited (3) · sub")
        );
    }

    #[test]
    fn dims_are_clamped() {
        assert_eq!(clamp_dims(0, 0), (2, 1));
        assert_eq!(clamp_dims(9999, 9999), (500, 300));
        assert_eq!(clamp_dims(120, 40), (120, 40));
    }
}
