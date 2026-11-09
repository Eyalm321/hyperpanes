//! Pure decision logic for pasting a clipboard IMAGE into an in-pane TUI (Claude Code).
//!
//! The pty can't carry image bytes: the TUI reads the OS clipboard itself when its own
//! image-paste key arrives. That key is platform-specific, so the app has to send the right one.

use hyperpanes_terminal_widget::clipboard::ImageProbe;

/// What Ctrl+V does when the clipboard holds no TEXT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTextPaste {
    /// Swallow the key (nothing useful to paste).
    Nothing,
    /// Send the platform's image-paste key to the pane.
    ForwardImageKey,
}

/// The bytes that trigger "paste image from clipboard" in the in-pane TUI for `target_os`
/// (a `std::env::consts::OS` value).
pub fn image_paste_sequence(target_os: &str) -> &'static str {
    if target_os == "windows" {
        // Claude Code's Windows image-paste shortcut is Alt+V; a bare Ctrl+V (0x16) does not
        // trigger it there. In the VT input stream Alt+V is ESC followed by 'v'.
        "\u{1b}v"
    } else {
        "\u{16}"
    }
}

/// The image-paste bytes for the OS this binary runs on.
pub fn image_paste_sequence_native() -> &'static str {
    image_paste_sequence(std::env::consts::OS)
}

/// Decide what Ctrl+V does when the clipboard has no text.
pub fn no_text_paste_action(target_os: &str, probe: ImageProbe) -> NoTextPaste {
    match probe {
        ImageProbe::Image => NoTextPaste::ForwardImageKey,
        ImageProbe::Absent => NoTextPaste::Nothing,
        // The probe could not tell (e.g. a Windows screenshot DIB that will not decode).
        // Forward on Windows, where Alt+V is harmless in a shell; elsewhere 0x16 would be a
        // quoted-insert in bash, so stay silent.
        ImageProbe::Unknown if target_os == "windows" => NoTextPaste::ForwardImageKey,
        ImageProbe::Unknown => NoTextPaste::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_uses_alt_v_others_ctrl_v() {
        assert_eq!(image_paste_sequence("windows"), "\u{1b}v");
        assert_eq!(image_paste_sequence("linux"), "\u{16}");
        assert_eq!(image_paste_sequence("macos"), "\u{16}");
    }

    #[test]
    fn no_text_decision_table() {
        use ImageProbe::*;
        use NoTextPaste::*;
        for os in ["linux", "macos", "windows"] {
            assert_eq!(no_text_paste_action(os, Image), ForwardImageKey, "{os}");
            assert_eq!(no_text_paste_action(os, Absent), Nothing, "{os}");
        }
        // Unknown (probe failed, e.g. an undecodable Windows DIB): only Windows forwards.
        assert_eq!(no_text_paste_action("windows", Unknown), ForwardImageKey);
        assert_eq!(no_text_paste_action("linux", Unknown), Nothing);
        assert_eq!(no_text_paste_action("macos", Unknown), Nothing);
    }
}
