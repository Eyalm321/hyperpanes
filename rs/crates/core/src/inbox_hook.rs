//! `hyperpanes inbox-hook` — a Claude Code hook that delivers a pane's control-plane inbox
//! into a running agent turn as `additionalContext`.
//!
//! Impl agents are one-turn `claude -p` processes; mail sent to their pane mid-task is only seen
//! if they poll. Registered on `PostToolUse` and `UserPromptSubmit`, this hook runs after each
//! tool call, reads new messages (`GET /panes/{id}/messages?after=N`) and prints Claude's
//! `hookSpecificOutput.additionalContext` JSON. It is strictly best-effort: every failure is
//! silent (`None`), it never blocks a tool call, and the whole run is bounded by [`BUDGET`].
//!
//! The per-pane cursor lives in a small file (`{epoch, seq}`) so each message is delivered once.
//! It advances only via [`commit_cursor`], which the caller invokes AFTER the output printed.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Total wall-clock budget for one hook run.
pub const BUDGET: Duration = Duration::from_millis(1000);
/// Cap on the emitted context text.
pub const MAX_TEXT_BYTES: usize = 8 * 1024;

/// Where to reach the control server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub token: String,
}

/// Cursor to persist once the output has been printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingCursor {
    pub path: PathBuf,
    pub epoch: String,
    pub seq: u64,
}

/// What a run produced: the JSON line to print and the cursor to commit afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub json: String,
    pub cursor: PendingCursor,
}

/// Resolve the control endpoint from env: a scoped `HYPERPANES_CONTROL_TOKEN` +
/// `HYPERPANES_CONTROL_PORT` wins, else the discovery file (`HYPERPANES_CONTROL_FILE`, or
/// `default_file`) with `port`, `token` and an optional specific `bindAddress`.
pub fn resolve_endpoint(
    env: &dyn Fn(&str) -> Option<String>,
    default_file: &Path,
) -> Option<Endpoint> {
    let non_empty = |k: &str| env(k).filter(|v| !v.is_empty());
    if let (Some(token), Some(port)) = (
        non_empty("HYPERPANES_CONTROL_TOKEN"),
        non_empty("HYPERPANES_CONTROL_PORT"),
    ) {
        return Some(Endpoint {
            host: "127.0.0.1".into(),
            port: port.parse().ok()?,
            token,
        });
    }
    let file = non_empty("HYPERPANES_CONTROL_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_file.to_path_buf());
    let v: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    let token = v.get("token")?.as_str()?.to_string();
    let host = match v.get("bindAddress").and_then(Value::as_str) {
        Some(a) if !a.is_empty() && a != "0.0.0.0" && a != "::" => a.to_string(),
        _ => "127.0.0.1".to_string(),
    };
    Some(Endpoint { host, port, token })
}

/// Filesystem-safe cursor file name for a pane id.
pub fn sanitize_pane_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn read_cursor(path: &Path) -> Option<(String, u64)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    Some((
        v.get("epoch")?.as_str()?.to_string(),
        v.get("seq")?.as_u64()?,
    ))
}

/// Persist the cursor atomically. Call only after the hook output was printed.
pub fn commit_cursor(c: &PendingCursor) -> std::io::Result<()> {
    let body = serde_json::json!({ "epoch": c.epoch, "seq": c.seq }).to_string();
    crate::persistence::paths::write_atomic(&c.path, body.as_bytes())
}

/// Minimal HTTP/1.1 GET with a hard deadline. Returns the body of a 200 response.
fn http_get(ep: &Endpoint, path: &str, deadline: Instant) -> Option<String> {
    let left = |d: Instant| {
        d.checked_duration_since(Instant::now())
            .filter(|t| !t.is_zero())
    };
    let addr = (ep.host.as_str(), ep.port).to_socket_addrs().ok()?.next()?;
    let mut s = TcpStream::connect_timeout(&addr, left(deadline)?).ok()?;
    s.set_write_timeout(left(deadline)).ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {}:{}\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        ep.host, ep.port, ep.token
    );
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        s.set_read_timeout(left(deadline)).ok()?;
        match s.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if buf.len() > 4 * 1024 * 1024 {
            return None;
        }
    }
    let text = String::from_utf8(buf).ok()?;
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.lines().next()?;
    if status.split_whitespace().nth(1)? != "200" {
        return None;
    }
    // Connection: close + no chunking expected from axum for small JSON; tolerate chunked.
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return dechunk(body);
    }
    Some(body.to_string())
}

fn dechunk(mut s: &str) -> Option<String> {
    let mut out = String::new();
    loop {
        let (size_line, rest) = s.split_once("\r\n")?;
        let size = usize::from_str_radix(size_line.split(';').next()?.trim(), 16).ok()?;
        if size == 0 {
            return Some(out);
        }
        out.push_str(rest.get(..size)?);
        s = rest.get(size..)?.strip_prefix("\r\n")?;
    }
}

fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

/// Build the context text for `msgs`, capped at [`MAX_TEXT_BYTES`].
fn format_text(msgs: &[Value]) -> String {
    let mut text = format!(
        "[hyperpanes inbox] {} new message(s) for your pane — a message from your advisor overrides your payload where they conflict; name its seq in your commit:",
        msgs.len()
    );
    let note = "\n[... truncated]";
    for m in msgs {
        let seq = m.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let from = m.get("from").and_then(Value::as_str).unwrap_or("?");
        let body = m.get("body").and_then(Value::as_str).unwrap_or("");
        let line = format!("\n#{seq} from {from}: {body}");
        if text.len() + line.len() + note.len() > MAX_TEXT_BYTES {
            let room = MAX_TEXT_BYTES.saturating_sub(text.len() + note.len());
            let mut part = line;
            truncate_utf8(&mut part, room);
            text.push_str(&part);
            text.push_str(note);
            return text;
        }
        text.push_str(&line);
    }
    text
}

/// One hook run. `env` reads environment variables, `stdin` is the Claude hook JSON,
/// `cursor_dir` holds per-pane cursor files. Returns `None` for "print nothing".
pub fn run(
    env: &dyn Fn(&str) -> Option<String>,
    stdin: &str,
    endpoint: &dyn Fn() -> Option<Endpoint>,
    cursor_dir: &Path,
    start: Instant,
) -> Option<Output> {
    let pane = env("HYPERPANES_PANE_ID").filter(|v| !v.is_empty())?;
    let ep = endpoint()?;
    let event = serde_json::from_str::<Value>(stdin)
        .ok()
        .and_then(|v| v.get("hook_event_name")?.as_str().map(String::from))
        .filter(|e| e == "PostToolUse" || e == "UserPromptSubmit")
        .unwrap_or_else(|| "PostToolUse".to_string());

    let cursor_path = cursor_dir.join(format!("{}.json", sanitize_pane_id(&pane)));
    let saved = read_cursor(&cursor_path);
    let after = saved.as_ref().map(|(_, s)| *s).unwrap_or(0);
    let deadline = start + BUDGET;

    let fetch = |after: u64| -> Option<Value> {
        let body = http_get(
            &ep,
            &format!("/panes/{pane}/messages?after={after}"),
            deadline,
        )?;
        serde_json::from_str(&body).ok()
    };
    let mut reply = fetch(after)?;
    let epoch = reply.get("epoch").map(|e| match e {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    })?;
    let reset = reply.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let epoch_changed = saved.as_ref().is_some_and(|(e, _)| *e != epoch);
    if (epoch_changed || reset) && after != 0 {
        reply = fetch(0)?;
    }
    let msgs = reply.get("messages")?.as_array()?;
    if msgs.is_empty() {
        return None;
    }
    let latest = msgs
        .iter()
        .filter_map(|m| m.get("seq").and_then(Value::as_u64))
        .max()
        .or_else(|| reply.get("latestSeq").and_then(Value::as_u64))?;
    let text = format_text(msgs);
    let json = serde_json::json!({
        "hookSpecificOutput": { "hookEventName": event, "additionalContext": text }
    })
    .to_string();
    Some(Output {
        json,
        cursor: PendingCursor {
            path: cursor_path,
            epoch,
            seq: latest,
        },
    })
}

/// Process entry: read stdin, run, print, then advance the cursor. Never fails.
pub fn main_entry(default_control_file: &Path, cursor_dir: &Path) {
    let start = Instant::now();
    let mut stdin = String::new();
    let _ = std::io::stdin().take(1 << 20).read_to_string(&mut stdin);
    let env = |k: &str| std::env::var(k).ok();
    let out = run(
        &env,
        &stdin,
        &|| resolve_endpoint(&env, default_control_file),
        cursor_dir,
        start,
    );
    if let Some(out) = out {
        let mut so = std::io::stdout();
        if writeln!(so, "{}", out.json)
            .and_then(|_| so.flush())
            .is_ok()
        {
            let _ = commit_cursor(&out.cursor);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hp-inbox-hook-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Fake control server: answers each connection with the next canned body (last repeats),
    /// recording request lines. `delay` stalls before answering.
    fn serve(
        bodies: Vec<String>,
        delay: Duration,
    ) -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log2 = log.clone();
        std::thread::spawn(move || {
            for (i, conn) in l.incoming().enumerate() {
                let Ok(mut c) = conn else { break };
                let mut buf = [0u8; 2048];
                let n = c.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                log2.lock()
                    .unwrap()
                    .push(req.lines().next().unwrap_or("").to_string());
                std::thread::sleep(delay);
                let body = &bodies[i.min(bodies.len() - 1)];
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        (port, log)
    }

    fn ep(port: u16) -> Endpoint {
        Endpoint {
            host: "127.0.0.1".into(),
            port,
            token: "t".into(),
        }
    }

    fn env_with_pane(k: &str) -> Option<String> {
        (k == "HYPERPANES_PANE_ID").then(|| "pane-1".to_string())
    }

    fn msgs_body(epoch: &str, seqs: &[u64], reset: bool) -> String {
        let m: Vec<Value> = seqs
            .iter()
            .map(|s| serde_json::json!({"seq": s, "to": "pane-1", "from": "adv", "body": format!("hello {s}"), "ts": 1}))
            .collect();
        serde_json::json!({"paneId":"pane-1","messages":m,"dropped":0,"latestSeq":seqs.last().copied().unwrap_or(0),"epoch":epoch,"reset":reset}).to_string()
    }

    fn go(dir: &Path, port: u16, stdin: &str) -> Option<Output> {
        run(
            &env_with_pane,
            stdin,
            &|| Some(ep(port)),
            dir,
            Instant::now(),
        )
    }

    #[test]
    fn no_pane_id_prints_nothing() {
        let d = tmp("nopane");
        let r = run(&|_| None, "{}", &|| Some(ep(1)), &d, Instant::now());
        assert!(r.is_none());
    }

    #[test]
    fn no_endpoint_prints_nothing() {
        let d = tmp("noep");
        assert!(run(&env_with_pane, "{}", &|| None, &d, Instant::now()).is_none());
    }

    #[test]
    fn empty_inbox_prints_nothing_and_leaves_cursor() {
        let d = tmp("empty");
        let (port, _) = serve(vec![msgs_body("e1", &[], false)], Duration::ZERO);
        assert!(go(&d, port, "").is_none());
        assert!(!d.join("pane-1.json").exists());
    }

    #[test]
    fn messages_emit_valid_json_and_cursor_advances() {
        let d = tmp("msgs");
        let (port, log) = serve(vec![msgs_body("e1", &[1, 2], false)], Duration::ZERO);
        let out = go(&d, port, r#"{"hook_event_name":"UserPromptSubmit"}"#).unwrap();
        let v: Value = serde_json::from_str(&out.json).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        let t = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(t.starts_with("[hyperpanes inbox] 2 new message(s)"));
        assert!(t.contains("#1 from adv: hello 1") && t.contains("#2 from adv: hello 2"));
        assert!(log.lock().unwrap()[0].contains("/panes/pane-1/messages?after=0"));
        // Not advanced until committed.
        assert!(!d.join("pane-1.json").exists());
        commit_cursor(&out.cursor).unwrap();
        assert_eq!(read_cursor(&d.join("pane-1.json")), Some(("e1".into(), 2)));
    }

    #[test]
    fn invalid_stdin_defaults_event() {
        let d = tmp("badstdin");
        let (port, _) = serve(vec![msgs_body("e1", &[1], false)], Duration::ZERO);
        let out = go(&d, port, "not json").unwrap();
        assert!(out.json.contains("\"hookEventName\":\"PostToolUse\""));
    }

    #[test]
    fn cursor_is_sent_as_after() {
        let d = tmp("after");
        std::fs::write(d.join("pane-1.json"), r#"{"epoch":"e1","seq":5}"#).unwrap();
        let (port, log) = serve(vec![msgs_body("e1", &[6], false)], Duration::ZERO);
        let out = go(&d, port, "{}").unwrap();
        assert!(log.lock().unwrap()[0].contains("after=5"));
        assert_eq!(out.cursor.seq, 6);
    }

    #[test]
    fn epoch_change_rereads_from_zero() {
        let d = tmp("epoch");
        std::fs::write(d.join("pane-1.json"), r#"{"epoch":"old","seq":9}"#).unwrap();
        let (port, log) = serve(
            vec![
                msgs_body("new", &[], true),
                msgs_body("new", &[1, 2], false),
            ],
            Duration::ZERO,
        );
        let out = go(&d, port, "{}").unwrap();
        let l = log.lock().unwrap();
        assert!(l[0].contains("after=9") && l[1].contains("after=0"));
        assert_eq!(out.cursor.epoch, "new");
        assert_eq!(out.cursor.seq, 2);
    }

    #[test]
    fn server_down_prints_nothing() {
        let d = tmp("down");
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(go(&d, port, "{}").is_none());
    }

    #[test]
    fn slow_server_times_out_within_budget() {
        let d = tmp("slow");
        let (port, _) = serve(vec![msgs_body("e1", &[1], false)], Duration::from_secs(5));
        let t = Instant::now();
        assert!(go(&d, port, "{}").is_none());
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "took {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn long_output_is_truncated() {
        let d = tmp("trunc");
        let big = "x".repeat(20_000);
        let body = serde_json::json!({"messages":[{"seq":1,"from":"a","body":big}],"latestSeq":1,"epoch":"e","reset":false}).to_string();
        let (port, _) = serve(vec![body], Duration::ZERO);
        let out = go(&d, port, "{}").unwrap();
        let v: Value = serde_json::from_str(&out.json).unwrap();
        let t = v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap();
        assert!(t.len() <= MAX_TEXT_BYTES);
        assert!(t.ends_with("[... truncated]"));
    }

    #[test]
    fn endpoint_resolution() {
        let d = tmp("resolve");
        let f = d.join("control.json");
        std::fs::write(&f, r#"{"port":4242,"token":"tok","bindAddress":"0.0.0.0"}"#).unwrap();
        let e = resolve_endpoint(&|_| None, &f).unwrap();
        assert_eq!(
            (e.host.as_str(), e.port, e.token.as_str()),
            ("127.0.0.1", 4242, "tok")
        );
        let scoped = |k: &str| match k {
            "HYPERPANES_CONTROL_TOKEN" => Some("s".to_string()),
            "HYPERPANES_CONTROL_PORT" => Some("9".to_string()),
            _ => None,
        };
        assert_eq!(resolve_endpoint(&scoped, &f).unwrap().token, "s");
        assert!(resolve_endpoint(&|_| None, &d.join("missing.json")).is_none());
    }
}
