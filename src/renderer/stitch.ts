import { useWorkspace } from './store/useWorkspace';
import { effectiveLayout } from './layout/presets';

export type StitchSlot = {
  groupId: string;
  paneId: string;
  edge: 'left' | 'right' | 'top' | 'bottom';
};

// Fraction of a tile (along the layout axis) at each end that counts as the drop
// zone for a stitch. Only when the cursor is within this band of a side does the
// redock indicator appear; the central band is a no-op, so a pane "redocks" only
// when you aim near its sides. Capped in px so the band stays edge-like on a very
// large tile.
const EDGE_BAND_FRAC = 0.3;
const EDGE_BAND_MAX_PX = 140;

// The pane + near edge under (clientX, clientY), or null if not over a pane tile
// OR if the cursor sits in the tile's central (non-stitch) band. `exclude` skips
// the dragged pane so it never targets itself. The edge is along the target
// group's layout axis (rows → top/bottom, otherwise left/right). Shared by the
// in-window pane drag and the cross-window/tab-float stitch preview, so both place
// a pane the same way — and both gate the redock effect to the sides.
export function stitchSlotAt(x: number, y: number, exclude?: string): StitchSlot | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  const tile = el?.closest('[data-pane-id]') as HTMLElement | null;
  const paneId = tile?.getAttribute('data-pane-id') ?? undefined;
  const groupId = tile?.getAttribute('data-group-id') ?? undefined;
  if (!tile || !paneId || !groupId || paneId === exclude) return null;
  const g = useWorkspace.getState().groups.find((gg) => gg.id === groupId);
  const vertical = g ? effectiveLayout(g.layout, g.panes.length) === 'rows' : false;
  const r = tile.getBoundingClientRect();
  // Distance from the start edge (top/left) along the axis, and the axis size.
  const pos = vertical ? y - r.top : x - r.left;
  const size = vertical ? r.height : r.width;
  const band = Math.min(size * EDGE_BAND_FRAC, EDGE_BAND_MAX_PX);
  let edge: StitchSlot['edge'] | null = null;
  if (pos <= band) edge = vertical ? 'top' : 'left';
  else if (pos >= size - band) edge = vertical ? 'bottom' : 'right';
  if (!edge) return null; // central band → not a stitch target
  return { groupId, paneId, edge };
}

// The insertion index in the slot's group for that edge (before/after the pane).
export function slotToIndex(slot: StitchSlot): number {
  const g = useWorkspace.getState().groups.find((gg) => gg.id === slot.groupId);
  const j = g?.panes.findIndex((p) => p.id === slot.paneId) ?? -1;
  if (j < 0) return g?.panes.length ?? 0;
  const before = slot.edge === 'left' || slot.edge === 'top';
  return j + (before ? 0 : 1);
}
