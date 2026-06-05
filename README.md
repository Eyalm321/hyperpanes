# Hyperpanes

> **Tiling terminal panes — named, color‑framed, tabbed, and tear‑off‑able — in one frameless window.**

<!-- Add a screenshot or GIF: drop the file at docs/screenshot.png and replace the block below with
     ![Hyperpanes](docs/screenshot.png) — a tiled, multi-pane layout shows the app off best. -->
<p align="center"><em>📸 Screenshot coming soon — see <code>docs/screenshot.png</code>.</em></p>

A desktop **terminal workspace**: tabbed windows that tile multiple live terminal panes, where each
pane is spawned with a **locked label** and its **own frame color**, arranged via **layout presets**.
Every tab is a self-contained workspace; panes and whole tabs can be **dragged between tabs and torn
off into separate windows**. Panes are created two ways — ad‑hoc through a **command palette** or the
**New pane** form, or in one shot from a declarative **`workspace.json`** file.

It's tmux's power (tiling, zoom) on a modern GPU renderer (xterm.js + real shells via node‑pty), plus
first‑class named, color‑framed, command‑driven panes and a browser‑like tab/window model. Frameless,
with its window controls built into an icon‑only top bar.

> [!NOTE]
> **Status: early days (v0.1.0).** Every feature documented below is implemented; some of the newer
> window/drag features have been verified statically and still want a thorough manual pass. Expect
> rough edges, and please file issues.

## Features

### Panes
- **Tiled panes** that stay mounted — switching layouts only restyles, so shells and scrollback
  always survive.
- **Locked labels** — name a pane; it never gets overwritten by the shell's title escape codes
  (the shell title shows only as a tooltip). Double‑click a label to rename; an optional subtitle
  rides along.
- **Per‑pane frame colors** — a palette + custom color picker (click the header dot).
- **Command panes** — launch a pane running any command (`npm run dev`, `tail -f log`, …) with live
  status, an exit‑code badge, and one‑click **Restart**.
- **Maximize vs. fullscreen** — `⤢` (`Alt+Z`) maximizes a pane to fill its window; `⛶` (`F11`) takes
  it to OS fullscreen with the top bar hidden (hold `Esc` to exit).
- **Per‑pane font zoom** — `Ctrl +` / `Ctrl -` / `Ctrl 0`, or `Ctrl + mouse‑wheel`, with a live
  zoom‑% toast. Each pane remembers its own size.
- **Idle glow for AI panes** — when an agent CLI (claude, aider, codex, gemini, …) goes quiet at its
  prompt, its frame does a soft, irregular firefly pulse so you notice it's waiting for you. Tunable
  threshold; off‑window panes glow even when focused.

### Layouts
- **Automatic** layout plus five presets: **Single, Columns, Rows, Grid, Main + Stack**.
  Automatic picks one for you by pane count (1 → single, 2–3 → columns, more → grid).
- **Draggable dividers** resize Columns, Rows, and the Main + Stack split.
- In **Single** layout the hidden panes appear as a **bottom taskbar** — click to switch,
  middle‑click to close, right‑click for the pane menu.
- Layout is **per tab**; set it from the top‑bar Layout menu, the command palette, or a tab's
  right‑click menu.

### Tabs & windows
- **Tabs are workspaces** — each tab has its own panes, layout, focus, zoom and split sizes, and
  keeps its shells running in the background.
- **Full tab lifecycle** — new, close, **reopen closed** (`Ctrl+Shift+T`), duplicate, rename
  (double‑click), reorder by dragging, cycle (`Ctrl+Tab` / `Ctrl+Shift+Tab`), plus **Close Others**
  and **Close Tabs to the Right**.
- **Multi‑window tear‑off** — drag a tab off the strip to pop it into its own window, or drag a
  **pane** out of its window to spin up a new one. Tabs and panes can be **dragged between existing
  windows** (Chrome‑style docking). "Move to New Window" / "Move to New Tab" are also in the menus.
  Live shells move with their pane — the pty stays alive across the move.
- **Drag & drop within a window** — drag a pane's header to another tab to move it, or onto a sibling
  pane to reorder/re‑slot it in the layout.

### Terminal
- **WebGL renderer** with an automatic DOM fallback; real shells via node‑pty.
- **Per‑pane search** (`Ctrl+F`).
- **Copy‑on‑select** (auto‑copies the selection, with a toast) and **right‑click paste**.
- **Clickable file paths** — paths in output are verified on disk, then **click to open** (in your
  editor or the OS default) and **Ctrl+click to copy** the resolved absolute path; `file:line:col`
  jumps are honored.
- **Per‑pane shell** override (`pwsh`, `cmd`, `/bin/zsh`, …) on top of a configurable default.

### Command palette
- **`Ctrl/Cmd+Shift+P`** — fuzzy runner for tabs (new/close/reopen), panes (new/shell/restart/close),
  zoom, layout switching, focus‑by‑pane, font zoom, preferences, and open/save workspace.

### Workspaces & sessions
- **Save/Open** the current tab to a `.json` file (top bar or palette).
- **Session restore** — the whole window (every tab + the active one) is auto‑saved and restored on
  next launch.
- **CLI launch** — open a `.json`, or describe panes inline with `-c`/`--label`/`--color`/… (see
  below).

### Customization (Preferences)
- **Keybindings** — every shortcut is rebindable with live conflict detection and per‑key / reset‑all.
- **Appearance** — frame‑color palette (**Muted / Vivid / Neon / Grayscale**, remapped by slot so a
  pane keeps its logical color), terminal color theme (**Dark / Black / Light / High contrast**),
  font family and default size, and toggles for the pane frame, color dot, and AI idle glow.
- **General** — default shell, focused‑pane font size, and clickable‑paths on/off with a custom
  editor command.

## Quick start

```bash
npm install        # node-pty ships N-API prebuilds → no native rebuild needed
npm run dev        # launch in development (Vite HMR)
```

### Build & package

```bash
npm run make:icon  # rasterize build/icon.svg → build/icon.{png,ico} (run once / when the art changes)
npm run build      # bundle main / preload / renderer to out/
npm run pack:win   # build + produce a Windows installer in release/ (electron-builder nsis)
```

The installer's app icon comes from `build/icon.ico`; the source art is `build/icon.svg`.

### Tests & checks

```bash
npm test           # vitest unit tests (layout math, navigation, workspace round-trip, DnD)
npm run typecheck  # tsc --noEmit
```

## Workspace files

A workspace is a JSON file describing the panes and layout. Relative `cwd`s resolve against the
file's own directory. See [`workspaces/example.json`](workspaces/example.json).

```json
{
  "name": "dev",
  "layout": "main-stack",
  "panes": [
    { "label": "server", "color": "#e5484d", "command": "npm run dev", "cwd": "." },
    { "label": "logs",   "color": "#f5a623", "command": "tail -f logs/app.log" },
    { "label": "db",     "color": "#30a46c", "command": "psql mydb" },
    { "label": "shell",  "color": "#3b82f6", "shell": "pwsh" }
  ]
}
```

Each pane may set its own `shell` (e.g. `pwsh`, `powershell`, `cmd`, `/bin/zsh`); omit it to use the
**Default shell** from Preferences, which itself falls back to the system shell (`COMSPEC` / `$SHELL`).
`layout` accepts `auto` · `single` · `columns` · `rows` · `grid` · `main-stack` (defaults to `auto`).

A saved file can also carry a **whole session** — multiple tabs via a `groups` array plus an `active`
index — which is exactly what the auto‑saved last session uses; a plain single‑tab file (above) still
loads fine.

Launch one directly:

```bash
hyperpanes ./workspaces/example.json
```

Or use **Open** / **Save** in the top‑bar menu (or the palette). The most recent session is remembered
and restored automatically on next launch.

### Launch from the command line

Skip the JSON file entirely and describe the panes inline. Each `-c` (or `--command`) opens a pane
running that command; `--label`/`--color` attach to the command before them.

```bash
hyperpanes -c "npm run dev" --label server --color "#e5484d" \
      -c "tail -f logs/app.log" --label logs \
      --layout main-stack
```

| Flag | Meaning |
| --- | --- |
| `-c`, `--command <cmd>` | Open a pane running `<cmd>` (repeatable). |
| `-l`, `--label <name>` | Label the most recent `-c` pane (defaults to the command's first word). |
| `--color <hex>` | Frame color for the most recent `-c` pane. |
| `--cwd <dir>` | Working directory for panes that don't set their own. |
| `--shell <shell>` | Shell for panes that don't set their own — e.g. `pwsh`, `powershell`, `cmd`, `/bin/zsh`. |
| `--layout <id>` | `auto` · `single` · `columns` · `rows` · `grid` · `main-stack`. |
| `--name <name>` | Workspace name shown in the tab. |

> [!TIP]
> Inline `-c` flags take precedence over a positional `.json` path. During development, pass args
> after a `--`: `npm run dev -- -c "npm run dev"`. On a packaged install, call the `hyperpanes`
> executable (its install folder is added to `PATH`, so `hyperpanes` works from any terminal).

## Keyboard shortcuts

All shortcuts below are **rebindable** in Preferences → Keybindings (except focus‑by‑number). `Ctrl`
means Ctrl on Windows/Linux and Cmd on macOS.

| Shortcut | Action |
| --- | --- |
| `Ctrl/Cmd+Shift+P` | Command palette |
| `Ctrl+T` | New tab |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | Next / previous tab |
| `Ctrl+Shift+T` | Reopen closed tab |
| `Alt+←/→/↑/↓` | Move focus to the adjacent pane |
| `Alt+1`…`Alt+9` | Focus pane by index (fixed) |
| `Alt+Z` | Maximize / restore the focused pane (within the window) |
| `F11` | Fullscreen the focused pane (hold `Esc` to exit) |
| `Ctrl+F` | Search within the focused pane |
| `Ctrl +` / `Ctrl -` / `Ctrl 0` | Font zoom in / out / reset (also `Ctrl`+mouse‑wheel) |

> [!NOTE]
> The palette uses `Ctrl/Cmd+Shift+P` rather than `Ctrl+K` on purpose — `Ctrl+K` is the shell's
> kill‑to‑end‑of‑line binding inside the terminal.

## Mouse & context menus

- **Tabs** — click to switch, double‑click to rename, `×`/middle‑click to close, `+` to add, drag to
  reorder or tear off. Right‑click for: New / Rename / Duplicate / Move to New Window / Close /
  Close Others / Close to the Right / Reopen / Layout.
- **Pane header** — drag to move the pane to another tab or out to a new window; the dot is the color
  picker. Right‑click for: New Pane / Rename / Change Color / Maximize / Fullscreen / Search /
  Restart / Copy / Paste / Select All / Clear / Move to New Tab / Move to Tab / Close.
- **Terminal body** — select to copy, right‑click to paste, `Ctrl`+wheel to zoom, click a path to
  open it.

## Architecture

- **Main** (`src/main`) — owns the OS side: `session.ts` wraps `node-pty` (output batched ~16 ms to
  cut IPC), `session-manager.ts` tracks live sessions, `workspace.ts` handles file I/O + CLI args,
  `window.ts` manages multiple windows and tab/pane hand‑off, `paths.ts` resolves & opens clickable
  paths, `ipc.ts` bridges to every renderer (session output is broadcast and filtered by uid, so a
  pty isn't tied to the window that spawned it — that's what lets a tab move between windows).
- **Preload** (`src/preload`) — a typed `window.hp` `contextBridge` API; `contextIsolation` on,
  `nodeIntegration` off.
- **Renderer** (`src/renderer`) — React + Zustand. State splits into `useWorkspace` (tabs/panes —
  a tab is a "group" of a flat ordered pane list + a layout descriptor), `useUI` (modals, drags,
  context menu, fullscreen), `useSettings` (persisted preferences) and `useIdle` (AI‑pane
  quiescence). `layout/presets.ts` maps a layout to an absolute rect per pane, so switching layouts
  only restyles and **never remounts terminals** (sessions and scrollback survive). Panes carry a
  `sessionUid` so they can detach/re‑attach when moved between tabs or windows without killing the pty.

## Tech stack

Electron · xterm.js (`@xterm/*`, WebGL) · node‑pty · React 18 · TypeScript · Zustand · electron‑vite ·
electron‑builder · Vitest.

## Acknowledgements

Pane‑tree resize math and the data‑batching session pattern are adapted from
[vercel/hyper](https://github.com/vercel/hyper).
