//! Binary-level tests for the disk-backed worktree store (`--worktree-root`,
//! `HYPERPANES_WORKTREE_ROOT`, orphan sweep, shared cache slots, disk guard).
//!
//! Drives the real `hyperpanes worker` binary against a minimal mock of the control API's queue
//! endpoints with a scratch git repo as the runner cwd. EVERY invocation pins the worktree root to
//! a scratch dir (never `~/.cache`) and sets `HYPERPANES_WORKTREE_MIN_FREE=0` unless it is testing
//! the guard (CI runners may have <10G free).

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

struct Env {
    dir: PathBuf,
    repo: PathBuf,
    control: PathBuf,
    /// Default worktree root for this scratch (canonical).
    root: PathBuf,
    /// Child-side scratch for markers.
    rec: PathBuf,
}

/// Scratch dir with a git repo (one commit holding a tiny cargo crate), a control.json pointing
/// at the mock queue, a worktree root and a marker dir.
fn scratch(tag: &str, port: u16) -> Env {
    let dir = std::env::temp_dir().join(format!("hp-wtroot-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(&repo, &["init", "-b", "main"]);
    git(&repo, &["add", "Cargo.toml", "src/main.rs"]);
    git(&repo, &["commit", "-m", "A"]);
    let control = dir.join("control.json");
    std::fs::write(&control, format!(r#"{{"port":{port},"token":"t"}}"#)).unwrap();
    let root = dir.join("root");
    std::fs::create_dir_all(&root).unwrap();
    let rec = dir.join("rec");
    std::fs::create_dir_all(&rec).unwrap();
    Env {
        dir: dir.clone(),
        repo,
        control,
        root: root.canonicalize().unwrap(),
        rec: rec.canonicalize().unwrap(),
    }
}

fn worker_cmd(e: &Env, root_env: &Path, min_free: &str, extra: &[&str], child: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_hyperpanes"));
    c.current_dir(&e.repo)
        .env("HYPERPANES_CONTROL_FILE", &e.control)
        .env("HYPERPANES_WORKTREE_ROOT", root_env)
        .env("HYPERPANES_WORKTREE_MIN_FREE", min_free)
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
        .args(child);
    c
}

fn run_worker(e: &Env, extra: &[&str], child: &[&str]) -> std::process::Output {
    worker_cmd(e, &e.root, "0", extra, child)
        .output()
        .expect("worker binary runs")
}

/// The single `<root>/<repo-key>` dir the worker created.
fn key_dir(root: &Path) -> PathBuf {
    let dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap_or_else(|e| panic!("read {}: {e}", root.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    assert_eq!(
        dirs.len(),
        1,
        "expected one repo-key dir in {root:?}: {dirs:?}"
    );
    dirs.into_iter().next().unwrap()
}

fn entries(dir: &Path) -> Vec<String> {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect(),
        Err(_) => vec![],
    }
}

fn ids(records: &Records, suffix: &str) -> Vec<String> {
    records
        .lock()
        .unwrap()
        .iter()
        .filter(|(p, _)| p.ends_with(suffix))
        .map(|(p, _)| p.clone())
        .collect()
}

fn cleanup(e: &Env) {
    let _ = std::fs::remove_dir_all(&e.dir);
}

fn marker_script(marker: &Path) -> String {
    format!(
        "pwd -P > '{m}'; echo \"$HP_WORKTREE_CACHE_DIR\" >> '{m}'",
        m = marker.display()
    )
}

fn read_marker(marker: &Path) -> (PathBuf, PathBuf) {
    let s = std::fs::read_to_string(marker).expect("child wrote its marker");
    let mut l = s.lines();
    (
        PathBuf::from(l.next().unwrap()),
        PathBuf::from(l.next().unwrap()),
    )
}

/// 1. The env root is honoured: worktree under `<root>/<key>/wt/`, cache slot-0, acked, and the
///    worktree dir + lock are gone afterwards.
#[test]
fn env_root_holds_worktree_and_cache_slot() {
    let (port, records) = start_mock_queue(vec![claim_with("envroot0-0000-0000")]);
    let e = scratch("env", port);
    let marker = e.rec.join("m");
    let out = run_worker(&e, &[], &["sh", "-c", &marker_script(&marker)]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let kd = key_dir(&e.root);
    let (pwd, cache) = read_marker(&marker);
    assert!(
        pwd.starts_with(kd.join("wt")),
        "pwd {pwd:?} not under {kd:?}/wt"
    );
    assert_eq!(cache, kd.join("cache/slot-0"), "{stderr}");
    assert!(!ids(&records, "/ack").is_empty(), "task acked: {stderr}");
    assert!(ids(&records, "/nack").is_empty());
    assert_eq!(entries(&kd.join("wt")), Vec::<String>::new(), "wt/ cleaned");
    assert!(kd.join("repo").is_file());
    assert!(stderr.contains("worker: worktrees under"), "{stderr}");
    cleanup(&e);
}

/// 2. `--worktree-root` beats the env.
#[test]
fn flag_root_beats_env_root() {
    let (port, _records) = start_mock_queue(vec![claim_with("flagroot-0000-0000")]);
    let e = scratch("flag", port);
    let flag_root = e.dir.join("flagroot");
    let marker = e.rec.join("m");
    let out = run_worker(
        &e,
        &["--worktree-root", flag_root.to_str().unwrap()],
        &["sh", "-c", &marker_script(&marker)],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let (pwd, _) = read_marker(&marker);
    let flag_root = flag_root.canonicalize().unwrap();
    assert!(
        pwd.starts_with(&flag_root),
        "{pwd:?} not under {flag_root:?}"
    );
    assert_eq!(entries(&e.root), Vec::<String>::new(), "env root untouched");
    cleanup(&e);
}

/// 3. A crashed runner's worktree is swept by the next runner start; a worktree whose lock is
///    held (live owner) survives; the branch survives.
#[test]
fn crashed_runner_orphan_is_swept_live_lock_is_kept() {
    let (port, _records) = start_mock_queue(vec![claim_with("crash000-0000-0000")]);
    let e = scratch("crash", port);
    // The child SIGKILLs the runner, lingers ~1s (holding the inherited lock fd), exits 0.
    // `output()` returns only when the child's inherited pipes close, i.e. once it is gone.
    let out = run_worker(&e, &[], &["sh", "-c", "kill -9 $PPID; sleep 1; exit 0"]);
    assert!(!out.status.success(), "runner was killed");
    let kd = key_dir(&e.root);
    let wt = kd.join("wt");
    let orphan = entries(&wt)
        .into_iter()
        .find(|n| !n.ends_with(".lock"))
        .unwrap_or_else(|| panic!("orphan dir left behind: {:?}", entries(&wt)));
    let orphan_path = wt.join(&orphan);
    assert!(orphan_path.is_dir());

    // A dir whose lock the TEST holds must survive the next runner start.
    let held = wt.join("held-by-test");
    std::fs::create_dir_all(&held).unwrap();
    let lock = std::fs::File::create(wt.join("held-by-test.lock")).unwrap();
    lock.try_lock().expect("test takes the lock");

    let (port2, _r2) = start_mock_queue(vec![]);
    std::fs::write(&e.control, format!(r#"{{"port":{port2},"token":"t"}}"#)).unwrap();
    let out2 = run_worker(&e, &[], &["true"]);
    let stderr = String::from_utf8_lossy(&out2.stderr);
    assert!(out2.status.success(), "{stderr}");
    assert!(!orphan_path.exists(), "orphan swept: {stderr}");
    assert!(
        stderr.contains("swept orphan worktree"),
        "sweep is logged: {stderr}"
    );
    assert!(held.is_dir(), "live-locked dir kept");
    let list = git(&e.repo, &["worktree", "list"]);
    assert!(!list.contains(&orphan), "worktree list: {list}");
    git(&e.repo, &["rev-parse", "--verify", "worker/q/crash000"]);
    drop(lock);
    cleanup(&e);
}

/// 4. Disk guard: nack with the explanation and a 60s delay, no child, nothing created, worker
///    stops (does not claim the second task).
#[test]
fn disk_guard_nacks_and_stops_without_creating_anything() {
    let (port, records) = start_mock_queue(vec![
        claim_with("guard000-0000-0000"),
        claim_with("guard111-0000-0000"),
    ]);
    let e = scratch("guard", port);
    let marker = e.rec.join("ran");
    let out = worker_cmd(
        &e,
        &e.root,
        "1000000T",
        &[],
        &["sh", "-c", &format!("touch '{}'", marker.display())],
    )
    .output()
    .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "worker exits: {stderr}");
    let recs = records.lock().unwrap().clone();
    let nacks: Vec<_> = recs.iter().filter(|(p, _)| p.ends_with("/nack")).collect();
    assert_eq!(nacks.len(), 1, "{recs:?}");
    assert!(
        nacks[0].1.contains("insufficient disk space"),
        "{}",
        nacks[0].1
    );
    assert!(nacks[0].1.contains("\"delayMs\":60000"), "{}", nacks[0].1);
    assert!(!recs.iter().any(|(p, _)| p.ends_with("/ack")));
    assert_eq!(
        recs.iter().filter(|(p, _)| p.contains("/claim")).count(),
        1,
        "worker stopped instead of churning the queue: {recs:?}"
    );
    assert!(!marker.exists(), "child must not run");
    assert_eq!(entries(&key_dir(&e.root).join("wt")), Vec::<String>::new());
    cleanup(&e);
}

fn concurrency_child(rec: &Path) -> String {
    let r = rec.display();
    format!(
        r#"c="$HP_WORKTREE_CACHE_DIR"; echo "$c" > '{r}'/$HP_TASK_ID.cache; echo "$HP_TASK_ID" > "$c/owner"; touch '{r}'/start-$HP_TASK_ID; sleep 1.5; [ "$(cat "$c/owner")" = "$HP_TASK_ID" ] || exit 1; [ "$(ls '{r}' | grep -c '^start-')" -ge 2 ] && exit 0; touch '{r}'/no-overlap-$HP_TASK_ID; exit 0"#
    )
}

fn cache_of(rec: &Path, id: &str) -> PathBuf {
    PathBuf::from(
        std::fs::read_to_string(rec.join(format!("{id}.cache")))
            .unwrap_or_else(|e| panic!("{id}: {e}"))
            .trim(),
    )
}

/// 5a. Two concurrent workers get DISTINCT cache slots whose lifetimes overlap.
#[test]
fn concurrent_workers_get_distinct_cache_slots() {
    let (port, records) = start_mock_queue(vec![
        claim_with("conc0000-0000-0000"),
        claim_with("conc1111-0000-0000"),
    ]);
    let e = scratch("conc", port);
    let out = run_worker(
        &e,
        &["--count", "2"],
        &["sh", "-c", &concurrency_child(&e.rec)],
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(ids(&records, "/ack").len(), 2, "both acked: {stderr}");
    assert!(ids(&records, "/nack").is_empty(), "{stderr}");
    let (a, b) = (
        cache_of(&e.rec, "conc0000-0000-0000"),
        cache_of(&e.rec, "conc1111-0000-0000"),
    );
    assert_ne!(a, b, "distinct slots");
    assert!(
        entries(&e.rec).iter().all(|n| !n.starts_with("no-overlap")),
        "intervals overlapped: {:?}",
        entries(&e.rec)
    );
    cleanup(&e);
}

/// 5b. Sequential tasks on one worker reuse slot-0 (warm cache).
#[test]
fn sequential_tasks_reuse_slot_zero() {
    let (port, records) = start_mock_queue(vec![
        claim_with("seq00000-0000-0000"),
        claim_with("seq11111-0000-0000"),
    ]);
    let e = scratch("seq", port);
    let out = run_worker(
        &e,
        &["--count", "1"],
        &[
            "sh",
            "-c",
            &format!(
                r#"echo "$HP_WORKTREE_CACHE_DIR" > '{}'/$HP_TASK_ID.cache"#,
                e.rec.display()
            ),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(ids(&records, "/ack").len(), 2);
    let kd = key_dir(&e.root);
    for id in ["seq00000-0000-0000", "seq11111-0000-0000"] {
        assert_eq!(cache_of(&e.rec, id), kd.join("cache/slot-0"), "{id}");
    }
    cleanup(&e);
}

/// 6. Real cargo builds in two concurrent worktrees, each into its own cache slot.
#[test]
fn concurrent_cargo_builds_use_distinct_cache_slots() {
    let (port, records) = start_mock_queue(vec![
        claim_with("cargo000-0000-0000"),
        claim_with("cargo111-0000-0000"),
    ]);
    let e = scratch("cargo", port);
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let script = format!(
        r#"echo "$HP_WORKTREE_CACHE_DIR" > '{}'/$HP_TASK_ID.cache; CARGO_TARGET_DIR="$HP_WORKTREE_CACHE_DIR/cargo-target" '{cargo}' build --offline"#,
        e.rec.display()
    );
    let out = run_worker(&e, &["--count", "2"], &["sh", "-c", &script]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(
        ids(&records, "/ack").len(),
        2,
        "both builds acked: {stderr}"
    );
    assert!(ids(&records, "/nack").is_empty(), "{stderr}");
    let (a, b) = (
        cache_of(&e.rec, "cargo000-0000-0000"),
        cache_of(&e.rec, "cargo111-0000-0000"),
    );
    assert_ne!(a, b, "distinct slots");
    assert!(a.join("cargo-target").is_dir() && b.join("cargo-target").is_dir());
    cleanup(&e);
}
