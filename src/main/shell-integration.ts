// Shell integration injected when an INTERACTIVE pane spawns. Two jobs:
//   1. teach the app the live cwd via OSC 7 (→ git-project detection, feature 2);
//   2. turn on the shell's own history autocomplete (PSReadLine inline prediction,
//      feature 3).
// Strictly ADDITIVE: a missing script, a rejected flag, or an old shell must still
// leave a plain, working interactive shell — every path here degrades to "no
// integration" (a null / unchanged spawn) rather than failing.

import { existsSync } from 'node:fs';
import { join } from 'node:path';
import { app } from 'electron';

export type ShellKind = 'pwsh' | 'bash' | 'cmd' | 'other';

// Classify a shell by its executable name/path. PowerShell is checked FIRST
// because "powershell" also ends in "sh", so a naive POSIX test would misfire.
export function classify(shell: string): ShellKind {
  const lower = (shell || '').toLowerCase();
  if (lower.includes('pwsh') || lower.includes('powershell')) return 'pwsh';
  if (/(?:^|[\\/])bash(?:\.exe)?$/.test(lower)) return 'bash';
  if (/(?:^|[\\/])cmd(?:\.exe)?$/.test(lower)) return 'cmd';
  return 'other';
}

// Absolute on-disk directory holding the hp-init.* scripts. The *external shell*
// reads these, so they must be on the real filesystem — NEVER `__dirname` (it lives
// inside the asar in production). Packaged → process.resourcesPath/shell-integration
// (electron-builder `extraResources`); dev → <appPath>/resources/shell-integration,
// with a resourcesPath fallback just in case.
export function shellIntegrationDir(): string {
  if (app.isPackaged) {
    return join(process.resourcesPath, 'shell-integration');
  }
  const devDir = join(app.getAppPath(), 'resources', 'shell-integration');
  if (existsSync(devDir)) return devDir;
  if (process.resourcesPath) {
    const prodDir = join(process.resourcesPath, 'shell-integration');
    if (existsSync(prodDir)) return prodDir;
  }
  return devDir;
}

// Spawn additions for an interactive shell: the extra argv that loads our init
// script plus any env to merge. Returns null (→ plain shell, no integration) for
// cmd/other or when the expected script is missing on disk.
export function integrationFor(
  shell: string,
  dir: string
): { args: string[]; env: Record<string, string> } | null {
  const kind = classify(shell);
  if (kind === 'pwsh') {
    const script = join(dir, 'hp-init.ps1');
    if (!existsSync(script)) return null;
    // Dot-source the script (runs in session scope) AFTER the user's $PROFILE.
    // -Command (NOT -File) so the profile loads first; -NoExit keeps the shell
    // interactive; single-quote the path and double-up any embedded quotes.
    const quoted = script.replace(/'/g, "''");
    return { args: ['-NoExit', '-Command', `. '${quoted}'`], env: {} };
  }
  if (kind === 'bash') {
    const script = join(dir, 'hp-init.sh');
    if (!existsSync(script)) return null;
    // --rcfile REPLACES ~/.bashrc, so the script sources it back itself. Bash wants
    // forward slashes even on Windows (git-bash).
    const posix = script.replace(/\\/g, '/');
    return { args: ['--rcfile', posix, '-i'], env: {} };
  }
  return null;
}

// Convert a `file://` URI (from OSC 7) to an OS-native absolute path, or null if it
// can't / shouldn't be used as a local cwd. Pure — unit-tested.
//   • pwsh emits `file:///C:/Users/me/repo`  → `C:\Users\me\repo`
//   • git-bash emits MSYS `file:///c/Users/me/repo` → `C:\Users\me\repo`
//   • `%20` etc. are percent-decoded
//   • a non-empty, non-localhost authority (a REMOTE host, e.g. an SSH prompt) is
//     rejected so a remote shell can't relocate the local pane.
export function fileUriToPath(uri: string): string | null {
  if (!uri) return null;
  const trimmed = uri.trim();
  const prefix = 'file://';
  if (trimmed.slice(0, prefix.length).toLowerCase() !== prefix) return null;

  const rest = trimmed.slice(prefix.length); // authority + path
  let authority: string;
  let path: string;
  const slashIdx = rest.indexOf('/');
  if (slashIdx === -1) {
    authority = rest;
    path = '';
  } else {
    authority = rest.slice(0, slashIdx);
    path = rest.slice(slashIdx); // keeps the leading '/'
  }
  // Reject remote hosts; allow empty authority or explicit localhost.
  if (authority && authority.toLowerCase() !== 'localhost') return null;

  let decoded: string;
  try {
    decoded = decodeURIComponent(path);
  } catch {
    decoded = path;
  }

  // Windows drive with colon: /C:/Users/me → C:\Users\me
  let m = decoded.match(/^\/([A-Za-z]):(.*)$/);
  if (m) {
    return `${m[1].toUpperCase()}:${m[2].replace(/\//g, '\\')}`;
  }
  // MSYS drive (git-bash): /c/Users/me → C:\Users\me
  m = decoded.match(/^\/([A-Za-z])\/(.*)$/);
  if (m) {
    return `${m[1].toUpperCase()}:\\${m[2].replace(/\//g, '\\')}`;
  }
  // POSIX absolute path: hand back as-is.
  return decoded || null;
}

// Bound on a carried, still-incomplete OSC 7 sequence. A real cwd URI is short; a
// sequence that grows past this is junk and is abandoned rather than buffered.
const OSC7_MAX = 8192;
const OSC7_PREFIX = '\x1b]7;'; // ESC ] 7 ;

// Pure, stateful-free OSC 7 scanner. Given the carry from the previous call and the
// next raw pty chunk, returns the cwd of the LAST complete OSC 7 in this window (or
// null) plus the carry to feed the next call. Handles sequences split across chunks
// (both a split URI and a split prefix) via a bounded carry. De-duping on change is
// the caller's job (a prompt re-emits OSC 7 every keystroke).
export function parseOsc7(carry: string, chunk: string): { cwd: string | null; carry: string } {
  // Fast reject: nothing pending and no ESC anywhere → impossible to hold OSC 7.
  if (!carry && chunk.indexOf('\x1b') === -1) return { cwd: null, carry: '' };

  const buf = carry + chunk;
  let lastUri: string | null = null;
  let searchFrom = 0;
  for (;;) {
    const start = buf.indexOf(OSC7_PREFIX, searchFrom);
    if (start === -1) break;
    const afterPrefix = start + OSC7_PREFIX.length;
    const belIdx = buf.indexOf('\x07', afterPrefix); // BEL terminator
    const stIdx = buf.indexOf('\x1b\\', afterPrefix); // ST terminator (ESC \)
    let end = -1;
    let termLen = 0;
    if (belIdx !== -1 && (stIdx === -1 || belIdx < stIdx)) {
      end = belIdx;
      termLen = 1;
    } else if (stIdx !== -1) {
      end = stIdx;
      termLen = 2;
    }
    if (end === -1) break; // incomplete sequence at the tail — handled by carry below
    lastUri = buf.slice(afterPrefix, end);
    searchFrom = end + termLen;
  }

  // Carry forward only a trailing partial that might complete in the next chunk.
  let nextCarry = '';
  const lastStart = buf.lastIndexOf(OSC7_PREFIX);
  if (lastStart !== -1) {
    const after = lastStart + OSC7_PREFIX.length;
    const complete = buf.indexOf('\x07', after) !== -1 || buf.indexOf('\x1b\\', after) !== -1;
    if (!complete) {
      const tail = buf.slice(lastStart);
      nextCarry = tail.length > OSC7_MAX ? '' : tail; // abandon oversized junk
    }
  } else {
    // No full prefix, but the prefix itself may be split (ends with ESC / ESC] / ESC]7).
    for (let i = OSC7_PREFIX.length - 1; i >= 1; i--) {
      if (buf.length >= i && buf.slice(-i) === OSC7_PREFIX.slice(0, i)) {
        nextCarry = buf.slice(-i);
        break;
      }
    }
  }

  const cwd = lastUri !== null ? fileUriToPath(lastUri) : null;
  return { cwd, carry: nextCarry };
}
