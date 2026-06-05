import { memo, useEffect, useRef, useState } from 'react';
import { Terminal as XTerm } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { SearchAddon } from '@xterm/addon-search';
import { WebglAddon } from '@xterm/addon-webgl';
import '@xterm/xterm/css/xterm.css';
import { SearchBox } from './SearchBox';
import { movingSessions, paneFontOverride, useWorkspace } from '../store/useWorkspace';
import { useKeybindings } from '../store/useKeybindings';
import { useSettings } from '../store/useSettings';
import { useIdle } from '../store/useIdle';
import { comboMatches } from '../keybindings';
import { DEFAULT_FONT_FAMILY, TERMINAL_THEMES } from './terminal-themes';
import { cellFromIndex, extractPathCandidates } from './pathLinks';
import { incWebglContexts, decWebglContexts } from '../perf';
import { serializeTerminal } from '../screen';
import { paneScreens } from '../paneScreens';

// An imperative handle onto a mounted pane terminal. The right-click menus live
// outside the xterm host (on the pane header / taskbar), so they reach a pane's
// terminal ops — copy/paste/select-all/clear and open-search — through here.
export interface PaneTerminalHandle {
  term: XTerm;
  openSearch: () => void;
}

// Registered per pane after open(), removed on unmount. A pane moved between tabs
// or windows re-mounts its Terminal, so it re-registers under the same paneId.
export const paneTerminals = new Map<string, PaneTerminalHandle>();

interface TerminalProps {
  paneId: string;
  sessionUid: string;
  command?: string;
  cwd?: string;
  shell?: string; // pty shell override; falls back to the default-shell setting
  env?: Record<string, string>; // extra pty env (e.g. a scoped control token, agent-orchestration F)
  focused?: boolean;
  // True only while this pane is actually painting on screen (active tab AND a
  // shown tile). Gates the GPU renderer: a hidden pane drops its WebGL context so
  // background tabs don't each pin one (Chromium caps them ~16). The pty keeps
  // running regardless — only the renderer is detached.
  visible: boolean;
  onExit?: (code: number) => void;
  onTitle?: (title: string) => void;
  // Routes keyboard input. When provided, replaces the default self-write so the
  // owner controls how input reaches the pty.
  onInput?: (data: string) => void;
}

function TerminalImpl({ paneId, sessionUid, command, cwd, shell, env, focused, visible, onExit, onTitle, onInput }: TerminalProps) {
  const hostRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const searchRef = useRef<SearchAddon | null>(null);

  // Latest visibility, read inside the mount effect (which doesn't depend on
  // `visible`, so a pane move re-attaches WebGL based on the current value).
  const visibleRef = useRef(visible);
  visibleRef.current = visible;
  // Imperative toggle onto this pane's WebGL renderer, installed by the mount
  // effect once the terminal is open and re-installed when the pane re-mounts.
  const webglCtlRef = useRef<((on: boolean) => void) | null>(null);
  const [searchOpen, setSearchOpen] = useState(false);
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<ReturnType<typeof setTimeout>>();
  // Hover tooltip for a clickable file path (fixed-positioned at the cursor).
  const [pathTip, setPathTip] = useState<{ x: number; y: number; text: string } | null>(null);

  // Live focus state, read inside the mount effect's link-click handler so a
  // click that merely focuses the pane doesn't also open a file under it.
  const focusedRef = useRef(focused);
  focusedRef.current = focused;

  // This pane's font size: its own zoom override, else the default-size setting
  // (so changing the default reflows un-zoomed panes live).
  const fontOverride = useWorkspace((s) => paneFontOverride(s, paneId));
  const defaultFontSize = useSettings((s) => s.defaultFontSize);
  const fontSize = fontOverride ?? defaultFontSize;
  const fontSizeRef = useRef(fontSize);
  fontSizeRef.current = fontSize;

  // Appearance: terminal color theme + font family, applied live (see effects).
  const terminalTheme = useSettings((s) => s.terminalTheme);
  const fontFamily = useSettings((s) => s.fontFamily) || DEFAULT_FONT_FAMILY;

  const onExitRef = useRef(onExit);
  onExitRef.current = onExit;
  const onTitleRef = useRef(onTitle);
  onTitleRef.current = onTitle;
  const onInputRef = useRef(onInput);
  onInputRef.current = onInput;

  // A transient notice anchored inside this pane (copy confirmation, zoom %).
  const flashToast = (msg: string) => {
    setToast(msg);
    clearTimeout(toastTimer.current);
    toastTimer.current = setTimeout(() => setToast(null), 1600);
  };

  useEffect(() => {
    const host = hostRef.current!;
    const term = new XTerm({
      fontFamily: useSettings.getState().fontFamily || DEFAULT_FONT_FAMILY,
      fontSize: fontSizeRef.current,
      cursorBlink: true,
      allowProposedApi: true,
      theme: TERMINAL_THEMES[useSettings.getState().terminalTheme]
    });
    termRef.current = term;

    const fit = new FitAddon();
    fitRef.current = fit;
    term.loadAddon(fit);
    const search = new SearchAddon();
    term.loadAddon(search);
    searchRef.current = search;
    term.open(host);

    // Expose this terminal to the context menus (copy/paste/clear/search).
    paneTerminals.set(paneId, { term, openSearch: () => setSearchOpen(true) });
    // Expose a clean-text serializer of this pane's buffer to the control bridge
    // (read_pane mode:"screen"). The buffer stays faithful even on a hidden tab
    // (output is written to term regardless of visibility), so screen reads work
    // off-screen too. Unregistered on unmount below.
    paneScreens.set(paneId, () => serializeTerminal(term));

    // GPU rendering, attached ONLY while the pane is on screen. Each WebGL renderer
    // is a real GPU context, and Chromium force-loses the oldest once a process
    // holds more than ~16 — so without this gate every background tab's panes would
    // pin a context and starve the visible ones onto the DOM renderer. A hidden
    // pane drops its context (its pty keeps running); it re-attaches when shown.
    let webgl: WebglAddon | null = null;
    let webglUnavailable = false;
    const dropWebgl = () => {
      if (!webgl) return;
      try {
        webgl.dispose();
      } catch {
        /* already disposed (e.g. via term.dispose) */
      }
      webgl = null;
      decWebglContexts();
    };
    const addWebgl = () => {
      if (webgl || webglUnavailable) return;
      try {
        const addon = new WebglAddon();
        addon.onContextLoss(() => dropWebgl()); // GPU dropped it → fall back to DOM
        term.loadAddon(addon);
        webgl = addon;
        incWebglContexts();
      } catch {
        webglUnavailable = true; // WebGL2 unavailable — DOM renderer is used
      }
    };
    webglCtlRef.current = (on: boolean) => (on ? addWebgl() : dropWebgl());
    // Attach now if this pane is mounting on screen (e.g. the active tab); panes
    // that mount hidden stay on the DOM renderer until the visibility effect shows
    // them. term.open(host) above is a prerequisite for loadAddon(WebGL).
    if (visibleRef.current) addWebgl();

    // The search binding (default Ctrl/Cmd+F) opens this pane's search box and
    // isn't forwarded to the shell. Read live so user rebinds take effect.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type === 'keydown' && comboMatches(useKeybindings.getState().combos['pane.search'], e)) {
        setSearchOpen(true);
        return false;
      }
      return true;
    });

    // Ctrl + mouse wheel zooms this pane's font instead of scrolling. Capture
    // phase + preventDefault stops xterm's own wheel-scroll while zooming.
    const onWheel = (e: WheelEvent) => {
      if (!e.ctrlKey && !e.metaKey) return;
      e.preventDefault();
      if (e.deltaY < 0) useWorkspace.getState().zoomPane(paneId, 1);
      else if (e.deltaY > 0) useWorkspace.getState().zoomPane(paneId, -1);
    };
    host.addEventListener('wheel', onWheel, { capture: true, passive: false });

    // Copy-on-select: when a mouse selection settles, copy it and flash a toast.
    const onMouseUp = () => {
      const sel = term.getSelection();
      if (!sel) return;
      navigator.clipboard
        .writeText(sel)
        .then(() => flashToast(`Copied ${sel.length} char${sel.length === 1 ? '' : 's'} to clipboard`))
        .catch(() => {
          /* clipboard unavailable */
        });
    };
    host.addEventListener('mouseup', onMouseUp);

    // Right-click pastes the clipboard into the shell (routed via onData).
    // Suppresses the native context menu.
    const onContextMenu = (e: MouseEvent) => {
      e.preventDefault();
      navigator.clipboard
        .readText()
        .then((text) => {
          if (text) term.paste(text);
        })
        .catch(() => {
          /* clipboard unavailable */
        });
    };
    host.addEventListener('contextmenu', onContextMenu);

    // ---- Clickable file paths --------------------------------------------------
    // Plain click opens the file (editor / OS default); Ctrl+click copies the
    // resolved absolute path. Paths are verified on disk (against this pane's
    // cwd) before they linkify, so prose tokens don't light up.

    // The focusing click on an unfocused pane only focuses — it must not also
    // open a file. Capture-phase mousedown snapshots focus BEFORE focusPane runs.
    const wasFocusedAtDown = { value: false };
    const onPathMouseDownCapture = () => {
      wasFocusedAtDown.value = focusedRef.current ?? false;
    };
    host.addEventListener('mousedown', onPathMouseDownCapture, true);

    // Verified paths cached for this pane's lifetime (keyed by cwd + token), so
    // repeated hovers over the same line don't re-hit main. Negatives aren't
    // cached, so a file Claude creates becomes clickable on the next hover.
    const verified = new Map<string, { absPath: string; isDir: boolean; isExe: boolean }>();
    const keyOf = (token: string) => `${cwd ?? ''} ${token}`;

    // Join a buffer row with its wrapped continuation rows into one logical line.
    // translateToString(false) keeps each row padded to `cols`, so a string index
    // maps cleanly to a cell (exact for ASCII paths; see cellFromIndex).
    const getLogicalLine = (lineNumber: number) => {
      const buf = term.buffer.active;
      const cols = term.cols;
      let startRow = lineNumber - 1;
      if (startRow < 0) return null;
      while (startRow > 0 && buf.getLine(startRow)?.isWrapped) startRow--;
      let text = '';
      let row = startRow;
      for (;;) {
        const ln = buf.getLine(row);
        if (!ln) break;
        text += ln.translateToString(false);
        const next = buf.getLine(row + 1);
        if (next?.isWrapped) row++;
        else break;
      }
      return { text, startRow, cols };
    };

    const linkDisposable = term.registerLinkProvider({
      provideLinks(lineNumber, callback) {
        if (!useSettings.getState().clickablePaths) return callback(undefined);
        const logical = getLogicalLine(lineNumber);
        if (!logical) return callback(undefined);
        const cands = extractPathCandidates(logical.text);
        if (!cands.length) return callback(undefined);

        const build = () => {
          const links = cands
            .map((c) => {
              const hit = verified.get(keyOf(c.path));
              if (!hit) return null;
              const label =
                hit.absPath +
                (c.line != null ? `:${c.line}${c.col != null ? `:${c.col}` : ''}` : '');
              return {
                range: {
                  start: cellFromIndex(c.start, logical.startRow, logical.cols),
                  end: cellFromIndex(c.end - 1, logical.startRow, logical.cols)
                },
                text: hit.absPath,
                // Pointer cursor + hover tooltip are affordance enough; the solid
                // underline reads as too heavy in path-dense output.
                decorations: { pointerCursor: true, underline: false },
                activate: (event: MouseEvent) => {
                  setPathTip(null);
                  if (event.ctrlKey || event.metaKey) {
                    navigator.clipboard
                      .writeText(hit.absPath)
                      .then(() => flashToast(`Copied ${hit.absPath}`))
                      .catch(() => {
                        /* clipboard unavailable */
                      });
                    return;
                  }
                  if (!wasFocusedAtDown.value) return; // this click only focused the pane
                  void window.hp.paths
                    .open(hit.absPath, c.line, c.col, useSettings.getState().editorCommand)
                    .then((res) => {
                      if (!res.ok) {
                        flashToast(
                          res.blocked
                            ? `Won't auto-open ${res.error} — Ctrl+click to copy`
                            : `Couldn't open file`
                        );
                      }
                    });
                },
                hover: (event: MouseEvent) =>
                  setPathTip({ x: event.clientX, y: event.clientY, text: label }),
                leave: () => setPathTip(null)
              };
            })
            .filter((l): l is NonNullable<typeof l> => l !== null);
          callback(links.length ? links : undefined);
        };

        const need = cands.map((c) => c.path).filter((p) => !verified.has(keyOf(p)));
        if (!need.length) return build();
        void window.hp.paths
          .resolve(cwd, need)
          .then((results) => {
            for (const r of results) {
              if (r.exists) verified.set(keyOf(r.token), { absPath: r.absPath, isDir: r.isDir, isExe: r.isExe });
            }
            build();
          })
          .catch(() => callback(undefined));
      }
    });

    const fitIfVisible = () => {
      if (host.clientWidth === 0 || host.clientHeight === 0) return false;
      try {
        fit.fit();
      } catch {
        /* xterm not ready */
      }
      return true;
    };

    // Buffer incoming output until spawn/attach resolves. On a re-attach the
    // replay (returned by spawn) already contains everything emitted up to the
    // moment main handled the call — including output that streamed in live while
    // this terminal was wiring up — so we drop the buffer to avoid a duplicated
    // seam. On a fresh spawn there's no replay, so we flush the buffer instead.
    let replayed = false;
    let disposed = false;
    const pending: string[] = [];
    const offData = window.hp.onData((uid, data) => {
      if (uid !== sessionUid) return;
      // Any output = this pane is active; resets its idle-glow timer.
      useIdle.getState().markActivity(paneId);
      if (replayed) term.write(data);
      else pending.push(data);
    });
    const offExit = window.hp.onExit((uid, code) => {
      if (uid !== sessionUid) return;
      // A finished process isn't "idle waiting" — stop tracking it.
      useIdle.getState().forget(paneId);
      term.write(`\r\n\x1b[90m[process exited: ${code}]\x1b[0m\r\n`);
      onExitRef.current?.(code);
    });

    fitIfVisible();
    // Per-pane shell wins; otherwise the saved default-shell setting; otherwise
    // undefined, which lets main fall back to the system shell (COMSPEC/$SHELL).
    // Read non-reactively so changing the default doesn't respawn live panes —
    // it takes effect on the next spawn (new pane / restart).
    const effectiveShell = shell || useSettings.getState().defaultShell || undefined;
    const releaseBuffer = (replay?: string) => {
      if (disposed) return;
      if (replay) term.write(replay); // re-attach: replay supersedes the buffer
      else for (const d of pending) term.write(d); // fresh spawn: buffer is the output
      pending.length = 0;
      replayed = true;
    };
    void window.hp
      .spawn({
        uid: sessionUid,
        paneId, // injected into the pty env as HYPERPANES_PANE_ID (pane self-awareness)
        shell: effectiveShell,
        command,
        cwd,
        env, // extra env (e.g. a scoped control token) injected into the pty (F)
        cols: term.cols,
        rows: term.rows
      })
      .then((res) => releaseBuffer(res.attached ? res.replay : undefined))
      .catch(() => releaseBuffer()); // spawn failed — flush whatever arrived

    const inputDisposable = term.onData((data) => {
      if (onInputRef.current) onInputRef.current(data);
      else window.hp.write(sessionUid, data);
    });
    const titleDisposable = term.onTitleChange((t) => onTitleRef.current?.(t));

    const ro = new ResizeObserver(() => {
      if (fitIfVisible()) window.hp.resize(sessionUid, term.cols, term.rows);
    });
    ro.observe(host);

    return () => {
      disposed = true;
      paneTerminals.delete(paneId);
      paneScreens.delete(paneId);
      useIdle.getState().forget(paneId);
      ro.disconnect();
      host.removeEventListener('wheel', onWheel, { capture: true } as EventListenerOptions);
      host.removeEventListener('mouseup', onMouseUp);
      host.removeEventListener('contextmenu', onContextMenu);
      host.removeEventListener('mousedown', onPathMouseDownCapture, true);
      linkDisposable.dispose();
      clearTimeout(toastTimer.current);
      inputDisposable.dispose();
      titleDisposable.dispose();
      offData();
      offExit();
      // A pane being moved between tabs keeps its pty alive (the new mount
      // re-attaches); only a genuine close/unmount kills it.
      if (movingSessions.has(sessionUid)) movingSessions.delete(sessionUid);
      else window.hp.kill(sessionUid);
      webglCtlRef.current = null;
      dropWebgl(); // keep the context counter honest (term.dispose would also drop it)
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
      searchRef.current = null;
    };
    // Re-spawn the pty only when an identity/spawn field changes. `env` and
    // `paneId` are intentionally omitted (#4): a live pty's env is fixed at spawn
    // and can't change in place, and a pane MOVE is a full remount (props are read
    // fresh), so there's no stale-capture to chase — adding them would only force
    // a needless teardown+respawn (killing the shell) on an env change we can't
    // actually apply. A moved pane therefore keeps its spawn-time scoped token.
  }, [sessionUid, command, cwd, shell]);

  useEffect(() => {
    if (focused) termRef.current?.focus();
  }, [focused]);

  // Attach/detach the GPU renderer as the pane enters or leaves the screen. The
  // controller is reinstalled by the mount effect on a pane move, and the effect
  // re-runs whenever `visible` flips (tab switch, layout change, solo) — so a
  // freshly shown pane gets WebGL and a hidden one releases its context.
  useEffect(() => {
    webglCtlRef.current?.(visible);
  }, [visible]);

  // Apply zoom: update font size, refit, resync the pty, and flash the zoom %.
  // The terminal is created with the right size, so only react to real changes
  // (comparing the last applied size is StrictMode/restart-safe).
  const lastFont = useRef(fontSize);
  useEffect(() => {
    if (fontSize === lastFont.current) return;
    lastFont.current = fontSize;
    const term = termRef.current;
    const fit = fitRef.current;
    const host = hostRef.current;
    if (!term || !fit || !host) return;
    term.options.fontSize = fontSize;
    if (host.clientWidth > 0 && host.clientHeight > 0) {
      try {
        fit.fit();
      } catch {
        /* xterm not ready */
      }
      window.hp.resize(sessionUid, term.cols, term.rows);
    }
    flashToast(`Zoom ${Math.round((fontSize / defaultFontSize) * 100)}%`);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fontSize]);

  // Live-apply the terminal color theme (colors only — no refit needed).
  const lastTheme = useRef(terminalTheme);
  useEffect(() => {
    if (terminalTheme === lastTheme.current) return;
    lastTheme.current = terminalTheme;
    const term = termRef.current;
    if (term) term.options.theme = TERMINAL_THEMES[terminalTheme];
  }, [terminalTheme]);

  // Live-apply the terminal font family. Font metrics change, so refit + resync
  // the pty to the new cell grid.
  const lastFamily = useRef(fontFamily);
  useEffect(() => {
    if (fontFamily === lastFamily.current) return;
    lastFamily.current = fontFamily;
    const term = termRef.current;
    const fit = fitRef.current;
    const host = hostRef.current;
    if (!term) return;
    term.options.fontFamily = fontFamily;
    if (fit && host && host.clientWidth > 0 && host.clientHeight > 0) {
      try {
        fit.fit();
      } catch {
        /* xterm not ready */
      }
      window.hp.resize(sessionUid, term.cols, term.rows);
    }
  }, [fontFamily, sessionUid]);

  const closeSearch = () => {
    setSearchOpen(false);
    termRef.current?.focus();
  };

  return (
    <div className="hp-term-wrap">
      <div ref={hostRef} className="hp-term-host" />
      {searchOpen && <SearchBox search={searchRef.current} onClose={closeSearch} />}
      {toast &&
        (() => {
          const tt = TERMINAL_THEMES[terminalTheme] ?? TERMINAL_THEMES.dark;
          return (
            <div
              className="hp-toast"
              style={{
                fontFamily,
                fontSize,
                color: tt.foreground,
                // No box — a soft halo in the terminal's own bg keeps the bare
                // text legible over whatever output sits behind it.
                textShadow: `0 0 3px ${tt.background}, 0 0 3px ${tt.background}`
              }}
            >
              {toast}
            </div>
          );
        })()}
      {pathTip && (
        <div className="hp-path-tooltip" style={{ left: pathTip.x + 12, top: pathTip.y + 16 }}>
          <div className="hp-path-tooltip-path">{pathTip.text}</div>
          <div className="hp-path-tooltip-hint">click to open · Ctrl+click to copy</div>
        </div>
      )}
    </div>
  );
}

export const Terminal = memo(
  TerminalImpl,
  (a, b) =>
    a.paneId === b.paneId &&
    a.sessionUid === b.sessionUid &&
    a.command === b.command &&
    a.cwd === b.cwd &&
    a.shell === b.shell &&
    a.focused === b.focused &&
    a.visible === b.visible
);
