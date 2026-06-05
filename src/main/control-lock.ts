// ---------------------------------------------------------------------------
// Advisory per-pane write locks (agent-orchestration H). When several managers
// might drive the same pane, a holder takes a short-lived lock so `send_input`
// from anyone else is refused until it expires or is released. ADVISORY: an
// unlocked pane is writable by anyone (preserves the single-orchestrator case);
// the lock only bites once someone has explicitly claimed the pane.
//
// Pure + clock-injected (now passed in) so it's deterministic in tests.
// ---------------------------------------------------------------------------

export interface LockState {
  owner: string;
  expiresAt: number; // ms epoch
}

export interface LockResult {
  ok: boolean;
  owner: string; // current holder (the requester on success, else the blocker)
  expiresAt: number;
}

export class PaneLocks {
  private locks = new Map<string, LockState>();

  // Acquire/renew. Succeeds if the pane is free, the prior lock has expired, or
  // the requester already holds it (renew). Fails (ok:false) if a *different*
  // owner holds an unexpired lock — the result names the blocking holder.
  acquire(paneId: string, owner: string, now: number, ttlMs: number): LockResult {
    const cur = this.locks.get(paneId);
    if (cur && cur.expiresAt > now && cur.owner !== owner) {
      return { ok: false, owner: cur.owner, expiresAt: cur.expiresAt };
    }
    const state: LockState = { owner, expiresAt: now + Math.max(0, ttlMs) };
    this.locks.set(paneId, state);
    return { ok: true, owner, expiresAt: state.expiresAt };
  }

  // Release. Only the holder may release; an expired/absent lock counts as freed.
  release(paneId: string, owner: string, now: number): boolean {
    const cur = this.locks.get(paneId);
    if (!cur || cur.expiresAt <= now) {
      this.locks.delete(paneId);
      return true;
    }
    if (cur.owner !== owner) return false;
    this.locks.delete(paneId);
    return true;
  }

  // The current unexpired holder, or null if the pane is free. `send_input` uses
  // this: free → anyone writes; held → only that owner writes.
  holder(paneId: string, now: number): string | null {
    const cur = this.locks.get(paneId);
    if (!cur || cur.expiresAt <= now) return null;
    return cur.owner;
  }

  // Forget a pane's lock (on close).
  drop(paneId: string): void {
    this.locks.delete(paneId);
  }
}
