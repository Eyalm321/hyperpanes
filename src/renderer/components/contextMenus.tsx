import { useWorkspace } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { useSettings } from '../store/useSettings';
import { useKeybindings } from '../store/useKeybindings';
import { comboLabel } from '../keybindings';
import { AUTO_LAYOUT, LAYOUTS, effectiveLayout } from '../layout/presets';
import { paneTerminals } from './Terminal';
import { ColorSwatches } from './ColorSwatches';
import type { Layout } from '../types';
import type { MenuItem } from './ContextMenu';

// Pure builders that read the current store state and return the menu rows for a
// surface. Built fresh each time a menu opens, so labels, gating and checkmarks
// always reflect the moment of the right-click. Every row targets the clicked id
// directly — opening a menu never changes the active tab or focused pane.

const layoutGlyph = (layout: Layout): string | undefined =>
  layout === 'auto' ? AUTO_LAYOUT.icon : LAYOUTS.find((l) => l.id === layout)?.icon;

function layoutSubmenu(current: Layout, paneCount: number, pick: (l: Layout) => void): MenuItem[] {
  const autoResolved = LAYOUTS.find((l) => l.id === effectiveLayout('auto', paneCount));
  return [
    {
      kind: 'item',
      glyph: AUTO_LAYOUT.icon,
      label: autoResolved ? `${AUTO_LAYOUT.label} — ${autoResolved.label}` : AUTO_LAYOUT.label,
      checked: current === 'auto',
      onSelect: () => pick('auto')
    },
    { kind: 'sep' },
    ...LAYOUTS.map<MenuItem>((l) => ({
      kind: 'item',
      glyph: l.icon,
      label: l.label,
      checked: current === l.id,
      onSelect: () => pick(l.id)
    }))
  ];
}

// The Change Color swatches, kept live (subscribed to the pane's color) so the
// selected/bordered swatch tracks the current color even after you pick a new one
// — the menu stays open, but the static menu data wouldn't otherwise update.
function MenuColorSwatches({ paneId, groupId }: { paneId: string; groupId: string }) {
  const pane = useWorkspace((s) => {
    const g = s.groups.find((x) => x.id === groupId);
    return g?.panes.find((p) => p.id === paneId);
  });
  const recolorPane = useWorkspace((s) => s.recolorPane);
  const setPaneFrame = useWorkspace((s) => s.setPaneFrame);
  const setPaneDot = useWorkspace((s) => s.setPaneDot);
  const gFrame = useSettings((s) => s.showFrame);
  const gDot = useSettings((s) => s.showDot);
  if (!pane) return null;
  return (
    <ColorSwatches
      value={pane.color}
      onChange={(c) => recolorPane(paneId, c)}
      frameOn={pane.showFrame ?? gFrame}
      dotOn={pane.showDot ?? gDot}
      onToggleFrame={(on) => setPaneFrame(paneId, on)}
      onToggleDot={(on) => setPaneDot(paneId, on)}
    />
  );
}

const colorSubmenu = (paneId: string, groupId: string): MenuItem[] => [
  { kind: 'custom', node: <MenuColorSwatches paneId={paneId} groupId={groupId} /> }
];

// ---- Tab (workspace) menu ----
export function buildTabMenu(groupId: string): MenuItem[] {
  const ws = useWorkspace.getState();
  const ui = useUI.getState();
  const combos = useKeybindings.getState().combos;
  const g = ws.groups.find((x) => x.id === groupId);
  if (!g) return [];
  const idx = ws.groups.findIndex((x) => x.id === groupId);
  const only = ws.groups.length < 2;
  const isLast = idx === ws.groups.length - 1;

  return [
    { kind: 'item', label: 'New Tab', shortcut: comboLabel(combos['tab.new']), onSelect: () => ws.addGroup() },
    { kind: 'item', label: 'Rename…', onSelect: () => ui.requestRenameTab(groupId) },
    { kind: 'item', label: 'Duplicate Tab', onSelect: () => ws.duplicateGroup(groupId) },
    { kind: 'item', label: 'Move to New Window', disabled: only, onSelect: () => ws.popOutGroup(groupId) },
    { kind: 'sep' },
    { kind: 'item', label: 'Close Tab', onSelect: () => ws.closeGroup(groupId) },
    { kind: 'item', label: 'Close Other Tabs', disabled: only, onSelect: () => ws.closeOthers(groupId) },
    { kind: 'item', label: 'Close Tabs to the Right', disabled: isLast, onSelect: () => ws.closeToRight(groupId) },
    {
      kind: 'item',
      label: 'Reopen Closed Tab',
      shortcut: comboLabel(combos['tab.reopen']),
      disabled: ws.closed.length === 0,
      onSelect: () => ws.reopenGroup()
    },
    { kind: 'sep' },
    {
      kind: 'submenu',
      label: 'Layout',
      glyph: layoutGlyph(g.layout),
      items: layoutSubmenu(g.layout, g.panes.length, (l) => ws.setGroupLayout(groupId, l))
    }
  ];
}

// ---- Pane menu (shared by the pane header and the single-layout taskbar) ----
export function buildPaneMenu(
  paneId: string,
  groupId: string,
  opts?: { inTaskbar?: boolean }
): MenuItem[] {
  const ws = useWorkspace.getState();
  const ui = useUI.getState();
  const combos = useKeybindings.getState().combos;
  const g = ws.groups.find((x) => x.id === groupId);
  const p = g?.panes.find((x) => x.id === paneId);
  if (!g || !p) return [];

  const handle = paneTerminals.get(paneId);
  const hasSelection = !!handle?.term.hasSelection();
  const zoomed = g.zoomedId === paneId;
  const fullscreen = ui.fullscreenPaneId === paneId;
  const others = ws.groups.filter((x) => x.id !== groupId);

  const items: MenuItem[] = [];

  // The taskbar's left-click already shows the pane; offer it as the default row too.
  if (opts?.inTaskbar) {
    items.push({ kind: 'item', label: 'Show', onSelect: () => ws.focusPane(paneId) }, { kind: 'sep' });
  }

  // Effective frame/dot: a per-pane override wins, otherwise the global setting.
  const settings = useSettings.getState();
  const frameOn = p.showFrame ?? settings.showFrame;
  const dotOn = p.showDot ?? settings.showDot;

  items.push(
    { kind: 'item', label: 'New Pane…', onSelect: () => ui.openNewPane() },
    { kind: 'item', label: 'Rename…', onSelect: () => ui.requestRenamePane(paneId) },
    { kind: 'submenu', label: 'Change Color', items: colorSubmenu(paneId, groupId) },
    { kind: 'item', label: 'Show Frame', checked: frameOn, onSelect: () => ws.setPaneFrame(paneId, !frameOn) },
    { kind: 'item', label: 'Show Dot', checked: dotOn, onSelect: () => ws.setPaneDot(paneId, !dotOn) },
    // Ambient AI: a check means this pane is muted (no AI summary line).
    { kind: 'item', label: 'Mute AI Summary', checked: ui.aiMuted.has(paneId), onSelect: () => ui.toggleAiMute(paneId) },
    { kind: 'sep' }
  );

  // Maximize is meaningless in single layout (the taskbar surface), so drop it there.
  if (!opts?.inTaskbar) {
    items.push({
      kind: 'item',
      label: zoomed ? 'Restore' : 'Maximize',
      shortcut: comboLabel(combos['pane.toggleZoom']),
      onSelect: () => ws.toggleZoom(paneId)
    });
  }
  items.push({
    kind: 'item',
    label: fullscreen ? 'Exit Fullscreen' : 'Fullscreen',
    shortcut: comboLabel(combos['pane.toggleFullscreen']),
    onSelect: () => ui.toggleFullscreenPane(paneId)
  });
  items.push({
    kind: 'item',
    label: 'Search…',
    shortcut: comboLabel(combos['pane.search']),
    disabled: !handle,
    onSelect: () => paneTerminals.get(paneId)?.openSearch()
  });
  items.push({ kind: 'item', label: 'Restart', onSelect: () => ws.restartPane(paneId) });

  items.push(
    { kind: 'sep' },
    { kind: 'item', label: 'Copy', disabled: !hasSelection, onSelect: () => copySelection(paneId) },
    { kind: 'item', label: 'Paste', onSelect: () => pasteInto(paneId) },
    { kind: 'item', label: 'Select All', onSelect: () => paneTerminals.get(paneId)?.term.selectAll() },
    { kind: 'item', label: 'Clear', onSelect: () => paneTerminals.get(paneId)?.term.clear() },
    { kind: 'sep' },
    { kind: 'item', label: 'Move to New Tab', disabled: g.panes.length < 2, onSelect: () => ws.movePaneToNewGroup(paneId) }
  );
  if (others.length) {
    items.push({
      kind: 'submenu',
      label: 'Move to Tab',
      items: others.map<MenuItem>((o) => ({
        kind: 'item',
        label: o.title || 'workspace',
        onSelect: () => ws.movePaneToGroup(paneId, o.id)
      }))
    });
  }
  items.push({ kind: 'sep' }, { kind: 'item', label: 'Close Pane', danger: true, onSelect: () => ws.removePane(paneId) });

  return items;
}

function copySelection(paneId: string) {
  const sel = paneTerminals.get(paneId)?.term.getSelection();
  if (sel) void navigator.clipboard.writeText(sel).catch(() => {});
}

function pasteInto(paneId: string) {
  void navigator.clipboard
    .readText()
    .then((text) => {
      if (text) paneTerminals.get(paneId)?.term.paste(text);
    })
    .catch(() => {});
}
