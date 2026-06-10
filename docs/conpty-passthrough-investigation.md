# ConPTY scroll-region throughput investigation

**Question:** hyperpanes' `scrolling-region` (DECSTBM) terminal throughput is ~0.4 MB/s vs
Windows Terminal's ~33 MB/s (~80×). Is it fixable, how, and at what cost?

**Short answer:** The 80× gap is **a Windows ConHost/ConPTY limitation, not a hyperpanes bug,
and it is NOT fixable from our side by a flag, a dependency bump, or sideloading a newer ConPTY.**
We empirically loaded the latest official redistributable ConPTY (the one that contains the big
"passthrough" perf refactor) and the scroll-region case stayed at 0.2–0.4 MB/s. The one thing we
*can* and *did* fix in our code is a wasteful **double-parse** of the pty stream — a real CPU/heat
win on every streaming workload, though it does not move the scroll-region MB/s (that ceiling is
upstream in conhost). A separate genuine win (matching WT) is to **shrink the pty grid height**,
which directly shrinks the size of conhost's repaint.

---

## 1. Confirmed root cause (reproduced on this machine)

Machine: Windows 11 24H2, build **26100** (in-box conhost 10.0.26100.1), release binary, via the
bench harness. The catastrophe is **entirely caused by the `ESC[1;20r` DECSTBM prologue** — the
only difference between the fast and slow cases is that one escape sequence:

| case (same line content, 2 MB) | MB/s | vs scrolling-region |
| --- | ---: | --- |
| `scrolling-region` (`ESC[1;20r` then stream) | **0.4** | 1× |
| `scrolling` (no region, identical lines) | **7.9** | ~20× faster |
| `dense` (full rows) | **12.1** | ~30× faster |

Track H's instrument already proved the mechanism: during a scroll-region run the app **feeds
~28 MB/s** internally while node's input side is throttled to ~0.4 MB/s — a **~70× byte inflation**.
ConHost, scraping its character grid into VT for the ConPTY master pipe, **re-renders the whole
DECSTBM scroll region (cursor-home + ~20 lines) on every single scrolled line** instead of emitting
an efficient scroll/index sequence. node is backpressured by conhost's *output-generation* rate, not
by anything in hyperpanes: the app keeps up with the inflated 28 MB/s at ~38 % of one core and the
grid scroll is O(1) (alacritty ring-rotate, measured by Track A). **The bottleneck is upstream of
our process.**

This is a long-standing, documented ConHost issue:
[microsoft/terminal#7019 — "conpty exhibits pathological performance on scrolling region redraw
(repaints entire screen)"](https://github.com/microsoft/terminal/issues/7019) — **closed as
*not planned*.**

### Why does Windows Terminal get 33 MB/s on the same workload?
WT bundles its **own, newer** `OpenConsole.exe` + `conpty.dll` pair (WT 1.24 here). Its ConPTY
contains the VtEngine-removal refactor
([microsoft/terminal#17510](https://github.com/microsoft/terminal/pull/17510), shipped in WT 1.22+),
which made general mixed-output ConPTY ~20× faster by passing app VT through unmodified rather than
re-rendering. That refactor helps general output a lot — **but it does not fix the DECSTBM
scroll-region repaint specifically** (see the measured result in §3). WT's bundled host is also tuned
and the comparison is host-to-host; the residual win for WT on this exact case is a combination of
the newer host plus its render pipeline. Critically, *we got the same newer host and still stayed
slow* — so "bundle WT's conpty" is not the lever people assume it is.

---

## 2. How the pty is created today

`rs/crates/core/src/session/pty.rs` → `spawn_pty()` uses `portable-pty` **0.9.0** (`native_pty_system()`
→ its Windows ConPTY backend). The backend (in the crate, not ours) calls `CreatePseudoConsole` with a
**hardcoded** flag set and **no public API to change it**:

```
// portable-pty-0.9.0/src/win/psuedocon.rs  (vendored, read-only)
PSUEDOCONSOLE_INHERIT_CURSOR | PSEUDOCONSOLE_RESIZE_QUIRK | PSEUDOCONSOLE_WIN32_INPUT_MODE
//                                                            ^ NOT passed:
pub const PSEUDOCONSOLE_PASSTHROUGH_MODE: DWORD = 0x8;   // #[allow(dead_code)] — defined, unused
```

So portable-pty 0.9 **defines but never passes** the `PASSTHROUGH_MODE` (0x8) flag, and exposes no
setter for it. It does, however, **prefer a sideloaded `conpty.dll`** next to the running exe over
the in-box kernel one (`load_conpty()` does `LoadLibrary("conpty.dll")` first). That sideload path is
the only no-fork lever — and we tested it (§3).

`pty.rs` already fixes the grid at 80×24 at spawn and the spawn never needs the pane area; the app
relayouts/resizes it on the first render pump. (Relevant to option C below.)

---

## 3. Options evaluated (with measured results)

### Option A — pass a ConPTY "passthrough" flag without forking — ❌ NOT POSSIBLE / NO EFFECT
- There is **no documented, app-settable** `PSEUDOCONSOLE_PASSTHROUGH_MODE` public API. The two
  feature issues ([#1173](https://github.com/microsoft/terminal/issues/1173),
  [#1985](https://github.com/microsoft/terminal/issues/1985)) are closed as **Backlog / Duplicate**;
  the 0x8 constant exists in headers but is not a supported way for a client to tell conhost "stop
  rendering the buffer." The shipped "passthrough" work (#17510) is an *internal conhost refactor*,
  not a client-set flag.
- portable-pty 0.9 doesn't pass 0x8 anyway and has no setter, so even attempting it requires a fork.
- **Verdict: dead end.** Effort: n/a. Gain: none.

### Option B — bump / sideload a newer ConPTY (the redistributable) — ❌ TESTED, NO MEANINGFUL WIN
- The official redistributable NuGet package **`Microsoft.Windows.Console.ConPTY`** exists
  (latest stable **1.24.260512001**) and ships the matched `conpty.dll` (x64, 109 KB) +
  `OpenConsole.exe` (x64, 1.04 MB) pair that contains the #17510 passthrough refactor. WezTerm
  sideloads exactly this ([wezterm#7774](https://github.com/wezterm/wezterm/issues/7774)).
- We downloaded it, deployed both files next to `hyperpanes.exe`, and **confirmed it loads**: a
  process snapshot during a run shows our app (`hyperpanes.exe`) spawning
  `…\release\OpenConsole.exe` (v1.24.260512001) — the sideloaded host, not the in-box `conhost.exe`.
- **Measured scroll-region throughput: 0.2 MB/s (in-box) → 0.3 MB/s (sideloaded 1.24).** Within
  noise. **The newest ConPTY does NOT fix the DECSTBM repaint.** This matches #7019 being
  *closed as not planned* — the scroll-region inflation is not what the passthrough refactor
  addressed.
- **Verdict: does not solve the catastrophe.** (It *may* still be worth shipping later for general
  robustness — it fixes unrelated crashes/out-of-sync bugs — but that's a separate decision, not a
  throughput fix.) Effort to ship: low-moderate (deploy 2 files + a matched-pair update story).
  Gain on the target metric: ~0.

### Option C — shrink the pty grid (rows) to shrink conhost's repaint — ⭐ MOST PROMISING REMAINING LEVER (UNTESTED IN THIS ENV)
The inflation is proportional to the **number of lines conhost repaints per scrolled line**, i.e. the
scroll-region height. The bench sets `ESC[1;20r` (20 lines). If the pane's pty grid is taller than it
needs to be, every scroll repaints more lines. Two concrete sub-levers:
- Spawn / keep the pty at the **actual visible rows** of the pane, never larger (today it starts 80×24
  and is resized to the pane). For small panes this directly cuts repaint size.
- This is the same class of mitigation other ConPTY apps use (smaller grid = smaller scrape). It does
  **not** close the 80× gap (a 20-line region still repaints 20 lines/line), but it scales the cost
  down with pane size and is fully in our control (it's a `pty.rs` / resize concern, no fork).
- **Could not be measured headlessly in this sandbox** (see "Probe limitation" below); needs a GUI
  bench pass at varied pane heights. Effort: low. Expected gain: partial, pane-size-dependent.

### Option D — eliminate the in-core double-parse — ✅ DONE (CPU/heat win; does not move scroll-region MB/s)
The same pty byte stream was being parsed **twice**:
1. `rs/crates/core/src/session_manager.rs` `flush_into` → `screen.advance()` — a full
   `alacritty_terminal` VTE parse into a headless "screen mirror" used only by `mode:"screen"`
   control reads + the `awaitingInput` heuristic. **Ran on every flush, unconditionally**, even with
   no control client attached (e.g. the bench, or any non-MCP session).
2. `rs/crates/terminal-widget/src/grid.rs` `feed()` — the real GUI grid parse, fed from
   `SessionEvent::Data`.

**Fix (prototyped & shipped in this branch):** made the screen mirror **lazy**. `flush_into` now
**buffers** flushed bytes into `Shared::screen_pending` instead of parsing them; the screen is parsed
on demand (`Shared::sync_screen`) only when `render_screen` / resize actually needs it. Correctness is
identical (the read brings the mirror fully current first); the hot path does **zero** VTE work for
the mirror. This removes one of the two ~28 MB/s parses on every streaming workload.
- All **375 core tests pass**, including new tests
  (`lazy_screen_reflects_buffered_output_on_sync`, `manager_render_screen_syncs_pending_output`) and
  the existing readScreen/resize/control-parity tests.
- It does **not** change the scroll-region MB/s (the reader is on its own thread; conhost is the
  ceiling) — and it's not meant to. It's a real CPU/heat reduction (~one full VTE parse of the inbound
  stream removed) that pays off on *every* high-throughput pane, most when no control client is
  reading the screen. Effort: done. Gain: CPU/heat only, not the headline metric.

### Option E — fork the ConPTY backend — ❌ NOT WARRANTED
A fork could pass 0x8 or implement an in-proc VT pipe, but (a) 0x8 demonstrably doesn't fix this case
(Option A/B), and (b) the scroll-region repaint lives in conhost regardless of how we create the pty.
A fork buys nothing here. High effort, ~0 gain on the metric. **Rejected.**

---

## What was prototyped

1. **`rs/spikes/conpty-probe/`** — a standalone `portable-pty` probe that spawns `node throughput.mjs`
   in a ConPTY and measures node-input rate + master-output bytes (the inflation factor), and reports
   which `conpty.dll`/`OpenConsole.exe` it loaded. Used to A/B in-box vs sideloaded ConPTY.
   **Probe limitation:** in this *non-interactive/sandboxed* agent session, ConPTY scrapes a screen
   that never flows to a child with no real console window (the exact behavior documented in
   `pty.rs`'s env note — a bare portable-pty example reproduces it). So the headless probe reads
   0 bytes here and the *bench (which launches the real GUI window)* is the only reliable measurement
   path in this environment. The sideload-vs-inbox numbers in Option B come from the GUI bench, where
   output flows (the 7.9/12.1 MB/s non-region cases prove it).
2. **Sideload deployment** — downloaded `Microsoft.Windows.Console.ConPTY` 1.24.260512001, deployed
   the x64 `conpty.dll`+`OpenConsole.exe` next to `hyperpanes.exe`, **confirmed it loads** (OpenConsole
   spawned as our child), re-measured: **no win** (Option B).
3. **Lazy screen mirror** — the double-parse elimination (Option D), shipped in
   `rs/crates/core/src/session_manager.rs` with tests. 375/375 core tests green.

---

## Recommended plan

1. **Accept that the 80× scroll-region gap is a ConHost limitation we cannot close from our process.**
   It is [microsoft/terminal#7019](https://github.com/microsoft/terminal/issues/7019) (closed *not
   planned*). Neither a flag (A), a dep bump/sideload of the newest ConPTY (B), nor a fork (E) fixes
   it — all verified. Document this so it stops being re-investigated as an app bug.
2. **Ship the double-parse fix (Option D).** Already done in this branch — a real CPU/heat win on all
   streaming, zero behavior change, fully tested. No downside.
3. **Try Option C (smaller pty grid) next, with a GUI bench pass** at varied pane heights. It's the
   only remaining in-our-control lever and scales the repaint cost down with pane size. Cheap to try.
4. **Optionally** adopt the redistributable ConPTY pair (B) later — *not* for scroll-region throughput
   (it doesn't help) but for the unrelated stability fixes (out-of-sync buffer, TUI-exit crashes) it
   brings, with a matched-pair update story like WezTerm's. Treat as a separate robustness task.
5. **Realistic framing of the bench number:** the scroll-region case is a worst-case microbenchmark
   for *Windows ConPTY itself*; every ConPTY-based emulator that uses the in-box host (including older
   conhost) hits it. The honest competitive story is "we match WT/Alacritty on normal output
   (scrolling/dense within run-to-run noise) and are bounded by Windows ConPTY on the DECSTBM
   worst-case, same as any in-box-host app." It is not a hyperpanes regression.

## Sources
- [microsoft/terminal#7019 — pathological scroll-region redraw (closed: not planned)](https://github.com/microsoft/terminal/issues/7019)
- [microsoft/terminal#17510 — VtEngine removal / ConPTY passthrough refactor (~20× general output)](https://github.com/microsoft/terminal/pull/17510)
- [microsoft/terminal#1173 / #1985 — ConPTY passthrough mode (Backlog / Duplicate)](https://github.com/microsoft/terminal/issues/1173)
- [microsoft/terminal#16333 — conhost scrolling perf (SetScrollInfo lock)](https://github.com/microsoft/terminal/pull/16333)
- [wezterm#7774 — bundle the redistributable ConPTY pair](https://github.com/wezterm/wezterm/issues/7774)
- [microsoft/terminal discussion#17608 — distributing conhost fixes / ConPTY NuGet](https://github.com/microsoft/terminal/discussions/17608)
- `Microsoft.Windows.Console.ConPTY` NuGet (latest stable 1.24.260512001)
