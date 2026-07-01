# Hyperpanes Mobile — iOS/Android remote client

Status: **v1 implemented** on branch `feat/mobile-client` (host additions + Flutter app).
The mobile apps are pure *clients*: everything (ptys, agents, files, queues) runs on the
host hyperpanes; the phone streams, observes, and drives.

## 1. Why this architecture

The native app already exposes everything a remote client needs through the **control
API** (`rs/crates/core/src/control/`):

| Need | Existing surface |
|---|---|
| Session tree (windows→tabs→panes, labels, colors, AI subtitles, meta) | `GET /state` |
| Live output stream (raw pty bytes per pane) | WS `/events` → `output` frames |
| Agent liveness (working / awaiting-input / done / exited) | `activity` + `liveness` frames |
| Scrollback / attach snapshot | `GET /panes/{id}/output` (raw ANSI replay + byte `cursor`) |
| Typing / prompting / key chords | `POST /panes/{id}/input` (`data`+`submit`, `keys`) |
| Pane management | `POST /command` (`newPane`, `closePane`, `restartPane`, `renamePane`, `recolorPane`, `setLayout`, `focusPane`) |
| Auth | bearer token (master or scoped via `POST /tokens`) |

So the client is architecturally identical to the MCP server — but with a terminal
*emulator on the device*: we stream **raw pty bytes** and emulate/render locally
(xterm.dart), exactly like `tmux attach`: snapshot (replay buffer) + live tail. This
gives full styling/fidelity, local scrollback, and offline redraw at zero host cost.

### Terminal fidelity rule
The device must emulate at the **host pane's cols×rows** (a terminal stream is only
valid at the width it was produced for). The phone renders that grid and auto-fits the
font to screen width (pinch-zoom + pan for detail). v1 deliberately does **not** resize
the host pty from mobile — that would reflow the desktop.

### Gapless attach (why `cursor` on output frames)
Attach sequence: connect WS (start buffering frames) → `GET /output` (returns replay +
`cursor` C) → feed replay → apply buffered/live frames **only where `frame.cursor > C`**.
Without a cursor on WS frames the overlap between snapshot and stream double-prints.
Host change H3 adds it (additive; legacy clients ignore unknown fields).

## 2. Host-side additions (all additive, default-off)

- **H1 `bindAddress` + `port` in control-settings.json** — server today binds
  `127.0.0.1:0` (ephemeral). New optional fields: `bindAddress` (default `127.0.0.1`)
  and `port` (default `0` = ephemeral). Set `bindAddress` to the Tailscale IP (or
  `0.0.0.0`) + a fixed port for mobile. Missing/invalid values coerce to the old
  behaviour; `control.json` keeps its exact legacy fields and gains `bindAddress`.
- **H2 events URL** — `control.json.events` still advertises `127.0.0.1` (it is a
  *local* discovery file consumed by the MCP); the mobile app builds its own URLs from
  the pairing info.
- **H3 `cursor` on WS `output` frames** — monotonic byte cursor after the batch.
- **H4 `cols`/`rows` on `/state` panes** — from the session screen, so the device knows
  the grid to emulate.
- **H5 `hyperpanes pair`** — CLI subcommand: reads `control.json`, enumerates non-loopback
  IPs (Tailscale 100.64/10 preferred), prints `hp://<host>:<port>/#<token>` pairing URLs
  + a scannable terminal QR code.

### Security model
Token-bearer auth (existing), transport security by **network layer**: the recommended
deployment is Tailscale (WireGuard-encrypted, no open LAN ports). Binding `0.0.0.0` on
untrusted LANs is possible but the pair output warns about it. `allowInput` stays a
separate host-side switch. TLS termination is a non-goal for v1 (tailscale serve can
provide HTTPS if wanted).

## 3. Mobile app (Flutter, `mobile/hyperpanes_mobile/`)

**Why Flutter**: one codebase → iOS+Android; `xterm.dart` is a mature, fast
canvas-rendered terminal emulator; excellent perf headroom vs. RN/webview approaches;
Android builds on Linux, iOS builds on the Mac mini.

### Layers
```
lib/src/api/      pairing.dart, models.dart, control_client.dart, events.dart
lib/src/term/     pane_terminal.dart (seed+stream+dedupe), quick_keys.dart
lib/src/state/    app_state.dart (host session: state tree + event fan-in)
lib/src/ui/       connect, dashboard, terminal, composer, actions, settings
```

### Screens / UX (coding + Claude first)
- **Connect** — scan `hyperpanes pair` QR or paste URL / manual host+port+token; saved
  hosts (flutter_secure_storage for tokens).
- **Dashboard** — live tree of windows→tabs→panes; each pane card shows label, project
  color, **AI subtitle** (`ai.subtitle` meta), liveness chip (`working` amber pulse /
  `awaiting-input` red / `idle` green / `exited` gray). Chips update from `activity`/
  `liveness` frames only — the dashboard never renders output (cheap).
- **Terminal** — full-screen xterm view at host cols×rows, font auto-fit + pinch zoom,
  scrollback, selection/copy. Bottom: quick-keys bar (Esc ⇥ ⌃C ↑ ↓ ← → ⏎ / - | ~) +
  input field. All keys route through `/panes/{id}/input`.
- **Claude composer** — for panes whose command/meta says Claude (or toggled manually):
  a chat-style multiline prompt box (send = `input {data, submit:true}` with the CR-beat
  semantics the host already implements), quick replies when `awaiting-input`
  (1/2/3, y/n, Esc), Shift+Tab mode-cycle button, ⌃C stop.
- **Actions sheet** — rename, recolor, restart, close, focus-on-host, new pane in tab
  (project picker from `/projects`).
- **Notifications** — local notification when a pane flips to `awaiting-input` or
  `done` while the app is backgrounded (Android: connection kept by the OS as long as
  the process lives; iOS: fires on return/while active — true remote push is out of
  scope v1 and would need a relay).

### Performance strategy
- **One WS** per host, demuxed; frames for the open pane go straight to its emulator.
- **Dashboard is output-free**: liveness/activity/state frames only; `state` pings
  (coalesced ~100ms host-side) trigger a single `/state` refetch, debounced.
- **Background panes buffer nothing**: on open, re-seed from `/output` (replay buffer)
  instead of holding N live emulators. Only the visible terminal consumes output frames.
- xterm.dart writes are already batched per frame; the stream handler appends without
  per-byte work (cursor dedupe is O(1) per frame).
- Reconnect: exponential backoff + jitter; on reconnect the open pane re-seeds (cursor
  monotonicity makes this seamless).

### Tests
- Dart unit tests: pairing URL parse, event frame decode, seed/stream cursor dedupe,
  quick-key → control `keys` mapping, state-tree merge.
- Rust unit tests for H1–H5.

## 4. Build & release

- **Android** (on Linux/WSL/mac): `cd mobile/hyperpanes_mobile && flutter build apk --release`.
- **iOS** (on the Mac mini): `flutter build ipa` (needs Xcode + signing; for personal
  use, sideload via Xcode/AltStore).
- CI later: `flutter-action` workflow building the APK on tag, mirroring release.yml.

## 5. Non-goals v1 / follow-ups
- Remote pty resize / mobile-driven layouts (would fight the desktop).
- True push notifications (needs a relay service).
- TLS in the control server (Tailscale covers transport; `tailscale serve` for HTTPS).
- Prefs GUI fields for bindAddress/port (settings file + `pair` cover it; Slint prefs
  panel is a follow-up).
- Worker-queue dashboards on mobile (API exists; UI later).
