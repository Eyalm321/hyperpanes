# Dev-instance isolation: the single-owner `work.db` and the partly-isolated refusal

## The incident

On 2026-11-06 an "isolated" headless control server was booted from an agent pane with
`HYPERPANES_CONTROL_FILE` and `XDG_STATE_HOME` pointed at a scratch dir, but NOT
`XDG_DATA_HOME`. Its control file was private, so the discovery guard
(`docs/control-discovery-guard.md`) had nothing to object to. But the durable work queue
lives in the **data** dir (`$XDG_DATA_HOME/hyperpanes/work.db`, default
`~/.local/share/hyperpanes/work.db`), so the instance opened the LIVE queue. Its boot
recovery (`WorkQueue::recover_in_flight`) then requeued another project's claimed task. Boot
recovery requeues every `claimed` row on the premise that all workers died with this process.
That premise is only true for the process that owns the queue. The live worker's fencing token
went stale and the task ran twice.

Two guards now close this, plus the discovery guard that already existed:

| Layer | Stops | Where |
|---|---|---|
| Startup isolation check | a partly isolated boot (control file / state dir moved, data dir not) | `persistence::isolation`, called by the headless `app::run` and the GUI `main` |
| Discovery guard | a second instance overwriting a live `control.json` | `control::discovery_guard` (`run_server`) |
| `work.db` single-owner lock | any second process attaching a live `work.db`, however it got there | `control::work_lock`, called by `Shared::attach_durable_work_queue` |

## Which dir holds what

| Location | Linux default | Override | Holds |
|---|---|---|---|
| control file | `~/.local/state/hyperpanes/control.json` | `HYPERPANES_CONTROL_FILE` (wins over everything; **every pane inherits it pointing at the live file**) | port + master token |
| state dir | `~/.local/state/hyperpanes` | `XDG_STATE_HOME` | last session, pane-id map, resume prompts, device tokens |
| data dir | `~/.local/share/hyperpanes` | `XDG_DATA_HOME` | **`work.db`** (the queue every goal runs on), `projects.json`, AI memory, account registry |
| config dir | `~/.config/hyperpanes` | `XDG_CONFIG_HOME` | `control-settings.json` (bind address), speech/AI settings; also the GUI's single-instance + session-daemon key |

On Windows every dir is `%APPDATA%\hyperpanes` (override: `APPDATA`); on macOS it is
`~/Library/Application Support/hyperpanes` (override: `HOME`).

## Running an isolated dev instance (the supported way)

Set **all four** on the command line itself, under one scratch dir:

```sh
D=$(mktemp -d -p ~/.cache)   # not /tmp: tmpfs here
HYPERPANES_CONTROL_FILE=$D/state/hyperpanes/control.json \
  XDG_STATE_HOME=$D/state XDG_DATA_HOME=$D/data XDG_CONFIG_HOME=$D/config \
  cargo run --bin headless
```

Windows: `set APPDATA=C:\tmp\hp-dev` plus `set HYPERPANES_CONTROL_FILE=C:\tmp\hp-dev\hyperpanes\control.json`.
macOS: `HOME=$D HYPERPANES_CONTROL_FILE="$D/Library/Application Support/hyperpanes/control.json"`.

Reference scripts that already do this: `scripts/g3-recovery-demo.sh`, `scripts/talk-demo.sh`,
`rs/tools/run-unix-smoke.sh`. Plain `cargo test` is safe: tests use scratch paths and never
attach the real queue.

## Guard 1: the startup isolation check (`persistence::isolation`)

**Rule.** Refuse to start when the control file OR the state dir is diverted from its default
while the data dir still resolves to its default. "Default" means what `persistence::paths`
resolves with no override variable: on Linux `$HOME/.local/...` with `XDG_*_HOME` ignored; on
Windows the Roaming known folder with `APPDATA` ignored. The refusal names the diverted
location(s), the shared data dir, the env var to set (`XDG_DATA_HOME` / `APPDATA` / `HOME`) and
a fully isolated launch line. The headless daemon exits 1. The GUI exits 2, after an error dialog
for desktop launches that have no visible stderr.

**Override.** `HYPERPANES_ALLOW_SHARED_DATA=1` (also `true`/`yes`) starts anyway, for when
sharing the data dir is intended. It bypasses only this check, never the `work.db` lock.

**Deliberately asymmetric.** A diverted data dir with a default state dir is not refused here:
that instance shares the live control file, which the discovery guard refuses at
`run_server`.

**Scope.** Only the processes that serve a control plane run it: the headless daemon and the GUI.
The CLI subcommands (`worker`, `pair`, `devices`, `revoke`), the session daemon and
`--kill-daemon` are not gated, because they never open the data dir's queue. So an agent pane
running `hyperpanes worker` with odd env is unaffected.

**Edge to know.** A *primary* GUI launched from a desktop session that sets `XDG_STATE_HOME`
to a non-default dir but leaves `XDG_DATA_HOME` unset is partly isolated by this rule and will
be refused. Set `XDG_DATA_HOME` alongside it, or `HYPERPANES_ALLOW_SHARED_DATA=1`. On macOS
the baseline is `$HOME`, so a `HOME`-isolated instance must keep its control file inside its
own `~/Library/Application Support/hyperpanes`.

## Guard 2: the `work.db` single-owner lock (`control::work_lock`)

**Mechanism.** An exclusive OS file lock on `work.db.lock` beside the DB
(`std::fs::File::try_lock`: `flock(LOCK_EX|LOCK_NB)` on unix, `LockFileEx` on Windows).
`Shared::attach_durable_work_queue` takes it **before** opening the DB. Only the owner opens
`work.db` and runs `recover_in_flight`. A non-owner never opens the DB at all, so it does no
recovery and no writes, and its lease reaper only ever sweeps its own in-memory queue. Two
writers would be unsafe even without recovery: each `WorkQueue` caches the `seq` /
fencing-token counters at open, so two of them mint colliding ids and tokens.

**Owner record.** After acquiring, the owner writes `{pid, exe, version, controlFile,
startedAt}` into the lock file. A refused process logs it:

```
[work] NOT attaching the durable work queue …/work.db: another live hyperpanes instance owns it
(pid 4242, /usr/bin/hyperpanes v0.0.33, control file ~/.local/state/hyperpanes/control.json). …
```

The record is informational. The kernel lock is authoritative: it is released when the owner
dies, however it dies, so a crashed owner's stale record never blocks the next primary, and pid
reuse cannot fake an owner. On Windows the record is unreadable while locked, so the message
omits it. The lock file is never deleted: unlinking a locked file would let a newcomer lock a
fresh inode at the same path and become a second owner.

**Lifetime = the process.** An acquired lock is parked in a process-global registry until exit.
The GUI's in-process control-host restart (toggle Allow agent control off/on, which builds a
fresh `Shared` while the old one may linger in a handler) therefore re-attaches as the same
owner and recovers exactly as before. The fd is `O_CLOEXEC`, so the session daemon, pty
children and the restart relauncher never inherit it.

**Restart overlap.** A contended attach retries for 5s (`work_lock::ACQUIRE_WAIT`, the same
window as the discovery guard). An incoming primary that boots while the outgoing one is still
exiting waits for it, then takes over and runs boot recovery. If it is still held after the
window, the instance stays in-memory for its lifetime.

**Visible state.** `/health` carries an additive, stable `workQueue` field:
`"durable"` (owns `work.db`), `"refused"` (another live instance owns it; this one is
in-memory, no recovery or reaper on the DB) or `"memory"` (never attached / the DB would not
open). It has no paths or pids, because `/health` is unauthenticated.

**Fails open** on lock errors other than contention (a filesystem without lock support): it
logs and attaches without protection, because losing the primary's durable queue would be the
worse wedge. Guard 1 still covers dev instances there.

## The primary across GUI relaunch and daemon re-attach

- `restartApp {scope:"gui"|"full"}`: the relaunched GUI only gets past the single-instance
  gate once the old process has died (that lock is held until death, plus the relauncher's 2s
  sleep), and by then the kernel has dropped the old process's `work.db` lock. The new GUI
  acquires it immediately and runs boot recovery as before. If the old process lingers anyway,
  the 5s wait covers it.
- The session daemon never opens `work.db`. It owns the PTYs only, so daemon re-attach does
  not interact with the lock.
- Headless respawn: same as the GUI, through the restart-overlap wait.

Pre-existing and **not** changed here: with the session daemon on, worker panes *survive* a GUI
relaunch, yet the new owner's boot recovery still requeues their claims, because recovery's
"every worker died with us" premise predates the daemon. That is the same duplicate-run class as
this incident, but for the legitimate primary. It is tracked as a follow-up, not addressed by
this guard.

## Tests

- `rs/crates/core/tests/work_db_single_owner.rs` (real child processes running the production
  `attach_durable_work_queue()` under a scratch `HOME`/`XDG_*`/`APPDATA`):
  `second_opener_does_not_requeue_claimed_task` (the incident: red before the fix, green after;
  the live worker's ack still succeeds), `relaunch_overlap_waits_for_outgoing_owner` (red
  before, green after), `relaunched_primary_recovers_its_claimed_task` (a SIGKILLed owner's
  stale record stays behind but never blocks the next primary, which recovers the claim) and
  `same_process_reattach_stays_owner`.
- `rs/crates/core/tests/dev_instance_isolation.rs` (the real `headless` binary, Linux):
  `headless_refuses_a_partly_isolated_boot`, `headless_starts_partly_isolated_with_allow_shared_data`,
  `headless_starts_fully_isolated` and `second_headless_on_a_shared_data_dir_reports_refused`
  (`/health` → `durable` vs `refused`).
- Unit tests: `control::work_lock::tests::*` and `persistence::isolation::tests::*`.
