import { useEffect, useRef, useState } from 'react';
import { activeGroup, useWorkspace } from './store/useWorkspace';
import { useUI } from './store/useUI';
import { stitchSlotAt, slotToIndex } from './stitch';
import { useKeybindings } from './store/useKeybindings';
import { comboMatches } from './keybindings';
import { serializeSession } from './workspace/serialize';
import { TopBar } from './components/TopBar';
import { PaneArea } from './components/PaneArea';
import { NewPaneDialog } from './components/NewPaneDialog';
import { CommandPalette } from './components/CommandPalette';
import { PreferencesDialog } from './components/PreferencesDialog';
import { ContextMenu } from './components/ContextMenu';

// Hold Esc this long (ms) to leave fullscreen. A deliberate hold (not a tap, which
// still reaches the terminal) avoids yanking out of fullscreen by accident.
const ESC_HOLD_MS = 700;

// Insertion index for a tab docking at window-relative x: the count of existing
// tabs whose horizontal midpoint sits left of the cursor.
function tabInsertIndex(x: number): number {
  const tabs = Array.from(document.querySelectorAll('.hp-tabstrip .hp-tab')) as HTMLElement[];
  let i = 0;
  for (const tab of tabs) {
    const r = tab.getBoundingClientRect();
    if (x < r.left + r.width / 2) break;
    i++;
  }
  return i;
}

export default function App() {
  const groups = useWorkspace((s) => s.groups);
  const activeId = useWorkspace((s) => s.activeId);
  // Drag label that follows the cursor during a pane drag — rendered here (not in
  // the pane) so it stays visible when a spring-load hides the source pane's tab.
  const paneGhost = useUI((s) => s.paneGhost);
  // The single open right-click menu, rendered once here (like paneGhost/dialogs).
  const contextMenu = useUI((s) => s.contextMenu);
  const closeContextMenu = useUI((s) => s.closeContextMenu);
  // A fullscreened pane hides the top bar so only the terminal shows.
  const fullscreenPaneId = useUI((s) => s.fullscreenPaneId);
  // Fullscreen exit hint: shown briefly on enter, and while Esc is being held
  // (with a progress bar filling over ESC_HOLD_MS).
  const [escHolding, setEscHolding] = useState(false);
  const [hintVisible, setHintVisible] = useState(false);

  // Only the session-of-record (primary) window persists last-workspace.json, so
  // extra windows don't clobber it. Set from main's getSeed; defaults to primary.
  const primaryRef = useRef(true);

  useEffect(() => {
    // On launch, decide what this window shows. A window torn off from another
    // adopts its handed-over group (live shells included) and does nothing else.
    // The primary window restores the saved session (all tabs) or seeds a shell.
    let cancelled = false;
    void window.hp.win.getSeed().then(({ seed, primary }) => {
      if (cancelled) return;
      primaryRef.current = primary;
      const s = useWorkspace.getState();
      if (seed) {
        s.injectGroup(seed);
        return;
      }
      if (!primary) return; // a seedless non-primary window keeps its empty tab
      void window.hp.workspace.getInitial().then((file) => {
        if (cancelled) return;
        const st = useWorkspace.getState();
        // Already populated (e.g. StrictMode re-run) — leave it.
        if (activeGroup(st).panes.length > 0 || st.groups.length > 1) return;
        const hasContent =
          !!file &&
          ((file.groups?.length ?? 0) > 0 || (Array.isArray(file.panes) && file.panes.length > 0));
        if (hasContent) st.loadSession(file!);
        else st.addPane({ label: 'shell' });
      });
    });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    // A tab from another window docked onto this one — insert it at the cursor
    // slot and clear any dock-preview ghost.
    return window.hp.win.onReceiveTab((group, x) => {
      useUI.getState().setTabPreview(null);
      useWorkspace.getState().injectGroup(group, x != null ? tabInsertIndex(x) : undefined);
    });
  }, []);

  useEffect(() => {
    // Mid-drag dock preview from main: a tab is hovering our strip (show/move the
    // ghost slot) or has left (clear it).
    return window.hp.win.onTabPreview((preview) => useUI.getState().setTabPreview(preview));
  }, []);

  useEffect(() => {
    // Cross-window pane stitch: a single-pane float from another window is hovering
    // our pane area. Preview shows the insert indicator at the slot; the commit
    // adopts the pane into the targeted (active) group at that slot.
    const offPreview = window.hp.win.onPaneStitchPreview((at) =>
      useUI.getState().setLayoutDrop(at ? stitchSlotAt(at.x, at.y) : null)
    );
    const offStitch = window.hp.win.onPaneStitch(({ group, x, y }) => {
      const pane = group.panes[0];
      if (!pane) return;
      const ws = useWorkspace.getState();
      const slot = stitchSlotAt(x, y);
      const targetGid = slot?.groupId ?? ws.activeId;
      const index = slot
        ? slotToIndex(slot)
        : (ws.groups.find((g) => g.id === targetGid)?.panes.length ?? 0);
      ws.adoptPaneInto(pane, targetGid, index);
      useUI.getState().setLayoutDrop(null);
    });
    return () => {
      offPreview();
      offStitch();
    };
  }, []);

  useEffect(() => {
    // Promoted to session-of-record (the prior primary window closed): take over
    // persistence and write the current session immediately.
    return window.hp.win.onPrimary(() => {
      primaryRef.current = true;
      window.hp.workspace.saveLast(serializeSession());
    });
  }, []);

  useEffect(() => {
    // Persist the whole session (all tabs, debounced) so launch restores it —
    // primary window only (see primaryRef).
    let timer: ReturnType<typeof setTimeout> | undefined;
    const unsub = useWorkspace.subscribe(() => {
      if (!primaryRef.current) return;
      if (timer) clearTimeout(timer);
      timer = setTimeout(() => window.hp.workspace.saveLast(serializeSession()), 600);
    });
    return () => {
      unsub();
      if (timer) clearTimeout(timer);
    };
  }, []);

  useEffect(() => {
    // Reflect pane-fullscreen into (simple) OS fullscreen. Entering also hides the
    // top bar via the conditional render below. Simple fullscreen is app-driven —
    // exit is via the ⛶ button or Esc (see the keydown handler), never the OS — so
    // there's no native-exit event to sync back.
    window.hp.win.setFullScreen(fullscreenPaneId != null);
  }, [fullscreenPaneId]);

  useEffect(() => {
    // Flash the "hold Esc to exit" hint for a moment on entering fullscreen so the
    // exit gesture is discoverable, then let it fade.
    if (!fullscreenPaneId) {
      setHintVisible(false);
      return;
    }
    setHintVisible(true);
    const t = setTimeout(() => setHintVisible(false), 2500);
    return () => clearTimeout(t);
  }, [fullscreenPaneId]);

  useEffect(() => {
    // Drop fullscreen if its pane leaves the active group (closed, moved, or a tab
    // switch) — otherwise we'd sit OS-fullscreen showing nothing.
    return useWorkspace.subscribe((s) => {
      const fid = useUI.getState().fullscreenPaneId;
      if (!fid) return;
      const active = s.groups.find((g) => g.id === s.activeId);
      if (!active || !active.panes.some((p) => p.id === fid)) {
        useUI.getState().setFullscreenPane(null);
      }
    });
  }, []);

  useEffect(() => {
    // Esc-to-exit-fullscreen is a HOLD, not a tap: a quick Esc still reaches the
    // terminal (vim etc.), and only a sustained hold leaves fullscreen. The timer
    // lives in this effect closure; keyup clears it.
    let escHoldTimer: ReturnType<typeof setTimeout> | null = null;
    const clearEscHold = () => {
      if (escHoldTimer != null) {
        clearTimeout(escHoldTimer);
        escHoldTimer = null;
      }
      setEscHolding(false);
    };

    // Capture-phase so shortcuts work even while a terminal is focused. All
    // combos come from the keybindings store, so they reflect any user rebinds.
    const onKey = (e: KeyboardEvent) => {
      const ui = useUI.getState();
      // Preferences owns the keyboard while open (incl. combo recording).
      if (ui.preferencesOpen) return;

      const combos = useKeybindings.getState().combos;
      const consume = () => {
        e.preventDefault();
        e.stopPropagation();
      };

      // Hold Esc to leave a fullscreen pane (only when no modal owns Esc). The
      // first press passes through so the terminal still gets a tap; the OS
      // auto-repeat is swallowed so the pane isn't flooded while held.
      if (ui.fullscreenPaneId && !ui.paletteOpen && !ui.newPaneOpen && e.key === 'Escape') {
        if (e.repeat) {
          consume();
          return;
        }
        if (escHoldTimer == null) {
          setEscHolding(true);
          escHoldTimer = setTimeout(() => {
            escHoldTimer = null;
            setEscHolding(false);
            useUI.getState().setFullscreenPane(null);
          }, ESC_HOLD_MS);
        }
        return;
      }

      // Command palette toggles even when itself is open.
      if (comboMatches(combos['palette.toggle'], e)) {
        consume();
        ui.togglePalette();
        return;
      }

      // Pane shortcuts are suppressed while a modal owns the keyboard.
      if (ui.paletteOpen || ui.newPaneOpen) return;

      const ws = useWorkspace.getState();
      const focusedPaneId = activeGroup(ws).focusedId;
      const actions: Record<string, () => void> = {
        'zoom.in': () => focusedPaneId && ws.zoomPane(focusedPaneId, 1),
        'zoom.out': () => focusedPaneId && ws.zoomPane(focusedPaneId, -1),
        'zoom.reset': () => focusedPaneId && ws.resetPaneZoom(focusedPaneId),
        'tab.new': () => ws.addGroup(),
        'tab.next': () => ws.cycleGroup(1),
        'tab.prev': () => ws.cycleGroup(-1),
        'tab.reopen': () => ws.reopenGroup(),
        'pane.focusLeft': () => ws.focusDirection('left'),
        'pane.focusRight': () => ws.focusDirection('right'),
        'pane.focusUp': () => ws.focusDirection('up'),
        'pane.focusDown': () => ws.focusDirection('down'),
        'pane.toggleZoom': () => ws.toggleZoom(),
        // F11 toggles fullscreen for the focused pane (or exits if any is on).
        'pane.toggleFullscreen': () => {
          const u = useUI.getState();
          if (u.fullscreenPaneId) u.setFullscreenPane(null);
          else if (focusedPaneId) u.setFullscreenPane(focusedPaneId);
        }
        // 'pane.search' is handled inside the focused Terminal.
      };
      for (const id in actions) {
        if (comboMatches(combos[id], e)) {
          consume();
          actions[id]();
          return;
        }
      }

      // Focus pane by number — Alt+1…9 (fixed, not rebindable).
      if (e.altKey && !e.ctrlKey && !e.metaKey && !e.shiftKey && e.key >= '1' && e.key <= '9') {
        consume();
        ws.focusIndex(Number(e.key) - 1);
      }
    };
    const onKeyUp = (e: KeyboardEvent) => {
      if (e.key === 'Escape') clearEscHold();
    };
    window.addEventListener('keydown', onKey, true);
    window.addEventListener('keyup', onKeyUp, true);
    return () => {
      window.removeEventListener('keydown', onKey, true);
      window.removeEventListener('keyup', onKeyUp, true);
      if (escHoldTimer != null) clearTimeout(escHoldTimer);
    };
  }, []);

  return (
    <div className="hp-app">
      {!fullscreenPaneId && <TopBar />}
      <div className="hp-groups">
        {groups.map((g) => (
          <div
            key={g.id}
            className="hp-group"
            style={{ display: g.id === activeId ? 'flex' : 'none' }}
          >
            <PaneArea group={g} active={g.id === activeId} />
          </div>
        ))}
      </div>
      <NewPaneDialog />
      <CommandPalette />
      <PreferencesDialog />
      {contextMenu && (
        <ContextMenu
          x={contextMenu.x}
          y={contextMenu.y}
          items={contextMenu.items}
          onClose={closeContextMenu}
        />
      )}
      {paneGhost && (
        <div className="hp-tab-ghost" style={{ left: paneGhost.x + 12, top: paneGhost.y + 8 }}>
          {paneGhost.label}
        </div>
      )}
      {fullscreenPaneId && (
        <div
          className={`hp-fs-hint${hintVisible || escHolding ? ' hp-fs-hint--show' : ''}${
            escHolding ? ' hp-fs-hint--holding' : ''
          }`}
        >
          <span>
            Hold <kbd>Esc</kbd> to exit fullscreen
          </span>
          <div className="hp-fs-hint-track">
            <div
              className="hp-fs-hint-fill"
              style={{ transitionDuration: escHolding ? `${ESC_HOLD_MS}ms` : '150ms' }}
            />
          </div>
        </div>
      )}
    </div>
  );
}
