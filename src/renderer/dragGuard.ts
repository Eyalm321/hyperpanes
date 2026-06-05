// Suppress native text-selection for the lifetime of a drag. Grabbing a tab or a
// pane header and moving would otherwise start a text selection that drags across
// the UI (most visibly highlighting the top bar). CSS `user-select: none` on the
// handles isn't enough once pointer capture moves to <html> and the cursor sweeps
// over selectable content (terminal bodies are `user-select: text`).
//
// We add a body class (CSS forces `user-select: none` everywhere) and clear any
// selection that already started. The guard removes itself on the next pointerup/
// pointercancel registered on `window` in the capture phase — that fires even after
// capture has been moved to <html> or the gesture has been handed to a torn-off
// window, so the class can't get stuck on.
export function beginDragGuard(): void {
  if (document.body.classList.contains('hp-dragging')) return;
  document.body.classList.add('hp-dragging');
  window.getSelection?.()?.removeAllRanges();
  const end = () => {
    document.body.classList.remove('hp-dragging');
    window.removeEventListener('pointerup', end, true);
    window.removeEventListener('pointercancel', end, true);
  };
  window.addEventListener('pointerup', end, true);
  window.addEventListener('pointercancel', end, true);
}
