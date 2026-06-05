import { describe, it, expect } from 'vitest';
import {
  waitDecision,
  nextPollDelay,
  sliceSince,
  detectAwaitingInput,
  WAIT_POLL_MIN_MS,
  WAIT_POLL_MAX_MS
} from './control-output';

describe('waitDecision', () => {
  const base = { totalBytes: 100, since: undefined, now: 1000, start: 0, settleMs: 600, timeoutMs: 30000 };

  it('settles once output has been quiet for settleMs', () => {
    expect(waitDecision({ ...base, lastOutputAt: 1000 - 600 })).toBe('settled');
    expect(waitDecision({ ...base, lastOutputAt: 1000 - 599 })).toBe('wait');
  });

  it('treats a pane that never emitted output as quiet (settles)', () => {
    expect(waitDecision({ ...base, lastOutputAt: undefined })).toBe('settled');
  });

  it('keeps waiting while output is still streaming, until the timeout', () => {
    expect(waitDecision({ ...base, lastOutputAt: 990, now: 1000 })).toBe('wait');
    // Past the deadline with output still recent → give up.
    expect(waitDecision({ ...base, lastOutputAt: 29990, now: 30001, start: 0 })).toBe('timeout');
  });

  it('with `since`, will not settle until output advances past the cursor', () => {
    // Quiet, but no new output since the cursor → the reply has not started yet.
    expect(waitDecision({ ...base, lastOutputAt: 0, since: 100, totalBytes: 100 })).toBe('wait');
    // Output advanced past the cursor and then went quiet → settle.
    expect(waitDecision({ ...base, lastOutputAt: 0, since: 100, totalBytes: 250 })).toBe('settled');
  });

  it('with `since`, still times out if output never arrives', () => {
    expect(
      waitDecision({ ...base, lastOutputAt: undefined, since: 100, totalBytes: 100, now: 30001 })
    ).toBe('timeout');
  });
});

describe('nextPollDelay', () => {
  it('sleeps until just past the quiet point, clamped to the poll band', () => {
    // 500ms since last output, settle 600 → ~100ms until quiet (== max band).
    expect(nextPollDelay({ lastOutputAt: 500, now: 1000, start: 0, settleMs: 600, timeoutMs: 30000 })).toBe(
      WAIT_POLL_MAX_MS
    );
    // Output just arrived (still ~590ms from quiet) → re-check at the band max.
    expect(nextPollDelay({ lastOutputAt: 990, now: 1000, start: 0, settleMs: 600, timeoutMs: 30000 })).toBe(
      WAIT_POLL_MAX_MS
    );
    // Almost quiet (only ~10ms left in the settle window) → floor at the band min.
    expect(nextPollDelay({ lastOutputAt: 410, now: 1000, start: 0, settleMs: 600, timeoutMs: 30000 })).toBe(
      WAIT_POLL_MIN_MS
    );
  });

  it('never overshoots the remaining deadline, even below the poll band', () => {
    expect(
      nextPollDelay({ lastOutputAt: 1000, now: 1010, start: 0, settleMs: 600, timeoutMs: 1020 })
    ).toBe(10);
  });
});

describe('sliceSince', () => {
  it('returns nothing new when the cursor is at or ahead of the total', () => {
    expect(sliceSince('abcdef', 6, 6)).toEqual({ output: '', cursor: 6, truncated: false });
    expect(sliceSince('abcdef', 6, 99)).toEqual({ output: '', cursor: 6, truncated: false });
  });

  it('returns the exact tail slice for a cursor inside the buffer', () => {
    // Total 6 bytes, all retained; since 4 → last 2 bytes.
    expect(sliceSince('abcdef', 6, 4)).toEqual({ output: 'ef', cursor: 6, truncated: false });
    expect(sliceSince('abcdef', 6, 0)).toEqual({ output: 'abcdef', cursor: 6, truncated: false });
  });

  it('flags truncation when the cursor predates the retained buffer', () => {
    // 1000 bytes ever emitted but the buffer only holds the last 6.
    const r = sliceSince('uvwxyz', 1000, 10);
    expect(r.output).toBe('uvwxyz');
    expect(r.cursor).toBe(1000);
    expect(r.truncated).toBe(true);
  });

  it('hands back the whole buffer without truncation when the gap exactly fits', () => {
    // newBytes (6) == replay.length (6): everything new is still retained.
    expect(sliceSince('abcdef', 6, 0)).toEqual({ output: 'abcdef', cursor: 6, truncated: false });
  });
});

describe('detectAwaitingInput', () => {
  it('flags y/n and yes/no prompts', () => {
    expect(detectAwaitingInput('Overwrite the file? (y/n)')).toBe(true);
    expect(detectAwaitingInput('Continue [Y/n]')).toBe(true);
    expect(detectAwaitingInput('Proceed? (yes/no)')).toBe(true);
  });

  it('flags trust dialogs, press-enter prompts, and the claude prompt caret', () => {
    expect(detectAwaitingInput('Do you trust the files in this folder?')).toBe(true);
    expect(detectAwaitingInput('Press enter to continue')).toBe(true);
    expect(detectAwaitingInput('❯ 1. Yes, proceed')).toBe(true);
  });

  it('looks only at the last non-empty line', () => {
    expect(detectAwaitingInput('lots of output\nmore\n\nReady? (y/n)\n\n')).toBe(true);
    // A question earlier in the scrollback, but the agent has moved on.
    expect(detectAwaitingInput('Are you sure?\nOK, done.\nSpun you up a server.')).toBe(false);
  });

  it('does not flag ordinary completed output', () => {
    expect(detectAwaitingInput('Spun you up a server on port 3000.')).toBe(false);
    expect(detectAwaitingInput('')).toBe(false);
    expect(detectAwaitingInput('\n\n   \n')).toBe(false);
  });
});
