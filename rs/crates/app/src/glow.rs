//! Idle glow — the AI-pane quiescence effect (the native port of
//! `src/renderer/components/idle-effects.ts`).
//!
//! When a pane has produced no output for `idle_alert_seconds` (its agent finished and
//! is waiting), its frame softly glows. The glow's *opacity* is animated here, per pane,
//! every pump tick (the deterministic styles read a free-running clock; firefly is driven
//! by a small per-pane PRNG, the native equivalent of the renderer's `setTimeout` pulses).
//! `paneview` projects the current alpha into the `PaneItem.glow` model field, and
//! `paneview.slint` draws an accent-tinted ring + halo at that opacity.

use std::time::{Duration, Instant};

/// One glow style. The token is what's persisted in [`crate::prefs::Settings::idle_effect`];
/// the order matches [`Self::OPTIONS`] (the picker list).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEffect {
    /// Irregular soft blinks with random dark gaps — keeps catching the eye.
    Firefly,
    /// Calm, regular breathing — back-to-back swells with no dark gap.
    Pulse,
    /// Faster, insistent on/off blink — a louder nudge.
    Blink,
    /// No animation — a constant soft glow that just holds until you act.
    Solid,
}

impl IdleEffect {
    /// The picker options as `(token, label)` in display order — a subset of the renderer's
    /// `IDLE_EFFECT_LABELS` (the styles the native ring can reproduce).
    pub const OPTIONS: [(&'static str, &'static str); 4] = [
        ("firefly", "Firefly (random)"),
        ("pulse", "Pulse (steady)"),
        ("blink", "Blink (insistent)"),
        ("solid", "Solid glow"),
    ];

    /// Parse a persisted token, defaulting to Firefly for an unknown/empty value (so an
    /// older blob or a renderer-only style like `fluorescent` still resolves to something).
    pub fn from_token(s: &str) -> Self {
        match s {
            "pulse" => Self::Pulse,
            "blink" => Self::Blink,
            "solid" => Self::Solid,
            _ => Self::Firefly,
        }
    }

    /// The token persisted for this effect.
    pub fn token(self) -> &'static str {
        match self {
            Self::Firefly => "firefly",
            Self::Pulse => "pulse",
            Self::Blink => "blink",
            Self::Solid => "solid",
        }
    }

    /// The index of this effect's token in [`Self::OPTIONS`] (the picker's active row).
    pub fn index(self) -> usize {
        Self::OPTIONS.iter().position(|(t, _)| *t == self.token()).unwrap_or(0)
    }
}

/// `sin(πx)` — a smooth 0→1→0 hump over `x ∈ [0,1]` (the swell shape every animated style
/// uses for one pulse).
fn hump(x: f32) -> f32 {
    (x.clamp(0.0, 1.0) * std::f32::consts::PI).sin()
}

/// Per-pane glow animation state. The deterministic styles are stateless (computed from a
/// wall-clock phase); firefly carries a tiny PRNG + the current segment schedule.
pub struct Glow {
    /// The opacity currently shown (0 = no glow), projected into the model each tick.
    pub alpha: f32,
    /// xorshift64 state — seeded per pane so two idle panes don't blink in lockstep.
    rng: u64,
    /// Firefly: end of the current segment (a lit pulse or a dark gap).
    seg_end: Option<Instant>,
    /// Firefly: start + length (ms) of the current lit pulse (`None`/0 during a dark gap).
    pulse_start: Option<Instant>,
    pulse_ms: f32,
    /// Firefly: whether the current segment is a lit pulse (vs a dark gap).
    pulsing: bool,
}

impl Glow {
    /// A fresh, dark glow seeded from `seed` (e.g. a hash of the pane uid).
    pub fn new(seed: u64) -> Self {
        Glow {
            alpha: 0.0,
            rng: seed | 1, // never 0 (xorshift would stick)
            seg_end: None,
            pulse_start: None,
            pulse_ms: 0.0,
            pulsing: false,
        }
    }

    /// Next pseudo-random float in `[0,1)` (xorshift64).
    fn rand(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        // top 24 bits → a clean [0,1) without bias from the low bits.
        ((x >> 40) as f32) / ((1u32 << 24) as f32)
    }

    /// Advance the animation and return the alpha to display. `idle` gates the glow on the
    /// pane's quiescence; when it goes false the glow resets to dark. `now`/`now_ms` are a
    /// shared monotonic instant + wall-clock-ms read once per tick.
    pub fn update(&mut self, effect: IdleEffect, idle: bool, now: Instant, now_ms: u64) -> f32 {
        if !idle {
            self.alpha = 0.0;
            self.seg_end = None;
            self.pulse_start = None;
            self.pulsing = false;
            return 0.0;
        }
        self.alpha = match effect {
            // constant soft glow.
            IdleEffect::Solid => 0.5,
            // 2.2 s breathing between 0.12 and 0.70 (no dark gap).
            IdleEffect::Pulse => {
                let p = (now_ms % 2200) as f32 / 2200.0;
                0.12 + 0.58 * hump(p)
            }
            // 0.94 s cycle: a 0.42 s on-swell to full, then dark.
            IdleEffect::Blink => {
                let p = (now_ms % 940) as f32;
                if p < 420.0 {
                    hump(p / 420.0)
                } else {
                    0.0
                }
            }
            IdleEffect::Firefly => self.firefly(now),
        };
        self.alpha
    }

    /// Firefly: alternate random lit pulses (≈0.85–1.4 s, peaking ~0.8) with random dark
    /// gaps (≈0.85–2.6 s), the native port of the renderer's randomized `setTimeout` loop.
    fn firefly(&mut self, now: Instant) -> f32 {
        // First call: wait a short randomized beat before the first blink.
        if self.seg_end.is_none() {
            let delay = 350.0 + self.rand() * 500.0;
            self.seg_end = Some(now + Duration::from_millis(delay as u64));
            self.pulsing = false;
            return 0.0;
        }
        if now >= self.seg_end.unwrap() {
            if self.pulsing {
                // lit pulse ended → start a dark gap.
                self.pulsing = false;
                self.pulse_start = None;
                let gap = 850.0 + self.rand() * 1750.0;
                self.seg_end = Some(now + Duration::from_millis(gap as u64));
            } else {
                // dark gap ended → start a lit pulse.
                self.pulsing = true;
                self.pulse_ms = 850.0 + self.rand() * 550.0;
                self.pulse_start = Some(now);
                self.seg_end = Some(now + Duration::from_millis(self.pulse_ms as u64));
            }
        }
        if self.pulsing {
            let t = now.duration_since(self.pulse_start.unwrap()).as_secs_f32() * 1000.0;
            hump(t / self.pulse_ms) * 0.8
        } else {
            0.0
        }
    }
}

/// Wall-clock epoch-ms now, to compare against `SessionManager::last_output_at` (which is
/// also epoch-ms). `0` on the (impossible) pre-epoch error.
pub fn now_epoch_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Agent/AI CLI names that mark a pane as worth watching for idle (the native port of
/// `useIdle.ts`'s `AI_PATTERN`). The glow only arms on a pane whose shell title contains
/// one of these — a plain quiet shell never glows, matching the Electron behaviour.
const AI_NAMES: [&str; 13] = [
    "claude", "aider", "gemini", "ollama", "llm", "chatgpt", "codex", "cursor-agent", "goose",
    "cody", "copilot", "continue",
    // common when an agent is launched via `npx <tool>` / shows in the title:
    "agent",
];

/// Whether `hay` (a pane's shell/OSC title) names an AI/agent CLI — a case-insensitive
/// token match so "claude" hits but an unrelated word merely *containing* a name does not.
pub fn is_ai_pane(hay: &str) -> bool {
    let lower = hay.to_ascii_lowercase();
    // Split on anything that isn't a name character so "user@host: claude" → ["user","host","claude"].
    lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|tok| AI_NAMES.contains(&tok))
}

/// Extract the last OSC window-title (`ESC ] 0|2 ; <title> BEL`/`ST`) from a pty output
/// chunk, if any. Lets the app sniff a pane's running program app-side (an agent CLI sets
/// the terminal title) without a core/widget change. Returns the newest title in `data`.
pub fn sniff_osc_title(data: &str) -> Option<String> {
    let mut found: Option<String> = None;
    let bytes = data.as_bytes();
    let mut i = 0;
    while i + 3 < bytes.len() {
        // OSC introducer: ESC ] (0|2) ;
        if bytes[i] == 0x1b && bytes[i + 1] == b']' && (bytes[i + 2] == b'0' || bytes[i + 2] == b'2')
            && bytes[i + 3] == b';'
        {
            let start = i + 4;
            // terminator: BEL (0x07) or ST (ESC \).
            let mut j = start;
            while j < bytes.len() && bytes[j] != 0x07 && !(bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\') {
                j += 1;
            }
            if let Ok(title) = std::str::from_utf8(&bytes[start..j]) {
                found = Some(title.to_string());
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    found
}

/// A cheap stable seed from a pane uid (FNV-1a) so each pane's firefly is out of phase.
pub fn seed_from(uid: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in uid.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_roundtrip_and_index() {
        for (i, (tok, _)) in IdleEffect::OPTIONS.iter().enumerate() {
            let e = IdleEffect::from_token(tok);
            assert_eq!(e.token(), *tok);
            assert_eq!(e.index(), i);
        }
        // Unknown / renderer-only styles fall back to firefly.
        assert_eq!(IdleEffect::from_token("fluorescent"), IdleEffect::Firefly);
        assert_eq!(IdleEffect::from_token(""), IdleEffect::Firefly);
    }

    #[test]
    fn not_idle_is_dark() {
        let mut g = Glow::new(seed_from("pane-0"));
        let now = Instant::now();
        assert_eq!(g.update(IdleEffect::Pulse, false, now, 1234), 0.0);
        assert_eq!(g.update(IdleEffect::Solid, false, now, 1234), 0.0);
    }

    #[test]
    fn solid_holds_and_pulse_breathes_in_range() {
        let mut g = Glow::new(seed_from("pane-1"));
        let now = Instant::now();
        assert!((g.update(IdleEffect::Solid, true, now, 0) - 0.5).abs() < 1e-6);
        // Pulse stays within its [0.12, 0.70] envelope across the cycle.
        for ms in (0..2200).step_by(50) {
            let a = g.update(IdleEffect::Pulse, true, now, ms);
            assert!((0.11..=0.71).contains(&a), "pulse alpha {a} at {ms}ms out of range");
        }
    }

    #[test]
    fn blink_goes_dark_in_the_gap() {
        let mut g = Glow::new(seed_from("pane-2"));
        let now = Instant::now();
        // Peak near the middle of the on-swell, dark in the tail of the cycle.
        assert!(g.update(IdleEffect::Blink, true, now, 210) > 0.9);
        assert_eq!(g.update(IdleEffect::Blink, true, now, 700), 0.0);
    }

    #[test]
    fn ai_pane_detection() {
        assert!(is_ai_pane("claude"));
        assert!(is_ai_pane("user@host: ~/proj — claude"));
        assert!(is_ai_pane("cursor-agent"));
        assert!(!is_ai_pane("pwsh")); // a plain shell never glows
        assert!(!is_ai_pane("ssh-agent")); // token match, not substring
        assert!(!is_ai_pane(""));
    }

    #[test]
    fn osc_title_sniff() {
        // BEL-terminated and ST-terminated, last one wins.
        assert_eq!(sniff_osc_title("\x1b]0;claude\x07ready").as_deref(), Some("claude"));
        assert_eq!(sniff_osc_title("a\x1b]2;build\x1b\\b").as_deref(), Some("build"));
        assert_eq!(
            sniff_osc_title("\x1b]0;first\x07\x1b]0;second\x07").as_deref(),
            Some("second")
        );
        assert_eq!(sniff_osc_title("no title here"), None);
    }

    #[test]
    fn firefly_alpha_is_bounded() {
        let mut g = Glow::new(seed_from("pane-3"));
        let start = Instant::now();
        for step in 0..400 {
            let now = start + Duration::from_millis(step * 25);
            let a = g.update(IdleEffect::Firefly, true, now, step * 25);
            assert!((0.0..=0.8001).contains(&a), "firefly alpha {a} out of range");
        }
    }
}
