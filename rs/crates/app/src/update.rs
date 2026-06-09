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

/// The running app version (the app `Cargo.toml`'s `version`), surfaced in the General
/// panel's "About" block and compared against the latest GitHub release tag.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
