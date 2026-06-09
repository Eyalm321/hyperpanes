// Launch primitives: build a per-run .cmd wrapper, spawn a terminal, wait for the
// workload's result/ready file, kill the process tree, and the process helpers the
// orchestrator needs (single-instance preflight, orphan checks).
//
// Why a .cmd wrapper? Terminals receive the workload differently: -e-style
// terminals take an argv array (child_process quotes each element cleanly), but
// hyperpanes takes a single command *string* that it re-parses through a shell —
// and cmd.exe /c cannot reliably handle the \"-escaped quotes node-pty would
// inject for a spaces-in-path node.exe. A .cmd file uses cmd-native quoting, which
// IS reliable, so every terminal runs `cmd /c <wrapper>`: one quoting routine and
// a fair, identical cmd+node subtree across all of them.

import { spawn, spawnSync } from 'node:child_process';
import { writeFileSync, existsSync, readFileSync, mkdirSync, unlinkSync, rmSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { sleep } from '../measure/timing.mjs';

const here = dirname(fileURLToPath(import.meta.url));
export const BENCH_ROOT = join(here, '..');
export const REPO_ROOT = join(BENCH_ROOT, '..');
export const RESULTS_DIR = join(BENCH_ROOT, 'results');
export const NODE = process.execPath; // identical interpreter inside every terminal

let counter = 0;
export function uid(prefix = 'r') {
  counter += 1;
  return `${prefix}-${Date.now().toString(36)}-${counter}`;
}

export function ensureResultsDir() {
  mkdirSync(RESULTS_DIR, { recursive: true });
  return RESULTS_DIR;
}

// cmd-native quoting: wrap in quotes, double any embedded quote.
const cmdQuote = (s) => `"${String(s).replace(/"/g, '""')}"`;

/**
 * Write a .cmd that runs `<node> <script> ...args` with reliable native quoting.
 * @returns absolute path to the wrapper.
 */
export function writeCmdWrapper(id, scriptPath, scriptArgs = []) {
  ensureResultsDir();
  const line = [NODE, scriptPath, ...scriptArgs].map(cmdQuote).join(' ');
  const path = join(RESULTS_DIR, `wrap-${id}.cmd`);
  writeFileSync(path, `@echo off\r\n${line}\r\n`);
  return path;
}

/** Spawn a terminal (GUI window visible). stdio ignored — the workload writes to its own pane.
 * `env` overlays onto the harness env (used to hand each measured app an isolated data dir). */
export function spawnTerminal(exe, args, { cwd = REPO_ROOT, env } = {}) {
  return spawn(exe, args, {
    cwd,
    stdio: 'ignore',
    windowsHide: false,
    env: env ? { ...process.env, ...env } : process.env
  });
}

/**
 * Create a fresh throwaway data dir under the OS temp dir and return its path. Used to give each
 * measured hyperpanes (native or Electron) an ISOLATED userData/APPDATA so it starts as a clean,
 * fresh instance — the native data dir keys on %APPDATA% and Electron's on --user-data-dir, so an
 * isolated dir avoids restoring a saved session and (for Electron) sidesteps the single-instance
 * lock that would otherwise forward the launch to a running instance. Caller cleans it up.
 */
export function makeTempDataDir(prefix = 'hpbench') {
  const dir = join(tmpdir(), `${prefix}-${uid('d')}`);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/** Poll for a file to appear. Resolves true if it shows up before timeout. */
export async function waitForFile(path, timeoutMs = 60000, pollMs = 100) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (existsSync(path)) return true;
    await sleep(pollMs);
  }
  return existsSync(path);
}

/** Read + parse a JSON file, retrying briefly in case we caught a partial write. */
export async function readJsonSafe(path, tries = 5) {
  for (let i = 0; i < tries; i++) {
    try {
      return JSON.parse(readFileSync(path, 'utf8'));
    } catch {
      await sleep(50);
    }
  }
  return null;
}

/** True if a PID is still alive (EPERM counts as alive). */
export function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === 'EPERM';
  }
}

/** Force-kill a process and all descendants. */
export function killTree(pid) {
  if (!pid) return;
  spawnSync('taskkill', ['/PID', String(pid), '/T', '/F'], { encoding: 'utf8' });
}

/** Kill the tree and wait until the root PID is gone (so the next spawn starts clean). */
export async function killTreeAndWait(pid, timeoutMs = 8000) {
  killTree(pid);
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (!pidAlive(pid)) return true;
    await sleep(100);
  }
  return !pidAlive(pid);
}

/** All running processes as {name, pid} (via tasklist CSV). */
export function listProcesses() {
  const res = spawnSync('tasklist', ['/FO', 'CSV', '/NH'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 });
  if (res.status !== 0 || !res.stdout) return [];
  return res.stdout
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => {
      const cols = line.split('","').map((c) => c.replace(/^"|"$/g, ''));
      return { name: cols[0], pid: Number(cols[1]) };
    })
    .filter((p) => Number.isFinite(p.pid));
}

/** Running PIDs whose image name matches any of `names` (case-insensitive). */
export function processesByName(names) {
  const set = new Set(names.map((n) => n.toLowerCase()));
  return listProcesses().filter((p) => set.has(p.name.toLowerCase()));
}

/**
 * True if a NATIVE hyperpanes instance built from this repo is already up — a `hyperpanes.exe`
 * whose executable path lives under `…\rs\crates\app\target\…` (the cargo build output). Replaces
 * the Electron-era `repoElectronRunning()` (which looked for `electron.exe` with the repo path).
 *
 * The native GUI has no single-instance lock and we always launch with an isolated %APPDATA%, so a
 * running dev instance does NOT capture the harness's launch — this is informational only (the
 * preflight surfaces it as a note, not a hard skip).
 */
export function nativeRepoRunning() {
  const res = spawnSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      "Get-CimInstance Win32_Process -Filter \"Name='hyperpanes.exe'\" | ForEach-Object { $_.ExecutablePath }"
    ],
    { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 }
  );
  if (res.status !== 0 || !res.stdout) return false;
  const needle = join(REPO_ROOT, 'rs', 'crates', 'app', 'target').toLowerCase();
  return res.stdout.toLowerCase().includes(needle);
}

/**
 * Delete generated temp files, ignoring errors. A path ending in `\` is a directory
 * (the isolated data dirs minted by makeTempDataDir): remove it recursively — a plain
 * rmSync without {recursive:true} throws EISDIR on a non-empty dir and would silently
 * leak it under %TEMP%.
 */
export function cleanup(paths) {
  for (const p of paths) {
    try {
      if (!p || !existsSync(p)) continue;
      if (p.endsWith('\\')) rmSync(p, { recursive: true, force: true });
      else unlinkSync(p);
    } catch {
      /* ignore */
    }
  }
}
