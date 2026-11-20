//! D-Bus session-bus health, and the guards that stop a missing or misbehaving bus from crashing
//! the app.
//!
//! hyperpanes' own D-Bus code is one call: [`notify`], the "needs input" desktop notification
//! (`crate::ask_alert`). Its dependencies use D-Bus through `zbus` too, on Linux only:
//!
//! | zbus user | What it does | Where it drives zbus |
//! |---|---|---|
//! | [`notify`] (`crate::ask_alert`) | `org.freedesktop.Notifications.Notify` for a tab that needs input | a short-lived `std::thread`, this module's `block_on`, zbus's own executor thread |
//! | `accesskit_unix` + `atspi` (slint `accessibility`) | AT-SPI bridge: exposes the UI and the terminal text to screen readers | its own `std::thread`, `futures_lite::block_on` + `async_executor` |
//! | i-slint-backend-winit `xdg_desktop_settings` | color scheme, accent, cursor blink and font size from the Settings portal | `spawn_local` on the UI thread |
//! | `ashpd` (rfd `xdg-portal`) | Open/Save dialogs through the FileChooser portal (falls back to `zenity` on error) | `pollster::block_on` on the calling (UI) thread |
//! | `ksni` (i-slint-core) | `SystemTrayIcon` | its own thread. hyperpanes has no tray, so this never runs |
//!
//! **Executor contract.** zbus is compiled once for the whole dependency graph, with the union of
//! every user's features. With zbus's `tokio` feature on, its executor becomes bare
//! `tokio::spawn`/`spawn_blocking`, which panic on any thread that is not inside a Tokio runtime.
//! Every user in the table except ashpd is written for zbus's `async-io` backend and calls zbus
//! from a thread or executor it owns, so we cannot enter a runtime for them. Only rfd's `tokio`
//! feature turned zbus/tokio on, and that panicked the AT-SPI thread on every Linux GUI launch,
//! with or without a session bus. So the app builds rfd with its `async-std` portal transport
//! (ashpd over `zbus/async-io`), and zbus is executor-agnostic on every thread. The
//! `connect_*_without_a_tokio_runtime` tests below fail if anything turns zbus/tokio back on.
//!
//! **Graceful degradation.** [`start_probe`] connects to the session bus once, off the UI thread,
//! the same way `accesskit_unix` does. If that fails it records the bus as unavailable and prints
//! a single warning naming the features that are off for this run. The AT-SPI terminal text
//! (`paneview::a11y_wanted`) is then not extracted. The features themselves already fail soft
//! without a bus: accesskit's thread exits, slint keeps its default theme and rfd falls back to
//! zenity.
//!
//! **Panic containment.** [`is_contained_dbus_panic`] (via `crash::route`) narrows the panic hook
//! for one case: a panic located in a D-Bus crate, off the main thread. That thread dies alone
//! and the process keeps running, so instead of raising the crash reporter over a live app the
//! hook logs a `CONTAINED …` record plus one ERROR line with the full panic text
//! ([`contain_panic`]), and the D-Bus features switch off. Every other panic, including any panic
//! on the UI thread, still reaches the reporter. The executor fix above removes the panic that
//! actually fired. This guard stays because `accesskit_unix` still panics by design on a broken
//! accessibility bus (`panic!("Accessibility event loop failed")` on `InterfaceExists` /
//! `MissingParameter`, and `unique_name().unwrap()` in its bus wrapper). A misbehaving D-Bus
//! peer must not look like a hyperpanes crash.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
#[cfg(target_os = "linux")]
use std::time::Duration;

/// How long [`start_probe`] waits for the session bus to answer `Hello` before calling it
/// unavailable. A missing socket fails in microseconds; this bounds a bus that accepts the
/// connection and then never answers.
#[cfg(target_os = "linux")]
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

const UNKNOWN: u8 = 0;
const AVAILABLE: u8 = 1;
const UNAVAILABLE: u8 = 2;

static STATE: AtomicU8 = AtomicU8::new(UNKNOWN);
static WARNED: AtomicBool = AtomicBool::new(false);

/// The session bus's state as far as this process knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionBus {
    /// Not probed yet (or not Linux). Treated as available: nothing waits on the probe.
    Unknown,
    Available,
    Unavailable,
}

pub fn session_bus() -> SessionBus {
    match STATE.load(Ordering::Acquire) {
        AVAILABLE => SessionBus::Available,
        UNAVAILABLE => SessionBus::Unavailable,
        _ => SessionBus::Unknown,
    }
}

/// True once the session bus is known to be unusable. D-Bus-backed features check this to switch
/// themselves off rather than doing work no D-Bus peer will ever read.
pub fn session_bus_unavailable() -> bool {
    session_bus() == SessionBus::Unavailable
}

/// The one warning printed when the session bus is unusable.
pub fn warning_text(reason: &str) -> String {
    format!(
        "warning: D-Bus session bus unavailable ({reason}). Running without it: accessibility (AT-SPI) \
         and desktop theme/portal settings are off for this run, and file dialogs use zenity \
         instead of the desktop portal."
    )
}

/// Mark the session bus unusable and print the warning, once per process. Later calls (another
/// failure, a contained panic after a failed probe) stay silent.
pub fn mark_unavailable(reason: &str) {
    STATE.store(UNAVAILABLE, Ordering::Release);
    if !WARNED.swap(true, Ordering::AcqRel) {
        let msg = warning_text(reason);
        eprintln!("[hyperpanes] {msg}");
        crate::dbg_log(&format!("dbus: {msg}"));
    }
}

#[cfg(target_os = "linux")]
fn mark_available() {
    // A contained panic or an earlier failure wins: never flip back to available.
    let _ = STATE.compare_exchange(UNKNOWN, AVAILABLE, Ordering::AcqRel, Ordering::Acquire);
}

/// Probe the session bus once on a background thread. Never blocks the caller, never panics.
/// No-op off Linux, where the dependencies above don't use D-Bus.
pub fn start_probe() {
    #[cfg(target_os = "linux")]
    {
        let spawned = std::thread::Builder::new()
            .name("hp-dbus-probe".into())
            .spawn(|| match probe_timed(None, PROBE_TIMEOUT) {
                Ok(()) => {
                    mark_available();
                    crate::dbg_log("dbus: session bus reachable");
                }
                Err(reason) => mark_unavailable(&reason),
            });
        if let Err(e) = spawned {
            crate::dbg_log(&format!("dbus: could not start the probe thread: {e}"));
        }
    }
}

/// Connect to `address` (the session bus when `None`) the way `accesskit_unix` does: a plain
/// thread, a non-Tokio executor, `internal_executor(false)`. Returns why it failed.
#[cfg(target_os = "linux")]
pub fn probe(address: Option<&str>) -> Result<(), String> {
    let builder = match address {
        Some(a) => zbus::connection::Builder::address(a),
        None => zbus::connection::Builder::session(),
    }
    .map_err(|e| e.to_string())?;
    block_on(builder.internal_executor(false).build())
        .map(drop)
        .map_err(|e| e.to_string())
}

/// [`probe`] on its own thread, given up on after `timeout`. A panic inside zbus is reported as
/// a failure here (and separately by the panic hook), never propagated.
#[cfg(target_os = "linux")]
pub fn probe_timed(address: Option<String>, timeout: Duration) -> Result<(), String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("hp-dbus-connect".into())
        .spawn(move || {
            let _ = tx.send(probe(address.as_deref()));
        })
        .map_err(|e| format!("could not start the probe thread: {e}"))?;
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "no answer from the bus within {}s",
            timeout.as_secs()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(match worker.join() {
            Err(payload) => format!("zbus panicked: {}", panic_message(payload.as_ref())),
            Ok(()) => "the probe thread exited without an answer".into(),
        }),
    }
}

/// Raise a desktop notification through `org.freedesktop.Notifications` on the session bus and
/// return its id. Blocking (connect, call, wait for the reply, which fails after `timeout`): call
/// it off the UI thread. It drives zbus with this module's `block_on` plus zbus's own executor
/// thread, never a Tokio runtime (the executor contract above).
#[cfg(target_os = "linux")]
pub fn notify(app_name: &str, summary: &str, body: &str, timeout: Duration) -> Result<u32, String> {
    use std::collections::HashMap;
    use zbus::zvariant::Value;
    block_on(async {
        let conn = zbus::connection::Builder::session()
            .map_err(|e| e.to_string())?
            .method_timeout(timeout)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let hints: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = conn
            .call_method(
                Some("org.freedesktop.Notifications"),
                "/org/freedesktop/Notifications",
                Some("org.freedesktop.Notifications"),
                "Notify",
                &(
                    app_name,
                    0u32,
                    "",
                    summary,
                    body,
                    Vec::<&str>::new(),
                    hints,
                    -1i32,
                ),
            )
            .await
            .map_err(|e| e.to_string())?;
        reply.body().deserialize::<u32>().map_err(|e| e.to_string())
    })
}

/// Minimal `block_on` that parks the current thread between polls. Deliberately not
/// `zbus::block_on`: with zbus/tokio on, that one runs the future inside zbus's private Tokio
/// runtime, which would hide exactly the regression [`probe`] exists to catch.
#[cfg(target_os = "linux")]
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// The text of a panic payload (`panic!` with a literal or a formatted message).
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".into()
    }
}

/// Crates whose code runs only on D-Bus client threads. A panic located in one of them, off the
/// main thread, is a dead D-Bus integration, not a dead app.
const DBUS_CRATES: &[&str] = &[
    "zbus",
    "zbus_names",
    "zvariant",
    "accesskit_unix",
    "accesskit_atspi_common",
    "atspi",
    "atspi-common",
    "atspi-proxies",
    "atspi-connection",
    "ksni",
    "ashpd",
];

/// Should the panic hook contain this panic instead of raising the crash reporter?
///
/// `location_file` is `PanicHookInfo::location().file()`. It is contained when it lies in a
/// D-Bus crate (a registry path like `.../zbus-5.16.0/src/...`, or a vendored `.../zbus/src/...`)
/// or in slint's `xdg_desktop_settings.rs`, and the panic is off the main thread. A main-thread
/// panic unwinds the UI and ends the process, so it always goes to the crash reporter.
pub fn is_contained_dbus_panic(location_file: &str, on_main_thread: bool) -> bool {
    if on_main_thread {
        return false;
    }
    let mut components = location_file.split(['/', '\\']).peekable();
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            // The file name itself.
            return component == "xdg_desktop_settings.rs";
        }
        let base = match component.rsplit_once('-') {
            Some((name, ver)) if ver.starts_with(|c: char| c.is_ascii_digit()) => name,
            _ => component,
        };
        if DBUS_CRATES.contains(&base) {
            return true;
        }
    }
    false
}

/// Called by the panic hook for a contained D-Bus panic (`crash::PanicRoute::ContainDbus`). Logs
/// it at ERROR with the full panic text, every time: it is not deduplicated like the
/// unavailable-bus warning. Then marks the bus unusable so D-Bus-backed features switch off.
pub fn contain_panic(thread: &str, info: &str) {
    let msg = contained_panic_text(thread, info);
    eprintln!("[hyperpanes] {msg}");
    crate::dbg_log(&format!("dbus: {msg}"));
    WARNED.store(true, Ordering::Release);
    STATE.store(UNAVAILABLE, Ordering::Release);
}

/// The ERROR line for a contained D-Bus panic. `info` is the panic's `Display` ("panicked at
/// file:line:col:\nmessage"), flattened onto one line.
pub fn contained_panic_text(thread: &str, info: &str) -> String {
    let info = info.split('\n').collect::<Vec<_>>().join(" ");
    format!(
        "ERROR: D-Bus client thread '{thread}' {info} (contained: the app keeps running; \
         accessibility (AT-SPI) and desktop portal integration are off for this run)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Panic-hook classifier: the real locations from the original crash, plus vendored and
    /// Windows-style paths, are contained off the main thread and never on it.
    #[test]
    fn dbus_panics_off_the_main_thread_are_contained() {
        let reg = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
        for file in [
            format!("{reg}/zbus-5.16.0/src/abstractions/executor.rs"),
            format!("{reg}/accesskit_unix-0.22.1/src/context.rs"),
            format!("{reg}/atspi-common-0.13.0/src/lib.rs"),
            format!("{reg}/ksni-0.3.4/src/lib.rs"),
            format!("{reg}/ashpd-0.11.1/src/desktop/file_chooser.rs"),
            "/build/vendor/zbus/src/connection/mod.rs".to_string(),
            r"C:\Users\u\.cargo\registry\src\x\zvariant-5.8.0\src\lib.rs".to_string(),
            "/home/u/.cargo/git/checkouts/slint-774a/9463a10/internal/backends/winit/xdg_desktop_settings.rs"
                .to_string(),
        ] {
            assert!(is_contained_dbus_panic(&file, false), "{file}");
            assert!(!is_contained_dbus_panic(&file, true), "{file} on main");
        }
    }

    /// Everything else still raises the crash reporter, including tokio itself and crates whose
    /// names merely start like a D-Bus crate.
    #[test]
    fn other_panics_are_not_contained() {
        let reg = "/home/u/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f";
        for file in [
            "src/state.rs".to_string(),
            "rs/crates/app/src/main.rs".to_string(),
            format!("{reg}/tokio-1.52.3/src/runtime/scheduler/mod.rs"),
            format!("{reg}/zbusy-1.0.0/src/lib.rs"),
            format!("{reg}/accesskit-0.24.1/src/lib.rs"),
            format!("{reg}/slint-1.17.0/src/lib.rs"),
            "/x/zbus.rs".to_string(),
        ] {
            assert!(!is_contained_dbus_panic(&file, false), "{file}");
        }
    }

    #[test]
    fn warning_names_the_reason_and_the_degraded_features() {
        let w = warning_text("No such file or directory (os error 2)");
        assert!(w.contains("D-Bus session bus unavailable (No such file or directory"));
        assert!(w.contains("accessibility (AT-SPI)"));
        assert!(w.contains("zenity"));
    }

    #[test]
    fn contained_panic_is_one_error_line_with_the_full_payload() {
        let info = "panicked at /r/zbus-5.16.0/src/abstractions/executor.rs:190:27:\n\
                    there is no reactor running, must be called from the context of a Tokio 1.x runtime";
        let line = contained_panic_text("<unnamed>", info);
        assert!(
            line.starts_with("ERROR: D-Bus client thread '<unnamed>' panicked at /r/zbus-5.16.0/")
        );
        assert!(line.contains("executor.rs:190:27: there is no reactor running"));
        assert!(!line.contains('\n'));
    }

    #[test]
    fn panic_message_reads_str_and_string_payloads() {
        let a: Box<dyn std::any::Any + Send> = Box::new("lit");
        let b: Box<dyn std::any::Any + Send> = Box::new(String::from("fmt 1"));
        let c: Box<dyn std::any::Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(a.as_ref()), "lit");
        assert_eq!(panic_message(b.as_ref()), "fmt 1");
        assert_eq!(panic_message(c.as_ref()), "non-string panic payload");
    }

    /// Run `f` on a fresh plain thread: no Tokio runtime is entered there, exactly like the
    /// threads `accesskit_unix` and `ksni` spawn. Returns the panic message if `f` panicked, and
    /// fails the test instead of hanging if `f` never returns.
    #[cfg(target_os = "linux")]
    fn off_runtime<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            assert!(
                tokio::runtime::Handle::try_current().is_err(),
                "test thread must not be inside a Tokio runtime"
            );
            let _ = tx.send(f());
        });
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(v) => Ok(v),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("probe hung for 20s"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => match h.join() {
                Err(payload) => Err(panic_message(payload.as_ref())),
                Ok(()) => panic!("probe thread exited without a result"),
            },
        }
    }

    /// THE regression test. With no session bus at the dialed address, a zbus connection made from
    /// a thread with no Tokio runtime must fail cleanly. When zbus is built with its `tokio`
    /// feature (origin/main before this fix, via rfd's `tokio`), this panics with "there is no
    /// reactor running, must be called from the context of a Tokio 1.x runtime". That is the panic
    /// that killed the AT-SPI thread on every launch.
    #[cfg(target_os = "linux")]
    #[test]
    fn connect_to_a_missing_bus_without_a_tokio_runtime_fails_cleanly() {
        let dir = std::env::temp_dir().join(format!("hp-dbus-missing-{}", std::process::id()));
        let addr = format!("unix:path={}/no-such-bus", dir.display());
        let outcome = off_runtime(move || probe(Some(&addr)));
        match outcome {
            Ok(Err(reason)) => assert!(!reason.is_empty()),
            Ok(Ok(())) => panic!("connected to a bus that does not exist"),
            Err(panic) => panic!("zbus panicked without a Tokio runtime: {panic}"),
        }
    }

    /// The bounded probe reports a missing bus as a reason string, never a panic or a hang.
    #[cfg(target_os = "linux")]
    #[test]
    fn timed_probe_of_a_missing_bus_reports_unavailable() {
        let addr = "unix:path=/nonexistent/hyperpanes-test/bus".to_string();
        let r = off_runtime(move || probe_timed(Some(addr), Duration::from_secs(5)))
            .expect("probe_timed must not panic");
        let reason = r.expect_err("a missing bus must be unavailable");
        assert!(!reason.contains("panicked"), "{reason}");
    }

    /// With a live bus, the same runtime-less connection succeeds: the AT-SPI bridge's code path
    /// works, it doesn't merely fail politely. Uses a private `dbus-daemon`, so it touches neither
    /// the user's session bus nor the environment. Skipped when `dbus-daemon` isn't installed.
    #[cfg(target_os = "linux")]
    #[test]
    fn connect_to_a_live_bus_without_a_tokio_runtime_succeeds() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let dir = std::env::temp_dir().join(format!("hp-dbus-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listen = format!("unix:path={}/bus", dir.display());
        let child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .arg(format!("--address={listen}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let Ok(mut child) = child else {
            eprintln!("skipping: dbus-daemon not installed");
            return;
        };
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line.trim().to_string();
        let outcome = off_runtime(move || probe(Some(&addr)));
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => panic!("could not connect to a live private bus: {reason}"),
            Err(panic) => panic!("zbus panicked without a Tokio runtime: {panic}"),
        }
    }
}
