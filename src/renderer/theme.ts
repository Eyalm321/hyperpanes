// Frame palettes for a pane's "point" (header dot) + frame border color. Which
// palette is active is an Appearance setting (see useSettings); switching it
// remaps existing panes slot-for-slot (see useWorkspace.remapPalette), so a
// pane keeps its logical color — a "red" pane stays red, just lighter/grayer.
//
// Every palette shares the same 8 slots in the same order:
//   red · amber · green · blue · purple · pink · teal · yellow
// Keeping them parallel is what makes the by-slot remap survive a palette change.

export type PaletteName = 'dark' | 'medium' | 'light' | 'grayscale';

export const PALETTE_NAMES: PaletteName[] = ['dark', 'medium', 'light', 'grayscale'];

// Display labels name the saturation/intensity ramp (the internal keys are kept
// for back-compat with persisted settings). 'dark'→Muted, 'medium'→Vivid,
// 'light'→Neon.
export const PALETTE_LABELS: Record<PaletteName, string> = {
  dark: 'Muted',
  medium: 'Vivid',
  light: 'Neon',
  grayscale: 'Grayscale'
};

export const PALETTES: Record<PaletteName, string[]> = {
  // The original saturated set, kept as the default so existing/saved panes
  // match these slots and upgrading is a visual no-op until the user switches.
  dark: ['#e5484d', '#f5a623', '#30a46c', '#3b82f6', '#a855f7', '#ec4899', '#14b8a6', '#eab308'],
  // "Vivid": bold, fully saturated versions of each hue — punchier than Muted.
  medium: ['#ff4040', '#ffa12e', '#21c25c', '#3573f0', '#ad44f2', '#f73d92', '#14c8b6', '#f7cb24'],
  // "Neon": brightest/boldest — near-pure "basic RGB" colors.
  light: ['#ff1a1a', '#ff8800', '#00dd33', '#2e8bff', '#c026ff', '#ff1f8c', '#00e6cf', '#ffe000'],
  // 8 distinct grays, light→dark, all kept light enough to read against the
  // dark UI (and against each other) so panes stay distinguishable.
  grayscale: ['#e0e0e0', '#c8c8c8', '#b0b0b0', '#989898', '#808080', '#6a6a6a', '#565656', '#444444']
};

export const DEFAULT_PALETTE: PaletteName = 'dark';

// Back-compat alias for the original constant (the default palette's colors).
export const FRAME_COLORS = PALETTES.dark;

export function paletteColors(name: PaletteName): string[] {
  return PALETTES[name] ?? PALETTES[DEFAULT_PALETTE];
}

const eq = (a: string, b: string) => a.toLowerCase() === b.toLowerCase();

// Color sets consulted ONLY to recover a pane's slot from its stored hex. It
// includes every current palette plus historical values, so a remap still works
// when a stored color predates a change to the palette's hex values (otherwise
// such colors would orphan — match no palette — and become un-remappable). Sets
// don't collide on a hex, so first match wins.
const LEGACY_PALETTES: string[][] = [
  // pre-vivid "medium" (lightened hues)
  ['#f0676b', '#f7bb52', '#4cbe86', '#6aa0f8', '#bd7bf5', '#f06bb0', '#3fcabb', '#f1c63a'],
  // pre-vivid "light" (pastels)
  ['#f6a8aa', '#fbd99a', '#9bdcbb', '#aecbfb', '#dcb6f9', '#f7b3d6', '#9ce0d8', '#f7e391']
];
const SLOT_LOOKUP: string[][] = [...Object.values(PALETTES), ...LEGACY_PALETTES];

// The slot index (0–7) of `hex` across any known palette (current or historical),
// or -1 for a hand-picked custom color that belongs to none.
export function slotOf(hex: string): number {
  for (const set of SLOT_LOOKUP) {
    const i = set.findIndex((c) => eq(c, hex));
    if (i >= 0) return i;
  }
  return -1;
}

// Remap a color into palette `to` by its slot. A custom color (no known slot) is
// returned unchanged. The slot is inferred from the hex itself — not a supplied
// "from" palette — so it's robust to a pane's color having drifted from the
// active palette (e.g. a value saved under an older palette definition).
export function remapColor(hex: string, to: PaletteName): string {
  const slot = slotOf(hex);
  return slot < 0 ? hex : paletteColors(to)[slot];
}

// The frame color for the Nth pane created, cycling within the active palette.
export function nextColor(index: number, palette: PaletteName = DEFAULT_PALETTE): string {
  const colors = paletteColors(palette);
  return colors[index % colors.length];
}
