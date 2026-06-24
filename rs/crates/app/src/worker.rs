//! `hyperpanes worker` — headless work-queue drain loop (worker runner MVP, issue #10).
//!
//! Usage:
//! ```text
//! hyperpanes worker --queue <name> [--worker <id>] -- <cmd> [args...]
//! ```
//!
//! Discovers the running app's control API from `control.json` (or `HYPERPANES_CONTROL_FILE`),
//! then loops: **claim** one task → run `<cmd>` as a child with the task injected via env
//! (`HP_TASK_ID`, `HP_TASK_PAYLOAD`, `HP_FENCING_TOKEN`, `HP_QUEUE`, `HP_TASK_TITLE`) → **ack**
//! on child exit 0 / **nack** on non-zero → repeat until a claim comes back empty, then exit 0
//! (so a hyperpanes pane running the worker auto-closes on drain).
//!
//! Single worker in this slice; `--count N` (concurrency), lease heartbeat, retry/backoff and
//! `--worktree` isolation are follow-up slices (#11–#14). The child reads its task from the
//! environment, so shell expansion like `$HP_TASK_PAYLOAD` requires an explicit inner shell:
//! `-- sh -c 'claude -p "$HP_TASK_PAYLOAD"'`.

use std::error::Error;
use std::process::Command;
use std::time::Duration;

use serde::Deserialize;

/// Parsed `hyperpanes worker` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct WorkerArgs {
    pub queue: String,
    pub worker: String,
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
            "--" => {
                child = argv[i + 1..].to_vec();
                break;
            }
            other => {
                if let Some(v) = other.strip_prefix("--queue=") {
                    queue = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("--worker=") {
                    worker = Some(v.to_string());
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
    Ok(WorkerArgs {
        queue,
        worker,
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

    eprintln!(
        "[{}] worker online — draining '{}' via {base}",
        args.worker, args.queue
    );

    let mut done: u64 = 0;
    loop {
        let Some(task) = claim_one(&client, &base, &disco.token, &args.queue, &args.worker)? else {
            eprintln!(
                "[{}] queue drained — {done} task(s) acked, exiting",
                args.worker
            );
            return Ok(());
        };
        eprintln!(
            "[{}] >> claimed {} (fence {}) :: {}",
            args.worker,
            short(&task.id),
            task.fencing_token,
            task.title
        );

        match run_child(&args.child, &task, &args.queue) {
            Ok(true) => {
                ack(&client, &base, &disco.token, &task.id, task.fencing_token)?;
                done += 1;
                eprintln!("[{}] << acked  {}", args.worker, short(&task.id));
            }
            other => {
                let reason = match other {
                    Ok(false) => "child exited non-zero".to_string(),
                    Err(e) => e.to_string(),
                    Ok(true) => unreachable!(),
                };
                nack(
                    &client,
                    &base,
                    &disco.token,
                    &task.id,
                    task.fencing_token,
                    &reason,
                )?;
                eprintln!("[{}] !! nacked {} ({reason})", args.worker, short(&task.id));
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

/// Run the child command with the task in its environment. Returns Ok(true) on exit 0.
fn run_child(child: &[String], task: &Task, queue: &str) -> Result<bool, Box<dyn Error>> {
    let status = Command::new(&child[0])
        .args(&child[1..])
        .env("HP_TASK_ID", &task.id)
        .env("HP_TASK_PAYLOAD", &task.payload)
        .env("HP_TASK_TITLE", &task.title)
        .env("HP_FENCING_TOKEN", task.fencing_token.to_string())
        .env("HP_QUEUE", queue)
        .status()
        .map_err(|e| format!("failed to spawn '{}': {e}", child[0]))?;
    Ok(status.success())
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

fn nack(
    client: &reqwest::blocking::Client,
    base: &str,
    token: &str,
    id: &str,
    fence: u64,
    error: &str,
) -> Result<(), Box<dyn Error>> {
    let resp = client
        .post(format!("{base}/tasks/{id}/nack"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "fencingToken": fence, "error": error }))
        .send()?;
    if !resp.status().is_success() {
        return Err(format!("nack failed: HTTP {}", resp.status().as_u16()).into());
    }
    Ok(())
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
}
