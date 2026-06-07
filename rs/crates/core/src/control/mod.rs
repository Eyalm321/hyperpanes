//! Control-plane pure cores — ports of `src/main/control-*.ts`.
//! Frozen module map; tracks own the leaf files only.
pub mod scope;
pub mod inbox;
pub mod lock;
pub mod input;
pub mod output;
