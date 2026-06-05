// ---------------------------------------------------------------------------
// Per-pane message inbox — the structured inter-node transport for the agent
// message bus (agent-orchestration E). Pure + in-memory so it's unit-testable
// without a running server; ControlServer owns one instance and feeds it the
// clock (Date.now) so this module stays deterministic.
//
// Delivery model (the open question the plan flagged, decided here):
//   • DURABLE — every message is kept in the target pane's queue, so a node that
//     connects late (or reconnects) still reads its backlog. Push on `/events` is
//     only a *nudge*; the read is the source of truth.
//   • AT-LEAST-ONCE, cursor-based — `read(paneId, afterSeq)` returns everything
//     with a higher monotonic seq, so a reader advances its own cursor. No acks,
//     no server-side "read" state (a pane may have several readers).
//   • BOUNDED — the oldest messages are evicted past MAX_PER_PANE so a chatty
//     sender can't grow memory without limit (the dropped count is observable).
// ---------------------------------------------------------------------------

export interface PaneMessage {
  seq: number; // global monotonic id; readers use it as a cursor
  to: string; // target paneId
  from: string; // sender id (a paneId, or a free-form orchestrator label)
  body: string;
  ts: number; // ms epoch, stamped by the caller
}

// Keep the last N messages per pane. Generous — these are small text payloads.
export const MAX_PER_PANE = 500;

export class MessageInbox {
  private byPane = new Map<string, PaneMessage[]>();
  private seq = 0;
  // Messages evicted by the per-pane cap, so callers can surface "you missed N".
  private dropped = new Map<string, number>();

  // Enqueue a message for `to`. Returns the stored message (with its seq).
  post(to: string, from: string, body: string, ts: number): PaneMessage {
    const msg: PaneMessage = { seq: ++this.seq, to, from, body, ts };
    const list = this.byPane.get(to);
    if (!list) {
      this.byPane.set(to, [msg]);
      return msg;
    }
    list.push(msg);
    if (list.length > MAX_PER_PANE) {
      const overflow = list.length - MAX_PER_PANE;
      list.splice(0, overflow);
      this.dropped.set(to, (this.dropped.get(to) ?? 0) + overflow);
    }
    return msg;
  }

  // Messages for `paneId` with seq > afterSeq (afterSeq=0 ⇒ all retained). The
  // returned array is a copy, ordered by seq ascending.
  read(paneId: string, afterSeq = 0): PaneMessage[] {
    const list = this.byPane.get(paneId);
    if (!list) return [];
    return afterSeq > 0 ? list.filter((m) => m.seq > afterSeq) : list.slice();
  }

  // How many messages were evicted for `paneId` by the cap (for "you missed N").
  droppedCount(paneId: string): number {
    return this.dropped.get(paneId) ?? 0;
  }

  // The highest seq currently retained for `paneId` (0 if empty) — a fresh
  // reader can start its cursor here to skip backlog.
  latestSeq(paneId: string): number {
    const list = this.byPane.get(paneId);
    return list && list.length ? list[list.length - 1].seq : 0;
  }

  // Forget a pane's inbox (on close). Keeps the dropped counter cleared too.
  drop(paneId: string): void {
    this.byPane.delete(paneId);
    this.dropped.delete(paneId);
  }
}
