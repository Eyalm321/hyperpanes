import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { SummaryScheduler, type JobResult } from './scheduler';

// The scheduler is pure timing/queue logic, so drive it with fake timers.
// advanceTimersByTimeAsync also flushes the microtasks between async runJob steps.
describe('SummaryScheduler', () => {
  beforeEach(() => vi.useFakeTimers());
  afterEach(() => vi.useRealTimers());

  it('summarizes a pane once after it goes quiet (settle), resetting on new output', async () => {
    const runs: string[] = [];
    const s = new SummaryScheduler(
      { settleMs: 100, maxStalenessSec: 9999, concurrency: 1 },
      { runJob: async (uid) => (runs.push(uid), 'ok') }
    );
    s.start();
    s.noteOutput('a');
    await vi.advanceTimersByTimeAsync(60);
    s.noteOutput('a'); // more output → settle timer restarts
    await vi.advanceTimersByTimeAsync(60);
    expect(runs).toEqual([]); // not quiet for a full 100ms since the last output
    await vi.advanceTimersByTimeAsync(50);
    expect(runs).toEqual(['a']); // fired once after the pane finally settled
    s.stop();
  });

  it('caps concurrency across panes', async () => {
    let inFlight = 0;
    let peak = 0;
    const release: Array<() => void> = [];
    const s = new SummaryScheduler(
      { settleMs: 10, maxStalenessSec: 9999, concurrency: 2 },
      {
        runJob: async () => {
          inFlight++;
          peak = Math.max(peak, inFlight);
          await new Promise<void>((res) => release.push(res));
          inFlight--;
          return 'ok' as JobResult;
        }
      }
    );
    s.start();
    for (const uid of ['a', 'b', 'c', 'd']) s.noteOutput(uid);
    await vi.advanceTimersByTimeAsync(10); // all four settle, but only 2 may run at once
    expect(peak).toBe(2);
    release.forEach((r) => r());
    await vi.advanceTimersByTimeAsync(0);
    expect(peak).toBe(2); // the other two ran after slots freed, still capped
    s.stop();
  });

  it('goes offline on failure then back online on recovery (after backoff)', async () => {
    const statuses: boolean[] = [];
    let result: JobResult = 'fail';
    const s = new SummaryScheduler(
      { settleMs: 10, maxStalenessSec: 9999, concurrency: 1 },
      { runJob: async () => result, onStatus: (online) => statuses.push(online) }
    );
    s.start();
    s.noteOutput('a');
    await vi.advanceTimersByTimeAsync(10); // runs → fail → offline + backoff armed
    expect(statuses).toEqual([false]);
    result = 'ok';
    await vi.advanceTimersByTimeAsync(2000); // backoff (min 2s) elapses → retry head → ok
    expect(statuses).toEqual([false, true]);
    s.stop();
  });

  it('skips work cleanly without flipping status', async () => {
    const statuses: boolean[] = [];
    const s = new SummaryScheduler(
      { settleMs: 10, maxStalenessSec: 9999, concurrency: 1 },
      { runJob: async () => 'skip', onStatus: (online) => statuses.push(online) }
    );
    s.start();
    s.noteOutput('a');
    await vi.advanceTimersByTimeAsync(10);
    expect(statuses).toEqual([]); // a skip is neither online nor offline
    s.stop();
  });
});
