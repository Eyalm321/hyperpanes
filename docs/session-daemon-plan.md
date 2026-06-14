# Session daemon — true process survival across a GUI crash (#3)

**Goal.** When the hyperpanes GUI crashes (or is killed, or the user relaunches), the shells and
programs running in panes should **keep running** and be **re-attached** on the next launch — not
re-spawned. Today the PTYs are children of the GUI process, so a GUI crash SIGHUPs them and they die.
True survival requires a separate, long-lived **session daemon** that owns the PTYs; the GUI becomes a
**client** that attaches to it (the tmux / zellij-mux / abduco model).

This is the "pursue true survival" option from the crash-recovery work — see
[crash-recovery-feature](../) (the dialog + autosave already restore the *layout*; this restores the
*live processes*). It is a staged, multi-PR effort.

## Why this is tractable here

The current architecture already has the right seams:

1. **`SessionManager` is a small, clean API** (`crates/core/src/session_manager.rs`) — the GUI only
   ever touches sessions through it:
   - mutate: `create(opts)` / `create_with(opts, factory)`, `write(uid, data)`, `resize(uid, c, r)`,
     `kill(uid)`, `kill_all()`
   - query: `has(uid)`, `uids()`, `replay(uid) -> Option<String>`, `output_bytes(uid)`,
     `last_output_at(uid)`, `render_screen(uid) -> Option<String>`
   - events: an `UnboundedSender<SessionEvent>` (`Data{uid,data}`, `Cwd{uid,cwd}`, `Exit{uid,code}`)
   Reimplement this one type as a daemon client and **nothing else in the GUI changes**.

2. **Session lifetime is already decoupled from the on-screen grid.** Re-hosting a pane is
   "replay-into-a-fresh-grid, never a restart" — the GUI grid is fed purely by `Data` events + a
   `replay()` seed. A reconnecting *process* is the same operation as a reconnecting *window*.

3. **All the survivable state lives inside the session, core-side** — the 128 KB rolling `replay`
   buffer (`session/replay.rs`), the alacritty screen model behind `render_screen` (`session/screen.rs`,
   `shared.screen`), cwd, output counters. The daemon owns these; **no GUI types cross the boundary**.

4. **The transport already exists.** `single_instance/unix.rs` is a flock'd lock file + a Unix-domain
   socket under `$XDG_RUNTIME_DIR`, salted by the user-data dir, with a Windows named-pipe sibling.
   The daemon reuses this exact pattern for discovery + single-daemon-per-data-dir.

Because the daemon is entirely in `core` (no Slint), **most of it is headless-testable** with plain
`cargo test` — spawn daemon, create a session running `printf`, assert the `Data`/`Exit` events,
attach a second client, assert replay. Only the final visual re-attach (M2) needs the GUI.

## Target architecture

```
            ┌───────────────────────────── hyperpanes GUI (client) ─────────────────────────────┐
            │  SessionManager  ── same API ──>  DaemonClient                                      │
            │     (events)  <── UnboundedSender<SessionEvent> ── reader thread                    │
            └───────────────────────────────────┬───────────────────────────────────────────────┘
                                                 │  framed protocol over UDS / named pipe
                          (GUI crash cuts this line; everything below survives)
            ┌───────────────────────────────────┴──────────── hyperpanesd (daemon) ──────────────┐
            │  SessionRegistry { uid -> Session{ Pty(ConPTY/unix), Replay, Screen, cwd, counters }}│
            │  owns the PTY children · buffers output · multiplexes to attached clients            │
            └───────────────────────────────────────────────────────────────────────────────────┘
```

- The daemon is a **mode of the same binary** (`hyperpanes --session-daemon <salt>`), spawned
  **detached** by the first GUI launch (so it outlives the GUI). One daemon per data-dir (salted lock,
  reusing the single-instance machinery). It idle-exits after a grace period with **no sessions AND no
  clients**.
- The PTY children are children of the **daemon**, so a GUI crash never SIGHUPs them.

## Keeping `SessionManager`'s synchronous API non-blocking

The query methods return synchronously and some are called on the UI thread, so we must not do a
blocking socket round-trip on the hot path. Resolution per method:

| Method | Strategy |
| --- | --- |
| `has`, `uids` | **Client shadow** from `Create`/`Exit` events + one `ListSessions` on connect. |
| `output_bytes`, `last_output_at`, cwd | **Client shadow** — the client sees every `Data`/`Cwd` event; accumulate locally. |
| `replay(uid)` | **Client-side mirror buffer** fed by `Data` events → a local return, *no* round-trip. After a crash+reconnect the mirror is empty, so `Attach` returns the daemon's replay **once** to seed it. |
| `render_screen(uid)` | **Request/response** to the daemon. Off the hot path (control-API `read_pane{mode:"screen"}` only), so a bounded round-trip is fine. |
| `create/write/resize/kill/kill_all` | Fire-and-forget request (no response needed). |

Net: the GUI tick loop and rendering never block on the socket; only `render_screen` (rare) and the
one-time reconnect `Attach` do I/O round-trips.

## Wire protocol (`core::session::proto`)

Length-framed (`u32` LE length + body), `serde` (bincode for speed; JSON behind a debug flag for
inspectability). `SpawnOptions` already derives serde.

```
ClientMsg  = Hello{ proto_ver } | ListSessions | Attach{ uid } | Subscribe
           | Create(SpawnOptions) | Write{ uid, data } | Resize{ uid, cols, rows }
           | Kill{ uid } | KillAll | RenderScreen{ uid } | Ping
DaemonMsg  = Hello{ proto_ver, daemon_pid }
           | Sessions(Vec<SessionMeta>)              // reply to ListSessions
           | Replay{ uid, data }                     // reply to Attach
           | Screen{ uid, text: Option<String> }     // reply to RenderScreen
           | Event(SessionEvent)                      // Data / Cwd / Exit, streamed
           | Pong
SessionMeta = { uid, cwd, output_bytes, last_output_at, alive }
```

Versioned `proto_ver`; a mismatch makes the client kill + respawn the daemon (the daemon is ours, so
lock-step upgrades are fine — no third-party compat burden).

## Daemon lifecycle & discovery

- **Discovery/lock:** reuse `single_instance` — a flock'd `hyperpanesd-<salt>.lock` + a
  `hyperpanesd-<salt>.sock` under the runtime dir. Salt = the user-data dir (same as the GUI gate), so
  an isolated/dev instance gets its own daemon and never collides with the installed app.
- **Spawn:** on `SessionManager::new`, try to connect; if no daemon, `Command::new(current_exe)
  .arg("--session-daemon").arg(salt)` **detached** (`setsid` + null stdio on unix; `DETACHED_PROCESS`
  on Windows), then retry-connect with backoff.
- **Idle exit:** the daemon exits when it has 0 sessions and 0 clients for `GRACE` (e.g. 30 s) — so it
  doesn't linger forever, but survives the seconds between a GUI crash and relaunch.
- **Explicit control:** a `Shutdown` admin message + a `hyperpanes --kill-daemon` for clean teardown;
  the GUI's "Quit" can leave the daemon running (sessions persist) or shut it down per a preference.

## Reconnect / re-attach (the payoff — M2)

On GUI launch with a live daemon:
1. `ListSessions` → the set of surviving uids.
2. The restored workspace (`last-workspace.json`, written by the crash-recovery autosave) references
   panes by uid. For each restored pane whose uid is **still alive in the daemon**, `Attach{uid}` →
   seed the mirror/grid with the returned replay and resume the live stream → **the process survived**.
3. Panes whose uid is gone (the program had exited) fall back to today's behaviour: re-spawn in the
   saved cwd.
   → This requires the autosave snapshot to also record each pane's **session uid** (and ideally its
   spawn command, already noted as a gap in the crash-recovery work) so restore can match.

## Windows

- Transport: named pipe (`\\.\pipe\hyperpanesd-<salt>`), the sibling already sketched in
  `single_instance`.
- PTY: `portable-pty`'s ConPTY in the daemon; the bundled ConPTY redistributable ships next to the
  exe and the daemon is the same exe, so it's found.
- Detach: `CREATE_NO_WINDOW | DETACHED_PROCESS`.

## Security

Socket/pipe is filesystem-scoped to the user (UDS mode `0600` in a `0700` runtime dir; pipe ACL to the
user SID). No network surface. Same trust boundary as the existing single-instance socket.

## Staged milestones (fan-out-friendly — each is an independent PR)

- **M0 — protocol + daemon core (headless).** `proto.rs` (msgs + framing, unit-tested); move the
  current `SessionManager` internals into a `SessionRegistry`; `--session-daemon` mode that listens,
  owns the registry, handles create/write/resize/kill, streams events, serves replay/screen.
  Headless integration test: client lib drives the daemon over a temp socket.
- **M1 — `DaemonSessionManager` behind `HYPERPANES_SESSION_DAEMON=1`.** API-compatible client with the
  shadow-state + mirror-buffer scheme above; daemon spawn/discovery/connect; reader thread → the GUI's
  existing `SessionEvent` channel. In-process path stays the default.
- **M2 — reconnect & re-attach.** Record session uid (+ spawn command) in the autosave snapshot; on
  launch, attach surviving sessions and replay into fresh grids. **The crash-survival demo.**
- **M3 — lifecycle hardening.** idle-exit, daemon crash/restart, proto-version handshake, Windows named
  pipes, `--kill-daemon`, quit-vs-keep-alive preference, socket perms.
- **M4 — default on.** Flip `HYPERPANES_SESSION_DAEMON` to default; keep the in-process path as a
  `--no-daemon` fallback (headless/CI, or daemon-spawn failure).

## Risks / open questions

- **Latency:** a local UDS hop per keystroke/output chunk. Output is already batched (~16 ms,
  `session/batcher.rs`); input is tiny. Expected negligible, but M1 should bench keystroke→echo vs the
  in-process path.
- **Double screen model:** the daemon runs the core alacritty screen (for `render_screen`); the widget
  runs its own display grid. Already true today (two consumers of the same `Data`), so no change — just
  confirm the daemon is the one feeding `render_screen`.
- **Orphan daemons:** the idle-exit + salted single-lock prevent accumulation; `--kill-daemon` + a
  startup "reap stale lock/socket" pass (already in `single_instance`) cover crashes.
- **uid stability:** uids must be stable across GUI runs for re-attach. They're process-global counters
  today (`state.rs` PANE_UID) — for the daemon they must be **daemon-assigned** (the daemon is the
  source of truth across GUI restarts). M1 moves uid minting to the daemon.

## What's testable without the GUI

M0 + M1's client are pure `core` — full `cargo test` coverage of protocol round-trips, registry
behaviour, shadow-state correctness, and a real daemon+client loopback (spawn a session running
`printf hi`, assert `Data{"hi"}` + `Exit{0}`, attach a 2nd client, assert replay). Only M2's visual
re-attach and the end-to-end "kill the GUI, relaunch, process still running" demo need a live GUI —
which is the user's manual pass.

## Fan-out execution

The pipeline is mostly sequential, so launch by wave. Each milestone = one branch + one PR;
build in a worktree, verify, commit incrementally, hand back the branch for review.

| Track | Branch | Depends on | Verify (headless) |
| --- | --- | --- | --- |
| **M0** daemon core | `daemon/m0-core` | — | `cargo test -p hyperpanes-core` incl. a daemon+client loopback test |
| **Prep** snapshot uid+cmd | `daemon/prep-snapshot` | — | `cargo test` (core + app); round-trip test for the new `PaneSpec` fields |
| **M1** client SessionManager | `daemon/m1-client` | M0 | core tests + keystroke→echo bench vs in-process |
| **M2** reconnect/re-attach | `daemon/m2-reattach` | M1, Prep | GUI manual: kill GUI, relaunch, process still alive |
| **M3** lifecycle hardening | `daemon/m3-lifecycle` | M1 | idle-exit + reconnect tests; Windows pipe |
| **M4** default-on | `daemon/m4-default` | M2, M3 | full suites; `--no-daemon` fallback |

- **Wave 1 (parallel, now):** M0 + Prep — no shared files (M0: `session_manager.rs`, new
  `session/proto.rs`, `main.rs`; Prep: `state.rs::to_session_file`, `workspace/model.rs::PaneSpec`).
- **Wave 2:** M1 (after M0). **Wave 3:** M2 (after M1 + Prep) ‖ M3 (after M1). **Wave 4:** M4.
- Build crates via `--manifest-path` (app + terminal-widget are non-member crates); commit
  incrementally; never run the GUI against prod.
```
