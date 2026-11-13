//! Respawn ordering: a pane that is killed and spawned again under the same pane id
//! (`restartPane`, `recoverPane` resume, the GUI's Restart) gets its new process only once
//! the old session's processes have exited, or [`RESPAWN_WAIT`] after the kill.
//!
//! Killing a pty signals only its direct child (SIGHUP, then SIGKILL after a short grace),
//! and the terminal hang-up then signals the rest of the old session. A program that shuts
//! down gracefully can take a second or more to go: an agent CLI flushing its transcript, or
//! a launcher's child it waits on. A respawned command started at once raced it. A launcher
//! that refuses to start a second copy of a session while the first still runs then exited,
//! and the pane closed.
//!
//! The wait never blocks the caller. The control server holds its read-model lock across a
//! dispatch, and the session daemon handles one client's messages in order. So
//! [`SessionRegistry::create`](crate::session_manager::SessionRegistry::create) returns at
//! once with a [`DeferredPty`]. That pty buffers writes and resizes, and a helper thread
//! spawns the real pty once the old session is gone or the bound passes.

use std::collections::HashMap;
use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::pty::{Pty, PtyEvent, PtySpec};
use crate::session_manager::{EventSink, SpawnFn};

/// The longest a respawn waits for the old session's processes, counted from the kill.
pub const RESPAWN_WAIT: Duration = Duration::from_secs(5);

/// How often a waiting respawn looks again.
const POLL: Duration = Duration::from_millis(50);

/// Whether the session a leader pid leads still has a live process (see
/// [`session_has_live_processes`]); a test injects its own.
pub(crate) type LiveProbe = Arc<dyn Fn(u32) -> bool + Send + Sync>;

/// How a respawn waits: the liveness probe, the bound after the kill and the poll interval.
#[derive(Clone)]
pub(crate) struct RespawnPolicy {
    pub probe: LiveProbe,
    pub bound: Duration,
    pub poll: Duration,
}

impl Default for RespawnPolicy {
    fn default() -> Self {
        Self {
            probe: Arc::new(session_has_live_processes),
            bound: RESPAWN_WAIT,
            poll: POLL,
        }
    }
}

/// A killed session whose processes may still be exiting: the pid of the pty child that
/// led it, and when it was killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Dying {
    pub leader: u32,
    pub killed_at: Instant,
}

/// Each pane id's last killed session, until that pane spawns again or the bound passes.
#[derive(Default)]
pub(crate) struct DyingSessions(HashMap<String, Dying>);

impl DyingSessions {
    /// Remembers `dying` as `pane_id`'s last killed session, dropping entries past `bound`.
    pub fn record(&mut self, pane_id: &str, dying: Dying, bound: Duration) {
        self.prune(bound);
        self.0.insert(pane_id.to_string(), dying);
    }

    /// Takes `pane_id`'s last killed session, if it was killed within `bound`.
    pub fn take(&mut self, pane_id: &str, bound: Duration) -> Option<Dying> {
        self.prune(bound);
        self.0.remove(pane_id)
    }

    fn prune(&mut self, bound: Duration) {
        self.0.retain(|_, d| d.killed_at.elapsed() < bound);
    }
}

/// Polls until the dying session has no live process, or until `bound` after its kill.
/// True when it went in time.
fn wait_gone(dying: Dying, policy: &RespawnPolicy) -> bool {
    let deadline = dying.killed_at + policy.bound;
    loop {
        if !(policy.probe)(dying.leader) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(policy.poll.min(deadline - now));
    }
}

/// The state of a [`DeferredPty`].
enum Deferred {
    /// Waiting for the old session. Holds the input and the last size the pane got
    /// meanwhile, applied once the real pty spawns.
    Waiting {
        input: Vec<u8>,
        size: Option<(u16, u16)>,
    },
    Live(Box<dyn Pty>),
    /// Killed before it spawned, killed after, or its spawn failed.
    Done,
}

/// A pty that spawns once a dying predecessor of the same pane has exited (see the module
/// docs). Until then it buffers writes and the last resize. A kill during the wait cancels
/// the spawn.
pub(crate) struct DeferredPty {
    state: Arc<Mutex<Deferred>>,
}

impl DeferredPty {
    /// Starts the helper thread that waits on `dying` under `policy`, then spawns `spec`
    /// through `factory` with `sink`. A spawn that fails sends `Exit(-1)` on the sink, so
    /// the pane shows a dead session instead of hanging blank. `uid` and `pane_id` only
    /// label the session log.
    pub(crate) fn spawn_after(
        dying: Dying,
        policy: RespawnPolicy,
        spec: PtySpec,
        sink: EventSink,
        factory: SpawnFn,
        uid: String,
        pane_id: Option<String>,
    ) -> io::Result<Self> {
        let state = Arc::new(Mutex::new(Deferred::Waiting {
            input: Vec::new(),
            size: None,
        }));
        let shared = Arc::clone(&state);
        thread::Builder::new()
            .name("hp-respawn-wait".into())
            .spawn(move || {
                let gone = wait_gone(dying, &policy);
                let waited = dying.killed_at.elapsed().as_millis();
                let reason = if gone {
                    format!("previous session gone {waited} ms after its kill")
                } else {
                    format!("previous session still running {waited} ms after its kill; spawning anyway")
                };
                crate::persistence::session_log::record(
                    "respawn",
                    &uid,
                    &reason,
                    pane_id.as_deref(),
                );
                let mut guard = shared.lock().unwrap();
                let Deferred::Waiting { input, size } =
                    std::mem::replace(&mut *guard, Deferred::Done)
                else {
                    return; // killed during the wait: never spawn
                };
                match factory(&spec, Arc::clone(&sink)) {
                    Ok(pty) => {
                        if let Some((cols, rows)) = size {
                            let _ = pty.resize(cols, rows);
                        }
                        if !input.is_empty() {
                            let _ = pty.write(&input);
                        }
                        *guard = Deferred::Live(pty);
                    }
                    Err(e) => {
                        drop(guard);
                        crate::persistence::session_log::record(
                            "respawn-failed",
                            &uid,
                            &e.to_string(),
                            pane_id.as_deref(),
                        );
                        sink(PtyEvent::Exit(-1));
                    }
                }
            })?;
        Ok(Self { state })
    }
}

impl Pty for DeferredPty {
    fn write(&self, data: &[u8]) -> io::Result<()> {
        match &mut *self.state.lock().unwrap() {
            Deferred::Waiting { input, .. } => {
                input.extend_from_slice(data);
                Ok(())
            }
            Deferred::Live(pty) => pty.write(data),
            Deferred::Done => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        match &mut *self.state.lock().unwrap() {
            Deferred::Waiting { size, .. } => {
                *size = Some((cols, rows));
                Ok(())
            }
            Deferred::Live(pty) => pty.resize(cols, rows),
            Deferred::Done => Ok(()),
        }
    }

    fn kill(&self) -> io::Result<()> {
        let previous = std::mem::replace(&mut *self.state.lock().unwrap(), Deferred::Done);
        match previous {
            Deferred::Live(pty) => pty.kill(),
            Deferred::Waiting { .. } | Deferred::Done => Ok(()),
        }
    }

    /// The real child's pid once it spawned; `None` while it waits.
    fn pid(&self) -> Option<u32> {
        match &*self.state.lock().unwrap() {
            Deferred::Live(pty) => pty.pid(),
            Deferred::Waiting { .. } | Deferred::Done => None,
        }
    }
}

/// Whether the session that `leader` leads still has a live process. On unix the pty child
/// leads its own session (portable-pty `setsid`s it), so this covers every process it
/// started inside the terminal. Zombies count as gone: they have exited and only wait to
/// be reaped. Linux reads `/proc`.
#[cfg(target_os = "linux")]
pub(crate) fn session_has_live_processes(leader: u32) -> bool {
    if leader <= 1 {
        return false;
    }
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            return false;
        };
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| parse_stat(&stat))
            .is_some_and(|(state, sid)| sid == leader && state != 'Z' && state != 'X')
    })
}

/// Other unix systems have no `/proc`, so this probes the process group the child leads.
/// portable-pty makes the child a group leader too. A process that moved to another group
/// of the session is missed.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn session_has_live_processes(leader: u32) -> bool {
    match i32::try_from(leader) {
        // SAFETY: signal 0 only checks that the group exists; nothing is delivered.
        Ok(pgid) if pgid > 1 => unsafe { libc::kill(-pgid, 0) == 0 },
        _ => false,
    }
}

/// Windows: no wait. The respawn spawns at once, as before.
#[cfg(not(unix))]
pub(crate) fn session_has_live_processes(_leader: u32) -> bool {
    false
}

/// `(state, session id)` from a `/proc/<pid>/stat` line. The fields after the `)` that ends
/// the command name are `state ppid pgrp session …`. The name itself may hold spaces or
/// parentheses, hence the last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat(stat: &str) -> Option<(char, u32)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let _pgrp = fields.next()?;
    let sid = fields.next()?.parse().ok()?;
    Some((state, sid))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_stat_reads_state_and_session_past_a_tricky_name() {
        assert_eq!(
            parse_stat("4242 (a (b) c) S 1 4242 4242 34816 4242 4194560"),
            Some(('S', 4242))
        );
        assert_eq!(parse_stat("7 (sleep) Z 1 7 9 0"), Some(('Z', 9)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (x) R 0"), None);
    }

    #[test]
    fn dying_sessions_hand_back_a_kill_within_the_bound_once() {
        let bound = Duration::from_secs(5);
        let mut dying = DyingSessions::default();
        let d = Dying {
            leader: 99,
            killed_at: Instant::now(),
        };
        dying.record("p1", d, bound);
        assert_eq!(dying.take("p2", bound), None);
        assert_eq!(dying.take("p1", bound), Some(d));
        assert_eq!(dying.take("p1", bound), None, "taken once");
        dying.record(
            "old",
            Dying {
                leader: 98,
                killed_at: Instant::now() - Duration::from_secs(6),
            },
            bound,
        );
        assert_eq!(dying.take("old", bound), None, "past the bound");
    }

    fn policy(probe: impl Fn(u32) -> bool + Send + Sync + 'static, bound_ms: u64) -> RespawnPolicy {
        RespawnPolicy {
            probe: Arc::new(probe),
            bound: Duration::from_millis(bound_ms),
            poll: Duration::from_millis(5),
        }
    }

    #[test]
    fn wait_gone_returns_once_the_session_goes_or_at_the_bound() {
        let now = Instant::now();
        let d = Dying {
            leader: 5,
            killed_at: now,
        };
        assert!(wait_gone(d, &policy(|_| false, 1_000)));
        let polls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counted = Arc::clone(&polls);
        let p = policy(
            move |_| counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3,
            1_000,
        );
        assert!(wait_gone(d, &p));
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 4);
        let start = Instant::now();
        assert!(!wait_gone(d, &policy(|_| true, 60)));
        assert!(start.elapsed() < Duration::from_millis(1_000));
        assert!(
            now.elapsed() >= Duration::from_millis(60),
            "waited to the bound"
        );
    }

    /// Spawns `sh -c script` as the leader of a new session, as portable-pty spawns a pane.
    #[cfg(target_os = "linux")]
    fn session_leader(script: &str) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        // SAFETY: setsid is async-signal-safe and touches no parent state.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn sh")
    }

    #[cfg(target_os = "linux")]
    fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(target_os = "linux")]
    fn is_zombie(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| parse_stat(&s))
            .is_some_and(|(state, _)| state == 'Z')
    }

    /// A session lives while any process in it runs, the leader's zombie aside; it is gone
    /// once its last process exits.
    #[cfg(target_os = "linux")]
    #[test]
    fn session_probe_follows_every_process_of_the_session_on_real_processes() {
        // The leader exits at once and leaves a child in its session.
        let mut leader = session_leader("sleep 0.6 & exit 0");
        let pid = leader.id();
        eventually("the leader to exit", || is_zombie(pid));
        assert!(
            session_has_live_processes(pid),
            "the leader's child still runs in its session"
        );
        leader.wait().expect("reap the leader");
        assert!(
            session_has_live_processes(pid),
            "reaping the leader changes nothing"
        );
        eventually("the child to exit", || !session_has_live_processes(pid));

        // A leader that has exited but is not yet reaped (a zombie) is no live process.
        let mut lone = session_leader("exit 0");
        let pid = lone.id();
        eventually("the lone leader to exit", || is_zombie(pid));
        assert!(!session_has_live_processes(pid));
        lone.wait().expect("reap");
        assert!(!session_has_live_processes(pid));
        assert!(!session_has_live_processes(0) && !session_has_live_processes(1));
    }
}
