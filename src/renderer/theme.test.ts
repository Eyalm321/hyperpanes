import { describe, expect, it } from 'vitest';
import {
  DEFAULT_PALETTE,
  PALETTES,
  PALETTE_NAMES,
  nextColor,
  paletteColors,
  remapColor,
  slotOf
} from './theme';

describe('theme palettes', () => {
  it('every palette has the same 8 parallel slots', () => {
    const len = PALETTES[DEFAULT_PALETTE].length;
    expect(len).toBe(8);
    for (const name of PALETTE_NAMES) {
      expect(paletteColors(name)).toHaveLength(len);
    }
  });

  it('all colors within a palette are distinct (so panes stay distinguishable)', () => {
    for (const name of PALETTE_NAMES) {
      const lower = paletteColors(name).map((c) => c.toLowerCase());
      expect(new Set(lower).size).toBe(lower.length);
    }
  });

  it('nextColor cycles within the requested palette', () => {
    const light = paletteColors('light');
    expect(nextColor(0, 'light')).toBe(light[0]);
    expect(nextColor(light.length, 'light')).toBe(light[0]); // wraps
    expect(nextColor(1, 'light')).toBe(light[1]);
  });

  it('nextColor defaults to the default palette', () => {
    expect(nextColor(2)).toBe(PALETTES[DEFAULT_PALETTE][2]);
  });

  it('slotOf finds a color case-insensitively, -1 for non-members', () => {
    expect(slotOf(PALETTES.dark[3])).toBe(3);
    expect(slotOf(PALETTES.dark[3].toUpperCase())).toBe(3);
    expect(slotOf('#abc123')).toBe(-1);
  });

  it('remapColor moves a slot color into the target palette by index', () => {
    const slot = 4;
    expect(remapColor(PALETTES.dark[slot], 'light')).toBe(PALETTES.light[slot]);
    expect(remapColor(PALETTES.medium[slot], 'grayscale')).toBe(PALETTES.grayscale[slot]);
  });

  it('remapColor leaves custom (non-slot) colors unchanged', () => {
    expect(remapColor('#abc123', 'light')).toBe('#abc123');
  });

  it('remapColor into a color’s own palette is a no-op', () => {
    expect(remapColor(PALETTES.light[2], 'light')).toBe(PALETTES.light[2]);
  });

  it('remapColor recovers a legacy/orphaned color by slot', () => {
    // A pre-vivid "light" pastel that no current palette contains; it must still
    // resolve to slot 0 and remap, not be treated as a stuck custom color.
    const legacyLightSlot0 = '#f6a8aa';
    expect(slotOf(legacyLightSlot0)).toBe(0);
    expect(remapColor(legacyLightSlot0, 'dark')).toBe(PALETTES.dark[0]);
    expect(remapColor(legacyLightSlot0, 'light')).toBe(PALETTES.light[0]);
  });

  it('round-trips a slot color through a remap and back', () => {
    const original = PALETTES.dark[6];
    const toLight = remapColor(original, 'light');
    expect(remapColor(toLight, 'dark')).toBe(original);
  });
});
