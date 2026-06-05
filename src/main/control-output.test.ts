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

// P4b — regression fixtures over REALISTIC rendered screens (the kind a
// mode:"screen" read produces). The gold test only ever exercised the `false`
// (idle/working) path because the cwd auto-trusted at boot, so the positive
// (blocked-on-a-decision) path went unverified live. These lock in both sides and
// the intended semantics: `awaitingInput` means "blocked on a decision a human
// must answer", NOT merely "idle at a prompt box".
describe('detectAwaitingInput — realistic rendered-screen fixtures (P4b)', () => {
  // The first-run trust dialog — the canonical blocking prompt. Its footer line
  // ("Enter to confirm …") is the last non-empty line.
  const TRUST_DIALOG = [
    '╭──────────────────────────────────────────────────────────╮',
    '│ Do you trust the files in this folder?                     │',
    '│                                                            │',
    '│ C:\\hyperpanes                                              │',
    '│                                                            │',
    '│ ❯ 1. Yes, proceed                                          │',
    '│   2. No, exit                                              │',
    '│                                                            │',
    '╰──────────────────────────────────────────────────────────╯',
    '   Enter to confirm · Esc to exit',
    ''
  ].join('\n');

  // An idle claude session: an empty input box (with the ❯ caret INSIDE it) and a
  // status hint as the last visible line. The ❯ is not the last non-empty line, so
  // — correctly — this does NOT read as blocked.
  const IDLE_PROMPT = [
    '╭──────────────────────────────────────────────────────────╮',
    '│ ❯ Try "edit src/main/session.ts"                           │',
    '╰──────────────────────────────────────────────────────────╯',
    '  ⏵⏵ accept edits on (shift+tab to cycle)',
    ''
  ].join('\n');

  // Mid-turn: the agent is actively working (spinner + token counter). Not blocked.
  const WORKING = [
    '● Reading session.ts…',
    '  ⎿ 173 lines',
    '',
    '✶ Crafting response… (12s · ↑ 2.1k tokens)',
    ''
  ].join('\n');

  // A selection menu whose last visible line IS the cursored option (no footer):
  // a genuine "blocked on a decision" state the ❯ caret is meant to catch.
  const MENU_TAIL = ['Select a model:', '  1. Opus', '❯ 2. Sonnet'].join('\n');

  it('flags the blocking trust dialog (the previously-unverified positive path)', () => {
    expect(detectAwaitingInput(TRUST_DIALOG)).toBe(true);
  });

  it('flags a cursored menu selection awaiting a choice', () => {
    expect(detectAwaitingInput(MENU_TAIL)).toBe(true);
  });

  it('flags an inline confirm at the bottom of a transcript', () => {
    expect(detectAwaitingInput('Applied 3 edits across 2 files.\nRun the tests now? (y/n)')).toBe(true);
  });

  it('does NOT flag the idle prompt box — idle is not "blocked on a decision"', () => {
    expect(detectAwaitingInput(IDLE_PROMPT)).toBe(false);
  });

  it('does NOT flag an agent that is mid-turn / working', () => {
    expect(detectAwaitingInput(WORKING)).toBe(false);
  });
});
