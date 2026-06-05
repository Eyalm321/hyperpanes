// Render a pane's xterm.js buffer to clean, readable text for the control API's
// `read_pane({ mode: "screen" })` (interactive-pane-driving plan C1). The raw pty
// byte stream linearizes a TUI's in-place redraws into overlapping garbage
// (collapsed spaces, spinner frames, repeated status bars); the xterm.js buffer
// is the faithful VT state AFTER cursor moves / line clears / wrapping, so
// serializing it gives what's actually on screen.
//
// Typed against the minimal slice of xterm's Terminal we touch, so this module
// stays free of the heavy '@xterm/xterm' import and is unit-testable with a fake.

export interface BufferLineLike {
  translateToString(trimRight?: boolean): string;
}
export interface BufferLike {
  readonly length: number;
  getLine(index: number): BufferLineLike | undefined;
}
export interface TerminalLike {
  buffer: { active: BufferLike };
}

// Drop trailing blank lines and join. The buffer is usually mostly-empty rows
// below the content; keeping them would bury the real output under whitespace.
// Interior blank lines are preserved (they're part of the layout).
export function trimScreenText(lines: string[]): string {
  let end = lines.length;
  while (end > 0 && lines[end - 1].trim() === '') end--;
  return lines.slice(0, end).join('\n');
}

// Walk the active buffer (scrollback + viewport for the normal buffer; the
// visible screen for the alt buffer a fullscreen TUI uses) top to bottom,
// right-trimming each line, then drop trailing blanks. Horizontal spacing done
// via cursor-forward survives here as real spaces — unlike an SGR-only strip of
// the raw stream, where it vanishes.
export function serializeTerminal(term: TerminalLike): string {
  const buf = term.buffer.active;
  const lines: string[] = [];
  for (let i = 0; i < buf.length; i++) {
    lines.push(buf.getLine(i)?.translateToString(true) ?? '');
  }
  return trimScreenText(lines);
}
