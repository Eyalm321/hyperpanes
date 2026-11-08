//! D-Bus connection probe, and the regression tests for zbus's executor contract.
//!
//! hyperpanes has no D-Bus code of its own; its dependencies do (accesskit_unix's AT-SPI bridge,
//! slint's xdg-settings watcher, ashpd under rfd, ksni). zbus is compiled once for all of them,
//! and with its `tokio` feature on, its executor is bare `tokio::spawn`/`spawn_blocking`, which
//! panics on any thread outside a Tokio runtime. accesskit_unix connects from its own plain
//! thread, so that is fatal for it. [`probe`] connects exactly the way accesskit does; the tests
//! below run it on a runtime-less thread.
#![cfg_attr(not(test), allow(dead_code))]

#[cfg(target_os = "linux")]
use std::time::Duration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
