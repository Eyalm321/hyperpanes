//! The **session-daemon wire protocol** (`docs/session-daemon-plan.md` §"Wire
//! protocol"). The GUI becomes a *client* that attaches to a PTY-owning daemon over a
//! framed Unix-domain-socket (Windows: named-pipe) stream; this module is the language
//! they speak. It is entirely in `core` (no Slint, no GUI types) so the daemon and the
//! protocol round-trips are fully headless-testable.
//!
//! ## Framing
//! Length-prefixed: a `u32` little-endian body length followed by a `serde_json` body.
//! [`write_frame`] / [`read_frame`] work over any blocking [`Read`]/[`Write`], so the
//! same code frames over a UDS, a pipe, or an in-memory pipe in tests. JSON (not
//! bincode) keeps the stream inspectable; the daemon is ours so the modest size cost is
//! fine, and the framing layer is transport-agnostic regardless.
//!
//! ## Why a `SpawnSpec`, not `SpawnOptions`
//! The in-process [`SpawnOptions`](crate::session_manager::SpawnOptions) is NOT made
//! `serde` — its `Integration` field is a wiring-layer concern that the daemon resolves
//! itself, and an `Option<EnvMap>`/integration shape on the wire would be fragile. So
//! the wire carries a small, flat, owned [`SpawnSpec`] with exactly the fields the daemon
//! needs, plus a [`SpawnSpec::into_options`] conversion back to the engine's type. The
//! client fills `uid: None` to let the daemon mint the authoritative uid (see the plan's
//! "uid stability" note).
//!
//! ## Versioning
//! [`PROTO_VER`] rides in the [`ClientMsg::Hello`] / [`DaemonMsg::Hello`] handshake. A
//! mismatch lets the client kill + respawn the daemon (lock-step upgrades — the daemon
//! is ours, no third-party compat burden). M0 only carries the field; M3 acts on it.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use crate::session::spawn::EnvMap;
use crate::session_manager::{Integration, SessionEvent, SpawnOptions};

/// Wire-protocol version, bumped on any incompatible change to the message shapes.
/// Carried in the `Hello` handshake so a client can detect a stale daemon (M3 acts on
/// the mismatch; M0 just transports it).
pub const PROTO_VER: u32 = 1;

/// Hard cap on a single frame body, so a corrupt/hostile length prefix can't make a
/// reader allocate unbounded memory. 64 MiB is far above any real replay/screen payload.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// A serde-clean spawn request carried on the wire — the daemon-facing subset of
/// [`SpawnOptions`]. Flat and owned: no `Integration` (the daemon resolves integration
/// itself), no borrowed data. `uid: None` asks the daemon to mint the authoritative uid.
///
/// `into_options` rebuilds a [`SpawnOptions`] for [`SessionRegistry::create`]. The
/// `integration_*` fields let a client pass already-resolved integration args/env
/// through (the GUI computes these); they fold back into a [`Integration`] only when
/// present, matching the in-process additive-no-op default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnSpec {
    /// The session uid. `None` → the daemon assigns one (`SessionRegistry::mint_uid`).
    #[serde(default)]
    pub uid: Option<String>,
    /// Shell to launch; `None` → the daemon's `session::spawn::default_shell()`.
    #[serde(default)]
    pub shell: Option<String>,
    /// A command to run (shell-wrapped unless `args` is also set). `None` → an
    /// interactive shell.
    #[serde(default)]
    pub command: Option<String>,
    /// Program argv (see [`SpawnOptions::args`] for the direct-vs-interactive semantics).
    #[serde(default)]
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    /// Per-pane env override.
    #[serde(default)]
    pub env: Option<EnvMap>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// The owning pane's stable id → `HYPERPANES_PANE_ID`.
    #[serde(default)]
    pub pane_id: Option<String>,
    /// Resolved shell-integration leading args (interactive branch only). Empty → none.
    #[serde(default)]
    pub integration_args: Vec<String>,
    /// Resolved shell-integration env (interactive branch only). Empty → none.
    #[serde(default)]
    pub integration_env: EnvMap,
    /// Path to `control.json` → `HYPERPANES_CONTROL_FILE`. `None` → not injected.
    #[serde(default)]
    pub control_file: Option<String>,
}

impl SpawnSpec {
    /// Convert into the engine's [`SpawnOptions`], using `uid` for the session id. The
    /// daemon supplies the (possibly freshly-minted) uid here; the integration fields
    /// fold into an [`Integration`] only when non-empty (else a plain shell, the
    /// in-process default).
    pub fn into_options(self, uid: String) -> SpawnOptions {
        let integration = if self.integration_args.is_empty() && self.integration_env.is_empty() {
            None
        } else {
            Some(Integration { args: self.integration_args, env: self.integration_env })
        };
        SpawnOptions {
            uid,
            shell: self.shell,
            args: self.args,
            command: self.command,
            cwd: self.cwd,
            env: self.env,
            cols: self.cols,
            rows: self.rows,
            pane_id: self.pane_id,
            integration,
            control_file: self.control_file,
        }
    }
}

/// A summary of one live session, returned by `ListSessions`. Mirrors the read-path
/// accessors a client shadows locally (`output_bytes`, `last_output_at`, cwd).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    pub uid: String,
    /// Last sniffed cwd, if any has been reported yet.
    pub cwd: Option<String>,
    /// Monotonic UTF-16 output cursor (`SessionRegistry::output_bytes`).
    pub output_bytes: u64,
    /// Epoch-ms of the last flush, or `None` if nothing has been emitted.
    pub last_output_at: Option<u64>,
    /// Whether the session is still live (always `true` in a `ListSessions` reply; the
    /// field exists so a future cache of dead sessions can be expressed).
    pub alive: bool,
}

/// A request from a client to the daemon. Fire-and-forget for mutators; request/response
/// for `ListSessions` / `Attach` / `RenderScreen` / `Ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Handshake: announce the client's protocol version. The daemon replies with
    /// [`DaemonMsg::Hello`].
    Hello { proto_ver: u32 },
    /// List every live session (→ [`DaemonMsg::Sessions`]).
    ListSessions,
    /// Begin receiving this session's streamed [`SessionEvent`]s on this connection, and
    /// get its replay buffer ONCE to seed a fresh grid (→ [`DaemonMsg::Replay`]).
    Attach { uid: String },
    /// Spawn a new session. `spec.uid` may be `None` to let the daemon assign the uid;
    /// the daemon replies with [`DaemonMsg::Created`] carrying the final uid.
    Create(SpawnSpec),
    /// Write input bytes (as a UTF-8 string, mirroring `SessionRegistry::write`).
    Write { uid: String, data: String },
    /// Resize a session's grid.
    Resize { uid: String, cols: u16, rows: u16 },
    /// Kill one session (silent — the natural-exit event is suppressed).
    Kill { uid: String },
    /// Kill every session.
    KillAll,
    /// Serialize a session's current screen (→ [`DaemonMsg::Screen`]).
    RenderScreen { uid: String },
    /// Liveness probe (→ [`DaemonMsg::Pong`]).
    Ping,
}

/// A message from the daemon to a client: handshake/replies plus the streamed event feed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DaemonMsg {
    /// Handshake reply: the daemon's protocol version + its pid (for diagnostics / the
    /// future kill-on-mismatch path).
    Hello { proto_ver: u32, daemon_pid: u32 },
    /// Reply to [`ClientMsg::ListSessions`].
    Sessions(Vec<SessionMeta>),
    /// Reply to [`ClientMsg::Create`]: the (possibly daemon-minted) uid of the new
    /// session, so the client can correlate its request.
    Created { uid: String },
    /// Reply to [`ClientMsg::Attach`]: the session's replay buffer, to seed a fresh grid
    /// exactly once. Empty string when the session has produced nothing yet.
    Replay { uid: String, data: String },
    /// Reply to [`ClientMsg::RenderScreen`]: the serialized screen, or `None` if the
    /// session is gone.
    Screen { uid: String, text: Option<String> },
    /// A streamed live session event (Data / Cwd / Exit) for any session this connection
    /// has attached to (or all, per the daemon's broadcast policy).
    Event(SessionEvent),
    /// Reply to [`ClientMsg::Ping`].
    Pong,
}

/// Write one length-prefixed JSON frame: a `u32` LE body length then the JSON body.
/// Flushes so the peer sees the frame promptly. Errors on serialization or I/O failure.
pub fn write_frame<W: Write>(w: &mut W, msg: &impl Serialize) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let len: u32 = body
        .len()
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame body exceeds u32"))?;
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame body exceeds MAX_FRAME_LEN"));
    }
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Read one length-prefixed JSON frame written by [`write_frame`]. Returns `Ok(None)` on
/// a clean EOF *before any byte of a frame* (the peer closed between frames); a partial
/// frame (EOF mid-length or mid-body) is an `UnexpectedEof` error. `read_exact` transparently
/// reassembles a frame delivered across multiple reads (partial-read safe).
pub fn read_frame<R: Read, T: for<'de> Deserialize<'de>>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    // Distinguish a clean between-frames EOF from a truncated length prefix.
    match read_exact_or_eof(r, &mut len_buf)? {
        ReadEof::Eof => return Ok(None),
        ReadEof::Filled => {}
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length exceeds MAX_FRAME_LEN"));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?; // mid-body EOF → UnexpectedEof (a truncated frame is an error)
    let msg = serde_json::from_slice(&body).map_err(io::Error::other)?;
    Ok(Some(msg))
}

enum ReadEof {
    /// `buf` was filled completely.
    Filled,
    /// EOF hit before any byte was read (a clean between-frames close).
    Eof,
}

/// Like [`Read::read_exact`] but reports a clean EOF *before the first byte* distinctly
/// from a partial read (EOF after some bytes → `UnexpectedEof`, as a truncated length
/// prefix is a protocol error, not an orderly shutdown).
fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<ReadEof> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return if filled == 0 {
                    Ok(ReadEof::Eof)
                } else {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF mid length prefix"))
                };
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(ReadEof::Filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn env(pairs: &[(&str, &str)]) -> EnvMap {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    // ---- SpawnSpec → SpawnOptions conversion ----

    #[test]
    fn spawn_spec_into_options_carries_fields_and_uses_passed_uid() {
        let spec = SpawnSpec {
            uid: None, // the daemon will mint; into_options takes the resolved uid
            shell: Some("/bin/zsh".into()),
            command: Some("ls".into()),
            args: None,
            cwd: Some("/tmp".into()),
            env: Some(env(&[("FOO", "bar")])),
            cols: Some(120),
            rows: Some(40),
            pane_id: Some("pane-7".into()),
            integration_args: vec!["-i".into()],
            integration_env: env(&[("HP_SHELL", "1")]),
            control_file: Some("/data/control.json".into()),
        };
        let opts = spec.into_options("s42".into());
        assert_eq!(opts.uid, "s42");
        assert_eq!(opts.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(opts.command.as_deref(), Some("ls"));
        assert_eq!(opts.cwd.as_deref(), Some("/tmp"));
        assert_eq!(opts.cols, Some(120));
        assert_eq!(opts.pane_id.as_deref(), Some("pane-7"));
        assert_eq!(opts.control_file.as_deref(), Some("/data/control.json"));
        let integ = opts.integration.expect("integration folded in");
        assert_eq!(integ.args, vec!["-i".to_string()]);
        assert_eq!(integ.env.get("HP_SHELL").map(String::as_str), Some("1"));
    }

    #[test]
    fn spawn_spec_without_integration_yields_a_plain_shell() {
        let spec = SpawnSpec::default();
        let opts = spec.into_options("s1".into());
        assert!(opts.integration.is_none(), "no integration fields → plain shell (no-op default)");
        assert_eq!(opts.uid, "s1");
    }

    // ---- framing round-trips ----

    fn roundtrip_client(msg: &ClientMsg) -> ClientMsg {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame::<_, ClientMsg>(&mut cur).unwrap().unwrap()
    }

    fn roundtrip_daemon(msg: &DaemonMsg) -> DaemonMsg {
        let mut buf = Vec::new();
        write_frame(&mut buf, msg).unwrap();
        let mut cur = Cursor::new(buf);
        read_frame::<_, DaemonMsg>(&mut cur).unwrap().unwrap()
    }

    #[test]
    fn every_client_msg_round_trips() {
        let msgs = [
            ClientMsg::Hello { proto_ver: PROTO_VER },
            ClientMsg::ListSessions,
            ClientMsg::Attach { uid: "s1".into() },
            ClientMsg::Create(SpawnSpec { command: Some("echo hi".into()), ..Default::default() }),
            ClientMsg::Write { uid: "s1".into(), data: "ls\n".into() },
            ClientMsg::Resize { uid: "s1".into(), cols: 100, rows: 30 },
            ClientMsg::Kill { uid: "s1".into() },
            ClientMsg::KillAll,
            ClientMsg::RenderScreen { uid: "s1".into() },
            ClientMsg::Ping,
        ];
        for m in &msgs {
            assert_eq!(&roundtrip_client(m), m);
        }
    }

    #[test]
    fn every_daemon_msg_round_trips() {
        let msgs = [
            DaemonMsg::Hello { proto_ver: PROTO_VER, daemon_pid: 4242 },
            DaemonMsg::Sessions(vec![SessionMeta {
                uid: "s1".into(),
                cwd: Some("/home/me".into()),
                output_bytes: 12,
                last_output_at: Some(1000),
                alive: true,
            }]),
            DaemonMsg::Created { uid: "s9".into() },
            DaemonMsg::Replay { uid: "s1".into(), data: "recent output".into() },
            DaemonMsg::Screen { uid: "s1".into(), text: Some("clean screen".into()) },
            DaemonMsg::Event(SessionEvent::Data { uid: "s1".into(), data: "hi".into() }),
            DaemonMsg::Event(SessionEvent::Cwd { uid: "s1".into(), cwd: "/tmp".into() }),
            DaemonMsg::Event(SessionEvent::Exit { uid: "s1".into(), code: 0 }),
            DaemonMsg::Pong,
        ];
        for m in &msgs {
            assert_eq!(&roundtrip_daemon(m), m);
        }
    }

    #[test]
    fn the_hello_handshake_carries_the_version_field() {
        // The version must survive the wire so a client can detect a stale daemon.
        let DaemonMsg::Hello { proto_ver, daemon_pid } =
            roundtrip_daemon(&DaemonMsg::Hello { proto_ver: PROTO_VER, daemon_pid: 77 })
        else {
            panic!("expected Hello");
        };
        assert_eq!(proto_ver, PROTO_VER);
        assert_eq!(daemon_pid, 77);
    }

    // ---- framing edge cases ----

    #[test]
    fn multiple_frames_read_back_in_order_from_one_stream() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &ClientMsg::Ping).unwrap();
        write_frame(&mut buf, &ClientMsg::ListSessions).unwrap();
        write_frame(&mut buf, &ClientMsg::Kill { uid: "s2".into() }).unwrap();
        let mut cur = Cursor::new(buf);
        assert_eq!(read_frame::<_, ClientMsg>(&mut cur).unwrap(), Some(ClientMsg::Ping));
        assert_eq!(read_frame::<_, ClientMsg>(&mut cur).unwrap(), Some(ClientMsg::ListSessions));
        assert_eq!(
            read_frame::<_, ClientMsg>(&mut cur).unwrap(),
            Some(ClientMsg::Kill { uid: "s2".into() })
        );
        // A clean EOF between frames returns None (peer closed).
        assert_eq!(read_frame::<_, ClientMsg>(&mut cur).unwrap(), None);
    }

    /// A `Read` that hands out at most `chunk` bytes per call, to prove `read_frame`
    /// reassembles a frame delivered across many short reads (partial-read safety — the
    /// real socket does exactly this).
    struct DribbleReader {
        data: Vec<u8>,
        pos: usize,
        chunk: usize,
    }
    impl Read for DribbleReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let n = self.chunk.min(buf.len()).min(self.data.len() - self.pos);
            buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn read_frame_reassembles_across_partial_reads() {
        let mut buf = Vec::new();
        let msg = DaemonMsg::Replay { uid: "s1".into(), data: "a".repeat(500) };
        write_frame(&mut buf, &msg).unwrap();
        // One byte at a time: the length prefix AND the body both arrive piecemeal.
        let mut r = DribbleReader { data: buf, pos: 0, chunk: 1 };
        assert_eq!(read_frame::<_, DaemonMsg>(&mut r).unwrap(), Some(msg));
        // Stream exhausted → clean EOF.
        assert_eq!(read_frame::<_, DaemonMsg>(&mut r).unwrap(), None);
    }

    #[test]
    fn a_truncated_length_prefix_is_an_error_not_a_clean_eof() {
        // Two bytes of a 4-byte length, then EOF: a truncated frame is a protocol error.
        let mut r = Cursor::new(vec![0x10u8, 0x00]);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn a_truncated_body_is_an_unexpected_eof() {
        // A valid length prefix promising more body than is present.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"only a few bytes");
        let mut r = Cursor::new(buf);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn an_oversized_length_prefix_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        let mut r = Cursor::new(buf);
        let err = read_frame::<_, ClientMsg>(&mut r).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
