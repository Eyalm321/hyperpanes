# Ports seams — frozen surfaces for the Wave-1 platform tracks

Written by track **W0-SEAMS** (branch `fanout/seams`). This is the contract the seven
parallel Wave-1 tracks build against: the per-platform module surfaces below are
**frozen** (signature changes need the orchestrator), and each file is owned by exactly
one track so no two tracks touch the same file.

Every seam is a **cfg-selected module re-export**: a `mod.rs` (or shared module) holds
the cross-platform logic + the dispatch, and one file per platform implements the same
item surface. Selection rules everywhere:

```rust
#[cfg(windows)]                                  → windows.rs / platform_windows.rs
#[cfg(target_os = "macos")]                      → macos.rs / platform_macos.rs
#[cfg(not(any(windows, target_os = "macos")))]   → linux.rs / platform_linux.rs   (Linux is the unix fallback)
```

Core (`rs/crates/core`) splits use plain `#[cfg(windows)]` / `#[cfg(not(windows))]`
(`windows.rs` / `unix.rs`) — one unix implementation serves Linux and macOS there.

## 1. Frozen surfaces

### `app/src/window/` — window chrome (per-OS files)

```rust
pub type/struct SavedPlacement;                  // opaque pre-fullscreen placement
pub fn hwnd_of(win: &slint::Window) -> isize;    // native handle (0 until realized)
pub fn make_frameless(raw: isize);               // strip OS chrome + install hooks
pub fn start_drag(raw: isize);                   // system move-drag (drag-the-bar)
pub fn begin_drag_cursor(raw: isize);            // force the tear-off drag cursor
pub fn end_drag_cursor(raw: isize);              // release drag cursor + capture
pub fn set_hover_cursor(on: bool);               // open-hand hover cursor on/off
pub fn minimize(raw: isize);
pub fn toggle_max(raw: isize);
pub fn is_maximized(raw: isize) -> bool;
pub fn close(raw: isize);
pub fn enter_fullscreen(raw: isize) -> Option<SavedPlacement>;
pub fn exit_fullscreen(raw: isize, saved: SavedPlacement);
```

`raw` is whatever `hwnd_of` returned (Win32: the HWND as `isize`; other platforms pick
their own encoding — callers only round-trip it). `0` must always mean "not realized /
no-op".

### `app/src/drag/` — global pointer + drag ghost (per-OS files)

Pure geometry (`DragKind`, `DragState`, `Hover`, `edge_band`, `DRAG_THRESHOLD_PX`) is
shared in `drag/mod.rs` and is NOT platform work. The per-platform surface:

```rust
pub trait GlobalPointer {                         // defined in drag/mod.rs
    fn poll(&self) -> Option<(slint::PhysicalPosition, bool /* primary down */)>;
    fn supports_cross_window(&self) -> bool;
}
pub fn global_pointer() -> &'static dyn GlobalPointer;   // returns platform::PlatformPointer

// per-platform file:
pub struct PlatformPointer;                       // impl GlobalPointer
pub fn window_rect(raw: isize) -> (i32, i32, i32, i32);  // screen px (l, t, r, b)
pub struct Ghost;                                 // new() / follow((x, y)) / hide()
```

Behavioral contract: `poll() == None` ⇒ the app never starts or pumps a drag (clicks
still work). `supports_cross_window() == false` (Wayland) ⇒ implement the in-window
fallback in the platform file; the tear-off/new-window paths must not be reachable.

### `app/src/prefs/platform_*.rs` — `PlatformDefaults` provider

```rust
pub const SHELL_OPTIONS: [(&str, &str); N];       // picker label + spawn token ("" = system)
pub const FALLBACK_FONT: &str;                    // always-present monospace font path
pub fn preferred_shell() -> Option<String>;       // "System" resolution (None = let core pick)
pub fn font_dirs() -> Vec<std::path::PathBuf>;    // candidate font folders, must INCLUDE
                                                  // super::bundled_font_dir() last
```

`N` may differ per platform (each file compiles alone). Everything else in `prefs/`
(Settings blob, font resolution, persistence) is shared and off-limits to platform tracks.

### `app/src/update/` — self-update apply step (per-OS files)

```rust
pub enum ApplyStrategy { SilentInstaller, NotifyOnly }    // defined in update/mod.rs
pub fn apply_strategy() -> ApplyStrategy;                 // in mod.rs → platform const

// per-platform file:
pub const APPLY_STRATEGY: ApplyStrategy;
pub fn launch_installer(path: &Path) -> Result<(), String>;
```

Non-Windows ships `NotifyOnly` today (surface "update available" + open the releases
page); `launch_installer` returns `Err` until a real flow lands. The check/download
machinery in `update/mod.rs` is shared.

### `core/src/single_instance/` — single-instance gate + argv hand-off

Shared in `mod.rs`: `HandoffMessage { argv: Vec<String>, cwd: String }` (the JSON wire
shape — do not change), `InstanceNames`, `instance_names(salt)`, `user_salt()`,
`enum Instance { Primary(..), Secondary(..) }`. Per-platform (`windows.rs` / `unix.rs`):

```rust
pub fn acquire(salt: &str) -> io::Result<Instance>;
pub struct PrimaryInstance;
impl PrimaryInstance {
    pub fn pipe_name(&self) -> &str;
    pub async fn run_server<F: FnMut(HandoffMessage)>(self, handler: F) -> io::Result<()>;
}
pub struct SecondaryInstance;
impl SecondaryInstance {
    pub fn pipe_name(&self) -> &str;
    pub async fn forward(&self, msg: &HandoffMessage) -> io::Result<()>;
}
```

`unix.rs` currently returns `ErrorKind::Unsupported` from `acquire`. Expected unix
shape: an `O_EXCL`/`flock` lock file as the detector + a unix-domain socket carrying the
same `{argv, cwd}` JSON. `user_salt()` falls back to `"hyperpanes-default"` off-Windows;
the unix track should key it off `$XDG_RUNTIME_DIR`/`$HOME` (a `mod.rs` edit — coordinate,
it is shared).

### `core/src/session/env/` — `FreshEnvProvider`

```rust
pub trait FreshEnvProvider {                      // defined in env/mod.rs
    fn fresh_env_with_process(&self, process: EnvMap) -> EnvMap;
}
pub struct PlatformEnv;                           // impl per platform file
pub fn fresh_env() -> EnvMap;                     // shared entry point (unchanged)
```

`windows.rs` = the registry machine+user merge; `unix.rs` = process-env pass-through
(already correct POSIX behavior — only touch it if a fresher source than the process env
exists). The pure merge/expand logic + its tests live in `env/mod.rs` and are shared.

## 2. Wave-1 file-ownership map

| Track | Owns (exclusively) |
|---|---|
| **linux-window** | `app/src/window/linux.rs`, `app/src/drag/linux.rs` |
| **macos-window** | `app/src/window/macos.rs`, `app/src/drag/macos.rs` |
| **app-unix-shared** | `app/src/prefs/platform_linux.rs`, `app/src/prefs/platform_macos.rs`, `app/src/update/linux.rs`, `app/src/update/macos.rs` |
| **unix-core** | `core/src/single_instance/unix.rs`, `core/src/session/env/unix.rs` (unix provider), `core/src/persistence/paths.rs` |
| *(frozen — touch only via orchestrator)* | every `mod.rs` above, `windows.rs`/`platform_windows.rs` files, both `Cargo.toml`s |

Notes:
- `paths.rs` is a single shared file today; unix-core owns making its dirs XDG-correct.
- New platform-specific deps go under `[target.'cfg(...)'.dependencies]` (the `windows`
  crate is already gated that way in both core and app; the app has `libc` under
  `cfg(unix)`).
- `cargo check --manifest-path rs/crates/app/Cargo.toml` must stay green on Linux at all
  times (CI gates core + widget; the app check is the local bar for window/drag work).

## 3. Packaging-script contract (for the CI/release track)

Fixed paths + argv, so `release-rust.yml` can reference them before they exist:

```
rs/packaging/appimage.sh <version>      # builds rs/crates/app for x86_64 Linux,
                                        # emits  rs/packaging/out/hyperpanes-<version>-x86_64.AppImage
rs/packaging/macos/bundle.sh <version>  # builds rs/crates/app for macOS,
                                        # emits  rs/packaging/out/hyperpanes-<version>.dmg
```

Both scripts: run from any cwd (they resolve the repo root themselves), exit non-zero on
any failure, and put ALL artifacts under `rs/packaging/out/`. `<version>` arrives
without a leading `v` (the workflow strips the tag prefix).

## 4. Latent-bug fixes landed in this wave (context, not work items)

- `projects::path_key` dedups case-insensitively on `cfg!(any(windows, target_os = "macos"))`
  (APFS default is case-insensitive); Linux keeps exact-case keys.
- `state::local_secs_since_midnight` (unix) uses `libc::localtime_r` — the old
  `epoch % 86400` was UTC and skewed reminder due-times by the timezone offset.
- The ConPTY startup-query interception in `session/pty.rs` is compile-time
  `#[cfg(windows)]` (`StartupQueryFilter`); on POSIX an early `ESC[6n` passes through
  untouched (pinned by a unix-only test) because it is a real application query.
- `app/build.rs` deploys the ConPTY pair only when `CARGO_CFG_TARGET_OS == "windows"`.
