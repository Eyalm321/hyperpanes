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
import { createRequire } from 'node:module';
import { join } from 'node:path';
import { REPO_ROOT } from './lib/launch.mjs';

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
    if (first && existsSync(first)) return first;
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

// hyperpanes app version comes from the repo's package.json (running the exe with
// an unknown --version flag would just open a window).
function hyperpanesVersion() {
  try {
    return JSON.parse(readFileSync(join(REPO_ROOT, 'package.json'), 'utf8')).version || '?';
  } catch {
    return '?';
  }
}

function resolveElectron() {
  const require = createRequire(import.meta.url);
  return require(join(REPO_ROOT, 'node_modules', 'electron')); // returns path to electron.exe
}

export const TERMINALS = [
  {
    id: 'hyperpanes',
    name: 'hyperpanes',
    wingetId: null,
    exeNames: ['Hyperpanes.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Programs', 'Hyperpanes'), join(PROGRAMFILES, 'Hyperpanes')],
    versionArgs: null,
    fixedVersion: hyperpanesVersion,
    driven: true,
    procMatch: ['Hyperpanes.exe', 'electron.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['Hyperpanes.exe'], [
      join(LOCALAPPDATA, 'Programs', 'Hyperpanes'),
      join(PROGRAMFILES, 'Hyperpanes')
    ]),
    // Prefer the installed exe; fall back to dev electron when HYPERPANES_DEV=1 or no exe.
    launch({ wrapperPath, cwd, label = 'bench' }) {
      const installed = this.resolveExe();
      const wantDev = process.env.HYPERPANES_DEV === '1' || !installed;
      const tail = ['--shell', 'cmd.exe', '-c', wrapperPath, '--name', label, '--cwd', cwd];
      if (wantDev) {
        return { exe: resolveElectron(), args: [join(REPO_ROOT, 'out', 'main', 'index.js'), ...tail], dev: true };
      }
      return { exe: installed, args: tail, dev: false };
    },
    // hyperpanes is "available" if either the installed exe or a dev build exists.
    isAvailable() {
      if (this.resolveExe()) return true;
      return existsSync(join(REPO_ROOT, 'out', 'main', 'index.js'));
    }
  },
  {
    id: 'wt',
    name: 'Windows Terminal',
    wingetId: 'Microsoft.WindowsTerminal',
    exeNames: ['wt.exe'],
    searchDirs: [join(LOCALAPPDATA, 'Microsoft', 'WindowsApps')],
    versionArgs: ['--version'],
    driven: true,
    procMatch: ['wt.exe', 'WindowsTerminal.exe', 'OpenConsole.exe'],
    suites: DRIVEN_SUITES,
    resolveExe: resolver(['wt.exe'], [join(LOCALAPPDATA, 'Microsoft', 'WindowsApps')]),
    launch({ wrapperPath }) {
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
