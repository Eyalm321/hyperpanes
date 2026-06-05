// "Idle glow" effect styles — HOW an AI/agent pane's frame catches your eye once
// it has gone quiet past the idle threshold (see useIdle / PaneFrame). The active
// style is an Appearance setting; the glow itself is the color-tinted ring in
// `.hp-pane-glow`, and these specs drive its opacity via the Web Animations API.
// Distinct from terminal-themes (terminal colors) and the frame palette.

export type IdleEffectName = 'firefly' | 'pulse' | 'blink' | 'fluorescent' | 'solid';

export const IDLE_EFFECT_NAMES: IdleEffectName[] = [
  'firefly',
  'pulse',
  'blink',
  'fluorescent',
  'solid'
];

export const IDLE_EFFECT_LABELS: Record<IdleEffectName, string> = {
  firefly: 'Firefly (random)',
  pulse: 'Pulse (steady)',
  blink: 'Blink (insistent)',
  fluorescent: 'Fluorescent (flicker)',
  solid: 'Solid glow'
};

export const DEFAULT_IDLE_EFFECT: IdleEffectName = 'firefly';

export interface IdleEffectSpec {
  // A constant opacity to hold with NO animation (a steady glow). When set, the
  // pulse fields below are ignored.
  steady?: number;
  // One pulse's opacity keyframes. A function form is re-evaluated every pulse, so
  // a style (fluorescent) can vary its flicker each cycle instead of looping an
  // identical, mechanical-looking sequence.
  keyframes?: Keyframe[] | (() => Keyframe[]);
  easing?: string;
  // One pulse's length, ms. A function so a style can randomize each cycle.
  duration?: () => number;
  // Start-to-start interval to the NEXT pulse, ms (the cycle period). The dark
  // gap between blinks is this minus the duration, so a continuous style sets it
  // equal to the duration and an irregular one (firefly) leaves a random gap.
  period?: () => number;
  // Delay before the very first pulse, ms.
  startDelay?: () => number;
}

const rand = (base: number, spread: number) => base + Math.random() * spread;

// A struggling fluorescent tube: a burst of rapid strike-flashes with dark gaps,
// then it catches and holds lit (with a faint buzz and one mid-cycle stutter).
// Opacity levels are randomized each cycle so it never reads as a fixed loop; the
// offsets stay fixed and strictly ascending (a hard requirement of the WAAPI).
function fluorescentKeyframes(): Keyframe[] {
  const flash = () => 0.75 + Math.random() * 0.25; // a strike flash
  const lit = 0.36 + Math.random() * 0.1; // the level it settles to once "on"
  return [
    { opacity: 0, offset: 0 },
    { opacity: flash(), offset: 0.02 },
    { opacity: 0.04, offset: 0.05 },
    { opacity: flash(), offset: 0.08 },
    { opacity: 0, offset: 0.11 },
    { opacity: flash(), offset: 0.15 },
    { opacity: 0.1, offset: 0.19 },
    { opacity: lit, offset: 0.26 }, // catches and holds
    { opacity: lit, offset: 0.46 },
    { opacity: flash(), offset: 0.48 }, // a sudden mid-cycle stutter
    { opacity: 0.06, offset: 0.5 },
    { opacity: lit, offset: 0.54 },
    { opacity: lit * 0.88, offset: 0.78 }, // faint buzz
    { opacity: lit, offset: 1 }
  ];
}

export const IDLE_EFFECTS: Record<IdleEffectName, IdleEffectSpec> = {
  // Irregular soft blinks with random dark gaps. A periodic pulse gets tuned out
  // by the eye within seconds; the randomness is what keeps catching it.
  firefly: {
    keyframes: [{ opacity: 0 }, { opacity: 0.8, offset: 0.4 }, { opacity: 0 }],
    easing: 'ease-in-out',
    duration: () => rand(850, 550),
    period: () => rand(1700, 2600),
    startDelay: () => rand(350, 500)
  },
  // Calm, regular breathing — back-to-back swells with no dark gap.
  pulse: {
    keyframes: [{ opacity: 0.12 }, { opacity: 0.7, offset: 0.5 }, { opacity: 0.12 }],
    easing: 'ease-in-out',
    duration: () => 2200,
    period: () => 2200,
    startDelay: () => 0
  },
  // Faster, more insistent on/off blink — a louder nudge when you want one.
  blink: {
    keyframes: [{ opacity: 0 }, { opacity: 1, offset: 0.5 }, { opacity: 0 }],
    easing: 'ease-in-out',
    duration: () => 420,
    period: () => 940,
    startDelay: () => 0
  },
  // A failing fluorescent tube: it stutters to strike, catches, buzzes lit, and
  // re-strikes every few seconds. 'linear' keeps the flashes snappy/electric; the
  // randomized duration wanders the rhythm so it doesn't tick like a metronome.
  // ~2.6–4.2s: the first ~7% (fast flicker) is ~180–290ms, the struggle ~1s, and
  // the held tail just over half the cycle. period === duration → loops, dropping
  // out at the boundary to re-strike.
  fluorescent: {
    keyframes: fluorescentKeyframes,
    easing: 'linear',
    duration: () => rand(2600, 1600),
    period: () => rand(2600, 1600),
    startDelay: () => rand(100, 220)
  },
  // No animation at all — a constant soft glow that just stays until you act.
  solid: {
    steady: 0.5
  }
};

// Run the chosen glow effect on a `.hp-pane-glow` overlay element, returning a
// cleanup that stops it. Shared by the real pane (PaneFrame, looping, gated on
// idle) and the Appearance preview (which passes `once` to demo the effect a
// single time when it's picked from the list). Steady styles just hold an
// opacity; animated ones schedule WAAPI pulses where period − duration is the
// dark gap between blinks.
export function runIdleEffect(
  el: HTMLElement,
  name: IdleEffectName,
  opts: { once?: boolean } = {}
): () => void {
  const spec = IDLE_EFFECTS[name] ?? IDLE_EFFECTS[DEFAULT_IDLE_EFFECT];

  if (spec.steady != null) {
    // One-shot demo of a constant glow: ease it in, hold, ease it back out (so
    // the preview doesn't just stick on). Looping just holds the level.
    if (opts.once) {
      const anim = el.animate(
        [
          { opacity: 0 },
          { opacity: spec.steady, offset: 0.18 },
          { opacity: spec.steady, offset: 0.72 },
          { opacity: 0 }
        ],
        { duration: 1500, easing: 'ease-in-out' }
      );
      return () => anim.cancel();
    }
    el.style.opacity = String(spec.steady);
    return () => {
      el.style.opacity = '0';
    };
  }

  // Animated: let CSS hold the base opacity (0) — clear any inline opacity a
  // prior steady style left behind — and let the pulses drive it.
  el.style.opacity = '';
  const keyframes = () =>
    typeof spec.keyframes === 'function' ? spec.keyframes() : spec.keyframes!;

  // One-shot: play a single cycle (e.g. when the effect is picked in Preferences).
  if (opts.once) {
    const anim = el.animate(keyframes(), { duration: spec.duration!(), easing: spec.easing });
    return () => anim.cancel();
  }

  let cancelled = false;
  let timer: ReturnType<typeof setTimeout>;
  let anim: Animation | null = null;
  const pulse = () => {
    if (cancelled) return;
    anim?.cancel();
    anim = el.animate(keyframes(), { duration: spec.duration!(), easing: spec.easing });
    timer = setTimeout(pulse, spec.period!());
  };
  timer = setTimeout(pulse, spec.startDelay!());
  return () => {
    cancelled = true;
    clearTimeout(timer);
    anim?.cancel();
  };
}
