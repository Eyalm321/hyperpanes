//! Self-update — Wave-2 Task 8 (General preferences panel).
//!
//! A tiny, **offline-safe** GitHub-releases updater. It never blocks startup and never
//! mutates the running binary in place: it checks the public Releases API for a newer
//! tag, optionally downloads the release's `*-setup.exe` installer to a temp path on a
//! background thread, and (on explicit user consent) launches that installer **silently**
//! and exits — the safe "staged in-place upgrade" the brief calls for.
//!
//! All network + file work runs on a dedicated [`std::thread`] (never the UI thread); the
//! UI polls [`Updater::snapshot`] each pump tick and projects it into the General panel.
//! Every network failure is non-fatal (offline simply leaves the phase unchanged / errored)
//! so a missing connection can never break the app.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

// The per-platform apply step: how a downloaded update is actually installed. Windows =
// the silent NSIS `/S` flow (`windows.rs`, moved verbatim); Linux/macOS are NotifyOnly
// stubs the Wave-1 `app-unix-shared` track fills. Surface frozen in `docs/ports-seams.md`:
//   pub const APPLY_STRATEGY: ApplyStrategy;
//   pub fn launch_installer(path: &Path) -> Result<(), String>;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod platform;
#[cfg(not(any(windows, target_os = "macos")))]
#[path = "linux.rs"]
mod platform;

pub use platform::launch_installer;

/// How an available update is applied on this platform. Seam surface for the Wave-1
/// tracks — the Windows UI flow doesn't branch on it yet (allow dead_code until the
/// non-Windows General panel consumes it).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyStrategy {
    /// Download the release's installer asset and run it silently, then exit so it can
    /// replace the files (the Windows NSIS `/S` flow).
    SilentInstaller,
    /// Only surface "update available" and point the user at the releases page — no
    /// in-app download/install (the non-Windows default until a native flow ships).
    NotifyOnly,
}

/// This platform's apply strategy.
#[allow(dead_code)]
pub fn apply_strategy() -> ApplyStrategy {
    platform::APPLY_STRATEGY
}

/// The running app version (the app `Cargo.toml`'s `version`), surfaced in the General
/// panel's "About" block and compared against the latest GitHub release tag.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The "latest release" REST endpoint for the public `Eyalm321/hyperpanes` repo.
const LATEST_RELEASE_API: &str = "https://api.github.com/repos/Eyalm321/hyperpanes/releases/latest";

/// GitHub rejects API requests without a User-Agent; identify ourselves + the running version.
const USER_AGENT: &str = concat!("hyperpanes-updater/", env!("CARGO_PKG_VERSION"));

/// The updater's coarse phase. Cast to `i32` and mirrored into the UI — keep the
/// discriminants in lock-step with the `update-phase` mapping in overlays.slint's General
/// panel (0 idle · 1 checking · 2 up-to-date · 3 available · 4 downloading · 5 downloaded ·
/// 6 error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle = 0,
    Checking = 1,
    UpToDate = 2,
    Available = 3,
    Downloading = 4,
    Downloaded = 5,
    Error = 6,
}

/// The shared, background-mutated updater state behind the [`Updater`]'s mutex.
struct Inner {
    phase: Phase,
    /// Human status line shown in the panel.
    message: String,
    /// Download progress, 0..1 (only meaningful while [`Phase::Downloading`]).
    progress: f32,
    /// `browser_download_url` of the latest release's installer asset (set by a check).
    asset_url: Option<String>,
    /// That asset's file name (used to name the temp download).
    asset_name: Option<String>,
    /// The staged installer on disk, once [`Phase::Downloaded`].
    installer: Option<PathBuf>,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            phase: Phase::Idle,
            message: String::new(),
            progress: 0.0,
            asset_url: None,
            asset_name: None,
            installer: None,
        }
    }
}

/// A cheap, lock-free-for-the-caller snapshot of the updater state for the UI projection.
pub struct Snapshot {
    pub phase: i32,
    pub message: String,
    pub progress: f32,
}

/// The self-updater. Cloneable handle around a shared state mutex; background threads it
/// spawns hold their own `Arc` clone. Default-constructed in [`Idle`](Phase::Idle).
pub struct Updater {
    inner: Arc<Mutex<Inner>>,
}

impl Default for Updater {
    fn default() -> Self {
        Updater {
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }
}

/// Lock the state, recovering from a poisoned mutex rather than panicking — a settings/
/// updater hiccup must never take down the UI.
fn guard(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner.lock().unwrap_or_else(|e| e.into_inner())
}

impl Updater {
    pub fn new() -> Self {
        Self::default()
    }

    /// A snapshot for the per-tick UI mirror.
    pub fn snapshot(&self) -> Snapshot {
        let g = guard(&self.inner);
        Snapshot {
            phase: g.phase as i32,
            message: g.message.clone(),
            progress: g.progress,
        }
    }

    /// Kick off a check for a newer release on a background thread (no-op while a check or
    /// download is already in flight). `quiet` is for the startup auto-check: on offline /
    /// failure it silently resets to Idle (no alarming message), and it stays silent when
    /// already up to date — only a genuinely newer release surfaces a hint. A manual check
    /// (`quiet = false`) reports every outcome, including errors.
    pub fn check(&self, quiet: bool) {
        {
            let mut g = guard(&self.inner);
            if matches!(g.phase, Phase::Checking | Phase::Downloading) {
                return;
            }
            g.phase = Phase::Checking;
            g.message = "Checking for updates…".to_string();
            g.progress = 0.0;
        }
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let result = fetch_latest();
            let mut g = guard(&inner);
            match result {
                Ok(info) => {
                    let tag = info.tag.trim_start_matches('v').to_string();
                    if is_newer(&tag, CURRENT_VERSION) {
                        g.phase = Phase::Available;
                        g.message = format!("v{tag} available — you have v{CURRENT_VERSION}");
                        g.asset_url = info.asset_url;
                        g.asset_name = info.asset_name;
                    } else if quiet {
                        // Startup check, already current → stay silent (don't announce it).
                        g.phase = Phase::Idle;
                        g.message = String::new();
                    } else {
                        g.phase = Phase::UpToDate;
                        g.message = format!("Up to date (v{CURRENT_VERSION})");
                    }
                }
                Err(e) => {
                    if quiet {
                        // Offline / startup failure → silently skip (non-fatal).
                        g.phase = Phase::Idle;
                        g.message = String::new();
                    } else {
                        g.phase = Phase::Error;
                        g.message = format!("Update check failed: {e}");
                    }
                }
            }
        });
    }

    /// Download the staged release's installer asset to a temp path on a background thread,
    /// updating progress as it streams. No-op unless a prior check found a newer release with
    /// a downloadable installer asset.
    pub fn download(&self) {
        let url;
        let name;
        {
            let mut g = guard(&self.inner);
            if g.phase != Phase::Available {
                return;
            }
            match g.asset_url.clone() {
                Some(u) => {
                    url = u;
                    name = g
                        .asset_name
                        .clone()
                        .unwrap_or_else(|| "hyperpanes-setup.exe".to_string());
                    g.phase = Phase::Downloading;
                    g.message = "Downloading update…".to_string();
                    g.progress = 0.0;
                }
                None => {
                    g.phase = Phase::Error;
                    g.message = "No installer asset on the latest release".to_string();
                    return;
                }
            }
        }
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let result = download_to_temp(&url, &name, &inner);
            let mut g = guard(&inner);
            match result {
                Ok(path) => {
                    g.phase = Phase::Downloaded;
                    g.message = "Update downloaded — Restart & install to apply".to_string();
                    g.progress = 1.0;
                    g.installer = Some(path);
                }
                Err(e) => {
                    g.phase = Phase::Error;
                    g.message = format!("Download failed: {e}");
                }
            }
        });
    }

    /// The staged installer path, only when an installer has actually been downloaded
    /// ([`Phase::Downloaded`]). The caller launches it + quits the app.
    pub fn installer_path(&self) -> Option<PathBuf> {
        let g = guard(&self.inner);
        (g.phase == Phase::Downloaded)
            .then(|| g.installer.clone())
            .flatten()
    }

    /// Force an error state with `msg` (used when launching the installer fails).
    pub fn set_error(&self, msg: String) {
        let mut g = guard(&self.inner);
        g.phase = Phase::Error;
        g.message = msg;
    }
}

/// The fields a release check extracts from the GitHub JSON.
struct ReleaseInfo {
    tag: String,
    asset_url: Option<String>,
    asset_name: Option<String>,
}

/// Blocking GitHub "latest release" query — builds a short-timeout client, parses `tag_name`
/// and locates the `*-setup.exe` installer asset. Pure (no shared state) so it's trivially
/// run on a worker thread; all failures map to an `Err(String)`.
fn fetch_latest() -> Result<ReleaseInfo, String> {
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub returned HTTP {}", resp.status().as_u16()));
    }
    let json: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    let tag = json["tag_name"].as_str().unwrap_or_default().to_string();
    if tag.is_empty() {
        return Err("release has no tag_name".to_string());
    }
    // Prefer an NSIS-style `*-setup.exe`; fall back to any `*setup*.exe` asset.
    let mut asset_url = None;
    let mut asset_name = None;
    if let Some(assets) = json["assets"].as_array() {
        for a in assets {
            let n = a["name"].as_str().unwrap_or_default();
            let ln = n.to_ascii_lowercase();
            let is_installer =
                ln.ends_with("-setup.exe") || (ln.ends_with(".exe") && ln.contains("setup"));
            if is_installer {
                asset_url = a["browser_download_url"].as_str().map(str::to_string);
                asset_name = Some(n.to_string());
                break;
            }
        }
    }
    Ok(ReleaseInfo {
        tag,
        asset_url,
        asset_name,
    })
}

/// Stream the installer to `%TEMP%\hyperpanes-update\<name>`, updating `inner.progress` as it
/// goes. Returns the written path.
fn download_to_temp(url: &str, name: &str, inner: &Arc<Mutex<Inner>>) -> Result<PathBuf, String> {
    use std::io::{Read, Write};
    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    let total = resp.content_length().unwrap_or(0);
    // Reduce the asset name to a bare, safe file name.
    let fname = Path::new(name)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hyperpanes-setup.exe".to_string());
    let dir = std::env::temp_dir().join("hyperpanes-update");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(&fname);
    let mut file = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        downloaded += n as u64;
        if total > 0 {
            let frac = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
            guard(inner).progress = frac;
        }
    }
    file.flush().map_err(|e| e.to_string())?;
    Ok(path)
}

/// Whether dotted-numeric `latest` is a higher version than `current` (each `v`-stripped).
/// Compares component-by-component as integers, ignoring any pre-release/build suffix — good
/// enough for `0.0.2` vs `0.1.0`; a non-numeric or equal version is "not newer".
fn is_newer(latest: &str, current: &str) -> bool {
    let a = parse_ver(latest);
    let b = parse_ver(current);
    let n = a.len().max(b.len());
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

/// Parse a version string into its leading numeric components (`v1.2.3-rc1` → `[1,2,3]`).
fn parse_ver(v: &str) -> Vec<u64> {
    v.trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_version_detected() {
        assert!(is_newer("0.1.0", "0.0.2"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.0.3", "0.0.2"));
        assert!(is_newer("0.0.10", "0.0.9")); // numeric, not lexical
    }

    #[test]
    fn same_or_older_is_not_newer() {
        assert!(!is_newer("0.0.2", "0.0.2"));
        assert!(!is_newer("0.0.1", "0.0.2"));
        assert!(!is_newer("v0.0.2", "0.0.2")); // a leading v is tolerated
    }

    #[test]
    fn prerelease_suffix_is_ignored_for_the_numeric_prefix() {
        // 0.1.0-rc1 vs 0.0.2 → still newer on the numeric prefix.
        assert!(is_newer("0.1.0-rc1", "0.0.2"));
        // equal numeric prefix with a suffix is treated as not-newer (no false update).
        assert!(!is_newer("0.0.2-rc1", "0.0.2"));
    }

    #[test]
    fn api_endpoint_targets_the_public_repo() {
        assert!(LATEST_RELEASE_API.contains("Eyalm321/hyperpanes"));
        assert!(LATEST_RELEASE_API.ends_with("/releases/latest"));
    }
}
