//! `ControlEvent` enum + scope-filtered fan-out for the GET /events WebSocket:
//! hello / output / exit / activity / message / state.
//!
//! Ordering rules that MUST hold (MCP depends on them): `note_output` (byte cursor +
//! last_output_at) updates UNCONDITIONALLY before any subscriber guard, so `since`/`waitForIdle`
//! work with zero clients; output/exit/message/activity are pane-addressed (broadcast_for_pane
//! scope-filter); pure `state` is a coalesced (~100ms) broadcast; a busy⇄idle flip emits
//! `activity` but NOT a `state` ping (the structural-fingerprint diff).
//!
//! Note: in this single-process core, `last_output_at` + the monotonic byte cursor live in
//! `session_manager` (updated on every batch flush, before any client guard), so the
//! zero-clients invariant holds structurally — this module only fans frames out.
//!
//! Events are serialized via the `ControlEvent` enum DIRECTLY to a string (not via a key-sorted
//! `serde_json::Value`), so frame field order matches the TS source.

use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::control::scope::{pane_in_scope, PaneCoords, Scope};

/// Server→client frames on `/events`. Pane-addressed frames (output/exit/activity/message)
/// are scope-filtered per client by [`EventHub::broadcast_for_pane`]; `state` is a bare ping.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ControlEvent {
    Hello {
        pid: u32,
        version: String,
    },
    #[serde(rename_all = "camelCase")]
    Output {
        session_uid: String,
        pane_id: Option<String>,
        data: String,
    },
    #[serde(rename_all = "camelCase")]
    Exit {
        session_uid: String,
        pane_id: Option<String>,
        code: i32,
    },
    #[serde(rename_all = "camelCase")]
    Activity {
        pane_id: String,
        activity: String,
    },
    Message {
        to: String,
        from: String,
        seq: u64,
        body: String,
    },
    /// Phase-4: the precise, marker-derived pane liveness. Rides ALONGSIDE the frozen
    /// `activity` frame (which stays `busy|idle|exited`), so legacy clients are unaffected
    /// — they simply don't recognize this `type` and ignore it. `state` is one of
    /// `working | awaiting-input | done | exited`; `exit_code` is the last `133;D` code.
    #[serde(rename_all = "camelCase")]
    Liveness {
        pane_id: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
    /// Phase-4: a worker's per-command pass/fail edge (`133;C` start, `133;D;code` end),
    /// so an orchestrator can watch a looping worker's commands WITHOUT the pane exiting.
    #[serde(rename_all = "camelCase")]
    Command {
        pane_id: String,
        phase: String, // "start" | "end"
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    /// Phase-5: a supervisor decision/transition for a supervised pane. `state` is one of
    /// `restarting | restarted | completed | exhausted | crashed`.
    #[serde(rename_all = "camelCase")]
    Supervisor {
        pane_id: String,
        state: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        max: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        delay_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<i32>,
    },
    State,
}

impl ControlEvent {
    /// The exact wire bytes for this frame.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("ControlEvent serializes")
    }
}

struct Client {
    id: u64,
    scope: Option<Scope>,
    tx: UnboundedSender<String>,
}

/// The set of connected `/events` clients, each tagged with its token scope. Pane-addressed
/// frames are filtered to each client's authority; `state` reaches everyone.
#[derive(Default)]
pub struct EventHub {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    clients: Vec<Client>,
    next_id: u64,
}

impl EventHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while ≥1 client is streaming — lets the hot output path bail when nobody listens.
    pub fn has_clients(&self) -> bool {
        !self.inner.lock().unwrap().clients.is_empty()
    }

    /// Register a client with its scope; returns its id + the channel its socket task drains.
    pub fn add_client(&self, scope: Option<Scope>) -> (u64, UnboundedReceiver<String>) {
        let (tx, rx) = unbounded_channel();
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let id = inner.next_id;
        inner.clients.push(Client { id, scope, tx });
        (id, rx)
    }

    pub fn remove_client(&self, id: u64) {
        self.inner.lock().unwrap().clients.retain(|c| c.id != id);
    }

    /// Drop every client's sender (server stop): each `handle_ws` `rx.recv()` then returns `None`
    /// and its socket task exits, so no WS task lingers holding an `Arc<Shared>` after a stop/toggle.
    pub fn clear_clients(&self) {
        self.inner.lock().unwrap().clients.clear();
    }

    /// Deliver one frame to a single client (the `hello` greeting on connect).
    pub fn send_to(&self, id: u64, event: &ControlEvent) {
        let json = event.to_json();
        let inner = self.inner.lock().unwrap();
        if let Some(c) = inner.clients.iter().find(|c| c.id == id) {
            let _ = c.tx.send(json);
        }
    }

    /// Send to EVERY client (structure-only `state` ping). Serialized once.
    pub fn broadcast(&self, event: &ControlEvent) {
        let json = event.to_json();
        let inner = self.inner.lock().unwrap();
        for c in &inner.clients {
            let _ = c.tx.send(json.clone());
        }
    }

    /// Send a pane-addressed frame only to clients whose scope includes the pane. A master
    /// client (None scope) always receives it; an unknown pane (None coords) is master-only,
    /// so a scoped client never sees an unresolvable pane (TS `broadcastForPane`).
    pub fn broadcast_for_pane(&self, coords: Option<&PaneCoords>, event: &ControlEvent) {
        let json = event.to_json();
        let inner = self.inner.lock().unwrap();
        for c in &inner.clients {
            let deliver = match &c.scope {
                None => true,
                Some(s) => coords.is_some_and(|co| pane_in_scope(Some(s), co)),
            };
            if deliver {
                let _ = c.tx.send(json.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_each_frame_with_tag_first_and_camel_fields() {
        assert_eq!(
            ControlEvent::Hello {
                pid: 42,
                version: "1.2.3".into()
            }
            .to_json(),
            r#"{"type":"hello","pid":42,"version":"1.2.3"}"#
        );
        assert_eq!(
            ControlEvent::Output {
                session_uid: "u1".into(),
                pane_id: Some("p1".into()),
                data: "hi".into(),
            }
            .to_json(),
            r#"{"type":"output","sessionUid":"u1","paneId":"p1","data":"hi"}"#
        );
        // null paneId is preserved (older frames).
        assert_eq!(
            ControlEvent::Exit {
                session_uid: "u1".into(),
                pane_id: None,
                code: 0
            }
            .to_json(),
            r#"{"type":"exit","sessionUid":"u1","paneId":null,"code":0}"#
        );
        assert_eq!(
            ControlEvent::Activity {
                pane_id: "p1".into(),
                activity: "idle".into()
            }
            .to_json(),
            r#"{"type":"activity","paneId":"p1","activity":"idle"}"#
        );
        assert_eq!(
            ControlEvent::Message {
                to: "p1".into(),
                from: "mgr".into(),
                seq: 7,
                body: "go".into(),
            }
            .to_json(),
            r#"{"type":"message","to":"p1","from":"mgr","seq":7,"body":"go"}"#
        );
        assert_eq!(ControlEvent::State.to_json(), r#"{"type":"state"}"#);
    }

    #[test]
    fn phase4_and_phase5_frames_serialize_with_camel_and_omitted_optionals() {
        // liveness: exit_code omitted when None.
        assert_eq!(
            ControlEvent::Liveness {
                pane_id: "p1".into(),
                state: "working".into(),
                exit_code: None,
            }
            .to_json(),
            r#"{"type":"liveness","paneId":"p1","state":"working"}"#
        );
        assert_eq!(
            ControlEvent::Liveness {
                pane_id: "p1".into(),
                state: "done".into(),
                exit_code: Some(0),
            }
            .to_json(),
            r#"{"type":"liveness","paneId":"p1","state":"done","exitCode":0}"#
        );
        // command: code omitted on a "start" edge.
        assert_eq!(
            ControlEvent::Command {
                pane_id: "p1".into(),
                phase: "start".into(),
                code: None
            }
            .to_json(),
            r#"{"type":"command","paneId":"p1","phase":"start"}"#
        );
        assert_eq!(
            ControlEvent::Command {
                pane_id: "p1".into(),
                phase: "end".into(),
                code: Some(2)
            }
            .to_json(),
            r#"{"type":"command","paneId":"p1","phase":"end","code":2}"#
        );
        // supervisor: all optionals omitted-when-None, camelCase delay_ms.
        assert_eq!(
            ControlEvent::Supervisor {
                pane_id: "p1".into(),
                state: "restarting".into(),
                attempt: Some(1),
                max: Some(5),
                delay_ms: Some(500),
                code: Some(1),
            }
            .to_json(),
            r#"{"type":"supervisor","paneId":"p1","state":"restarting","attempt":1,"max":5,"delayMs":500,"code":1}"#
        );
        assert_eq!(
            ControlEvent::Supervisor {
                pane_id: "p1".into(),
                state: "completed".into(),
                attempt: None,
                max: None,
                delay_ms: None,
                code: Some(0),
            }
            .to_json(),
            r#"{"type":"supervisor","paneId":"p1","state":"completed","code":0}"#
        );
    }

    fn coords(pane: &str, tab: &str, window: i64) -> PaneCoords {
        PaneCoords {
            pane_id: pane.into(),
            tab_id: tab.into(),
            window_id: window,
        }
    }

    #[test]
    fn state_ping_reaches_all_clients() {
        let hub = EventHub::new();
        let (_m, mut master) = hub.add_client(None);
        let (_s, mut scoped) = hub.add_client(Some(Scope {
            pane_ids: Some(vec!["p1".into()]),
            ..Default::default()
        }));
        hub.broadcast(&ControlEvent::State);
        assert_eq!(master.try_recv().unwrap(), r#"{"type":"state"}"#);
        assert_eq!(scoped.try_recv().unwrap(), r#"{"type":"state"}"#);
    }

    #[test]
    fn pane_frames_are_scope_filtered_no_leak() {
        let hub = EventHub::new();
        let (_m, mut master) = hub.add_client(None);
        let (_s, mut scoped) = hub.add_client(Some(Scope {
            pane_ids: Some(vec!["p1".into()]),
            ..Default::default()
        }));
        // An in-scope frame reaches both.
        let in_scope = ControlEvent::Output {
            session_uid: "u1".into(),
            pane_id: Some("p1".into()),
            data: "x".into(),
        };
        hub.broadcast_for_pane(Some(&coords("p1", "t1", 1)), &in_scope);
        assert!(master.try_recv().is_ok());
        assert!(scoped.try_recv().is_ok());
        // A sibling frame reaches only the master — the scoped client sees nothing.
        let sibling = ControlEvent::Output {
            session_uid: "u2".into(),
            pane_id: Some("p2".into()),
            data: "y".into(),
        };
        hub.broadcast_for_pane(Some(&coords("p2", "t1", 1)), &sibling);
        assert!(master.try_recv().is_ok());
        assert!(
            scoped.try_recv().is_err(),
            "scoped client must not see a sibling pane"
        );
        // An unresolvable pane (None coords) is master-only.
        hub.broadcast_for_pane(None, &sibling);
        assert!(master.try_recv().is_ok());
        assert!(scoped.try_recv().is_err());
    }

    #[test]
    fn remove_client_stops_delivery() {
        let hub = EventHub::new();
        let (id, mut rx) = hub.add_client(None);
        assert!(hub.has_clients());
        hub.remove_client(id);
        assert!(!hub.has_clients());
        hub.broadcast(&ControlEvent::State);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn clear_clients_closes_every_receiver() {
        let hub = EventHub::new();
        let (_a, mut ra) = hub.add_client(None);
        let (_b, mut rb) = hub.add_client(None);
        assert!(hub.has_clients());
        hub.clear_clients();
        assert!(!hub.has_clients());
        // Both senders dropped ⇒ each receiver reports the channel closed (recv → None),
        // which is what makes the `handle_ws` loop break and the socket task exit.
        assert!(matches!(
            ra.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
        assert!(matches!(
            rb.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
        ));
    }
}
