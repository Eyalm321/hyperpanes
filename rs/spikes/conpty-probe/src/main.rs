//! ConPTY scroll-region throughput probe.
//!
//! Spawns `node throughput.mjs --case <case>` INSIDE a portable-pty (ConPTY) and
//! measures two things the bench can't separate:
//!   1. node-side INPUT rate  = payload bytes (what node wrote) / wall time
//!      → this is what the bench reports as "MB/s".
//!   2. master-side OUTPUT bytes = bytes we actually READ from the pty master.
//!      → if this is >> payload, ConPTY/conhost is INFLATING the stream (re-rendering
//!        the scroll region per line). The ratio is the inflation factor.
//!
//! Which conpty is used is decided by portable-pty's `load_conpty()`: it prefers a
//! `conpty.dll` next to the *current exe*. So run this probe FROM a dir that has the
//! sideloaded pair (or not) to A/B the in-box vs redistributable conhost.
//!
//! Usage:
//!   conpty-probe <path-to-throughput.mjs> [case] [bytesMB] [cols] [rows] [cols2] [rows2]
//! Defaults: case=scrolling-region bytesMB=4 cols=120 rows=30
//! If cols2/rows2 are given, the pty is resized to that size ~500ms after spawn
//! (mimicking how a real app spawns at a default grid then resizes to the pane).

use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let script = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: conpty-probe <throughput.mjs> [case] [bytesMB] [cols] [rows]");
        std::process::exit(2);
    });
    let case = args.get(2).cloned().unwrap_or_else(|| "scrolling-region".into());
    let bytes_mb: u64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(4);
    let cols: u16 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(120);
    let rows: u16 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(30);
    let resize_to: Option<(u16, u16)> = match (
        args.get(6).and_then(|s| s.parse().ok()),
        args.get(7).and_then(|s| s.parse().ok()),
    ) {
        (Some(c), Some(r)) => Some((c, r)),
        _ => None,
    };

    // Report which conpty.dll / OpenConsole.exe sit next to us (what load_conpty picks).
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let dll = dir.join("conpty.dll");
            let oc = dir.join("OpenConsole.exe");
            eprintln!(
                "[probe] exe dir: {}\n[probe] sideloaded conpty.dll present: {} | OpenConsole.exe present: {}",
                dir.display(),
                dll.exists(),
                oc.exists()
            );
        }
    }

    let payload_bytes = bytes_mb * 1000 * 1000;
    eprintln!(
        "[probe] case={case} payloadMB={bytes_mb} grid={cols}x{rows} — spawning node…"
    );

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .expect("openpty");

    // ConPTY's CreateProcessW needs a resolvable application path; a bare "node"
    // may not be found the way a shell would. Allow override via NODE_EXE, else try
    // a couple of known locations, else fall back to the bare name.
    let node = std::env::var("NODE_EXE").ok().unwrap_or_else(|| {
        for cand in [r"C:\nvm4w\nodejs\node.exe"] {
            if std::path::Path::new(cand).exists() {
                return cand.to_string();
            }
        }
        "node.exe".to_string()
    });
    eprintln!("[probe] node: {node}");
    let mut cmd = CommandBuilder::new(&node);
    cmd.arg(&script);
    cmd.arg("--case");
    cmd.arg(&case);
    cmd.arg("--bytes");
    cmd.arg(format!("{bytes_mb}"));
    // node writes to its stdout (the pty slave); no --out file needed.

    let mut child = pair.slave.spawn_command(cmd).expect("spawn node");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");
    let total_read = Arc::new(AtomicU64::new(0));
    let tr = Arc::clone(&total_read);

    let start = Instant::now();
    let reader_handle = thread::spawn(move || {
        let mut buf = [0u8; 65536];
        let mut answered = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    tr.fetch_add(n as u64, Ordering::Relaxed);
                    // ConPTY (INHERIT_CURSOR) queries the terminal's cursor position
                    // (ESC[6n) and emits NOTHING until it gets a reply. A real terminal
                    // answers automatically; a raw byte-counting client must answer too,
                    // or the probe sits at 0 bytes forever (this was the "headless
                    // limitation" — it has nothing to do with console windows).
                    if !answered && buf[..n].windows(4).any(|w| w == b"\x1b[6n") {
                        use std::io::Write;
                        let _ = writer.write_all(b"\x1b[1;1R");
                        let _ = writer.flush();
                        answered = true;
                        eprintln!("[probe] answered ConPTY cursor-position query (ESC[6n -> ESC[1;1R])");
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Live progress: print master bytes + inflation every ~1s so we see the inflation
    // factor WITHOUT waiting for the (slow, load-dependent) full drain to finish.
    {
        let tr2 = Arc::clone(&total_read);
        thread::spawn(move || loop {
            thread::sleep(std::time::Duration::from_millis(1000));
            let read = tr2.load(Ordering::Relaxed);
            let s = start.elapsed().as_secs_f64();
            eprintln!(
                "[probe] t={:.1}s master_read={:.2} MB ({:.2} MB/s)  inflation~{:.1}x",
                s,
                read as f64 / 1e6,
                (read as f64 / 1e6) / s,
                read as f64 / payload_bytes as f64
            );
        });
    }

    // Optional mid-run resize: real apps spawn at a default grid then resize to the
    // pane; this tests whether ResizePseudoConsole re-triggers the repaint path.
    if let Some((c2, r2)) = resize_to {
        thread::sleep(std::time::Duration::from_millis(500));
        match pair.master.resize(PtySize { rows: r2, cols: c2, pixel_width: 0, pixel_height: 0 }) {
            Ok(()) => eprintln!("[probe] resized pty {cols}x{rows} -> {c2}x{r2} at t=0.5s"),
            Err(e) => eprintln!("[probe] resize FAILED: {e}"),
        }
    }

    let status = child.wait().expect("wait");
    let elapsed = start.elapsed();
    // Drain the tail, then close the pty (drop → ClosePseudoConsole) BEFORE joining:
    // conhost keeps the output pipe open until the pseudoconsole is closed, so
    // joining first deadlocks waiting for an EOF that never comes.
    thread::sleep(std::time::Duration::from_millis(750));
    drop(pair.master);
    let _ = reader_handle.join();

    let secs = elapsed.as_secs_f64();
    let read = total_read.load(Ordering::Relaxed);
    let node_mbps = (payload_bytes as f64 / (1000.0 * 1000.0)) / secs;
    let master_mbps = (read as f64 / (1000.0 * 1000.0)) / secs;
    let inflation = read as f64 / payload_bytes as f64;

    println!("---- conpty-probe result ----");
    println!("case                : {case}");
    println!("grid                : {cols}x{rows}");
    println!("node exit           : {:?}", status.success());
    println!("wall time           : {:.3} s", secs);
    println!("payload (node wrote): {:.2} MB", payload_bytes as f64 / 1e6);
    println!("master  (we read)   : {:.2} MB", read as f64 / 1e6);
    println!("node INPUT rate     : {:.2} MB/s   <- what the bench calls 'MB/s'", node_mbps);
    println!("master OUTPUT rate  : {:.2} MB/s", master_mbps);
    println!("INFLATION factor    : {:.1}x   (master bytes / payload bytes)", inflation);
}
