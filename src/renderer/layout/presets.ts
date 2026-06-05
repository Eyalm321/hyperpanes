import type { Layout } from '../types';
import { clampFraction, equalSizes, normalize } from './sizes';

export const LAYOUTS: { id: Layout; label: string; icon: string }[] = [
  { id: 'single', label: 'Single', icon: '□' },
  { id: 'columns', label: 'Columns', icon: '▥' },
  { id: 'rows', label: 'Rows', icon: '▤' },
  { id: 'grid', label: 'Grid', icon: '▦' },
  { id: 'main-stack', label: 'Main + Stack', icon: '▧' }
];

// The 'auto' layout entry, kept out of LAYOUTS so computeTiles/computeDividers
// keep their exhaustive switch over the 5 concrete presets. The Layout menu and
// command palette prepend this so 'auto' is selectable without ever reaching the
// tiling functions un-resolved.
export const AUTO_LAYOUT: { id: Layout; label: string; icon: string } = {
  id: 'auto',
  label: 'Automatic',
  icon: '⊞'
};

// The columns→grid boundary for auto: 2..AUTO_COLUMNS_MAX panes tile as columns,
// more tile as a grid. A single tunable knob (Q2).
export const AUTO_COLUMNS_MAX = 3;

/**
 * Resolve a layout to a concrete preset for a given pane count. 'auto' maps
 * 1 → single, 2..AUTO_COLUMNS_MAX → columns, more → grid; 'main-stack' and
 * 'rows' are manual-only and never produced here. Concrete layouts pass through
 * unchanged, so computeTiles/computeDividers/focusDirection always see one of
 * the 5 real presets — never 'auto'.
 */
export function effectiveLayout(layout: Layout, n: number): Layout {
  if (layout !== 'auto') return layout;
  if (n <= 1) return 'single';
  if (n <= AUTO_COLUMNS_MAX) return 'columns';
  return 'grid';
}

/** A rectangle in fractions of the container (0..1). */
export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Tile {
  index: number; // index into the pane order
  rect: Rect;
  visible: boolean;
}

export interface DividerDesc {
  id: string;
  kind: 'size' | 'main';
  orientation: 'vertical' | 'horizontal';
  index: number; // boundary after pane `index` (for kind 'size'); -1 for 'main'
  at: number; // position along the axis, fraction 0..1
}

const FULL: Rect = { x: 0, y: 0, w: 1, h: 1 };

/**
 * Maps (layout, pane count, sizes) to a rectangle per pane. Every pane gets a
 * tile every time (panes stay mounted); `visible: false` just hides it (used by
 * the `single` preset) so terminal sessions and scrollback are never destroyed.
 */
export function computeTiles(
  layout: Layout,
  n: number,
  sizes: number[],
  mainFraction: number,
  focusedIndex: number
): Tile[] {
  if (n <= 0) return [];
  if (n === 1) return [{ index: 0, rect: FULL, visible: true }];

  const norm = normalize(sizes.length === n ? sizes : equalSizes(n));
  const tiles: Tile[] = [];

  switch (layout) {
    case 'single': {
      const shown = focusedIndex >= 0 && focusedIndex < n ? focusedIndex : 0;
      for (let i = 0; i < n; i++) tiles.push({ index: i, rect: FULL, visible: i === shown });
      return tiles;
    }
    case 'columns': {
      let x = 0;
      for (let i = 0; i < n; i++) {
        tiles.push({ index: i, rect: { x, y: 0, w: norm[i], h: 1 }, visible: true });
        x += norm[i];
      }
      return tiles;
    }
    case 'rows': {
      let y = 0;
      for (let i = 0; i < n; i++) {
        tiles.push({ index: i, rect: { x: 0, y, w: 1, h: norm[i] }, visible: true });
        y += norm[i];
      }
      return tiles;
    }
    case 'grid': {
      const cols = Math.ceil(Math.sqrt(n));
      const rows = Math.ceil(n / cols);
      for (let i = 0; i < n; i++) {
        const r = Math.floor(i / cols);
        const itemsInRow = r < rows - 1 ? cols : n - cols * (rows - 1);
        const c = i - r * cols;
        tiles.push({
          index: i,
          rect: { x: c / itemsInRow, y: r / rows, w: 1 / itemsInRow, h: 1 / rows },
          visible: true
        });
      }
      return tiles;
    }
    case 'main-stack': {
      const mf = clampFraction(mainFraction);
      tiles.push({ index: 0, rect: { x: 0, y: 0, w: mf, h: 1 }, visible: true });
      const stackN = n - 1;
      const h = 1 / stackN;
      for (let i = 1; i < n; i++) {
        tiles.push({ index: i, rect: { x: mf, y: (i - 1) * h, w: 1 - mf, h }, visible: true });
      }
      return tiles;
    }
    default:
      return tiles;
  }
}

/** Draggable seams for the current layout. Phase 2 resizes columns, rows, and
 * the main divider of main-stack; grid and the stack interior use fixed splits. */
export function computeDividers(
  layout: Layout,
  n: number,
  sizes: number[],
  mainFraction: number
): DividerDesc[] {
  if (n < 2) return [];
  const norm = normalize(sizes.length === n ? sizes : equalSizes(n));
  const out: DividerDesc[] = [];

  if (layout === 'columns') {
    let x = 0;
    for (let i = 0; i < n - 1; i++) {
      x += norm[i];
      out.push({ id: `v-${i}`, kind: 'size', orientation: 'vertical', index: i, at: x });
    }
  } else if (layout === 'rows') {
    let y = 0;
    for (let i = 0; i < n - 1; i++) {
      y += norm[i];
      out.push({ id: `h-${i}`, kind: 'size', orientation: 'horizontal', index: i, at: y });
    }
  } else if (layout === 'main-stack') {
    out.push({ id: 'main', kind: 'main', orientation: 'vertical', index: -1, at: clampFraction(mainFraction) });
  }

  return out;
}
