import { useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent } from 'react';
import { useWorkspace, type Group } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { startLiveTearOff } from '../liveTearOff';
import { beginDragGuard } from '../dragGuard';
import { buildTabMenu } from './contextMenus';
import { IconPlus } from './Icons';

// Pointer travel (px) before a tab press becomes a tear-off drag. Small enough
// to stay over the grabbed tab (so we don't miss it before capture), large
// enough that ordinary clicks/double-clicks never trigger a drag.
const TEAR_THRESHOLD = 6;

// The workspace tab strip (Chrome-like). Each tab is a group: click to switch,
// double-click to rename, ×/middle-click to close, + to open a new tab. Dropping
// a dragged pane on a tab moves it there; dropping on empty strip area makes a
// new workspace from it.
export function TabStrip() {
  const groups = useWorkspace((s) => s.groups);
  const activeId = useWorkspace((s) => s.activeId);
  const setActiveGroup = useWorkspace((s) => s.setActiveGroup);
  const addGroup = useWorkspace((s) => s.addGroup);
  const closeGroup = useWorkspace((s) => s.closeGroup);
  const renameGroup = useWorkspace((s) => s.renameGroup);
  const extractGroup = useWorkspace((s) => s.extractGroup);
  const moveGroupToIndex = useWorkspace((s) => s.moveGroupToIndex);

  // Drop highlight while a pane is being dragged (set by PaneFrame via useUI).
  const paneDropTarget = useUI((s) => s.paneDropTarget);
  // A tab from another window is hovering our strip (Chrome-style mid-drag dock):
  // show a ghost slot at the cursor x. Set from main via App's onTabPreview.
  const tabPreview = useUI((s) => s.tabPreview);

  const [editingId, setEditingId] = useState<string | null>(null);
  const [draft, setDraft] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  // Active tab tear-off gesture (HTML5 DnD can't cross OS windows, so tabs use a
  // native pointer drag). Past 6px it's a `dragging` gesture, but the tab only
  // tears off once it's pulled OFF the strip (Chrome-like) — dragging or
  // releasing on the tab bar keeps it docked, so small nudges never tear off.
  const tearRef = useRef<{
    pointerId: number;
    groupId: string;
    startX: number;
    startY: number;
    dragging: boolean;
  } | null>(null);

  const onTabPointerDown = (e: ReactPointerEvent, g: Group) => {
    // Left button only; leave clicks on the × (and rename editing) untouched.
    if (e.button !== 0 || editingId === g.id) return;
    if ((e.target as HTMLElement).closest('.hp-tab-close')) return;
    tearRef.current = {
      pointerId: e.pointerId,
      groupId: g.id,
      startX: e.clientX,
      startY: e.clientY,
      dragging: false
    };
  };

  const onTabPointerMove = (e: ReactPointerEvent, g: Group) => {
    const d = tearRef.current;
    if (!d || d.pointerId !== e.pointerId) return;
    if (!d.dragging) {
      if (Math.hypot(e.clientX - d.startX, e.clientY - d.startY) < TEAR_THRESHOLD) return;
      d.dragging = true;
      // Kill native text-selection for the drag (otherwise the bar's text drags
      // into a selection). Self-clears on the next pointerup, incl. after tear-off.
      beginDragGuard();
      // Capture so we keep getting moves once the cursor leaves the strip/window
      // (without it we'd stop hearing the pointer the moment it left this tab).
      try {
        e.currentTarget.setPointerCapture(e.pointerId);
      } catch {
        /* capture unsupported — drag still works while over this window */
      }
    }
    // Still over a tab strip → keep it docked and reorder live among its siblings.
    // The slot is counted from the OTHER tabs' centers (the dragged one skipped),
    // which is jitter-free — moving the dragged tab can't shift its own references.
    const strip = (document.elementFromPoint(e.clientX, e.clientY) as HTMLElement | null)?.closest(
      '.hp-tabstrip'
    );
    if (strip) {
      let index = 0;
      for (const t of strip.querySelectorAll<HTMLElement>('.hp-tab')) {
        if (t.getAttribute('data-group-id') === d.groupId) continue;
        const r = t.getBoundingClientRect();
        if (e.clientX > r.left + r.width / 2) index++;
      }
      moveGroupToIndex(d.groupId, index);
      return;
    }
    // Pulled off the strip → tear off. Capture moves to <html> (survives this tab
    // unmounting) and main opens/follows a window until release.
    tearRef.current = null;
    setActiveGroup(g.id);
    if (groups.length === 1) {
      // The window's only tab → drag the whole window (Chrome-like). Don't extract
      // (which would reseed a fresh tab and leave a duplicate window behind);
      // hand main the live group and let it move THIS window.
      startLiveTearOff(e.pointerId, g, { moveWindow: true });
      return;
    }
    // Extract the live tab up front (marks its sessions "moving" so they detach,
    // not die) and hand it to the live tear-off as a new floating window.
    const group = extractGroup(d.groupId);
    if (group) startLiveTearOff(e.pointerId, group);
  };

  // Released without ever leaving the strip → it stays docked (a click or an
  // on-strip nudge). Just reset and let go of the capture.
  const onTabPointerUp = (e: ReactPointerEvent) => {
    if (tearRef.current?.pointerId !== e.pointerId) return;
    tearRef.current = null;
    try {
      e.currentTarget.releasePointerCapture(e.pointerId);
    } catch {
      /* nothing captured */
    }
  };

  const onTabPointerCancel = () => {
    tearRef.current = null;
  };

  useEffect(() => {
    if (editingId) inputRef.current?.select();
  }, [editingId]);

  const startEdit = (id: string, title: string) => {
    setEditingId(id);
    setDraft(title);
  };
  const commitEdit = () => {
    if (editingId) {
      const t = draft.trim();
      if (t) renameGroup(editingId, t);
    }
    setEditingId(null);
  };

  // "Rename…" from a tab's context menu opens that tab's inline editor.
  const renameTabRequest = useUI((s) => s.renameTabRequest);
  useEffect(() => {
    if (!renameTabRequest) return;
    const g = groups.find((x) => x.id === renameTabRequest);
    if (g) startEdit(g.id, g.title);
    useUI.getState().requestRenameTab(null);
  }, [renameTabRequest, groups]);

  return (
    <>
    <div
      className={`hp-tabstrip${paneDropTarget === 'new' || tabPreview ? ' hp-tabstrip-drop' : ''}`}
    >
      {tabPreview && (
        <div className="hp-tab-dock-ghost" style={{ left: tabPreview.x }} title={tabPreview.title}>
          <span className="hp-tab-title">{tabPreview.title}</span>
        </div>
      )}
      {groups.map((g) => (
        <div
          key={g.id}
          data-group-id={g.id}
          className={`hp-tab${g.id === activeId ? ' active' : ''}${paneDropTarget === g.id ? ' hp-tab-drop' : ''}`}
          title={g.title}
          onMouseDown={(e) => {
            if (e.button === 0 && editingId !== g.id) setActiveGroup(g.id);
            if (e.button === 1) {
              e.preventDefault();
              closeGroup(g.id);
            }
          }}
          onPointerDown={(e) => onTabPointerDown(e, g)}
          onPointerMove={(e) => onTabPointerMove(e, g)}
          onPointerUp={onTabPointerUp}
          onPointerCancel={onTabPointerCancel}
          onDoubleClick={() => startEdit(g.id, g.title)}
          onContextMenu={(e) => {
            e.preventDefault();
            e.stopPropagation();
            useUI.getState().openContextMenu(e.clientX, e.clientY, buildTabMenu(g.id));
          }}
        >
          {editingId === g.id ? (
            <input
              ref={inputRef}
              className="hp-tab-input"
              value={draft}
              autoFocus
              onChange={(e) => setDraft(e.target.value)}
              onBlur={commitEdit}
              onMouseDown={(e) => e.stopPropagation()}
              onKeyDown={(e) => {
                if (e.key === 'Enter') commitEdit();
                else if (e.key === 'Escape') setEditingId(null);
                e.stopPropagation();
              }}
            />
          ) : (
            <>
              <span className="hp-tab-title">{g.title}</span>
              <button
                className="hp-tab-close"
                title="Close tab"
                aria-label="Close tab"
                onMouseDown={(e) => e.stopPropagation()}
                onClick={(e) => {
                  e.stopPropagation();
                  closeGroup(g.id);
                }}
              >
                ×
              </button>
            </>
          )}
        </div>
      ))}
      <button
        className={`hp-tab-new${paneDropTarget === 'new' ? ' hp-tab-drop' : ''}`}
        onClick={() => addGroup()}
        title="New tab (or drop a pane here to split it off)"
        aria-label="New tab"
      >
        <IconPlus />
      </button>
    </div>
    </>
  );
}
