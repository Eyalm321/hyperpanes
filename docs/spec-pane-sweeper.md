# Spec-pane sweeper

A goal's spec agent is an interactive `claude` pane that idles forever once its goal is over.
The app sweeps these panes once a minute (`control::spec_sweep`) and closes the ones that are
provably done, leaving a handover record and a resume command.

A "spec pane" is a pane whose meta has `role: "spec"`.

## When a pane is closed

A pane is closed only when a close verdict has held on two sweeps at least 60 s apart (any sweep
that says keep resets the clock), by either trigger:

- **A. Goal done**: meta `hp.goalState` is `done`, or the pane's newest report to its parent
  starts with `done`; closed once idle for `doneCloseMinutes` (default 30).
- **B. Idle**: idle for `idleCloseHours` (default 3).

`idle since` is the newest of: last pty output, the peer status change (while the peer is idle),
and the last inbox message to or from the pane.

Meta `hp.idleClose`: `off` disables both triggers for that pane; a number overrides the idle
hours for trigger B.

### Never closed while any of these hold

- the pane is busy (output-quiet heuristic), or its Claude peer record is missing or not `idle`
- the screen shows a selection dialog (`Enter to select`, `↑/↓`, `Esc to cancel`, `Do you want`)
  or a typed draft in the prompt box (a dim ghost suggestion does not count)
- trigger B only: its last report was `needs-decision` or `blocked` and nothing has been sent to it since
- a task is `claimed` on its goal queue (`hp.queue`, else `<goal>` and `<project-basename>-<goal>`)
- a worker pane (role `worker`, `parent` = this pane) is running
- a live descendant process started more than 60 s after the claude process (Linux only; on
  other platforms this is unknown and the sweeper never closes)
- an unread inbox nudge is pending for it
- meta `hp.report` is set but the file is missing or empty

## Settings

`control-settings.json`, all optional:

```json
{ "specSweep": { "enabled": true, "idleCloseHours": 3, "doneCloseMinutes": 30, "dryRun": false } }
```

Missing or garbage values take the defaults. `HYPERPANES_SPEC_SWEEP=0|false|off` disables the
sweeper regardless. `dryRun` only logs, once per run of close verdicts, with `wouldClose: true`.

## What a close does

1. Writes `<state dir>/closed-spec-panes/<goal>-<first 8 of pane id>.md`: goal, reason, pane,
   parent, session id, config dir, cwd, the resume command, the last ~10 reports the pane sent its
   parent, and the last 60 screen lines. If this write fails the pane is kept.
2. Appends one JSON line to `<state dir>/spec-sweep.log`:
   `{ts, paneId, goal, parent, reason, sessionId, configDir, cwd, idleMin, rssKb, resume, record}`
   (plus `wouldClose` in dry-run), and prints a one-line summary to stderr.
3. Posts to the parent's inbox, from `hyperpanes`, without a nudge:
   `spec pane closed: goal <g> (<reason>, idle <n>m). Resume: <cmd>. Record: <path>`.
4. Closes the pane exactly as the `closePane` command does.

## Resuming

Run the `Resume:` command from the message or record:

```sh
cd '<cwd>' && CLAUDE_CONFIG_DIR='<dir>' claude --resume <session id>
```

(`CLAUDE_CONFIG_DIR` is omitted for the default account.) The command is only emitted when the
session id and cwd pass the same shell-safety checks restore uses.

## Related surfaces

- `GET /panes/{id}/open-consults` returns `{ok, paneId, open:[{from, seq, ts}]}`: `needs-decision`
  messages sent to the pane by spec panes that it has not answered since. Same auth and scope as
  reading its messages. A holder of an orchestrator refresh can use it to wait for open consults.
- `control::spec_sweep::snapshot` lists every spec pane with its age, idle time, RSS and current
  verdict, for a team panel.
