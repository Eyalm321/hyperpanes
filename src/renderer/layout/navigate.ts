import type { Direction } from '../types';
import type { Tile } from './presets';

function center(t: Tile) {
  return { x: t.rect.x + t.rect.w / 2, y: t.rect.y + t.rect.h / 2 };
}

/**
 * Picks the nearest tile in `dir` from the tile at `fromIndex`, scoring by the
 * distance along the travel axis plus a penalty for perpendicular drift (so
 * focus moves to the best-aligned neighbour). Returns its index or null.
 */
export function neighborIndex(tiles: Tile[], fromIndex: number, dir: Direction): number | null {
  const from = tiles.find((t) => t.index === fromIndex);
  if (!from) return null;
  const fc = center(from);

  let best: { idx: number; score: number } | null = null;
  for (const t of tiles) {
    if (t.index === fromIndex) continue;
    const c = center(t);
    const dx = c.x - fc.x;
    const dy = c.y - fc.y;

    let ok = false;
    let score = 0;
    if (dir === 'right') {
      ok = dx > 0.001;
      score = dx + Math.abs(dy) * 2;
    } else if (dir === 'left') {
      ok = dx < -0.001;
      score = -dx + Math.abs(dy) * 2;
    } else if (dir === 'down') {
      ok = dy > 0.001;
      score = dy + Math.abs(dx) * 2;
    } else {
      ok = dy < -0.001;
      score = -dy + Math.abs(dx) * 2;
    }

    if (ok && (best === null || score < best.score)) best = { idx: t.index, score };
  }

  return best ? best.idx : null;
}
