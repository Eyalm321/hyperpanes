//! All HTTP route handlers (axum) — the byte-compatible surface the MCP server depends on.
//! Port the route table from `src/main/control-server.ts` EXACTLY:
//!   GET  /health                 (the ONLY unauthenticated route)
//!   GET  /state                  scope-filtered windows tree (readmodel)
//!   POST /tokens                 mint scoped token (tokens + scope::checkMintable)
//!   GET  /panes/:id/output       mode=screen|raw, tail, strip, since, waitForIdle/settleMs/timeoutMs
//!                                (control::output cores); cursor ALWAYS present
//!   POST /panes/:id/input        allowInput gate (403); data|keys (control::input); submit; lock 423
//!   GET|POST /panes/:id/messages durable inbox (control::inbox)
//!   POST|DELETE /panes/:id/lock  advisory lock (control::lock)
//!   POST /command                dispatch
//!   + 401 unauthorized / 404 {error,path} / 405 method-not-allowed fallbacks
//! Bearer via `Authorization: Bearer` or `?token=`. Every body shape must match (omit-when-unset).
//!
//! STUB — owned by track `control-server`.
