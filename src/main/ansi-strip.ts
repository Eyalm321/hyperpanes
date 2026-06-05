// Strip ANSI escape sequences for a plain-text view of pane output (clean output
// mode, agent-orchestration G). Covers CSI (colors/cursor), OSC (title/hyperlink)
// and the remaining two-byte ESC sequences. Printable text, newlines and tabs are
// left intact so a manager can parse a worker's TUI.
//
// Control bytes are built from char codes (not literals) so the source stays pure
// ASCII — no invisible escape bytes hiding in the file.

const ESC = String.fromCharCode(0x1b); // ESC
const CSI8 = String.fromCharCode(0x9b); // 8-bit CSI
const OSC8 = String.fromCharCode(0x9d); // 8-bit OSC
const BEL = String.fromCharCode(0x07); // OSC terminator (BEL)

// OSC: (ESC ] | 8-bit OSC) … (BEL | ST = ESC \). Lazy to the terminator; run
// BEFORE CSI so the CSI pass can't nibble an OSC body. Strip first.
const OSC = new RegExp('(?:' + ESC + '\\]|' + OSC8 + ')[\\s\\S]*?(?:' + BEL + '|' + ESC + '\\\\)', 'g');
// CSI: (ESC [ | 8-bit CSI) params(0x30-3F) intermediates(0x20-2F) final(0x40-7E).
const CSI = new RegExp('(?:' + ESC + '\\[|' + CSI8 + ')[0-?]*[ -/]*[@-~]', 'g');
// Remaining two-byte ESC Fe sequences (ESC followed by 0x40-0x5F), e.g. ESC M.
const ESC_FE = new RegExp(ESC + '[@-_]', 'g');

export function stripAnsi(input: string): string {
  return input.replace(OSC, '').replace(CSI, '').replace(ESC_FE, '');
}
