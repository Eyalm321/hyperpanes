//! Binary-level tests for `hyperpanes worker` surviving a control-server relaunch: the
//! relaunched server binds a NEW port and mints a NEW token (rewriting control.json); the
//! worker must re-read it, ack with its ORIGINAL fence, keep heartbeating, and still give up
//! after the reconnect window when nothing comes back.
//!
//! Everything is scratch-isolated (HYPERPANES_CONTROL_FILE + HOME/XDG_*), so nothing can touch a
//! developer's live app.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// (request path, Authorization header, body), in arrival order.
type Records = Arc<Mutex<Vec<(String, String, String)>>>;

struct Mock {
    port: u16,
    records: Records,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Mock {
    /// `claims`: successive claim responses (then `{"tasks":[]}`). Any request whose bearer
    /// token isn't `token` gets 401.
    fn start(token: &str, claims: Vec<String>) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        let records: Records = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let claims = Arc::new(Mutex::new(std::collections::VecDeque::from(claims)));
        let want = format!("Bearer {token}");
        let (rec, stop2) = (Arc::clone(&records), Arc::clone(&stop));
        let handle = std::thread::spawn(move || {
            for conn in listener.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break; // listener dropped on return → connects are refused
                }
                let Ok(mut s) = conn else { continue };
                let Some((path, auth, body)) = read_request(&mut s) else {
                    continue;
                };
                rec.lock().unwrap().push((path.clone(), auth.clone(), body));
                let (status, resp) = if auth != want {
                    (
                        "401 Unauthorized",
                        r#"{"error":"unauthorized"}"#.to_string(),
                    )
                } else if path.contains("/claim") {
                    (
                        "200 OK",
                        claims
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| r#"{"tasks":[]}"#.to_string()),
                    )
                } else if path.ends_with("/extend") {
                    (
                        "200 OK",
                        format!(r#"{{"ok":true,"visibilityDeadline":{}}}"#, now_ms() + 3000),
                    )
                } else if path.ends_with("/nack") {
                    ("200 OK", r#"{"state":"queued"}"#.to_string())
                } else {
                    ("200 OK", r#"{"ok":true}"#.to_string())
                };
                let _ = write!(
                    s,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                    resp.len()
                );
            }
        });
        Mock {
            port,
            records,
            stop,
            handle: Some(handle),
        }
    }

    /// Stop serving and drop the listener so later connects are refused.
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // wake accept()
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }

    fn recs(&self) -> Vec<(String, String, String)> {
        self.records.lock().unwrap().clone()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Parse one HTTP request off the socket: returns (path, authorization header, body).
fn read_request(s: &mut TcpStream) -> Option<(String, String, String)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p;
                }
            }
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let header = |name: &str| -> Option<String> {
        head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let len: usize = header("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let auth = header("authorization").unwrap_or_default();
    let body_start = header_end + 4;
    while buf.len() < body_start + len {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let body = String::from_utf8_lossy(&buf[body_start..body_start + len]).to_string();
    Some((path, auth, body))
}

fn claim_task(id: &str) -> String {
    format!(
        r#"{{"tasks":[{{"id":"{id}","title":"t","payload":"","fencingToken":7,"visibilityDeadline":{},"attempts":1,"maxAttempts":3}}]}}"#,
        now_ms() + 3000
    )
}

fn write_control(path: &Path, port: u16, token: &str) {
    // write-then-rename so the worker never reads a half-written file
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, format!(r#"{{"port":{port},"token":"{token}"}}"#)).unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hp-relaunch-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn spawn_worker(dir: &Path, extra_env: &[(&str, &str)], script: &str) -> std::process::Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hyperpanes"));
    c.current_dir(dir)
        .env("HYPERPANES_CONTROL_FILE", dir.join("control.json"))
        .env("HOME", dir.join("home"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_RUNTIME_DIR", dir.join("run"));
    for (k, v) in extra_env {
        c.env(k, v);
    }
    c.args([
        "worker", "--queue", "q", "--worker", "w", "--", "sh", "-c", script,
    ])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("worker binary runs")
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn has(m: &Mock, suffix: &str) -> bool {
    m.recs().iter().any(|(p, _, _)| p.ends_with(suffix))
}

/// Tests a + b: the claim carries holderPid; after a relaunch (new port + new token) the
/// heartbeat recovers and the ack arrives at the new server with the ORIGINAL fence + new token.
#[test]
fn worker_acks_original_fence_after_server_relaunch() {
    let dir = scratch("a");
    let go = dir.join("go");
    let mut m1 = Mock::start("t1", vec![claim_task("aaaa1111-0000")]);
    write_control(&dir.join("control.json"), m1.port, "t1");

    let script = format!("while [ ! -f {} ]; do sleep 0.1; done", go.display());
    let child = spawn_worker(&dir, &[], &script);
    let pid = child.id();

    wait_for("claim on server 1", Duration::from_secs(20), || {
        has(&m1, "/claim")
    });
    // Relaunch: old server gone, new one on a new port with a new token.
    m1.shutdown();
    let m2 = Mock::start("t2", vec![]);
    write_control(&dir.join("control.json"), m2.port, "t2");

    // (b) the heartbeat recovers rather than dying: extend with the original fence on server 2.
    wait_for("extend on server 2", Duration::from_secs(20), || {
        m2.recs().iter().any(|(p, a, b)| {
            p.ends_with("/extend") && a == "Bearer t2" && b.contains("\"fencingToken\":7")
        })
    });
    std::fs::write(&go, "").unwrap();

    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "worker exits 0 after drain: {stderr}");

    // (a) claim body carried the worker's pid.
    let claim = m1
        .recs()
        .into_iter()
        .find(|(p, _, _)| p.ends_with("/claim"))
        .expect("claim recorded");
    assert!(
        claim.2.contains(&format!("\"holderPid\":{pid}")),
        "claim body: {}",
        claim.2
    );
    // ack landed on server 2 with the original fence and the new token.
    let recs = m2.recs();
    let ack = recs
        .iter()
        .find(|(p, _, _)| p.ends_with("/ack") && p.contains("aaaa1111"))
        .unwrap_or_else(|| panic!("no ack on server 2: {recs:?}\nstderr: {stderr}"));
    assert_eq!(ack.1, "Bearer t2");
    assert!(ack.2.contains("\"fencingToken\":7"), "ack body: {}", ack.2);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test d: with a tiny reconnect window and no server coming back, the worker gives up
/// (non-zero) in bounded time instead of hanging.
#[test]
fn worker_gives_up_after_reconnect_window() {
    let dir = scratch("d");
    let mut m1 = Mock::start("t1", vec![claim_task("dddd1111-0000")]);
    write_control(&dir.join("control.json"), m1.port, "t1");

    let child = spawn_worker(&dir, &[("HP_WORKER_RECONNECT_SECS", "2")], "sleep 1");
    wait_for("claim", Duration::from_secs(20), || has(&m1, "/claim"));
    m1.shutdown(); // never comes back

    let t = Instant::now();
    let out = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must exit non-zero: {stderr}");
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "gave up in bounded time: {:?}",
        t.elapsed()
    );
    assert!(stderr.contains("giving up"), "stderr: {stderr}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Test c: over several beats the requested extraMs keeps the deadline near now+L instead of
/// growing (the server adds extraMs to the current deadline).
#[test]
fn extend_does_not_creep() {
    let dir = scratch("c");
    let go = dir.join("go");
    let m = Mock::start("t1", vec![claim_task("cccc1111-0000")]);
    write_control(&dir.join("control.json"), m.port, "t1");
    let script = format!("while [ ! -f {} ]; do sleep 0.1; done", go.display());
    let child = spawn_worker(&dir, &[], &script);

    wait_for("4 extends", Duration::from_secs(30), || {
        m.recs()
            .iter()
            .filter(|(p, _, _)| p.ends_with("/extend"))
            .count()
            >= 4
    });
    std::fs::write(&go, "").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());

    // The mock reports deadline = now+3000 per extend. Lease L=3000, beats every ~1500ms, so a
    // non-creeping request is ≈1500 (never the old full-lease 3000).
    for (p, _, b) in m.recs().iter().filter(|(p, _, _)| p.ends_with("/extend")) {
        let v: serde_json::Value = serde_json::from_str(b).unwrap();
        let extra = v["extraMs"].as_i64().unwrap();
        assert!(
            (0..=2200).contains(&extra),
            "extra {extra} should be ≈L/2, not a full lease ({p})"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
