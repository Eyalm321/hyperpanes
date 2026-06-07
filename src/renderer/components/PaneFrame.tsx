import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, PointerEvent as ReactPointerEvent } from 'react';
import { useWorkspace, type Group } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { useSettings } from '../store/useSettings';
import { useIdle, isAiPane } from '../store/useIdle';
import { runIdleEffect } from './idle-effects';
import { TERMINAL_THEMES } from './terminal-themes';
import { beginPaneDrag } from '../paneDrag';
import { useKeybindings } from '../store/useKeybindings';
import { comboLabel } from '../keybindings';
import { Terminal } from './Terminal';
import { EditableLabel, type EditableLabelHandle } from './EditableLabel';
import { ColorPopover } from './ColorPopover';
import { buildPaneMenu } from './contextMenus';
import type { Pane } from '../types';
import type { Rect } from '../layout/presets';

// Travel (px) before a header press becomes a pane drag.
const DRAG_THRESHOLD = 6;

interface PaneFrameProps {
  group: Group;
  pane: Pane;
  rect: Rect;
  visible: boolean; // shown within its group's layout (drives the tile's display)
  active: boolean; // this group is the active tab (its whole .hp-group is shown)
  focused: boolean;
}

export function PaneFrame({ group, pane, rect, visible, active, focused }: PaneFrameProps) {
  const focusPane = useWorkspace((s) => s.focusPane);
  const removePane = useWorkspace((s) => s.removePane);
  const renamePane = useWorkspace((s) => s.renamePane);
  const recolorPane = useWorkspace((s) => s.recolorPane);
  const setPaneFrame = useWorkspace((s) => s.setPaneFrame);
  const setPaneDot = useWorkspace((s) => s.setPaneDot);
  const markExited = useWorkspace((s) => s.markExited);
  const toggleZoom = useWorkspace((s) => s.toggleZoom);
  const toggleFullscreenPane = useUI((s) => s.toggleFullscreenPane);
  // Insert indicator while another pane is being dragged over this one's layout.
  const layoutDrop = useUI((s) => s.layoutDrop);
  const globalShowFrame = useSettings((s) => s.showFrame);
  const globalShowDot = useSettings((s) => s.showDot);
  // Effective per-pane values: an explicit per-pane override wins, otherwise the
  // pane inherits the global Appearance setting. New panes default both off.
  const frameOn = pane.showFrame ?? globalShowFrame;
  const dotOn = pane.showDot ?? globalShowDot;
  const idleAlert = useSettings((s) => s.idleAlert);
  const idleEffect = useSettings((s) => s.idleEffect);
  // The active terminal background, painted on the body so its padding (and any
  // sub-cell sliver xterm leaves) matches the terminal instead of showing the
  // dark pane bg — most visible on a light theme.
  const termBg = useSettings((s) => TERMINAL_THEMES[s.terminalTheme].background);
  const zoomKey = useKeybindings((s) => comboLabel(s.combos['pane.toggleZoom']));

  const zoomed = group.zoomedId === pane.id;
  const fullscreen = useUI((s) => s.fullscreenPaneId) === pane.id;

  const dotRef = useRef<HTMLButtonElement>(null);
  const [colorOpen, setColorOpen] = useState(false);
  const [shellTitle, setShellTitle] = useState('');

  // This pane has been quiet past the idle threshold (output quiescence — see
  // useIdle). Only AI/agent panes are ever flagged worth glowing for.
  const idle = useIdle((s) => !!s.idle[pane.id]);

  // The focused pane normally shouldn't glow — you're already looking at it. The
  // exception is when the whole window has lost focus (you tabbed away): then
  // even the focused AI pane should call you back. Only the focused pane needs to
  // watch window focus, so only it mounts the listeners.
  const [winFocused, setWinFocused] = useState(true);
  useEffect(() => {
    if (!focused) return;
    const sync = () => setWinFocused(document.hasFocus());
    sync();
    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    return () => {
      window.removeEventListener('focus', sync);
      window.removeEventListener('blur', sync);
    };
  }, [focused]);

  const glowOn =
    idleAlert &&
    idle &&
    pane.status === 'running' &&
    isAiPane(pane, shellTitle) &&
    !(focused && winFocused);

  // The idle glow. Driven imperatively via the Web Animations API on a dedicated
  // overlay element, so it (a) survives React re-renders without being wiped,
  // (b) never collides with the frame's inline focus box-shadow, and (c) can use
  // genuinely random gaps — a periodic CSS loop gets tuned out by the eye. The
  // chosen effect (firefly / pulse / blink / solid) is an Appearance setting; its
  // spec lives in idle-effects and is interpreted by the scheduler below.
  const glowRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = glowRef.current;
    if (!el || !glowOn) return;
    return runIdleEffect(el, idleEffect);
  }, [glowOn, pane.color, idleEffect]);

  // "Rename…" from this pane's context menu opens the label editor. Only the
  // targeted pane's selector flips true, so other panes don't re-render.
  const labelRef = useRef<EditableLabelHandle>(null);
  const wantRename = useUI((s) => s.renamePaneRequest === pane.id);
  useEffect(() => {
    if (!wantRename) return;
    labelRef.current?.start();
    useUI.getState().requestRenamePane(null);
  }, [wantRename]);
  // Pending drag: armed on pointerdown, becomes a real drag once travel passes the
  // threshold — then handed to the global pane drag (capture on <html> so it
  // survives this pane's tab hiding on a spring-load). See paneDrag.ts.
  const dragRef = useRef<{ pointerId: number; startX: number; startY: number } | null>(null);

  const onHeaderPointerDown = (e: ReactPointerEvent) => {
    if (e.button !== 0) return;
    // Leave the buttons/color-dot and the rename input to their own handlers.
    if ((e.target as HTMLElement).closest('button, input')) return;
    dragRef.current = { pointerId: e.pointerId, startX: e.clientX, startY: e.clientY };
  };

  const onHeaderPointerMove = (e: ReactPointerEvent) => {
    const d = dragRef.current;
    if (!d || d.pointerId !== e.pointerId) return;
    if (Math.hypot(e.clientX - d.startX, e.clientY - d.startY) < DRAG_THRESHOLD) return;
    // Past the threshold → hand off to the global pane drag and go quiet.
    dragRef.current = null;
    beginPaneDrag(e.pointerId, pane.id, group.id, pane.label);
  };

  // Fires only for a press that never became a drag (a click): just reset.
  const onHeaderPointerUp = (e: ReactPointerEvent) => {
    if (dragRef.current?.pointerId === e.pointerId) dragRef.current = null;
  };

  const onHeaderPointerCancel = () => {
    dragRef.current = null;
  };

  const dropEdge = layoutDrop?.paneId === pane.id ? layoutDrop.edge : null;

  const tileStyle: CSSProperties = {
    left: `${rect.x * 100}%`,
    top: `${rect.y * 100}%`,
    width: `${rect.w * 100}%`,
    height: `${rect.h * 100}%`,
    display: visible ? 'block' : 'none'
  };

  // The frame toggle drives the colored border, focus glow and header tint. With
  // it off the pane falls back to a neutral border and an accent focus ring, so
  // focus is still visible without the per-pane color.
  const paneStyle: CSSProperties = {
    borderColor: frameOn ? pane.color : undefined,
    boxShadow: focused
      ? frameOn
        ? `0 0 0 1px ${pane.color}, 0 0 16px ${pane.color}40`
        : '0 0 0 1px var(--hp-accent)'
      : 'none'
  };

  // Writes input from this pane's terminal to its pty session.
  const handleInput = (data: string) => {
    window.hp.write(pane.sessionUid, data);
  };

  return (
    <div className="hp-tile" style={tileStyle} data-pane-id={pane.id} data-group-id={group.id}>
      <div
        className={`hp-pane${focused ? ' hp-pane-focused' : ''}`}
        style={paneStyle}
        onMouseDown={() => focusPane(pane.id)}
      >
        {dropEdge && <div className={`hp-pane-dropline hp-dropline-${dropEdge}`} />}
        {glowOn && (
          <div
            className="hp-pane-glow"
            ref={glowRef}
            style={{ ['--hp-idle-c' as string]: pane.color } as CSSProperties}
          />
        )}
        <div
          className="hp-pane-header"
          style={{ background: frameOn ? `${pane.color}1a` : undefined }}
          title="Drag to another tab, or out of the window to open a new one"
          onPointerDown={onHeaderPointerDown}
          onPointerMove={onHeaderPointerMove}
          onPointerUp={onHeaderPointerUp}
          onPointerCancel={onHeaderPointerCancel}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            useUI.getState().openContextMenu(e.clientX, e.clientY, buildPaneMenu(pane.id, group.id));
          }}
        >
          {dotOn && (
            <button
              ref={dotRef}
              className="hp-pane-dot"
              style={{ background: pane.color }}
              title="Change frame color"
              onMouseDown={(e) => e.stopPropagation()}
              onClick={(e) => {
                e.stopPropagation();
                setColorOpen((o) => !o);
              }}
            />
          )}
          <EditableLabel
            ref={labelRef}
            value={pane.label}
            subtitle={pane.subtitle}
            shellTitle={shellTitle}
            onCommit={(label, subtitle) => renamePane(pane.id, label, subtitle)}
          />
          {pane.status === 'exited' && (
            <span className="hp-pane-exit">
              exited{pane.exitCode != null ? ` (${pane.exitCode})` : ''}
            </span>
          )}
          <span className="hp-spacer" />
          {/* Restart lives in the pane's right-click menu (and the command palette),
              not the header — keeps the top bar to layout/window controls. */}
          <button
            className={`hp-pane-btn${zoomed ? ' active' : ''}`}
            title={zoomed ? `Restore (${zoomKey})` : `Maximize within window (${zoomKey})`}
            onMouseDown={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation();
              toggleZoom(pane.id);
            }}
          >
            ⤢
          </button>
          <button
            className={`hp-pane-btn${fullscreen ? ' active' : ''}`}
            title={fullscreen ? 'Exit fullscreen (F11, or hold Esc)' : 'Fullscreen (F11)'}
            onMouseDown={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation();
              toggleFullscreenPane(pane.id);
            }}
          >
            ⛶
          </button>
          <button
            className="hp-pane-btn hp-pane-close"
            title="Close pane"
            onMouseDown={(e) => e.stopPropagation()}
            onClick={(e) => {
              e.stopPropagation();
              removePane(pane.id);
            }}
          >
            ×
          </button>
        </div>
        <div className="hp-pane-body" style={{ background: termBg }}>
          <Terminal
            paneId={pane.id}
            sessionUid={pane.sessionUid}
            command={pane.command}
            args={pane.args}
            cwd={pane.cwd}
            shell={pane.shell}
            env={pane.env}
            focused={focused}
            // On screen only when this tab is active AND this tile is shown — the
            // gate for keeping a GPU context (see Terminal's `visible`).
            visible={active && visible}
            onExit={(code) => markExited(pane.id, code)}
            onTitle={setShellTitle}
            onInput={handleInput}
          />
        </div>
      </div>

      {colorOpen && dotRef.current && (
        <ColorPopover
          anchor={dotRef.current.getBoundingClientRect()}
          value={pane.color}
          onChange={(c) => recolorPane(pane.id, c)}
          frameOn={frameOn}
          dotOn={dotOn}
          onToggleFrame={(on) => setPaneFrame(pane.id, on)}
          onToggleDot={(on) => setPaneDot(pane.id, on)}
          onClose={() => setColorOpen(false)}
        />
      )}
    </div>
  );
}
