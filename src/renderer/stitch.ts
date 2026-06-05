import { useWorkspace } from './store/useWorkspace';
import { effectiveLayout } from './layout/presets';

export type StitchSlot = {
  groupId: string;
  paneId: string;
  edge: 'left' | 'right' | 'top' | 'bottom';
};

// The pane + near edge under (clientX, clientY), or null if not over a pane tile.
// `exclude` skips the dragged pane so it never targets itself. The edge is along
// the target group's layout axis (rows → top/bottom, otherwise left/right). Shared
// by the in-window pane drag and the cross-window stitch preview, so both place a
// pane the same way.
export function stitchSlotAt(x: number, y: number, exclude?: string): StitchSlot | null {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  const tile = el?.closest('[data-pane-id]') as HTMLElement | null;
  const paneId = tile?.getAttribute('data-pane-id') ?? undefined;
  const groupId = tile?.getAttribute('data-group-id') ?? undefined;
  if (!tile || !paneId || !groupId || paneId === exclude) return null;
  const g = useWorkspace.getState().groups.find((gg) => gg.id === groupId);
  const vertical = g ? effectiveLayout(g.layout, g.panes.length) === 'rows' : false;
  const r = tile.getBoundingClientRect();
  const edge = vertical
    ? y < r.top + r.height / 2
      ? 'top'
      : 'bottom'
    : x < r.left + r.width / 2
      ? 'left'
      : 'right';
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
