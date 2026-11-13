//! A scripted session daemon for re-attach tests (issue #140): it holds one session whose
//! screen is what `program` drew, answers the daemon protocol over a socket pair, and logs
//! every request it receives. `offer_seed: false` makes it an OLDER daemon: its `Hello` lists no
//! features, and like a real older daemon it drops the connection on a `ScreenSeed` request.

use std::sync::{Arc, Mutex};

use hyperpanes_core::session::daemon_client::DaemonSessionManager;
use hyperpanes_core::session::proto::{
    read_frame, write_frame, ClientMsg, DaemonMsg, SessionMeta, FEATURE_SCREEN_SEED, PROTO_VER,
};
use hyperpanes_core::session::screen::{Screen, ScreenSeed};
use hyperpanes_core::session_manager::SessionManager;

pub(crate) struct FakeDaemon {
    pub mgr: SessionManager,
    /// Every request frame received, as JSON; `"DROPPED"` when it closed the connection.
    pub log: Arc<Mutex<Vec<String>>>,
    /// The session's output cursor (the seed's).
    pub cursor: u64,
}

impl FakeDaemon {
    pub fn saw(&self, what: &str) -> bool {
        self.log.lock().unwrap().iter().any(|f| f.contains(what))
    }
}

/// A daemon holding session `uid`, a `size` pty on which `program` ran. Its replay is the
/// bounded tail `program[tail_from..]`, which starts mid-state the way a real one does.
pub(crate) fn fake_daemon(
    uid: &str,
    program: &str,
    tail_from: usize,
    size: (u16, u16),
    offer_seed: bool,
) -> FakeDaemon {
    let (client, mut server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
    let mut screen = Screen::new(size.0, size.1);
    screen.advance(program.as_bytes());
    let cursor = program.encode_utf16().count() as u64;
    let tail = program[tail_from..].to_string();
    let log: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = Arc::clone(&log);
    let uid_s = uid.to_string();
    std::thread::spawn(move || loop {
        let Ok(Some(msg)) = read_frame::<_, ClientMsg>(&mut server) else {
            return;
        };
        seen.lock()
            .unwrap()
            .push(serde_json::to_string(&msg).unwrap_or_default());
        let reply = match msg {
            ClientMsg::Hello { .. } => DaemonMsg::Hello {
                proto_ver: PROTO_VER,
                daemon_pid: 1,
                features: if offer_seed {
                    vec![FEATURE_SCREEN_SEED.to_string()]
                } else {
                    Vec::new()
                },
            },
            ClientMsg::ListSessions => DaemonMsg::Sessions(vec![SessionMeta {
                uid: uid_s.clone(),
                cwd: None,
                output_bytes: cursor,
                last_output_at: Some(1),
                alive: true,
            }]),
            ClientMsg::Attach { uid } => DaemonMsg::Replay {
                uid,
                data: tail.clone(),
            },
            ClientMsg::ScreenSeed { uid } if offer_seed => DaemonMsg::ScreenSeed {
                seed: (uid == uid_s).then(|| ScreenSeed {
                    vt: screen.snapshot_vt(),
                    cols: size.0,
                    rows: size.1,
                    cursor,
                }),
                uid,
            },
            ClientMsg::ScreenSeed { .. } => {
                seen.lock().unwrap().push("DROPPED".into());
                return;
            }
            ClientMsg::Ping => DaemonMsg::Pong,
            _ => continue, // Resize / Write / …: logged, no reply
        };
        if write_frame(&mut server, &reply).is_err() {
            return;
        }
    });
    let (etx, _erx) = tokio::sync::mpsc::unbounded_channel();
    let mgr = DaemonSessionManager::from_stream(client, etx).expect("manager");
    FakeDaemon {
        mgr: SessionManager::Daemon(Arc::new(mgr)),
        log,
        cursor,
    }
}

/// A scripted diff-rendering full-screen program at 100x30: the alternate screen, modes, a
/// coloured frame, then a second frame that rewrites only some cells. A replay tail that
/// starts after the first frame can't rebuild it.
pub(crate) fn diff_tui() -> (String, usize) {
    let mut s =
        String::from("$ run-tui\r\n\x1b[?1049h\x1b[?2004h\x1b[?1002h\x1b[?1006h\x1b[H\x1b[2J");
    for row in 1..=30 {
        s.push_str(&format!(
            "\x1b[{row};1H\x1b[3{}mrow {row} \x1b[1;44m frame-one \x1b[0m",
            row % 8
        ));
        s.push_str(&format!("\x1b[{row};80H\x1b[38;2;1;2;3mright {row}\x1b[0m"));
    }
    let tail_from = s.len();
    for row in (2..=30).step_by(3) {
        s.push_str(&format!(
            "\x1b[{row};8H\x1b[1;45mtwo\x1b[0m\x1b[{row};85H\x1b[K"
        ));
    }
    s.push_str("\x1b[30;5H");
    (s, tail_from)
}
