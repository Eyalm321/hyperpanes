import { BrowserWindow, ipcMain, screen } from 'electron';
import { SessionManager } from './session-manager';
import { resolvePaths, openResolvedPath } from './paths';
import type { SpawnOptions } from './session';
import type { GroupPayload } from '../renderer/types';
import { originForDrop, WINDOW_WIDTH, WINDOW_HEIGHT, DOCK_BAND_HEIGHT } from './window-geometry';
import {
  getInitialWorkspace,
  openWorkspaceDialog,
  saveWorkspaceDialog,
  writeLast,
  type WorkspaceFile
} from './workspace';

export interface IpcHandle {
  manager: SessionManager;
  /**
   * Open a window, optionally seeded with a torn-off group (Stage 2), positioned
   * at a drop point `at` (else OS-centered), and shown without focus (`inactive`).
   */
  spawnWindow: (
    seed?: GroupPayload,
    at?: { x: number; y: number },
    opts?: { inactive?: boolean }
  ) => BrowserWindow;
}

/**
 * Wires every renderer<->main channel for ALL windows. Session output is
 * broadcast to every window and filtered by uid in the renderer, so ptys aren't
 * tied to the window that spawned them — that's what lets a tab (and its live
 * shells) move between windows. The calling window is resolved per-message via
 * `BrowserWindow.fromWebContents`, so window controls and dialogs target the
 * right window with no global "current window".
 *
 * `buildWindow` is the bare BrowserWindow factory (see window.ts); `spawnWindow`
 * wraps it with the session-ownership bookkeeping below.
 */
export function registerIpc(
  buildWindow: (at?: { x: number; y: number }, opts?: { inactive?: boolean }) => BrowserWindow
): IpcHandle {
  const manager = new SessionManager();

  // Which window each live session belongs to (BrowserWindow.id). Updated on
  // every spawn/attach, so a session re-attached by a different window is now
  // owned by that window — and the original window closing won't reap it.
  const owner = new Map<string, number>();
  // A group waiting for its freshly-opened window to pull it via window:getSeed.
  const pendingSeed = new Map<number, GroupPayload>();
  // The session-of-record window — the only one that persists last-workspace.json.
  let primaryWindowId: number | null = null;
  // The in-flight live tear-off (Chrome-style). `floatWinId` is the real seeded
  // window that follows the cursor; while the cursor is over another window's tab
  // bar that float is HIDDEN and `previewWinId` shows a dock-preview ghost in that
  // window's strip. `group` is kept so a drop can re-home the live sessions.
  let dragState: {
    group: GroupPayload;
    timer: ReturnType<typeof setInterval>;
    floatWinId: number;
    previewWinId: number | null; // window showing a tab dock-preview (strip), or null
    stitchWinId: number | null; // window showing a pane stitch-preview (body), or null
    moveWindow: boolean; // floatWin IS the source window (sole-tab drag), not a copy
  } | null = null;
  const FOLLOW_INTERVAL_MS = 16;

  // End the drag. Three outcomes, by what was being previewed at release:
  //   • tab dock-preview (strip) → dock the group as a tab at the cursor slot;
  //   • pane stitch-preview (body) → hand the single pane to the target, which
  //     inserts it into its active group's layout at the cursor slot;
  //   • neither (free-floating) → settle the float in place.
  // The dragged window is destroyed for dock/stitch; its sessions are re-homed to
  // the target FIRST so its `closed` reap can't kill ptys the target now owns.
  const finalizeDrag = (): { action: 'docked' | 'stitched' | 'detached' | 'none' } => {
    const ds = dragState;
    if (!ds) return { action: 'none' };
    dragState = null;
    clearInterval(ds.timer);
    const floatWin = BrowserWindow.fromId(ds.floatWinId);
    const rehomeTo = (target: BrowserWindow) => {
      for (const p of ds.group.panes) owner.set(p.sessionUid, target.id);
      if (target.isMinimized()) target.restore();
      target.focus();
      if (floatWin && !floatWin.isDestroyed()) floatWin.destroy();
    };
    if (ds.previewWinId != null) {
      const target = BrowserWindow.fromId(ds.previewWinId);
      if (target && !target.isDestroyed()) {
        const pt = screen.getCursorScreenPoint();
        const x = pt.x - target.getContentBounds().x; // window-relative slot x
        target.webContents.send('tab:receive', { group: ds.group, x });
        rehomeTo(target);
      } else if (floatWin && !floatWin.isDestroyed()) floatWin.destroy();
      return { action: 'docked' };
    }
    if (ds.stitchWinId != null) {
      const target = BrowserWindow.fromId(ds.stitchWinId);
      if (target && !target.isDestroyed()) {
        const b = target.getContentBounds();
        const pt = screen.getCursorScreenPoint();
        target.webContents.send('pane:stitch', { group: ds.group, x: pt.x - b.x, y: pt.y - b.y });
        rehomeTo(target);
      } else if (floatWin && !floatWin.isDestroyed()) floatWin.destroy();
      return { action: 'stitched' };
    }
    if (floatWin && !floatWin.isDestroyed()) {
      floatWin.setAlwaysOnTop(false); // it's a normal window now, not a drag proxy
      floatWin.setOpacity(1); // a sole-tab window may have been dimmed mid-preview
      floatWin.show();
      floatWin.focus();
    }
    return { action: 'detached' };
  };

  const send = (channel: string, payload: unknown) => {
    for (const win of BrowserWindow.getAllWindows()) {
      if (!win.isDestroyed()) win.webContents.send(channel, payload);
    }
  };

  // Kill every session a (closing) window still owns. Sessions that moved to
  // another window were re-owned on re-attach, so they're spared here.
  const reap = (windowId: number) => {
    for (const [uid, w] of owner) {
      if (w === windowId) {
        manager.kill(uid);
        owner.delete(uid);
      }
    }
  };

  const spawnWindow = (
    seed?: GroupPayload,
    at?: { x: number; y: number },
    opts?: { inactive?: boolean }
  ): BrowserWindow => {
    const win = buildWindow(at, opts);
    if (primaryWindowId === null) primaryWindowId = win.id;
    if (seed) pendingSeed.set(win.id, seed);
    win.on('closed', () => {
      pendingSeed.delete(win.id);
      reap(win.id);
      // If the session-of-record window went away, hand the role to a survivor
      // so the session keeps autosaving (getAllWindows excludes the closed one).
      if (primaryWindowId === win.id) {
        primaryWindowId = null;
        const next = BrowserWindow.getAllWindows().find((w) => !w.isDestroyed());
        if (next) {
          primaryWindowId = next.id;
          next.webContents.send('window:primary');
        }
      }
    });
    return win;
  };

  ipcMain.handle('session:spawn', (e, opts: SpawnOptions) => {
    // The window asking to spawn/attach now owns this session.
    const win = BrowserWindow.fromWebContents(e.sender);
    if (win) owner.set(opts.uid, win.id);

    // Re-attach: the pty is still alive (the pane/tab was moved, not closed), so
    // keep it running and hand its recent-output buffer back in the reply. We
    // return the replay (rather than send a session:data event) so it's ordered
    // against the renderer's await: any live output that streamed in while the
    // terminal was wiring up is already in this buffer, so the terminal drops
    // that pre-attach output and renders the replay instead — no duplicated seam.
    const existing = manager.get(opts.uid);
    if (existing) {
      return { uid: opts.uid, attached: true, replay: existing.getReplay() };
    }
    manager.create(opts, {
      onData: (uid, data) => send('session:data', { uid, data }),
      onExit: (uid, code) => {
        send('session:exit', { uid, code });
        owner.delete(uid);
      }
    });
    return { uid: opts.uid, attached: false };
  });

  ipcMain.on('session:write', (_e, { uid, data }: { uid: string; data: string }) => {
    manager.write(uid, data);
  });

  ipcMain.on(
    'session:resize',
    (_e, { uid, cols, rows }: { uid: string; cols: number; rows: number }) => {
      manager.resize(uid, cols, rows);
    }
  );

  ipcMain.on('session:kill', (_e, { uid }: { uid: string }) => {
    manager.kill(uid);
    owner.delete(uid);
  });

  // ---- Clickable file paths: verify on disk, then open in editor / OS handler ----
  ipcMain.handle('path:resolve', (_e, { cwd, tokens }: { cwd?: string; tokens: string[] }) =>
    resolvePaths(cwd, tokens)
  );
  ipcMain.handle(
    'path:open',
    (
      _e,
      {
        absPath,
        line,
        col,
        editorCommand
      }: { absPath: string; line?: number; col?: number; editorCommand?: string }
    ) => openResolvedPath(absPath, line, col, editorCommand || '')
  );

  // ---- Window controls (frameless: the top bar draws its own buttons) ----
  ipcMain.on('window:minimize', (e) => BrowserWindow.fromWebContents(e.sender)?.minimize());
  ipcMain.on('window:toggleMaximize', (e) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (!win) return;
    if (win.isMaximized()) win.unmaximize();
    else win.maximize();
  });
  ipcMain.on('window:close', (e) => BrowserWindow.fromWebContents(e.sender)?.close());
  ipcMain.handle(
    'window:isMaximized',
    (e) => BrowserWindow.fromWebContents(e.sender)?.isMaximized() ?? false
  );
  // Pane fullscreen uses *simple* fullscreen: a frameless window's native
  // setFullScreen flickers on Windows (enters, then the OS immediately fires
  // leave-full-screen and it bounces back to windowed). Simple fullscreen is
  // app-driven — no enter/leave-full-screen events — so it stays put. Guarded so
  // a redundant set doesn't thrash the window.
  ipcMain.on('window:setFullScreen', (e, on: boolean) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (win && win.isSimpleFullScreen() !== on) win.setSimpleFullScreen(on);
  });

  // ---- Multi-window: tear-off seed + tab drop (Stage 2) ----
  ipcMain.handle('window:getSeed', (e) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (!win) return { seed: null, primary: false };
    const seed = pendingSeed.get(win.id) ?? null;
    pendingSeed.delete(win.id);
    return { seed, primary: win.id === primaryWindowId };
  });

  // Move to New Window (tab menu): open a fresh window seeded with an already-
  // extracted group, near the cursor. The renderer flagged its sessions "moving",
  // so the new window re-attaches to the live ptys rather than respawning.
  ipcMain.handle('window:spawnGroup', (_e, group: GroupPayload) => {
    const pt = screen.getCursorScreenPoint();
    spawnWindow(group, pt);
    return { ok: true };
  });

  // ---- Live tear-off: a window that follows the cursor and docks like Chrome ----
  //
  // The renderer extracts the tab/pane up front (its live sessions flagged
  // "moving") and calls drag:detach. We open a real, seeded window at the cursor
  // and run a timer (driven from main, so it doesn't depend on the renderer
  // delivering pointermoves outside its own window) that, every tick:
  //   • cursor over another window's tab bar → HIDE the float and show a
  //     dock-preview ghost in that window's strip at the cursor slot;
  //   • cursor over empty space → SHOW the float and move it to the cursor.
  // The source keeps the pointer capture, so its pointerup ends the drag
  // (drag:drop / drag:cancel → finalizeDrag): dock into the previewed strip, or
  // settle the float in place.
  ipcMain.handle('drag:detach', (e, group: GroupPayload, moveWindow?: boolean) => {
    if (dragState) finalizeDrag(); // settle any prior drag first
    const pt = screen.getCursorScreenPoint();
    let float: BrowserWindow;
    if (moveWindow) {
      // The dragged tab/pane is the source window's entire content → drag THIS
      // window instead of spawning a seeded copy (which, with the source reseeding
      // a fresh tab, would leave a duplicate window behind).
      const src = BrowserWindow.fromWebContents(e.sender);
      if (!src) return { id: -1 };
      if (src.isMaximized()) src.unmaximize();
      float = src;
    } else {
      float = spawnWindow(group, pt, { inactive: true });
    }
    // Float above everything (incl. the still-focused source window) so it's
    // visible following the cursor even while over the source. Cleared on drop.
    float.setAlwaysOnTop(true);
    let hidden = false;
    const hideFloat = () => {
      if (hidden) return;
      // Can't hide() the window that holds the pointer capture (a sole-tab move) —
      // dropping the capture loses the release. Dim it instead.
      if (moveWindow) float.setOpacity(0);
      else float.hide();
      hidden = true;
    };
    const showFloat = () => {
      if (!hidden) return;
      if (moveWindow) float.setOpacity(1);
      else float.showInactive();
      hidden = false;
    };
    const sendTo = (id: number | null, channel: string, payload: unknown) => {
      if (id != null) BrowserWindow.fromId(id)?.webContents.send(channel, payload);
    };
    const timer = setInterval(() => {
      const ds = dragState;
      if (!ds || float.isDestroyed()) return;
      const p = screen.getCursorScreenPoint();
      const win = windowAtPoint(p, ds.floatWinId);
      // Over a window's tab bar → dock preview; over its body with a single pane →
      // stitch preview; otherwise the float just follows the cursor.
      let mode: 'strip' | 'stitch' | 'float' = 'float';
      if (win) {
        const b = win.getContentBounds();
        if (p.y < b.y + DOCK_BAND_HEIGHT) mode = 'strip';
        else if (ds.group.panes.length === 1 && !ds.moveWindow) mode = 'stitch';
      }
      if (mode === 'float') {
        if (ds.previewWinId != null) { sendTo(ds.previewWinId, 'tab:preview', null); ds.previewWinId = null; }
        if (ds.stitchWinId != null) { sendTo(ds.stitchWinId, 'pane:preview', null); ds.stitchWinId = null; }
        showFloat();
        const o = originForDrop(p, screen.getDisplayNearestPoint(p).workArea, {
          width: WINDOW_WIDTH,
          height: WINDOW_HEIGHT
        });
        float.setPosition(o.x, o.y);
        return;
      }
      // strip or stitch → tuck the float away and show the target's preview.
      hideFloat();
      const b = win!.getContentBounds();
      if (mode === 'strip') {
        if (ds.stitchWinId != null) { sendTo(ds.stitchWinId, 'pane:preview', null); ds.stitchWinId = null; }
        if (ds.previewWinId !== win!.id) {
          sendTo(ds.previewWinId, 'tab:preview', null);
          ds.previewWinId = win!.id;
        }
        win!.webContents.send('tab:preview', { x: p.x - b.x, title: ds.group.title });
      } else {
        if (ds.previewWinId != null) { sendTo(ds.previewWinId, 'tab:preview', null); ds.previewWinId = null; }
        if (ds.stitchWinId !== win!.id) {
          sendTo(ds.stitchWinId, 'pane:preview', null);
          ds.stitchWinId = win!.id;
        }
        win!.webContents.send('pane:preview', { x: p.x - b.x, y: p.y - b.y });
      }
    }, FOLLOW_INTERVAL_MS);
    dragState = {
      group,
      timer,
      floatWinId: float.id,
      previewWinId: null,
      stitchWinId: null,
      moveWindow: !!moveWindow
    };
    float.on('closed', () => {
      if (dragState?.floatWinId !== float.id) return;
      clearInterval(dragState.timer);
      sendTo(dragState.previewWinId, 'tab:preview', null);
      sendTo(dragState.stitchWinId, 'pane:preview', null);
      dragState = null;
    });
    return { id: float.id };
  });

  ipcMain.handle('drag:drop', () => finalizeDrag());
  ipcMain.handle('drag:cancel', () => finalizeDrag());

  // ---- Workspace persistence (dialogs parent to the calling window) ----
  ipcMain.handle('workspace:getInitial', () => getInitialWorkspace());
  ipcMain.handle('workspace:open', (e) =>
    openWorkspaceDialog(BrowserWindow.fromWebContents(e.sender))
  );
  ipcMain.handle('workspace:save', (e, data: WorkspaceFile) =>
    saveWorkspaceDialog(BrowserWindow.fromWebContents(e.sender), data)
  );
  ipcMain.on('workspace:saveLast', (_e, data: WorkspaceFile) => writeLast(data));

  return { manager, spawnWindow };
}

// First non-minimized window (in BrowserWindow.getAllWindows() / creation order)
// whose CONTENT area contains the point, excluding the dragging float window. The
// caller splits it into the tab-bar band (dock) vs the body (stitch) via
// DOCK_BAND_HEIGHT. Electron exposes no window z-order, so with overlapping windows
// this isn't necessarily the visually frontmost one; an accepted limitation.
// Content bounds and the cursor point are both DIP, so they compare directly.
function windowAtPoint(pt: { x: number; y: number }, excludeId: number): BrowserWindow | null {
  for (const win of BrowserWindow.getAllWindows()) {
    if (win.id === excludeId || win.isDestroyed() || win.isMinimized()) continue;
    const b = win.getContentBounds();
    if (pt.x >= b.x && pt.x < b.x + b.width && pt.y >= b.y && pt.y < b.y + b.height) {
      return win;
    }
  }
  return null;
}
