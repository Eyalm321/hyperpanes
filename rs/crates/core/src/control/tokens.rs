//! Master + scoped bearer tokens for the control API. Master = 32 random bytes hex,
//! compared in constant time via `subtle` (timingSafeEqual parity). Scoped tokens limit
//! reach (uses `crate::control::scope`) with an optional TTL; mint via POST /tokens with
//! NO privilege escalation (`scope::checkMintable`). A scoped token minted into a child's
//! env must suppress `HYPERPANES_CONTROL_FILE` (see session::spawn).
//!
//! Port of the token half of `src/main/control-server.ts`:
//!   * `start()` mints the master token (`randomBytes(32).toString('hex')`) — [`random_token`].
//!   * `tokenMatches` is the length-guarded `timingSafeEqual` — [`TokenStore::token_matches`].
//!   * `resolveToken` resolves a presented bearer to its authority, expiring TTL'd scoped
//!     tokens lazily — [`TokenStore::resolve`].
//!   * `mintToken` registers a scoped token with an optional `Date.now()+ttlMs` expiry —
//!     [`TokenStore::mint`].
//!
//! The no-escalation check itself is `scope::check_mintable`, applied by the routes layer
//! against the live read-model.

use std::collections::HashMap;

use rand::RngCore;
use subtle::ConstantTimeEq;

use crate::control::scope::Scope;

/// A bearer token's authority. `None` scope = master (unscoped); a `Scope` limits it.
/// `expires_at` is ms-epoch; `None` = no expiry (the master token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenInfo {
    pub scope: Option<Scope>,
    pub expires_at: Option<i64>,
}

/// 32 random bytes as lowercase hex — byte-identical to the TS
/// `randomBytes(32).toString('hex')` master/scoped token shape (64 hex chars).
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    to_hex(&bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

/// A paired device's credential: an unscoped (full-authority) token, distinct from the master,
/// tagged with a human `label` so it can be listed and individually revoked. Unlike scoped
/// tokens these are PERSISTED (`device-tokens.json`) and reloaded on start, so a phone paired
/// via `hyperpanes pair` survives a host restart — mirroring how the master token is itself
/// persisted for remote binds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub label: String,
    pub expires_at: Option<i64>,
}

/// The control server's token table: the unscoped master (written to `control.json`), any minted
/// scoped tokens (ephemeral — cleared on stop, like TS `stop()`), and any paired device tokens
/// (unscoped + persisted, keyed by token → [`DeviceInfo`]).
#[derive(Debug, Default)]
pub struct TokenStore {
    master: Option<String>,
    scoped: HashMap<String, TokenInfo>,
    devices: HashMap<String, DeviceInfo>,
}

impl TokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a freshly-minted master token (called on server start).
    pub fn set_master(&mut self, token: String) {
        self.master = Some(token);
    }

    /// The master token string (for writing `control.json`), or `None` before start.
    pub fn master(&self) -> Option<&str> {
        self.master.as_deref()
    }

    /// Forget every token (server stop): the master is nulled and scoped tokens cleared. Device
    /// tokens are cleared from memory too — they live on in `device-tokens.json` and are reloaded
    /// on the next start via [`Self::add_device`].
    pub fn clear(&mut self) {
        self.master = None;
        self.scoped.clear();
        self.devices.clear();
    }

    /// Constant-time master-token compare, length-guarded so `ct_eq` can't be fed mismatched
    /// slices (the TS `timingSafeEqual` guard). False if no master is set or `presented` empty.
    pub fn token_matches(&self, presented: &str) -> bool {
        match &self.master {
            Some(m) if !presented.is_empty() => {
                let a = presented.as_bytes();
                let b = m.as_bytes();
                a.len() == b.len() && a.ct_eq(b).into()
            }
            _ => false,
        }
    }

    /// Resolve a presented bearer to its authority: the master token (constant-time) →
    /// unscoped; a known, unexpired minted token → its scope; else `None`. Expired scoped
    /// tokens are pruned on access (`now` = ms epoch), exactly as TS `resolveToken` does.
    pub fn resolve(&mut self, presented: Option<&str>, now: i64) -> Option<TokenInfo> {
        let presented = presented?;
        if presented.is_empty() {
            return None;
        }
        if self.token_matches(presented) {
            return Some(TokenInfo {
                scope: None,
                expires_at: None,
            });
        }
        // Paired device tokens: unscoped (full authority, like the master) but TTL-prunable.
        if let Some(d) = self.devices.get(presented) {
            let expires_at = d.expires_at;
            if matches!(expires_at, Some(exp) if exp <= now) {
                self.devices.remove(presented);
                return None;
            }
            return Some(TokenInfo {
                scope: None,
                expires_at,
            });
        }
        let info = self.scoped.get(presented)?.clone();
        if let Some(exp) = info.expires_at {
            if exp <= now {
                self.scoped.remove(presented);
                return None;
            }
        }
        Some(info)
    }

    /// Mint + register a scoped token, optionally TTL'd. Returns the token and its expiry
    /// (`now + ttl_ms` when a positive TTL is given, else `None`).
    pub fn mint(&mut self, scope: Scope, ttl_ms: Option<i64>, now: i64) -> (String, Option<i64>) {
        let token = random_token();
        let expires_at = ttl_ms.filter(|&t| t > 0).map(|t| now + t);
        self.scoped.insert(
            token.clone(),
            TokenInfo {
                scope: Some(scope),
                expires_at,
            },
        );
        (token, expires_at)
    }

    /// Mint + register a persisted device token (unscoped, full authority), optionally TTL'd.
    /// Returns the token and its expiry (`now + ttl_ms` for a positive TTL, else `None`). The
    /// caller persists the table via [`Self::list_devices`] → `device-tokens.json`.
    pub fn mint_device(
        &mut self,
        label: String,
        ttl_ms: Option<i64>,
        now: i64,
    ) -> (String, Option<i64>) {
        let token = random_token();
        let expires_at = ttl_ms.filter(|&t| t > 0).map(|t| now + t);
        self.devices
            .insert(token.clone(), DeviceInfo { label, expires_at });
        (token, expires_at)
    }

    /// Register a device token loaded from `device-tokens.json` on start (mints nothing new).
    pub fn add_device(&mut self, token: String, label: String, expires_at: Option<i64>) {
        self.devices.insert(token, DeviceInfo { label, expires_at });
    }

    /// Revoke every device token carrying `label`. Returns how many were dropped (0 = no such
    /// label). Labels are not enforced-unique at mint, so all matches are removed together.
    pub fn revoke_device(&mut self, label: &str) -> usize {
        let before = self.devices.len();
        self.devices.retain(|_, d| d.label != label);
        before - self.devices.len()
    }

    /// Snapshot of paired devices as `(token, label, expires_at)`, for listing (`hyperpanes
    /// devices`) and for persisting the table to `device-tokens.json`.
    pub fn list_devices(&self) -> Vec<(String, String, Option<i64>)> {
        self.devices
            .iter()
            .map(|(tok, d)| (tok.clone(), d.label.clone(), d.expires_at))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope_panes(ids: &[&str]) -> Scope {
        Scope {
            pane_ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        }
    }

    #[test]
    fn random_token_is_64_lowercase_hex_chars() {
        let t = random_token();
        assert_eq!(t.len(), 64);
        assert!(t
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        // Two draws practically never collide.
        assert_ne!(random_token(), random_token());
    }

    #[test]
    fn master_resolves_unscoped_and_compares_constant_time() {
        let mut store = TokenStore::new();
        store.set_master("deadbeef".to_string());
        assert!(store.token_matches("deadbeef"));
        // A length mismatch must not panic and must be false.
        assert!(!store.token_matches("dead"));
        assert!(!store.token_matches("deadbee0"));
        assert!(!store.token_matches(""));
        let info = store.resolve(Some("deadbeef"), 0).expect("master resolves");
        assert_eq!(
            info,
            TokenInfo {
                scope: None,
                expires_at: None
            }
        );
    }

    #[test]
    fn unknown_or_absent_token_is_none() {
        let mut store = TokenStore::new();
        assert_eq!(store.resolve(Some("nope"), 0), None);
        assert_eq!(store.resolve(None, 0), None);
        store.set_master("m".to_string());
        assert_eq!(store.resolve(Some("other"), 0), None);
    }

    #[test]
    fn mints_a_scoped_token_resolvable_to_its_scope() {
        let mut store = TokenStore::new();
        let scope = scope_panes(&["p1"]);
        let (tok, exp) = store.mint(scope.clone(), None, 1000);
        assert_eq!(exp, None);
        let info = store.resolve(Some(&tok), 2000).expect("scoped resolves");
        assert_eq!(info.scope, Some(scope));
        assert_eq!(info.expires_at, None);
    }

    #[test]
    fn ttl_token_expires_and_is_pruned_on_access() {
        let mut store = TokenStore::new();
        let (tok, exp) = store.mint(scope_panes(&["p1"]), Some(500), 1000);
        assert_eq!(exp, Some(1500));
        // Before expiry: resolves.
        assert!(store.resolve(Some(&tok), 1499).is_some());
        // At/after expiry: gone, and pruned (a second look is still gone).
        assert_eq!(store.resolve(Some(&tok), 1500), None);
        assert_eq!(store.resolve(Some(&tok), 1499), None);
    }

    #[test]
    fn clear_forgets_master_and_scoped() {
        let mut store = TokenStore::new();
        store.set_master("m".to_string());
        let (tok, _) = store.mint(scope_panes(&["p"]), None, 0);
        store.clear();
        assert_eq!(store.master(), None);
        assert_eq!(store.resolve(Some("m"), 0), None);
        assert_eq!(store.resolve(Some(&tok), 0), None);
    }

    #[test]
    fn device_token_resolves_unscoped_full_authority() {
        let mut store = TokenStore::new();
        let (tok, exp) = store.mint_device("iphone".into(), None, 0);
        assert_eq!(exp, None);
        // A device token resolves like the master: unscoped (scope None) = full authority.
        let info = store.resolve(Some(&tok), 1000).expect("device resolves");
        assert_eq!(info.scope, None);
        assert_eq!(info.expires_at, None);
    }

    #[test]
    fn device_token_ttl_expires_and_is_pruned() {
        let mut store = TokenStore::new();
        let (tok, exp) = store.mint_device("ipad".into(), Some(500), 1000);
        assert_eq!(exp, Some(1500));
        assert!(store.resolve(Some(&tok), 1499).is_some());
        assert!(store.resolve(Some(&tok), 1500).is_none()); // pruned on access at expiry
        assert!(store.resolve(Some(&tok), 1600).is_none());
    }

    #[test]
    fn add_device_reloads_a_persisted_token() {
        let mut store = TokenStore::new();
        store.add_device("a".repeat(64), "restored".into(), None);
        let info = store
            .resolve(Some(&"a".repeat(64)), 0)
            .expect("reloaded resolves");
        assert_eq!(info.scope, None);
    }

    #[test]
    fn revoke_device_drops_all_matching_labels_and_reports_count() {
        let mut store = TokenStore::new();
        let (t1, _) = store.mint_device("phone".into(), None, 0);
        let (t2, _) = store.mint_device("phone".into(), None, 0); // same label, re-paired
        let (t3, _) = store.mint_device("laptop".into(), None, 0);
        assert_eq!(store.revoke_device("phone"), 2);
        assert_eq!(store.revoke_device("phone"), 0); // idempotent
        assert!(store.resolve(Some(&t1), 0).is_none());
        assert!(store.resolve(Some(&t2), 0).is_none());
        assert!(store.resolve(Some(&t3), 0).is_some()); // untouched
    }

    #[test]
    fn list_devices_snapshots_label_and_expiry() {
        let mut store = TokenStore::new();
        let (tok, _) = store.mint_device("iphone".into(), Some(500), 1000);
        let listed = store.list_devices();
        assert_eq!(listed.len(), 1);
        let (t, label, expires_at) = &listed[0];
        assert_eq!(t, &tok);
        assert_eq!(label, "iphone");
        assert_eq!(*expires_at, Some(1500));
    }

    #[test]
    fn clear_also_forgets_devices() {
        let mut store = TokenStore::new();
        let (tok, _) = store.mint_device("iphone".into(), None, 0);
        store.clear();
        assert!(store.resolve(Some(&tok), 0).is_none());
        assert!(store.list_devices().is_empty());
    }
}
