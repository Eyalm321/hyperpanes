import { describe, expect, it } from 'vitest';
import { MIN_SIZE, clampFraction, equalSizes, insertSize, normalize, removeSize, resizeAt } from './sizes';

const sum = (a: number[]) => a.reduce((x, y) => x + y, 0);

describe('sizes', () => {
  it('equalSizes sums to 1 (and handles empty)', () => {
    expect(sum(equalSizes(4))).toBeCloseTo(1);
    expect(equalSizes(0)).toEqual([]);
  });

  it('insertSize adds a slot keeping the total ~1', () => {
    const s = insertSize(equalSizes(2), 1);
    expect(s).toHaveLength(3);
    expect(sum(s)).toBeCloseTo(1);
  });

  it('removeSize drops a slot keeping the total ~1', () => {
    const s = removeSize(equalSizes(3), 0);
    expect(s).toHaveLength(2);
    expect(sum(s)).toBeCloseTo(1);
  });

  it('resizeAt shifts the boundary and respects MIN_SIZE', () => {
    const s = resizeAt([0.5, 0.5], 0, 0.1);
    expect(s[0]).toBeCloseTo(0.6);
    expect(s[1]).toBeCloseTo(0.4);
    // clamped: cannot push a neighbour below MIN_SIZE
    expect(resizeAt([0.5, 0.5], 0, -0.9)).toEqual([0.5, 0.5]);
  });

  it('clampFraction bounds within [MIN_SIZE, 1-MIN_SIZE]', () => {
    expect(clampFraction(0)).toBe(MIN_SIZE);
    expect(clampFraction(1)).toBe(1 - MIN_SIZE);
    expect(clampFraction(0.5)).toBe(0.5);
  });

  it('normalize scales any positive vector to sum 1', () => {
    expect(sum(normalize([1, 1, 2]))).toBeCloseTo(1);
  });
});
