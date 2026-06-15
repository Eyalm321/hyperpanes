//! Phase-5 auto-restart / retry supervisor for worker panes.
//!
//! The supervisor hooks `SessionEvent::Exit` (which, by design, fires ONLY on an
//! unsolicited death — a manual `closePane`/`restartPane` is silent, see the exit
//! plumbing in `session_manager`), and for panes that opted in (via the `meta` map)
//! decides whether to auto-restart with exponential backoff + a max-retries cap,
//! distinguishing a clean completion (exit code in `successCodes`) from a crash.
//!
//! This module is the PURE policy + ledger core: it makes decisions and records
//! attempts, but performs no I/O. The control server applies a [`Decision::Restart`]
//! by scheduling a delayed `sessions.create` (see `control::server`). Keeping the
//! brain pure makes backoff, the cap, and the clean-vs-crash split unit-testable with
//! no runtime, no pty, and no clock.
//!
//! ## Opt-in via `meta` (zero new spawn schema)
//! A pane opts in through the existing string→string `meta` map (`setMeta` / spawn
//! `pane.meta`). Default (`hp.supervise` unset / `off`) ⇒ every decision is
//! [`Decision::None`] and the `Exit` arm behaves exactly as it does today.
//!
//! | meta key          | default       | meaning                                            |
//! |-------------------|---------------|----------------------------------------------------|
//! | `hp.supervise`    | `off`         | `on`/`true` to supervise; anything else ⇒ off      |
//! | `hp.restartOn`    | `failure`     | `failure` (code∉success) · `always` · `never`      |
//! | `hp.successCodes` | `0`           | comma list, e.g. `0,2`                              |
//! | `hp.maxRetries`   | `5`           | give up after N restarts                           |
//! | `hp.backoffMs`    | `500`         | base delay                                         |
//! | `hp.backoffCapMs` | `30000`       | max delay                                          |
//! | `hp.backoff`      | `exponential` | `exponential` (base·2ⁿ) or `fixed`                 |

use std::collections::BTreeMap;
use std::collections::HashMap;

/// How an exit is treated relative to `successCodes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartOn {
    /// Restart only on a non-success exit code (the default).
    Failure,
    /// Always restart, even on a clean exit.
    Always,
    /// Never auto-restart (record the outcome and stop).
    Never,
}

/// Backoff growth shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// `min(cap, base << attempt)` — doubles each attempt.
    Exponential,
    /// Always `base`.
    Fixed,
}

/// A pane's parsed supervision policy. `enabled == false` ⇒ the legacy no-supervision path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub enabled: bool,
    pub restart_on: RestartOn,
    pub success_codes: Vec<i32>,
    pub max_retries: u32,
    pub backoff_ms: u64,
    pub backoff_cap_ms: u64,
    pub backoff: Backoff,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            enabled: false,
            restart_on: RestartOn::Failure,
            success_codes: vec![0],
            max_retries: 5,
            backoff_ms: 500,
            backoff_cap_ms: 30_000,
            backoff: Backoff::Exponential,
        }
    }
}

impl Policy {
    /// Parse a policy out of a pane's `meta` map. An absent/unparseable key falls back to
    /// the [`Default`] value, so a partial config is always valid.
    pub fn from_meta(meta: &BTreeMap<String, String>) -> Policy {
        let mut p = Policy::default();
        if let Some(v) = meta.get("hp.supervise") {
            p.enabled = matches!(v.trim().to_ascii_lowercase().as_str(), "on" | "true" | "1" | "yes");
        }
        if let Some(v) = meta.get("hp.restartOn") {
            p.restart_on = match v.trim().to_ascii_lowercase().as_str() {
                "always" => RestartOn::Always,
                "never" => RestartOn::Never,
                _ => RestartOn::Failure,
            };
        }
        if let Some(v) = meta.get("hp.successCodes") {
            let codes: Vec<i32> = v.split(',').filter_map(|s| s.trim().parse::<i32>().ok()).collect();
            if !codes.is_empty() {
                p.success_codes = codes;
            }
        }
        if let Some(v) = meta.get("hp.maxRetries").and_then(|v| v.trim().parse::<u32>().ok()) {
            p.max_retries = v;
        }
        if let Some(v) = meta.get("hp.backoffMs").and_then(|v| v.trim().parse::<u64>().ok()) {
            p.backoff_ms = v;
        }
        if let Some(v) = meta.get("hp.backoffCapMs").and_then(|v| v.trim().parse::<u64>().ok()) {
            p.backoff_cap_ms = v;
        }
        if let Some(v) = meta.get("hp.backoff") {
            p.backoff = match v.trim().to_ascii_lowercase().as_str() {
                "fixed" => Backoff::Fixed,
                _ => Backoff::Exponential,
            };
        }
        p
    }

    fn is_success(&self, code: i32) -> bool {
        self.success_codes.contains(&code)
    }

    /// The backoff delay (ms) for the `attempt`-th retry (0-based: the first retry uses
    /// `attempt = 0`). Exponential is `min(cap, base << attempt)`, saturating; fixed is
    /// always `base`. Pure and deterministic (jitter is added by the scheduler, not here,
    /// so the cap and growth are testable).
    pub fn delay_for(&self, attempt: u32) -> u64 {
        match self.backoff {
            Backoff::Fixed => self.backoff_ms,
            Backoff::Exponential => {
                let shifted = self.backoff_ms.checked_shl(attempt).unwrap_or(u64::MAX);
                shifted.min(self.backoff_cap_ms)
            }
        }
    }
}

/// What the supervisor decides to do about one exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Not supervised (or `restartOn=never` with the legacy semantics) — do nothing; the
    /// `Exit` arm runs exactly as it does today.
    None,
    /// A clean exit treated as a completion (e.g. `restartOn=failure`, code 0): record &
    /// surface, but do not restart.
    Completed { code: i32 },
    /// Out of retries — give up. The pane stays exited; surface via the `supervisor` frame.
    Exhausted { attempt: u32, max: u32, code: i32 },
    /// Restart after `delay_ms`. `attempt` is 1-based (the Nth restart). The caller
    /// schedules the respawn and, on success, calls [`Supervisor::record_restart`].
    Restart { attempt: u32, max: u32, delay_ms: u64, code: i32 },
}

/// Per-pane supervision state (the ledger entry). The full spawn recipe lives in the
/// caller's ledger; here we keep only what the policy engine needs.
#[derive(Debug, Clone)]
struct Entry {
    policy: Policy,
    retries_used: u32,
}

/// The supervisor: a per-pane ledger + the pure decision engine. Lives behind a `Mutex`
/// on the control server's `Shared`.
#[derive(Debug, Default)]
pub struct Supervisor {
    panes: HashMap<String, Entry>,
}

impl Supervisor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register / update a pane's policy (called at spawn and on `setMeta`). A disabled
    /// policy is still recorded so a later `setMeta` enabling it starts from a clean count.
    pub fn set_policy(&mut self, pane_id: &str, policy: Policy) {
        let entry = self.panes.entry(pane_id.to_string()).or_insert_with(|| Entry {
            policy: policy.clone(),
            retries_used: 0,
        });
        entry.policy = policy;
    }

    /// Forget a pane (on `closePane`).
    pub fn forget(&mut self, pane_id: &str) {
        self.panes.remove(pane_id);
    }

    /// Drop ledger entries for panes no longer in the live set (reconciliation after a
    /// `closePane`, where dispatch doesn't call into the supervisor directly).
    pub fn retain_panes(&mut self, live: &std::collections::HashSet<&str>) {
        self.panes.retain(|id, _| live.contains(id.as_str()));
    }

    /// Whether a pane is currently supervised (enabled policy present).
    pub fn is_supervised(&self, pane_id: &str) -> bool {
        self.panes.get(pane_id).is_some_and(|e| e.policy.enabled)
    }

    /// A health signal: the worker reached a prompt-ready / agent-done state, so it ran
    /// long enough to be considered healthy → reset its retry budget (phase-4 interlock).
    /// No-op for an unknown / unsupervised pane.
    pub fn note_healthy(&mut self, pane_id: &str) {
        if let Some(e) = self.panes.get_mut(pane_id) {
            e.retries_used = 0;
        }
    }

    /// Decide what to do about an exit, WITHOUT mutating the retry count (the caller bumps
    /// it via [`record_restart`] only once a respawn actually succeeds, so a failed respawn
    /// doesn't silently burn the budget twice). Pure given the current ledger.
    pub fn on_exit(&self, pane_id: &str, code: i32) -> Decision {
        let Some(e) = self.panes.get(pane_id) else {
            return Decision::None;
        };
        let p = &e.policy;
        if !p.enabled {
            return Decision::None;
        }
        match p.restart_on {
            RestartOn::Never => {
                // Record the terminal outcome; never restart. A clean code is a completion.
                if p.is_success(code) {
                    Decision::Completed { code }
                } else {
                    Decision::Exhausted { attempt: e.retries_used, max: p.max_retries, code }
                }
            }
            RestartOn::Failure if p.is_success(code) => Decision::Completed { code },
            RestartOn::Failure | RestartOn::Always => {
                if e.retries_used >= p.max_retries {
                    Decision::Exhausted { attempt: e.retries_used, max: p.max_retries, code }
                } else {
                    Decision::Restart {
                        attempt: e.retries_used + 1,
                        max: p.max_retries,
                        delay_ms: p.delay_for(e.retries_used),
                        code,
                    }
                }
            }
        }
    }

    /// Record that a scheduled restart actually fired (the respawn succeeded), bumping the
    /// retry counter. Returns the new count.
    pub fn record_restart(&mut self, pane_id: &str) -> u32 {
        match self.panes.get_mut(pane_id) {
            Some(e) => {
                e.retries_used += 1;
                e.retries_used
            }
            None => 0,
        }
    }

    /// The current retry count for a pane (test/observability helper).
    pub fn retries_used(&self, pane_id: &str) -> u32 {
        self.panes.get(pane_id).map(|e| e.retries_used).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn enabled_policy() -> Policy {
        Policy::from_meta(&meta(&[("hp.supervise", "on")]))
    }

    // ---- policy parsing ----

    #[test]
    fn default_meta_is_disabled() {
        let p = Policy::from_meta(&meta(&[]));
        assert!(!p.enabled);
        assert_eq!(p.restart_on, RestartOn::Failure);
        assert_eq!(p.success_codes, vec![0]);
        assert_eq!(p.max_retries, 5);
    }

    #[test]
    fn parses_full_meta() {
        let p = Policy::from_meta(&meta(&[
            ("hp.supervise", "true"),
            ("hp.restartOn", "always"),
            ("hp.successCodes", "0,2"),
            ("hp.maxRetries", "3"),
            ("hp.backoffMs", "100"),
            ("hp.backoffCapMs", "5000"),
            ("hp.backoff", "fixed"),
        ]));
        assert!(p.enabled);
        assert_eq!(p.restart_on, RestartOn::Always);
        assert_eq!(p.success_codes, vec![0, 2]);
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.backoff_ms, 100);
        assert_eq!(p.backoff_cap_ms, 5000);
        assert_eq!(p.backoff, Backoff::Fixed);
    }

    // ---- backoff math (exponential growth + cap + fixed) ----

    #[test]
    fn exponential_backoff_doubles_then_caps() {
        let p = Policy {
            backoff_ms: 500,
            backoff_cap_ms: 30_000,
            backoff: Backoff::Exponential,
            ..Policy::default()
        };
        assert_eq!(p.delay_for(0), 500); // 500 << 0
        assert_eq!(p.delay_for(1), 1000); // 500 << 1
        assert_eq!(p.delay_for(2), 2000);
        assert_eq!(p.delay_for(3), 4000);
        assert_eq!(p.delay_for(6), 30_000); // 500<<6=32000 -> capped to 30000
        assert_eq!(p.delay_for(10), 30_000); // capped
        // Huge attempt must not overflow/panic — saturates to the cap.
        assert_eq!(p.delay_for(1000), 30_000);
    }

    #[test]
    fn fixed_backoff_is_constant() {
        let p = Policy { backoff_ms: 250, backoff: Backoff::Fixed, ..Policy::default() };
        assert_eq!(p.delay_for(0), 250);
        assert_eq!(p.delay_for(5), 250);
    }

    // ---- clean vs crash ----

    #[test]
    fn unsupervised_pane_is_always_none() {
        let mut s = Supervisor::new();
        s.set_policy("p1", Policy::default()); // disabled
        assert_eq!(s.on_exit("p1", 1), Decision::None);
        assert_eq!(s.on_exit("unknown", 1), Decision::None);
    }

    #[test]
    fn clean_exit_with_restart_on_failure_is_completed_not_restarted() {
        let mut s = Supervisor::new();
        s.set_policy("p1", enabled_policy());
        assert_eq!(s.on_exit("p1", 0), Decision::Completed { code: 0 });
    }

    #[test]
    fn crash_is_restarted_with_backoff() {
        let mut s = Supervisor::new();
        s.set_policy("p1", enabled_policy()); // base 500, exp
        // First crash → attempt 1, delay 500 (500 << 0).
        assert_eq!(
            s.on_exit("p1", 1),
            Decision::Restart { attempt: 1, max: 5, delay_ms: 500, code: 1 }
        );
    }

    #[test]
    fn restart_on_always_restarts_even_on_clean_exit() {
        let mut s = Supervisor::new();
        s.set_policy("p1", Policy::from_meta(&meta(&[("hp.supervise", "on"), ("hp.restartOn", "always")])));
        assert!(matches!(s.on_exit("p1", 0), Decision::Restart { .. }));
    }

    #[test]
    fn restart_on_never_completes_clean_and_exhausts_crash() {
        let mut s = Supervisor::new();
        s.set_policy("p1", Policy::from_meta(&meta(&[("hp.supervise", "on"), ("hp.restartOn", "never")])));
        assert_eq!(s.on_exit("p1", 0), Decision::Completed { code: 0 });
        assert!(matches!(s.on_exit("p1", 9), Decision::Exhausted { .. }));
    }

    #[test]
    fn success_codes_list_is_respected() {
        let mut s = Supervisor::new();
        s.set_policy("p1", Policy::from_meta(&meta(&[("hp.supervise", "on"), ("hp.successCodes", "0,2")])));
        // 2 is a "success" → completion, not a restart.
        assert_eq!(s.on_exit("p1", 2), Decision::Completed { code: 2 });
        // 1 is not → restart.
        assert!(matches!(s.on_exit("p1", 1), Decision::Restart { .. }));
    }

    // ---- max-retries cap + the record/decide split ----

    #[test]
    fn retries_climb_then_exhaust_at_the_cap() {
        let mut s = Supervisor::new();
        s.set_policy(
            "p1",
            Policy::from_meta(&meta(&[("hp.supervise", "on"), ("hp.maxRetries", "2"), ("hp.backoffMs", "10")])),
        );
        // Crash 1: attempt 1, delay 10<<0=10. Caller respawns → record.
        assert_eq!(
            s.on_exit("p1", 1),
            Decision::Restart { attempt: 1, max: 2, delay_ms: 10, code: 1 }
        );
        assert_eq!(s.record_restart("p1"), 1);
        // Crash 2: attempt 2, delay 10<<1=20.
        assert_eq!(
            s.on_exit("p1", 1),
            Decision::Restart { attempt: 2, max: 2, delay_ms: 20, code: 1 }
        );
        assert_eq!(s.record_restart("p1"), 2);
        // Crash 3: retries_used (2) >= max (2) → exhausted.
        assert_eq!(
            s.on_exit("p1", 1),
            Decision::Exhausted { attempt: 2, max: 2, code: 1 }
        );
    }

    #[test]
    fn on_exit_does_not_mutate_so_a_failed_respawn_can_retry_same_attempt() {
        let mut s = Supervisor::new();
        s.set_policy("p1", enabled_policy());
        // Deciding twice without record_restart yields the SAME attempt/delay (no burn).
        let d1 = s.on_exit("p1", 1);
        let d2 = s.on_exit("p1", 1);
        assert_eq!(d1, d2);
        assert_eq!(s.retries_used("p1"), 0);
    }

    // ---- phase-4 health interlock ----

    #[test]
    fn note_healthy_resets_the_retry_budget() {
        let mut s = Supervisor::new();
        s.set_policy(
            "p1",
            Policy::from_meta(&meta(&[("hp.supervise", "on"), ("hp.maxRetries", "1")])),
        );
        s.record_restart("p1");
        assert_eq!(s.retries_used("p1"), 1);
        // A healthy prompt-ready edge resets the count → budget restored.
        s.note_healthy("p1");
        assert_eq!(s.retries_used("p1"), 0);
        // So the next crash restarts again rather than exhausting.
        assert!(matches!(s.on_exit("p1", 1), Decision::Restart { .. }));
    }

    #[test]
    fn forget_drops_the_pane() {
        let mut s = Supervisor::new();
        s.set_policy("p1", enabled_policy());
        assert!(s.is_supervised("p1"));
        s.forget("p1");
        assert!(!s.is_supervised("p1"));
        assert_eq!(s.on_exit("p1", 1), Decision::None);
    }
}
