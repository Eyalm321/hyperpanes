//! Master + scoped bearer tokens for the control API. Master = 32 random bytes hex,
//! compared in constant time via `subtle` (timingSafeEqual parity). Scoped tokens limit
//! reach (uses `crate::control::scope`) with an optional TTL; mint via POST /tokens with
//! NO privilege escalation (`scope::checkMintable`). A scoped token minted into a child's
//! env must suppress `HYPERPANES_CONTROL_FILE` (see session::spawn).
//!
//! STUB — owned by track `control-server`.
