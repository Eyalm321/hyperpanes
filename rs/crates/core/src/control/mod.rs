//! Control plane. The pure cores (scope/inbox/lock/input/output — done, Wave 0) plus the
//! HTTP+WS server stack (this wave): tokens / readmodel / events / dispatch / routes / server.
//! Frozen module map.
pub mod scope;
pub mod inbox;
pub mod lock;
pub mod input;
pub mod output;
pub mod work;

pub mod tokens;
pub mod readmodel;
pub mod events;
pub mod supervisor;
pub mod dispatch;
pub mod routes;
pub mod server;
