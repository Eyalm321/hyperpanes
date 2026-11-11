# Pane messages and the prompt-box read

Three control-API additions. All are additive: existing callers see the same fields as before.

## `POST /panes/{id}/messages` — `nudge: false`

```json
{ "from": "<sender>", "body": "<text>", "nudge": false }
```

By default a message to a pane that opts into nudges (`meta.role` `goals-orch` / `spec`, or
`hp.nudge=on`) also types one `[hyperpanes] inbox: …` line into that pane once it is idle
(`control/nudge.rs`). A sender that already wakes the receiver its own way, for example by typing
its own prompt into the pane, can pass `nudge: false`. The message is still stored and readable
with `read_messages`, but nothing is typed. One send then costs the receiver one turn, not two.

Only an explicit JSON `false` turns the nudge off. Any other value, or no field, keeps it on.
The reply now carries `{ ok, seq, epoch, nudged }`.

### When the nudge is typed, and when it is dropped

- **Settle.** The nudge waits until no new message has arrived for 8 s, capped at 30 s after
  the first one. Relays a lead chains a few seconds apart therefore cost one nudge that names
  them all.
- **Never mid-turn, never lost.** It also waits for the pane to be idle, however long the turn
  runs. A pending message always ends in exactly one nudge once the pane is idle.
- **Read-aware.** A `GET /panes/{id}/messages` read drops every pending message it returned.
  MCP `read_messages` and the `messages` resource both make that read. A pane woken some other
  way that reads its mail before going idle is not nudged for it afterwards. Mail that arrives
  after the read is still nudged.
- **Limit: any reader counts.** The control API cannot tell who made the read, so a read of
  pane P's inbox by another pane also counts as P having read it.

## `GET /panes/{id}/messages` — `epoch` and `reset`

The inbox is in memory in the GUI process. Every app start, including a GUI-only restart
(`restartApp gui`), begins a new inbox, and `seq` starts again at 1. A reader that kept
`after: 48` from before the restart used to get an empty list while new mail sat at seq 1.

Each read now carries:

- `epoch`: identifies this inbox lifetime (ms timestamp of its creation). It is stable until the
  app restarts, then it changes. A reader can store it next to its cursor.
- `reset`: `true` when `after` is past the pane's `latestSeq`. Seqs only grow and the newest
  message is never evicted, so such a cursor can only come from an earlier lifetime. The reply
  then holds the pane's whole retained backlog, not a delta. Re-base the cursor on `latestSeq`.

The inbox is still not persisted across restarts. Messages posted before a restart are gone.

## Screen reads — `inputLine`

`GET /panes/{id}/output?mode=screen` (and MCP `read_pane {mode:"screen"}`) adds:

```json
"inputLine": { "text": "rerun it to see warm cache times", "kind": "suggestion" }
```

or `null` when the screen has no Claude-Code-style prompt box. The prompt box is the last row
whose first visible glyph is `❯` and whose row above is a `─` rule, plus its continuation rows
down to the next rule. A `❯` cursor in a selection menu has no rule above it, so it does not count.

`kind` is one of:

- `empty`: nothing in the box.
- `typed`: at least one visible character is drawn normally. Someone typed or pasted it.
- `suggestion`: every visible character is drawn faint (SGR 2). This is Claude Code's own
  suggested next prompt (ghost text) or a placeholder hint. **Nobody typed it.** Never relay,
  submit or act on it as input someone wrote.

The plain `output` text cannot tell these apart. Both render as `❯ <text>`. A watcher that reads
the box as text will report Claude's suggestions as unsent user input.

The classification is computed from the terminal grid's cell flags in the session daemon. A
running daemon from an older build answers without the field, so `inputLine` is `null` until the
daemon restarts. The wire change needs no protocol bump: the field is optional in both
directions.
