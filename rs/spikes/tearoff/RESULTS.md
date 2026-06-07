# Spike B — cross-window live tear-off · RESULTS

**Track:** `spike-tearoff` · **Phase 0 go/no-go** · Windows 11, Slint 1.16, `windows` 0.58
**Status:** harness BUILT + launches clean + two-window reparent logic wired +
**live-drag confirmed by user (2026-06-07): smooth across the gap and over the second
window, no stutter/flicker/lost grab, card reparents on drop.**
**Recommendation:** **GO** — confirmed.

---

## What was built

A standalone throwaway crate (`rs/spikes/tearoff`) with **two real Slint top-level
windows** (one `AppWindow` component instantiated twice) showing stacks of "pane cards".
You grab a card and drag it out of one window and across to the other; a translucent
**ghost** chases the cursor the whole way; the target window highlights; on release over
the target the card is **reparented** (removed from the source model, pushed into the
target model).

~330 lines, one file (`src/main.rs`). All Win32 glue is isolated in a `win32` module.

## The key architectural decision (this is the whole point of the spike)

Slint gives you **neither a global cursor-position API nor a cross-window pointer grab**.
A naive implementation drives the drag from the source `TouchArea`'s `moved` events — but
those are delivered per-window and the grab is *lost the instant the cursor crosses into
the other window*. That is exactly the failure mode the spike exists to test.

**We sidestep it entirely.** The `TouchArea` is used *only* to detect the initial press
(`pointer-event` → `down`). From that moment the entire drag is driven by an **8 ms
(~125 Hz) Slint timer that reads global Win32 state**, never Slint pointer events:

| Need | Win32 call | Why Slint can't |
| --- | --- | --- |
| screen-global cursor | `GetCursorPos` | no global-cursor API at all |
| drag-end (button release) | `GetAsyncKeyState(VK_LBUTTON)` | no cross-window grab to deliver an `up` |
| hit-test vs each window | `GetWindowRect` (+ HWND via raw-window-handle) | Slint coords are window-local |
| follow-cursor overlay | layered window + `SetWindowPos` | no transparent/click-through/topmost window type |

Because the pump is decoupled from Slint's pointer plumbing, **there is no grab to lose** —
crossing window boundaries, dead screen space, or the other window's surface are all
identical to the pump: it just reads the OS cursor every 8 ms. This is the finding that
makes tear-off viable in Slint.

## Win32 pieces required (the real cost of this interaction)

1. **`GetCursorPos`** — global cursor each tick.
2. **`GetAsyncKeyState(VK_LBUTTON)`** — poll button-still-down / released (drag end),
   debounced so a stale poll right after grab can't end the drag early (`armed` flag).
3. **Ghost window** = a pure-Win32 layered popup: `CreateWindowExW` with
   `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`,
   `WS_POPUP`, alpha via `SetLayeredWindowAttributes`, moved with `SetWindowPos`
   (`SWP_NOACTIVATE | SWP_SHOWWINDOW`). **Crucially the ghost is NOT a Slint window** — it
   never enters the Slint render path, so following the cursor is a single `SetWindowPos`
   per tick (cheap, no compositor churn).
4. **`GetWindowRect`** per window for hit-testing the cursor against window edges.
5. **HWND extraction** from each Slint window via `raw-window-handle` 0.6
   (`slint = { features = ["raw-window-handle-06"] }`).
6. **`WM_NCHITTEST`** — **NOT needed here.** We kept the OS-native titlebar/decorations,
   so window move/resize is free. *If* the real app wants a frameless / custom titlebar
   (Slint's frameless support is weak), window drag + resize hit-zones must be hand-rolled
   via `WM_NCHITTEST` in a custom wndproc — flagged as a separate, additive cost, not a
   blocker for tear-off.

## What worked / verified

- ✅ **Compiles clean** (`cargo build`, 0 warnings after cleanup).
- ✅ **Launches and stays up**; two windows realize side-by-side, models populate, native
  decorations present (see `_smoke_crop.png` evidence captured during the run).
- ✅ **Multi-window in Slint is real** — two independent top-levels, independent models,
  one shared event loop, one shared timer pump.
- ✅ **HWND extraction works** via raw-window-handle once windows are realized.
- ✅ **Reparent logic** wired end-to-end (remove from source `VecModel`, push to target).
- ✅ **Symmetric** — either window can be source or target (drag A→B *and* B→A).

## Gotchas found (documented so the real port doesn't re-hit them)

- **HWND is `NotSupported` before the event loop runs.** Slint creates the native winit
  window lazily; `window_handle()` only yields a valid HWND *after* `run_event_loop()`
  starts. Fix: fetch HWNDs lazily on the first timer ticks (`ensure_hwnds`), not at setup.
- **`DefWindowProcW` is generic in `windows` 0.58** and can't be used as a raw
  `lpfnWndProc` fn-pointer — needs a thin `extern "system"` shim that forwards to it.
- **Slint color literals trip the Rust lexer.** `#5ee08f` inside the `slint!` macro is
  tokenized by *Rust* first, which reads `5e…` as a float exponent → "expected at least
  one digit in exponent". Avoid `<digit>e…` hex colors (use e.g. `#3ad07a`).
- **DPI:** `GetCursorPos`, `GetWindowRect`, and `SetWindowPos` are all in physical pixels,
  so they're mutually consistent with no scaling math. Worth re-checking on a mixed-DPI
  multi-monitor setup in the real port.

## Smoothness assessment

The design removes the usual stutter/flicker sources by construction:
- ghost is a bare layered window (no Slint frame in the hot path) → move = one `SetWindowPos`;
- 8 ms pump ≈ 125 Hz, comfortably above display refresh;
- no z-order fights: ghost is `WS_EX_TOPMOST | WS_EX_NOACTIVATE` and click-through, so it
  never steals focus or activation from the Slint windows;
- no lost-capture path exists because we never rely on capture.

Statically + smoke-verified (compiles, launches, windows + ghost created, logic wired),
and **the felt smoothness of a live human drag across the second window was confirmed by
the user on 2026-06-07: smooth, no stutter/flicker/lost grab, card reparents on drop.**
The predicted "no jank by construction" held in practice.

## Plan B (only if live drag is NOT smooth)

If the ghost stutters / flickers / fights z-order in practice and can't be tuned (e.g.
drop the pump to 4 ms, or switch ghost moves to `DeferWindowPos`/`UpdateLayeredWindow`):
fall back to **degraded detach UX** — a "Detach pane" menu item / button spawns the pane in
a new window immediately (no follow-cursor), plus static **drop-zones** (highlighted edges)
you release onto. No global-cursor choreography required; uses only plain multi-window +
`GetWindowRect`. Strictly less magical than the current app's live tear-off, but fully
achievable in Slint with near-zero risk. **Assessment: viable safety net, not needed unless
the live test below fails.**

## How to run it

```
cd rs/spikes/tearoff
cargo run
```
Drag a card out of **Window A** and release it over **Window B** (and vice-versa). Watch
the ghost follow the cursor and Window B highlight green when you're over it.

---

### HITL — resolved
Live-drag test performed by the user on 2026-06-07. Verdict: **GO — smooth.** Ghost
follows the cursor smoothly across the gap and over the second window with no stutter /
flicker / lost grab; the card reparents on drop. Plan B not needed. The signature
tear-off interaction is proven viable in Slint + Win32.
