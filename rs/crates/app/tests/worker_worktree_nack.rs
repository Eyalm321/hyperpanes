//! Binary-level tests for worker worktree-isolation failure handling (g8): when
//! `--worktree` is requested and `git worktree add` fails, the task must be NACKed
//! with the error recorded, and the child must NOT run — specifically not in the
//! runner's shared cwd (the silent-degradation defect that put an unisolated
//! `claude -p` in the shared checkout on 2026-07-30).
//!
//! Drives the real `hyperpanes worker` binary against a minimal mock of the control
//! API's queue endpoints, with a scratch git repo as the runner cwd. The failure is
//! forced the same way it happens in production: a stale `worker/<queue>/<id8>`
//! branch AHEAD of the base, which `Worktree::create_in` refuses to clobber.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Recorded control-API traffic: (request path, request body), in arrival order.
type Records = Arc<Mutex<Vec<(String, String)>>>;

/// Minimal mock queue server. Successive claim calls pop from `claims`; once empty,
/// claims return `{"tasks":[]}` so the worker drains and exits. Nacks answer
/// `{"state":"queued"}` (a retryable outcome). Every request is recorded.
fn start_mock_queue(claims: Vec<String>) -> (u16, Records) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock queue");
    let port = listener.local_addr().unwrap().port();
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let claims = Arc::new(Mutex::new(std::collections::VecDeque::from(claims)));
    let rec = Arc::clone(&records);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut s) = conn else { continue };
            let Some((path, body)) = read_request(&mut s) else {
                continue;
            };
            rec.lock().unwrap().push((path.clone(), body));
            let resp = if path.contains("/claim") {
                claims
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| r#"{"tasks":[]}"#.to_string())
            } else if path.ends_with("/nack") {
                r#"{"state":"queued"}"#.to_string()
            } else {
                r#"{"ok":true}"#.to_string()
            };
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp}",
                resp.len()
            );
        }
    });
    (port, records)
}

/// Parse one HTTP request off the socket: returns (path, body).
fn read_request(s: &mut std::net::TcpStream) -> Option<(String, String)> {
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
    let len: usize = head
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(str::to_string)
        })
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let body_start = header_end + 4;
    while buf.len() < body_start + len {
        match s.read(&mut tmp) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
        }
    }
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let body = String::from_utf8_lossy(&buf[body_start..body_start + len]).to_string();
    Some((path, body))
}

/// A claim response carrying one task with the given id.
fn claim_with(id: &str) -> String {
    format!(
        r#"{{"tasks":[{{"id":"{id}","title":"t","payload":"","fencingToken":7,"attempts":1,"maxAttempts":3}}]}}"#
    )
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Scratch dir holding a git repo (the runner cwd) and a control.json pointing the
/// worker at the mock queue.
fn scratch(tag: &str, port: u16) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("hp-g8-nack-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["commit", "--allow-empty", "-m", "A"]);
    let control = dir.join("control.json");
    std::fs::write(&control, format!(r#"{{"port":{port},"token":"t"}}"#)).unwrap();
    (repo, control)
}

/// Plant a stale `worker/<queue>/<id8>` branch AHEAD of main, so `git worktree add`
/// for a task with this id8 is refused (uncollected-work guard).
fn plant_stale_branch(repo: &Path, queue: &str, id8: &str) {
    git(repo, &["checkout", "-b", &format!("worker/{queue}/{id8}")]);
    git(repo, &["commit", "--allow-empty", "-m", "uncollected"]);
    git(repo, &["checkout", "main"]);
}

fn run_worker(repo: &Path, control: &Path, extra: &[&str], child: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_hyperpanes"))
        .current_dir(repo)
        .env("HYPERPANES_CONTROL_FILE", control)
        .args([
            "worker",
            "--queue",
            "q",
            "--worker",
            "w",
            "--worktree",
            "--base",
            "main",
        ])
        .args(extra)
        .arg("--")
        .args(child)
        .output()
        .expect("worker binary runs")
}

/// Acceptance A: a failed worktree create NACKs the task (error recorded) and the
/// child is never executed — in particular, nothing runs in the runner's cwd.
#[test]
fn worktree_create_failure_nacks_and_never_runs_child_in_runner_cwd() {
    let (port, records) = start_mock_queue(vec![claim_with("stalea00-0000-0000")]);
    let (repo, control) = scratch("a", port);
    plant_stale_branch(&repo, "q", "stalea00");

    let out = run_worker(&repo, &control, &[], &["sh", "-c", "touch executed-marker"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "worker exits clean after drain: {stderr}"
    );

    // The child must not have run — not anywhere, and specifically not in the
    // runner's cwd (the shared checkout in production).
    assert!(
        !repo.join("executed-marker").exists(),
        "child ran in the runner's cwd despite worktree failure\nstderr: {stderr}"
    );

    let recs = records.lock().unwrap().clone();
    let nack = recs
        .iter()
        .find(|(p, _)| p.ends_with("/nack"))
        .unwrap_or_else(|| panic!("no nack recorded; traffic: {recs:?}\nstderr: {stderr}"));
    assert!(
        nack.0.contains("stalea00"),
        "nack targets the failed task: {nack:?}"
    );
    // The queue-visible reason carries the git error, naming the branch.
    assert!(
        nack.1.contains("already exists") && nack.1.contains("worker/q/stalea00"),
        "nack reason records the worktree error: {}",
        nack.1
    );
    assert!(
        !recs.iter().any(|(p, _)| p.ends_with("/ack")),
        "task must not be acked: {recs:?}"
    );
    // The failure is loud in the runner output, naming the branch it tried.
    assert!(
        stderr.contains("worker/q/stalea00"),
        "stderr names the branch: {stderr}"
    );

    let _ = std::fs::remove_dir_all(repo.parent().unwrap());
}

/// Acceptance B: the nack is RETRYABLE — it goes through the standard nack path
/// (honoring --nack-delay) and the worker keeps draining: a later task whose
/// worktree creates fine still runs (isolated) and acks.
#[test]
fn worktree_failure_nack_is_retryable_and_worker_continues() {
    let (port, records) = start_mock_queue(vec![
        claim_with("staleb00-0000-0000"),
        claim_with("goodb000-0000-0000"),
    ]);
    let (repo, control) = scratch("b", port);
    plant_stale_branch(&repo, "q", "staleb00");

    let out = run_worker(
        &repo,
        &control,
        &["--nack-delay", "250"],
        &["sh", "-c", "touch ran-here"],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "worker exits clean after drain: {stderr}"
    );

    let recs = records.lock().unwrap().clone();
    let nack_pos = recs
        .iter()
        .position(|(p, _)| p.ends_with("/nack") && p.contains("staleb00"))
        .unwrap_or_else(|| panic!("failed task not nacked; traffic: {recs:?}\nstderr: {stderr}"));
    // Standard retry semantics: the --nack-delay backoff rides the nack, and the
    // queue's attempts/max_attempts bound the retries (state comes from the server).
    assert!(
        recs[nack_pos].1.contains("\"delayMs\":250"),
        "nack honors --nack-delay: {}",
        recs[nack_pos].1
    );
    // The worker moved on: the healthy task was claimed after the nack, ran, acked.
    let ack_pos = recs
        .iter()
        .position(|(p, _)| p.ends_with("/ack") && p.contains("goodb000"))
        .unwrap_or_else(|| panic!("healthy task not acked; traffic: {recs:?}\nstderr: {stderr}"));
    assert!(nack_pos < ack_pos, "nack precedes the next task's ack");
    // The healthy task's child ran in ITS worktree (removed afterwards), so its
    // marker must not appear in the runner's cwd either.
    assert!(
        !repo.join("ran-here").exists(),
        "healthy task leaked into the runner's cwd\nstderr: {stderr}"
    );

    let _ = std::fs::remove_dir_all(repo.parent().unwrap());
}
