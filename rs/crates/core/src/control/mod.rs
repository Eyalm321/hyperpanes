//! Control plane. The pure cores (scope/inbox/lock/input/output — done, Wave 0) plus the
//! HTTP+WS server stack (this wave): tokens / readmodel / events / dispatch / routes / server.
//! Frozen module map.
pub mod inbox;
pub mod input;
pub mod lock;
pub mod output;
pub mod scope;
pub mod work;

pub mod dispatch;
pub mod events;
pub mod readmodel;
pub mod routes;
pub mod server;
pub mod supervisor;
pub mod tokens;
