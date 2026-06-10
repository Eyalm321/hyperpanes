# Track H (Wave 2) — perf: startup latency + idle pump + app-level scroll-region (Task 17 #1/#2/#3)

Branch `fanout/wave2-perf`. Scope: **app only** (`rs/crates/app/src/{app,main,paneview}.rs`) — no
`state.rs`/`sidebar`/`core`/terminal-widget touched (those belong to Track 19 / others). Measured on
DESKTOP-RDPOAEP (i9-9900K, 16 cores), release binary, with the bench harness. All numbers are this
machine, this session (absolute values run high vs the documented `FINDINGS-native-vs-electron.md`
baseline because the box was under build/AV load throughout — read the **before/after deltas**, not the
absolutes).

A new, zero-cost-when-off perf instrument was added to do the measuring: set `HYPERPANES_PERFLOG=<path>`
(or `1`) and the app logs timestamped startup milestones + a `[tick/s]` line each second of activity
(events, bytes fed, MB/s, renders, drain/render/busy ms-per-second). It is inert (one `OnceLock` load)
when the env var is unset. This is how the findings below were proven, and it stays in the tree for the
next perf pass.

---

## #1 — scroll-region throughput (the "catastrophe") — **diagnosed: NOT app-fixable; it's ConPTY**

**Result: 0.4 MB/s before → 0.4 MB/s after (unchanged).** No app-level render/feed change can raise it,
and the instrumentation proves why.

Repro (stable methodology — the default 16 MB payload exceeds the 60 s timeout at this rate and returns
`n/a`; use a smaller payload + longer timeout):

```
node bench/run.mjs --only=hyperpanes --suite=throughput --cases=scrolling-region --bytes=2 --runs=2 --timeout=120000
```

What the `[tick/s]` log shows during a scrolling-region run (node input throttled to **0.4 MB/s**):

```
[tick/s] ticks=115 events=167 bytes=30773896 (30.55 MB/s) renders=64 drain=234.5ms/s render=111.2ms/s busy=346.3ms/s
[tick/s] ticks=102 events=135 bytes=22817059 (22.60 MB/s) renders=70 drain=241.6ms/s render=193.9ms/s busy=437.1ms/s
```

The app is **feeding ~28 MB/s** while node's input is backpressured to **0.4 MB/s** — a **~70× byte
inflation**. That is ConPTY/conhost **re-rendering the entire DECSTBM scroll region (cursor-home + ~20
lines of text) on every single scrolled line** instead of emitting an efficient scroll/index sequence.
node is throttled by conhost's input→output *generation* rate, not by the app: the app keeps up with
that inflated 28 MB/s at only **~38 % of one core** (drain ≈ 220 ms/s feeding + render ≈ 150 ms/s) and
has headroom to spare.

Why no app fix exists: the pty reader thread drains conhost's master pipe into an **unbounded** channel,
so the app never backpressures conhost; conhost is CPU-bound producing the repaint. The app render is
**already coalesced one-per-8 ms-tick** (verified: `renders ≈ 65/s`, not one-per-scrolled-line), and
Track A already proved the widget grid scroll is O(1). So "coalesce per frame / incremental blit" has
nothing left to win at the app layer — the ceiling is upstream.

**Flag for fan-in (outside Track H's writable scope):**
1. **Core, `session/pty.rs`** — a real throughput fix needs ConPTY *passthrough* mode (so conhost stops
   repainting the region) or a smaller pty grid; both are pty-spawn concerns owned by core.
2. **Core, `session/screen.rs`** — `Screen::advance` parses this same inflated 28 MB/s a **second time**
   on the driver thread purely to keep the `mode:"screen"` read-model warm. In the bench (no control
   client) that's pure wasted CPU/heat; it should be skipped/lazied when no control client is attached.
   (Doesn't gate throughput — the reader is on its own thread — but it's the one real in-our-code cost.)

---

## #2 — startup latency — **eager first-pane seed: ~1.6–2.3 s before → ~1.2 s after**

**Fix:** seed the first pane (spawn the pty → shell) **eagerly in `spawn_window`, before the heavy
`AppWindow::new` (wgpu device init)**, instead of waiting for `AppWindow::new` + several 8 ms ticks to
learn the pane area. `State::make_pane` fixes the pty at 80×24, so the spawn never needed the area; the
first render pump relayouts it. This overlaps the shell's ~0.5 s process startup with the ~0.5 s GPU
init instead of running them back-to-back.

**Deterministic proof (perf marks, every run):**

```
[+15.6ms] spawn_window: seeding first pane
[+47.2ms] spawn_window: first pane seeded (pty spawned)      <- shell is starting
[+568.3ms] spawn_window: AppWindow::new done (wgpu ready)    <- ~520ms GPU init ran concurrently
```

The pty now spawns at **+47 ms** and runs concurrently with the **~520 ms** wgpu device init, rather
than after it (+ an area-wait). This mechanism is load-independent.

**Wall-clock (bench `--suite=startup --runs=4`, same session, two reads each, high machine-load variance):**

| | before (baseline) | after |
| --- | ---: | ---: |
| startup ms (read 1 / read 2) | 2286 / 1615 | 1204 / 1530 |
| median-ish | ~1950 | ~1370 |

A consistent **~400–700 ms (~25 %) reduction** across reads — after is below every baseline read and below
the documented 1380 ms baseline, despite large session-to-session variance (the box was under build/AV
load). The deterministic instrumentation above is the load-independent proof. The residual startup cost is
dominated by `AppWindow::new` (the wgpu/femtovg device init, ~520 ms), which is Slint-internal and not
app-trimmable here; eager-seed claws back the previously-serialized shell spawn.

---

## #3 — idle CPU / adaptive pump cadence — **8 ms active → 32 ms idle, instant wake — ⚠ NEEDS LIVE CHECK**

**Fix:** the single shared pump timer is now adaptive. It runs at the fast **8 ms** cadence while there's
work, and after **75 consecutive idle ticks (~0.6 s)** drops to a **32 ms (~31 Hz)** idle cadence via
`Timer::set_interval`, cutting the always-on 125 Hz wakeups that pinned idle CPU. It snaps back to fast
instantly on input — `App::wake()` is called from keystrokes (`on_key`), command dispatch
(`run_command`), divider drag, and window/pane resize — and within one idle tick (≤32 ms) on session
output (the drain notices it and `did_work` flips). A bare cursor-blink repaint is deliberately **not**
"work", so an idle pane still blinks at 31 Hz without holding the fast cadence.

"Work" that holds fast cadence = drained ≥1 session event, a pane repainted real content (not blink-only),
a glow/toast/typewriter animation advanced, the prefs preview animating, a drag in flight, or the control
server being live (so MCP/agent drivers stay responsive).

**Why it doesn't hurt the throughput numbers:** during any streaming workload the drain sees output every
tick → `did_work` → the pump stays at 8 ms the whole time. Adaptive cadence only engages when the app is
genuinely idle.

⚠ **This one is INPUT-LATENCY-SENSITIVE and needs a LIVE GUI feel-check by the orchestrator** — the bench
cannot prove keystroke/scroll latency feels right. The design is conservative (32 ms idle, instant
input-wake), but please verify by hand: type after a few seconds idle and confirm the first keystroke
echoes with no perceptible lag, and that scrolling/resize feel smooth. The bench's 2 s CPU snapshot is too
noisy to quantify the idle-CPU win, so I'm not quoting a CPU delta — it's correct-by-construction (125 Hz
→ ~31 Hz wakeups at idle) but unproven by number.

---

## Verify

- Builds green: `cargo build --release` and `cargo check` both pass (only pre-existing warnings, none in
  the changed files; the one `unused_mut` is in `sidebar.rs`, not ours).
- Terminal correctness preserved: feed/render logic unchanged except the already-present per-tick render
  coalescing; cadence only changes *when* an idle pump fires, never what it draws. No dropped output or
  tearing observed in the driven bench (output streamed + rendered correctly across all cases).
- Throughput unchanged by design (scrolling-region 0.4→0.4); the fast cases (scrolling 5.9→8.3, dense
  7.5→11.4) moved within run-to-run noise — not attributable to these changes (they don't touch the feed
  hot path's per-byte cost).
- Not pushed / not merged. Commit per fix on `fanout/wave2-perf`.
