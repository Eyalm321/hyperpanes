import { useWorkspace, type Group } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { buildPaneMenu } from './contextMenus';

// Bottom strip shown only in `single` layout (see PaneArea): every pane gets a
// button so the hidden ones are reachable. Click switches which pane is shown
// (focusPane → the single preset shows the focused pane); middle-click closes,
// matching the workspace tab strip.
export function PaneTaskbar({ group }: { group: Group }) {
  const focusPane = useWorkspace((s) => s.focusPane);
  const removePane = useWorkspace((s) => s.removePane);

  return (
    <div className="hp-pane-taskbar">
      {group.panes.map((pane) => {
        const shown = pane.id === group.focusedId;
        return (
          <button
            key={pane.id}
            className={`hp-pane-taskitem${shown ? ' active' : ''}`}
            title={pane.subtitle ? `${pane.label} — ${pane.subtitle}` : pane.label}
            onMouseDown={(e) => {
              if (e.button === 0) focusPane(pane.id);
              else if (e.button === 1) {
                e.preventDefault();
                removePane(pane.id);
              }
            }}
            onContextMenu={(e) => {
              e.preventDefault();
              e.stopPropagation();
              useUI
                .getState()
                .openContextMenu(e.clientX, e.clientY, buildPaneMenu(pane.id, group.id, { inTaskbar: true }));
            }}
          >
            <span className="hp-pane-taskitem-dot" style={{ background: pane.color }} />
            <span className="hp-pane-taskitem-label">{pane.label || 'pane'}</span>
            {pane.status === 'exited' && <span className="hp-pane-taskitem-exit">exited</span>}
          </button>
        );
      })}
    </div>
  );
}
