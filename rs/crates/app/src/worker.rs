//! `hyperpanes worker` — headless work-queue drain loop (worker runner MVP, issue #10).
//!
//! Usage:
//! ```text
//! hyperpanes worker --queue <name> [--worker <id>] [--count N] [--worktree --base <committish>] \
//!   [--retry-window <secs>] [--nack-delay <ms>] \
//!   [--stream] [--log-dir <dir>] [--linger <secs>] -- <cmd> [args...]
//! ```
//!
//! Discovers the running app's control API from `control.json` (or `HYPERPANES_CONTROL_FILE`),
//! then loops: **claim** one task → run `<cmd>` as a child with the task injected via env
//! (`HP_TASK_ID`, `HP_TASK_PAYLOAD`, `HP_FENCING_TOKEN`, `HP_QUEUE`, `HP_TASK_TITLE`) → **ack**
//! on child exit 0 / **nack** on non-zero → repeat until a claim comes back empty, then exit 0
//! (so a hyperpanes pane running the worker auto-closes on drain).
//!
//! Flags: `--count N` runs N competing workers in this process (#11); `--worktree` runs each
//! task in a throwaway git worktree that auto-removes (#14), forked from `--base <committish>`
//! (any branch, tag, or sha) — `--base` is REQUIRED with `--worktree`: the old implicit fork
//! point was the runner cwd's HEAD, i.e. whatever a shared checkout happened to have checked
//! out, and `--base HEAD` stays expressible for anyone who really means that;
//! `--retry-window <secs>` keeps
//! polling after the queue empties so backoff retries get reclaimed, and `--nack-delay <ms>`
//! overrides the retry backoff (#13). A lease heartbeat renews the lease while a task runs so a
//! long task isn't reclaimed mid-flight (#12).
//!
//! Visibility flags (a headless `claude -p` child prints nothing until it exits, and the pane
//! auto-closes the moment the queue drains, so a run could finish leaving no trace):
//! `--stream` renders the child's Claude `--output-format stream-json` events as readable
//! progress lines (non-JSON output passes through); `--log-dir <dir>` tees every child's raw
//! output to `<dir>/<queue>-<taskId>.log`, which outlives the pane; `--linger <secs>` holds the
//! process open after the drain so the pane stays readable. All three default off = the original
//! inherit-stdio, exit-on-drain behaviour.
//!
//! The child reads its task from the environment, so
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
    /// Fork point for `--worktree` task worktrees: any committish (branch, tag, sha). Required
    /// with `--worktree` (parse rejects the combination without it): the old implicit fallback —
    /// the runner cwd's HEAD, whatever a shared checkout happened to have checked out — is the
    /// footgun `--base` exists to remove. `None` only when `--worktree` is off.
    pub base: Option<String>,
    /// Render the child's Claude `--output-format stream-json` lines as readable progress
    /// instead of raw JSON, so a worker pane shows what the agent is doing while it runs.
    pub stream: bool,
    /// Tee every child's raw output to `<dir>/<queue>-<taskId>.log`, so the transcript survives
    /// the pane closing when the queue drains. `None` = no logs (frozen behaviour).
    pub log_dir: Option<PathBuf>,
    /// Stay alive this many seconds after the queue drains, so the pane (which auto-closes when
    /// this process exits) stays readable. 0 = exit immediately, as before.
    pub linger_secs: u64,
    /// Everything after `--`: program + args, executed directly (no shell).
    pub child: Vec<String>,
}

/// The bits of `control.json` the worker needs to reach the control API.
#[derive(Deserialize)]
struct Discovery {
    port: u16,
    token: String,
    /// Written only when the app binds a SPECIFIC address (mobile-client remote access);
    /// omitted for the default loopback bind. When present it is the only address the
    /// server listens on, so the worker must dial it rather than loopback.
    #[serde(rename = "bindAddress", default)]
    bind_address: Option<String>,
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
    let mut linger_arg: Option<String> = None;
    let mut log_dir: Option<PathBuf> = None;
    let mut stream = false;
    let mut worktree = false;
    let mut base: Option<String> = None;
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
                retry_window_arg = Some(
                    argv.get(i + 1)
                        .ok_or("--retry-window needs a value")?
                        .clone(),
                );
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
            "--base" => {
                base = Some(argv.get(i + 1).ok_or("--base needs a value")?.clone());
                i += 2;
            }
            "--stream" => {
                stream = true;
                i += 1;
            }
            "--log-dir" => {
                log_dir = Some(PathBuf::from(
                    argv.get(i + 1).ok_or("--log-dir needs a value")?,
                ));
                i += 2;
            }
            "--linger" => {
                linger_arg = Some(argv.get(i + 1).ok_or("--linger needs a value")?.clone());
                i += 2;
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
                } else if let Some(v) = other.strip_prefix("--base=") {
                    base = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--log-dir=") {
                    log_dir = Some(PathBuf::from(v));
                } else if let Some(v) = other.strip_prefix("--linger=") {
                    linger_arg = Some(v.to_string());
                } else {
                    return Err(format!("unexpected argument: {other}"));
                }
                i += 1;
            }
        }
    }
    let queue = queue.ok_or("missing --queue <name>")?;
    if worktree && base.is_none() {
        return Err(
            "--worktree now requires --base <committish>: the fork point for task worktrees must \
             be explicit, not whatever the shared checkout's HEAD happens to be. Use `--base main` \
             for independent work, or `--base <your-integration-branch>` for a dependent wave \
             (`--base HEAD` keeps the old behaviour if you really mean it)."
                .to_string(),
        );
    }
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
    let linger_secs = match linger_arg {
        Some(s) => s
            .parse()
            .map_err(|_| format!("--linger must be a non-negative integer (secs), got '{s}'"))?,
        None => 0,
    };
    Ok(WorkerArgs {
        queue,
        worker,
        count,
        retry_window_secs,
        nack_delay_ms,
        worktree,
        base,
        stream,
        log_dir,
        linger_secs,
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
/// Panes may inherit `HYPERPANES_CONTROL_FILE` set-but-empty from the app; treat empty as unset.
fn load_discovery() -> Result<Discovery, Box<dyn Error>> {
    let path = std::env::var_os("HYPERPANES_CONTROL_FILE")
        .filter(|v| !v.is_empty())
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

/// Base URL the worker dials for the control API.
///
/// Honours a specific `bindAddress`: that is a single-socket bind, so loopback is NOT
/// listening and every claim fails with ConnectionRefused — the worker then exits before
/// claiming, which from the outside looks like an empty queue rather than a config fault.
/// An unspecified (`0.0.0.0`/`::`) or absent bind does listen on loopback, so prefer it.
fn control_base(disco: &Discovery) -> String {
    crate::control_cli::base_url(disco.port, disco.bind_address.as_deref())
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
    let base = control_base(&disco);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let retry_window = Duration::from_secs(args.retry_window_secs);
    let nack_delay = args.nack_delay_ms;
    let worktree = args.worktree;
    // Fork point for per-task worktrees — parse guarantees `--base` whenever `--worktree` is on
    // (the "HEAD" fallback only feeds the non-worktree path, where it is never used). Resolve it
    // up front so a typo'd committish fails loudly here, before any task is claimed.
    let fork_base = args.base.clone().unwrap_or_else(|| "HEAD".to_string());
    if worktree {
        validate_base(&fork_base)?;
    }
    // Output policy shared by every worker thread: render Claude's stream-json readably and/or
    // tee raw child output to a per-task log that outlives the pane.
    let out = Arc::new(ChildOutput {
        stream: args.stream,
        log_dir: args.log_dir.clone(),
    });
    if let Some(dir) = &out.log_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("worker: cannot create --log-dir {}: {e}", dir.display());
        } else {
            eprintln!("worker: per-task logs in {}", dir.display());
        }
    }

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
            &fork_base,
            &out,
        )?;
        eprintln!(
            "[{}] queue drained — {done} task(s) acked, exiting",
            args.worker
        );
        linger(args.linger_secs);
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
        let out = Arc::clone(&out);
        let fork_base = fork_base.clone();
        handles.push(std::thread::spawn(move || {
            match drain(
                &client,
                &base,
                &token,
                &queue,
                &worker,
                &child,
                retry_window,
                nack_delay,
                worktree,
                &fork_base,
                &out,
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
    linger(args.linger_secs);
    Ok(())
}

/// Hold the process (and therefore its pane, which auto-closes on exit) open after the drain, so
/// the run stays readable. No-op at 0.
fn linger(secs: u64) {
    if secs == 0 {
        return;
    }
    eprintln!("worker: holding this pane open for {secs}s (--linger)");
    std::thread::sleep(Duration::from_secs(secs));
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
    fork_base: &str,
    out: &ChildOutput,
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
        // after (the commit, if any, stays on its branch). If the worktree can't be created,
        // the task must NOT fall through to the runner's own cwd — that silently strips the
        // isolation the caller asked for and puts the child in the shared checkout (the
        // 2026-07-30 incident class). Nack instead: worktree failures are usually environmental
        // (stale branch, git lock, disk), so the standard retry path applies, bounded by the
        // queue's max_attempts.
        let wt = if worktree {
            match Worktree::create(queue, &task.id, fork_base) {
                Ok(w) => Some(w),
                Err(e) => {
                    let reason = format!("worktree create failed (base {fork_base}): {e}");
                    eprintln!("[{worker}] !! {reason}");
                    eprintln!("[{worker}] !! child NOT run — refusing to execute without the requested isolation");
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
                        "[{worker}] !! nacked {} (attempt {}/{}) → {state}",
                        short(&task.id),
                        task.attempts,
                        task.max_attempts
                    );
                    continue;
                }
            }
        } else {
            None
        };
        let cwd = wt.as_ref().map(|w| w.path.as_path());
        let outcome = run_child(child, &task, queue, client, base, token, cwd, worker, out);
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

/// How a child's output reaches the operator. Default (`stream: false`, `log_dir: None`) inherits
/// the worker's stdio — byte-for-byte the original behaviour. Either option switches to piped
/// stdio so the runner can render and/or persist what the child says.
#[derive(Debug, Default)]
pub struct ChildOutput {
    pub stream: bool,
    pub log_dir: Option<PathBuf>,
}

impl ChildOutput {
    fn piped(&self) -> bool {
        self.stream || self.log_dir.is_some()
    }
    /// Where this task's raw transcript is kept (so it outlives the pane).
    fn log_path(&self, queue: &str, task_id: &str) -> Option<PathBuf> {
        self.log_dir
            .as_ref()
            .map(|d| d.join(format!("{queue}-{}.log", short(task_id))))
    }
}

/// Run the child command with the task in its environment, while a background heartbeat renews
/// the lease (#12) so a long-running task is not reclaimed mid-flight. Returns Ok(true) on exit 0.
#[allow(clippy::too_many_arguments)]
fn run_child(
    child: &[String],
    task: &Task,
    queue: &str,
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    cwd: Option<&Path>,
    worker: &str,
    out: &ChildOutput,
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
    let result = if out.piped() {
        run_child_piped(cmd, task, queue, worker, out)
    } else {
        cmd.status()
            .map(|s| s.success())
            .map_err(|e| format!("failed to spawn '{}': {e}", child[0]))
    };

    // Stop the heartbeat before ack/nack so we never extend a finished task.
    stop.store(true, Ordering::Relaxed);
    if let Some(h) = heartbeat {
        let _ = h.join();
    }
    Ok(result?)
}

/// Piped variant of the child run: stdout+stderr are captured line-by-line so each line can be
/// (a) appended raw to this task's log — the transcript that survives the pane closing — and
/// (b) rendered into the pane. With `--stream`, Claude `--output-format stream-json` lines become
/// readable progress; anything else passes through unchanged, so a plain shell task still looks
/// the same. Every line is prefixed with the worker + task so `--count N` interleaving is legible.
fn run_child_piped(
    mut cmd: Command,
    task: &Task,
    queue: &str,
    worker: &str,
    out: &ChildOutput,
) -> Result<bool, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let log = out.log_path(queue, &task.id).and_then(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)
            .map_err(|e| eprintln!("[{worker}] cannot open log {}: {e}", p.display()))
            .ok()
            .map(|f| (p, Arc::new(std::sync::Mutex::new(f))))
    });
    if let Some((p, _)) = &log {
        eprintln!("[{worker}] log: {}", p.display());
    }

    let mut c = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn child: {e}"))?;

    let tag = format!("[{worker}/{}]", short(&task.id));
    let stream = out.stream;
    let mut pumps = Vec::new();
    for (reader, is_stderr) in [
        (
            c.stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            false,
        ),
        (
            c.stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            true,
        ),
    ] {
        let Some(reader) = reader else { continue };
        let tag = tag.clone();
        let log = log.as_ref().map(|(_, f)| Arc::clone(f));
        pumps.push(std::thread::spawn(move || {
            for line in BufReader::new(reader).lines().map_while(Result::ok) {
                if let Some((_, f)) = log.as_ref().map(|f| ((), f)) {
                    if let Ok(mut f) = f.lock() {
                        let _ = writeln!(f, "{line}");
                    }
                }
                // stderr is already human text; only stdout carries the JSON stream.
                match if stream && !is_stderr {
                    render_stream_line(&line)
                } else {
                    Some(line)
                } {
                    Some(text) if !text.is_empty() => eprintln!("{tag} {text}"),
                    _ => {}
                }
            }
        }));
    }
    let status = c.wait().map_err(|e| format!("child wait failed: {e}"))?;
    for p in pumps {
        let _ = p.join();
    }
    Ok(status.success())
}

/// Render one line of Claude's `--output-format stream-json` into a short human line, or `None`
/// to drop it. A line that isn't such an event (plain program output) is returned unchanged, so
/// this is safe to run over any child's stdout.
fn render_stream_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') {
        return Some(line.to_string());
    }
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return Some(line.to_string()),
    };
    match v.get("type").and_then(serde_json::Value::as_str) {
        Some("system") => {
            let model = v.get("model").and_then(serde_json::Value::as_str)?;
            Some(format!("▶ started ({model})"))
        }
        Some("assistant") => {
            let content = v.get("message")?.get("content")?.as_array()?;
            let mut parts: Vec<String> = Vec::new();
            for block in content {
                match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("text") => {
                        let t = block.get("text").and_then(serde_json::Value::as_str)?;
                        let t = t.trim();
                        if !t.is_empty() {
                            parts.push(one_line(t, 300));
                        }
                    }
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("tool");
                        parts.push(format!("⚙ {name}"));
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                None
            } else {
                Some(parts.join(" "))
            }
        }
        Some("result") => {
            let ok = v
                .get("is_error")
                .and_then(serde_json::Value::as_bool)
                .map(|e| !e)
                .unwrap_or(true);
            let text = v
                .get("result")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            Some(format!(
                "{} {}",
                if ok { "✓ done:" } else { "✗ failed:" },
                one_line(text, 300)
            ))
        }
        // user turns are tool results — noise in a progress view.
        _ => None,
    }
}

/// Collapse whitespace and clip to `max` chars, so one rendered event stays one readable line.
fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let clipped: String = flat.chars().take(max).collect();
    format!("{clipped}…")
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

/// Early, loud check that the `--worktree` fork point resolves to a commit in the runner's cwd,
/// before any task is claimed. Each task still resolves the ref at claim time (see
/// `Worktree::create_in`), so a branch that advances mid-run — a dependent wave's integration
/// branch — is picked up per task.
fn validate_base(base: &str) -> Result<(), Box<dyn Error>> {
    let ok = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet"])
        .arg(format!("{base}^{{commit}}"))
        .output()
        .is_ok_and(|o| o.status.success());
    if ok {
        return Ok(());
    }
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "the worker's cwd".to_string());
    Err(format!("--base {base}: does not resolve to a commit in {cwd} — pass a branch, tag, or sha that exists there").into())
}

/// A throwaway git worktree for one task (#14). Created off an explicit base committish on a
/// fresh branch; the working dir is removed when the task finishes (a commit, if any, stays on
/// the branch). Created from the worker's cwd, so `--worktree` requires running inside a git
/// repo.
struct Worktree {
    path: PathBuf,
    /// The repo the worktree was created from — `remove()` must run git there, not in whatever
    /// the process cwd is by then.
    repo: PathBuf,
}

impl Worktree {
    fn create(queue: &str, task_id: &str, base: &str) -> Result<Self, Box<dyn Error>> {
        let repo =
            std::env::current_dir().map_err(|e| format!("worktree: cannot resolve cwd: {e}"))?;
        Self::create_in(&repo, queue, task_id, base)
    }

    /// Explicit-repo body so tests can prove the fork point without touching the process cwd.
    /// `base` is resolved by git HERE, at task start — not pinned at runner start — so a ref
    /// that advances between tasks forks from its current tip.
    fn create_in(
        repo: &Path,
        queue: &str,
        task_id: &str,
        base: &str,
    ) -> Result<Self, Box<dyn Error>> {
        let id8 = &task_id[..8.min(task_id.len())];
        let safe = queue.replace(['/', ' '], "-");
        let branch = format!("worker/{safe}/{id8}");
        let path = std::env::temp_dir().join(format!("hp-wt-{safe}-{id8}-{}", std::process::id()));
        // The branch name is deterministic (per task id), so a stale `worker/<q>/<id8>` left by a
        // prior run makes `git worktree add -b` fail. Prune dead worktree admin entries first, then
        // decide by whether the stale branch carries uncollected commits: if it's an ancestor of
        // the base (nothing beyond it) reset it with `-B`; if it's AHEAD of the base (uncollected
        // impl work per the commit-first protocol, or checked out in a live worktree) refuse and
        // surface it rather than clobber committed work.
        let _ = Command::new("git")
            .current_dir(repo)
            .args(["worktree", "prune"])
            .output();
        let branch_exists = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{branch}"))
            .output()
            .is_ok_and(|o| o.status.success());
        let add_flag = if branch_exists {
            let safe_to_reset = Command::new("git")
                .current_dir(repo)
                .args(["merge-base", "--is-ancestor", &branch, base])
                .output()
                .is_ok_and(|o| o.status.success());
            if !safe_to_reset {
                return Err(format!(
                    "git worktree add: branch {branch} already exists with commits not in {base} \
                     (uncollected work from a prior run?) — collect or delete it before retrying"
                )
                .into());
            }
            "-B" // stale but empty relative to the base → safe to re-point onto it
        } else {
            "-b"
        };
        let out = Command::new("git")
            .current_dir(repo)
            .args(["worktree", "add", add_flag, &branch])
            .arg(&path)
            .arg(base)
            .output()
            .map_err(|e| format!("git worktree add: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "git worktree add failed for {branch} at {}: {}",
                path.display(),
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
        eprintln!("  [worktree] {} @ {branch} (base {base})", path.display());
        Ok(Self {
            path,
            repo: repo.to_path_buf(),
        })
    }

    fn remove(&self) {
        let _ = Command::new("git")
            .current_dir(&self.repo)
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

    /// The worker dials whatever `control.json` says the server bound to. A specific
    /// `bindAddress` is a single-socket bind, so loopback is NOT listening: hardcoding
    /// 127.0.0.1 made every claim fail with ConnectionRefused and the worker exit before
    /// claiming — indistinguishable from an empty queue.
    fn base_from_control_json(json: &str) -> String {
        let d: Discovery = serde_json::from_str(json).expect("control.json parses");
        // Calls the PRODUCTION helper `run()` uses, so reverting that line turns these red.
        control_base(&d)
    }

    #[test]
    fn dials_a_specific_bind_address_not_loopback() {
        assert_eq!(
            base_from_control_json(r#"{"port":41419,"token":"t","bindAddress":"100.120.216.17"}"#),
            "http://100.120.216.17:41419"
        );
    }

    #[test]
    fn brackets_a_specific_ipv6_bind_address() {
        assert_eq!(
            base_from_control_json(r#"{"port":41419,"token":"t","bindAddress":"fd7a::1"}"#),
            "http://[fd7a::1]:41419"
        );
    }

    #[test]
    fn falls_back_to_loopback_when_bind_address_is_absent_or_unspecified() {
        // Legacy control.json (no bindAddress at all) must keep working.
        assert_eq!(
            base_from_control_json(r#"{"port":41419,"token":"t"}"#),
            "http://127.0.0.1:41419"
        );
        // An unspecified bind DOES listen on loopback, so prefer it.
        assert_eq!(
            base_from_control_json(r#"{"port":41419,"token":"t","bindAddress":"0.0.0.0"}"#),
            "http://127.0.0.1:41419"
        );
        assert_eq!(
            base_from_control_json(r#"{"port":41419,"token":"t","bindAddress":"::"}"#),
            "http://127.0.0.1:41419"
        );
    }

    #[test]
    fn detects_worker_mode() {
        assert!(wants_worker(&argv(&[
            "hyperpanes",
            "worker",
            "--queue",
            "q"
        ])));
        assert!(!wants_worker(&argv(&["hyperpanes"])));
        assert!(!wants_worker(&argv(&[
            "hyperpanes",
            "--session-daemon",
            "x"
        ])));
    }

    #[test]
    fn parses_queue_worker_and_child() {
        let a = parse_args(&argv(&[
            "hp",
            "worker",
            "--queue",
            "hp-issues",
            "--worker",
            "w1",
            "--",
            "claude",
            "-p",
            "hi",
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
            parse_args(&argv(&[
                "hp",
                "worker",
                "--queue=q",
                "--count=2",
                "--",
                "true"
            ]))
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
            "hp",
            "worker",
            "--queue",
            "q",
            "--retry-window",
            "x",
            "--",
            "true"
        ]))
        .is_err());
    }

    #[test]
    fn parses_visibility_flags_with_frozen_defaults() {
        let a = parse_args(&argv(&[
            "hp",
            "worker",
            "--queue",
            "q",
            "--stream",
            "--log-dir",
            "/tmp/hp-logs",
            "--linger",
            "30",
            "--",
            "true",
        ]))
        .unwrap();
        assert!(a.stream);
        assert_eq!(a.log_dir.as_deref(), Some(Path::new("/tmp/hp-logs")));
        assert_eq!(a.linger_secs, 30);

        let eq = parse_args(&argv(&[
            "hp",
            "worker",
            "--queue=q",
            "--log-dir=/tmp/l",
            "--linger=5",
            "--",
            "true",
        ]))
        .unwrap();
        assert_eq!(eq.log_dir.as_deref(), Some(Path::new("/tmp/l")));
        assert_eq!(eq.linger_secs, 5);

        // Defaults keep the original inherit-stdio, exit-on-drain behaviour.
        let d = parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"])).unwrap();
        assert!(!d.stream);
        assert_eq!(d.log_dir, None);
        assert_eq!(d.linger_secs, 0);
        assert!(!ChildOutput::default().piped());
        assert!(parse_args(&argv(&[
            "hp", "worker", "--queue", "q", "--linger", "x", "--", "true"
        ]))
        .is_err());
    }

    #[test]
    fn renders_claude_stream_json_events_and_passes_other_output_through() {
        // Plain program output is untouched — a non-claude task still reads normally.
        assert_eq!(
            render_stream_line("cargo test: 12 passed").as_deref(),
            Some("cargo test: 12 passed")
        );
        // Malformed JSON is output, not an error.
        assert_eq!(
            render_stream_line("{not json").as_deref(),
            Some("{not json")
        );
        // Assistant text + tool calls collapse to one progress line.
        let a = render_stream_line(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixing the\n  installer"},{"type":"tool_use","name":"Edit"}]}}"#,
        )
        .unwrap();
        assert_eq!(a, "Fixing the installer ⚙ Edit");
        // Tool results (user turns) are dropped as noise.
        assert_eq!(
            render_stream_line(r#"{"type":"user","message":{"content":[]}}"#),
            None
        );
        // The final result line reports success/failure.
        let ok = render_stream_line(r#"{"type":"result","is_error":false,"result":"branch x"}"#)
            .unwrap();
        assert_eq!(ok, "✓ done: branch x");
        let bad = render_stream_line(r#"{"type":"result","is_error":true,"result":"build broke"}"#)
            .unwrap();
        assert_eq!(bad, "✗ failed: build broke");
    }

    #[test]
    fn per_task_log_path_is_queue_and_task_scoped() {
        let out = ChildOutput {
            stream: false,
            log_dir: Some(PathBuf::from("/tmp/logs")),
        };
        assert!(out.piped());
        assert_eq!(
            out.log_path("g7", "0123456789abcdef"),
            Some(PathBuf::from("/tmp/logs/g7-01234567.log"))
        );
        assert_eq!(ChildOutput::default().log_path("g7", "abc"), None);
    }

    #[test]
    fn parses_worktree_flag() {
        assert!(
            parse_args(&argv(&[
                "hp",
                "worker",
                "--queue",
                "q",
                "--worktree",
                "--base",
                "main",
                "--",
                "true"
            ]))
            .unwrap()
            .worktree
        );
        assert!(
            !parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"]))
                .unwrap()
                .worktree
        );
    }

    #[test]
    fn worktree_without_base_is_refused_with_teaching_error() {
        let err = parse_args(&argv(&[
            "hp",
            "worker",
            "--queue",
            "q",
            "--worktree",
            "--",
            "true",
        ]))
        .unwrap_err();
        // The refusal must teach the fix: name the flag, state the new requirement, and show
        // both real forms — independent work off main and a dependent wave off its integration
        // branch.
        assert!(err.contains("--worktree now requires --base"), "{err}");
        assert!(err.contains("--base main"), "{err}");
        assert!(err.contains("--base <your-integration-branch>"), "{err}");
        // Without --worktree, --base stays optional.
        assert!(parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"])).is_ok());
    }

    #[test]
    fn parses_base_flag() {
        assert_eq!(
            parse_args(&argv(&[
                "hp",
                "worker",
                "--queue",
                "q",
                "--worktree",
                "--base",
                "main",
                "--",
                "true"
            ]))
            .unwrap()
            .base
            .as_deref(),
            Some("main")
        );
        assert_eq!(
            parse_args(&argv(&[
                "hp",
                "worker",
                "--queue=q",
                "--worktree",
                "--base=g5/int",
                "--",
                "true"
            ]))
            .unwrap()
            .base
            .as_deref(),
            Some("g5/int")
        );
        assert_eq!(
            parse_args(&argv(&["hp", "worker", "--queue", "q", "--", "true"]))
                .unwrap()
                .base,
            None
        );
        assert!(parse_args(&argv(&["hp", "worker", "--queue", "q", "--base"])).is_err());
    }

    /// Run git in `repo` with host config neutralized (a global `commit.gpgsign` etc. must not
    /// leak into the scratch repo), panicking on failure so a broken fixture reads as the test's
    /// own assertion.
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

    /// Scratch repo whose HEAD sits on a diverged branch — the shared-checkout hazard the
    /// `--base` flag exists for. Layout: `main` at commit A; `int` = A + one commit;
    /// `wrong` = A + one commit, checked out (HEAD).
    fn scratch_repo(tag: &str) -> PathBuf {
        let repo = std::env::temp_dir().join(format!("hp-base-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-b", "main"]);
        git(&repo, &["commit", "--allow-empty", "-m", "A"]);
        git(&repo, &["checkout", "-b", "int"]);
        git(&repo, &["commit", "--allow-empty", "-m", "B"]);
        git(&repo, &["checkout", "-b", "wrong", "main"]);
        git(&repo, &["commit", "--allow-empty", "-m", "C"]);
        repo
    }

    #[test]
    fn worktree_forks_from_explicit_base_not_cwd_head() {
        let repo = scratch_repo("fork");
        let head = git(&repo, &["rev-parse", "HEAD"]);
        let main = git(&repo, &["rev-parse", "main"]);
        assert_ne!(
            head, main,
            "fixture must diverge or the test proves nothing"
        );

        // Explicit --base main forks from main even though the checkout's HEAD is elsewhere.
        let wt = Worktree::create_in(&repo, "q", "aaaa0000", "main").unwrap();
        assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), main);
        wt.remove();

        // A dependent wave forks its integration branch the same way.
        let int = git(&repo, &["rev-parse", "int"]);
        let wt = Worktree::create_in(&repo, "q", "bbbb0000", "int").unwrap();
        assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), int);
        wt.remove();

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn stale_task_branch_guard_compares_against_base_not_head() {
        let repo = scratch_repo("guard");

        // Stale branch AT the base (no uncollected work) → reset with -B and fork the base.
        git(&repo, &["branch", "worker/q/cccc0000", "main"]);
        let wt = Worktree::create_in(&repo, "q", "cccc0000", "main").unwrap();
        assert_eq!(
            git(&wt.path, &["rev-parse", "HEAD"]),
            git(&repo, &["rev-parse", "main"])
        );
        wt.remove();

        // Stale branch AHEAD of the base (uncollected commits; here it sits on `wrong`) → refuse
        // even though HEAD == `wrong` would have made the old HEAD-relative check wave it through.
        git(&repo, &["branch", "worker/q/dddd0000", "wrong"]);
        let err = match Worktree::create_in(&repo, "q", "dddd0000", "main") {
            Err(e) => e.to_string(),
            Ok(_) => panic!("stale branch ahead of base must be refused"),
        };
        assert!(err.contains("already exists"), "unexpected error: {err}");

        let _ = std::fs::remove_dir_all(&repo);
    }
}
