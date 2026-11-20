//! Control plane. The pure cores (scope/inbox/lock/input/output — done, Wave 0) plus the
//! HTTP+WS server stack (this wave): tokens / readmodel / events / dispatch / routes / server.
//! Frozen module map.
pub mod holder;
pub mod inbox;
pub mod input;
pub mod lock;
pub mod nudge;
pub mod output;
pub mod scope;
pub mod spec_sweep;
pub mod work;
pub mod work_lock;

pub mod classifier;
pub mod discovery_guard;
pub mod dispatch;
pub mod endpoints;
pub mod events;
pub mod readmodel;
pub mod routes;
pub mod server;
pub mod speech_service;
pub mod supervisor;
pub mod tokens;
