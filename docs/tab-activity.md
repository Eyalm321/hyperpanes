# Tab activity dots

A tab shows its animated "working" dots while at least one of its panes is **busy**. The same
value is `tabs[].status` in `GET /state`, and the MCP `list_panes` forwards it as `tabStatus`
next to each pane's `activity`.

## One source

Every reader calls `control::server::activity_for` for a pane:

- `/state` computes each pane's `activity` and its tab's `status` in one pass over one model
  (`ReadModel::state_for_scope_with_dims`), so the two always agree within a response;
- `list_panes` is a projection of one `/state` response;
- the GUI tab chip (`paneview::tab_status_for`) runs the same function over the same panes
  (`control_host::publish` mirrors every local tab pane into the read model) every 200 ms.

## What counts as busy

A pane is busy while it produced **genuine** output within the idle threshold (10 s), or while a
shell-integration marker says a command is running. Output that only redraws the same cells
does not count:

- each flushed output chunk passes through `session::activity::OutputGate`;
- a small chunk (≤ 1 KiB) that cannot scroll or append (no newline, no scroll escapes) and
  repeats one of the last two such chunks is **cosmetic**: a blinking indicator or cursor
  toggling between two states;
- everything else is genuine: new text, newlines, floods, and spinners (three or more frames,
  or a changing timer, never repeat one of their last two frames);
- the two-chunk history expires after 5 s without such a chunk.

The gate is byte-level and runs where the output stream is observed (the in-process pipeline,
or the daemon client), so it needs no VTE parse and no daemon change.

## Awaiting input

A pane whose screen shows a decision prompt reads `awaiting-input` (it never makes a tab
working). `control::output::detect_awaiting_input` recognises y/n prompts, "press enter", a
cursored menu on the last line, and Claude-style selection dialogs (`❯ 1. …` with another
numbered option and a question above or an "Esc to cancel" footer below) even when a
statusline or frame is drawn under them. A menu that is only quoted in the chat, above the
agent's visible input box, does not count.

The screen is classified lazily: the control server's activity ticker renders a pane's screen
once its genuine output has been quiet for 750 ms inside the busy window, and a
`mode:"screen"` read reuses its result. The verdict is cached under the output cursor of the
last genuine chunk, so a blink does not invalidate it and the next real change does.

## Never typing into a decision prompt

The app types text followed by Enter into a pane on its own in three places: the inbox nudge
(`routes::arm_inbox_nudge`), the resume queue (`deliver_queued_prompts`, which delivers both
`queue_prompt` and the speak-first `prompt` of a `restartPane` resume), and New-goal delivery
(`deliver_pending_goals`). Each one renders the screen right before typing and holds while
`control::output::blocks_typed_input` is true. That predicate covers permission and trust
dialogs, pickers, y/n questions and "press enter", but not a plain `❯` shell prompt or a
question in the chat, so a delivery is never held forever by an idle prompt. The Enter of a
nudge typed into a permission dialog would otherwise confirm its default "Yes".

`send_input` / `prompt_pane` type only when a caller asks, and never wait for the pane first.
Answering a dialog that way is intended.

## Checking it live

`scripts/tab-status-snapshot.sh [--json] [samples] [interval_s]` samples `GET /state` and
checks every tab's `status` against its panes' `activity` in the same response (read-only;
never prints the token).
