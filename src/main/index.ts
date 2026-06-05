import { app, BrowserWindow, Menu } from 'electron';
import { createWindow } from './window';
import { registerIpc } from './ipc';

app.whenReady().then(() => {
  // Drop the default application menu so Alt-based pane shortcuts reach the
  // renderer instead of toggling the (auto-hidden) menu bar.
  Menu.setApplicationMenu(null);

  // `spawnWindow` is the IPC-aware window factory: it wraps createWindow with
  // per-window session ownership so closing one window reaps only its sessions.
  const { manager, spawnWindow } = registerIpc(createWindow);
  // Terminals no longer kill their pty on unmount (so panes/tabs can move
  // between tabs and windows without restarting), so reap every session when
  // the whole app quits.
  app.on('before-quit', () => manager.killAll());
  spawnWindow();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) spawnWindow();
  });
});

app.on('window-all-closed', () => {
  if (process.platform !== 'darwin') app.quit();
});
