# Native vs Electron (idle memory) + native vs real-world terminals (driven)

First run of the ported harness against the **native Rust** hyperpanes (the rewrite) vs the
pre-rewrite **Electron** build, to answer: *did the rewrite deliver the memory win?*

- **Machine:** DESKTOP-RDPOAEP — Windows 11 (10.0.26100), Intel i9-9900K, 16 cores
- **Date:** 2026-06-09
- **Native:** v0.0.1, `cargo build --release` (`rs/crates/app`)
- **Electron baseline:** v0.1.8, `archive/electron` worktree, dev build (`electron out/main/index.js`)
- **Method:** each launched as a fresh **isolated** instance (native: throwaway `%APPDATA%`;
  Electron: `--user-data-dir <temp>`), one default-shell pane, settled, then a `Win32_Process`
  tree-walk summed Working Set + Private Bytes + idle CPU. Idle-only (the native GUI v0.0.1 has no
  run-a-command CLI, so it can't be driven for throughput/startup). Reproduce:
  `node bench/run.mjs --only=hyperpanes,hyperpanes-electron,wt --suite=memory --idle-bare`.

## Results (idle, fresh instance + one default-shell pane)

| App | Idle WS (MB) | Idle Private (MB) | Idle CPU (%) | Procs |
| --- | ---: | ---: | ---: | ---: |
| **hyperpanes (native) v0.0.1** | **308.5** | 308.6 | ~13 | **3** |
| hyperpanes (Electron) v0.1.8 | 486.9 | 282.2 | ~0 | 6 |
| Windows Terminal | — | — | — | — |

Windows Terminal can't be tree-walked: `wt.exe` is a launcher stub that hands off to a shared
`WindowsTerminal.exe` host (not a child of the spawned PID), so its tree reads empty. (Known caveat.)

### Per-process composition (representative settled sample)

```
native (3 procs):                         Electron (6 procs):
  hyperpanes.exe   185 WS / 249 priv        electron.exe ×4  363 WS / 214 priv  (main+GPU+renderer+utility)
  pwsh.exe         114 WS /  56 priv        pwsh.exe         112 WS /  56 priv  } the shell pane —
  conhost.exe        8 WS /   1 priv        conhost.exe        8 WS /   1 priv  } common to both
```

The `pwsh + conhost` shell pane (~123 MB WS) is the workload, present in both — so the **app
framework** comparison is: native **one process ~185 MB WS** vs Electron **four processes ~363 MB WS**.

## Verdict: a real win on working set + process count; a wash on committed memory

- ✅ **Working Set: native is ~37% lower (308 vs 487 MB, −178 MB)** and runs **3 processes vs 6.**
  The single-process native app roughly **halves the framework resident footprint** (185 vs 363 MB)
  — this is the figure Task Manager shows and what users "feel," and it is the headline win.
- ⚠️ **Private Bytes (committed): native is ~9% HIGHER (309 vs 282 MB).** The single-process wgpu/GPU
  renderer commits about as much memory (249 MB private in one process) as Electron's *entire*
  multi-process tree (214 MB across four). WS double-counts shared DLL pages across Electron's procs,
  so part of the WS gap is accounting, not freed memory — by the stricter private-commit metric the
  rewrite is **not** lighter yet.
- ⚠️ **Idle CPU: native originally ~13% vs Electron ~0%.** The native app's continuous 8 ms
  (~125 Hz) render pump burned CPU at idle even with nothing changing; Electron only repaints on
  change. **Partially addressed (2026-06-09):** the pump no longer pushes unchanged Slint props
  every tick (which forced a wgpu re-render at the pump rate) — release idle CPU now samples ~5–8%.
  This is correct-by-construction but the bench's 2 s CPU snapshot is too noisy to prove the delta
  (post-fix 4.7–7.8% overlaps the pre-fix 5–13% range), and native idle CPU is still the highest of
  the real-world group (see below) — more pump/idle-throttle work remains.

**Bottom line:** v0.0.1 already delivers the resident-footprint + process-count win the rewrite was
for (−37% working set, 3 vs 6 processes), but committed memory is currently a wash (GPU buffers) and
idle CPU regressed (always-on pump). The memory win is real on the headline metric; the next wins are
trimming GPU/private commit and throttling the idle render pump. Native is also early (v0.0.1,
unoptimized) vs a mature Electron v0.1.8.

## Native vs real-world terminals (driven — added 2026-06-09)

The native GUI now wires CLI launch (`hyperpanes --shell cmd.exe -c <wrapper> …`), so it can be
**driven** like the other terminals — real throughput + startup, not just idle memory. Run against
the installed competitors that expose a run-a-command flag (WezTerm/Rio/ConEmu aren't installed on
this box): `node bench/run.mjs --only=hyperpanes,wt,alacritty,hyper --idle-bare --runs=3`. Release
binary; `--idle-bare` makes the memory column apples-to-apples (every app idle) while throughput +
startup still run driven.

### Throughput (MB/s, median of 3 — higher is better)

| Case | hyperpanes (native) | Windows Terminal | Alacritty |
| --- | ---: | ---: | ---: |
| dense | 10.2 | **107.1** | 18.5 |
| scrolling | 7.5 | **79.7** | 11.6 |
| scrolling-region | **0.4** ⚠ | **33.0** | n/a |
| alt-screen | **31.4** | 60.5 | 14.8 |
| unicode | 9.8 | **54.3** | 13.2 |
| cursor-motion | 17.1 | **39.0** | 17.0 |

### Startup (ms — "process launch → command running in a pane", lower is better)

| hyperpanes (native) | Windows Terminal | Alacritty |
| ---: | ---: | ---: |
| 1380 | **239** | 418 |

### Idle (fresh instance + one default-shell pane, `--idle-bare`)

| App | Idle WS (MB) | Idle CPU (%) | Procs |
| --- | ---: | ---: | ---: |
| hyperpanes (native) v0.0.1 | 308 | ~5–8 | 3 |
| Windows Terminal | — | — | — |
| Alacritty 0.17 | **198** | ~2 | 3 |
| Hyper 3.x (Electron) | 396 | ~0 | 6 |

(Windows Terminal is untrackable — `wt.exe` is a launcher stub whose host isn't a child of the
spawned PID, so the tree reads empty. Known caveat.)

### Verdict vs real-world terminals

- **Throughput: native is the weakest of the driven three** on most cases (it does win alt-screen,
  beating Alacritty). Windows Terminal's Atlas GPU text engine is **5–10× faster** on dense/scrolling.
  The glaring outlier is **scrolling-region: native 0.4 MB/s — ~80× slower than WT (33) and far below
  everything else.** That's an isolated bug in the native terminal-widget's scroll-region (DECSTBM)
  path, not a broad slowness, and it is the single highest-value perf fix.
- **Startup: native is the slowest (1380 ms vs WT 239 / Alacritty 418).** It pays a full GUI realize
  + deferred first-pane seed + a `cmd.exe -c` spawn before the command runs; the lean native
  terminals are up in a fraction of that. (The metric includes the in-pane shell spawn, a small
  constant the leaner terminals also pay — but the gap is real.)
- **Idle memory: native (308 MB) sits between lean Alacritty (198) and Electron-based Hyper (396)** —
  lighter than an Electron terminal, heavier than a minimal wgpu one. Consistent with the
  native-vs-Electron section: the win is process count + working set vs Electron, not absolute leanness.
- **Idle CPU: native (~5–8%) is still the highest** of the group (Alacritty ~2%, WT/Hyper ~0). The
  #2 pump fix landed but more idle-throttle work remains.

**Bottom line vs competitors:** at **v0.0.1** the native app is **memory-competitive** (beats
Electron terminals, near the GPU-native pack) but **clearly behind on throughput and startup** — a
young, unoptimized app against mature terminals. Actionable targets, in priority order: (1) the
scrolling-region throughput bug (catastrophic + isolated), (2) startup latency, (3) idle CPU.

## Caveats

- Idle-only single snapshots on one machine — not medians; native idle CPU varied 5–13% across samples.
- The native-vs-Electron section is idle-only (the Electron baseline is measured idle for parity).
  Native **can** now be driven (CLI launch wired 2026-06-09) — see the real-world section below for
  throughput/startup.
- WS sums shared pages per-process (inflates Electron's multi-proc total); Private Bytes is the
  truer committed-cost metric. Read both rows together.
- Real-world throughput/startup are medians of 3 runs on one machine; throughput is the PTY-
  backpressure proxy (a terminal that buffers a big PTY read can ack before rendering, under-
  reporting render cost), not a pixel-accurate timer. Native is **v0.0.1, unoptimized** vs mature
  competitors — read the gaps as "where to optimize," not a fixed verdict.
- Only the installed terminals with a run-a-command flag are driven (here: native, WT, Alacritty).
  WezTerm/Rio/ConEmu weren't installed; Hyper/Tabby/Wave have no such flag (idle-only); kitty/
  Ghostty have no Windows build.
