// Terminal registry. One entry per terminal describing how to find it, how to read
// its version, and how to launch a workload in it. The orchestrator and detector
// consume this — there is no per-terminal logic outside this file.
//
// Fields:
//   id, name           stable id + display name
//   wingetId           for `--check-updates` (winget upgrade), report-only
//   exeNames[]         candidate executable names (resolved on PATH + searchDirs)
//   searchDirs[]       extra dirs to probe when not on PATH
//   versionArgs        args to print a version (null = don't run the exe; unsafe/UWP)
//   driven             true = full suite (throughput+startup+memory); false = idle-memory only
//   memoryIdleOnly     true = no scrollback-fill phase (config-only terminals)
//   platformUnavailable true = no Windows build (listed n/a, never launched)
//   procMatch[]        image names used for orphan/instance checks
//   suites             which suites apply
//   resolveExe()       -> absolute exe path | null
//   launch(ctx)        ctx={ wrapperPath, cwd, label } -> { exe, args } (config-only ignores wrapperPath)

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { REPO_ROOT, makeTempDataDir } from './lib/launch.mjs';

const LOCALAPPDATA = process.env.LOCALAPPDATA || join(process.env.USERPROFILE || 'C:\\', 'AppData', 'Local');
const PROGRAMFILES = process.env.ProgramFiles || 'C:\\Program Files';
const PROGRAMFILESX86 = process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)';

const DRIVEN_SUITES = ['throughput', 'startup', 'memory'];

function whichExe(name) {
  const res = spawnSync('where', [name], { encoding: 'utf8' });
  if (res.status === 0 && res.stdout) {
    const first = res.stdout
      .split(/\r?\n/)
      .map((s) => s.trim())
      .filter(Boolean)[0];
    // Trust `where`'s resolution. `existsSync` returns false for App Execution Aliases (the
    // WindowsApps `wt.exe` is a 0-byte reparse point) even though they are launchable on PATH,
    // so an `existsSync` gate would wrongly drop Windows Terminal.
    if (first) return first;
  }
  return null;
}

function findInDirs(exeNames, dirs) {
  for (const dir of dirs) {
    for (const n of exeNames) {
      const p = join(dir, n);
      if (existsSync(p)) return p;
    }
  }
  return null;
}

function resolver(exeNames, searchDirs = []) {
  return () => {
    for (const n of exeNames) {
      const w = whichExe(n);
      if (w) return w;
    }
    return findInDirs(exeNames, searchDirs);
  };
}

// `cmd /c <wrapper>` argv used by every -e-style driven terminal.
const CMD_RUN = (wrapperPath) => ['cmd', '/c', wrapperPath];

// ---- hyperpanes (native Rust) ----

// The native GUI binary built by `cargo build --release --manifest-path rs/crates/app/Cargo.toml`.
// Prefer release; fall back to a debug build if that's all that exists (noted in the report).
const NATIVE_RELEASE = join(REPO_ROOT, 'rs', 'crates', 'app', 'target', 'release', 'hyperpanes.exe');
const NATIVE_DEBUG = join(REPO_ROOT, 'rs', 'crates', 'app', 'target', 'debug', 'hyperpanes.exe');

function resolveNativeExe() {
  if (existsSync(NATIVE_RELEASE)) return NATIVE_RELEASE;
  if (existsSync(NATIVE_DEBUG)) return NATIVE_DEBUG;
  return null;
}

// Native app version comes from the crate's Cargo.toml (running the exe with an unknown flag would
// just open a window — the GUI ignores argv).
function nativeVersion() {
  try {
    const toml = readFileSync(join(REPO_ROOT, 'rs', 'crates', 'app', 'Cargo.toml'), 'utf8');
    const m = toml.match(/^\s*version\s*=\s*"([^"]+)"/m);
    return m ? m[1] : '?';
  } catch {
    return '?';
  }
}

// ---- hyperpanes (Electron baseline) ----
//
// The installed production app is now the NATIVE build (Electron was retired), so the Electron
// baseline comes from a `git worktree` of branch `archive/electron` built next to this one:
//   git worktree add ../electron-baseline archive/electron && (cd ../electron-baseline && npm ci && npm run build)
// We then run it in dev mode — the worktree's `electron` binary + `out/main/index.js` — which spawns
// the real multi-process Electron tree (main + GPU + renderer + utility helpers) the proctree sums.
const ELECTRON_WT = join(REPO_ROOT, '..', 'electron-baseline');
const ELECTRON_EXE = join(ELECTRON_WT, 'node_modules', 'electron', 'dist', 'electron.exe');
const ELECTRON_MAIN = join(ELECTRON_WT, 'out', 'main', 'index.js');

function electronVersion() {
  try {
    return JSON.parse(readFileSync(join(ELECTRON_WT, 'package.json'), 'utf8')).version || '?';
  } catch {
    return '?';
  }
}

// ---- Windows Terminal version ----
//
// `wt.exe --version` has no stdout output — it OPENS the GUI About dialog. Probing it that way
// pops a window on every `detect.mjs` (which runs on every `run.mjs`, even `--only=hyperpanes`).
// Read the version from the installed Store package instead — no UI, best-effort.
function wtVersion() {
  try {
    const res = spawnSync(
      'powershell.exe',
      ['-NoProfile', '-NonInteractive', '-Command', '(Get-AppxPackage Microsoft.WindowsTerminal).Version'],
      { encoding: 'utf8', timeout: 8000 }
    );
    return (res.stdout || '').split(/\r?\n/).map((s) => s.trim()).filter(Boolean)[0] || '?';
  } catch {
    return '?';
  }
}

export const TERMINALS = [
  {
    // The NATIVE Rust app (the rewrite). The GUI binary ignores CLI argv and has no
    // run-a-command flag, so — like the config-only terminals — it is launched bare and reports
    // IDLE memory/CPU of a fresh instance (one default-shell pane). Throughput/startup-in-pane
    // need command injection the native GUI v0.0.1 does not yet wire (only core + the headless
    // daemon parse argv), so those suites are n/a for native. Idle memory is the headline.
    id: 'hyperpanes',
    name: 'hyperpanes (native)',
    wingetId: null,
    exeNames: ['hyperpanes.exe'],
    searchDirs: [],
    versionArgs: null,
    fixedVersion: nativeVersion,
    driven: false,
    memoryIdleOnly: true,
    procMatch: ['hyperpanes.exe'],
    suites: ['memory'],
    resolveExe: resolveNativeExe,
    // Launch the native exe bare with an ISOLATED %APPDATA% so it starts as a clean fresh
    // instance (no restored session/prefs; data dir keys on %APPDATA%). `temp` is cleaned up by
    // the caller after sampling.
    launch() {
      const dataDir = makeTempDataDir('hpbench-native');
      return { exe: this.resolveExe(), args: [], env: { APPDATA: dataDir }, temp: [dataDir + '\\'] };
    },
    // True iff a native build exists (release preferred, debug fallback).
    isAvailable() {
      return !!resolveNativeExe();
    },
    // Note whether we fell back to a debug build (release is the fair comparison target).
    buildNote() {
      const exe = resolveNativeExe();
      if (!exe) return '';
      return exe === NATIVE_DEBUG ? 'DEBUG build (release not found — slower/larger; rebuild with --release)' : '';
    }
  },
  {
    // The pre-rewrite Electron build, as the memory baseline. The INSTALLED app launched bare with
    // an isolated --user-data-dir gives a fresh instance (and its own single-instance lock keyed on
    // that dir, so a running installed copy won't capture it). Idle-only, to compare apples-to-apples
    // with the native idle figure (both: fresh instance, one default pane, no in-pane workload).
    id: 'hyperpanes-electron',
    name: 'hyperpanes (Electron)',
    wingetId: null,
    exeNames: ['electron.exe'],
    searchDirs: [],
    versionArgs: null,
    fixedVersion: electronVersion,
    driven: false,
    memoryIdleOnly: true,
    procMatch: ['electron.exe'],
    suites: ['memory'],
    // Available only once the archive/electron worktree is built (both the electron binary AND the
    // built main entry must exist).
    resolveExe: () => (existsSync(ELECTRON_EXE) && existsSync(ELECTRON_MAIN) ? ELECTRON_EXE : null),
    launch() {
      const dataDir = makeTempDataDir('hpbench-electron');
      // electron <main.js> --user-data-dir <temp>  → a fresh isolated Electron instance.
      return { exe: this.resolveExe(), args: [ELECTRON_MAIN, '--user-data-dir', dataDir], temp: [dataDir + '\\'] };
    }
  },
  {
    id: 'wt',
    name: 'Windows Terminal',
    wingetId: 'Microsoft.WindowsTerminal',
    exeNames: ['wt.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Microsoft', 'WindowsApps')],
    // No `--version` probe: `wt.exe --version` opens the GUI About dialog. Use the Appx package.
    versionArgs: null,
    fixedVersion: wtVersion,
    driven: true,
    procMatch: ['wt.exe', 'WindowsTerminal.exe', 'OpenConsole.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['wt.exe'], [join(LOCALAPPDATA, 'Microsoft', 'WindowsApps')]),
    launch({ wrapperPath }) {
      // Bare form (wrapperPath null, e.g. `--idle-bare`): open a new window with the default
      // profile shell so WT can serve as an idle-memory reference alongside the hyperpanes apps.
      if (!wrapperPath) return { exe: this.resolveExe(), args: ['-w', 'new'] };
      return { exe: this.resolveExe(), args: ['-w', 'new', '--', ...CMD_RUN(wrapperPath)] };
    }
  },
  {
    id: 'wezterm',
    name: 'WezTerm',
    wingetId: 'wez.wezterm',
    exeNames: ['wezterm.exe'],
    searchDirs: [join(PROGRAMFILES, 'WezTerm'), join(LOCALAPPDATA, 'Programs', 'WezTerm')],
    versionArgs: ['--version'],
    driven: true,
    procMatch: ['wezterm.exe', 'wezterm-gui.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['wezterm.exe'], [join(PROGRAMFILES, 'WezTerm'), join(LOCALAPPDATA, 'Programs', 'WezTerm')]),
    launch({ wrapperPath }) {
      // --always-new-process avoids the wezterm mux reusing an existing GUI process.
      // Bare form (wrapperPath null, e.g. `--idle-bare`): open a default-shell window.
      if (!wrapperPath) return { exe: this.resolveExe(), args: ['start', '--always-new-process'] };
      return { exe: this.resolveExe(), args: ['start', '--always-new-process', '--', ...CMD_RUN(wrapperPath)] };
    }
  },
  {
    id: 'alacritty',
    name: 'Alacritty',
    wingetId: 'Alacritty.Alacritty',
    exeNames: ['alacritty.exe'],
    searchDirs: [join(PROGRAMFILES, 'Alacritty')],
    versionArgs: ['--version'],
    driven: true,
    procMatch: ['alacritty.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['alacritty.exe'], [join(PROGRAMFILES, 'Alacritty')]),
    launch({ wrapperPath }) {
      // Bare form (wrapperPath null, e.g. `--idle-bare`): no `-e`, so a default-shell window.
      if (!wrapperPath) return { exe: this.resolveExe(), args: [] };
      return { exe: this.resolveExe(), args: ['-e', ...CMD_RUN(wrapperPath)] };
    }
  },
  {
    id: 'rio',
    name: 'Rio',
    wingetId: 'raphamorim.rio',
    exeNames: ['rio.exe'],
    searchDirs: [join(PROGRAMFILES, 'Rio'), join(LOCALAPPDATA, 'Programs', 'Rio')],
    versionArgs: ['--version'],
    driven: true,
    procMatch: ['rio.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['rio.exe'], [join(PROGRAMFILES, 'Rio'), join(LOCALAPPDATA, 'Programs', 'Rio')]),
    launch({ wrapperPath }) {
      // Bare form (wrapperPath null, e.g. `--idle-bare`): no `-e`, so a default-shell window.
      if (!wrapperPath) return { exe: this.resolveExe(), args: [] };
      return { exe: this.resolveExe(), args: ['-e', ...CMD_RUN(wrapperPath)] };
    }
  },
  {
    id: 'conemu',
    name: 'ConEmu',
    wingetId: 'Maximus5.ConEmu',
    exeNames: ['ConEmu64.exe', 'ConEmu.exe'],
    searchDirs: [join(PROGRAMFILES, 'ConEmu'), join(PROGRAMFILESX86, 'ConEmu')],
    versionArgs: null, // no clean stdout version
    driven: true,
    procMatch: ['ConEmu64.exe', 'ConEmu.exe', 'ConEmuC64.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['ConEmu64.exe', 'ConEmu.exe'], [join(PROGRAMFILES, 'ConEmu'), join(PROGRAMFILESX86, 'ConEmu')]),
    launch({ wrapperPath }) {
      // Bare form (wrapperPath null, e.g. `--idle-bare`): no `-run`, so a default-task window.
      if (!wrapperPath) return { exe: this.resolveExe(), args: [] };
      return { exe: this.resolveExe(), args: ['-run', ...CMD_RUN(wrapperPath)] };
    }
  },
  // ---- Config-only (no run-a-command CLI flag): idle memory only ----
  {
    id: 'tabby',
    name: 'Tabby',
    wingetId: 'Eugeny.Tabby',
    exeNames: ['Tabby.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Programs', 'Tabby'), join(PROGRAMFILES, 'Tabby')],
    versionArgs: null,
    driven: false,
    memoryIdleOnly: true,
    procMatch: ['Tabby.exe'],
    suites: ['memory'],
    resolveExe: resolver(['Tabby.exe'], [join(LOCALAPPDATA, 'Programs', 'Tabby'), join(PROGRAMFILES, 'Tabby')]),
    launch() {
      return { exe: this.resolveExe(), args: [] };
    }
  },
  {
    id: 'hyper',
    name: 'Hyper',
    wingetId: 'vercel.hyper',
    exeNames: ['Hyper.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Programs', 'Hyper'), join(PROGRAMFILES, 'Hyper')],
    versionArgs: null,
    driven: false,
    memoryIdleOnly: true,
    procMatch: ['Hyper.exe'],
    suites: ['memory'],
    resolveExe: resolver(['Hyper.exe'], [join(LOCALAPPDATA, 'Programs', 'Hyper'), join(PROGRAMFILES, 'Hyper')]),
    launch() {
      return { exe: this.resolveExe(), args: [] };
    }
  },
  {
    id: 'wave',
    name: 'Wave',
    wingetId: 'CommandLine.Wave',
    exeNames: ['Wave.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Programs', 'waveterm'), join(LOCALAPPDATA, 'Programs', 'Wave')],
    versionArgs: null,
    driven: false,
    memoryIdleOnly: true,
    procMatch: ['Wave.exe'],
    suites: ['memory'],
    resolveExe: resolver(['Wave.exe'], [join(LOCALAPPDATA, 'Programs', 'waveterm'), join(LOCALAPPDATA, 'Programs', 'Wave')]),
    launch() {
      return { exe: this.resolveExe(), args: [] };
    }
  },
  // ---- Excluded on Windows (no native build) ----
  {
    id: 'kitty',
    name: 'kitty',
    wingetId: null,
    exeNames: [],
    versionArgs: null,
    driven: false,
    platformUnavailable: true,
    suites: [],
    resolveExe: () => null
  },
  {
    id: 'ghostty',
    name: 'Ghostty',
    wingetId: null,
    exeNames: [],
    versionArgs: null,
    driven: false,
    platformUnavailable: true,
    suites: [],
    resolveExe: () => null
  }
];

export function getTerminal(id) {
  return TERMINALS.find((t) => t.id === id) || null;
}
