export interface Point {
  x: number;
  y: number;
}

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

// Default window size; shared by window.ts (creation) and ipc.ts (drag-follow).
export const WINDOW_WIDTH = 1024;
export const WINDOW_HEIGHT = 680;

// Height of the "tab bar" band at the top of a window's CONTENT area. The tab
// strip lives INSIDE the 32px top bar (CSS `.hp-topbar` height), so the dock zone
// is just that bar — anything below is pane area. ipc.ts hit-tests the cursor
// against this band to decide whether to dock the dragged tab into that window's
// strip (Chrome-style mid-drag dock). Keep in sync with `.hp-topbar` in styles.css.
export const DOCK_BAND_HEIGHT = 32;

// Offsets from the drop point to the window's top-left, so the cursor lands over a
// tab in the top bar. Tabs are 28px tall, bottom-aligned in the 32px bar (≈y 3–31,
// center ≈17); `x≈120` clears the menu/layout controls onto the first tabs.
// Purely cosmetic (where a torn-off / followed window sits under the cursor).
export const GRAB_OFFSET_X = 120;
export const GRAB_OFFSET_Y = 17;

/**
 * Top-left origin for a `size` window so the drop point `at` lands over the tab
 * strip, clamped to stay within `workArea` (the target display's usable bounds).
 * Every coordinate is DIP, so this composes directly with Electron's
 * `getCursorScreenPoint`, `BrowserWindow` bounds and `Display.workArea`.
 *
 * If the window is larger than the display, the inner `min` drops below the work
 * area's origin and the outer `max` pins the window to the display's top-left.
 */
export function originForDrop(
  at: Point,
  workArea: Rect,
  size: { width: number; height: number }
): Point {
  const x = Math.round(
    Math.max(workArea.x, Math.min(at.x - GRAB_OFFSET_X, workArea.x + workArea.width - size.width))
  );
  const y = Math.round(
    Math.max(workArea.y, Math.min(at.y - GRAB_OFFSET_Y, workArea.y + workArea.height - size.height))
  );
  return { x, y };
}
