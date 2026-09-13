//! The key that signs install records.
//!
//! [`KeyStore`] is the seam: the host asks for a key by id and gets back a [`SecretKey`]
//! that zeroes itself on drop, prints nothing under `Debug`, and has no `Display`.
//! Three stores ship:
//!
//! * [`KeyringKeyStore`] — the intended production home. The key lives in the platform
//!   keychain through the `keyring` crate (the same seam the license token uses), and a
//!   [`FileKeyStore`] rides along as the fallback for a host with no reachable keychain
//!   (a headless Linux box with no Secret Service, a locked login keychain) and as the
//!   migration origin for a key an older, file-only build already wrote. A key found on
//!   disk is **adopted, never regenerated** — a new key would fail to verify every record
//!   the old one signed — and its plaintext file is then removed. Wiring this store into
//!   the production marketplace path is a one-line change to a frozen `mod.rs`, filed for
//!   the orchestrator; until it lands the app still constructs [`FileKeyStore`] directly.
//! * [`FileKeyStore`] — the fallback. The key is 32 random bytes in an owner-only file
//!   under `<data_dir>/modules/keys/<key_id>`, created exclusively (`O_EXCL`) so two
//!   racing processes never clobber each other's key. On Windows the file is created
//!   plainly and then locked down with an owner-only DACL.
//! * [`MemoryKeyStore`] — for tests: keys live in a map and never touch disk.
//!
//! Key bytes are never logged, printed, or placed in an error, here or anywhere. The
//! file store writes the key with plain `std::fs` calls rather than
//! `paths::write_atomic_private`, whose `tracing::instrument(ret)` would record its
//! `contents` argument in a debug span. The keychain path leans on the caller's install
//! lock, not an atomic create, to settle a first-write race — the same choice the license
//! store makes; the file fallback keeps its own `O_EXCL` guard.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The key id every install record is signed under today.
pub const DEFAULT_KEY_ID: &str = "install-record-v1";

/// Size of a generated key, in bytes (256 bits for HMAC-SHA-256).
pub const KEY_LEN: usize = 32;

/// Raw HMAC key bytes. Zeroed on drop; `Debug` prints only the length.
pub struct SecretKey(Vec<u8>);

impl SecretKey {
    /// Wrap key bytes.
    pub fn new(bytes: Vec<u8>) -> Self {
        SecretKey(bytes)
    }

    /// Generate [`KEY_LEN`] random bytes from the OS RNG.
    pub fn generate() -> Self {
        use rand::Rng;
        let mut bytes = vec![0u8; KEY_LEN];
        rand::rng().fill_bytes(&mut bytes);
        SecretKey(bytes)
    }

    /// The bytes, for `SignedInstallRecord::sign` / `verify`.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

/// Overwrite `bytes` with zeros in a way the optimizer cannot elide, even though the
/// buffer is about to be freed.
pub(super) fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusively borrowed `u8`.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

impl Drop for SecretKey {
    fn drop(&mut self) {
        wipe(&mut self.0);
    }
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretKey([redacted; {} bytes])", self.0.len())
    }
}

/// Why a key could not be produced. Never carries key bytes.
#[derive(Debug)]
pub enum KeyError {
    /// The store's storage failed.
    Io {
        /// The file involved.
        path: PathBuf,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The stored key is present but not a usable key (wrong length, empty).
    Corrupt {
        /// The file involved.
        path: PathBuf,
    },
    /// The key id is not something this store will accept as a file name.
    BadKeyId(String),
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Io { path, source } => write!(f, "{}: {source}", path.display()),
            KeyError::Corrupt { path } => write!(f, "{}: stored key is unusable", path.display()),
            KeyError::BadKeyId(id) => write!(f, "invalid key id {id:?}"),
        }
    }
}

impl std::error::Error for KeyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            KeyError::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Where the install-record signing key lives.
pub trait KeyStore: Send + Sync {
    /// The key named `key_id`, generating and storing a fresh one if none exists.
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError>;
}

/// Keys held in memory only. For tests, and for any host that wants a per-run key.
#[derive(Default)]
pub struct MemoryKeyStore {
    keys: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryKeyStore {
    /// An empty store; keys are generated on first request.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the key for `key_id`, so a test can prove a record signed under one key
    /// fails under another.
    pub fn set(&self, key_id: &str, key: SecretKey) {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key_id.to_string(), key.expose().to_vec());
    }
}

impl Drop for MemoryKeyStore {
    fn drop(&mut self) {
        if let Ok(keys) = self.keys.get_mut() {
            for bytes in keys.values_mut() {
                wipe(bytes);
            }
        }
    }
}

impl fmt::Debug for MemoryKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.keys.lock().map(|m| m.len()).unwrap_or(0);
        write!(f, "MemoryKeyStore({n} keys)")
    }
}

impl KeyStore for MemoryKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        let mut keys = self.keys.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = keys
            .entry(key_id.to_string())
            .or_insert_with(|| SecretKey::generate().expose().to_vec());
        Ok(SecretKey::new(bytes.clone()))
    }
}

/// The keychain fallback: one owner-only file per key id under a directory.
#[derive(Debug, Clone)]
pub struct FileKeyStore {
    dir: PathBuf,
}

impl FileKeyStore {
    /// A store whose keys live directly in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        FileKeyStore { dir: dir.into() }
    }

    /// The directory the keys live in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path_for(&self, key_id: &str) -> Result<PathBuf, KeyError> {
        let ok = !key_id.is_empty()
            && !key_id.starts_with('.')
            && key_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !ok {
            return Err(KeyError::BadKeyId(key_id.to_string()));
        }
        Ok(self.dir.join(key_id))
    }

    fn read(path: &Path) -> Result<Option<SecretKey>, KeyError> {
        let mut file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_err(path, e)),
        };
        let mut bytes = Vec::with_capacity(KEY_LEN);
        file.read_to_end(&mut bytes).map_err(|e| io_err(path, e))?;
        let key = SecretKey::new(bytes);
        if key.expose().len() != KEY_LEN {
            return Err(KeyError::Corrupt {
                path: path.to_path_buf(),
            });
        }
        Ok(Some(key))
    }

    /// Create the key file exclusively. `Ok(false)` means somebody else got there first.
    fn create(path: &Path, key: &SecretKey) -> Result<bool, KeyError> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = match opts.open(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
            Err(e) => return Err(io_err(path, e)),
        };
        file.write_all(key.expose())
            .and_then(|_| file.sync_all())
            .map_err(|e| io_err(path, e))?;
        // `create_new` + mode 0o600 did this on Unix at open time; NTFS has no mode
        // bits, so the owner-only DACL is applied to the file we just wrote.
        #[cfg(windows)]
        crate::persistence::acl_windows::restrict_to_owner(path).map_err(|e| io_err(path, e))?;
        Ok(true)
    }
}

fn io_err(path: &Path, source: std::io::Error) -> KeyError {
    KeyError::Io {
        path: path.to_path_buf(),
        source,
    }
}

impl KeyStore for FileKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        let path = self.path_for(key_id)?;
        if let Some(key) = Self::read(&path)? {
            return Ok(key);
        }
        super::dirs::ensure_private_dir(&self.dir).map_err(|e| match e {
            super::InstallError::Io { path, source } => KeyError::Io { path, source },
            other => KeyError::Io {
                path: self.dir.clone(),
                source: std::io::Error::other(other.to_string()),
            },
        })?;
        let fresh = SecretKey::generate();
        if Self::create(&path, &fresh)? {
            return Ok(fresh);
        }
        // Lost the race: the other writer's key is the key.
        Self::read(&path)?.ok_or(KeyError::Corrupt { path })
    }
}

/// The service name the install-record key is stored under in the platform keychain.
/// Distinct from the license token's service so the two secrets never collide.
const KEYRING_SERVICE: &str = "avada-terminal-install-key";

/// A keychain backend was unreachable. Carries a message only — never the key bytes.
struct VaultError(String);

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The seam over the OS keychain, mirroring the license store's `TokenVault`.
///
/// `Ok(Some)` — the backend holds a key for this id.
/// `Ok(None)` — the backend is reachable but has no entry yet.
/// `Err(_)`   — the backend is unavailable (headless Linux with no Secret Service, a
/// locked keychain, no backend compiled in); the caller falls back to the file store.
///
/// There is no `delete`: [`KeyStore`] only ever reads or creates, so the key is
/// write-once and never revoked through this seam.
trait KeyVault: Send + Sync {
    fn get(&self, key_id: &str) -> Result<Option<Vec<u8>>, VaultError>;
    fn set(&self, key_id: &str, key: &[u8]) -> Result<(), VaultError>;
}

/// The production keychain backend, over the `keyring` crate's binary-secret API.
struct KeyringVault;

impl KeyringVault {
    fn entry(key_id: &str) -> Result<::keyring::Entry, VaultError> {
        ::keyring::Entry::new(KEYRING_SERVICE, key_id)
            .map_err(|e| VaultError(format!("keyring entry: {e}")))
    }
}

impl KeyVault for KeyringVault {
    fn get(&self, key_id: &str) -> Result<Option<Vec<u8>>, VaultError> {
        match Self::entry(key_id)?.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(::keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(VaultError(format!("keyring get: {e}"))),
        }
    }

    fn set(&self, key_id: &str, key: &[u8]) -> Result<(), VaultError> {
        Self::entry(key_id)?
            .set_secret(key)
            .map_err(|e| VaultError(format!("keyring set: {e}")))
    }
}

/// The install-record signing key, kept in the platform keychain.
///
/// Reads and creates go to the keychain first. A [`FileKeyStore`] rides along for two
/// jobs: it is the fallback when the keychain is unreachable, and it is the migration
/// origin — a key an older, file-only build left on disk is adopted verbatim (so every
/// record it already signed still verifies) and its plaintext file is then removed.
pub struct KeyringKeyStore {
    vault: Box<dyn KeyVault>,
    fallback: FileKeyStore,
}

impl KeyringKeyStore {
    /// A store whose keychain fallback (and migration origin) lives in `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        KeyringKeyStore {
            vault: Box::new(KeyringVault),
            fallback: FileKeyStore::new(dir),
        }
    }

    /// Same, but over a supplied vault, so a test can drive the keychain-up,
    /// keychain-down, and migration paths without a real keychain.
    #[cfg(test)]
    fn with_vault(dir: impl Into<PathBuf>, vault: Box<dyn KeyVault>) -> Self {
        KeyringKeyStore {
            vault,
            fallback: FileKeyStore::new(dir),
        }
    }

    /// A key of exactly [`KEY_LEN`] bytes, or `Corrupt` against a keychain pseudo-path
    /// that names the entry without leaking the bytes.
    fn checked(bytes: Vec<u8>, key_id: &str) -> Result<SecretKey, KeyError> {
        let key = SecretKey::new(bytes);
        if key.expose().len() == KEY_LEN {
            Ok(key)
        } else {
            Err(KeyError::Corrupt {
                path: PathBuf::from(format!("keyring://{KEYRING_SERVICE}/{key_id}")),
            })
        }
    }
}

impl fmt::Debug for KeyringKeyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the key bytes; the fallback dir is the only useful, safe detail.
        write!(f, "KeyringKeyStore(fallback={:?})", self.fallback.dir())
    }
}

impl KeyStore for KeyringKeyStore {
    fn get_or_create(&self, key_id: &str) -> Result<SecretKey, KeyError> {
        // Validate the id (and get the fallback file path) before touching the keychain.
        let file_path = self.fallback.path_for(key_id)?;

        match self.vault.get(key_id) {
            Ok(Some(bytes)) => Self::checked(bytes, key_id),
            Ok(None) => {
                // Keychain reachable but empty. Adopt a key an older build left on disk,
                // or mint a fresh one; either way the keychain becomes the home.
                if let Some(existing) = FileKeyStore::read(&file_path)? {
                    if self.vault.set(key_id, existing.expose()).is_ok() {
                        let _ = std::fs::remove_file(&file_path);
                        tracing::info!(
                            "migrated install-record signing key from disk into the OS keychain"
                        );
                    }
                    // Return the on-disk bytes whether or not the keychain write took:
                    // they are the bytes every existing record was signed under.
                    return Ok(existing);
                }
                let fresh = SecretKey::generate();
                if self.vault.set(key_id, fresh.expose()).is_ok() {
                    Ok(fresh)
                } else {
                    // Write raced or failed after the read said empty: let the file
                    // store settle it (its own O_EXCL guard handles the race).
                    self.fallback.get_or_create(key_id)
                }
            }
            Err(e) => {
                // Keychain unavailable (headless Linux, locked keychain, no backend).
                // The message carries no secret; fall back to the owner-only file.
                tracing::debug!("install-key keychain unavailable ({e}); using file store");
                self.fallback.get_or_create(key_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "avada-keys-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn secret_key_is_redacted_and_wiped() {
        let key = SecretKey::generate();
        assert_eq!(key.expose().len(), KEY_LEN);
        assert_eq!(format!("{key:?}"), "SecretKey([redacted; 32 bytes])");
        let mut bytes = vec![7u8; 4];
        wipe(&mut bytes);
        assert_eq!(bytes, vec![0u8; 4]);
    }

    #[test]
    fn memory_store_is_stable_per_id_and_distinct_across_ids() {
        let store = MemoryKeyStore::new();
        let a1 = store.get_or_create("a").unwrap();
        let a2 = store.get_or_create("a").unwrap();
        let b = store.get_or_create("b").unwrap();
        assert_eq!(a1.expose(), a2.expose());
        assert_ne!(a1.expose(), b.expose());
        assert_eq!(format!("{store:?}"), "MemoryKeyStore(2 keys)");
        store.set("a", SecretKey::generate());
        assert_ne!(store.get_or_create("a").unwrap().expose(), a1.expose());
    }

    #[test]
    fn file_store_creates_once_and_reads_back() {
        let dir = scratch("file");
        let store = FileKeyStore::new(&dir);
        let k1 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        let k2 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(k1.expose(), k2.expose());
        assert_eq!(k1.expose().len(), KEY_LEN);
        let path = dir.join(DEFAULT_KEY_ID);
        assert!(path.starts_with(&dir));
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_store_refuses_a_corrupt_key_and_bad_ids() {
        let dir = scratch("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("short"), b"abc").unwrap();
        let store = FileKeyStore::new(&dir);
        assert!(matches!(
            store.get_or_create("short").unwrap_err(),
            KeyError::Corrupt { .. }
        ));
        assert!(matches!(
            store.get_or_create("../escape").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        assert!(matches!(
            store.get_or_create("").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn errors_and_debug_never_mention_key_bytes() {
        let dir = scratch("errors");
        let store = FileKeyStore::new(&dir);
        let key = store.get_or_create("k").unwrap();
        let hex: String = key.expose().iter().map(|b| format!("{b:02x}")).collect();
        let e = KeyError::Corrupt {
            path: dir.join("k"),
        };
        assert!(!e.to_string().contains(&hex));
        assert!(!format!("{key:?}").contains(&hex));
        assert!(!format!("{store:?}").contains(&hex));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- KeyringKeyStore: a fake keychain drives every branch ----

    use std::sync::Arc;

    /// A stand-in keychain. Cloning shares the same map, so a second store built on the
    /// same handle models a process restart against a persistent keychain.
    #[derive(Clone, Default)]
    struct MemVault(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

    impl MemVault {
        fn new() -> Self {
            Self::default()
        }
        fn boxed(&self) -> Box<dyn KeyVault> {
            Box::new(self.clone())
        }
        fn count(&self) -> usize {
            self.0.lock().unwrap().len()
        }
        /// Plant a raw value under `key_id`, e.g. a wrong-length one to model corruption.
        fn seed(&self, key_id: &str, bytes: Vec<u8>) {
            self.0.lock().unwrap().insert(key_id.to_string(), bytes);
        }
    }

    impl KeyVault for MemVault {
        fn get(&self, key_id: &str) -> Result<Option<Vec<u8>>, VaultError> {
            Ok(self.0.lock().unwrap().get(key_id).cloned())
        }
        fn set(&self, key_id: &str, key: &[u8]) -> Result<(), VaultError> {
            self.0
                .lock()
                .unwrap()
                .insert(key_id.to_string(), key.to_vec());
            Ok(())
        }
    }

    /// A keychain that is never reachable — every op errors, as on a headless box.
    struct UnavailableVault;

    impl KeyVault for UnavailableVault {
        fn get(&self, _key_id: &str) -> Result<Option<Vec<u8>>, VaultError> {
            Err(VaultError("no keychain backend".into()))
        }
        fn set(&self, _key_id: &str, _key: &[u8]) -> Result<(), VaultError> {
            Err(VaultError("no keychain backend".into()))
        }
    }

    #[test]
    fn keyring_store_keeps_the_key_off_disk_when_the_keychain_is_up() {
        let dir = scratch("kr-up");
        let vault = MemVault::new();
        let store = KeyringKeyStore::with_vault(&dir, vault.boxed());

        let k1 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        let k2 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(k1.expose(), k2.expose(), "key is stable across calls");
        assert_eq!(k1.expose().len(), KEY_LEN);
        assert_eq!(vault.count(), 1, "key lives in the keychain");
        assert!(
            !dir.join(DEFAULT_KEY_ID).exists(),
            "keychain-up path never writes the plaintext file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_falls_back_to_an_owner_only_file_when_the_keychain_is_down() {
        let dir = scratch("kr-down");
        let store = KeyringKeyStore::with_vault(&dir, Box::new(UnavailableVault));

        let k1 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        let k2 = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(k1.expose(), k2.expose(), "file fallback is stable too");
        let path = dir.join(DEFAULT_KEY_ID);
        assert!(
            path.is_file(),
            "unreachable keychain writes the file fallback"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600,
                "fallback key file is owner-only"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_adopts_an_existing_on_disk_key_without_regenerating_it() {
        let dir = scratch("kr-migrate");
        // An older, file-only build wrote the key to disk.
        let original = FileKeyStore::new(&dir)
            .get_or_create(DEFAULT_KEY_ID)
            .unwrap();
        let original_bytes = original.expose().to_vec();
        assert!(dir.join(DEFAULT_KEY_ID).is_file());

        // A newer build with a reachable (empty) keychain opens the same dir.
        let vault = MemVault::new();
        let store = KeyringKeyStore::with_vault(&dir, vault.boxed());
        let migrated = store.get_or_create(DEFAULT_KEY_ID).unwrap();

        assert_eq!(
            migrated.expose(),
            &original_bytes[..],
            "migration adopts the exact bytes — records signed under it must still verify"
        );
        assert_eq!(vault.count(), 1, "key is now in the keychain");
        assert!(
            !dir.join(DEFAULT_KEY_ID).exists(),
            "plaintext file is removed after a successful migration"
        );
        // And a later open reads it back from the keychain, unchanged.
        let again = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(again.expose(), &original_bytes[..]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_reports_a_corrupt_keychain_value() {
        let dir = scratch("kr-corrupt");
        let vault = MemVault::new();
        vault.seed(DEFAULT_KEY_ID, b"too-short".to_vec());
        let store = KeyringKeyStore::with_vault(&dir, vault.boxed());
        assert!(matches!(
            store.get_or_create(DEFAULT_KEY_ID).unwrap_err(),
            KeyError::Corrupt { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_key_survives_a_restart_against_a_persistent_keychain() {
        let dir = scratch("kr-restart");
        let vault = MemVault::new();
        let first = KeyringKeyStore::with_vault(&dir, vault.boxed());
        let k1 = first.get_or_create(DEFAULT_KEY_ID).unwrap();
        drop(first);

        // New store, new process — but the same underlying keychain.
        let second = KeyringKeyStore::with_vault(&dir, vault.boxed());
        let k2 = second.get_or_create(DEFAULT_KEY_ID).unwrap();
        assert_eq!(k1.expose(), k2.expose());
        assert!(
            !dir.join(DEFAULT_KEY_ID).exists(),
            "still no plaintext file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_rejects_bad_key_ids_before_touching_the_keychain() {
        let dir = scratch("kr-badid");
        let store = KeyringKeyStore::with_vault(&dir, Box::new(UnavailableVault));
        assert!(matches!(
            store.get_or_create("../escape").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        assert!(matches!(
            store.get_or_create("").unwrap_err(),
            KeyError::BadKeyId(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn keyring_store_debug_never_mentions_key_bytes() {
        let dir = scratch("kr-debug");
        let vault = MemVault::new();
        let store = KeyringKeyStore::with_vault(&dir, vault.boxed());
        let key = store.get_or_create(DEFAULT_KEY_ID).unwrap();
        let hex: String = key.expose().iter().map(|b| format!("{b:02x}")).collect();
        assert!(!format!("{store:?}").contains(&hex));
        assert!(format!("{store:?}").starts_with("KeyringKeyStore(fallback="));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
