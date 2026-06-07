//! Port of `src/main/control-lock.ts` — advisory per-pane write locks: TTL,
//! holder/owner tracking, acquire/refresh/release, non-owner rejection.
//! Mirror every case in `control-lock.test.ts`.
//!
//! Advisory per-pane write locks (agent-orchestration H). When several managers
//! might drive the same pane, a holder takes a short-lived lock so `send_input`
//! from anyone else is refused until it expires or is released. ADVISORY: an
//! unlocked pane is writable by anyone (preserves the single-orchestrator case);
//! the lock only bites once someone has explicitly claimed the pane.
//!
//! Pure + clock-injected (`now` passed in) so it's deterministic in tests.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockState {
    pub owner: String,
    /// ms epoch
    pub expires_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockResult {
    pub ok: bool,
    /// current holder (the requester on success, else the blocker)
    pub owner: String,
    pub expires_at: i64,
}

#[derive(Debug, Default)]
pub struct PaneLocks {
    locks: HashMap<String, LockState>,
}

impl PaneLocks {
    pub fn new() -> Self {
        Self::default()
    }

    /// Acquire/renew. Succeeds if the pane is free, the prior lock has expired, or
    /// the requester already holds it (renew). Fails (`ok:false`) if a *different*
    /// owner holds an unexpired lock — the result names the blocking holder.
    pub fn acquire(&mut self, pane_id: &str, owner: &str, now: i64, ttl_ms: i64) -> LockResult {
        if let Some(cur) = self.locks.get(pane_id) {
            if cur.expires_at > now && cur.owner != owner {
                return LockResult {
                    ok: false,
                    owner: cur.owner.clone(),
                    expires_at: cur.expires_at,
                };
            }
        }
        let expires_at = now + ttl_ms.max(0);
        self.locks.insert(
            pane_id.to_string(),
            LockState {
                owner: owner.to_string(),
                expires_at,
            },
        );
        LockResult {
            ok: true,
            owner: owner.to_string(),
            expires_at,
        }
    }

    /// Release. Only the holder may release; an expired/absent lock counts as freed.
    pub fn release(&mut self, pane_id: &str, owner: &str, now: i64) -> bool {
        match self.locks.get(pane_id) {
            None => {
                self.locks.remove(pane_id);
                true
            }
            Some(cur) if cur.expires_at <= now => {
                self.locks.remove(pane_id);
                true
            }
            Some(cur) if cur.owner != owner => false,
            Some(_) => {
                self.locks.remove(pane_id);
                true
            }
        }
    }

    /// The current unexpired holder, or `None` if the pane is free. `send_input`
    /// uses this: free → anyone writes; held → only that owner writes.
    pub fn holder(&self, pane_id: &str, now: i64) -> Option<String> {
        match self.locks.get(pane_id) {
            Some(cur) if cur.expires_at > now => Some(cur.owner.clone()),
            _ => None,
        }
    }

    /// Forget a pane's lock (on close).
    pub fn drop(&mut self, pane_id: &str) {
        self.locks.remove(pane_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unlocked_pane_has_no_holder() {
        let locks = PaneLocks::new();
        assert_eq!(locks.holder("p", 1000), None);
    }

    #[test]
    fn acquire_blocks_a_different_owner_until_expiry() {
        let mut locks = PaneLocks::new();
        let a = locks.acquire("p", "mgrA", 1000, 5000); // holds until 6000
        assert!(a.ok);
        assert_eq!(a.owner, "mgrA");
        assert_eq!(a.expires_at, 6000);
        assert_eq!(locks.holder("p", 2000), Some("mgrA".to_string()));

        // A different owner is refused while the lock is live, told who blocks.
        let b = locks.acquire("p", "mgrB", 2000, 5000);
        assert!(!b.ok);
        assert_eq!(b.owner, "mgrA");

        // After expiry the pane is free; mgrB can take it.
        assert_eq!(locks.holder("p", 7000), None);
        assert!(locks.acquire("p", "mgrB", 7000, 1000).ok);
    }

    #[test]
    fn the_holder_may_renew_its_own_lock() {
        let mut locks = PaneLocks::new();
        locks.acquire("p", "mgr", 1000, 1000); // expires 2000
        let renew = locks.acquire("p", "mgr", 1500, 1000); // extend to 2500
        assert!(renew.ok);
        assert_eq!(renew.owner, "mgr");
        assert_eq!(renew.expires_at, 2500);
    }

    #[test]
    fn only_the_holder_may_release_expired_or_absent_counts_as_freed() {
        let mut locks = PaneLocks::new();
        locks.acquire("p", "mgr", 1000, 5000);
        assert!(!locks.release("p", "intruder", 2000));
        assert!(locks.release("p", "mgr", 2000));
        assert_eq!(locks.holder("p", 2000), None);
        // Releasing a free pane is a no-op success.
        assert!(locks.release("free", "anyone", 0));
    }
}
