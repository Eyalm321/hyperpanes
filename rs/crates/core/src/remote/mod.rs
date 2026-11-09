//! Remote-attach CLIENT for the hyperpanes control protocol (GUI-free).
//!
//! A hyperpanes build can attach to another host's panes over the same HTTP + WebSocket
//! control API the mobile app uses (`docs/mobile-client-plan.md`). This module is the pure
//! protocol layer; the Slint app wires it into "remote tabs" separately.
//!
//! * [`pairing`] — parse `hp://host:port/?token=T&v=1` (and friends) into a [`RemoteTarget`];
//!   the token is a secret and is redacted from `Debug`/`Display`.
//! * [`client`] — typed `/state` model, the HTTP [`RemoteClient`], the ordered per-pane
//!   [`InputSender`], and the self-healing [`RemoteSession`] that turns the `/events`
//!   WebSocket into a stream of [`RemoteEvent`]s (gapless attach, reconnect with delta
//!   resync, auth-failure stop).
//!
//! Client only: nothing here changes or depends on host-side behaviour beyond the published
//! protocol (cursor on `output` frames, `cols`/`rows` on `/state`). No platform-specific code.

pub mod client;
pub mod pairing;

#[cfg(test)]
mod tests;

pub use client::{
    InputSender, OutputChunk, RemoteClient, RemoteError, RemoteEvent, RemotePane, RemoteSession,
    RemoteState, RemoteTab, RemoteWindow, SessionConfig,
};
pub use pairing::{PairingError, RemoteTarget};

/// Process-wide allocator for remote host ids (the small counter embedded in remote pane uids,
/// `remote:<hostId>:<hostPaneId>`; NEVER derived from the token).
pub fn next_host_id() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// A request, made through the local control API (`connectRemote`), for the GUI to attach a
/// remote host exactly like the "Connect to host…" dialog. Debug is token-safe
/// ([`RemoteTarget`] redacts it).
#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub host_id: u32,
    pub target: RemoteTarget,
    pub label: Option<String>,
    /// Persist to `remote-hosts.json` (the control command defaults to `false`).
    pub remember: bool,
}
