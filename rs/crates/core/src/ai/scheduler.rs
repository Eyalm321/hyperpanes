//! Port of `src/main/ai/scheduler.ts` — the queue/timing state machine deciding WHEN to
//! summarize a pane (FIFO + coalesce + backoff + staleness). Pure; inject the clock for
//! tests. Mirror `scheduler.test.ts`.
//!
//! STUB — owned by track `ai`.
