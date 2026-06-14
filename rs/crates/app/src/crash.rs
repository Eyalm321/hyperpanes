//! Crash reporting & recovery.
//!
//! The panic hook (in `main`) writes a crash log + a "pending" marker, then spawns
//! `hyperpanes --crash-report <log>` — a *fresh* process (the crashing one is unwinding) that shows
//! a native dialog with the diagnostics and three actions: **Send diagnostics / Relaunch / Close**.
//! If that instant reporter never runs (a hard crash), the next normal launch sees the leftover
//! marker and shows it then ("both instant + next-launch"). The workspace itself is saved
//! continuously by `App`'s autosave, so **Relaunch** restores the session as it was.
//!
//! "Send diagnostics" copies the full report to the clipboard and opens a prefilled GitHub issue
//! (the issue body carries a truncated copy; URLs have length limits).

use std::path::{Path, PathBuf};

use hyperpanes_core::persistence::paths;

const REPO_NEW_ISSUE: &str = "https://github.com/Eyalm321/hyperpanes/issues/new";

/// The crash-log path the panic hook writes to (must match `main`'s hook).
pub fn default_log_path() -> PathBuf {
    std::env::temp_dir().join("hyperpanes-crash.log")
}

/// The "unhandled crash" marker. Its presence means a crash hasn't been surfaced to the user yet;
/// its contents are the crash-log path to read.
fn marker_path() -> PathBuf {
    paths::state_dir().join("crash-pending")
}

/// Record an unacknowledged crash (best-effort — called from the panic hook).
pub fn write_marker(log_path: &Path) {
    let _ = std::fs::create_dir_all(paths::state_dir());
    let _ = paths::write_atomic(&marker_path(), log_path.to_string_lossy().as_bytes());
}

/// The pending crash-log path, if a crash is still unacknowledged.
pub fn pending() -> Option<PathBuf> {
    let s = std::fs::read_to_string(marker_path()).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Mark the pending crash as handled (the reporter calls this once shown).
pub fn clear_marker() {
    let _ = std::fs::remove_file(marker_path());
}

/// What the user chose in the crash dialog.
pub enum Outcome {
    Relaunch,
    Close,
}

/// Read the crash log and assemble the full diagnostics report (latest panic block + environment).
pub fn gather(log_path: &Path) -> String {
    let raw =
        std::fs::read_to_string(log_path).unwrap_or_else(|_| "(crash log not found)".to_string());
    format!(
        "hyperpanes {ver}  ({os}/{arch})\n\n{block}",
        ver = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        block = last_panic_block(&raw).trim(),
    )
}

/// The crash log is append-only across runs; keep only the most recent panic block.
fn last_panic_block(log: &str) -> &str {
    match log.rfind("PANIC:") {
        Some(i) => &log[i..],
        None => log,
    }
}

/// A short body for the dialog: the version line + the panic message, stopping before the
/// backtrace frames (which look like `0: symbol`, `12: symbol`).
pub fn summary(report: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in report.lines() {
        let t = line.trim_start();
        let is_frame = t
            .split_once(':')
            .map(|(n, _)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            .unwrap_or(false);
        if is_frame {
            break;
        }
        lines.push(line);
        if lines.iter().map(|l| l.len() + 1).sum::<usize>() > 700 {
            break;
        }
    }
    lines.join("\n").trim_end().to_string()
}

/// First panic line, trimmed, for the issue title.
fn panic_headline(report: &str) -> String {
    report
        .lines()
        .find(|l| l.contains("PANIC:"))
        .map(|l| {
            l.split("PANIC:")
                .nth(1)
                .unwrap_or(l)
                .trim()
                .chars()
                .take(120)
                .collect()
        })
        .unwrap_or_else(|| "startup crash".to_string())
}

/// Prefilled GitHub "new issue" URL. The body is short (URLs cap out); the full report goes to the
/// clipboard.
pub fn github_issue_url(report: &str) -> String {
    let title = format!("Crash: {}", panic_headline(report));
    let body = format!(
        "Describe what you were doing when it crashed.\n\n(Full diagnostics are on your clipboard — paste them below.)\n\n```\n{}\n```",
        truncate(report, 1200),
    );
    format!("{REPO_NEW_ISSUE}?labels=crash&title={}&body={}", pct(&title), pct(&body))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\n…(truncated — full report on clipboard)", &s[..i]),
        None => s.to_string(),
    }
}

/// Minimal percent-encoding for URL query values (RFC 3986 unreserved kept).
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Show the crash dialog (native rfd) and act on the buttons. Needs a Tokio runtime entered for
/// rfd's portal backend — `main`'s `--crash-report` arm enters one first. Loops so "Send" can be
/// followed by Relaunch/Close.
pub fn run_report(log_path: &Path) -> Outcome {
    let report = gather(log_path);
    let body = summary(&report);
    let mut sent = false;
    loop {
        let desc = if sent {
            format!("{body}\n\nDiagnostics copied to the clipboard and a prefilled GitHub issue opened in your browser.\n\nYour session was saved — Relaunch restores it.")
        } else {
            format!("{body}\n\nYour session was saved — Relaunch restores it exactly as it was.")
        };
        let buttons = if sent {
            rfd::MessageButtons::OkCancelCustom("Relaunch".into(), "Close".into())
        } else {
            rfd::MessageButtons::YesNoCancelCustom(
                "Send diagnostics".into(),
                "Relaunch".into(),
                "Close".into(),
            )
        };
        let res = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Error)
            .set_title("Hyperpanes crashed")
            .set_description(desc)
            .set_buttons(buttons)
            .show();
        match res {
            rfd::MessageDialogResult::Custom(s) if s == "Send diagnostics" => {
                copy_to_clipboard(&report);
                let _ = hyperpanes_core::paths::os_open(&github_issue_url(&report));
                sent = true;
                continue;
            }
            rfd::MessageDialogResult::Custom(s) if s == "Relaunch" => return Outcome::Relaunch,
            _ => return Outcome::Close,
        }
    }
}

fn copy_to_clipboard(text: &str) {
    if let Ok(mut cb) = arboard::Clipboard::new() {
        let _ = cb.set_text(text.to_owned());
    }
}

/// Spawn a fresh hyperpanes (which restores the autosaved session) and detach. Strip the
/// reporter's own env markers so the relaunched app is a normal instance: without clearing
/// `HYPERPANES_CRASH_CHILD` a recovered app couldn't report a *future* crash, and clearing
/// `HYPERPANES_TEST_PANIC` stops the simulated-crash test from looping after recovery.
pub fn relaunch() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe)
            .env_remove("HYPERPANES_CRASH_CHILD")
            .env_remove("HYPERPANES_TEST_PANIC")
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "PANIC: panicked at src/foo.rs:1:2:\nold crash\n   0: frame\nPANIC: panicked at src/bar.rs:3:4:\nboom\n   0: aaa\n   1: bbb\n";

    #[test]
    fn last_panic_block_takes_latest() {
        let b = last_panic_block(LOG);
        assert!(b.contains("src/bar.rs"));
        assert!(!b.contains("src/foo.rs"));
        assert!(b.contains("boom"));
    }

    #[test]
    fn summary_stops_before_backtrace() {
        let report = gather_from_str(LOG);
        let s = summary(&report);
        assert!(s.contains("boom"));
        assert!(s.contains("hyperpanes")); // version/env line
        assert!(!s.contains("0: aaa")); // backtrace dropped
    }

    #[test]
    fn issue_url_is_encoded_and_titled() {
        let report = gather_from_str(LOG);
        let url = github_issue_url(&report);
        assert!(url.starts_with(REPO_NEW_ISSUE));
        assert!(url.contains("labels=crash"));
        assert!(url.contains("title=Crash%3A")); // ':' encoded
        assert!(!url.contains(' ')); // fully encoded
    }

    #[test]
    fn pct_keeps_unreserved_escapes_rest() {
        assert_eq!(pct("aA0-_.~"), "aA0-_.~");
        assert_eq!(pct(" /:"), "%20%2F%3A");
    }

    // gather() reads a file; this mirrors its formatting against an in-memory log for tests.
    fn gather_from_str(raw: &str) -> String {
        format!(
            "hyperpanes {ver}  ({os}/{arch})\n\n{block}",
            ver = env!("CARGO_PKG_VERSION"),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            block = last_panic_block(raw).trim(),
        )
    }
}
