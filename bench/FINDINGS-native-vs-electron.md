# Native vs Electron — first idle-memory comparison

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
- ⚠️ **Idle CPU: native ~13% vs Electron ~0%.** The native app's continuous 8 ms (~125 Hz) render
  pump burns CPU at idle even with nothing changing; Electron only repaints on change. A clear
  optimization target (idle-throttle the pump / repaint-on-dirty).

**Bottom line:** v0.0.1 already delivers the resident-footprint + process-count win the rewrite was
for (−37% working set, 3 vs 6 processes), but committed memory is currently a wash (GPU buffers) and
idle CPU regressed (always-on pump). The memory win is real on the headline metric; the next wins are
trimming GPU/private commit and throttling the idle render pump. Native is also early (v0.0.1,
unoptimized) vs a mature Electron v0.1.8.

## Caveats

- Idle-only single snapshots on one machine — not medians; native idle CPU varied 5–13% across samples.
- Native can't yet be driven (no in-pane CLI workload), so throughput/startup aren't compared here.
- WS sums shared pages per-process (inflates Electron's multi-proc total); Private Bytes is the
  truer committed-cost metric. Read both rows together.
