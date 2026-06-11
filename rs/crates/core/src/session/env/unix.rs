//! Unix [`FreshEnvProvider`](super::FreshEnvProvider): the process env IS the freshest
//! durable source (there is no registry equivalent — login-shell rc files are applied at
//! spawn by the shell itself), so the provider passes it through unchanged. Owned by the
//! Wave-1 `unix-core` track if it ever needs more.

use super::*;

impl FreshEnvProvider for PlatformEnv {
    fn fresh_env_with_process(&self, process: EnvMap) -> EnvMap {
        process
    }
}
