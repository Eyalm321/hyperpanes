# Per-pane talk (spoken assistant replies)

A per-pane **Talk** toggle: while it is on, that pane speaks every NEW Claude assistant reply
aloud through a local TTS backend. Off by default, per pane, persisted in the workspace
snapshot. The text comes from the conversation's own transcript (the pane→session marker
JSONL), never from scraping the terminal — so what is spoken is the assistant's actual reply,
normalized from markdown to listenable prose. All panes share ONE serialized speech queue with
pane-label prefixes, a global mute/stop, and an optional focused-pane-only mode.

**Status (2026-07-30): BUILT** — core pipeline + control API + GUI + MCP tools; verified
end-to-end against the headless binary (see "Verifying" below).

## Pipeline

```
pane (talk on)                       core, every 750ms (speech_service::run_ticker)
  └─ claude-sessions/<paneId>.json   session marker (session id + cwd + configDir)
       └─ <configDir>/projects/<encoded-cwd>/<sessionId>.jsonl
            └─ TranscriptTail        byte-cursor tail, starts at EOF (history is never spoken)
                 └─ extract_assistant_text   text blocks only; tool_use/tool_result dropped
                      └─ normalize_for_speech  markdown → prose (see below)
                           └─ SpeechEngine     ONE global queue, one utterance at a time
                                └─ backend      custom argv template | spd-say | espeak-ng | say
```

Every stage is core-owned (`rs/crates/core/src/speech/` + `control/speech_service.rs`), spawned
beside the control server — so the GUI app and the headless daemon (`--bin headless`) behave
identically, and the whole feature is testable without audio or a display.

- **Default-off costs nothing**: the engine thread spawns lazily on the first enabled pane, and
  the ticker's no-talkers fast path does no filesystem work at all.
- **Normalization** (`speech/normalize.rs`): fenced code blocks become the phrase
  "code block omitted."; inline code/emphasis/heading/list/table markup is stripped to its text;
  links keep their label (bare URLs become "link"); whitespace collapses; long replies truncate
  at a sentence boundary (~1200 chars) with a spoken "Truncated."
- **Serialization** (`speech/engine.rs`): one utterance plays at a time, FIFO (bounded queue,
  cap 64, drop-oldest). With two or more panes talking, each utterance is prefixed with its
  pane's label ("build. Tests are green."). `stop_all` kills the in-flight backend process
  immediately and discards the backlog; mute is the sticky variant.

## Control API

| Command | Effect |
| --- | --- |
| `{"type":"setTalk","paneId":…,"enabled":bool}` | toggle a pane's talk; result reports the resolved state + backend (+ a warning when no backend exists) |
| `{"type":"stopSpeech"}` | global one-shot: kill in-flight speech, drop the backlog |
| `{"type":"setSpeechMuted","muted":bool}` | global sticky mute (persisted) |
| `{"type":"setSpeechFocusedOnly","focusedOnly":bool}` | speak only the focused pane (persisted) |

The three global commands are pane-less (handled before window resolution, like `queuePrompt`).
`GET /state` reports `talk: true` on each talking pane and a top-level
`speech: {muted, focusedOnly, backend, speakingPane}` block. The read-model is the single
source of truth: GUI toggles publish into it each tick, control-originated changes reconcile
back onto the GUI (`control_host.rs`), so the two surfaces can never disagree.

## Settings — `<config dir>/speech.json`

A user setting (beside `ai-settings.json`), all fields optional:

```json
{
  "commandTemplate": ["/bin/sh", "/path/to/speak.sh", "{text}"],
  "muted": false,
  "focusedOnly": false
}
```

`commandTemplate` overrides backend auto-detection (spd-say → espeak-ng on Linux, `say` on
macOS): an argv array run per utterance WITHOUT a shell; `{text}` in any element is replaced
with the spoken text, and a template with no `{text}` receives it on stdin. This is also the
testing seam — point it at a script that appends to a file and the whole pipeline is
observable headless. **No backend at all** degrades to a one-time GUI notice + no-op; nothing
crashes and the toggle still round-trips.

## GUI

- Pane context menu → **Talk (speak replies)** checkable row; a speaker glyph shows in the
  pane header while talk is on.
- Command palette: **Speech: Stop Now / Mute / Only Focused Pane**.
- One-time toasts: enabling talk with no TTS backend, or while the control server (which hosts
  the speech service) is disabled in Preferences.
- Persistence: `PaneSpec.talk` in the workspace snapshot (`workspace/model.rs`), written by
  `to_session_file`/`to_workspace_file` and restored on relaunch.

## MCP tools (hyperpanes-mcp)

`set_talk {paneId, enabled}`, `set_speech {muted?, focusedOnly?}`, `stop_speech {}` — thin
wrappers over the commands above (`src/control-tools.ts`).

## Verifying

`scripts/talk-demo.sh` boots an ISOLATED headless instance (XDG overrides +
`HYPERPANES_CONTROL_FILE` pinned into the sandbox — the env var otherwise leaks in from a pane
and clobbers the live app's discovery file), fakes two panes' session markers + transcripts,
points `commandTemplate` at an evidence-file writer, and asserts: talk toggles are observable
in `/state`; interleaved appends to two transcripts come out as serialized, pane-labelled,
non-interleaved utterances of normalized prose; pre-existing history is never spoken; and
`stopSpeech` kills an in-flight utterance. Unit coverage lives in `speech/{normalize,engine,
tailer}.rs` and the `setTalk`/`setSpeech*`/`stopSpeech` dispatch tests.
