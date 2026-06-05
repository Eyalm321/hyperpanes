// On-disk verification + opening for clickable terminal paths (see Terminal.tsx
// and pathLinks.ts). Lives in main because the renderer is sandboxed: only here
// do we have fs, child_process and Electron's shell.

import os from 'os';
import path from 'path';
import { promises as fs } from 'fs';
import { execFile, spawn } from 'child_process';
import { shell } from 'electron';

// Extensions we refuse to auto-open via the OS default handler, because on
// Windows shell.openPath would EXECUTE them. Only relevant on the OS-default
// path: a configured editor opens these as text just fine (.js/.ps1 are source).
export const EXECUTABLE_EXTS = new Set([
  '.exe', '.bat', '.cmd', '.com', '.scr', '.msi', '.msp', '.ps1', '.psm1',
  '.vbs', '.vbe', '.js', '.jse', '.wsf', '.wsh', '.hta', '.cpl', '.jar',
  '.reg', '.lnk', '.pif', '.sh', '.bash', '.zsh', '.fish', '.command', '.app'
]);

export interface ResolveResult {
  token: string;
  absPath: string;
  exists: boolean;
  isDir: boolean;
  isExe: boolean;
}

// Resolve each candidate token against the pane's cwd (falling back to the home
// dir, matching the pty's own `opts.cwd || os.homedir()` start dir) and stat it.
export async function resolvePaths(cwd: string | undefined, tokens: string[]): Promise<ResolveResult[]> {
  const base = cwd || os.homedir();
  return Promise.all(
    tokens.map(async (token): Promise<ResolveResult> => {
      let p = token;
      if (p === '~' || p.startsWith('~/') || p.startsWith('~\\')) p = os.homedir() + p.slice(1);
      let absPath = '';
      try {
        absPath = path.resolve(base, p);
        const st = await fs.stat(absPath);
        return {
          token,
          absPath,
          exists: true,
          isDir: st.isDirectory(),
          isExe: EXECUTABLE_EXTS.has(path.extname(absPath).toLowerCase())
        };
      } catch {
        return { token, absPath, exists: false, isDir: false, isExe: false };
      }
    })
  );
}

export interface OpenResult {
  ok: boolean;
  blocked?: boolean; // refused an executable on the OS-default path
  error?: string;
}

// Cached one-shot detection of VS Code on PATH (the zero-config default editor).
let vscodeChecked = false;
let vscodeCmd: string | null = null;
function detectVSCode(): Promise<string | null> {
  if (vscodeChecked) return Promise.resolve(vscodeCmd);
  return new Promise((resolve) => {
    const finder = process.platform === 'win32' ? 'where' : 'which';
    execFile(finder, ['code'], (err, stdout) => {
      vscodeChecked = true;
      if (!err && stdout.trim()) vscodeCmd = stdout.split(/\r?\n/)[0].trim();
      resolve(vscodeCmd);
    });
  });
}

function quote(arg: string): string {
  if (process.platform === 'win32') return `"${arg.replace(/"/g, '""')}"`;
  return `'${arg.replace(/'/g, `'\\''`)}'`;
}

// Launch a detached command line through the shell so things like `code.cmd`
// resolve. Errors are swallowed — a missing editor just no-ops (the renderer
// already toasted nothing-to-do cases via the OS fallback).
function launch(commandLine: string) {
  try {
    const child = spawn(commandLine, { shell: true, detached: true, stdio: 'ignore', windowsHide: true });
    child.on('error', () => {});
    child.unref();
  } catch {
    /* spawn failed — nothing we can do */
  }
}

function runEditorTemplate(template: string, absPath: string, line?: number, col?: number) {
  // Split into argv BEFORE substitution so {path} stays a single argument even
  // when the path has spaces, then re-quote each piece.
  const argv = template
    .split(/\s+/)
    .filter(Boolean)
    .map((part) =>
      part
        .replace(/\{path\}/g, absPath)
        .replace(/\{line\}/g, line != null ? String(line) : '')
        .replace(/\{col\}/g, col != null ? String(col) : '')
        .replace(/:+$/, '') // tidy a dangling `::` left when there's no line/col
    )
    .filter(Boolean);
  if (!argv.length) return;
  launch(argv.map(quote).join(' '));
}

export async function openResolvedPath(
  absPath: string,
  line: number | undefined,
  col: number | undefined,
  editorCommand: string
): Promise<OpenResult> {
  let st;
  try {
    st = await fs.stat(absPath);
  } catch {
    return { ok: false, error: 'not found' };
  }

  // Directories: just open the folder.
  if (st.isDirectory()) {
    const err = await shell.openPath(absPath);
    return err ? { ok: false, error: err } : { ok: true };
  }

  // A configured editor wins and is trusted to handle any extension (incl.
  // source scripts), so the executable guard does not apply to this branch.
  const template = editorCommand?.trim();
  if (template) {
    runEditorTemplate(template, absPath, line, col);
    return { ok: true };
  }

  // Zero-config default: VS Code if present, with a line/col jump.
  const code = await detectVSCode();
  if (code) {
    const target = line != null ? `${absPath}:${line}${col != null ? `:${col}` : ''}` : absPath;
    launch(`${quote(code)} -g ${quote(target)}`);
    return { ok: true };
  }

  // OS default handler — refuse to execute scripts/binaries.
  const ext = path.extname(absPath).toLowerCase();
  if (EXECUTABLE_EXTS.has(ext)) return { ok: false, blocked: true, error: ext };
  const err = await shell.openPath(absPath);
  return err ? { ok: false, error: err } : { ok: true };
}
