# ConPTY scroll-region throughput investigation

> **⚠ 2026-06-09 addendum at the bottom partially corrects this doc.** The standalone probe now
> works headless (the old "0 bytes" limitation was the INHERIT_CURSOR `ESC[6n` handshake, not a
> missing console window), the 0x8 passthrough flag is now **measured** a no-op (not inferred),
> and — important correction — the sideloaded 1.24 host does **NOT** repaint the scroll region
> (1.0× inflation); what it doesn't fix is end-to-end delivery pacing. See §Addendum.

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

---

## Addendum 2026-06-09 — standalone probe verification (corrects parts of the above)

The probe was made to work headless, the 0x8 flag was actually tested via a locally patched
portable-pty, and the in-box vs 1.24 comparison was re-run at the probe level with the host
identity proven. Three findings **correct** earlier sections; the headline (in-box conhost is
the bottleneck and it is not app-fixable) stands.

### A. The "headless probe reads 0 bytes" limitation is SOLVED — it was the `ESC[6n` handshake
portable-pty passes `PSUEDOCONSOLE_INHERIT_CURSOR`, so the pseudoconsole host starts by sending
a cursor-position query (`ESC[6n`) to the *master* and **emits nothing until the terminal
replies**. A real terminal answers automatically; a raw byte-counting client never does, so the
probe sat at 0 bytes forever — in *any* session. It had nothing to do with console windows or
interactivity (a minimized real-console run also read 0 bytes; the host process
`conhost.exe --headless --inheritcursor` was alive the whole time). The probe now replies
`ESC[1;1R` when it sees the query and **runs fine in a fully sandboxed agent session**. The
same handshake explains `pty.rs`'s environment note (the ignored smoke tests could answer the
query and run headless). A second probe bug was fixed while here: it joined the reader thread
before dropping the master, deadlocking on an EOF that only arrives after `ClosePseudoConsole`.

### B. The 0x8 passthrough flag is now MEASURED a no-op (Option A upgraded from inference)
We vendored portable-pty 0.9.0 into the spike (`vendor/portable-pty`, wired via
`[patch.crates-io]`) and made it OR in `PSEUDOCONSOLE_PASSTHROUGH_MODE` (0x8) when
`PORTABLE_PTY_CONPTY_PASSTHROUGH` is set. Result: byte-identical behavior with and without the
flag on **both** hosts (in-box: 40.72 MB master / 20.4× either way; 1.24: 8.00 MB / 1.0× either
way). The flag does nothing; the fork question is settled empirically.

### C. CORRECTION — the 1.24 host does NOT repaint the scroll region; its problem is pacing
§1/§3-Option-B's "the newest ConPTY does NOT fix the DECSTBM repaint" conflated two things.
Probe-level truth (host identity proven mid-run: child is `OpenConsole.exe 1.24.2605.12001`):

| host (probe, 2026-06-09) | case | grid | child-side MB/s | master bytes | inflation |
| --- | --- | --- | ---: | ---: | ---: |
| in-box conhost 26100 | region `1;20r` | 120×30 | 1.20 | 40.72 MB (2 MB in) | **20.4×** |
| in-box | region `1;20r` | 120×40 | 0.72 | 80.27 MB (2 MB in) | **40.1×** |
| in-box | region `1;20r`, grid 10 rows (invalid → ignored) | 120×10 | 7.60 | 2.00 MB | 1.0× |
| in-box | no region | 120×30 | 6.19 | 2.00 MB | 1.0× |
| in-box | region + 0x8 | 120×30 | 1.22 | 40.72 MB | 20.4× |
| sideloaded 1.24 | region | 120×30 | 2.27* | 8.00 MB (8 MB in) | **1.0×** |
| sideloaded 1.24 | no region | 120×30 | 2.46* | 8.00 MB | 1.0× |
| sideloaded 1.24 | region + 0x8 | 120×30 | 2.33* | 8.00 MB | 1.0× |
| sideloaded 1.24 | region + mid-run resize 80×24→120×30 | — | 2.32* | 8.00 MB | 1.0× |

\* 1.24 rates are not backpressure-meaningful: the 1.24 host **buffers the raw VT unboundedly
and defers flushing while the child streams** — a phased workload proved bytes arrive when the
child *pauses* (flush-on-idle), so under sustained output the consumer sees nothing until the
stream lulls. That, not repaint inflation, is the 1.24-era pathology, and it is consistent with
the GUI bench measuring no end-to-end win from the sideload (§3 Option B's 0.2→0.3 MB/s result
stands; its *explanation* — "repaint not fixed" — was wrong).

### D. The unified model (reconciles probe and app numbers exactly)
In-box conhost generates repaint VT at a roughly constant **~25–29 MB/s** ceiling; the child is
backpressured to `ceiling ÷ inflation`, and inflation ≈ rows repainted per scrolled line:
- 120×30 → 20.4× → 24.5/20.4 = **1.20 MB/s** (measured 1.20)
- 120×40 → 40.1× → 29.05/40.1 = **0.72 MB/s** (measured 0.72)
- app pane (Track H) → ~70× → 28/70 = **0.4 MB/s** (measured 0.4)

This also *strengthens* Option C (shrink the pty grid): inflation tracked total grid rows in the
40-row run, so keeping panes' pty rows minimal directly raises the child's MB/s.

### E. Upstream-facing conclusions (for the microsoft/terminal#7019 contribution)
1. The default in-box conhost — what every ConPTY app gets without sideloading — still has the
   #7019 pathology on Win11 26100, freshly measured, with a clean headless repro (inflation ≈
   rows repainted; negative control: invalid region → 1.0×).
2. Do **not** claim "1.24 still repaints" — it doesn't (1.0× measured). The accurate asks are
   (a) service the fix into the in-box host, and (b) the 1.24 host's deferred flush-on-idle
   under sustained output deserves its own look.
3. 0x8 is measured irrelevant on both hosts; no portable-pty change is warranted (the local
   vendored patch exists only to prove this).

Probe runs live in `rs/spikes/conpty-probe/results/`; the phased workload is
`rs/spikes/conpty-probe/phases.mjs`.

### F. 2026-06-09 (late) — GUI bench A/B REVERSES Option B's verdict: the sideload is a huge win
A clean same-binary A/B through the real GUI bench (release build incl. Option C + the lazy
screen mirror, 3 runs, only delta = `conpty.dll`+`OpenConsole.exe` 1.24.260512001 next to the
exe), label `sideload124-2026-06-09` in `bench/results/`:

| case (MB/s) | in-box | sideloaded 1.24 | gain | WT 1.24 same night |
| --- | ---: | ---: | ---: | ---: |
| dense | 5.9 | **98.6** | 16.7× | 56.5 |
| scrolling | 7.9 | **65.5** | 8.3× | 63.9 |
| unicode | 8.5 | **58.9** | 6.9× | 47.0 |
| cursor-motion | 14.4 | **33.1** | 2.3× | 35.6 |
| scrolling-region | 0.4 | **17.6** | 44× | 26.0 |
| alt-screen | 22.4 | 29.2 | 1.3× | 53.2 |

17.6 MB/s on scroll-region is impossible on the in-box host (ceiling ≈ 28 ÷ rows ≈ 0.4), so the
1.24 host demonstrably carried the run. **Option B's "tested, no meaningful win" (0.2→0.3 MB/s)
does not reproduce on tonight's build** — that measurement predated the lazy-screen-mirror fix
(Option D) and Option C, and whatever else confounded it, the modern A/B is unambiguous. The
probe-observed flush-on-idle (§C) evidently does not bite a real GUI consumer.
**New recommendation: SHIP the redistributable pair** (deploy next to the exe in build +
packaging, matched-pair update story like WezTerm) — it turns the weakest column into one that
beats Windows Terminal on dense/scrolling/unicode and lifts scroll-region 44×. Remaining gaps to
WT after sideload: alt-screen (29 vs 53) and scroll-region (17.6 vs 26) — now app-side
render-pipeline territory, no longer host-bound.

**SHIPPED 2026-06-09:** pair vendored at `resources/conpty/` (README has the update story),
deployed next to the exe by `rs/crates/app/build.rs` (dev) and by `rs/packaging/installer.nsi`
($INSTDIR, alongside the previously-unpackaged shell-integration scripts). Stock-build
fingerprint: scrolling-region 11.2 MB/s single-run — impossible on the in-box host, so the
bundled pair demonstrably loads with zero configuration.

### G. 2026-06-10 — startup: CreateProcessW of pwsh-into-ConPTY blocks ~1s (fixed by async spawn)
Profiling the 2121 ms bench startup with `HYPERPANES_PERFLOG` + temporary core timings found the
first pane's `mgr.create` blocking the startup path for **1.0–1.1 s, every launch** — and the
entire cost is `CreateProcessW` into the pseudoconsole for **pwsh 7 specifically**:

| child spawned into the ConPTY | `spawn_command` |
| --- | ---: |
| pwsh 7 (token or full path — not a PATH-search issue) | **1.04–1.13 s** |
| powershell.exe 5.1 | 17 ms |
| cmd.exe | 8 ms |
| node.exe | 65 ms |
| pwsh 7 raw CreateProcessW, NO pseudoconsole (warm) | 2–6 ms |

~~The stall needs both pwsh 7 AND the pseudoconsole attribute; host version doesn't matter.~~
**Corrected in §H — the per-shell attribution was wrong; the gate is the HOST's unanswered
startup queries** (every measurement above ran with the bundled 1.24 pair deployed). The async
spawn fix here remains correct and shipped: `State::spawn_session_async` moves `mgr.create` to a
worker thread for all pane creation (seed, splits, restores — restores now spawn in parallel),
with a `spawn_done` queue drained by `App::tick` that re-applies geometry (a resize during the
spawn window would otherwise be silently lost) and kills sessions whose pane closed mid-spawn.
Result at this stage: window-ready 1789→545 ms (profile), bench startup 2121→1219 ms.

### H. 2026-06-10 — the real spawn gate: the host's startup queries must be ANSWERED (startup 142 ms, beats WT)
Wire-dumping the master stream killed §G's pwsh theory. On attach, the pseudoconsole host sends
startup queries to the terminal and **stalls the console session ~1.1 s per unanswered query**
(its timeout) — stalling some shells inside `CreateProcessW` (pwsh 7's console-heavy init) and
others at their first console call (cmd ran its first instruction at ~1.2 s):

- in-box conhost asks `ESC[6n` (DSR, INHERIT_CURSOR's cursor query) — the same query that froze
  the headless probe (§A);
- **OpenConsole 1.24 asks `ESC[6n` AND `ESC[c` (DA1, device attributes)**, then `ESC[?1004h` +
  `ESC[?9001h`. Answering only the DSR moves nothing: the host ACKs it (`ESC[1;1H`) and then
  sits on the unanswered DA1 until t≈1.17 s.

The GUI widget does answer DSR/DA (`take_replies`) but only once the render pump feeds it —
hundreds of ms after spawn, and at startup the window doesn't exist yet. Eliminated empirically
along the way: exe PATH search (full-path pwsh identical), the custom env block (inheriting
changed nothing), and RESIZE_QUIRK (an immediate post-spawn resize changed nothing).

**Fix (core, `session/pty.rs`):** the pty reader thread now starts BEFORE `spawn_command` and
answers the handshake inline — `ESC[6n` → `ESC[1;1R`, `ESC[c` → `ESC[?6c` — stripping the query
bytes so the widget can't send a late duplicate reply that would reach the shell as stray input.
Scanning is bounded (first 512 bytes, ≤3-byte cross-chunk carry, Windows-only) so child output
is never delayed and a child's own later queries still reach the widget for true-cursor answers.

**Measured (child-runs-at, app launch → first child instruction):** bundled 1.24 host
1231→**78 ms**; in-box 86 ms. Bench startup (3-run): hyperpanes **142 ms** vs WT 313 vs
Alacritty 517 — from 2121 ms two days of fixes ago, and now the fastest of the three. pwsh's
`CreateProcessW` no longer stalls (same handshake gate). The window itself appears at ~540 ms
(wgpu device init — the only remaining startup block); the shell is already live behind it.
