// Normalize control-API `send_input` line endings to what the local pty actually
// submits a line on. Windows conpty runs a line on CR (\r), not LF (\n): an agent
// that ends send_input with a bare "\n" types the line but never executes it
// (live finding, 2026-06-05). On Windows, collapse every newline — CRLF or a lone
// LF — to a single CR so "\n" submits exactly as it does on a POSIX pty, where LF
// is itself a canonical line delimiter. No-op off Windows. The platform is a
// parameter (defaulting to the real one) so this stays pure and unit-testable.
export function submitNewlines(data: string, platform: NodeJS.Platform = process.platform): string {
  if (platform !== 'win32') return data;
  return data.replace(/\r\n/g, '\r').replace(/\n/g, '\r');
}

// How long to wait between writing `send_input` text and the trailing bare CR
// when `submit` is set. A TUI that has bracketed-paste mode on (e.g. claude's)
// treats text + "\r" arriving in ONE pty read as a paste — the CR lands in the
// input box instead of submitting. Splitting them into two writes a beat apart
// makes the CR a distinct keystroke. ~40 ms is enough for conpty to deliver them
// as separate reads without a human-perceptible lag (live finding, 2026-06-05).
export const SUBMIT_DELAY_MS = 40;

// Named-key vocabulary for `send_keys`: a stable, terminal-agnostic name → the
// byte sequence a VT/xterm pty expects. Menus and prompts need real keystrokes
// (the first-run trust dialog wants `enter`; cancelling wants `escape`/`ctrl+c`)
// that a plain `send_input` string can't express. Keep this table the single
// source of truth — it's pure and unit-tested, and the control server writes its
// bytes straight to the pty (NO submitNewlines: these are already the exact
// bytes, e.g. `enter` IS the CR a Windows pty submits on).
const NAMED_KEYS: Record<string, string> = {
  enter: '\r',
  return: '\r',
  escape: '\x1b',
  esc: '\x1b',
  tab: '\t',
  'shift+tab': '\x1b[Z',
  backtab: '\x1b[Z',
  up: '\x1b[A',
  down: '\x1b[B',
  right: '\x1b[C',
  left: '\x1b[D',
  home: '\x1b[H',
  end: '\x1b[F',
  pageup: '\x1b[5~',
  pgup: '\x1b[5~',
  pagedown: '\x1b[6~',
  pgdn: '\x1b[6~',
  insert: '\x1b[2~',
  delete: '\x1b[3~',
  del: '\x1b[3~',
  backspace: '\x7f',
  space: ' '
};

// Resolve one named key to its bytes, or null if unknown. Case/space-insensitive.
// `ctrl+<a-z>` is handled generically (the C0 control code, ctrl+a → 0x01) on top
// of the explicit table above.
export function keyToBytes(key: string): string | null {
  const k = key.trim().toLowerCase();
  if (k in NAMED_KEYS) return NAMED_KEYS[k];
  const ctrl = /^ctrl\+([a-z])$/.exec(k);
  if (ctrl) return String.fromCharCode(ctrl[1].charCodeAt(0) - 96); // 'a'(97) → 0x01
  return null;
}

export type KeysResult = { ok: true; bytes: string } | { ok: false; unknown: string[] };

// Translate a list of named keys into one byte string to write to the pty.
// Reports EVERY unknown key (not just the first) so a caller fixes them in one
// round-trip. An empty list is a valid no-op write.
export function keysToBytes(keys: string[]): KeysResult {
  const unknown: string[] = [];
  let bytes = '';
  for (const key of keys) {
    const b = keyToBytes(key);
    if (b === null) unknown.push(key);
    else bytes += b;
  }
  return unknown.length ? { ok: false, unknown } : { ok: true, bytes };
}
