//! Ambient-AI port (local Ollama per-pane "what you're doing" subtitles).
//! Default-OFF, fully decoupled. Ports `src/main/ai/*`. Frozen map.
pub mod pane_buffer;
pub mod redactor;
pub mod scheduler;
pub mod ollama;
pub mod service;
pub mod ai_store;
