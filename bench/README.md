# hyperpanes terminal benchmark harness

Measures **hyperpanes** against other Windows terminals on the same machine, so the
"did the native rewrite deliver the memory win?" question (and future regressions) rest
on numbers rather than estimates. It is **detect-only**: it never installs, updates, or
otherwise mutates your system — it benchmarks whatever is already built/installed.

The primary target is the **native Rust** hyperpanes (`rs/crates/app`). The pre-rewrite
**Electron** build is an optional baseline (built from branch `archive/electron`).

Suites: **memory** (idle WS/Private + idle CPU — the headline) for every app, plus
**throughput** and **startup** for terminals that expose a run-a-command CLI. A **manual**
input-latency procedure (Typometer) is documented below.

> **The native hyperpanes is measured idle-only.** Its GUI binary (v0.0.1) ignores CLI
> argv and has no run-a-command flag, so the harness cannot inject an in-pane workload — it
> launches a fresh instance (one default-shell pane) and samples idle memory + CPU.
> Throughput/startup-in-pane are therefore n/a for native until the GUI wires CLI launch
> (the parser + single-instance gate already live in `core` + the headless daemon).

## Quick start

The harness is plain Node, **no dependencies**. Run it via `node` from the repo root, or
via the npm scripts in `bench/` (run them from inside `bench/`):

```powershell
# 1. See what's built/installed (writes bench/results/terminals.json)
node bench/detect.mjs                 # …or:  cd bench; npm run bench:detect

# 2. The headline native-vs-Electron(-vs-WT) idle comparison
node bench/run.mjs --only=hyperpanes,hyperpanes-electron,wt --suite=memory --idle-bare --label=native-vs-electron
```

`npm run bench` with no flags benchmarks every detected terminal with all applicable
suites at `--runs=5`. Output lands in `bench/results/` (gitignored).

### The native hyperpanes row

The native app has **no `hyperpanes` command on PATH**; the harness resolves the cargo
build output. Build it first:

```powershell
cargo build --release --manifest-path rs/crates/app/Cargo.toml
```

It prefers `rs/crates/app/target/release/hyperpanes.exe` and falls back to a `debug`
build (noted in the report — debug is slower/larger and not a fair comparison target).
Each measured native launch gets an **isolated throwaway `%APPDATA%`** so it starts as a
clean fresh instance (the data dir keys on `%APPDATA%`).

### The Electron baseline row (optional)

The installed production app is now the native build (Electron was retired), so the
Electron baseline is built from a worktree of `archive/electron` **next to this repo**:

```powershell
git worktree add ../electron-baseline archive/electron
cd ../electron-baseline; npm ci; npm run build      # → out/main/index.js
```

The harness then runs it in dev mode (the worktree's `electron` binary + `out/main/index.js`),
launched with an isolated `--user-data-dir`, which spawns the real multi-process Electron
tree (main + GPU + renderer + utility helpers) that the proctree walk sums. If the worktree
isn't built, the `hyperpanes (Electron)` row is simply absent (detect shows how to build it).

## Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--only=ids` | all detected | comma-separated terminal ids (`hyperpanes,hyperpanes-electron,wt,wezterm,alacritty,rio,conemu,tabby,hyper,wave`) |
| `--suite=...` | `throughput,startup,memory` | which suites to run |
| `--runs=N` | `5` | repetitions per measurement (median kept) |
| `--cases=...` | all six | throughput cases (`dense,scrolling,scrolling-region,alt-screen,unicode,cursor-motion`) |
| `--bytes=MB` | `16` | throughput payload size per case |
| `--lines=N` | `200000` | scrollback lines for the memory "after-load" phase |
| `--idle-bare` | off | force **every** targeted terminal through the bare idle sample (no in-pane workload) — identical methodology for an apples-to-apples idle memory/CPU comparison |
| `--cpu-ms=N` | `2000` | idle-CPU sample window in ms (`0` disables CPU sampling) |
| `--label=name` | `report` | names the `<label>.json` output |
| `--check-updates` | off | also report (not apply) `winget upgrade` status |

## What each metric means

- **Throughput (MB/s, higher is better).** A Node reimplementation of vtebench's
  streaming model (vtebench itself is Rust+bash, WSL-only). The workload runs *inside*
  the terminal, streams a fixed payload to stdout honoring PTY backpressure, and times
  itself — because a write blocks until the terminal drains/renders, elapsed ≈ render
  throughput. Not byte-identical to vtebench; a representative, internally-consistent
  proxy.
- **Startup (ms, lower is better).** "Process launch → your command is executing in a
  pane." The harness stamps `t0` immediately before spawning; the probe stamps
  `Date.now()` the instant it runs. The constant Node-start cost cancels across
  terminals.
- **Memory (MB, lower is better).** A `Win32_Process` tree-walk from the spawned root
  PID, summing Working Set (primary) and Private Bytes (overhead). For the native app and
  the Electron baseline this is an **idle** sample of a fresh instance (one default-shell
  pane); for driven terminals it is sampled at idle and again after filling scrollback. The
  tree-walk captures Electron's multi-process tree (main + GPU + renderer + utility) and the
  native app's single process — so the comparison is total resident cost, not just the main
  process.
- **Idle CPU (%, lower is better).** Each process's total processor time is diffed over a
  fixed window (`--cpu-ms`, default 2 s) and summed across the tree, expressed as percent of
  one core (so it can exceed 100). A short idle snapshot — sensitive to background
  animation/rendering — not a sustained average.

## How invocation works (and why)

Terminals receive the workload differently. `-e`-style terminals take an argv array;
hyperpanes takes a single command *string* it re-parses through a shell. To get one
reliable quoting path, the harness writes a per-run `.cmd` wrapper (cmd-native quoting
is robust for the spaces-in-path `node.exe`) and every terminal runs `cmd /c
<wrapper>`. The wrapper invokes the harness's own Node (`process.execPath`) so every
terminal runs an identical interpreter.

## Fairness caveats

1. **Native is idle-only.** The native GUI ignores CLI argv and has no run-a-command
   flag, so it can't be driven; only idle memory/CPU of a fresh instance are measured.
   Throughput/startup are n/a for native until the GUI wires CLI launch.
2. **Isolated fresh instances.** Each hyperpanes launch uses an isolated data dir
   (native: throwaway `%APPDATA%`; Electron: `--user-data-dir <temp>`) so it never hands
   off to a running copy. The Electron baseline is the `archive/electron` worktree run in
   dev mode — also idle-only, so the comparison is apples-to-apples.
3. **Memory tree-walk.** Sums Working Set + Private Bytes from the spawned root PID across
   the whole tree (Electron's helpers included). May miss a reused host process —
   **Windows Terminal** shares one host across windows and `wt.exe` is a launcher stub
   that exits, so its row is often empty/unreliable (shown with a note) — or include
   unrelated windows.
4. **Idle CPU** is a short snapshot (`--cpu-ms`), percent-of-one-core summed over the
   tree, not a sustained average.
5. **Backpressure proxy.** For driven terminals, a terminal that buffers a large PTY read
   can ack bytes before rendering, under-reporting render cost. vtebench shares this.
6. **Config-only terminals.** Tabby, Hyper and Wave have no run-a-command flag → idle
   memory/CPU only.
7. **Latency is manual** (Typometer — see below), not automated.
8. **Kitty / Ghostty** have no Windows build and are excluded.
9. **Environment.** Run on AC power with other apps closed. Results remain machine- and
   load-dependent.

## Manual input-latency procedure (Typometer)

Input latency (keypress → glyph on screen) is not automated here. To measure it:

1. Install [Typometer](https://github.com/pavelfatin/typometer) (a screen-capture
   keystroke-to-pixel latency tool).
2. Open the terminal under test, maximized, with a shell at an empty prompt and a
   high-contrast color scheme (Typometer detects glyph changes by luminance).
3. In Typometer, run a measurement of ~200 keystrokes; record the reported average and
   p99 latency (ms).
4. Repeat per terminal under identical window size / font / theme, on AC power with
   other apps closed.
5. Record the figures alongside this harness's `report.md` (Typometer has no headless
   CLI to fold into the automated run).

## Output files (`bench/results/`, gitignored)

- `terminals.json` — detection snapshot (installed y/n + versions).
- `<label>.json` — full structured results for a run.
- `report.md` — human-readable tables + caveats footer.
