import { EventEmitter } from 'events';
import os from 'os';
import { join, resolve } from 'node:path';
import fs from 'node:fs';
import { app } from 'electron';
import * as pty from 'node-pty';
import type { IPty } from 'node-pty';

// Max duration / size to batch pty output before flushing to the renderer.
// Mirrors vercel/hyper's DataBatcher: collapses many tiny pty chunks into one
// IPC message, cutting IPC count and GC pressure dramatically.
const BATCH_DURATION_MS = 16;
const BATCH_MAX_SIZE = 200 * 1024;

// Recent output kept so a re-attaching window (pane moved to another tab/window)
// can replay history into its fresh terminal instead of showing a blank pane.
const REPLAY_BUFFER_SIZE = 256 * 1024;

class DataBatcher extends EventEmitter {
  private data = '';
  private timeout: NodeJS.Timeout | null = null;

  write(chunk: string) {
    if (this.data.length + chunk.length >= BATCH_MAX_SIZE) {
      if (this.timeout) {
        clearTimeout(this.timeout);
        this.timeout = null;
      }
      this.flush();
    }
    this.data += chunk;
    if (!this.timeout) {
      this.timeout = setTimeout(() => this.flush(), BATCH_DURATION_MS);
    }
  }

  flush() {
    if (this.timeout) {
      clearTimeout(this.timeout);
      this.timeout = null;
    }
    if (!this.data) return;
    const data = this.data;
    this.data = '';
    this.emit('flush', data);
  }
}

export interface SpawnOptions {
  uid: string;
  shell?: string;
  // The program's literal argv. Its meaning depends on `command` (see resolveSpawn,
  // interactive-pane-driving plan P4a):
  //   • with `command` → run `command` DIRECTLY as the executable with these args,
  //     bypassing the shell (no re-parse) — the robust path for args containing
  //     spaces/quotes a shell would mangle;
  //   • without `command` → bare args handed to the interactive shell.
  args?: string[];
  command?: string;
  cwd?: string;
  env?: Record<string, string>;
  cols?: number;
  rows?: number;
  // The owning pane's stable id. Injected as HYPERPANES_PANE_ID so an MCP-capable
  // agent launched in this pane knows which pane it is (agent-orchestration A).
  paneId?: string;
}

export function defaultShell(): string {
  if (process.platform === 'win32') return process.env.COMSPEC || 'powershell.exe';
  return process.env.SHELL || '/bin/bash';
}

// Build argv. When a `command` is supplied we run it through the shell so the
// real exit code flows back via pty.onExit (powers pane status + restart). The
// invocation flag is keyed off the shell, not the platform, so a custom shell
// (e.g. pwsh, or git-bash on Windows) is launched with the right switch.
export function buildArgs(shell: string, command?: string, baseArgs?: string[]): string[] {
  if (!command) return baseArgs ?? [];
  const lower = shell.toLowerCase();
  // Check PowerShell first — 'powershell' also ends in 'sh', so the POSIX test
  // below would otherwise misfire on it.
  if (lower.includes('powershell') || lower.includes('pwsh')) {
    return ['-NoLogo', '-Command', command];
  }
  // POSIX-family shells use `-c` on every platform (covers git-bash on Windows).
  if (/(?:^|[\\/])(?:bash|zsh|fish|sh|dash|ash)(?:\.exe)?$/.test(lower)) {
    return ['-c', command];
  }
  if (process.platform === 'win32') return ['/c', command]; // cmd.exe
  return ['-c', command];
}

// Resolve the actual pty spawn target — the executable file and its argv — from a
// pane's shell/command/args (interactive-pane-driving plan P4a). Three shapes:
//   • `command` + a non-empty `args` → spawn `command` DIRECTLY with `args` as its
//     verbatim argv: NO shell, NO re-parse. node-pty applies the correct
//     per-platform quoting to each arg, so a value containing spaces or quotes
//     (e.g. `--append-system-prompt "…long persona…"`) survives intact instead of
//     being re-split by cmd.exe (the P4a bug). The caller owns making `command`
//     spawnable as-is (an absolute path, or a name the OS launches directly).
//   • `command` alone → run it through the shell (`shell -c "command"` etc.) so the
//     exit code + shell features (pipes, &&) work, exactly as before.
//   • no `command` → an interactive shell, with any `args` handed to it verbatim.
// Pure (string-only), so it's unit-tested without spawning anything.
function isFile(p: string): boolean {
  try {
    const stat = fs.statSync(p);
    return stat.isFile();
  } catch {
    return false;
  }
}

function getEnvVar(name: string, env?: Record<string, string>): string | undefined {
  const target = name.toUpperCase();
  if (env) {
    for (const key of Object.keys(env)) {
      if (key.toUpperCase() === target) {
        return env[key];
      }
    }
  }
  for (const key of Object.keys(process.env)) {
    if (key.toUpperCase() === target) {
      return process.env[key];
    }
  }
  return undefined;
}

export function resolveWindowsCommand(
  command: string,
  cwd?: string,
  env?: Record<string, string>
): string {
  if (!command) return command;

  const pathextVal = getEnvVar('PATHEXT', env) || '.COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC';
  const pathexts = pathextVal
    .split(';')
    .map(ext => ext.trim().toLowerCase())
    .filter(Boolean)
    .map(ext => ext.startsWith('.') ? ext : '.' + ext);

  const findExecutable = (basePath: string): string | null => {
    if (isFile(basePath)) return basePath;
    for (const ext of pathexts) {
      const candidate = basePath + ext;
      if (isFile(candidate)) return candidate;
    }
    return null;
  };

  if (command.includes('/') || command.includes('\\')) {
    const resolved = resolve(cwd || process.cwd(), command);
    const found = findExecutable(resolved);
    return found || command;
  }

  const searchDirs: string[] = [];
  searchDirs.push(cwd || process.cwd());

  const pathVal = getEnvVar('PATH', env) || '';
  const pathDirs = pathVal.split(';').map(d => d.trim()).filter(Boolean);
  searchDirs.push(...pathDirs);

  for (const dir of searchDirs) {
    const resolvedBase = resolve(dir, command);
    const found = findExecutable(resolvedBase);
    if (found) return found;
  }

  return command;
}

// Resolve the actual pty spawn target — the executable file and its argv — from a
// pane's shell/command/args (interactive-pane-driving plan P4a). Three shapes:
//   • `command` + a non-empty `args` → spawn `command` DIRECTLY with `args` as its
//     verbatim argv: NO shell, NO re-parse. node-pty applies the correct
//     per-platform quoting to each arg, so a value containing spaces or quotes
//     (e.g. `--append-system-prompt "…long persona…"`) survives intact instead of
//     being re-split by cmd.exe (the P4a bug). The caller owns making `command`
//     spawnable as-is (an absolute path, or a name the OS launches directly).
//   • `command` alone → run it through the shell (`shell -c "command"` etc.) so the
//     exit code + shell features (pipes, &&) work, exactly as before.
//   • no `command` → an interactive shell, with any `args` handed to it verbatim.
// Pure (string-only), so it's unit-tested without spawning anything.
export function resolveSpawn(
  shell: string,
  command?: string,
  args?: string[],
  cwd?: string,
  env?: Record<string, string>
): { file: string; args: string[] } {
  if (command && args && args.length > 0) {
    const file = process.platform === 'win32' ? resolveWindowsCommand(command, cwd, env) : command;
    return { file, args };
  }
  return { file: shell, args: buildArgs(shell, command, args) };
}

export class Session extends EventEmitter {
  readonly uid: string;
  private pty: IPty | null = null;
  private batcher = new DataBatcher();
  private ended = false;
  private replay = ''; // recent output, for re-attach

  constructor(opts: SpawnOptions) {
    super();
    this.uid = opts.uid;

    const shell = opts.shell || defaultShell();
    // file is the shell (command-via-shell / interactive), or the command itself
    // when an explicit argv was given (direct spawn, no re-parse — P4a).
    const { file, args } = resolveSpawn(shell, opts.command, opts.args, opts.cwd, opts.env);
    const env: Record<string, string> = {
      ...(process.env as Record<string, string>),
      ...opts.env,
      TERM: 'xterm-256color',
      COLORTERM: 'truecolor'
    };
    // Electron injects a default GOOGLE_API_KEY; don't leak it to the shell.
    if (env.GOOGLE_API_KEY && process.env.GOOGLE_API_KEY === env.GOOGLE_API_KEY) {
      delete env.GOOGLE_API_KEY;
    }
    // Pane self-awareness (agent-orchestration A): an agent running in this pane
    // reads its own id and how to reach the control plane straight from env.
    if (opts.paneId) env.HYPERPANES_PANE_ID = opts.paneId;
    // HYPERPANES_CONTROL_FILE mirrors ControlServer.discoveryPath() (set even
    // though control is off by default — the file may not exist yet, so a reader
    // checks first). But a pane handed a SCOPED control token via env (F) must
    // NOT also be able to read the master token from control.json — so only point
    // at the discovery file when no scoped token was injected.
    if (!env.HYPERPANES_CONTROL_TOKEN) {
      env.HYPERPANES_CONTROL_FILE = join(app.getPath('userData'), 'control.json');
    }

    this.pty = pty.spawn(file, args, {
      name: 'xterm-256color',
      cols: opts.cols ?? 80,
      rows: opts.rows ?? 24,
      cwd: opts.cwd || os.homedir(),
      env
    });

    this.pty.onData((chunk) => {
      if (this.ended) return;
      this.batcher.write(chunk);
    });
    this.batcher.on('flush', (data: string) => {
      this.replay = (this.replay + data).slice(-REPLAY_BUFFER_SIZE);
      this.emit('data', data);
    });

    this.pty.onExit(({ exitCode }) => {
      if (this.ended) return;
      this.ended = true;
      this.batcher.flush();
      this.emit('exit', exitCode);
    });
  }

  // Recent output, replayed into a re-attaching terminal.
  getReplay(): string {
    return this.replay;
  }

  write(data: string) {
    this.pty?.write(data);
  }

  resize(cols: number, rows: number) {
    try {
      this.pty?.resize(Math.max(cols, 1), Math.max(rows, 1));
    } catch (err) {
      console.error('resize error', err);
    }
  }

  destroy() {
    if (this.ended) return;
    this.ended = true;
    try {
      this.pty?.kill();
    } catch (err) {
      console.error('kill error', err);
    }
  }
}
