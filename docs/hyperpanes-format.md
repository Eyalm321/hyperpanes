# The `.hyperpanes` workspace format — design & options

**Status:** design doc / decision aid for the maintainer. **No code is changed by this
document.** It (1) documents the *current* workspace-file schema and how it is opened and
saved, (2) lays out three options for a dedicated `.hyperpanes` extension with tradeoffs and
a recommendation, (3) gives the Windows file-association snippet so a double-clicked
workspace opens in the app, and (4) lists the exact implementation touch-points for whichever
option is chosen.

The app today round-trips workspaces as plain **JSON**. There is no dedicated extension, no
magic header, and no schema-version field. Everything below is grounded in the existing port:

- schema: [`rs/crates/core/src/workspace/model.rs`](../rs/crates/core/src/workspace/model.rs)
- read/write/normalise: [`rs/crates/core/src/workspace/io.rs`](../rs/crates/core/src/workspace/io.rs)
- launch resolution: [`rs/crates/core/src/workspace/launch.rs`](../rs/crates/core/src/workspace/launch.rs)
- CLI positional capture: [`rs/crates/core/src/cli/parse.rs`](../rs/crates/core/src/cli/parse.rs)
- installer: [`rs/packaging/installer.nsi`](../rs/packaging/installer.nsi)

---

## 1. Current schema

The on-disk file is a single JSON object deserialized into `WorkspaceFile`. Panes can be
described at **any** of three nesting levels — top-level `panes`, a list of `groups` (tabs),
or a list of `windows` (each with its own tabs) — and the loader normalises whichever is
present into a flat window list (precedence `windows` → `groups` → `panes`, see
`windows_of` in `io.rs`).

### `WorkspaceFile` (top level)

| JSON field | Rust type | Notes |
|---|---|---|
| `name`    | `string?`           | workspace name; also the default window/tab title |
| `layout`  | `string?`           | layout id for the legacy single-tab shape (e.g. `main-stack`, `grid`, `columns`) |
| `panes`   | `PaneSpec[]?`       | top-level panes → one window, one tab |
| `groups`  | `GroupSpec[]?`      | tabs → one window of tabs |
| `active`  | `number?` (u32)     | active tab index when using `groups` |
| `windows` | `WindowSpec[]?`     | full multi-window shape |
| `tabDock` | `string?`           | tab bar side for the single window synthesized from `groups`/`panes`; same values as `WindowSpec.tabDock` |

### `WindowSpec`

| JSON field | Rust type | Notes |
|---|---|---|
| `title`  | `string?`        | window title |
| `active` | `number?` (u32)  | active tab index |
| `bounds` | `WindowBounds?`  | saved OS-window geometry |
| `groups` | `GroupSpec[]`    | tabs (required array; a window with none is dropped on load) |
| `tabDock` | `string?`       | where the tab bar docks: `"left"` \| `"right"`. Absent = top (the default and never written); any unknown value is treated as top. See `docs/tab-bar.md` |

### `WindowBounds`

| JSON field | Rust type | Notes |
|---|---|---|
| `x`, `y`            | `number?` (i64) | top-left position (may be negative) |
| `width`, `height`  | `number?` (i64) | size |
| `maximized`        | `bool?`         | restore maximized |
| `fullscreen`       | `bool?`         | restore OS fullscreen |

### `GroupSpec` (a tab)

| JSON field | Rust type | Notes |
|---|---|---|
| `title`        | `string?`       | tab title |
| `color`        | `string?`       | manual tab colour `#rrggbb` |
| `layout`       | `string?`       | tab layout id |
| `panes`        | `PaneSpec[]`    | the tab's panes (required array) |
| `sizes`        | `number[]?`     | per-slot split fractions (sum → 1; length matches `panes`) |
| `mainFraction` | `number?` (f64) | main-stack split fraction (0 < f < 1) |
| `focused`      | `number?` (u32) | index of the focused pane (default 0) |
| `zoomed`       | `number?` (u32) | index of the maximized pane (default: none) |
| `paneGroup`    | `PaneGroupSpec?` | present when the tab is a *pane group* (see below); omitted otherwise |

### `PaneGroupSpec` (a tab marked as a group)

| JSON field | Rust type | Notes |
|---|---|---|
| `project` | `string?` | project id the group belongs to |
| `color`   | `string?` | `#rrggbb` stripe/tint colour |

Both fields are optional and omitted when unset. Naming caveat: a **pane group** is a tab the
user created by multi-selecting panes and grouping them (it carries `paneGroup`), whereas
`GroupSpec` is the file's name for *any* tab. Older versions ignore `paneGroup` and load an
ordinary tab, and unknown fields inside it are tolerated.

### `PaneSpec`

| JSON field | Rust type | Notes |
|---|---|---|
| `label`    | `string?`              | pane label (defaults to the command's first token) |
| `subtitle` | `string?`              | the header's second line; written by the session snapshot so a descriptive subtitle survives a restart (absent in older files) |
| `color`    | `string?`              | accent color, e.g. `#e5484d` |
| `command`  | `string?`              | shell command line to run |
| `args`     | `string[]?`            | literal argv for a direct (no-shell) spawn with `command` |
| `cwd`      | `string?`              | working directory (relative cwds resolved on load — see below) |
| `shell`    | `string?`              | shell override (e.g. `pwsh`) |
| `fontSize` | `number?` (u32)        | per-pane font size |
| `meta`     | `{string:string}?`     | free-form per-pane metadata (sorted keys; used by agent orchestration / ambient AI) |

### Serialization rules (parity contract — `model.rs`)

These are guaranteed by the serde model and exercised by its round-trip tests:

- **camelCase** JSON field names (`fontSize`, `mainFraction`) via `#[serde(rename_all = "camelCase")]`.
- **Unset optionals are omitted, never written as `null`** (`skip_serializing_if = "Option::is_none"`); downstream is strict.
- **Field/`meta`-key order is canonical** — declaration order for fields, sorted keys for `meta` (it's a `BTreeMap`) — so a canonically-ordered file round-trips **byte-identically** through 2-space pretty printing (the same as `JSON.stringify(x, null, 2)`).

### Example

```json
{
  "name": "dev",
  "layout": "main-stack",
  "panes": [
    {
      "label": "server",
      "color": "#e5484d",
      "command": "npm run dev",
      "cwd": "/work",
      "shell": "pwsh",
      "fontSize": 14
    },
    { "command": "tail -f log" }
  ]
}
```

## 2. How a workspace is opened and saved today

**Open (launch / CLI).** `launch.rs` resolves what to load, in precedence order:

1. inline `-c …` flags assembled into a `WorkspaceFile` (`parse_cli` → `ParsedCli.workspace`);
2. a **positional `.json` path** (`ParsedCli.json_path` → `io::read_workspace`);
3. the last session at `last-workspace.json` (only in `resolve_launch_workspace`, not in the CLI-only `resolve_cli_workspace`).

The positional path is captured in `parse.rs` only when the argument **(case-insensitively) ends in `.json` *and* the file exists**, then resolved to an absolute path (`parse.rs` lines ~319-326). `read_workspace` reads the file, `serde_json::from_str`s it into `WorkspaceFile`, rejects a contentless file (`has_panes`), and resolves relative pane `cwd`s against the file's own directory.

**Open (GUI / double-click + drag).** The native GUI seeds from argv at startup (`-c …` or a `.json` positional → `load_workspace`); a bare `hyperpanes` with no args stays an empty shell pane (`resolve_cli_workspace` intentionally skips the last-session fallback). So a double-clicked file reaches the app as `argv[1]` and flows through the same positional-path capture.

**Save.** `io::write_workspace` serializes pretty (2-space) and writes the file; `windows_of` normalises any in-memory `WorkspaceFile` into the flat window list the launcher seeds from. Session auto-save targets `last-workspace.json`.

**Takeaway:** the *only* thing tying a file to the app is the **`.json` suffix check in `parse.rs`** (and the equivalent check in the GUI argv bootstrap). There is no content sniffing, no magic header, and no version field. Any `.hyperpanes` story has to start there.

---

## 3. Options for a dedicated `.hyperpanes` extension

### (a) Rename-only alias — accept `.hyperpanes` as JSON

Treat `.hyperpanes` as a second accepted suffix for the *exact same* JSON payload. No schema
change: the file is still a `WorkspaceFile` object; only the extension allow-list widens.

- **Pros:** smallest possible change (one extension check in `parse.rs` + the GUI bootstrap, plus the installer association); existing `.json` files keep working; nothing to migrate.
- **Cons:** no version marker, so a future breaking schema change still can't be detected or migrated cleanly; the extension is purely cosmetic; a `.hyperpanes` file and a `.json` file are indistinguishable by content (no magic header), so misnamed files can't be diagnosed.

### (b) Versioned container — **RECOMMENDED**

Wrap the existing payload in a thin envelope with a self-identifying header and a schema version:

```json
{
  "format": "hyperpanes",
  "version": 1,
  "workspace": {
    "name": "dev",
    "panes": [ { "command": "npm run dev" } ]
  }
}
```

(Or inline the payload fields alongside `format`/`version` — a flattened envelope — if you
prefer fewer levels of nesting. Either way the discriminators are `format` + `version`.)

- **Pros:** a **magic header** (`"format": "hyperpanes"`) lets the loader positively identify a workspace and reject look-alikes with a clear error; the **`version`** integer is the hook for real migrations (load v1, upgrade in memory, save v2) without guessing; still plain UTF-8 JSON — hand-editable, diff-friendly, copy-pasteable; can be introduced **backward-compatibly** by having the reader accept *both* a bare `WorkspaceFile` (legacy `.json`, treated as "version 0") and the wrapped form, and having the writer emit the wrapped form for `.hyperpanes`.
- **Cons:** slightly more code than (a) — a small envelope type plus a branch in the reader/writer; two accepted on-disk shapes during the compatibility window; the byte-identical round-trip contract has to be re-stated for the envelope (trivial, since it's still serde + 2-space pretty).

### (c) Binary capsule — **REJECTED**

A packed binary file (e.g. length-prefixed header + bincode/MessagePack/compressed blob).

- **Pros:** marginally smaller/faster to parse at very large sizes.
- **Cons:** **not human-editable**, not diffable, not pasteable into an issue or a chat; needs a custom tool to inspect; throws away the entire benefit of the current JSON model for files that are, in practice, tiny. Overkill for a few KB of workspace description. **Do not pursue.**

### Comparison

| | (a) rename alias | (b) versioned container ✅ | (c) binary capsule ❌ |
|---|---|---|---|
| Human-editable / diffable | yes | yes | no |
| Magic header (identifies file) | no | **yes** | yes |
| Schema migrations | no | **yes** (`version`) | yes |
| Backward-compatible with `.json` | yes (same payload) | yes (reader accepts both) | no |
| Implementation cost | minimal | small | large |
| Recommended | as an interim step | **yes** | no |

**Recommendation:** ship **(b)**. It costs little more than (a), is still plain JSON, and buys
the one thing (a) can't: a versioned, migratable, self-identifying format. If you want to land
the extension *today* and the envelope *later*, (a) is a valid stepping stone — accept
`.hyperpanes` now, then layer the `format`/`version` envelope on in a follow-up without
changing the extension again.

---

## 4. Windows file association (NSIS)

So a double-clicked `*.hyperpanes` opens in the app, register a ProgID and point its
`shell\open\command` at the binary with `"%1"` (the clicked path, delivered as `argv[1]`).

The brief's conceptual mapping is:

```
HKCR\.hyperpanes            (default) = Hyperpanes.Workspace
HKCR\Hyperpanes.Workspace\shell\open\command  (default) = "$INSTDIR\hyperpanes.exe" "%1"
```

**Important — match the per-user installer.** `installer.nsi` is a *per-user* install
(`RequestExecutionLevel user`, everything under **HKCU**, no elevation). The per-user half of
`HKCR` is **`HKCU\Software\Classes`**, so the association must be written there — writing
machine-wide `HKEY_CLASSES_ROOT`/`HKLM` would require admin and contradict the installer's
design. Snippet to add (using the existing shipped `icon.ico` for the file icon):

```nsis
; ----- .hyperpanes file association (per-user: HKCU\Software\Classes == per-user HKCR) -----
!define WS_EXT    ".hyperpanes"
!define WS_PROGID "Hyperpanes.Workspace"

; --- in Section "Install" (after the binary + icon are in $INSTDIR) ---
  WriteRegStr HKCU "Software\Classes\${WS_EXT}"   "" "${WS_PROGID}"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}" "" "Hyperpanes Workspace"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}\DefaultIcon"        "" "$INSTDIR\icon.ico,0"
  WriteRegStr HKCU "Software\Classes\${WS_PROGID}\shell\open\command" "" '"$INSTDIR\${MAIN_BINARY}" "%1"'
  ; Tell the shell the association table changed so the icon/verb apply immediately (optional).
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'  ; SHCNE_ASSOCCHANGED

; --- in Section "Uninstall" (mirror the cleanup like the PATH integration does) ---
  DeleteRegKey   HKCU "Software\Classes\${WS_PROGID}"
  DeleteRegValue HKCU "Software\Classes\${WS_EXT}" ""
  DeleteRegKey /ifempty HKCU "Software\Classes\${WS_EXT}"
  System::Call 'shell32::SHChangeNotify(i 0x08000000, i 0, i 0, i 0)'
```

Notes:
- `${MAIN_BINARY}` is already defined as `hyperpanes.exe` and `icon.ico` is already shipped to
  `$INSTDIR` by the install section — no new files needed.
- `System::Call` uses the `System` plugin bundled with NSIS; the `SHChangeNotify` calls are a
  nicety (refresh icons without a re-login) and can be dropped if you want zero plugin use.
- The quoting idiom (`'"$INSTDIR\..." "%1"'`) matches how `installer.nsi` already quotes paths
  with `$\"`.

## 5. Implementation touch-points (do **not** implement here)

For the recommended **option (b)**, the change surface is small and localized:

1. **`cli/parse.rs`** — widen the positional-path capture (currently `ends_with(".json")`, ~line
   321) to also accept `.hyperpanes`. Without this, a double-clicked `.hyperpanes` arrives as
   `argv[1]` but is **silently ignored** by the parser. Keep accepting `.json`.
2. **GUI argv bootstrap (app crate, seed-from-argv)** — the same extension allow-list applies to
   the GUI's `.json` → `load_workspace` path; widen it to `.hyperpanes` too.
3. **`workspace/io.rs`** — `read_workspace`: detect the envelope (`format == "hyperpanes"`), read
   `version`, and deserialize the inner payload; accept a bare `WorkspaceFile` as legacy
   ("version 0") for backward compat. `write_workspace`: emit the envelope for `.hyperpanes`
   targets (keep emitting bare JSON for `.json`, or migrate writes — your call). Re-assert the
   byte-identical 2-space round-trip for the envelope type. A small `WorkspaceEnvelope { format,
   version, workspace }` serde struct lives naturally next to `WorkspaceFile` in `model.rs`.
4. **`packaging/installer.nsi`** — add the association snippet from §4 to the Install section and
   its cleanup to the Uninstall section.
5. **(later) migrations** — when `version` bumps, add an in-memory upgrade step in `read_workspace`
   between deserialize and return. None needed for v1.

Option (a) is the subset of the above without steps 3 (envelope) and 5 (migrations) — only the
extension allow-lists (1, 2) and the installer association (4).

---

*This document is advisory. It changes no behavior; the schema and I/O above describe the code
as it exists at the time of writing.*

## Pane groups over the control API

- `GET /state`: every tab carries `"paneGroup": {"project": …, "color": …}` or `null`.
- `POST /command {"type":"groupPanes","paneIds":["…","…"],"title"?,"project"?}` → `{"ok":true,"tabId":"…","result":{"tabId":"…"}}`.
  Needs ≥2 distinct, existing, local panes in ONE window (they may come from different tabs).
  `project` is a project id or name. Otherwise `400` with a message (`no such pane`, `unknown
  project`, `… must all be in one window`, …).
- `POST /command {"type":"ungroupPanes","tabId":"…"}` → `{"ok":true}`; `400` if the tab doesn't
  exist or isn't a pane group.

Both are validated against the read model, then **queued for the GUI app** to apply
(`Shared::take_pane_group_requests`), since only the GUI can move panes across tabs. A headless
server (no GUI) accepts and queues the request but never applies it. Scoped tokens need every
pane (or the tab) in scope, like `recolorPane`.

## Moving a live pane over the control API

- `POST /command {"type":"movePane","paneId":"…", <one target>, "windowId"?}` →
  `{"ok":true,"tabId":"…","result":{"tabId":"…"}}`. Exactly one target:
  - `"tabId":"…"`: an existing tab, in any window.
  - `"newTab":{"title"?,"color"?}`: a new tab (`color` is `#rrggbb` or `#rgb`).
  - `"project":"…"` (a project id, name or absolute path): the window's pane-group tab for
    that project. It is reused if one exists. Otherwise one is created, titled with the
    project's name and coloured from the registry.

  For `newTab` and `project`, `windowId` picks the window. The default is the pane's own window.
- The pane keeps running: same PTY, session, pane id, label, subtitle, meta, inbox, cwd and talk
  state. Only its tab changes. A tab emptied by the move closes. A new tab opens in the
  background (the active tab doesn't change). The answered `tabId` stays valid for a new tab,
  and it is addressable at once: the new tab is in the read model (and `/state`) before the reply
  is sent, so an immediate `movePane {"tabId":…}` or `newPane {"tabId":…}` into it works. The
  moved pane itself shows in its new tab once the GUI applies the move.
  Positional ids (`"w:i"`) of other tabs can shift when a tab closes, so re-read `/state` before
  reusing one.
- Errors: `400` (no such pane / tab / window, unknown project, zero or several targets, a bad
  colour), `403` (pane, target tab or target window out of a scoped token's scope), `409` (the
  pane is its window's only pane). A pane already in the target tab answers `200` and nothing
  happens.
- `newPane` accepts `"pane":{"project":"…","projectTab":true,…}`: the new pane opens straight in
  that project's tab (reused or created, as above) instead of the caller's tab. It is opt-in.
  Without `projectTab`, `project` still only defaults the cwd and colour. When the window already
  has the project's tab, the pane is spawned directly into it. Otherwise the GUI creates the tab
  and moves the pane in on the same tick it adopts it, so it never shows in the active tab.
- `newPane` accepts a top-level `"tabId":"…"` (a tab id from `/state`): the new pane is spawned
  directly into that existing tab instead of the window's active tab, and the active tab doesn't
  change. `windowId` is optional with `tabId`, and must be the tab's own window when given. A
  tab-scoped token may spawn into its own tab this way even when that tab isn't active. `tabId`
  and `pane.projectTab` can't be combined. Errors: `400` (no such tab, a non-string `tabId`, a
  `windowId` that isn't the tab's window, or `tabId` together with `projectTab`), `403` (the tab
  is out of a scoped token's scope).
- `attach` answers with `result`: an array of TAB ids (one per group) for `"as":"tab"` (the
  default), or of PANE ids for `"as":"panes"`. Each new tab id is addressable at once, like
  `movePane`'s.
- `attach` with `"as":"panes"` accepts the same top-level `"tabId":"…"`: all its panes merge into
  that existing tab instead of the window's active tab, with the same rules and errors as
  `newPane` `tabId`. `tabId` with `"as":"tab"` (or no `as`) is a `400`.
- To put a new pane in another tab, use `tabId` or `projectTab`. Don't `newPane` and then
  `movePane`: the pane sits in the active tab until the move lands.
- `newPane` keeps `pane.subtitle` (the header's second line). It used to be dropped. The session
  snapshot records it, so it survives a restart.

Like `groupPanes`, `movePane` is validated against the read model and then queued for the GUI
app, which applies it on its next tick. A tab the move creates goes into the read model at once,
under the id the GUI then gives the tab it makes. If the GUI never makes it (the move was
dropped), the model forgets the tab after 30 s. A headless server accepts and queues the request but
never applies it. Tab membership is part of the session snapshot, so a moved pane comes back in
its new tab after a restart.

## Session file (`last-workspace.json`)

The autosave / clean-close / `restartApp` snapshot lists EVERY open window under `windows[]`
(window order, each with its tabs, `active`, `tabDock` and saved `bounds`; no top-level
`groups`). Relaunch reopens each window and re-attaches panes by `uid` per window.
Compatibility both ways: an older single-window reader takes `windows[0]`; an old file with
top-level `groups` still loads as one window. Bounds are logical px; fields the platform
can't report (e.g. position on Wayland) are omitted. "Open workspace…" loads only the first
window of a file into the current window.

Closing the LAST window with the daemon backend and keep-alive on behaves like Quit: the
sessions are left running (not killed), the snapshot of all windows is saved, and the daemon
stays up so a relaunch re-attaches. With other windows open, keep-alive off, or the
in-process backend, a closing window's sessions are killed as before.

"Quit Hyperpanes" (palette, `Ctrl/Cmd+Shift+Q`, `Cmd+Q` on macOS) closes every window at once:
with the daemon backend and keep-alive on it keeps every session and saves every window;
otherwise it ends the sessions like a close does. Closing a NON-last window ends its panes;
with the daemon backend and keep-alive on, the app first asks (Cancel / Close window / Quit)
when any of its panes runs a process.

Each pane's control meta (`setMeta` / `openPane` `meta`) is saved in its `PaneSpec.meta` and
re-applied to the pane after a relaunch, whether it was re-attached or re-spawned. Runtime-only
keys (`hp.recovered`, `hp.env-dropped`, any key whose last segment ends in `state` or is
`status` / `activity` / `liveness`) and secret-looking keys or values are not saved; at most
64 keys, values up to 4 KiB. A self-healed pane keeps its last-known label and meta.

A restored pane's `cwd` in `/state` starts from the snapshot: a re-attached pane reports the cwd
the snapshot recorded, a re-spawned one the cwd it was spawned in. A shell's own cwd reports
(OSC 7) update it later. Program panes (a `command`/`args` spawn) never send those, so this is
what keeps their `cwd` from reading `null` after a restart.

"Runs a process" for the close prompt means a program pane (spawned with a command) or a shell
whose shell-integration markers report a running command. Marker state is not carried across a
GUI relaunch, so a command started in a shell before the relaunch counts as idle until the
shell's next marker.
