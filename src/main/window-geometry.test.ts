import { describe, it, expect } from 'vitest';
import { originForDrop, GRAB_OFFSET_X, GRAB_OFFSET_Y, DOCK_BAND_HEIGHT } from './window-geometry';

const SIZE = { width: 1280, height: 820 };
// A primary display at the origin with a 40px taskbar at the bottom.
const PRIMARY = { x: 0, y: 0, width: 1920, height: 1040 };
// A secondary display to the right of the primary.
const SECONDARY = { x: 1920, y: 0, width: 1920, height: 1040 };

describe('grab geometry', () => {
  it('lands the grab point inside the tab-bar dock band, not the pane area', () => {
    // If GRAB_OFFSET_Y ever exceeds the band, a torn-off/followed window appears
    // with the cursor over its panes (and wouldn't be detected as a dock target).
    expect(GRAB_OFFSET_Y).toBeGreaterThan(0);
    expect(GRAB_OFFSET_Y).toBeLessThan(DOCK_BAND_HEIGHT);
  });
});

describe('originForDrop', () => {
  it('offsets the cursor over the tab strip when there is room', () => {
    // The 820px-tall window leaves only ~220px of vertical play on a 1040px work
    // area, so keep the drop point high enough to avoid the bottom clamp.
    const at = { x: 400, y: 200 };
    expect(originForDrop(at, PRIMARY, SIZE)).toEqual({
      x: at.x - GRAB_OFFSET_X,
      y: at.y - GRAB_OFFSET_Y
    });
  });

  it('clamps against the right and bottom edges so the window stays on-screen', () => {
    const origin = originForDrop({ x: 1900, y: 1030 }, PRIMARY, SIZE);
    expect(origin.x + SIZE.width).toBeLessThanOrEqual(PRIMARY.x + PRIMARY.width);
    expect(origin.y + SIZE.height).toBeLessThanOrEqual(PRIMARY.y + PRIMARY.height);
    expect(origin).toEqual({
      x: PRIMARY.width - SIZE.width,
      y: PRIMARY.height - SIZE.height
    });
  });

  it('clamps against the left and top edges to the work-area origin', () => {
    expect(originForDrop({ x: 5, y: 5 }, PRIMARY, SIZE)).toEqual({ x: 0, y: 0 });
  });

  it('lands fully inside a secondary display with a non-zero origin', () => {
    const at = { x: 2200, y: 200 };
    const origin = originForDrop(at, SECONDARY, SIZE);
    expect(origin.x).toBeGreaterThanOrEqual(SECONDARY.x);
    expect(origin.y).toBeGreaterThanOrEqual(SECONDARY.y);
    expect(origin.x + SIZE.width).toBeLessThanOrEqual(SECONDARY.x + SECONDARY.width);
    expect(origin.y + SIZE.height).toBeLessThanOrEqual(SECONDARY.y + SECONDARY.height);
    expect(origin).toEqual({ x: at.x - GRAB_OFFSET_X, y: at.y - GRAB_OFFSET_Y });
  });

  it('clamps near a secondary display edge instead of bleeding onto the primary', () => {
    const origin = originForDrop({ x: 1950, y: 500 }, SECONDARY, SIZE);
    expect(origin.x).toBe(SECONDARY.x);
  });

  it('pins to the work-area top-left when the window is larger than the display', () => {
    const tiny = { x: 100, y: 100, width: 800, height: 600 };
    expect(originForDrop({ x: 400, y: 400 }, tiny, SIZE)).toEqual({ x: tiny.x, y: tiny.y });
  });
});
