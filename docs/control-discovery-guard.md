# Control-file ownership guard (`control.json`)

## The failure mode this closes

`persistence::paths::resolve_control_file` resolves the discovery file as
`HYPERPANES_CONTROL_FILE` with precedence over the XDG-derived default (`~/.local/state/hyperpanes/control.json`).
Every spawned pane **inherits** that variable pointing at the LIVE file
(`session::spawn`), so a dev/test build launched from an agent pane — even one that
overrides `XDG_STATE_HOME` — still targets the live `control.json`. On start it
overwrote the live instance's port + token, and every agent reading the file began
failing with `fetch failed` / `unauthorized` / `no such pane`: symptoms that look like
a crashed agent or a pane-vanish race, with nothing pointing at the hijacked file
(tell-tale in the clobbered file: `"version": "0.0.0"`, the dev-build version, where a
release records the real one). `docs/agent-recovery.md` § "Named failure mode:
control-plane hijack by a dev/test instance" documents the recovery playbook; this
guard is the prevention.

The existing `single_instance` gate cannot catch this: it is deliberately
flavor-salted (`-headless`, see `core/src/app.rs`) so the GUI app and a headless
daemon never collide there — correct for argv hand-off, blind to file ownership.

## The guard (`core/src/control/discovery_guard.rs`)

Before `run_server` claims the file it reads it and checks the recorded pid. The file
is refused ONLY when that pid is alive, is not ours, and **verifiably looks like a
hyperpanes process** (comm/argv0 on Linux, `ps` comm elsewhere on unix, image path on
Windows — matching `hyperpanes` or the `headless` bin). Everything else claims
cleanly; the guard **fails open**, because a wrongly-refused legitimate launch would
be a worse wedge than the clobber it prevents:

- **live foreign hyperpanes pid → refuse startup**, before binding anything, with a
  message naming the live owner (pid / port / version), the copy-pasteable isolation
  env line, and the pid-reuse escape hatch. The refusal is retried for ~5s first, so
  an owner that is mid-exit (restart overlap) is claimed instead of refused. The
  headless daemon exits 1 with the message; the GUI host's detached server task also
  logs it to stderr.
- **pid dead (zombies count as dead), file missing/corrupt, or pid is ours**
  (in-process `ControlHost` restart) **→ claim cleanly.** Crash recovery needs no
  manual cleanup.
- **pid alive but some unrelated program (recycled pid), or identity unreadable →
  claim.** A reused pid can never brick startup forever.
- `remove_discovery` is owner-checked the same way: an instance only deletes a file
  recording its own pid, so a refused instance quitting cannot delete the live owner's
  file either.

`restartApp` (both scopes) is safe twice over: same-flavor restarts are serialized by
the `single_instance` flock (released only when the old process dies) plus the
relauncher's 2s sleep (`app::service_restart_request`), and the guard's retry window
covers any residual overlap. Exactly two code paths ever write discovery — the GUI's
`ControlHost` and the core headless `app::run` (`grep -rn "server::run_server"
rs/crates`); the session daemon (`--session-daemon`) and `hyperpanes worker` are
clients only, so no legitimate pair of processes can deadlock on the guard.

Known limit: two instances starting simultaneously against a not-yet-written file
both pass (last write wins) — the incident class is a dev build joining a long-lived
live instance, where the file always exists first.

## Running an isolated dev instance (the supported way)

```sh
D=$(mktemp -d -p ~/.cache)   # not /tmp: tmpfs
HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json \
  XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data XDG_CONFIG_HOME=$D/config \
  cargo run --bin headless
```

All four matter. `HYPERPANES_CONTROL_FILE` must be set explicitly because the inherited value
would otherwise win (that precedence is the whole failure mode). `XDG_STATE_HOME` isolates the
rest of the state dir. `XDG_DATA_HOME` isolates the durable work queue (`work.db`): leaving it
out was the second incident, a dev instance requeueing the live queue's claimed tasks.
`XDG_CONFIG_HOME` keeps the live `control-settings.json` (bind address) out. An unisolated dev
launch now fails fast with this guard's refusal instead of hijacking the live control plane,
and a partly isolated one (control file / state dir moved, data dir not) refuses to start, see
`docs/dev-instance-isolation.md`.
