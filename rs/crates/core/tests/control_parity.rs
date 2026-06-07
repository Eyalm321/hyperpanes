//! End-to-end control-server parity test from an EXTERNAL crate (so it only sees the public API +
//! std — no tokio/reqwest, which `tests/` crates don't get). Boots the real axum stack via
//! `control::server::serve_for_test`, seeds the read-model through the public types, and drives it
//! over a raw loopback socket, asserting byte-exact response bodies against the
//! `src/main/control-server.ts` oracle (modulo the dynamic port/token/pid).

use std::io::{Read, Write};
use std::net::TcpStream;

use hyperpanes_core::control::readmodel::{PaneInfo, PaneStatus, TabInfo, WindowInfo};
use hyperpanes_core::control::server::serve_for_test;

/// Minimal HTTP/1.1 request over loopback. `Connection: close` lets us read the whole response to
/// EOF, then split status + body. Returns (status_code, body).
fn request(port: u16, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n");
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).expect("write");
    let mut resp = String::new();
    stream.read_to_string(&mut resp).expect("read");
    let status: u16 = resp
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .expect("status code");
    let body = resp.splitn(2, "\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

fn boot() -> u16 {
    let control_file = std::env::temp_dir().join(format!("hp-parity-it-{}.json", std::process::id()));
    let token = "it-master-token";
    let (shared, port) = serve_for_test(control_file, true, token).expect("serve");
    // Seed one window with one pane through the public read-model API.
    shared.model.lock().unwrap().add_window(WindowInfo {
        window_id: 1,
        active_tab_id: Some("t1".into()),
        tabs: vec![TabInfo {
            id: "t1".into(),
            title: "Tab 1".into(),
            layout: "auto".into(),
            panes: vec![PaneInfo {
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
            }],
        }],
    });
    port
}

const TOKEN: &str = "it-master-token";

#[test]
fn health_is_reachable_without_auth() {
    let port = boot();
    let (status, body) = request(port, "GET", "/health", None, None);
    assert_eq!(status, 200);
    assert!(body.contains(r#""ok":true"#));
    assert!(body.contains(r#""app":"hyperpanes""#));
    assert!(body.contains(r#""allowInput":true"#));
}

#[test]
fn state_is_byte_exact_over_a_real_socket() {
    let port = boot();
    let (status, body) = request(port, "GET", "/state", Some(TOKEN), None);
    assert_eq!(status, 200);
    assert_eq!(
        body,
        r##"{"windows":[{"windowId":1,"activeTabId":"t1","tabs":[{"id":"t1","title":"Tab 1","layout":"auto","panes":[{"id":"p1","sessionUid":"u1","label":"shell","color":"#3b82f6","status":"running","activity":"busy"}]}]}]}"##
    );
}

#[test]
fn unauthorized_state_is_401() {
    let port = boot();
    let (status, body) = request(port, "GET", "/state", None, None);
    assert_eq!(status, 401);
    assert_eq!(body, r#"{"error":"unauthorized"}"#);
}

#[test]
fn missing_pane_is_404_with_exact_shape() {
    let port = boot();
    let (status, body) = request(port, "GET", "/panes/ghost/output", Some(TOKEN), None);
    assert_eq!(status, 404);
    assert_eq!(body, r#"{"error":"no such pane","paneId":"ghost"}"#);
}

#[test]
fn unknown_path_is_404_not_found() {
    let port = boot();
    let (status, body) = request(port, "GET", "/no/such/route", Some(TOKEN), None);
    assert_eq!(status, 404);
    assert_eq!(body, r#"{"error":"not found","path":"/no/such/route"}"#);
}
