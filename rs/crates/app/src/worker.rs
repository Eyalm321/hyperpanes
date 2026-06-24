//! `hyperpanes worker` — headless work-queue drain loop (worker runner MVP, issue #10).
//!
//! Usage:
//! ```text
//! hyperpanes worker --queue <name> [--worker <id>] [--count N] [--worktree] \
//!   [--retry-window <secs>] [--nack-delay <ms>] -- <cmd> [args...]
//! ```
//!
//! Discovers the running app's control API from `control.json` (or `HYPERPANES_CONTROL_FILE`),
//! then loops: **claim** one task → run `<cmd>` as a child with the task injected via env
//! (`HP_TASK_ID`, `HP_TASK_PAYLOAD`, `HP_FENCING_TOKEN`, `HP_QUEUE`, `HP_TASK_TITLE`) → **ack**
//! on child exit 0 / **nack** on non-zero → repeat until a claim comes back empty, then exit 0
//! (so a hyperpanes pane running the worker auto-closes on drain).
//!
//! Flags: `--count N` runs N competing workers in this process (#11); `--worktree` runs each
//! task in a throwaway git worktree that auto-removes (#14); `--retry-window <secs>` keeps
//! polling after the queue empties so backoff retries get reclaimed, and `--nack-delay <ms>`
//! overrides the retry backoff (#13). A lease heartbeat renews the lease while a task runs so a
//! long task isn't reclaimed mid-flight (#12). The child reads its task from the environment, so
//! shell expansion like `$HP_TASK_PAYLOAD` needs an explicit inner shell:
//! `-- sh -c 'claude -p "$HP_TASK_PAYLOAD"'`.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

/// Parsed `hyperpanes worker` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkerArgs {
    pub queue: String,
    pub worker: String,
    /// Number of concurrent competing workers to run in this process (#11).
    pub count: usize,
    /// Keep polling this many seconds after the queue empties, to reclaim backoff retries
    /// within one run (#13); 0 = exit on the first empty claim.
    pub retry_window_secs: u64,
    /// Override the nack backoff (ms) on failure; None = the queue's default (#13).
    pub nack_delay_ms: Option<i64>,
    /// Run each task in a throwaway git worktree, auto-removed on exit (#14).
    pub worktree: bool,
    /// Everything after `--`: program + args, executed directly (no shell).
    pub child: Vec<String>,
}

/// The bits of `control.json` the worker needs to reach the control API.
#[derive(Deserialize)]
struct Discovery {
    port: u16,
    token: String,
}

/// Only the task fields the worker uses (control API serializes camelCase).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Task {
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    payload: String,
    fencing_token: u64,
    /// ms-epoch lease deadline from the claim; drives the heartbeat interval (#12).
    #[serde(default)]
    visibility_deadline: Option<i64>,
    /// retry accounting from the queue, for logging the nack outcome (#13).
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    max_attempts: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimOut {
    tasks: Vec<Task>,
}

/// True if argv requests worker mode: `hyperpanes worker ...` (subcommand in argv[1]).
pub fn wants_worker(argv: &[String]) -> bool {
    argv.get(1).map(|a| a == "worker").unwrap_or(false)
}

/// Parse `worker --queue <q> [--worker <id>] -- <cmd...>`.
/// `argv[0]` is the program, `argv[1]` is `worker`; parsing starts at index 2.
pub fn parse_args(argv: &[String]) -> Result<WorkerArgs, String> {
    let mut queue: Option<String> = None;
    let mut worker: Option<String> = None;
    let mut count_arg: Option<String> = None;
    let mut retry_window_arg: Option<String> = None;
    let mut nack_delay_arg: Option<String> = None;
    let mut worktree = false;
    let mut child: Vec<String> = Vec::new();
    let mut i = 2;
    while i < argv.len() {
        let a = argv[i].as_str();
        match a {
            "--queue" | "-q" => {
                queue = Some(argv.get(i + 1).ok_or("--queue needs a value")?.clone());
                i += 2;
            }
            "--worker" | "-w" => {
                worker = Some(argv.get(i + 1).ok_or("--worker needs a value")?.clone());
                i += 2;
            }
            "--count" | "-n" => {
                count_arg = Some(argv.get(i + 1).ok_or("--count needs a value")?.clone());
                i += 2;
            }
            "--retry-window" => {
                retry_window_arg =
                    Some(argv.get(i + 1).ok_or("--retry-window needs a value")?.clone());
                i += 2;
            }
            "--nack-delay" => {
                nack_delay_arg = Some(argv.get(i + 1).ok_or("--nack-delay needs a value")?.clone());
                i += 2;
            }
            "--worktree" => {
                worktree = true;
                i += 1;
            }
            "--" => {
                child = argv[i + 1..].to_vec();
                break;
            }
            other => {
                if let Some(v) = other.strip_prefix("--queue=") {
                    queue = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--worker=") {
                    worker = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--count=") {
                    count_arg = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--retry-window=") {
                    retry_window_arg = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--nack-delay=") {
                    nack_delay_arg = Some(v.to_string());
                } else {
                    return Err(format!("unexpected argument: {other}"));
                }
                i += 1;
            }
        }
    }
    let queue = queue.ok_or("missing --queue <name>")?;
    if child.is_empty() {
        return Err("missing child command after `--`".to_string());
    }
    let worker = worker.unwrap_or_else(default_worker_name);
    let count = match count_arg {
        Some(c) => {
            let n: usize = c
                .parse()
                .map_err(|_| format!("--count must be a positive integer, got '{c}'"))?;
            if n == 0 {
                return Err("--count must be >= 1".to_string());
            }
            n
        }
        None => 1,
    };
    let retry_window_secs = match retry_window_arg {
        Some(s) => s
            .parse()
            .map_err(|_| format!("--retry-window must be a non-negative integer, got '{s}'"))?,
        None => 0,
    };
    let nack_delay_ms = match nack_delay_arg {
        Some(s) => Some(
            s.parse()
                .map_err(|_| format!("--nack-delay must be an integer (ms), got '{s}'"))?,
        ),
        None => None,
    };
    Ok(WorkerArgs {
        queue,
        worker,
        count,
        retry_window_secs,
        nack_delay_ms,
        worktree,
        child,
    })
}

/// pid-suffixed default so two bare `hyperpanes worker` invocations don't share an id.
fn default_worker_name() -> String {
    format!("worker-{}", std::process::id())
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Read `control.json` (env override `HYPERPANES_CONTROL_FILE`, else the state-dir default).
fn load_discovery() -> Result<Discovery, Box<dyn Error>> {
    let path = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(hyperpanes_core::persistence::paths::control_json);
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read control.json at {} ({e}); is the app running with the control API enabled?",
            path.display()
        )
    })?;
    Ok(serde_json::from_str(&raw)?)
}

/// Entry point from `main`. Drains `--queue` until empty, then returns `Ok(())`.
pub fn run(argv: &[String]) -> Result<(), Box<dyn Error>> {
    let args = match parse_args(argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hyperpanes worker: {e}");
            eprintln!("usage: hyperpanes worker --queue <name> [--worker <id>] -- <cmd> [args...]");
            return Err(e.into());
        }
    };

    let disco = load_discovery()?;
    let base = format!("http://127.0.0.1:{}", disco.port);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let retry_window = Duration::from_secs(args.retry_window_secs);
    let nack_delay = args.nack_delay_ms;
    let worktree = args.worktree;

    // One worker drains in this thread; `--count N` spawns N competing workers (#11), each
    // with its own id, and the process exits once they have all seen the queue empty.
    if args.count <= 1 {
        let done = drain(
            &client,
            &base,
            &disco.token,
            &args.queue,
            &args.worker,
            &args.child,
            retry_window,
            nack_delay,
            worktree,
        )?;
        eprintln!(
            "[{}] queue drained — {done} task(s) acked, exiting",
            args.worker
        );
        return Ok(());
    }

    eprintln!(
        "spawning {} workers on '{}' via {base}",
        args.count, args.queue
    );
    let mut handles = Vec::with_capacity(args.count);
    for i in 1..=args.count {
        let client = client.clone();
        let base = base.clone();
        let token = disco.token.clone();
        let queue = args.queue.clone();
        let child = args.child.clone();
        let worker = format!("{}-{i}", args.worker);
        handles.push(std::thread::spawn(move || {
            match drain(
                &client, &base, &token, &queue, &worker, &child, retry_window, nack_delay, worktree,
            ) {
                Ok(n) => {
                    eprintln!("[{worker}] drained {n} task(s)");
                    n
                }
                Err(e) => {
                    eprintln!("[{worker}] error: {e}");
                    0
                }
            }
        }));
    }
    let total: u64 = handles.into_iter().filter_map(|h| h.join().ok()).sum();
    eprintln!("all {} workers exited — {total} task(s) total", args.count);
    Ok(())
}

/// One worker's claim → run → ack/nack loop. Returns the number of tasks acked; stops when a
/// claim comes back empty. Shared by the single-worker and `--count` paths.
#[allow(clippy::too_many_arguments)]
fn drain(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    queue: &str,
    worker: &str,
    child: &[String],
    retry_window: Duration,
    nack_delay_ms: Option<i64>,
    worktree: bool,
) -> Result<u64, Box<dyn Error>> {
    eprintln!("[{worker}] online — draining '{queue}'");
    let mut done: u64 = 0;
    let mut empty_since: Option<Instant> = None;
    loop {
        let task = match claim_one(client, base, token, queue, worker)? {
            Some(t) => {
                empty_since = None;
                t
            }
            None => {
                // Queue empty. With a retry window, keep polling so backoff retries (#13) get
                // reclaimed within this run; otherwise exit on the first empty claim.
                if retry_window.is_zero() {
                    return Ok(done);
                }
                let since = *empty_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= retry_window {
                    return Ok(done);
                }
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }
        };
        eprintln!(
            "[{worker}] >> claimed {} (fence {}) :: {}",
            short(&task.id),
            task.fencing_token,
            task.title
        );

        // Optional per-task throwaway worktree (#14): create it, run the child there, remove it
        // after (the commit, if any, stays on its branch).
        let wt = if worktree {
            match Worktree::create(queue, &task.id) {
                Ok(w) => Some(w),
                Err(e) => {
                    eprintln!("[{worker}] worktree create failed: {e}");
                    None
                }
            }
        } else {
            None
        };
        let cwd = wt.as_ref().map(|w| w.path.as_path());
        let outcome = run_child(child, &task, queue, client, base, token, cwd);
        if let Some(w) = &wt {
            w.remove();
        }

        match outcome {
            Ok(true) => {
                ack(client, base, token, &task.id, task.fencing_token)?;
                done += 1;
                eprintln!("[{worker}] << acked  {}", short(&task.id));
            }
            other => {
                let reason = match other {
                    Ok(false) => "child exited non-zero".to_string(),
                    Err(e) => e.to_string(),
                    Ok(true) => unreachable!(),
                };
                let state = nack(
                    client,
                    base,
                    token,
                    &task.id,
                    task.fencing_token,
                    &reason,
                    nack_delay_ms,
                )?;
                eprintln!(
                    "[{worker}] !! nacked {} (attempt {}/{}) → {state} ({reason})",
                    short(&task.id),
                    task.attempts,
                    task.max_attempts
                );
            }
        }
    }
}

fn claim_one(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    queue: &str,
    worker: &str,
) -> Result<Option<Task>, Box<dyn Error>> {
    let resp = client
        .post(format!("{base}/queues/{queue}/claim"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "worker": worker, "count": 1 }))
        .send()?;
    if !resp.status().is_success() {
        return Err(format!("claim failed: HTTP {}", resp.status().as_u16()).into());
    }
    let out: ClaimOut = resp.json()?;
    Ok(out.tasks.into_iter().next())
}

/// Run the child command with the task in its environment, while a background heartbeat renews
/// the lease (#12) so a long-running task is not reclaimed mid-flight. Returns Ok(true) on exit 0.
fn run_child(
    child: &[String],
    task: &Task,
    queue: &str,
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    cwd: Option<&Path>,
) -> Result<bool, Box<dyn Error>> {
    // Heartbeat: while the child runs, `extend` the lease at ~half the remaining lease interval.
    let stop = Arc::new(AtomicBool::new(false));
    let heartbeat = task.visibility_deadline.map(|deadline| {
        let lease_ms = (deadline - now_ms()).max(2_000);
        let interval_ms = (lease_ms / 2).max(1_000) as u64;
        let extra_ms = lease_ms; // renew by a full lease each beat
        let stop = Arc::clone(&stop);
        let client = client.clone();
        let base = base.to_string();
        let token = token.to_string();
        let id = task.id.clone();
        let fence = task.fencing_token;
        std::thread::spawn(move || {
            // sleep, then extend, until the child finishes (stop flag set)
            while !sleep_interruptible(&stop, interval_ms) {
                if extend(&client, &base, &token, &id, fence, extra_ms).is_err() {
                    return; // lost lease / server gone — the ack will surface it
                }
            }
        })
    });

    let mut cmd = Command::new(&child[0]);
    cmd.args(&child[1..])
        .env("HP_TASK_ID", &task.id)
        .env("HP_TASK_PAYLOAD", &task.payload)
        .env("HP_TASK_TITLE", &task.title)
        .env("HP_FENCING_TOKEN", task.fencing_token.to_string())
        .env("HP_QUEUE", queue);
    if let Some(dir) = cwd {
        cmd.current_dir(dir).env("HP_WORKTREE", dir);
    }
    let result = cmd
        .status()
        .map_err(|e| format!("failed to spawn '{}': {e}", child[0]));

    // Stop the heartbeat before ack/nack so we never extend a finished task.
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = heartbeat {
        let _ = h.join();
    }
    Ok(result?.success())
}

/// Sleep up to `ms`, waking early and returning `true` if `stop` gets set; `false` on timeout.
fn sleep_interruptible(stop: &AtomicBool, ms: u64) -> bool {
    let step = 200u64;
    let mut waited = 0u64;
    while waited < ms {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        let nap = step.min(ms - waited);
        std::thread::sleep(Duration::from_millis(nap));
        waited += nap;
    }
    stop.load(Ordering::Relaxed)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn ack(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    id: &str,
    fence: u64,
) -> Result<(), Box<dyn Error>> {
    let resp = client
        .post(format!("{base}/tasks/{id}/ack"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "fencingToken": fence, "result": "ok" }))
        .send()?;
    if !resp.status().is_success() {
        return Err(format!("ack failed: HTTP {}", resp.status().as_u16()).into());
    }
    Ok(())
}

/// Nack a failed task. Returns the resulting queue state: `queued` (will retry) | `failed` |
/// `dead` (retries exhausted). `delay_ms` overrides the backoff when set (#13).
fn nack(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    id: &str,
    fence: u64,
    error: &str,
    delay_ms: Option<i64>,
) -> Result<String, Box<dyn Error>> {
    let mut body = serde_json::json!({ "fencingToken": fence, "error": error });
    if let Some(d) = delay_ms {
        body["delayMs"] = serde_json::json!(d);
    }
    let resp = client
        .post(format!("{base}/tasks/{id}/nack"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()?;
    if !resp.status().is_success() {
        return Err(format!("nack failed: HTTP {}", resp.status().as_u16()).into());
    }
    let out: serde_json::Value = resp.json().unwrap_or(serde_json::Value::Null);
    Ok(out
        .get("state")
        .and_then(|s| s.as_str())
        .unwrap_or("?")
        .to_string())
}

/// POST /tasks/{id}/extend — renew the lease (heartbeat, #12).
fn extend(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    id: &str,
    fence: u64,
    extra_ms: i64,
) -> Result<(), Box<dyn Error>> {
    let resp = client
        .post(format!("{base}/tasks/{id}/extend"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "fencingToken": fence, "extraMs": extra_ms }))
        .send()?;
    if !resp.status().is_success() {
        return Err(format!("extend failed: HTTP {}", resp.status().as_u16()).into());
    }
    Ok(())
}

/// A throwaway git worktree for one task (#14). Created off HEAD on a fresh branch; the working
/// dir is removed when the task finishes (a commit, if any, stays on the branch). Created from
/// the worker's cwd, so `--worktree` requires running inside a git repo.
struct Worktree {
    path: PathBuf,
}

impl Worktree {
    fn create(queue: &str, task_id: &str) -> Result<Self, Box<dyn Error>> {
        let id8 = &task_id[..8.min(task_id.len())];
        let safe = queue.replace(['/', ' '], "-");
        let branch = format!("worker/{safe}/{id8}");
        let path =
            std::env::temp_dir().join(format!("hp-wt-{safe}-{id8}-{}", std::process::id()));
        let out = Command::new("git")
            .args(["worktree", "add", "-b", &branch])
            .arg(&path)
            .arg("HEAD")
            .output()
            .map_err(|e| format!("git worktree add: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )
            .into());
        }
        // Ignore agent-scratch dirs in THIS worktree so a child `git add -A` can't sweep e.g.
        // Serena's auto-created `.serena/` into the commit (the contamination we hit 2026-06-24).
        if let Ok(o) = Command::new("git")
            .current_dir(&path)
            .args(["rev-parse", "--git-path", "info/exclude"])
            .output()
        {
            if o.status.success() {
                let rel = String::from_utf8_lossy(&o.stdout).trim().to_string();
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.join(rel))
                {
                    let _ = writeln!(f, ".serena/");
                }
            }
        }
        eprintln!("  [worktree] {} @ {branch}", path.display());
        Ok(Self { path })
    }

    fn remove(&self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn detects_worker_mode() {
        assert!(wants_worker(&argv(&["hyperpanes", "worker", "--queue", "q"])));
        assert!(!wants_worker(&argv(&["hyperpanes"])));
        assert!(!wants_worker(&argv(&["hyperpanes", "--session-daemon", "x"])));
    }

    #[test]
    fn parses_queue_worker_and_child() {
        let a = parse_args(&argv(&[
            "hp", "worker", "--queue", "hp-issues", "--worker", "w1", "--", "claude", "-p", "hi",
        ]))
        .unwrap();
        assert_eq!(a.queue, "hp-issues");
        assert_eq!(a.worker, "w1");
        assert_eq!(a.child, vec!["claude", "-p", "hi"]);
    }

    #[test]
    fn parses_eq_forms_and_defaults_worker() {
        let a = parse_args(&argv(&["hp", "worker", "--queue=q", "--", "true"])).unwrap();
        assert_eq!(a.queue, "q");
        assert!(a.worker.starts_with("worker-"));
        assert_eq!(a.child, vec!["true"]);
    }

    #[test]
    fn missing_queue_is_error() {
        assert!(parse_args(&argv(&["hp", "worker", "--", "true"])).is_err());
    }

    #[test]
    fn missing_child_is_error() {
        assert!(parse_args(&argv(&["hp", "worker", "--queue", "q"])).is_err());
        assert!(parse_args(&argv(&["hp", "worker", "--queue", "q", "--"])).is_err());
    }

    #[test]
    fn unknown_flag_is_error() {
        assert!(parse_args(&argv(&["hp", "worker", "--bogus", "--", "true"])).is_err());
    }

    #[test]
    fn parses_count_with_default_and_validation() {
        assert_eq!(
            parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"]))
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            parse_args(&argv(&[
                "hp", "worker", "--queue", "q", "--count", "4", "--", "true"
            ]))
            .unwrap()
            .count,
            4
        );
        assert_eq!(
            parse_args(&argv(&["hp", "worker", "--queue=q", "--count=2", "--", "true"]))
                .unwrap()
                .count,
            2
        );
        assert!(parse_args(&argv(&[
            "hp", "worker", "--queue", "q", "--count", "0", "--", "true"
        ]))
        .is_err());
        assert!(parse_args(&argv(&[
            "hp", "worker", "--queue", "q", "--count", "x", "--", "true"
        ]))
        .is_err());
    }

    #[test]
    fn parses_retry_window_and_nack_delay() {
        let a = parse_args(&argv(&[
            "hp",
            "worker",
            "--queue",
            "q",
            "--retry-window",
            "5",
            "--nack-delay",
            "250",
            "--",
            "true",
        ]))
        .unwrap();
        assert_eq!(a.retry_window_secs, 5);
        assert_eq!(a.nack_delay_ms, Some(250));

        let d = parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"])).unwrap();
        assert_eq!(d.retry_window_secs, 0);
        assert_eq!(d.nack_delay_ms, None);

        assert!(parse_args(&argv(&[
            "hp", "worker", "--queue", "q", "--retry-window", "x", "--", "true"
        ]))
        .is_err());
    }

    #[test]
    fn parses_worktree_flag() {
        assert!(
            parse_args(&argv(&["hp", "worker", "--queue", "q", "--worktree", "--", "true"]))
                .unwrap()
                .worktree
        );
        assert!(
            !parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"]))
                .unwrap()
                .worktree
        );
    }
}
