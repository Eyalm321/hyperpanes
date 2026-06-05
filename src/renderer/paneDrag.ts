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
// a global ghost (useUI.paneGhost) follows the cursor. Only a sibling pane's EDGE,
// a tab, or the strip keep the pane docked; everything else tears it off live.
// Each move hit-tests the cursor:
//   • over a tab        → highlight it and, after a hold, switch to it (spring);
//   • over the +/strip  → mark a "new tab" drop;
//   • near a sibling pane's EDGE → show an insert indicator at that slot;
//   • anywhere else (the pane's own tile — incl. dragging onto itself or down its
//     own body — another pane's dead centre, a gap, or outside the window) → tear
//     off NOW into a following window (live), like a tab pulled off its strip.
//     Main then re-docks/re-stitches it over a strip or pane edge.
// On release the docked target's move runs; a tear-off has already happened live,
// so its release is owned by the float (settles as a new window).
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

    // Over a tab → highlight + spring-load after a hold (stays docked).
    const tabEl = el?.closest('.hp-tab[data-group-id]') as HTMLElement | null;
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

    // Over the +/strip → "new tab" drop (stays docked).
    if (el && (el.closest('.hp-tab-new') || el.closest('.hp-tabstrip'))) {
      u.setPaneDropTarget('new');
      u.setLayoutDrop(null);
      return;
    }

    // Near a sibling pane's EDGE → insert indicator at that slot (stays docked).
    // stitchSlotAt only fires near the sides; a pane's centre returns null.
    const slot = el ? stitchSlotAt(e.clientX, e.clientY, paneId) : null;
    if (slot) {
      u.setLayoutDrop(slot);
      u.setPaneDropTarget(null);
      return;
    }

    const ws = useWorkspace.getState();

    // Over the body of a tab we sprang INTO (not a specific pane) → append there.
    if (el && ws.activeId !== sourceGroupId) {
      u.setPaneDropTarget(ws.activeId);
      u.setLayoutDrop(null);
      return;
    }

    // Anywhere else with no drop target — the pane's OWN tile (incl. dragging onto
    // itself or straight down its own body), another pane's dead centre, a gap, or
    // outside the window. Tear off NOW into a following window (live); main
    // re-docks/re-stitches over a strip or a pane edge, and a release here settles
    // it as its own window. Only a sibling EDGE / a tab / the strip keep it docked.
    cleanup();
    const src = ws.groups.find((g) => g.id === sourceGroupId);
    if (ws.groups.length === 1 && src && src.panes.length === 1) {
      startLiveTearOff(pointerId, src, { moveWindow: true });
    } else {
      const payload = ws.extractPaneAsGroup(paneId);
      if (payload) startLiveTearOff(pointerId, payload);
    }
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
    // else: defensive no-op. A release without a docked target only happens if a
    // move never registered a target; any dead-zone move (incl. the pane's own
    // tile) already tore off live in onMove, so it never reaches here.
  };

  const onCancel = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    cleanup();
  };

  root.addEventListener('pointermove', onMove);
  root.addEventListener('pointerup', onUp);
  root.addEventListener('pointercancel', onCancel);
}
