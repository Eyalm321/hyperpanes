//! Idle glow — the AI-pane quiescence effect, a faithful native port of
//! `src/renderer/components/idle-effects.ts`.
//!
//! When a pane has produced no output for `idle_alert_seconds` (its agent finished and
//! is waiting), its frame softly glows. This module reproduces the renderer's Web
//! Animations feel *exactly*: each style is one pulse's opacity **keyframes** + an
//! **easing**, played for a (possibly random) **duration**, repeated on a (possibly
//! random) **period** after a one-off **start delay**. The dark gap between blinks is
//! `period − duration`, so a continuous style sets `period == duration` and an irregular
//! one (firefly) leaves a random gap. `solid` holds a constant opacity with no animation.
//!
//! The renderer drives this with real wall-clock time + `Math.random()`; we mirror that
//! with a monotonic [`Instant`] clock + a per-pane xorshift PRNG (seeded from the pane uid
//! so two idle panes never blink in lockstep). `paneview` projects the current alpha into
//! the `PaneItem.glow` model field each tick, and `paneview.slint` draws an accent-tinted
//! ring + halo at that opacity.

use std::time::{Duration, Instant};

/// One glow style. The token is what's persisted in [`crate::prefs::Settings::idle_effect`];
/// the order matches [`Self::OPTIONS`] (the picker list) and the renderer's
/// `IDLE_EFFECT_NAMES`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleEffect {
    /// Irregular soft blinks with random dark gaps — keeps catching the eye.
    Firefly,
    /// Calm, regular breathing — back-to-back swells with no dark gap.
    Pulse,
    /// Faster, insistent on/off blink — a louder nudge.
    Blink,
    /// A failing fluorescent tube: stutter-strikes, catches, buzzes lit, then re-strikes.
    Fluorescent,
    /// No animation — a constant soft glow that just holds until you act.
    Solid,
}

impl IdleEffect {
    /// The picker options as `(token, label)` in display order — a 1:1 mirror of the
    /// renderer's `IDLE_EFFECT_LABELS` (every style the native ring reproduces).
    pub const OPTIONS: [(&'static str, &'static str); 5] = [
        ("firefly", "Firefly (random)"),
        ("pulse", "Pulse (steady)"),
        ("blink", "Blink (insistent)"),
        ("fluorescent", "Fluorescent (flicker)"),
        ("solid", "Solid glow"),
    ];

    /// Parse a persisted token, defaulting to Firefly for an unknown/empty value.
    pub fn from_token(s: &str) -> Self {
        match s {
            "pulse" => Self::Pulse,
            "blink" => Self::Blink,
            "fluorescent" => Self::Fluorescent,
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
            Self::Fluorescent => "fluorescent",
            Self::Solid => "solid",
        }
    }

    /// The index of this effect's token in [`Self::OPTIONS`] (the picker's active row).
    pub fn index(self) -> usize {
        Self::OPTIONS
            .iter()
            .position(|(t, _)| *t == self.token())
            .unwrap_or(0)
    }

    /// Whether one pulse interpolates its keyframes linearly (fluorescent — snappy/electric)
    /// rather than with `ease-in-out` (every other animated style).
    fn linear(self) -> bool {
        matches!(self, Self::Fluorescent)
    }
}

/// CSS `ease-in-out` (`cubic-bezier(0.42, 0, 0.58, 1)`), approximated by smoothstep — a
/// monotonic 0→1 curve that's visually indistinguishable for an opacity fade.
fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Interpolate an opacity keyframe list at progress `t ∈ [0,1]`. `stops` are `(offset,
/// opacity)` pairs sorted by ascending offset (a hard WAAPI requirement). `linear` selects
/// the per-segment easing.
fn interp(stops: &[(f32, f32)], t: f32, linear: bool) -> f32 {
    if stops.is_empty() {
        return 0.0;
    }
    let t = t.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 1 < stops.len() && t > stops[i + 1].0 {
        i += 1;
    }
    let (o0, a0) = stops[i];
    let (o1, a1) = if i + 1 < stops.len() {
        stops[i + 1]
    } else {
        stops[i]
    };
    if o1 <= o0 {
        return a1;
    }
    let local = ((t - o0) / (o1 - o0)).clamp(0.0, 1.0);
    let e = if linear { local } else { ease_in_out(local) };
    a0 + (a1 - a0) * e
}

/// Per-pane glow animation state — a single self-scheduling pulse loop (the native
/// equivalent of the renderer's `runIdleEffect` `setTimeout` loop). The deterministic
/// styles regenerate identical cycles; firefly/fluorescent re-roll their timing (and
/// fluorescent its keyframes) each cycle from the per-pane PRNG.
pub struct Glow {
    /// The opacity currently shown (0 = no glow), projected into the model each tick.
    pub alpha: f32,
    /// xorshift64 state — seeded per pane so two idle panes don't blink in lockstep.
    rng: u64,
    /// The effect the current schedule was built for (so a pref change restarts it).
    effect: Option<IdleEffect>,
    /// During the one-off start delay (before the first pulse): hold dark until this instant.
    pending_until: Option<Instant>,
    /// Start of the current pulse/cycle (`None` until the first one begins).
    cycle_start: Option<Instant>,
    /// This cycle's pulse length (ms) and start-to-start period (ms); the dark gap is the
    /// difference.
    dur_ms: f32,
    period_ms: f32,
    /// This cycle's opacity keyframes (`(offset, opacity)`, ascending) and easing flag.
    stops: Vec<(f32, f32)>,
    linear: bool,
}

impl Glow {
    /// A fresh, dark glow seeded from `seed` (e.g. a hash of the pane uid).
    pub fn new(seed: u64) -> Self {
        Glow {
            alpha: 0.0,
            rng: seed | 1, // never 0 (xorshift would stick)
            effect: None,
            pending_until: None,
            cycle_start: None,
            dur_ms: 0.0,
            period_ms: 0.0,
            stops: Vec::new(),
            linear: false,
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

    /// `base + rand()*spread` — the renderer's `rand(base, spread)`.
    fn rand_range(&mut self, base: f32, spread: f32) -> f32 {
        base + self.rand() * spread
    }

    /// Reset to dark (idle ended or no effect armed).
    fn reset(&mut self) {
        self.alpha = 0.0;
        self.effect = None;
        self.pending_until = None;
        self.cycle_start = None;
    }

    /// The one-off start delay (ms) before the first pulse of `e`.
    fn start_delay_for(&mut self, e: IdleEffect) -> f32 {
        match e {
            IdleEffect::Firefly => self.rand_range(350.0, 500.0),
            IdleEffect::Fluorescent => self.rand_range(100.0, 220.0),
            _ => 0.0,
        }
    }

    /// Begin a fresh pulse cycle of `e` at `now`: roll this cycle's duration, period and
    /// keyframes from the PRNG (the deterministic styles roll the same values every time).
    fn begin_cycle(&mut self, e: IdleEffect, now: Instant) {
        let dur = match e {
            IdleEffect::Firefly => self.rand_range(850.0, 550.0),
            IdleEffect::Pulse => 2200.0,
            IdleEffect::Blink => 420.0,
            IdleEffect::Fluorescent => self.rand_range(2600.0, 1600.0),
            IdleEffect::Solid => 0.0,
        };
        // period − duration is the dark gap; fluorescent loops (period == duration), dropping
        // out at the boundary to re-strike.
        let period = match e {
            IdleEffect::Firefly => self.rand_range(1700.0, 2600.0),
            IdleEffect::Pulse => 2200.0,
            IdleEffect::Blink => 940.0,
            IdleEffect::Fluorescent => dur,
            IdleEffect::Solid => 0.0,
        };
        let stops = match e {
            // 0 → 0.8 @ 0.4 → 0
            IdleEffect::Firefly => vec![(0.0, 0.0), (0.4, 0.8), (1.0, 0.0)],
            // 0.12 → 0.7 @ 0.5 → 0.12  (back-to-back swells, no dark gap)
            IdleEffect::Pulse => vec![(0.0, 0.12), (0.5, 0.7), (1.0, 0.12)],
            // 0 → 1 @ 0.5 → 0
            IdleEffect::Blink => vec![(0.0, 0.0), (0.5, 1.0), (1.0, 0.0)],
            IdleEffect::Fluorescent => self.fluorescent_stops(),
            IdleEffect::Solid => vec![(0.0, 0.5), (1.0, 0.5)],
        };
        self.dur_ms = dur;
        self.period_ms = period.max(1.0);
        self.stops = stops;
        self.linear = e.linear();
        self.cycle_start = Some(now);
    }

    /// A struggling fluorescent tube (the native port of the renderer's
    /// `fluorescentKeyframes`): a burst of rapid strike-flashes with dark gaps, then it
    /// catches and holds lit (with a faint buzz and one mid-cycle stutter). Levels are
    /// re-rolled each cycle so it never reads as a fixed loop; the offsets stay fixed and
    /// strictly ascending.
    fn fluorescent_stops(&mut self) -> Vec<(f32, f32)> {
        let lit = 0.36 + self.rand() * 0.1; // the level it settles to once "on"
        let flash = |r: f32| 0.75 + r * 0.25; // a strike flash from a [0,1) roll
        let (f1, f2, f3, f4) = (
            flash(self.rand()),
            flash(self.rand()),
            flash(self.rand()),
            flash(self.rand()),
        );
        vec![
            (0.0, 0.0),
            (0.02, f1),
            (0.05, 0.04),
            (0.08, f2),
            (0.11, 0.0),
            (0.15, f3),
            (0.19, 0.1),
            (0.26, lit), // catches and holds
            (0.46, lit),
            (0.48, f4), // a sudden mid-cycle stutter
            (0.5, 0.06),
            (0.54, lit),
            (0.78, lit * 0.88), // faint buzz
            (1.0, lit),
        ]
    }

    /// Advance the animation and return the alpha to display. `idle` gates the glow on the
    /// pane's quiescence; when it goes false the glow resets to dark. `now` is a shared
    /// monotonic instant read once per tick.
    pub fn update(&mut self, effect: IdleEffect, idle: bool, now: Instant) -> f32 {
        if !idle {
            self.reset();
            return 0.0;
        }
        // Solid: a constant hold, no schedule. Marking the effect means switching away from
        // it later restarts with a fresh start delay.
        if effect == IdleEffect::Solid {
            self.effect = Some(IdleEffect::Solid);
            self.pending_until = None;
            self.cycle_start = None;
            self.alpha = 0.5;
            return 0.5;
        }
        // (Re)start the schedule when the effect changed or nothing is armed yet.
        if self.effect != Some(effect)
            || (self.cycle_start.is_none() && self.pending_until.is_none())
        {
            self.effect = Some(effect);
            let delay = self.start_delay_for(effect);
            self.pending_until = Some(now + Duration::from_millis(delay as u64));
            self.cycle_start = None;
            self.alpha = 0.0;
            return 0.0;
        }
        // Hold dark through the start delay, then begin the first pulse.
        if let Some(p) = self.pending_until {
            if now < p {
                self.alpha = 0.0;
                return 0.0;
            }
            self.pending_until = None;
            self.begin_cycle(effect, now);
        }
        // Roll to the next cycle once the period elapses.
        let mut e = now
            .saturating_duration_since(self.cycle_start.unwrap())
            .as_secs_f32()
            * 1000.0;
        if e >= self.period_ms {
            self.begin_cycle(effect, now);
            e = 0.0;
        }
        // Within the pulse: interpolate the keyframes; past it (firefly/blink gap) the last
        // keyframe (0) holds dark until the next cycle.
        let t = if self.dur_ms > 0.0 {
            (e / self.dur_ms).min(1.0)
        } else {
            1.0
        };
        self.alpha = interp(&self.stops, t, self.linear);
        self.alpha
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
    "claude",
    "aider",
    "gemini",
    "ollama",
    "llm",
    "chatgpt",
    "codex",
    "cursor-agent",
    "goose",
    "cody",
    "copilot",
    "continue",
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
        if bytes[i] == 0x1b
            && bytes[i + 1] == b']'
            && (bytes[i + 2] == b'0' || bytes[i + 2] == b'2')
            && bytes[i + 3] == b';'
        {
            let start = i + 4;
            // terminator: BEL (0x07) or ST (ESC \).
            let mut j = start;
            while j < bytes.len()
                && bytes[j] != 0x07
                && !(bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\')
            {
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
        // fluorescent is now a real, selectable style (it was renderer-only before).
        assert_eq!(
            IdleEffect::from_token("fluorescent"),
            IdleEffect::Fluorescent
        );
        // Unknown / empty falls back to firefly.
        assert_eq!(IdleEffect::from_token("nope"), IdleEffect::Firefly);
        assert_eq!(IdleEffect::from_token(""), IdleEffect::Firefly);
    }

    #[test]
    fn not_idle_is_dark() {
        let mut g = Glow::new(seed_from("pane-0"));
        let now = Instant::now();
        assert_eq!(g.update(IdleEffect::Pulse, false, now), 0.0);
        assert_eq!(g.update(IdleEffect::Solid, false, now), 0.0);
    }

    #[test]
    fn solid_holds_immediately() {
        let mut g = Glow::new(seed_from("pane-1"));
        let now = Instant::now();
        assert!((g.update(IdleEffect::Solid, true, now) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn pulse_breathes_in_range() {
        let mut g = Glow::new(seed_from("pane-2"));
        let start = Instant::now();
        // First call arms the schedule (0 start delay), the second begins the cycle at
        // `start`; thereafter it breathes within its [0.12, 0.70] envelope.
        g.update(IdleEffect::Pulse, true, start);
        g.update(IdleEffect::Pulse, true, start);
        let mut peak = 0.0f32;
        for step in 0..120 {
            let now = start + Duration::from_millis(step * 50);
            let a = g.update(IdleEffect::Pulse, true, now);
            assert!(
                (0.119..=0.701).contains(&a),
                "pulse alpha {a} at step {step} out of range"
            );
            peak = peak.max(a);
        }
        assert!(peak > 0.6, "pulse never swelled (peak {peak})");
    }

    #[test]
    fn blink_peaks_then_goes_dark() {
        let mut g = Glow::new(seed_from("pane-3"));
        let start = Instant::now();
        // Arm + begin the cycle at `start` (0 start delay), then sample mid-swell (~210ms,
        // the keyframe peak at offset 0.5 of the 420ms pulse) and in the dark gap (~700ms,
        // past the pulse but before the 940ms period rolls over).
        g.update(IdleEffect::Blink, true, start);
        g.update(IdleEffect::Blink, true, start);
        let near_peak = g.update(IdleEffect::Blink, true, start + Duration::from_millis(210));
        let in_gap = g.update(IdleEffect::Blink, true, start + Duration::from_millis(700));
        assert!(
            near_peak > 0.9,
            "blink should peak near 1.0 mid-swell (got {near_peak})"
        );
        assert_eq!(in_gap, 0.0, "blink should be dark in the gap");
    }

    #[test]
    fn firefly_alpha_is_bounded() {
        let mut g = Glow::new(seed_from("pane-4"));
        let start = Instant::now();
        for step in 0..400 {
            let now = start + Duration::from_millis(step * 25);
            let a = g.update(IdleEffect::Firefly, true, now);
            assert!(
                (0.0..=0.8001).contains(&a),
                "firefly alpha {a} out of range"
            );
        }
    }

    #[test]
    fn fluorescent_alpha_is_bounded() {
        let mut g = Glow::new(seed_from("pane-5"));
        let start = Instant::now();
        for step in 0..600 {
            let now = start + Duration::from_millis(step * 20);
            let a = g.update(IdleEffect::Fluorescent, true, now);
            // flashes reach ~1.0; nothing ever exceeds it or goes negative.
            assert!(
                (0.0..=1.0001).contains(&a),
                "fluorescent alpha {a} out of range"
            );
        }
    }

    #[test]
    fn effect_change_restarts_cleanly() {
        let mut g = Glow::new(seed_from("pane-6"));
        let start = Instant::now();
        g.update(IdleEffect::Solid, true, start);
        // Switching solid → blink re-arms (dark during the fresh start window).
        let a = g.update(IdleEffect::Blink, true, start);
        assert_eq!(a, 0.0);
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
        assert_eq!(
            sniff_osc_title("\x1b]0;claude\x07ready").as_deref(),
            Some("claude")
        );
        assert_eq!(
            sniff_osc_title("a\x1b]2;build\x1b\\b").as_deref(),
            Some("build")
        );
        assert_eq!(
            sniff_osc_title("\x1b]0;first\x07\x1b]0;second\x07").as_deref(),
            Some("second")
        );
        assert_eq!(sniff_osc_title("no title here"), None);
    }
}
