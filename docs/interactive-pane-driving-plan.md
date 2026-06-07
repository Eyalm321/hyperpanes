# Interactive pane driving — control-plane improvements

How to make the control API good at **driving and observing an interactive TUI agent** in a pane
(e.g. a live `claude` session), not just spawning/structuring panes. Builds on the control API
([`cli-multiwindow-mcp-plan.md`](cli-multiwindow-mcp-plan.md)) and the agent-orchestration stack
([`agent-orchestration-plan.md`](agent-orchestration-plan.md)); the MCP that drives it lives at
`C:\hyperpanes-mcp`.

**Status (2026-06-05): BUILT (Phases 1–3, + Phase 4 follow-ups P4a/P4b — see the last
section).** Born from the first live interactive
conversation with a real `claude` agent running in a pane (orchestrator drove it via
`send_input` + read-back). The chat worked end to end — the agent even used `whoami`/`list_panes`
to be self-aware — but three rough edges showed up, plus a handful of smaller lessons. This doc
captures the fixes, now implemented across both repos.

**What shipped:**
- **A1 `send_input({ submit })`** + **A2 `send_keys({ keys })`** — write path. App: `keysToBytes`
  pure core (`control-input.ts`, tested) feeding the `/panes/:id/input` route, which gained a
  `submit` (delayed bare-CR, `SUBMIT_DELAY_MS`) and a `keys[]` body. MCP: `send_input.submit`,
  new `send_keys` tool, shared `postInput` client path.
- **B1 `read_pane({ waitForIdle, settleMs, timeoutMs })`** + **B2 `since` cursor** — read path.
  App: `control-output.ts` pure cores (`waitDecision`/`nextPollDelay`/`sliceSince`, tested); the
  server tracks `lastOutputAt`/`outputBytes` per session (fed by `emitOutput` before the
  no-clients guard) and the `/output` route blocks on quiescence + serves deltas, always
  returning `cursor`. MCP: `read_pane` gained the params; client takes an options object.
- **C1 `read_pane({ mode: "screen" })`** — rendered cell grid via the renderer round-trip. App:
  `screen.ts` serializer (pure `serializeTerminal`/`trimScreenText`, tested) + a lightweight
  `paneScreens` registry the `Terminal` mounts into; a `readScreen` control command serializes the
  live xterm buffer; the server dispatches it (raw-replay fallback if unavailable). MCP: `mode`.
- **C2 `awaitingInput`** — `detectAwaitingInput` (pure, tested) on the rendered screen, surfaced on
  `mode:"screen"` reads. **D1 `prompt_pane`** — the one-call turn (MCP-only, composed on A1+B1+C1).
  **D2** — README "Driving an interactive TUI agent" (structured-bus vs TUI-scrape).

## What the live session exposed (gap analysis + root cause)

| # | Symptom in the live session | Root cause |
|---|---|---|
| 1 | Had to shell out to PowerShell (`Start-Sleep` + raw GET `/panes/:id/output`) to see replies | `read_pane` returns **instantly**; there is no "wait until the agent finishes, *then* read". The only missing piece was the *wait*, so I faked it with sleeps. |
| 2 | Every message took **two** `send_input` calls (text, then a separate Enter) | Text + `\n` in one write is treated as a **bracketed paste** by the TUI, so the newline lands *in* the input box instead of submitting. A submit must be a **distinct** keystroke. (`submitNewlines` already turns `\n`→`\r` on Windows — that's why typing works at all — but it can't separate the keystroke.) |
| 3 | Read-back output was **mangled**: collapsed spaces (`spunyouup`), spinner frames (`Kneading…`), box fragments, repeated status bars | `GET /panes/:id/output` serves the **raw pty byte stream** (`ControlDeps.readOutput`), and `?strip=1` strips only **SGR/color** codes — it does **not** replay cursor moves, line clears, or the alt-screen buffer. TUI in-place redraws linearize into overlapping garbage; horizontal spacing done via cursor-forward (not literal spaces) vanishes when stripped. |

Smaller lessons (4–8) are folded into the refinements below.

## Proposed primitives

Three legs — **write**, **read**, **capture fidelity** — plus refinements. Each notes the
**app** (`C:\hyperpanes`) and **MCP** (`C:\hyperpanes-mcp`) touch points. Keep the MCP zod in
lockstep (it's `.strict()`); gate every change with `typecheck && test && build` on both repos.

### A. Write path — submit in one call + named keys *(addresses #2, #6)*

**A1. `send_input({ …, submit?: boolean })`.** When `submit` is true, the app writes `data`,
waits a short beat (~30–50 ms, configurable), then writes a **bare `CR`** as a separate write
(outside any paste). Collapses the type→Enter dance into one call.
- *App:* extend the `/panes/:id/input` handler (and `control-input.ts`) to optionally append the
  delayed CR; reuse `submitNewlines`. The inter-write delay is what defeats the
  bracketed-paste-vs-submit ambiguity.
- *MCP:* `send_input` gains `submit?: boolean` (schema + tool).

**A2. `send_keys({ paneId, keys: string[] })`** — a named-key vocabulary that maps to the right
byte sequences: `enter`, `escape`, `tab`, `shift+tab`, `up`/`down`/`left`/`right`, `ctrl+c`,
`ctrl+d`, `backspace`, `pageup`/`pagedown`, `home`/`end`. Needed for menus and prompts (the
first-run **trust dialog** needed `enter`; cancelling needs `escape`/`ctrl+c`). Same triple-gate
as `send_input` (it *is* input).
- *App:* a small key→bytes table feeding the same pty-write path as `send_input`.
- *MCP:* new `send_keys` tool; `send_input`'s `submit` is sugar for `…, send_keys(["enter"])`.

### B. Read path — waitable, delta reads *(addresses #1, #4, #8)*

**B1. `read_pane({ …, waitForIdle?: boolean, settleMs?: number, timeoutMs?: number })`.** Blocks
server-side until the pane's output has been **quiet for `settleMs`** (default ~600 ms) or the
`timeoutMs` elapses, then returns. This is the primitive that removes every `Start-Sleep`.
- Use a **short settle window for interactive turns** — distinct from `useIdle`'s 10 s
  `idleAlertSeconds` (the glow threshold), which is far too slow for chat. The activity
  `busy→idle` flip is the coarse signal; `settleMs` is the fine one.
- *App:* the output buffer already receives pty data; track "last output at" per session and
  resolve the request when `now - lastOutput ≥ settleMs` (or on the activity flip).
- *MCP:* `read_pane` gains `waitForIdle`/`settleMs`/`timeoutMs`.

**B2. `read_pane({ …, since?: cursor })`** — return only output produced **since a cursor**
(monotonic byte/seq offset), not the whole scrollback. Avoids re-scraping the welcome box every
turn; cheaper, quieter. Returns the new cursor for the next call.

### C. Capture fidelity — rendered screen, not raw bytes *(addresses #3, #5)*

**C1. `read_pane({ …, mode: "screen" | "raw" })`.** `mode:"screen"` returns the **rendered cell
grid** — what's actually visible — instead of the raw pty stream. No overdraw, no spinner spam,
correct spacing, alt-screen aware.
- *App:* the faithful VT state already exists in the renderer's **xterm.js** instance. Serialize
  it on demand — `@xterm/addon-serialize`, or walk `terminal.buffer.active` with
  `line.translateToString(true)` — and return it through the **renderer command round-trip**
  (same correlationId path `setMeta`/`newPane` use to return a result). Option: keep a headless
  VT parser in `main` over the pty stream instead, but reusing xterm.js is the most faithful
  (handles reflow/width/wrapping).
- *MCP:* `read_pane` gains `mode` (default stays `raw` for back-comat; `screen` for TUIs).

**C2. `awaitingInput` heuristic.** Idle alone can't distinguish "agent finished" from "agent
blocked on a y/n / trust prompt". With the rendered screen (C1), match the last non-empty line
against known prompt patterns (`❯`, `(y/n)`, `Enter to confirm`, …) and surface
`awaitingInput: true` on the pane/read result so an orchestrator knows to answer rather than wait
forever. Pair with a launch flag / pre-trusted cwd to avoid the first-run trust gate entirely.

### D. Convenience + pattern *(addresses #6, #7)*

**D1. `prompt_pane({ paneId, text, timeoutMs })`** — type → `submit` → `waitForIdle` → return the
rendered reply delta. The whole per-turn dance in **one** call; ideal for driving TUI / "dumb"
agents. (Built purely on A1+B1+C1; no new app surface.)

**D2. Document "structured beats scraping" for MCP-capable agents.** Pane *had* the hyperpanes
MCP — the clean channel was its **inbox** (`send_message`/`send_to_parent`), not scraping its
TUI. But an interactive `claude` won't poll its inbox unprompted, so scraping was the only live
option. Two supported patterns: (a) run pane-agents with an **inbox-poll loop** ("listening
agent") and converse over the bus; (b) drive the TUI directly with A–C. Write this up so the
choice is explicit.

## Phased plan

- **Phase 1 — cheap, high-impact (the two that hurt most):** A1 (`submit`) + A2 (`send_keys`),
  and B1 (`waitForIdle`/`settleMs`) + B2 (`since`). Pure additive params/tools; no renderer work.
  Removes PowerShell and the double-call.
- **Phase 2 — capture fidelity:** C1 (`mode:"screen"` via xterm.js serialize through the command
  round-trip). The quality unlock; touches the renderer.
- **Phase 3 — polish:** C2 (`awaitingInput` + trust-gate avoidance), D1 (`prompt_pane`), D2 (docs
  + listening-agent pattern).

## Open questions / risks

- **Settle window vs streaming.** A chatty agent that pauses mid-answer could trip `settleMs`
  early. Tune the default; allow `waitForIdle` to also key off the activity flip, and let callers
  raise `settleMs` for slow models.
- **Renderer round-trip cost for `mode:"screen"`.** Serializing a large scrollback each read is
  heavier than slicing a byte buffer. Default to the **visible screen** (cap rows), offer full
  buffer explicitly; consider caching the serialized frame between reads via the `since` cursor.
- **`send_keys` is still input** — keep it behind the same triple gate (`allowInput` + bridge
  opt-in + `confirm`) and scope checks; named keys must not become a gate bypass.
- **Bracketed-paste delay is heuristic.** ~30–50 ms works for `claude`'s TUI; expose it as a
  param in case other TUIs differ.

## Definition of done

A second live chat with a pane-agent that needs **zero** PowerShell and **one** call per turn
(`prompt_pane`), with clean, readable transcripts (`mode:"screen"`), and the orchestrator
correctly detecting a blocked prompt via `awaitingInput`. Both repos green
(`typecheck && test && build`); MCP zod kept in lockstep.

## Gold-test results & Phase 4 follow-ups (2026-06-05)

Re-ran the live `claude`-in-a-pane chat on **app v0.1.4 + the new MCP**, driven entirely through
the new primitives — **PASSED**: `prompt_pane` did each turn in one call; `read_pane({mode:
"screen"})` round-tripped a box-drawing table, unicode/RTL/emoji, a code fence, and long lines
(the exact content the raw scrape used to mangle); `read_pane({waitForIdle})` removed every
PowerShell sleep; `send_input({submit:false})` + a bare `send_keys(["enter"])` submitted a line
("key landed ✅"); `send_keys(["shift+tab"])` cycled the permission mode. Two gaps surfaced:

- **P4a — `open_pane` arg quoting (real bug/limitation). BUILT & LIVE-VERIFIED (v0.1.6).** A `command` with complex quoted
  args (e.g. `claude --append-system-prompt "…long persona…"`) is **mangled**: the `command` string
  is handed to the pane's shell (cmd.exe), which splits on spaces, so the flag value truncates and
  stray tokens leak as positional args — in the gold test a stray `"are"` became claude's first
  prompt and the persona never applied. **Fix (shipped v0.1.5, Windows PATHEXT resolution added v0.1.6):** an **`args?: string[]`** form. With
  `command` set, a non-empty `args` runs `command` **directly** as the executable with that argv —
  no shell, no re-parse — so values with spaces/quotes survive intact; `command` alone keeps the
  shell path (back-compat). On Windows, `resolveWindowsCommand` walks PATH+PATHEXT so a bare name
  like `"claude"` resolves to `claude.exe` automatically (no full path required by caller). *App:*
  `resolveSpawn` + `resolveWindowsCommand` (`session.ts`, tested) chooses file+argv; threaded
  through `PaneSpec`/`Pane`/`HpSpawnOptions`, `addPane`, `groupFromSpec`/`specFromGroup`
  (round-trip), `Terminal.tsx` (prop + respawn dep + memo), the `newPane` control command
  (`strArray`/`argvOf` coercion), and surfaced on `/state` + `buildControlPayload`. *MCP:*
  `open_pane` schema + tool, `PaneSpecSchema`, `compile-cli` (flagged JSON-only/lossy), `list_panes`
  display. **Live verified 2026-06-05:** `open_pane({ command:"claude", args:["--append-system-prompt","You are Pane…"] })`
  → persona applied, reply signed with 🐾, no stray tokens.
- **P4b — `awaitingInput:true` is unverified live. BUILT (regression fixtures).** The gold test
  never hit a blocking y/n: the cwd **auto-trusted** at boot (no startup dialog) and tool calls
  **auto-ran**, so only the `false` (idle/working) path was exercised. Added **deterministic
  rendered-screen fixtures** to `control-output.test.ts` over the pure `detectAwaitingInput` core:
  the real **trust dialog** (footer "Enter to confirm …") and a cursored **menu selection** →
  `true` (the positive path, now locked in); the **idle `❯` prompt box** (caret inside the box, a
  status hint as the last line) and a **mid-turn working** screen → `false`. Confirms the chosen
  semantics: `awaitingInput` means "blocked on a decision", not "idle".
