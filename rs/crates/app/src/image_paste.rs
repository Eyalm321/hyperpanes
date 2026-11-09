//! Pure decision logic for pasting a clipboard IMAGE into an in-pane TUI (Claude Code).
//!
//! The pty can't carry image bytes: the TUI reads the OS clipboard itself when its own
//! image-paste key arrives. That key is platform-specific, so the app has to send the right one.

use hyperpanes_terminal_widget::clipboard::ImageProbe;

/// What an image-paste gesture does for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoTextPaste {
    /// Swallow the key (nothing useful to paste).
    Nothing,
    /// Send the platform's image-paste key to the pane.
    ForwardImageKey,
    /// The pane is REMOTE: tell the user image paste isn't supported there and send nothing.
    /// Claude Code reads the image from the clipboard of the machine it runs on (the host), and
    /// this client's OS would pick the key (ESC v from Windows to a Linux host expecting 0x16).
    RemoteUnsupported,
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

/// Decide what Ctrl+V does when the clipboard has no text. A remote pane never gets the
/// image-paste key: an image (or an undecidable probe) there is reported as unsupported, an
/// empty clipboard stays silent.
pub fn no_text_paste_action(target_os: &str, probe: ImageProbe, is_remote: bool) -> NoTextPaste {
    if is_remote {
        return match probe {
            ImageProbe::Absent => NoTextPaste::Nothing,
            ImageProbe::Image | ImageProbe::Unknown => NoTextPaste::RemoteUnsupported,
        };
    }
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

/// The explicit image-paste gestures (the Alt+V binding, the goal-start image re-paste): forward
/// the image-paste key to a local pane; a remote pane gets the unsupported toast instead.
pub fn image_key_action(is_remote: bool) -> NoTextPaste {
    if is_remote {
        NoTextPaste::RemoteUnsupported
    } else {
        NoTextPaste::ForwardImageKey
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
            assert_eq!(
                no_text_paste_action(os, Image, false),
                ForwardImageKey,
                "{os}"
            );
            assert_eq!(no_text_paste_action(os, Absent, false), Nothing, "{os}");
        }
        // Unknown (probe failed, e.g. an undecodable Windows DIB): only Windows forwards.
        assert_eq!(
            no_text_paste_action("windows", Unknown, false),
            ForwardImageKey
        );
        assert_eq!(no_text_paste_action("linux", Unknown, false), Nothing);
        assert_eq!(no_text_paste_action("macos", Unknown, false), Nothing);
    }

    #[test]
    fn a_remote_pane_never_gets_the_image_paste_key() {
        use ImageProbe::*;
        use NoTextPaste::*;
        // The image lives on THIS machine's clipboard; Claude reads the host's. And the key
        // would be chosen by this client's OS, not the host's.
        for os in ["linux", "macos", "windows"] {
            assert_eq!(
                no_text_paste_action(os, Image, true),
                RemoteUnsupported,
                "{os}"
            );
            assert_eq!(
                no_text_paste_action(os, Unknown, true),
                RemoteUnsupported,
                "{os}"
            );
            assert_eq!(no_text_paste_action(os, Absent, true), Nothing, "{os}");
        }
        assert_eq!(image_key_action(true), RemoteUnsupported);
        assert_eq!(image_key_action(false), ForwardImageKey);
    }
}
