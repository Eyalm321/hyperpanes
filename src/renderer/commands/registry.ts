import { activeGroup, useWorkspace } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { useKeybindings } from '../store/useKeybindings';
import { comboLabel } from '../keybindings';
import { AUTO_LAYOUT, LAYOUTS } from '../layout/presets';
import { serializeWorkspace } from '../workspace/serialize';
import { getWebglContextCount } from '../perf';

export interface Command {
  id: string;
  title: string;
  subtitle?: string;
  keywords?: string;
  run: () => void;
}

/**
 * Builds the command list from current state each time the palette opens, so
 * pane-specific commands (focus/close) and the active-layout marker stay fresh.
 * Later phases register their commands here (zoom, restart, workspace).
 */
export function buildCommands(): Command[] {
  const ws = useWorkspace.getState();
  const g = activeGroup(ws);
  const ui = useUI.getState();
  const combos = useKeybindings.getState().combos;
  const key = (id: string) => comboLabel(combos[id]);
  const cmds: Command[] = [];

  cmds.push({
    id: 'new-tab',
    title: 'New tab',
    subtitle: 'Open a new workspace tab',
    keywords: 'group workspace add',
    run: () => ws.addGroup()
  });
  cmds.push({
    id: 'close-tab',
    title: 'Close tab',
    subtitle: `Close the current tab: ${g.title}`,
    keywords: 'group workspace remove',
    run: () => ws.closeGroup(ws.activeId)
  });
  cmds.push({
    id: 'reopen-tab',
    title: 'Reopen closed tab',
    subtitle: `Restore the last closed tab (${key('tab.reopen')})`,
    keywords: 'group workspace undo restore',
    run: () => ws.reopenGroup()
  });

  cmds.push({
    id: 'new-pane',
    title: 'New pane…',
    subtitle: 'Open the new-pane form',
    keywords: 'add create spawn',
    run: () => ui.openNewPane()
  });
  cmds.push({
    id: 'new-shell',
    title: 'New shell pane',
    subtitle: 'Spawn an interactive shell immediately',
    keywords: 'add create terminal',
    run: () => ws.addPane()
  });

  cmds.push({
    id: 'zoom-in',
    title: 'Zoom in',
    subtitle: `Increase the focused pane's font size (${key('zoom.in')} / Ctrl+wheel up)`,
    keywords: 'font bigger larger text size',
    run: () => g.focusedId && ws.zoomPane(g.focusedId, 1)
  });
  cmds.push({
    id: 'zoom-out',
    title: 'Zoom out',
    subtitle: `Decrease the focused pane's font size (${key('zoom.out')} / Ctrl+wheel down)`,
    keywords: 'font smaller text size',
    run: () => g.focusedId && ws.zoomPane(g.focusedId, -1)
  });
  cmds.push({
    id: 'zoom-reset',
    title: 'Reset zoom',
    subtitle: `Restore the focused pane's default font size (${key('zoom.reset')})`,
    keywords: 'font default text size',
    run: () => g.focusedId && ws.resetPaneZoom(g.focusedId)
  });

  cmds.push({
    id: 'preferences',
    title: 'Preferences…',
    subtitle: 'Open settings and keybindings',
    keywords: 'settings options config keybindings shortcuts',
    run: () => ui.openPreferences()
  });

  if (g.focusedId) {
    const focused = g.panes.find((p) => p.id === g.focusedId);
    cmds.push({
      id: 'zoom-pane',
      title: g.zoomedId ? 'Unzoom pane' : `Zoom pane: ${focused?.label ?? ''}`,
      subtitle: `Toggle full-window zoom (${key('pane.toggleZoom')})`,
      keywords: 'maximize fullscreen expand',
      run: () => ws.toggleZoom()
    });
    // Kill and respawn the focused pane. Also offered in the pane right-click menu.
    cmds.push({
      id: 'restart-pane',
      title: `Restart pane: ${focused?.label ?? ''}`,
      subtitle: 'Kill and respawn the pane',
      keywords: 'reload rerun relaunch',
      run: () => ws.restartPane(g.focusedId!)
    });
    cmds.push({
      id: 'close-pane',
      title: `Close pane: ${focused?.label ?? ''}`,
      subtitle: 'Close the focused pane',
      keywords: 'remove kill',
      run: () => ws.removePane(g.focusedId!)
    });
  }

  // Automatic first, then the 5 concrete presets (LAYOUTS stays auto-free).
  for (const l of [AUTO_LAYOUT, ...LAYOUTS]) {
    cmds.push({
      id: `layout-${l.id}`,
      title: `Layout: ${l.label}`,
      subtitle: g.layout === l.id ? 'current' : undefined,
      keywords: 'arrange tile split automatic',
      run: () => ws.setLayout(l.id)
    });
  }

  g.panes.forEach((p, i) => {
    cmds.push({
      id: `focus-${p.id}`,
      title: `Focus: ${p.label}`,
      subtitle: `pane ${i + 1}`,
      keywords: 'go switch select',
      run: () => ws.focusPane(p.id)
    });
  });

  cmds.push({
    id: 'save-workspace',
    title: 'Save workspace…',
    subtitle: 'Write panes + layout to a .json file',
    keywords: 'export write disk',
    run: () => void window.hp.workspace.save(serializeWorkspace())
  });
  cmds.push({
    id: 'open-workspace',
    title: 'Open workspace…',
    subtitle: 'Load panes + layout from a .json file',
    keywords: 'import load disk',
    run: async () => {
      const file = await window.hp.workspace.open();
      if (file && Array.isArray(file.panes) && file.panes.length > 0) {
        useWorkspace.getState().loadWorkspace(file);
      }
    }
  });

  // Diagnostics: shows memory / per-process / startup numbers (plus this window's
  // live WebGL-context count) in a panel, and also logs them to the console — the
  // before/after gauge for the visible-only-WebGL change.
  cmds.push({
    id: 'perf-metrics',
    title: 'Performance: Dump metrics',
    subtitle: 'Show memory, processes, startup, and WebGL contexts (panel + console)',
    keywords: 'performance memory profile diagnostics webgl startup footprint',
    run: async () => {
      const m = await window.hp.metrics();
      const liveCtx = getWebglContextCount();
      const paneCount = useWorkspace.getState().groups.reduce((n, gr) => n + gr.panes.length, 0);
      useUI.getState().openMetrics({ snap: m, liveCtx, paneCount });
      /* eslint-disable no-console */
      console.groupCollapsed(
        `%chyperpanes metrics%c — ${m.totalMemoryMB} MB · ${m.processes.length} procs · ${m.windows} win`,
        'font-weight:bold',
        'font-weight:normal'
      );
      console.log('startup (ms since process start):', m.startupMs);
      console.log(
        `WebGL contexts live in THIS window: ${liveCtx}` +
          ` (panes mounted across all tabs/windows: ${paneCount})`
      );
      console.log(`total memory: ${m.totalMemoryMB} MB across ${m.processes.length} processes`);
      console.table(m.byType);
      console.table(m.processes);
      console.groupEnd();
      /* eslint-enable no-console */
    }
  });

  return cmds;
}
