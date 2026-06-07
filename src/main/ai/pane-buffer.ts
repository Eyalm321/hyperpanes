// Per-pane rolling tail buffer for the ambient-AI feature. It consumes RAW pty
// output chunks and keeps a bounded, ANSI-stripped tail that the scheduler later
// hands to the LLM to summarise.
//
// The hard parts this module owns:
//   - detect screen state (alt-screen enter/leave, full clear) from the RAW chunk
//     BEFORE the ANSI is stripped away,
//   - reuse the existing `stripAnsi` (no second ANSI implementation),
//   - stitch partial lines that straddle chunk boundaries so a line is never
//     double-counted or split,
//   - stay bounded in memory per uid (line cap + char cap), for arbitrarily many uids.
//
// Control bytes are built from char codes (not literals) so the source stays pure
// ASCII — matching the convention in ../ansi-strip.

import { stripAnsi } from '../ansi-strip';

const ESC = String.fromCharCode(0x1b); // ESC
const CR = String.fromCharCode(0x0d); // carriage return

// Alt-screen toggle: ESC [ ? (1049|47) (h|l). h enters, l leaves.
const ALT_TOGGLE = new RegExp(ESC + '\\[\\?(?:1049|47)(h|l)', 'g');
// Full screen clear: ESC [ 2 J — matched as a literal so we can slice around it.
const CLEAR_SCREEN = ESC + '[2J';

export interface TailSnapshot {
  text: string; // last N ANSI-stripped lines joined with "\n" (includes the pending partial)
  altScreen: boolean; // true while the pane is in an alt-screen / full-screen TUI
  dirty: boolean; // true if appended since the last markClean()
  lines: number; // number of lines currently retained (including the pending partial)
}

interface PaneState {
  lines: string[]; // completed lines, oldest first
  pending: string; // partial line not yet terminated by a newline
  altScreen: boolean;
  dirty: boolean;
}

// A lone `\r` redraws the current line in place; the visible result is whatever
// follows the last carriage return. (CRLF newlines are normalised away before
// this runs, so any `\r` left here is a true in-line redraw.)
function applyCarriageReturns(line: string): string {
  const i = line.lastIndexOf(CR);
  return i === -1 ? line : line.slice(i + 1);
}

export class PaneTailBuffer {
  private readonly maxLines: number;
  private readonly maxChars: number;
  private readonly panes = new Map<string, PaneState>();

  constructor(opts?: { maxLines?: number; maxChars?: number }) {
    this.maxLines = opts?.maxLines ?? 120;
    this.maxChars = opts?.maxChars ?? 6144;
  }

  append(uid: string, rawChunk: string): void {
    if (!rawChunk) return; // tolerate empty chunks: nothing changed

    let state = this.panes.get(uid);
    if (!state) {
      state = { lines: [], pending: '', altScreen: false, dirty: false };
      this.panes.set(uid, state);
    }

    // 1. Screen state from the RAW chunk, before stripping. The last toggle wins.
    ALT_TOGGLE.lastIndex = 0;
    let match: RegExpExecArray | null;
    let lastToggle: string | null = null;
    while ((match = ALT_TOGGLE.exec(rawChunk)) !== null) lastToggle = match[1];
    if (lastToggle !== null) state.altScreen = lastToggle === 'h';

    // 2. Full clear: drop everything retained and process only what follows the
    //    last clear in this chunk (anything before it was on the cleared screen).
    let raw = rawChunk;
    const clearAt = raw.lastIndexOf(CLEAR_SCREEN);
    if (clearAt !== -1) {
      state.lines = [];
      state.pending = '';
      raw = raw.slice(clearAt + CLEAR_SCREEN.length);
    }

    // 3. Strip ANSI, stitch the pending partial onto the front, normalise CRLF,
    //    then split into lines. The trailing fragment becomes the new pending.
    const cleaned = stripAnsi(raw);
    const combined = (state.pending + cleaned).replace(/\r\n/g, '\n');
    const parts = combined.split('\n');
    state.pending = parts.pop() ?? '';
    for (const part of parts) state.lines.push(applyCarriageReturns(part));

    // 4. Stay bounded: keep pending and the retained lines under their caps.
    if (state.pending.length > this.maxChars) {
      state.pending = state.pending.slice(-this.maxChars);
    }
    if (state.lines.length > this.maxLines) {
      state.lines = state.lines.slice(-this.maxLines);
    }
    this.enforceCharCap(state);

    state.dirty = true;
  }

  snapshot(uid: string): TailSnapshot {
    const state = this.panes.get(uid);
    if (!state) return { text: '', altScreen: false, dirty: false, lines: 0 };
    const display =
      state.pending.length > 0
        ? [...state.lines, applyCarriageReturns(state.pending)]
        : state.lines;
    return {
      text: display.join('\n'),
      altScreen: state.altScreen,
      dirty: state.dirty,
      lines: display.length,
    };
  }

  markClean(uid: string): void {
    const state = this.panes.get(uid);
    if (state) state.dirty = false;
  }

  clear(uid: string): void {
    this.panes.delete(uid);
  }

  // Trim oldest lines until the retained text is under the char cap. If a single
  // line is itself over the cap, truncate it to its last maxChars so memory stays
  // bounded no matter how pathological the input.
  private enforceCharCap(state: PaneState): void {
    let total = this.charCount(state.lines);
    while (state.lines.length > 1 && total > this.maxChars) {
      const removed = state.lines.shift()!;
      total -= removed.length + 1; // +1 for the joining newline
    }
    if (state.lines.length === 1 && state.lines[0].length > this.maxChars) {
      state.lines[0] = state.lines[0].slice(-this.maxChars);
    }
  }

  private charCount(lines: string[]): number {
    if (lines.length === 0) return 0;
    let n = lines.length - 1; // joining newlines
    for (const line of lines) n += line.length;
    return n;
  }
}
