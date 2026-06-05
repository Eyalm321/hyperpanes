import { BrowserWindow, screen } from 'electron';
import { existsSync } from 'fs';
import { join } from 'path';
import { originForDrop, WINDOW_WIDTH, WINDOW_HEIGHT } from './window-geometry';
import { mark } from './metrics';
import type { WindowBounds } from './workspace';

// Each launch window past the first (without explicit bounds) is nudged by this
// much from the work-area corner so they don't land exactly on top of each other.
const CASCADE_STEP = 32;

// The packaged app gets its icon from electron-builder (build/icon.ico). For the
// dev run we point the window at the generated PNG if it's there.
function devIcon(): string | undefined {
  const png = join(__dirname, '../../build/icon.png');
  return existsSync(png) ? png : undefined;
}

// Top-left for a window given explicit bounds, or undefined if neither x nor y
// was supplied (let the OS center it, honoring only width/height).
function boundsOrigin(b?: WindowBounds): { x: number; y: number } | undefined {
  if (!b || (b.x == null && b.y == null)) return undefined;
  return { x: b.x ?? 0, y: b.y ?? 0 };
}

// Staggered top-left for the nth boundless launch window on the primary display.
function cascadeOrigin(index?: number): { x: number; y: number } | undefined {
  if (!index || index <= 0) return undefined;
  const wa = screen.getPrimaryDisplay().workArea;
  const off = index * CASCADE_STEP;
  return {
    x: Math.min(wa.x + off, wa.x + Math.max(0, wa.width - WINDOW_WIDTH)),
    y: Math.min(wa.y + off, wa.y + Math.max(0, wa.height - WINDOW_HEIGHT))
  };
}

// `at` is the drop point of a torn-off tab/pane (see ipc.ts drag handlers). When
// given, the new window opens under the cursor on the display nearest the drop,
// clamped on-screen; otherwise it falls back to OS-centered placement. `inactive`
// shows the window without stealing focus — used for the live drag window so the
// source window keeps the pointer capture driving the drag.
export function createWindow(
  at?: { x: number; y: number },
  opts?: { inactive?: boolean; bounds?: WindowBounds; cascadeIndex?: number }
): BrowserWindow {
  const b = opts?.bounds;
  // Placement precedence: a drop point (live tear-off) → explicit bounds (a
  // workspace file) → a cascade offset for the nth boundless launch window →
  // OS-centered.
  const origin = at
    ? originForDrop(at, screen.getDisplayNearestPoint(at).workArea, {
        width: WINDOW_WIDTH,
        height: WINDOW_HEIGHT
      })
    : boundsOrigin(b) ?? cascadeOrigin(opts?.cascadeIndex);
  const win = new BrowserWindow({
    width: b?.width ?? WINDOW_WIDTH,
    height: b?.height ?? WINDOW_HEIGHT,
    ...(origin ? { x: origin.x, y: origin.y } : {}),
    show: !opts?.inactive,
    minWidth: 640,
    minHeight: 400,
    backgroundColor: '#11111b',
    title: 'Hyperpanes',
    icon: devIcon(),
    // No native title bar — the top bar draws its own min / maximize / close.
    frame: false,
    autoHideMenuBar: true,
    webPreferences: {
      preload: join(__dirname, '../preload/index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false
    }
  });

  // Tell the renderer when the maximized state flips so it can swap the
  // maximize/restore glyph.
  const sendMax = () => {
    if (!win.isDestroyed()) win.webContents.send('window:maximized', win.isMaximized());
  };
  win.on('maximize', sendMax);
  win.on('unmaximize', sendMax);

  // Tell the renderer when OS fullscreen flips (incl. native exits like Esc/F11 or
  // the macOS traffic-light) so it can clear pane-fullscreen and restore the bar.
  const sendFull = () => {
    if (!win.isDestroyed()) win.webContents.send('window:fullscreen', win.isFullScreen());
  };
  win.on('enter-full-screen', sendFull);
  win.on('leave-full-screen', sendFull);

  // Cold-start milestones (mark() records each once, so the first window at
  // launch wins; later float/tear-off windows don't overwrite them).
  win.once('ready-to-show', () => mark('firstWindowShown'));
  win.webContents.once('did-finish-load', () => mark('rendererLoaded'));

  // With `show:false` (the live drag window) we still need to reveal it — but
  // without focus, so the dragging source window keeps its pointer capture.
  if (opts?.inactive) {
    win.once('ready-to-show', () => {
      if (!win.isDestroyed()) win.showInactive();
    });
  }

  // Do NOT auto-maximize on launch even if the workspace was saved maximized — the
  // window always opens at its normal size (per user preference). A saved
  // fullscreen request is still honored (it's an explicit, distinct mode).
  if (b?.fullscreen) {
    win.once('ready-to-show', () => {
      if (win.isDestroyed()) return;
      win.setSimpleFullScreen(true);
    });
  }

  // electron-vite exposes the dev server URL here; falls back to the built file.
  if (process.env.ELECTRON_RENDERER_URL) {
    win.loadURL(process.env.ELECTRON_RENDERER_URL);
  } else {
    win.loadFile(join(__dirname, '../renderer/index.html'));
  }

  return win;
}
