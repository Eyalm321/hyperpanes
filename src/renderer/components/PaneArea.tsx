import { useRef } from 'react';
import type { Group } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { computeDividers, computeTiles, effectiveLayout, type Tile } from '../layout/presets';
import { PaneFrame } from './PaneFrame';
import { PaneTaskbar } from './PaneTaskbar';
import { Divider } from './Divider';

const FULL = { x: 0, y: 0, w: 1, h: 1 };

interface PaneAreaProps {
  group: Group;
  active: boolean; // only the active tab takes focus / shows its glow
}

export function PaneArea({ group, active }: PaneAreaProps) {
  const { panes, layout, focusedId, zoomedId, sizes, mainFraction } = group;
  const containerRef = useRef<HTMLDivElement>(null);

  const focusedIndex = panes.findIndex((p) => p.id === focusedId);

  // A fullscreen pane (active tab only) solos exactly like zoom does. Falling back
  // to zoomedId means both maximize-in-window and fullscreen share one code path.
  const fullscreenPaneId = useUI((s) => s.fullscreenPaneId);
  const soloId =
    active && fullscreenPaneId && panes.some((p) => p.id === fullscreenPaneId)
      ? fullscreenPaneId
      : zoomedId;

  // When soloed (zoom or fullscreen), only that pane is shown full and seams hide.
  let tiles: Tile[];
  let dividers: ReturnType<typeof computeDividers>;
  let eff = layout;
  if (soloId) {
    tiles = panes.map((p, i) => ({ index: i, rect: FULL, visible: p.id === soloId }));
    dividers = [];
  } else {
    // 'auto' resolves to a concrete preset by pane count before tiling.
    eff = effectiveLayout(layout, panes.length);
    tiles = computeTiles(eff, panes.length, sizes, mainFraction, focusedIndex);
    dividers = computeDividers(eff, panes.length, sizes, mainFraction);
  }
  const tileByIndex = new Map(tiles.map((t) => [t.index, t]));

  // In single layout the non-shown panes are mounted but hidden, so surface them
  // as a bottom taskbar (click to switch). The tile region insets above it.
  const showTaskbar = !soloId && eff === 'single' && panes.length > 1;

  return (
    <div className="hp-panearea">
      <div
        className={`hp-panearea-tiles${showTaskbar ? ' hp-panearea-tiles--taskbar' : ''}`}
        ref={containerRef}
      >
        {panes.length === 0 && (
          <div className="hp-empty">No panes — press ＋ Pane to add one</div>
        )}
        {panes.map((pane, i) => {
          const tile = tileByIndex.get(i);
          if (!tile) return null;
          return (
            <PaneFrame
              key={pane.id}
              group={group}
              pane={pane}
              rect={tile.rect}
              visible={tile.visible}
              focused={active && pane.id === focusedId}
            />
          );
        })}
        {dividers.map((d) => (
          <Divider key={d.id} desc={d} containerRef={containerRef} />
        ))}
      </div>
      {showTaskbar && <PaneTaskbar group={group} />}
    </div>
  );
}
