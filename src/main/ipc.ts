import { app, BrowserWindow, ipcMain, screen } from 'electron';
import { SessionManager } from './session-manager';
import { resolvePaths, openResolvedPath } from './paths';
import type { SpawnOptions } from './session';
import type { GroupPayload, UpdateStatus } from '../renderer/types';
import { originForDrop, WINDOW_WIDTH, WINDOW_HEIGHT, DOCK_BAND_HEIGHT } from './window-geometry';
import {
  getInitialWorkspace,
  openWorkspaceDialog,
  saveWorkspaceDialog,
  writeLast,
  type WorkspaceFile,
  type WindowSpec,
  type WindowBounds,
  type GroupSpec,
  type LaunchRouting
} from './workspace';
import { ControlServer, type ControlWindowPayload, type ControlCommand } from './control-server';
import { collectMetrics } from './metrics';
import { findGitRoot } from './git';
import {
  listProjects,
  upsertProjectByRoot,
  setProjectColor,
  renameProject,
  removeProject
} from './projects';
import { join } from 'node:path';
import { AiService, type AiPanePublish } from './ai/ai-service';

// Per-window spawn options. `windowSpec`/`bounds`/`cascadeIndex` drive launch-time
// windows (M0); `inactive` is the live-drag float.
export interface SpawnWindowOpts {
  inactive?: boolean;
  windowSpec?: WindowSpec; // launch seed: tabs to materialize in this window
  bounds?: WindowBounds; // explicit on-screen bounds (from a workspace file)
  cascadeIndex?: number; // nth boundless launch window → stagger so they don't stack
}

export interface IpcHandle {
  manager: SessionManager;
  /** The loopback control API (M2). Off until enabled; shut down on quit. */
  control: ControlServer;
  /**
   * Open a window, optionally seeded with a torn-off group (Stage 2) or a
   * launch-time window spec (M0), positioned at a drop point `at` (else
   * OS-centered / bounds), and shown without focus (`inactive`).
   */
  spawnWindow: (
    seed?: GroupPayload,
    at?: { x: number; y: number },
    opts?: SpawnWindowOpts
  ) => BrowserWindow;
  /**
   * Place a launch's windows per its routing: open new OS windows, or attach the
   * content into an existing window (as new tabs or merged panes). Used by both
   * the first launch (always new windows) and a second `hyperpanes …` invocation.
   */
  routeLaunch: (windows: WindowSpec[], routing: LaunchRouting) => void;
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
  buildWindow: (
    at?: { x: number; y: number },
    opts?: { inactive?: boolean; bounds?: WindowBounds; cascadeIndex?: number }
  ) => BrowserWindow
): IpcHandle {
  const manager = new SessionManager();

  // In-flight control commands awaiting their renderer reply (D). A monotonic
  // correlationId keys each; the renderer echoes it on control:commandResult.
  // `windowId` is the window we dispatched to, so only that window may answer
  // this correlationId (#10).
  type DispatchResult = { ok: boolean; result?: unknown; timedOut?: boolean; error?: string };
  const pendingCommands = new Map<
    string,
    { resolve: (r: DispatchResult) => void; timer: ReturnType<typeof setTimeout>; windowId: number }
  >();
  let commandSeq = 0;
  const COMMAND_TIMEOUT_MS = 2000;

  // Forward a command to a window and wait for its reply (carrying any result,
  // e.g. a new pane id). Resolves {ok:false} if the window is gone, or
  // {ok:false, timedOut:true} if the renderer never answers within the timeout
  // — so a wedged renderer surfaces as a failure (not a phantom success) and
  // can't hang the HTTP request forever. A genuine reply carries {ok, result}
  // or {ok:false, error} when the store action threw (#2/#3). Shared by the
  // control API and the ambient-AI service (which pushes generated subtitles
  // into pane meta the same way).
  const dispatchToWindow = (
    windowId: number,
    command: ControlCommand
  ): Promise<DispatchResult> => {
    const win = BrowserWindow.fromId(windowId);
    if (!win || win.isDestroyed()) return Promise.resolve({ ok: false });
    const correlationId = String(++commandSeq);
    return new Promise<DispatchResult>((resolve) => {
      const timer = setTimeout(() => {
        pendingCommands.delete(correlationId);
        resolve({ ok: false, timedOut: true });
      }, COMMAND_TIMEOUT_MS);
      pendingCommands.set(correlationId, { resolve, timer, windowId });
      win.webContents.send('control:command', { ...command, correlationId });
    });
  };

  // The control API reads/writes ptys through the manager and forwards structural
  // commands to a window's renderer. `send` (defined below) broadcasts the
  // active-state toggle so renderers know when to publish their structure.
  const control = new ControlServer({
    readOutput: (uid) => manager.get(uid)?.getReplay() ?? null,
    sendInput: (uid, data) => manager.write(uid, data),
    dispatchCommand: dispatchToWindow,
    onActiveChange: (active) => send('control:active', active)
  });

  // The renderer's reply to a dispatched command — resolve the pending promise.
  // Only the window the command was dispatched to may answer this correlationId
  // (#10): a stray reply from another window (guessable sequential id) is ignored.
  ipcMain.on(
    'control:commandResult',
    (
      e,
      {
        correlationId,
        ok,
        result,
        error
      }: { correlationId: string; ok?: boolean; result?: unknown; error?: string }
    ) => {
      const pending = pendingCommands.get(correlationId);
      if (!pending) return; // unknown / already timed out
      const sender = BrowserWindow.fromWebContents(e.sender);
      if (!sender || sender.id !== pending.windowId) return; // not the dispatched window
      clearTimeout(pending.timer);
      pendingCommands.delete(correlationId);
      pending.resolve({ ok: ok !== false, result, ...(error ? { error } : {}) });
    }
  );

  // Which window each live session belongs to (BrowserWindow.id). Updated on
  // every spawn/attach, so a session re-attached by a different window is now
  // owned by that window — and the original window closing won't reap it.
  const owner = new Map<string, number>();
  // A group waiting for its freshly-opened window to pull it via window:getSeed.
  const pendingSeed = new Map<number, GroupPayload>();
  // A launch-time window spec (tabs to materialize) waiting to be pulled the same way.
  const pendingWindowSpec = new Map<number, WindowSpec>();
  // The session-of-record window — the only one that persists last-workspace.json.
  let primaryWindowId: number | null = null;
  // The most-recently-focused window, so an attach launch (`hyperpanes …` from an
  // external terminal, where the app itself isn't focused at that instant) can
  // route into the window the user last worked in, not an arbitrary one.
  let lastFocusedWindowId: number | null = null;
  // The in-flight live tear-off (Chrome-style). `floatWinId` is the real seeded
  // window that follows the cursor; while the cursor is over another window's tab
  // bar that float is HIDDEN and `previewWinId` shows a dock-preview ghost in that
  // window's strip. `group` is kept so a drop can re-home the live sessions.
  let dragState: {
    group: GroupPayload;
    timer: ReturnType<typeof setInterval>;
    floatWinId: number;
    previewWinId: number | null; // window showing a tab dock-preview (strip), or null
    stitchWinId: number | null; // window whose body is under the cursor (single-pane float), or null
    stitchValid: boolean; // the target renderer reports the cursor is near a pane edge (a real slot)
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
    // Only stitch when the cursor was near a pane EDGE at release (the renderer
    // reported a real slot). Over a pane's dead centre the float stayed visible,
    // so it settles as its own window instead of stitching.
    if (ds.stitchWinId != null && ds.stitchValid) {
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
    // Settled as a free-floating window — clear any preview left on the last
    // hovered target (a dock ghost or a stitch dropline).
    if (ds.previewWinId != null) BrowserWindow.fromId(ds.previewWinId)?.webContents.send('tab:preview', null);
    if (ds.stitchWinId != null) BrowserWindow.fromId(ds.stitchWinId)?.webContents.send('pane:preview', null);
    return { action: 'detached' };
  };

  const send = (channel: string, payload: unknown) => {
    for (const win of BrowserWindow.getAllWindows()) {
      if (!win.isDestroyed()) win.webContents.send(channel, payload);
    }
  };

  // Ambient AI (local Gemma via Ollama): taps pane output, summarizes it, and
  // writes a high-level "what you're doing" line into pane meta['ai.subtitle'].
  // Independent of the control server (its own enable + own settings file).
  const aiPushMeta = (windowId: number, paneId: string, meta: Record<string, string>) => {
    const win = BrowserWindow.fromId(windowId);
    if (win && !win.isDestroyed()) {
      void dispatchToWindow(windowId, { type: 'setMeta', paneId, meta });
    } else {
      // Unknown/stale window: broadcast; applyControlCommand no-ops where the pane
      // is absent, so only the owning window acts on it.
      send('control:command', { type: 'setMeta', paneId, meta });
    }
  };
  const ai = new AiService({
    settingsPath: join(app.getPath('userData'), 'ai-settings.json'),
    memoryPath: join(app.getPath('userData'), 'ai-memory.json'),
    pushMeta: aiPushMeta,
    onStatus: (status) => send('ai:status', status)
  });

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

  // ---- Multi-window session persistence ----
  // Each window publishes its tabs (workspace:windowSession); we key them by
  // window id and write the combined `windows[]` last session, so a relaunch
  // restores every window. `quitting` flips in before-quit so windows cascading
  // closed during a quit don't each rewrite a shrinking session.
  const sessionSpecs = new Map<number, { active: number; groups: GroupSpec[] }>();
  let quitting = false;
  let writeTimer: ReturnType<typeof setTimeout> | null = null;

  const windowBounds = (win: BrowserWindow): WindowBounds => {
    const b = win.getNormalBounds(); // restored bounds, even while maximized
    return { x: b.x, y: b.y, width: b.width, height: b.height, maximized: win.isMaximized() };
  };

  // Write the live windows (in creation/z order) to last-workspace.json. Skips an
  // empty result so closing the final window never wipes the saved session.
  const writeSession = () => {
    const windows: WindowSpec[] = [];
    for (const win of BrowserWindow.getAllWindows()) {
      if (win.isDestroyed()) continue;
      const spec = sessionSpecs.get(win.id);
      if (!spec) continue;
      windows.push({ active: spec.active, groups: spec.groups, bounds: windowBounds(win) });
    }
    if (windows.length === 0) return;
    writeLast({ windows });
  };

  const scheduleWrite = () => {
    if (writeTimer) clearTimeout(writeTimer);
    writeTimer = setTimeout(() => {
      writeTimer = null;
      writeSession();
    }, 400);
  };

  // Capture the full multi-window session before windows start closing on quit.
  app.on('before-quit', () => {
    quitting = true;
    if (writeTimer) {
      clearTimeout(writeTimer);
      writeTimer = null;
    }
    writeSession();
    ai.shutdown(); // flush ai-memory.json
  });

  const spawnWindow = (
    seed?: GroupPayload,
    at?: { x: number; y: number },
    opts?: SpawnWindowOpts
  ): BrowserWindow => {
    const win = buildWindow(at, {
      inactive: opts?.inactive,
      bounds: opts?.bounds,
      cascadeIndex: opts?.cascadeIndex
    });
    if (primaryWindowId === null) primaryWindowId = win.id;
    lastFocusedWindowId = win.id; // a fresh window is the natural attach target
    win.on('focus', () => {
      lastFocusedWindowId = win.id;
    });
    if (seed) pendingSeed.set(win.id, seed);
    if (opts?.windowSpec) pendingWindowSpec.set(win.id, opts.windowSpec);
    win.on('closed', () => {
      pendingSeed.delete(win.id);
      pendingWindowSpec.delete(win.id);
      control.dropWindow(win.id);
      ai.dropWindow(win.id);
      sessionSpecs.delete(win.id);
      reap(win.id);
      // If the session-of-record window went away, hand the role to a survivor.
      // (Persistence is now per-window; primary only gates a seedless window's
      // getInitial fallback — see App's getSeed handler.)
      if (primaryWindowId === win.id) {
        primaryWindowId = null;
        const next = BrowserWindow.getAllWindows().find((w) => !w.isDestroyed());
        if (next) {
          primaryWindowId = next.id;
          next.webContents.send('window:primary');
        }
      }
      // Closing one window (the app staying up) drops it from the saved session;
      // during a quit we leave the before-quit snapshot intact.
      if (!quitting) writeSession();
    });
    return win;
  };

  // Open one new OS window per spec. Bounds win over a cascade stagger so
  // file-described layouts land where asked and ad-hoc launches don't stack.
  const openWindowsNew = (windows: WindowSpec[]) => {
    windows.forEach((w, i) => {
      const opts: SpawnWindowOpts = { windowSpec: w, cascadeIndex: i };
      if (w.bounds) opts.bounds = w.bounds;
      spawnWindow(undefined, undefined, opts);
    });
  };

  // Resolve the existing window an attach launch should target. A numeric target
  // is a specific BrowserWindow id; `focused`/`last` use the last-focused window,
  // falling back to the OS-focused window, then any live window.
  const resolveTargetWindow = (target: 'focused' | 'last' | number): BrowserWindow | null => {
    if (typeof target === 'number') {
      const w = BrowserWindow.fromId(target);
      return w && !w.isDestroyed() ? w : null;
    }
    const tracked = lastFocusedWindowId != null ? BrowserWindow.fromId(lastFocusedWindowId) : null;
    if (tracked && !tracked.isDestroyed()) return tracked;
    const focused = BrowserWindow.getFocusedWindow();
    if (focused && !focused.isDestroyed()) return focused;
    return BrowserWindow.getAllWindows().find((w) => !w.isDestroyed()) ?? null;
  };

  // Merge a launched window's tabs into an existing window: surface it, then
  // dispatch a single `attach` command its renderer enacts (new tabs, or panes
  // merged into the active tab). Reuses the control-command pipeline (always
  // wired in the renderer), so it works whether or not the control server is on.
  const attachInto = (win: BrowserWindow, spec: WindowSpec, as: 'tab' | 'panes') => {
    if (win.isMinimized()) win.restore();
    win.focus();
    void dispatchToWindow(win.id, { type: 'attach', groups: spec.groups, as });
  };

  // Place a launch per its routing (see LaunchRouting). Attach merges the FIRST
  // window's tabs into the target window; any further windows from a multi-window
  // launch can't share one target, so they open as new windows. With no window to
  // attach to (e.g. all closed), attach degrades to opening new windows.
  const routeLaunch = (windows: WindowSpec[], routing: LaunchRouting) => {
    if (windows.length === 0) return;
    if (routing.mode === 'new-window') {
      openWindowsNew(windows);
      return;
    }
    const target = resolveTargetWindow(routing.target);
    if (!target) {
      openWindowsNew(windows);
      return;
    }
    const [first, ...rest] = windows;
    attachInto(target, first, routing.as);
    if (rest.length) openWindowsNew(rest);
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
    //
    // `opts.env` is intentionally NOT applied here: a live pty's environment is
    // fixed at spawn and can't be changed in place, so a moved pane keeps the
    // scoped token (agent-orchestration F) it was spawned with. Refreshing a
    // moved worker's token would require respawning the shell (losing its state);
    // rotation-on-move is deferred to the Phase B scope design, not patched here (#4).
    const existing = manager.get(opts.uid);
    if (existing) {
      return { uid: opts.uid, attached: true, replay: existing.getReplay() };
    }
    manager.create(opts, {
      onData: (uid, data) => {
        send('session:data', { uid, data });
        control.emitOutput(uid, data); // tee to the /events stream (no-op if no clients)
        ai.onData(uid, data); // tee to the ambient-AI tail (no-op if disabled)
      },
      // Live cwd (OSC 7 shell integration): forward it, and if it's inside a git
      // repo, remember the project and tell the renderer to tint the pane.
      onCwd: (uid, cwd) => {
        send('session:cwd', { uid, cwd });
        const root = findGitRoot(cwd);
        if (root) {
          const project = upsertProjectByRoot(root);
          send('session:project', { uid, project });
          send('projects:changed', listProjects());
          ai.onCwd(uid, cwd, { path: project.path, name: project.name });
        } else {
          ai.onCwd(uid, cwd, null);
        }
      },
      onExit: (uid, code) => {
        send('session:exit', { uid, code });
        owner.delete(uid);
        control.emitExit(uid, code);
        ai.onSessionExit(uid);
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

  // ---- Git projects (sidebar projects history) ----
  ipcMain.handle('projects:list', () => listProjects());
  ipcMain.handle('projects:setColor', (_e, { id, color }: { id: string; color: string }) => {
    setProjectColor(id, color);
    const list = listProjects();
    send('projects:changed', list);
    return list;
  });
  ipcMain.handle('projects:rename', (_e, { id, name }: { id: string; name: string }) => {
    renameProject(id, name);
    const list = listProjects();
    send('projects:changed', list);
    return list;
  });
  ipcMain.handle('projects:remove', (_e, { id }: { id: string }) => {
    removeProject(id);
    const list = listProjects();
    send('projects:changed', list);
    return list;
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
    if (!win) return { seed: null, windowSpec: null, primary: false };
    const seed = pendingSeed.get(win.id) ?? null;
    const windowSpec = pendingWindowSpec.get(win.id) ?? null;
    pendingSeed.delete(win.id);
    pendingWindowSpec.delete(win.id);
    return { seed, windowSpec, primary: win.id === primaryWindowId };
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
      // a CANDIDATE stitch (only confirmed near a pane edge, via the renderer's
      // drag:stitchHit report — over a dead centre the float keeps following);
      // otherwise the float just follows the cursor. A sole-pane window-move
      // (moveWindow) is stitch-eligible too: hideFloat dims it via setOpacity(0) to
      // keep the capture, and finalizeDrag's stitch branch closes the source window
      // after re-homing the pane — the pane equivalent of docking a window's last
      // tab into another window (T12). Only a single-pane group can stitch; a
      // multi-pane group can only dock as a tab (strip).
      let mode: 'strip' | 'stitch' | 'float' = 'float';
      if (win) {
        const b = win.getContentBounds();
        if (p.y < b.y + DOCK_BAND_HEIGHT) mode = 'strip';
        else if (ds.group.panes.length === 1) mode = 'stitch';
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
      // strip → tuck the float away and show the dock ghost in the target strip.
      if (mode === 'strip') {
        if (ds.stitchWinId != null) { sendTo(ds.stitchWinId, 'pane:preview', null); ds.stitchWinId = null; ds.stitchValid = false; }
        if (ds.previewWinId !== win!.id) {
          sendTo(ds.previewWinId, 'tab:preview', null);
          ds.previewWinId = win!.id;
        }
        hideFloat();
        const b = win!.getContentBounds();
        win!.webContents.send('tab:preview', { x: p.x - b.x, title: ds.group.title });
        return;
      }
      // stitch (single pane over a body): feed the cursor to the target so it can
      // compute the slot and report back (drag:stitchHit) whether we're near a
      // pane EDGE. Only then tuck the float away to reveal the dropline — over a
      // pane's dead centre the float stays visible (detached) and keeps following.
      if (ds.previewWinId != null) { sendTo(ds.previewWinId, 'tab:preview', null); ds.previewWinId = null; }
      if (ds.stitchWinId !== win!.id) {
        sendTo(ds.stitchWinId, 'pane:preview', null);
        ds.stitchWinId = win!.id;
        ds.stitchValid = false; // re-evaluated from the new target's next report
      }
      const sb = win!.getContentBounds();
      win!.webContents.send('pane:preview', { x: p.x - sb.x, y: p.y - sb.y });
      if (ds.stitchValid) {
        hideFloat();
      } else {
        showFloat();
        const o = originForDrop(p, screen.getDisplayNearestPoint(p).workArea, {
          width: WINDOW_WIDTH,
          height: WINDOW_HEIGHT
        });
        float.setPosition(o.x, o.y);
      }
    }, FOLLOW_INTERVAL_MS);
    dragState = {
      group,
      timer,
      floatWinId: float.id,
      previewWinId: null,
      stitchWinId: null,
      stitchValid: false,
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

  // A stitch target reports, per follow tick, whether the cursor is near a pane
  // EDGE (a real slot). Gates whether the float hides to reveal the dropline and
  // whether a release commits a stitch — over a pane's dead centre it stays a
  // detached float. One-way (no await) to keep the per-tick feedback cheap.
  ipcMain.on('drag:stitchHit', (_e, valid: boolean) => {
    if (dragState) dragState.stitchValid = !!valid;
  });

  // ---- Workspace persistence (dialogs parent to the calling window) ----
  ipcMain.handle('workspace:getInitial', () => getInitialWorkspace());
  ipcMain.handle('workspace:open', (e) =>
    openWorkspaceDialog(BrowserWindow.fromWebContents(e.sender))
  );
  ipcMain.handle('workspace:save', (e, data: WorkspaceFile) =>
    saveWorkspaceDialog(BrowserWindow.fromWebContents(e.sender), data)
  );
  ipcMain.on(
    'workspace:windowSession',
    (e, payload: { active: number; groups: GroupSpec[] }) => {
      const win = BrowserWindow.fromWebContents(e.sender);
      if (!win) return;
      sessionSpecs.set(win.id, payload);
      scheduleWrite();
    }
  );

  // ---- Control API (M2): renderers publish their structure; Preferences toggles ----
  ipcMain.on('control:publishState', (e, payload: ControlWindowPayload) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (win) control.setWindowState(win.id, payload);
  });
  ipcMain.handle('control:getStatus', () => control.status());
  ipcMain.handle('control:setEnabled', (_e, enabled: boolean) => control.setEnabled(enabled));
  ipcMain.handle('control:setAllowInput', (_e, allow: boolean) => control.setAllowInput(allow));
  // Start listening now if it was left enabled in a prior session (default: off).
  control.init();

  // ---- Ambient AI (local Gemma): status, toggle, config, per-window pane publish ----
  ipcMain.handle('ai:getStatus', () => ai.status());
  ipcMain.handle('ai:setEnabled', (_e, enabled: boolean) => {
    ai.setEnabled(enabled);
    return ai.status();
  });
  ipcMain.handle(
    'ai:configure',
    (
      _e,
      patch: Partial<{
        endpoint: string;
        model: string;
        settleMs: number;
        maxStalenessSec: number;
        concurrency: number;
      }>
    ) => {
      ai.configure(patch);
      return ai.status();
    }
  );
  // Each window publishes only its own panes; main stamps the window id from the sender.
  ipcMain.on('ai:publishPanes', (e, panes: AiPanePublish[]) => {
    const win = BrowserWindow.fromWebContents(e.sender);
    if (win) ai.onPaneContext(win.id, panes);
  });
  // Load persisted AI settings + memory; start watching if left enabled (default: off).
  ai.init();

  // ---- Update-in-place (electron-updater) — CONTRACT STUB ----
  // Base-branch placeholder so the renderer (toast + Preferences) compiles and
  // runs with update reported as unsupported/no-op. The `updater` fan-out track
  // REPLACES this whole block with the real UpdaterService wiring (events
  // broadcast on 'update:status' via `send`, autoCheck persisted in userData).
  const updateStub = (): UpdateStatus => ({
    stage: 'idle',
    currentVersion: app.getVersion(),
    supported: false,
    autoCheck: false
  });
  ipcMain.handle('update:getStatus', () => updateStub());
  ipcMain.handle('update:check', () => updateStub());
  ipcMain.handle('update:setAutoCheck', (_e, _on: boolean) => updateStub());
  ipcMain.on('update:quitAndInstall', () => {
    /* no-op until the updater track lands */
  });

  // ---- Diagnostics: memory/process/startup snapshot (Performance: Dump metrics) ----
  ipcMain.handle('metrics:get', () => collectMetrics());

  return { manager, control, spawnWindow, routeLaunch };
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
