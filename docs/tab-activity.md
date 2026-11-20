# Tab activity dots and the ASK state

A tab chip shows one of three states, in this order of precedence:

1. **ASK** (an amber badge; `"asking"`): a pane in the tab waits on a decision prompt, such
   as a permission or confirm dialog, a picker, or a y/n question. It needs the owner.
2. **Working** (animated dots; `"working"`): a pane in the tab is busy.
3. **IDLE** (`"idle"`): neither.

The same value is `tabs[].status` in `GET /state`, and the MCP `list_panes` forwards it as
`tabStatus` next to each pane's `activity`. `"asking"` is additive. A client that only knows
`"working"` and `"idle"` should derive a status from the panes for any other value; the
hyperpanes-mcp `tabStatusOf` already does. An asking pane's legacy `activity` reads `"idle"`.

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

Known misses, accepted on purpose (each one reads idle, never busy, after the 10 s threshold):

- a two-frame spinner with no timer next to it (indistinguishable from a blink);
- a progress line redrawn with identical text;
- a blink interleaved with another periodic cosmetic chunk, so the cycle is longer than two
  frames: that reads busy, the one miss in the other direction.

## Awaiting input and asking

A pane whose screen waits for input reads `awaiting-input` (it never makes a tab working).
`control::output::detect_awaiting_input` recognises y/n prompts, "press enter", a cursored menu
on the last line, and Claude-style selection dialogs (`❯ 1. …` with another numbered option and
a question above or an "Esc to cancel" footer below) even when a statusline or frame is drawn
under them. A menu that is only quoted in the chat, above the
agent's visible input box, does not count.

One render of the screen gives a verdict. `awaiting` comes from `detect_awaiting_input` and
drives `AwaitingInput`. `asking` comes from `control::output::decision_dialog` and drives ASK,
with the dialog's kind (permission, folder trust, or another choice) and its question row. The
verdict is cached under the output cursor of the last genuine chunk, so a blink does not
invalidate it and the next real change does.

### What reads ASK

ASK is stricter than the typing guard (`blocks_typed_input`, below). A screen that never changes
keeps its verdict, so a false ASK would read ASK, and alert, forever. The guard may hold on a
doubtful screen; ASK may not fire on one. Only a cursored selection dialog counts:

- a `❯ N.` option in a block of at least two numbered options, with a question above it or a
  key-hint footer ("Esc to cancel", "Enter to confirm") below it;
- at the bottom of the screen: under the options only their own wrapped text, blank rows,
  rules, key hints and at most three other rows (a statusline, a mode line) may follow, and
  none of them may look like a shell or pager prompt. A dialog printed by `cat` or `grep` in a
  shell has the prompt under it;
- not quoted in the chat: an agent input box (a `❯` row under a rule) below it rules it out;
- a side panel drawn beside the chat (a `│` column most rows share) is cut off first, so its
  text neither fakes a dialog nor hides one.

A bare y/n, "press enter" or "do you want" line never reads ASK: a log tail can end with one,
and the rendered text cannot tell it from a live prompt. It still reads `awaiting-input` and
still holds typed input.

### Ask episodes

A pane is in an **ask episode** from its first asking verdict until a verdict says the dialog
is gone, which ends it at once. A verdict taken at older content still counts while it is
recent: a dialog with a ticking element (a statusline clock, a spinner elsewhere on screen)
never holds still, so its current content is never classified. With no current or recent
verdict at all, the episode ends after 10 s. While a pane is in an episode, `/state` adds two
fields to it, both omitted otherwise:

- `askingSince`: epoch ms, when the prompt appeared;
- `question`: the dialog's question row, when one was found.

An episode starting or ending pings `state` on `/events`.

### The screen classifier

`control::classifier::run_screen_classifier` renders screens and steps the episodes. The app
starts it at launch, whether or not the control server is enabled. It is bounded:

- a pane is rendered only when its content cursor moved since its last verdict, so an
  unchanged screen is never rendered again (a blink does not move the cursor);
- once its genuine output has been quiet for 750 ms, or every 1.5 s while it never goes quiet;
- at most 4 renders per 500 ms tick, served round-robin by pane;
- a failed render is retried after 10 s, or sooner if the content changes.

A `mode:"screen"` read also refreshes the verdict.

### Needs-input alert

Once a pane has been asking for 60 s, the app raises one desktop notification for that episode.
Its title is "`<tab>` needs input" and its body is "`<pane label>` is waiting on a permission
prompt" (or "a folder trust prompt", "a choice"). The body never carries text from the screen:
a notification can show on the lock screen and stays in the notification history. Panes that
come due together share one notification ("3 panes need input", one line per pane). The minute
also counts from when the app first saw the episode, so after a restart a pane that has sat on
a dialog for hours alerts a minute later, not at once. Notifications go through
`org.freedesktop.Notifications` on Linux, one at a time from one worker thread, each given up
on after 5 s; elsewhere they do nothing. A new episode alerts again. Preferences → "Notify when
a tab needs input" turns it off for that window's panes (setting `askNotify`, on by default).

## Never typing into a decision prompt

The app types text followed by Enter into a pane on its own in three places: the inbox nudge
(`routes::arm_inbox_nudge`), the resume queue (`deliver_queued_prompts`, which delivers both
`queue_prompt` and the speak-first `prompt` of a `restartPane` resume), and New-goal delivery
(`deliver_pending_goals`). Each one renders the screen right before typing and holds while
`control::output::blocks_typed_input` is true. That predicate covers permission and trust
dialogs, pickers, y/n questions and "press enter", but not a plain `❯` shell prompt, a
question in the chat, or a numbered list in the chat or a shell listing: an option form counts
only when the `❯` cursor sits on a numbered option in a block of at least two, with no agent
input box below it. So a delivery is never held forever by an idle prompt. The Enter of a
nudge typed into a permission dialog would otherwise confirm its default "Yes".

`send_input` / `prompt_pane` type only when a caller asks, and never wait for the pane first.
Answering a dialog that way is intended.

## Checking it live

`scripts/tab-status-snapshot.sh [--json] [samples] [interval_s]` samples `GET /state` and
checks every tab's `status` against its panes' `activity` in the same response (read-only;
never prints the token).
