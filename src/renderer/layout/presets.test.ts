import { describe, expect, it } from 'vitest';
import { computeDividers, computeTiles, effectiveLayout } from './presets';
import { equalSizes } from './sizes';

describe('computeTiles', () => {
  it('a single pane fills the area for any layout', () => {
    const t = computeTiles('grid', 1, [1], 0.6, 0);
    expect(t).toHaveLength(1);
    expect(t[0].rect).toEqual({ x: 0, y: 0, w: 1, h: 1 });
    expect(t[0].visible).toBe(true);
  });

  it('columns are full-height and their widths sum to 1', () => {
    const t = computeTiles('columns', 3, equalSizes(3), 0.6, 0);
    expect(t).toHaveLength(3);
    expect(t.every((x) => x.visible && x.rect.h === 1)).toBe(true);
    expect(t.reduce((a, x) => a + x.rect.w, 0)).toBeCloseTo(1);
  });

  it('single layout shows only the focused pane', () => {
    const t = computeTiles('single', 3, equalSizes(3), 0.6, 1);
    expect(t.filter((x) => x.visible)).toHaveLength(1);
    expect(t[1].visible).toBe(true);
  });

  it('grid keeps every tile within bounds', () => {
    const t = computeTiles('grid', 4, equalSizes(4), 0.6, 0);
    expect(t).toHaveLength(4);
    expect(t.every((x) => x.rect.x >= 0 && x.rect.x + x.rect.w <= 1.0001)).toBe(true);
  });

  it('main-stack gives pane 0 the main width and stacks the rest', () => {
    const t = computeTiles('main-stack', 3, equalSizes(3), 0.6, 0);
    expect(t[0].rect.w).toBeCloseTo(0.6);
    expect(t[1].rect.x).toBeCloseTo(0.6);
    expect(t[2].rect.x).toBeCloseTo(0.6);
  });
});

describe('effectiveLayout', () => {
  it('maps 1 pane to single', () => {
    expect(effectiveLayout('auto', 1)).toBe('single');
  });

  it('maps 2 and 3 panes to columns', () => {
    expect(effectiveLayout('auto', 2)).toBe('columns');
    expect(effectiveLayout('auto', 3)).toBe('columns');
  });

  it('maps 4+ panes to grid', () => {
    expect(effectiveLayout('auto', 4)).toBe('grid');
    expect(effectiveLayout('auto', 9)).toBe('grid');
    expect(effectiveLayout('auto', 25)).toBe('grid');
  });

  it('treats an empty group as single', () => {
    expect(effectiveLayout('auto', 0)).toBe('single');
  });

  it('never auto-selects rows or main-stack at any count', () => {
    for (let n = 0; n <= 30; n++) {
      const eff = effectiveLayout('auto', n);
      expect(eff).not.toBe('rows');
      expect(eff).not.toBe('main-stack');
      expect(eff).not.toBe('auto');
    }
  });

  it('passes concrete layouts through unchanged regardless of count', () => {
    expect(effectiveLayout('rows', 5)).toBe('rows');
    expect(effectiveLayout('main-stack', 9)).toBe('main-stack');
    expect(effectiveLayout('single', 4)).toBe('single');
    expect(effectiveLayout('columns', 1)).toBe('columns');
    expect(effectiveLayout('grid', 2)).toBe('grid');
  });
});

describe('computeDividers', () => {
  it('columns produce n-1 vertical dividers', () => {
    expect(computeDividers('columns', 3, equalSizes(3), 0.6)).toHaveLength(2);
  });

  it('grid has no draggable dividers', () => {
    expect(computeDividers('grid', 4, equalSizes(4), 0.6)).toHaveLength(0);
  });

  it('main-stack has a single main divider', () => {
    const d = computeDividers('main-stack', 3, equalSizes(3), 0.6);
    expect(d).toHaveLength(1);
    expect(d[0].kind).toBe('main');
  });
});
