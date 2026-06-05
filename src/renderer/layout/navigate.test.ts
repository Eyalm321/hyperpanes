import { describe, expect, it } from 'vitest';
import { neighborIndex } from './navigate';
import { computeTiles } from './presets';
import { equalSizes } from './sizes';

// 2x2 grid order: 0 top-left, 1 top-right, 2 bottom-left, 3 bottom-right
const grid = () => computeTiles('grid', 4, equalSizes(4), 0.6, 0);

describe('neighborIndex', () => {
  it('moves across a 2x2 grid', () => {
    const t = grid();
    expect(neighborIndex(t, 0, 'right')).toBe(1);
    expect(neighborIndex(t, 0, 'down')).toBe(2);
    expect(neighborIndex(t, 1, 'left')).toBe(0);
    expect(neighborIndex(t, 3, 'up')).toBe(1);
  });

  it('returns null when there is no neighbour that way', () => {
    const t = grid();
    expect(neighborIndex(t, 0, 'left')).toBeNull();
    expect(neighborIndex(t, 0, 'up')).toBeNull();
  });
});
