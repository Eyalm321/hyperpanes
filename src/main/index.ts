import { mark } from './metrics'; // first import → t0 ≈ process start
import { app, BrowserWindow, Menu } from 'electron';
import { createWindow } from './window';
import { registerIpc, type IpcHandle, type SpawnWindowOpts } from './ipc';
import {
  getInitialWindows,
  resolveSecondInstanceWindows,
  type WindowSpec
} from './workspace';

// One canonical process. A second `hyperpanes …` invocation must route into THIS
// process (adding windows) rather than starting a rival — the foundation under
// launch-time multi-window and, later, the MCP control surface. If we don't hold
// the lock, another instance does: hand it our argv (Electron delivers it to the
// primary's `second-instance` event) and quit.
if (!app.requestSingleInstanceLock()) {
  app.quit();
} else {
  let spawnWindow: IpcHandle['spawnWindow'] | null = null;

  // Open a window per spec; bounds win over a cascade stagger so file-described
  // layouts land where asked and ad-hoc multi-window launches don't stack.
  const openWindows = (windows: WindowSpec[]) => {
    if (!spawnWindow) return;
    windows.forEach((w, i) => {
      const opts: SpawnWindowOpts = { windowSpec: w, cascadeIndex: i };
      if (w.bounds) opts.bounds = w.bounds;
      spawnWindow!(undefined, undefined, opts);
    });
  };

  const focusExisting = () => {
    const win = BrowserWindow.getAllWindows().find((w) => !w.isDestroyed());
    if (win) {
      if (win.isMinimized()) win.restore();
      win.focus();
    }
  };

  // A second launch while we're already running. Open the windows it asked for;
  // a bare relaunch (no CLI/json) just surfaces the running app.
  app.on('second-instance', (_e, argv, cwd) => {
    const windows = resolveSecondInstanceWindows(argv, cwd);
    if (windows.length === 0) focusExisting();
    else openWindows(windows);
  });

  app.whenReady().then(() => {
    mark('whenReady');
    // Drop the default application menu so Alt-based pane shortcuts reach the
    // renderer instead of toggling the (auto-hidden) menu bar.
    Menu.setApplicationMenu(null);

    // `spawnWindow` is the IPC-aware window factory: it wraps createWindow with
    // per-window session ownership so closing one window reaps only its sessions.
    const handle = registerIpc(createWindow);
    spawnWindow = handle.spawnWindow;
    const { manager, control } = handle;
    // Terminals no longer kill their pty on unmount (so panes/tabs can move
    // between tabs and windows without restarting), so reap every session when
    // the whole app quits; tear the control server down so its discovery file
    // doesn't outlive the port.
    app.on('before-quit', () => {
      manager.killAll();
      control.shutdown();
    });

    // Split the launch workspace (CLI / json / restored last session) across one
    // window per spec; nothing to restore → a single bare window (the renderer
    // seeds a shell).
    const windows = getInitialWindows();
    if (windows.length === 0) spawnWindow();
    else openWindows(windows);

    app.on('activate', () => {
      if (BrowserWindow.getAllWindows().length === 0) spawnWindow?.();
    });
  });
}

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
