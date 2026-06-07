import { create } from 'zustand';
import type { MenuItem } from '../components/ContextMenu';
import type { AiStatus, MetricsSnapshot } from '../types';

// A captured metrics snapshot plus the per-window renderer figures, shown by the
// "Performance: Dump metrics" command.
export interface MetricsView {
  snap: MetricsSnapshot;
  liveCtx: number;
  paneCount: number;
}

// Transient UI state that doesn't belong in the workspace model (modals, etc).
interface UIState {
  newPaneOpen: boolean;
  openNewPane: () => void;
  closeNewPane: () => void;

  paletteOpen: boolean;
  openPalette: () => void;
  closePalette: () => void;
  togglePalette: () => void;

  preferencesOpen: boolean;
  openPreferences: () => void;
  closePreferences: () => void;

  // A captured metrics snapshot shown by the "Performance: Dump metrics" command
  // (also logged to the console). Null = the metrics panel is closed.
  metricsData: MetricsView | null;
  openMetrics: (data: MetricsView) => void;
  closeMetrics: () => void;

  // While a pane is being dragged, the tab-strip drop target under the cursor:
  // a group id, 'new' (the +/strip), or null. Drives the drop highlight.
  paneDropTarget: string | null;
  setPaneDropTarget: (target: string | null) => void;

  // The drag label that follows the cursor during a pane drag (rendered globally
  // by App so it survives the source tab hiding on a spring-load). Null = idle.
  paneGhost: { x: number; y: number; label: string } | null;
  setPaneGhost: (ghost: { x: number; y: number; label: string } | null) => void;

  // Insertion indicator while dragging a pane over another tab's layout: drop the
  // pane on `edge` of pane `paneId` in group `groupId`. Null = not over a pane.
  layoutDrop: { groupId: string; paneId: string; edge: 'left' | 'right' | 'top' | 'bottom' } | null;
  setLayoutDrop: (drop: UIState['layoutDrop']) => void;

  // A tab from another window is hovering THIS window's strip (Chrome-style
  // mid-drag dock). `x` is the window-relative cursor x; drives the ghost slot.
  // Null when nothing is hovering.
  tabPreview: { x: number; title: string } | null;
  setTabPreview: (preview: { x: number; title: string } | null) => void;

  // The pane shown fullscreen (OS fullscreen + top bar hidden + pane fills the
  // screen), or null. Transient — launch never restores into fullscreen.
  fullscreenPaneId: string | null;
  setFullscreenPane: (id: string | null) => void;
  toggleFullscreenPane: (id: string) => void;

  // The single open right-click menu (tabs / pane header / taskbar). Centralizing
  // it here guarantees only one is ever open; <ContextMenu> is rendered once by App.
  contextMenu: { x: number; y: number; items: MenuItem[] } | null;
  openContextMenu: (x: number, y: number, items: MenuItem[]) => void;
  closeContextMenu: () => void;

  // "Rename…" from a context menu can't reach a component's inline editor directly,
  // so it sets a request id the surface watches: TabStrip opens its tab editor,
  // PaneFrame its label editor. Cleared by the surface once it starts editing.
  renameTabRequest: string | null;
  requestRenameTab: (id: string | null) => void;
  renamePaneRequest: string | null;
  requestRenamePane: (id: string | null) => void;

  // Ambient AI: latest status (drives the indicator + Preferences), and the set
  // of paneIds the user muted from AI summaries. Both transient — mute resets on
  // restart by design, and status is re-fetched on mount.
  aiStatus: AiStatus | null;
  setAiStatus: (status: AiStatus | null) => void;
  aiMuted: Set<string>;
  toggleAiMute: (paneId: string) => void;
}

export const useUI = create<UIState>((set) => ({
  newPaneOpen: false,
  openNewPane: () => set({ newPaneOpen: true }),
  closeNewPane: () => set({ newPaneOpen: false }),

  paletteOpen: false,
  openPalette: () => set({ paletteOpen: true }),
  closePalette: () => set({ paletteOpen: false }),
  togglePalette: () => set((s) => ({ paletteOpen: !s.paletteOpen })),

  preferencesOpen: false,
  openPreferences: () => set({ preferencesOpen: true }),
  closePreferences: () => set({ preferencesOpen: false }),

  metricsData: null,
  openMetrics: (data) => set({ metricsData: data }),
  closeMetrics: () => set({ metricsData: null }),

  paneDropTarget: null,
  setPaneDropTarget: (target) =>
    set((s) => (s.paneDropTarget === target ? s : { paneDropTarget: target })),

  paneGhost: null,
  setPaneGhost: (ghost) => set({ paneGhost: ghost }),

  layoutDrop: null,
  setLayoutDrop: (drop) =>
    set((s) => {
      const a = s.layoutDrop;
      if (a === drop) return s;
      if (a && drop && a.paneId === drop.paneId && a.edge === drop.edge && a.groupId === drop.groupId)
        return s;
      return { layoutDrop: drop };
    }),

  tabPreview: null,
  setTabPreview: (preview) => set({ tabPreview: preview }),

  fullscreenPaneId: null,
  setFullscreenPane: (id) => set({ fullscreenPaneId: id }),
  toggleFullscreenPane: (id) =>
    set((s) => ({ fullscreenPaneId: s.fullscreenPaneId === id ? null : id })),

  contextMenu: null,
  openContextMenu: (x, y, items) => set({ contextMenu: { x, y, items } }),
  closeContextMenu: () => set({ contextMenu: null }),

  renameTabRequest: null,
  requestRenameTab: (id) => set({ renameTabRequest: id }),
  renamePaneRequest: null,
  requestRenamePane: (id) => set({ renamePaneRequest: id }),

  aiStatus: null,
  setAiStatus: (aiStatus) => set({ aiStatus }),
  aiMuted: new Set<string>(),
  toggleAiMute: (paneId) =>
    set((s) => {
      const next = new Set(s.aiMuted);
      if (next.has(paneId)) next.delete(paneId);
      else next.add(paneId);
      return { aiMuted: next };
    })
}));
