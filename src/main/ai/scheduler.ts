// The timing/queue engine for the ambient-AI feature. It decides *when* a pane
// gets (re)summarized; it knows nothing about Ollama, buffers, or panes — the
// actual work is the injected `runJob(uid)` callback (see ai-service.ts).
//
// Model (from the plan):
//   - activity-driven: each output (noteOutput) re-arms a per-uid settle timer;
//     when a pane goes quiet for `settleMs`, it's enqueued once.
//   - safety tick: a slow interval re-enqueues panes that haven't summarized
//     within `maxStalenessSec` (covers panes that keep dribbling output so the
//     settle timer never fires). runJob itself skips panes with nothing new.
//   - single global FIFO with per-pane coalescing: a uid already queued or
//     in-flight is never double-queued; output during a job sets a rerun flag.
//   - concurrency cap: at most `concurrency` jobs run at once.
//   - backoff: a failed job pauses the queue with exponential backoff (the head
//     is retried) instead of hammering an unreachable server.
//   - status: online/offline transitions are reported once (not per job).

export type JobResult = 'ok' | 'fail' | 'skip';

export interface SchedulerConfig {
  settleMs: number;
  maxStalenessSec: number;
  concurrency: number;
}

export interface SchedulerDeps {
  runJob: (uid: string) => Promise<JobResult>;
  onStatus?: (online: boolean, lastError?: string) => void;
  now?: () => number;
}

const BACKOFF_MIN_MS = 2000;
const BACKOFF_MAX_MS = 300_000;
const STALENESS_TICK_MS = 30_000;

export class SummaryScheduler {
  private cfg: SchedulerConfig;
  private readonly runJob: (uid: string) => Promise<JobResult>;
  private readonly onStatus?: (online: boolean, lastError?: string) => void;
  private readonly now: () => number;

  private readonly settleTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private readonly queue: string[] = []; // FIFO of uids waiting to run
  private readonly queued = new Set<string>(); // membership mirror of `queue`
  private readonly inFlight = new Set<string>(); // uids currently running
  private readonly rerun = new Set<string>(); // output arrived while in-flight
  private readonly known = new Set<string>(); // uids we've seen output from
  private readonly lastSummaryAt = new Map<string, number>();

  private backoffMs = 0;
  private backoffTimer: ReturnType<typeof setTimeout> | null = null;
  private stalenessTimer: ReturnType<typeof setInterval> | null = null;
  private lastOnline: boolean | null = null;
  private running = false;

  constructor(cfg: SchedulerConfig, deps: SchedulerDeps) {
    this.cfg = cfg;
    this.runJob = deps.runJob;
    this.onStatus = deps.onStatus;
    this.now = deps.now ?? (() => Date.now());
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    this.stalenessTimer = setInterval(() => this.staleTick(), STALENESS_TICK_MS);
    if (typeof this.stalenessTimer === 'object' && 'unref' in this.stalenessTimer) {
      this.stalenessTimer.unref();
    }
  }

  // Stop scheduling and drop all pending timers/queue. In-flight jobs are left to
  // settle on their own (their results are ignored once stopped).
  stop(): void {
    this.running = false;
    for (const t of this.settleTimers.values()) clearTimeout(t);
    this.settleTimers.clear();
    if (this.backoffTimer) clearTimeout(this.backoffTimer);
    this.backoffTimer = null;
    if (this.stalenessTimer) clearInterval(this.stalenessTimer);
    this.stalenessTimer = null;
    this.queue.length = 0;
    this.queued.clear();
    this.rerun.clear();
    this.backoffMs = 0;
  }

  setConfig(cfg: SchedulerConfig): void {
    this.cfg = cfg;
  }

  // A pane produced output: remember it and (re)arm its settle timer.
  noteOutput(uid: string): void {
    if (!this.running) return;
    this.known.add(uid);
    if (this.inFlight.has(uid)) {
      this.rerun.add(uid); // re-run after the current job finishes
      return;
    }
    const existing = this.settleTimers.get(uid);
    if (existing) clearTimeout(existing);
    const t = setTimeout(() => {
      this.settleTimers.delete(uid);
      this.enqueue(uid);
    }, this.cfg.settleMs);
    if (typeof t === 'object' && 'unref' in t) t.unref();
    this.settleTimers.set(uid, t);
  }

  // A pane is gone (session exit / no longer published): forget all state for it.
  forget(uid: string): void {
    const t = this.settleTimers.get(uid);
    if (t) clearTimeout(t);
    this.settleTimers.delete(uid);
    this.known.delete(uid);
    this.rerun.delete(uid);
    this.lastSummaryAt.delete(uid);
    if (this.queued.has(uid)) {
      this.queued.delete(uid);
      const i = this.queue.indexOf(uid);
      if (i !== -1) this.queue.splice(i, 1);
    }
  }

  private enqueue(uid: string): void {
    if (!this.running) return;
    if (this.queued.has(uid) || this.inFlight.has(uid)) {
      if (this.inFlight.has(uid)) this.rerun.add(uid);
      return;
    }
    this.queue.push(uid);
    this.queued.add(uid);
    this.pump();
  }

  private pump(): void {
    if (!this.running || this.backoffTimer) return;
    while (this.inFlight.size < this.cfg.concurrency && this.queue.length > 0) {
      const uid = this.queue.shift()!;
      this.queued.delete(uid);
      this.inFlight.add(uid);
      void this.execute(uid);
    }
  }

  private async execute(uid: string): Promise<void> {
    let result: JobResult;
    try {
      result = await this.runJob(uid);
    } catch {
      result = 'fail';
    }
    this.inFlight.delete(uid);
    if (!this.running) return;

    if (result === 'ok') {
      this.lastSummaryAt.set(uid, this.now());
      this.resetBackoff();
      this.report(true);
    } else if (result === 'fail') {
      this.report(false);
      this.scheduleBackoff(uid); // retry the head after a growing delay
      return; // pump resumes when the backoff timer fires
    }
    // 'ok' or 'skip': if more output arrived mid-job, re-queue this pane.
    if (this.rerun.delete(uid)) this.enqueue(uid);
    this.pump();
  }

  // Re-enqueue panes that haven't summarized within the staleness window. runJob
  // skips those with nothing new, so this is a cheap backstop for slow-dribbling
  // panes whose settle timer keeps getting pushed out.
  private staleTick(): void {
    const cutoff = this.now() - this.cfg.maxStalenessSec * 1000;
    for (const uid of this.known) {
      if ((this.lastSummaryAt.get(uid) ?? 0) <= cutoff) this.enqueue(uid);
    }
  }

  private scheduleBackoff(uid: string): void {
    if (!this.queued.has(uid)) {
      this.queue.unshift(uid); // retry this one first
      this.queued.add(uid);
    }
    this.backoffMs = Math.min(Math.max(BACKOFF_MIN_MS, this.backoffMs * 2), BACKOFF_MAX_MS);
    if (this.backoffTimer) clearTimeout(this.backoffTimer);
    this.backoffTimer = setTimeout(() => {
      this.backoffTimer = null;
      this.pump();
    }, this.backoffMs);
    if (typeof this.backoffTimer === 'object' && 'unref' in this.backoffTimer) {
      this.backoffTimer.unref();
    }
  }

  private resetBackoff(): void {
    this.backoffMs = 0;
    if (this.backoffTimer) {
      clearTimeout(this.backoffTimer);
      this.backoffTimer = null;
    }
  }

  private report(online: boolean, lastError?: string): void {
    if (this.lastOnline === online) return;
    this.lastOnline = online;
    this.onStatus?.(online, lastError);
  }
}
