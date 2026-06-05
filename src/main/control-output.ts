// Pure cores for the control API's read path (interactive-pane-driving plan,
// legs B1/B2). Kept free of timers, sockets, and the session store so they're
// unit-testable; the control server wires them to its per-session output
// tracking and the HTTP `/panes/:id/output` route.

// Defaults for `read_pane({ waitForIdle })`. The settle window is DELIBERATELY
// short — a single interactive turn — and unrelated to useIdle's 10 s glow
// threshold (idleAlertSeconds), which is far too slow for chat. The activity
// busy→idle flip is the coarse signal; settleMs is the fine one.
export const DEFAULT_SETTLE_MS = 600;
export const DEFAULT_WAIT_TIMEOUT_MS = 30_000;
// How often the server re-checks quiescence while waiting. Bounded so a long
// settle window doesn't spin and a short one still resolves promptly.
export const WAIT_POLL_MIN_MS = 25;
export const WAIT_POLL_MAX_MS = 100;

export type WaitVerdict = 'settled' | 'timeout' | 'wait';

// Decide, from the current tracking snapshot, whether a `waitForIdle` read should
// resolve now (output quiet for settleMs), give up (timed out), or keep waiting.
//
// `since` makes the wait composable for a type→read turn (prompt_pane): when
// given, the read won't settle until output has actually advanced PAST that
// cursor — i.e. the agent has begun replying — so a stale-but-quiet screen from
// before the prompt can't satisfy the wait. Without `since`, an already-quiet
// pane settles immediately (the plain "wait until it's done" case).
export function waitDecision(args: {
  lastOutputAt: number | undefined; // ms epoch of the pane's last pty output, or undefined if none yet
  totalBytes: number; // monotonic count of bytes ever emitted by the pane
  since: number | undefined; // require totalBytes to exceed this before settling
  now: number;
  start: number; // ms epoch the wait began
  settleMs: number;
  timeoutMs: number;
}): WaitVerdict {
  const { lastOutputAt, totalBytes, since, now, start, settleMs, timeoutMs } = args;
  const advanced = since === undefined || totalBytes > since;
  // A pane that has never produced output counts as quiet (nothing is streaming).
  const quiet = lastOutputAt === undefined || now - lastOutputAt >= settleMs;
  if (advanced && quiet) return 'settled';
  if (now - start >= timeoutMs) return 'timeout';
  return 'wait';
}

// How long to sleep before the next quiescence check: just past the point the
// pane would become quiet, clamped to the poll band and never overshooting the
// deadline. Pure so the cadence is testable.
export function nextPollDelay(args: {
  lastOutputAt: number | undefined;
  now: number;
  start: number;
  settleMs: number;
  timeoutMs: number;
}): number {
  const { lastOutputAt, now, start, settleMs, timeoutMs } = args;
  const untilQuiet = lastOutputAt === undefined ? WAIT_POLL_MIN_MS : settleMs - (now - lastOutputAt);
  const untilDeadline = timeoutMs - (now - start);
  // Aim for the quiet point, clamped to the poll band so we neither spin nor
  // sleep so long we lag a settle. But the deadline always wins — never sleep
  // past it (even below the band), so the wait returns its `timeout` on time.
  const target = Math.min(Math.max(untilQuiet, WAIT_POLL_MIN_MS), WAIT_POLL_MAX_MS);
  return Math.max(1, Math.min(target, untilDeadline));
}

export interface SinceSlice {
  output: string; // bytes produced since the cursor (best-effort; see truncated)
  cursor: number; // next cursor to pass back — the pane's current totalBytes
  truncated: boolean; // true if the cursor fell off the back of the replay buffer (output was lost)
}

// Return only the output produced since a byte cursor, against the pane's rolling
// replay buffer (which holds at most the last N bytes ever emitted). `totalBytes`
// is the monotonic count of ALL bytes emitted, so:
//   • since >= totalBytes  → nothing new (also covers a stale/ahead cursor);
//   • since within buffer  → the exact tail slice;
//   • since older than the buffer holds → the whole buffer, flagged truncated
//     (older output between the cursor and the buffer start was already evicted).
// The returned cursor is always totalBytes, so the next delta read continues cleanly.
export function sliceSince(replay: string, totalBytes: number, since: number): SinceSlice {
  if (since >= totalBytes) return { output: '', cursor: totalBytes, truncated: false };
  const newBytes = totalBytes - Math.max(since, 0);
  if (newBytes >= replay.length) {
    // The cursor predates the buffer's oldest retained byte (or since < 0): we
    // can't reconstruct the gap, so hand back everything we still hold.
    return { output: replay, cursor: totalBytes, truncated: newBytes > replay.length };
  }
  return { output: replay.slice(replay.length - newBytes), cursor: totalBytes, truncated: false };
}

// Patterns that mark a TUI's last visible line as a prompt WAITING for the user
// (interactive-pane-driving plan C2). Idle alone can't tell "agent finished" from
// "agent blocked on a y/n / trust prompt"; matched against the rendered screen's
// last non-empty line, these let an orchestrator know to ANSWER rather than wait
// forever. Deliberately conservative — explicit prompt markers, not any prose.
const AWAITING_INPUT_PATTERNS: RegExp[] = [
  /\(y\/n\)/i,
  /\[y\/n\]/i,
  /\(yes\/no\)/i,
  /press\s+(enter|return|any key)/i,
  /\benter to (confirm|continue)\b/i,
  /\bdo you (want|wish|trust)\b/i,
  /❯/, // claude's selection cursor / prompt-box caret (menus, trust dialog)
  /\?\s*$/ // the last line is itself a question
];

// Best-effort "is this pane blocked on a prompt?" over the RENDERED screen text
// (clean — run it on a mode:"screen" read, not the mangled raw stream). Looks at
// the last non-empty line only. A heuristic, not a guarantee.
export function detectAwaitingInput(screenText: string): boolean {
  const lines = screenText.split('\n');
  let i = lines.length - 1;
  while (i >= 0 && lines[i].trim() === '') i--;
  if (i < 0) return false;
  const last = lines[i].trim();
  return AWAITING_INPUT_PATTERNS.some((re) => re.test(last));
}
