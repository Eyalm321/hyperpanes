import { useWorkspace } from './store/useWorkspace';
import { useUI } from './store/useUI';
import { stitchSlotAt, slotToIndex } from './stitch';
import { startLiveTearOff } from './liveTearOff';
import { beginDragGuard } from './dragGuard';

// How long the cursor must rest on a tab before it springs open (Chrome/Finder
// spring-loaded tabs), so you can drop the pane into that tab's layout.
const SPRING_DELAY_MS = 450;

// Begin a pane drag (header pulled past the threshold). Capture is moved to <html>
// so the drag survives the source pane's tab being hidden when a tab springs open;
// a global ghost (useUI.paneGhost) follows the cursor. Each move hit-tests the
// cursor:
//   • over a tab        → highlight it and, after a hold, switch to it (spring);
//   • over the +/strip  → mark a "new tab" drop;
//   • over a pane in ANOTHER tab's layout → show an insert indicator at the slot;
//   • outside the window → hand off to the live tear-off (new window).
// On release the corresponding move runs.
export function beginPaneDrag(
  pointerId: number,
  paneId: string,
  sourceGroupId: string,
  label: string
): void {
  // Kill native text-selection for the drag's lifetime (the header label and the
  // terminal bodies are otherwise selectable, so a drag paints a selection across
  // the UI). Self-clears on the next pointerup.
  beginDragGuard();

  const root = document.documentElement;
  try {
    root.setPointerCapture(pointerId);
  } catch {
    /* capture unsupported — drag still works while over this window */
  }

  let springTabId: string | null = null;
  let springTimer: ReturnType<typeof setTimeout> | undefined;
  const clearSpring = () => {
    if (springTimer !== undefined) {
      clearTimeout(springTimer);
      springTimer = undefined;
    }
    springTabId = null;
  };

  const cleanup = () => {
    clearSpring();
    root.removeEventListener('pointermove', onMove);
    root.removeEventListener('pointerup', onUp);
    root.removeEventListener('pointercancel', onCancel);
    try {
      root.releasePointerCapture(pointerId);
    } catch {
      /* already released */
    }
    const u = useUI.getState();
    u.setPaneGhost(null);
    u.setPaneDropTarget(null);
    u.setLayoutDrop(null);
  };

  const onMove = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    const u = useUI.getState();
    u.setPaneGhost({ x: e.clientX, y: e.clientY, label });

    const el = document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null;
    if (!el) {
      // Left the window → pop the pane into its own following window.
      cleanup();
      const ws = useWorkspace.getState();
      const src = ws.groups.find((g) => g.id === sourceGroupId);
      if (ws.groups.length === 1 && src && src.panes.length === 1) {
        startLiveTearOff(pointerId, src, { moveWindow: true });
      } else {
        const payload = ws.extractPaneAsGroup(paneId);
        if (payload) startLiveTearOff(pointerId, payload);
      }
      return;
    }

    // Over a tab → highlight + spring-load after a hold.
    const tabEl = el.closest('.hp-tab[data-group-id]') as HTMLElement | null;
    if (tabEl) {
      const gid = tabEl.getAttribute('data-group-id')!;
      u.setPaneDropTarget(gid);
      u.setLayoutDrop(null);
      if (springTabId !== gid) {
        clearSpring();
        springTabId = gid;
        springTimer = setTimeout(() => {
          useWorkspace.getState().setActiveGroup(gid);
          clearSpring();
        }, SPRING_DELAY_MS);
      }
      return;
    }
    clearSpring();

    if (el.closest('.hp-tab-new') || el.closest('.hp-tabstrip')) {
      u.setPaneDropTarget('new');
      u.setLayoutDrop(null);
      return;
    }

    // Over a pane (a sibling in this layout, or a target pane after a spring) → an
    // insert indicator on the near edge. Excludes the dragged pane itself.
    const slot = stitchSlotAt(e.clientX, e.clientY, paneId);
    if (slot) {
      u.setLayoutDrop(slot);
      u.setPaneDropTarget(null);
      return;
    }

    // Over the body of a tab we sprang INTO (not a specific pane) → append there.
    // (Only after a spring — over the source tab's own body this falls to cancel.)
    const ws = useWorkspace.getState();
    if (ws.activeId !== sourceGroupId) {
      u.setPaneDropTarget(ws.activeId);
      u.setLayoutDrop(null);
      return;
    }
    u.setPaneDropTarget(null);
    u.setLayoutDrop(null);
  };

  const onUp = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    const u = useUI.getState();
    const dropTarget = u.paneDropTarget;
    const layoutDrop = u.layoutDrop;
    cleanup();
    const ws = useWorkspace.getState();
    if (layoutDrop) {
      ws.movePaneToGroup(paneId, layoutDrop.groupId, slotToIndex(layoutDrop));
      ws.setActiveGroup(layoutDrop.groupId);
    } else if (dropTarget === 'new') {
      ws.movePaneToNewGroup(paneId);
    } else if (dropTarget) {
      ws.movePaneToGroup(paneId, dropTarget);
    }
    // else: released over empty body → cancel.
  };

  const onCancel = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    cleanup();
  };

  root.addEventListener('pointermove', onMove);
  root.addEventListener('pointerup', onUp);
  root.addEventListener('pointercancel', onCancel);
}
