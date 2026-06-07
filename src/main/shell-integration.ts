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
  if (kind === 'cmd') {
    // cmd has no init-script hook, but its PROMPT can carry the cwd: $E=ESC,
    // $P=current path, $G='>'. We prefix the OSC 9;9 cwd report
    // (ESC]9;9;<path>ST — the de-facto Windows "current directory" sequence) then
    // restore the normal "<path>>" prompt. No script/args needed, just the env
    // var. It replaces any custom cmd prompt but keeps the default look; strictly
    // additive to functionality (parseOscCwd reads OSC 9;9 too).
    return { args: [], env: { PROMPT: '$E]9;9;$P$E\\$P$G' } };
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

// Bound on a carried, still-incomplete OSC sequence. A real cwd payload is short; a
// sequence that grows past this is junk and is abandoned rather than buffered.
const OSC_MAX = 8192;
const OSC_PREFIX = '\x1b]'; // ESC ]

// Interpret one OSC payload (the bytes between `ESC]` and its terminator) as a cwd:
//   • `7;<file-uri>` → fileUriToPath  (pwsh, bash/git-bash)
//   • `9;9;<path>`   → a raw OS path, optionally double-quoted  (cmd, Windows Terminal)
// Anything else (title `0;…`, hyperlink `8;…`, progress `9;4;…`, …) is not a cwd.
function oscDataToCwd(data: string): string | null {
  if (data.startsWith('7;')) return fileUriToPath(data.slice(2));
  if (data.startsWith('9;9;')) {
    let p = data.slice(4).trim();
    if (p.length >= 2 && p.startsWith('"') && p.endsWith('"')) p = p.slice(1, -1);
    return p || null;
  }
  return null;
}

// Pure, state-free scanner for cwd-bearing OSC sequences (OSC 7 + OSC 9;9). Given
// the carry from the previous call and the next raw pty chunk, returns the cwd of
// the LAST recognized sequence in this window (or null) plus the carry to feed the
// next call. Handles sequences split across chunks (split payload and split prefix)
// via a bounded carry. De-duping on change is the caller's job (a prompt re-emits
// its cwd OSC every keystroke).
export function parseOscCwd(carry: string, chunk: string): { cwd: string | null; carry: string } {
  // Fast reject: nothing pending and no ESC anywhere → impossible to hold an OSC.
  if (!carry && chunk.indexOf('\x1b') === -1) return { cwd: null, carry: '' };

  const buf = carry + chunk;
  let lastCwd: string | null = null;
  let searchFrom = 0;
  for (;;) {
    const start = buf.indexOf(OSC_PREFIX, searchFrom);
    if (start === -1) break;
    const afterPrefix = start + OSC_PREFIX.length;
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
    const cwd = oscDataToCwd(buf.slice(afterPrefix, end));
    if (cwd) lastCwd = cwd;
    searchFrom = end + termLen;
  }

  // Carry forward only a trailing partial that might complete in the next chunk.
  let nextCarry = '';
  const lastStart = buf.lastIndexOf(OSC_PREFIX);
  if (lastStart !== -1) {
    const after = lastStart + OSC_PREFIX.length;
    const complete = buf.indexOf('\x07', after) !== -1 || buf.indexOf('\x1b\\', after) !== -1;
    if (!complete) {
      const tail = buf.slice(lastStart);
      nextCarry = tail.length > OSC_MAX ? '' : tail; // abandon oversized junk
    }
  } else if (buf.slice(-1) === '\x1b') {
    // The 2-char prefix may be split: a lone trailing ESC starts the next OSC.
    nextCarry = '\x1b';
  }

  return { cwd: lastCwd, carry: nextCarry };
}
