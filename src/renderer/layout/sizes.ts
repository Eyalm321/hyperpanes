// Fractional-size helpers shared by the layout engine and the resize dividers.
// The insert/remove rebalancing is ported from vercel/hyper's term-groups reducer.

export const MIN_SIZE = 0.05;

export function equalSizes(n: number): number[] {
  if (n <= 0) return [];
  return new Array<number>(n).fill(1 / n);
}

export function normalize(sizes: number[]): number[] {
  const sum = sizes.reduce((a, b) => a + b, 0);
  if (sum <= 0) return equalSizes(sizes.length);
  return sizes.map((s) => s / sum);
}

// Are these sizes an (≈) equal split? Used to skip serializing default splits.
export function isEqualSplit(sizes: number[]): boolean {
  if (sizes.length <= 1) return true;
  const eq = 1 / sizes.length;
  return sizes.every((s) => Math.abs(s - eq) < 1e-6);
}

export function clampFraction(f: number): number {
  return Math.min(Math.max(f, MIN_SIZE), 1 - MIN_SIZE);
}

// Insert a slot at `index`, shrinking the others proportionally (Hyper insertRebalance).
export function insertSize(sizes: number[], index: number): number[] {
  if (sizes.length === 0) return [1];
  const newSize = 1 / (sizes.length + 1);
  const balanced = sizes.map((s) => s - newSize * s);
  return [...balanced.slice(0, index), newSize, ...balanced.slice(index)];
}

// Remove the slot at `index`, spreading its size across the rest (Hyper removalRebalance).
export function removeSize(sizes: number[], index: number): number[] {
  if (sizes.length <= 1) return [];
  const removed = sizes[index];
  const increase = removed / (sizes.length - 1);
  return sizes.filter((_, i) => i !== index).map((s) => s + increase);
}

// Move the boundary between slot `i` and `i+1` by `delta` (a fraction), clamped.
export function resizeAt(sizes: number[], i: number, delta: number): number[] {
  if (i < 0 || i + 1 >= sizes.length) return sizes;
  const a = sizes[i] + delta;
  const b = sizes[i + 1] - delta;
  if (a < MIN_SIZE || b < MIN_SIZE) return sizes;
  const next = [...sizes];
  next[i] = a;
  next[i + 1] = b;
  return next;
}
