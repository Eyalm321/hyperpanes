import { BrowserWindow, screen } from 'electron';
import { existsSync } from 'fs';
import { join } from 'path';
import { originForDrop, WINDOW_WIDTH, WINDOW_HEIGHT } from './window-geometry';

// The packaged app gets its icon from electron-builder (build/icon.ico). For the
// dev run we point the window at the generated PNG if it's there.
function devIcon(): string | undefined {
  const png = join(__dirname, '../../build/icon.png');
  return existsSync(png) ? png : undefined;
}

// `at` is the drop point of a torn-off tab/pane (see ipc.ts drag handlers). When
// given, the new window opens under the cursor on the display nearest the drop,
// clamped on-screen; otherwise it falls back to OS-centered placement. `inactive`
// shows the window without stealing focus — used for the live drag window so the
// source window keeps the pointer capture driving the drag.
export function createWindow(
  at?: { x: number; y: number },
  opts?: { inactive?: boolean }
): BrowserWindow {
  const origin = at
    ? originForDrop(at, screen.getDisplayNearestPoint(at).workArea, {
        width: WINDOW_WIDTH,
        height: WINDOW_HEIGHT
      })
    : undefined;
  const win = new BrowserWindow({
    width: WINDOW_WIDTH,
    height: WINDOW_HEIGHT,
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

  // With `show:false` (the live drag window) we still need to reveal it — but
  // without focus, so the dragging source window keeps its pointer capture.
  if (opts?.inactive) {
    win.once('ready-to-show', () => {
      if (!win.isDestroyed()) win.showInactive();
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
