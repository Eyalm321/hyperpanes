//! A tiny wrapper over the system clipboard ([`arboard`]) for copy-on-select and
//! right-click paste — the OS-clipboard half of the Electron `navigator.clipboard` calls in
//! `Terminal.tsx`.
//!
//! `arboard`'s `Clipboard` holds a live platform connection, so we keep one open for the
//! pane's lifetime (re-opening per copy is slow and, on X11, loses ownership). On Windows the
//! handle is a unit value: arboard opens the OS clipboard *per operation* (with retries), so a
//! kept handle can never serve a stale read — verified against arboard 3.6.1 for #9. Every call is
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

    /// Whether the system clipboard currently holds an IMAGE (rather than text). Best-effort:
    /// a decode failure, a text-only clipboard, or an unavailable handle all read as `false`.
    /// `arboard::get_image` decodes to RGBA — we only need presence, so the pixels are dropped.
    /// Used by the paste path to decide whether a Ctrl+V should forward a literal 0x16 to an
    /// in-pane TUI (e.g. Claude Code) that reads the OS clipboard image itself, since the pty
    /// can't carry image bytes and this wrapper is text-only.
    pub fn has_image(&mut self) -> bool {
        self.image_probe() == ImageProbe::Image
    }

    /// Three-way image probe: [`ImageProbe::Image`] (decoded fine), [`ImageProbe::Absent`] (the
    /// clipboard definitively has no image) or [`ImageProbe::Unknown`] (the read failed for
    /// another reason: an unavailable handle, a busy clipboard, or an image format that would
    /// not decode, as happens with some Windows screenshot/snip DIB variants).
    pub fn image_probe(&mut self) -> ImageProbe {
        match self.inner.as_mut() {
            Some(c) => classify_image_read(c.get_image().map(|_| ())),
            None => ImageProbe::Unknown,
        }
    }
}

/// Outcome of probing the clipboard for an image. See [`Clipboard::image_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProbe {
    Image,
    Absent,
    Unknown,
}

/// Map an `arboard` image read to an [`ImageProbe`]. Only `ContentNotAvailable` is a definite
/// "no image"; every other failure leaves the question open.
pub fn classify_image_read(r: Result<(), arboard::Error>) -> ImageProbe {
    match r {
        Ok(()) => ImageProbe::Image,
        Err(arboard::Error::ContentNotAvailable) => ImageProbe::Absent,
        Err(_) => ImageProbe::Unknown,
    }
}

impl Default for Clipboard {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_read_classification() {
        assert_eq!(classify_image_read(Ok(())), ImageProbe::Image);
        assert_eq!(
            classify_image_read(Err(arboard::Error::ContentNotAvailable)),
            ImageProbe::Absent
        );
        assert_eq!(
            classify_image_read(Err(arboard::Error::ConversionFailure)),
            ImageProbe::Unknown
        );
        assert_eq!(
            classify_image_read(Err(arboard::Error::ClipboardOccupied)),
            ImageProbe::Unknown
        );
    }
}
