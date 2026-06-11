//! Unix stub of the single-instance seam (see `mod.rs` for the frozen surface). Owned by
//! the Wave-1 `unix-core` track, which replaces the Unsupported error with a real
//! detector (an O_EXCL/flock lock file under the user runtime dir) and a unix-domain
//! socket hand-off mirroring the Windows named-pipe `{argv,cwd}` JSON wire shape.

use super::*;
use std::io;

pub fn acquire(_salt: &str) -> io::Result<Instance> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "single-instance is implemented for Windows only",
    ))
}

pub struct PrimaryInstance {
    _priv: (),
}

impl PrimaryInstance {
    /// The hand-off endpoint we would serve (unwired in the stub).
    pub fn pipe_name(&self) -> &str {
        ""
    }

    /// Accept hand-offs forever (unreachable in the stub — `acquire` never returns a
    /// `PrimaryInstance` here; the signature is the frozen seam for `unix-core`).
    pub async fn run_server<F>(self, _handler: F) -> io::Result<()>
    where
        F: FnMut(HandoffMessage),
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "single-instance is implemented for Windows only",
        ))
    }
}

pub struct SecondaryInstance {
    _priv: (),
}

impl SecondaryInstance {
    /// The hand-off endpoint we would forward to (unwired in the stub).
    pub fn pipe_name(&self) -> &str {
        ""
    }

    /// Forward `{argv,cwd}` to the primary (unreachable in the stub — see `run_server`).
    pub async fn forward(&self, _msg: &HandoffMessage) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "single-instance is implemented for Windows only",
        ))
    }
}
