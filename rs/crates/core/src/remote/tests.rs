//! Integration tests: the remote client against the REAL in-process axum control stack
//! (same boot as `control::routes::golden`), with fake ptys (no shells, no GUI).

use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;

use super::client::*;
use super::pairing::RemoteTarget;
use crate::control::input::submit_newlines;
use crate::control::readmodel::{PaneInfo, PaneStatus, TabInfo, WindowInfo};
use crate::control::server::{forward_session_events, notify_state, Shared};
use crate::session::pty::{Pty, PtyEvent};
use crate::session_manager::{EventSink, SessionManager, SpawnFn, SpawnOptions};

const MASTER: &str = "tok-master-example";

fn fast() -> SessionConfig {
    SessionConfig {
        backoff_min: Duration::from_millis(50),
        backoff_max: Duration::from_millis(200),
        state_poll: Duration::from_millis(200),
        state_debounce: Duration::from_millis(30),
        ws_connect_timeout: Duration::from_secs(2),
        ping_every: Duration::from_secs(1),
    }
}

// ---- fake pty + host ---------------------------------------------------------------------

struct FakePty {
    written: Arc<Mutex<Vec<u8>>>,
}

impl Pty for FakePty {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        self.written.lock().unwrap().extend_from_slice(data);
        Ok(())
    }
    fn resize(&self, _: u16, _: u16) -> io::Result<()> {
        Ok(())
    }
    fn kill(&self) -> io::Result<()> {
        Ok(())
    }
}

struct FakePane {
    uid: String,
    sink: EventSink,
    written: Arc<Mutex<Vec<u8>>>,
}

impl FakePane {
    fn output(&self, s: &str) {
        (self.sink)(PtyEvent::Data(s.as_bytes().to_vec()));
    }
}

struct ServerHandle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop accepting AND drop every live connection (the runtime owning them dies).
    fn stop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Host {
    shared: Arc<Shared>,
    port: u16,
    server: Mutex<Option<ServerHandle>>,
}

fn bind(port: u16) -> TcpListener {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => return l,
            Err(e) if std::time::Instant::now() > deadline => panic!("bind {port}: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn spawn_server(shared: Arc<Shared>, listener: TcpListener) -> ServerHandle {
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            listener.set_nonblocking(true).unwrap();
            let l = tokio::net::TcpListener::from_std(listener).unwrap();
            let app = crate::control::routes::router(shared);
            tokio::select! {
                _ = axum::serve(l, app) => {}
                _ = stop_rx => {}
            }
        });
        // Runtime drop aborts all in-flight connection tasks → sockets close.
    });
    ServerHandle {
        stop: Some(stop_tx),
        thread: Some(thread),
    }
}

impl Host {
    async fn boot(allow_input: bool) -> Arc<Host> {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sessions = Arc::new(SessionManager::new(tx));
        let dir = std::env::temp_dir();
        let shared = Shared::new(
            sessions,
            allow_input,
            "0.0.0-test",
            dir.join(format!(
                "hp-remote-test-{}-{n}-control.json",
                std::process::id()
            )),
            dir.join(format!(
                "hp-remote-test-{}-{n}-speech.json",
                std::process::id()
            )),
        );
        tokio::spawn(forward_session_events(Arc::clone(&shared), rx));
        shared.model.lock().unwrap().add_window(WindowInfo {
            window_id: 1,
            active_tab_id: Some("t1".into()),
            tabs: vec![TabInfo {
                id: "t1".into(),
                title: "Tab 1".into(),
                color: None,
                layout: "auto".into(),
                panes: vec![],
            }],
        });
        shared.tokens.lock().unwrap().set_master(MASTER.to_string());
        let listener = bind(0);
        let port = listener.local_addr().unwrap().port();
        shared.port.store(port, Ordering::SeqCst);
        let server = spawn_server(Arc::clone(&shared), listener);
        Arc::new(Host {
            shared,
            port,
            server: Mutex::new(Some(server)),
        })
    }

    fn target(&self, token: &str) -> RemoteTarget {
        RemoteTarget::new("127.0.0.1", self.port, token).unwrap()
    }

    fn add_pane(&self, id: &str, uid: &str, cols: u16, rows: u16) -> FakePane {
        let written = Arc::new(Mutex::new(Vec::new()));
        let slot: Arc<Mutex<Option<EventSink>>> = Arc::new(Mutex::new(None));
        let (w2, s2) = (Arc::clone(&written), Arc::clone(&slot));
        let factory: SpawnFn = Box::new(move |_spec, sink| {
            *s2.lock().unwrap() = Some(sink);
            Ok(Box::new(FakePty { written: w2 }) as Box<dyn Pty>)
        });
        self.shared
            .sessions
            .create_with(
                SpawnOptions {
                    uid: uid.into(),
                    cols: Some(cols),
                    rows: Some(rows),
                    ..Default::default()
                },
                factory,
            )
            .unwrap();
        self.shared.model.lock().unwrap().insert_pane(
            1,
            PaneInfo {
                id: id.into(),
                session_uid: uid.into(),
                label: format!("label-{id}"),
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
            },
        );
        notify_state(&self.shared);
        let sink = slot.lock().unwrap().clone().unwrap();
        FakePane {
            uid: uid.into(),
            sink,
            written,
        }
    }

    /// Emit output and wait until the host has flushed it (cursor advanced).
    async fn produce(&self, pane: &FakePane, s: &str) {
        let before = self.shared.sessions.output_bytes(&pane.uid).unwrap_or(0);
        pane.output(s);
        let want = before + s.encode_utf16().count() as u64;
        for _ in 0..400 {
            if self.shared.sessions.output_bytes(&pane.uid).unwrap_or(0) >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("host never flushed output");
    }

    fn stop_server(&self) {
        if let Some(mut s) = self.server.lock().unwrap().take() {
            s.stop();
        }
    }

    fn start_server(&self) {
        let l = bind(self.port);
        *self.server.lock().unwrap() = Some(spawn_server(Arc::clone(&self.shared), l));
    }
}

// ---- event helpers -----------------------------------------------------------------------

async fn next(rx: &mut UnboundedReceiver<RemoteEvent>) -> RemoteEvent {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("timed out waiting for a remote event")
        .expect("event channel closed")
}

async fn wait_for<F: Fn(&RemoteEvent) -> bool>(
    rx: &mut UnboundedReceiver<RemoteEvent>,
    pred: F,
) -> RemoteEvent {
    loop {
        let e = next(rx).await;
        if pred(&e) {
            return e;
        }
    }
}

/// A trivial "emulator": Reset replaces, Data appends. Records the event kinds seen.
#[derive(Default)]
struct Screen {
    text: String,
    kinds: Vec<&'static str>,
}

impl Screen {
    fn apply(&mut self, pane: &str, e: &RemoteEvent) {
        match e {
            RemoteEvent::Reset { pane_id, data, .. } if pane_id == pane => {
                self.text = data.clone();
                self.kinds.push("reset");
            }
            RemoteEvent::Data { pane_id, data } if pane_id == pane => {
                self.text.push_str(data);
                self.kinds.push("data");
            }
            _ => {}
        }
    }

    async fn until_len(
        &mut self,
        rx: &mut UnboundedReceiver<RemoteEvent>,
        pane: &str,
        units: usize,
    ) {
        while self.text.encode_utf16().count() < units {
            let e = next(rx).await;
            self.apply(pane, &e);
        }
    }

    /// Keep consuming for `ms` (to catch duplicates).
    async fn settle(&mut self, rx: &mut UnboundedReceiver<RemoteEvent>, pane: &str, ms: u64) {
        let end = tokio::time::Instant::now() + Duration::from_millis(ms);
        while let Ok(Some(e)) = tokio::time::timeout_at(end, rx.recv()).await {
            self.apply(pane, &e);
        }
    }
}

// ---- a) state sync -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_state_sync_matches_server_incl_dims() {
    let host = Host::boot(true).await;
    let _p1 = host.add_pane("p1", "s1", 100, 30);
    let _p2 = host.add_pane("p2", "s2", 80, 24);
    let (_s, mut rx) = RemoteSession::start(host.target(MASTER), fast());

    let st = match wait_for(&mut rx, |e| matches!(e, RemoteEvent::State(_))).await {
        RemoteEvent::State(s) => s,
        _ => unreachable!(),
    };
    let panes: Vec<_> = st.panes().collect();
    assert_eq!(panes.len(), 2);
    let p1 = st.pane("p1").unwrap();
    assert_eq!((p1.cols, p1.rows), (Some(100), Some(30)));
    assert_eq!(p1.label, "label-p1");
    assert_eq!(p1.session_uid, "s1");
    assert_eq!(p1.status, "running");
    assert!(!p1.exited());
    let p2 = st.pane("p2").unwrap();
    assert_eq!((p2.cols, p2.rows), (Some(80), Some(24)));
    assert_eq!(st.windows[0].tabs[0].id, "t1");
    assert_eq!(st.windows[0].active_tab_id.as_deref(), Some("t1"));

    // The direct HTTP view agrees with the event view.
    let direct = RemoteClient::new(&host.target(MASTER))
        .get_state()
        .await
        .unwrap();
    assert_eq!(direct, st);
}

#[test]
fn state_model_is_lenient() {
    let s: RemoteState = serde_json::from_str(
        r#"{"windows":[{"windowId":2,"activeTabId":null,"future":1,"tabs":[{"id":"t","panes":[{"id":"p","unknownThing":{"x":1},"cols":null}]}]}],"speech":{"muted":true}}"#,
    )
    .unwrap();
    let p = s.pane("p").unwrap();
    assert_eq!(p.cols, None);
    assert_eq!(p.label, "");
    assert!(serde_json::from_str::<RemoteState>("{}")
        .unwrap()
        .windows
        .is_empty());
}

// ---- b) device token (minted exactly like `hyperpanes pair`) -----------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn b_device_token_opens_state_events_output_input() {
    let host = Host::boot(true).await;
    let pane = host.add_pane("p1", "s1", 90, 25);
    host.produce(&pane, "hello").await;

    // pair.rs::mint_device: POST /devices {label, ttlMs?} with the MASTER bearer.
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/devices", host.port))
        .bearer_auth(MASTER)
        .json(&serde_json::json!({ "label": "test-device", "ttlMs": 3_600_000 }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let v: serde_json::Value = resp.json().await.unwrap();
    let device = v["token"].as_str().unwrap().to_string();
    assert_ne!(device, MASTER);

    // Round-trip the way the phone does: through the pairing URL.
    let url = format!("hp://127.0.0.1:{}/?token={device}&v=1", host.port);
    let target = RemoteTarget::parse(&url).unwrap();

    let client = RemoteClient::new(&target);
    assert_eq!(client.get_state().await.unwrap().panes().count(), 1);
    let out = client.get_output("p1", None).await.unwrap();
    assert_eq!((out.output.as_str(), out.cursor), ("hello", 5));
    client.send_input("p1", "x").await.unwrap();

    let (session, mut rx) = RemoteSession::start(target, fast());
    let hello = wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;
    assert_eq!(
        hello,
        RemoteEvent::Connected {
            version: "0.0.0-test".into()
        }
    );
    session.attach("p1");
    let mut scr = Screen::default();
    scr.until_len(&mut rx, "p1", 5).await;
    assert_eq!(scr.text, "hello");
    host.produce(&pane, " live").await;
    scr.until_len(&mut rx, "p1", 10).await;
    assert_eq!(scr.text, "hello live");

    // A revoked device token stops working.
    reqwest::Client::new()
        .delete(format!(
            "http://127.0.0.1:{}/devices?label=test-device",
            host.port
        ))
        .bearer_auth(MASTER)
        .send()
        .await
        .unwrap();
    assert_eq!(
        client.get_state().await.unwrap_err(),
        RemoteError::Unauthorized
    );
}

// ---- c) attach splice --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn c_attach_splice_before_during_after_exactly_once_in_order() {
    let host = Host::boot(true).await;
    let pane = host.add_pane("p1", "s1", 80, 24);
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;

    let mut expected = String::new();
    // before attach
    for i in 0..20 {
        let line = format!("pre{i:03}|héllo🚀\n");
        host.produce(&pane, &line).await;
        expected.push_str(&line);
    }
    // during attach: a producer racing the snapshot
    let racer_host = Arc::clone(&host);
    let racer_pane = FakePane {
        uid: pane.uid.clone(),
        sink: Arc::clone(&pane.sink),
        written: Arc::clone(&pane.written),
    };
    let racer = tokio::spawn(async move {
        let mut s = String::new();
        for i in 0..120 {
            let line = format!("mid{i:03}|\n");
            racer_host.produce(&racer_pane, &line).await;
            s.push_str(&line);
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        s
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    session.attach("p1");
    expected.push_str(&racer.await.unwrap());
    // after attach
    for i in 0..20 {
        let line = format!("post{i:03}\n");
        host.produce(&pane, &line).await;
        expected.push_str(&line);
    }

    let mut scr = Screen::default();
    scr.until_len(&mut rx, "p1", expected.encode_utf16().count())
        .await;
    scr.settle(&mut rx, "p1", 300).await; // any duplicate would surface here
    assert_eq!(scr.text, expected);
    assert_eq!(scr.kinds.iter().filter(|k| **k == "reset").count(), 1);

    // Detached panes get nothing.
    session.detach("p1");
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.produce(&pane, "after-detach").await;
    let mut late = Screen::default();
    late.settle(&mut rx, "p1", 300).await;
    assert!(late.kinds.is_empty(), "detached pane still streamed");
}

// ---- d) reconnect: delta + truncated -----------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn d_reconnect_delivers_exact_delta_then_reset_when_truncated() {
    let host = Host::boot(true).await;
    let pane = host.add_pane("p1", "s1", 80, 24);
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    session.attach("p1");
    let mut scr = Screen::default();
    host.produce(&pane, "one|").await;
    scr.until_len(&mut rx, "p1", 4).await;
    assert_eq!(scr.text, "one|");

    host.stop_server();
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Disconnected { .. })).await;
    host.produce(&pane, "two|").await;
    host.produce(&pane, "three|").await;
    host.start_server();
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;
    scr.until_len(&mut rx, "p1", "one|two|three|".len()).await;
    scr.settle(&mut rx, "p1", 300).await;
    assert_eq!(scr.text, "one|two|three|");
    // The initial Reset may or may not already hold "one|" (attach/produce race), but the
    // reconnect itself must be a pure delta: exactly one Reset (the attach), never a second.
    assert_eq!(
        scr.kinds.iter().filter(|k| **k == "reset").count(),
        1,
        "reconnect must be a pure delta: {:?}",
        scr.kinds
    );

    // Truncated path: more than the host's replay buffer while we're away.
    host.stop_server();
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Disconnected { .. })).await;
    let chunk = "y".repeat(16 * 1024);
    for _ in 0..12 {
        host.produce(&pane, &chunk).await; // 192K > 128K replay cap
    }
    host.produce(&pane, "TAIL").await;
    host.start_server();
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;
    let reset = wait_for(&mut rx, |e| matches!(e, RemoteEvent::Reset { .. })).await;
    let RemoteEvent::Reset {
        data, cols, rows, ..
    } = reset
    else {
        unreachable!()
    };
    assert_eq!((cols, rows), (Some(80), Some(24)));
    assert_eq!(Some(data.clone()), host.shared.sessions.replay("s1"));
    assert!(data.ends_with("TAIL"));
    assert!(data.len() < 12 * 16 * 1024);
}

// ---- e) output backfill via /output ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e_output_backfill_and_since() {
    let host = Host::boot(true).await;
    let pane = host.add_pane("p1", "s1", 80, 24);
    host.produce(&pane, "héllo 🚀 ").await; // 9 UTF-16 units
    let client = RemoteClient::new(&host.target(MASTER));
    let full = client.get_output("p1", None).await.unwrap();
    assert_eq!(full.output, "héllo 🚀 ");
    assert_eq!(full.cursor, 9);
    assert!(!full.truncated);

    host.produce(&pane, "world").await;
    let d = client.get_output("p1", Some(9)).await.unwrap();
    assert_eq!(
        (d.output.as_str(), d.cursor, d.truncated),
        ("world", 14, false)
    );
    let none = client.get_output("p1", Some(14)).await.unwrap();
    assert_eq!((none.output.as_str(), none.cursor), ("", 14));

    // Attach of a pane with history seeds via Reset (backfill) before any live data.
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    session.attach("p1");
    let r = wait_for(&mut rx, |e| matches!(e, RemoteEvent::Reset { .. })).await;
    assert_eq!(
        r,
        RemoteEvent::Reset {
            pane_id: "p1".into(),
            data: "héllo 🚀 world".into(),
            cols: Some(80),
            rows: Some(24)
        }
    );
    assert_eq!(
        client.get_output("nope", None).await.unwrap_err(),
        RemoteError::NotFound
    );
}

// ---- f) ordered input --------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f_input_arrives_byte_exact_and_in_order() {
    let host = Host::boot(true).await;
    let pane = host.add_pane("p1", "s1", 80, 24);
    let (session, _rx) = RemoteSession::start(host.target(MASTER), fast());
    let input = session.input_sender("p1");

    let parts = [
        "héllo 世界 🚀",
        "\x1b[200~line1\nline2\x1b[201~",
        "\r",
        "\x03",
        "\x1b[A",
    ];
    let platform = if cfg!(windows) { "win32" } else { "linux" };
    let mut expected = Vec::new();
    for p in parts {
        assert!(input.send(p));
        // a Windows HOST rewrites \n→\r in `data` itself; the client must not.
        expected.extend_from_slice(submit_newlines(p, platform).as_bytes());
    }
    assert!(input.send_keys(vec!["enter".into()]));
    expected.push(b'\r');
    // plus a rapid burst to exercise coalescing
    for i in 0..150 {
        let s = format!("{}|", i);
        assert!(input.send(s.clone()));
        expected.extend_from_slice(s.as_bytes());
    }

    wait_until(|| pane.written.lock().unwrap().len() >= expected.len()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = pane.written.lock().unwrap().clone();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&expected)
    );
    assert_eq!(got, expected);
    // second handle for the same pane shares the queue
    assert!(session.input_sender("p1").send("z"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f2_input_rejected_when_host_disallows_input() {
    let host = Host::boot(false).await;
    let _pane = host.add_pane("p1", "s1", 80, 24);
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;
    session.input_sender("p1").send("nope");
    let e = wait_for(&mut rx, |e| matches!(e, RemoteEvent::InputRejected { .. })).await;
    let RemoteEvent::InputRejected {
        pane_id, status, ..
    } = e
    else {
        unreachable!()
    };
    assert_eq!((pane_id.as_str(), status), ("p1", 403));
    // session survives
    session.attach("p1");
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Reset { .. })).await;
}

async fn wait_until<F: Fn() -> bool>(f: F) {
    for _ in 0..600 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition never became true");
}

// ---- g) auth failure ---------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn g_bad_token_is_auth_failed_and_does_not_retry() {
    let host = Host::boot(true).await;
    let (session, mut rx) = RemoteSession::start(host.target("wrong-secret-token"), fast());
    assert_eq!(next(&mut rx).await, RemoteEvent::AuthFailed);
    // ~12 backoff periods pass: no retry loop, no Disconnected, no second AuthFailed.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(rx.try_recv().is_err(), "session kept retrying after 401");
    // Explicit re-arm tries exactly once more.
    session.reconnect_now();
    assert_eq!(next(&mut rx).await, RemoteEvent::AuthFailed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rx.try_recv().is_err());
    // HTTP view of the same thing
    let c = RemoteClient::new(&host.target("wrong-secret-token"));
    assert_eq!(c.get_state().await.unwrap_err(), RemoteError::Unauthorized);
}

// ---- h) host gone ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn h_host_gone_backs_off_without_busy_loop() {
    // A listener that accepts then immediately closes (WS handshake fails) and counts attempts.
    let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    let accepts = Arc::new(AtomicUsize::new(0));
    let a2 = Arc::clone(&accepts);
    std::thread::spawn(move || {
        for s in l.incoming() {
            a2.fetch_add(1, Ordering::SeqCst);
            drop(s);
        }
    });
    let target = RemoteTarget::new("127.0.0.1", port, "secret-xyz").unwrap();
    let (_session, mut rx) = RemoteSession::start(target, fast());
    let e = next(&mut rx).await;
    let RemoteEvent::Disconnected { reason } = e else {
        panic!("{e:?}")
    };
    assert!(!reason.contains("secret-xyz"), "token leaked: {reason}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let n = accepts.load(Ordering::SeqCst);
    // backoff 50→200ms (+jitter): ≈10 attempts in 1.5s; a busy loop would be hundreds.
    assert!((3..=40).contains(&n), "attempts = {n}");
    // Disconnected announced once per outage, not per attempt.
    assert!(rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn i_reconnect_now_skips_the_backoff_wait() {
    let host = Host::boot(true).await;
    host.stop_server();
    let cfg = SessionConfig {
        backoff_min: Duration::from_secs(30),
        backoff_max: Duration::from_secs(30),
        ..fast()
    };
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), cfg);
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Disconnected { .. })).await;
    host.start_server();
    session.reconnect_now();
    let t0 = std::time::Instant::now();
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Connected { .. })).await;
    assert!(t0.elapsed() < Duration::from_secs(5));
}

// ---- state polling / respawn / vanish ------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn j_state_updates_flow_and_vanished_pane_exits() {
    let host = Host::boot(true).await;
    let _p1 = host.add_pane("p1", "s1", 80, 24);
    let (session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::State(_))).await;
    session.attach("p1");
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::Reset { .. })).await;

    // a new pane appears → State event (ping or poll)
    let _p2 = host.add_pane("p2", "s2", 120, 40);
    let st = wait_for(
        &mut rx,
        |e| matches!(e, RemoteEvent::State(s) if s.pane("p2").is_some()),
    )
    .await;
    let RemoteEvent::State(st) = st else {
        unreachable!()
    };
    assert_eq!(st.pane("p2").unwrap().cols, Some(120));

    // the attached pane disappears → Exit{code: None}
    host.shared.model.lock().unwrap().remove_pane("p1");
    notify_state(&host.shared);
    let e = wait_for(&mut rx, |e| matches!(e, RemoteEvent::Exit { .. })).await;
    assert_eq!(
        e,
        RemoteEvent::Exit {
            pane_id: "p1".into(),
            code: None
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn k_no_state_polling_when_nothing_attached() {
    let host = Host::boot(true).await;
    let _p = host.add_pane("p1", "s1", 80, 24);
    let (_session, mut rx) = RemoteSession::start(host.target(MASTER), fast());
    wait_for(&mut rx, |e| matches!(e, RemoteEvent::State(_))).await;
    // Nothing attached + nothing changing → silence (and no duplicate State for unchanged data).
    tokio::time::sleep(Duration::from_millis(800)).await;
    let mut n = 0;
    while let Ok(e) = rx.try_recv() {
        if matches!(e, RemoteEvent::State(_)) {
            n += 1;
        }
    }
    assert_eq!(n, 0);
}

// ---- misc unit ---------------------------------------------------------------------------

#[test]
fn splice_semantics() {
    use super::client::splice;
    assert_eq!(splice(10, 10, "abc".into()), None); // already seen
    assert_eq!(splice(10, 8, "abc".into()), None);
    assert_eq!(splice(10, 13, "abc".into()), Some("abc".into())); // exactly next
    assert_eq!(splice(11, 13, "abc".into()), Some("bc".into())); // partial overlap
    assert_eq!(splice(0, 4, "a🚀".into()), Some("a🚀".into())); // 3 units, start=1
    let m: HashMap<u8, u8> = HashMap::new();
    assert!(m.is_empty());
}

#[test]
fn token_never_in_debug_output() {
    let t = RemoteTarget::new("127.0.0.1", 9, "supersecret").unwrap();
    let c = RemoteClient::new(&t);
    assert!(!format!("{c:?}").contains("supersecret"));
    assert!(!format!("{t:?}{t}").contains("supersecret"));
}
