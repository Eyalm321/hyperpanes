# hyperpanes terminal benchmark harness

Measures **hyperpanes** against other Windows terminals on the same machine, so the
"optimize within Electron vs. rewrite" decision (and future regressions) rest on
numbers rather than estimates. It is **detect-only**: it never installs, updates, or
otherwise mutates your system — it benchmarks whatever is already installed.

Three automated suites — **throughput**, **startup**, **memory** — plus a **manual**
input-latency procedure (Typometer) documented below.

## Quick start

```powershell
# 1. See what's installed (writes bench/results/terminals.json)
npm run bench:detect

# 2. Run the suites (writes bench/results/report.md + <label>.json)
npm run bench -- --only=hyperpanes,wezterm --suite=throughput,startup,memory --runs=3
```

`npm run bench` with no flags benchmarks every installed terminal with all suites at
`--runs=5`. Output lands in `bench/results/` (gitignored).

### Requirements for the hyperpanes rows

hyperpanes has **no `hyperpanes` command on PATH**, so the harness resolves the
installed `Hyperpanes.exe` (in `%LOCALAPPDATA%\Programs\Hyperpanes\`) — build/install
it with `npm run pack:win` — **or** set `HYPERPANES_DEV=1` to drive the dev build at
`out/main/index.js` (run `npm run build` first):

```powershell
$env:HYPERPANES_DEV = '1'; npm run bench -- --only=hyperpanes --suite=memory --runs=3
```

> **Close any running hyperpanes window first — installed *or* dev.** hyperpanes uses
> a single-instance lock: a second launch forwards its args to the running instance
> and exits, so the harness would measure/kill the wrong process. The harness
> pre-flights this (checking for `Hyperpanes.exe` in installed mode, or a repo dev
> `electron` instance in `HYPERPANES_DEV=1` mode) and skips with a message if one is
> already up.

## Flags

| Flag | Default | Meaning |
| --- | --- | --- |
| `--only=ids` | all installed | comma-separated terminal ids (`hyperpanes,wt,wezterm,alacritty,rio,conemu,tabby,hyper,wave`) |
| `--suite=...` | `throughput,startup,memory` | which suites to run |
| `--runs=N` | `5` | repetitions per measurement (median kept) |
| `--cases=...` | all six | throughput cases (`dense,scrolling,scrolling-region,alt-screen,unicode,cursor-motion`) |
| `--bytes=MB` | `16` | throughput payload size per case |
| `--lines=N` | `200000` | scrollback lines for the memory "after-load" phase |
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
  PID, summing Working Set (primary) and Private Bytes (overhead), sampled at idle and
  again after filling scrollback. hyperpanes is additionally cross-checked against its
  own in-app number (see below).

### Cross-checking hyperpanes memory

The harness's external tree-walk is cross-checked against hyperpanes' own metrics:
launch hyperpanes, run **"Performance: Dump metrics"** from the command palette, and
compare its `totalMemoryMB` to the harness's idle Working-Set figure — they should be
within a sane factor. (This is a manual step in v1: `metrics()` is a renderer API with
no external call path unless the off-by-default control server is enabled.)

## How invocation works (and why)

Terminals receive the workload differently. `-e`-style terminals take an argv array;
hyperpanes takes a single command *string* it re-parses through a shell. To get one
reliable quoting path, the harness writes a per-run `.cmd` wrapper (cmd-native quoting
is robust for the spaces-in-path `node.exe`) and every terminal runs `cmd /c
<wrapper>`. The wrapper invokes the harness's own Node (`process.execPath`) so every
terminal runs an identical interpreter.

## Fairness caveats

1. **Backpressure proxy.** A terminal that buffers a large PTY read can ack bytes
   before rendering, under-reporting its render cost. vtebench shares this; it's the
   accepted proxy.
2. **Startup constant.** Includes a constant Node-start cost (cancels across
   terminals); hyperpanes also pays a `shell -c` spawn inside the pane.
3. **Memory tree-walk.** May miss a reused host process — **Windows Terminal** shares
   one host across windows and `wt.exe` is a launcher stub that exits, so its memory
   row is unreliable (shown with a note) — or include unrelated windows. Hence the
   hyperpanes `metrics()` cross-check.
4. **Config-only terminals.** Tabby, Hyper and Wave have no run-a-command flag → **idle
   memory only**, labeled "not driven". (They're Electron apps with their own
   single-instance behavior; close them first for an accurate idle figure.)
5. **Latency is manual** (Typometer — see below), not automated.
6. **Kitty / Ghostty** have no Windows build and are excluded.
7. **Environment.** Run on AC power with other apps closed. Results are medians over
   multiple runs but remain machine- and load-dependent.

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
