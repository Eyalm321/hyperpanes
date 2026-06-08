//! A tiny wrapper over the system clipboard ([`arboard`]) for copy-on-select and
//! right-click paste — the OS-clipboard half of the Electron `navigator.clipboard` calls in
//! `Terminal.tsx`.
//!
//! `arboard`'s `Clipboard` holds a live platform connection, so we keep one open for the
//! pane's lifetime (re-opening per copy is slow and, on X11, loses ownership). Every call is
//! best-effort: clipboard access can transiently fail (another app holding it, a headless
//! session), and — exactly like the Electron `.catch(() => {})` — we never want that to take
//! the pane down. Failures degrade to "nothing happened".

/// An owned handle to the system clipboard. One per pane; cheap to keep around.
pub struct Clipboard {
    inner: Option<arboard::Clipboard>,
}

impl Clipboard {
    /// Open a connection to the system clipboard. Never fails: if the platform clipboard is
    /// unavailable, the handle is inert (all reads return `None`, writes are dropped) so the
    /// caller's copy/paste paths stay infallible.
    pub fn new() -> Self {
        Clipboard {
            inner: arboard::Clipboard::new().ok(),
        }
    }

    /// Copy `text` to the system clipboard. Returns `true` if it was written. A no-op (false)
    /// when the clipboard is unavailable or the write fails.
    pub fn copy(&mut self, text: &str) -> bool {
        match self.inner.as_mut() {
            Some(c) => c.set_text(text.to_owned()).is_ok(),
            None => false,
        }
    }

    /// Read the system clipboard as text. `None` when empty, non-text, or unavailable.
    pub fn paste(&mut self) -> Option<String> {
        let c = self.inner.as_mut()?;
        match c.get_text() {
            Ok(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}
