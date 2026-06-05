import type { GroupPayload } from './types';

// Begin a live tear-off: main opens a real window seeded with `group` at the
// cursor and follows it until the pointer is released.
//
// The caller has just extracted the dragged tab/pane, which unmounts the DOM node
// it was dragging — and with it any pointer capture on that node. So we move the
// capture onto <html> (which never unmounts) and listen there for the release.
// That keeps the cross-window `pointerup` flowing even after the cursor has left
// the source window, which is what ends the drag.
export function startLiveTearOff(
  pointerId: number,
  group: GroupPayload,
  opts?: { moveWindow?: boolean }
): void {
  const root = document.documentElement;
  try {
    root.setPointerCapture(pointerId);
  } catch {
    /* capture unsupported — the drag still works while over this window */
  }
  void window.hp.win.dragDetach(group, opts?.moveWindow);

  const cleanup = () => {
    root.removeEventListener('pointerup', onUp);
    root.removeEventListener('pointercancel', onCancel);
    try {
      root.releasePointerCapture(pointerId);
    } catch {
      /* already released */
    }
  };
  const onUp = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    cleanup();
    void window.hp.win.dragDrop();
  };
  const onCancel = (e: PointerEvent) => {
    if (e.pointerId !== pointerId) return;
    cleanup();
    void window.hp.win.dragCancel();
  };
  root.addEventListener('pointerup', onUp);
  root.addEventListener('pointercancel', onCancel);
}
