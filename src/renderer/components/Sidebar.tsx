import { useEffect, useRef, useState } from 'react';
import { useProjects } from '../store/useProjects';
import { useWorkspace } from '../store/useWorkspace';
import { useUI } from '../store/useUI';
import { ColorSwatches } from './ColorSwatches';
import { IconPlus, IconFolder } from './Icons';
import type { Project } from '../types';
import type { MenuItem } from './ContextMenu';

// Inline rename field dropped into the project's right-click menu as a 'custom'
// node — Enter commits, Escape/blur dismisses the whole menu (no separate dialog).
function ProjectRename({ project, onDone }: { project: Project; onDone: () => void }) {
  const rename = useProjects((s) => s.rename);
  const [value, setValue] = useState(project.name);
  return (
    <input
      className="hp-sidebar-rename"
      autoFocus
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === 'Enter') {
          const next = value.trim();
          if (next && next !== project.name) rename(project.id, next);
          onDone();
        } else if (e.key === 'Escape') {
          onDone();
        }
      }}
      // Stop the menu's own key handling from stealing typed keys.
      onClick={(e) => e.stopPropagation()}
    />
  );
}

// Slim right-edge rail: icon-only controls. The git-projects history lives behind
// the folder icon and expands as a flyout panel toward the pane area.
export function Sidebar() {
  const projects = useProjects((s) => s.projects);
  const setColor = useProjects((s) => s.setColor);
  const remove = useProjects((s) => s.remove);
  const openInPane = useProjects((s) => s.openInPane);
  const addPane = useWorkspace((s) => s.addPane);
  const openNewPane = useUI((s) => s.openNewPane);

  const [projectsOpen, setProjectsOpen] = useState(false);
  const railRef = useRef<HTMLElement>(null);

  // Dismiss the flyout on Escape or a click outside the rail — but NOT when the
  // click lands in a context menu (.hp-menu), so the project's own right-click
  // menu (recolor/rename/remove) stays usable.
  useEffect(() => {
    if (!projectsOpen) return;
    const onDown = (e: MouseEvent) => {
      const t = e.target as HTMLElement;
      if (railRef.current?.contains(t) || t.closest('.hp-menu')) return;
      setProjectsOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setProjectsOpen(false);
    };
    window.addEventListener('mousedown', onDown);
    window.addEventListener('keydown', onKey);
    return () => {
      window.removeEventListener('mousedown', onDown);
      window.removeEventListener('keydown', onKey);
    };
  }, [projectsOpen]);

  // The project row right-click menu, built inline (recolor swatches + rename
  // field + remove) and shown via the shared cursor-anchored ContextMenu.
  const openMenu = (e: React.MouseEvent, project: Project) => {
    e.preventDefault();
    const ui = useUI.getState();
    const items: MenuItem[] = [
      {
        kind: 'submenu',
        label: 'Change Color',
        items: [
          {
            kind: 'custom',
            node: <ColorSwatches value={project.color} onChange={(c) => setColor(project.id, c)} />
          }
        ]
      },
      {
        kind: 'submenu',
        label: 'Rename…',
        items: [
          { kind: 'custom', node: <ProjectRename project={project} onDone={ui.closeContextMenu} /> }
        ]
      },
      { kind: 'sep' },
      { kind: 'item', label: 'Remove', danger: true, onSelect: () => remove(project.id) }
    ];
    ui.openContextMenu(e.clientX, e.clientY, items);
  };

  return (
    <aside className="hp-sidebar" ref={railRef}>
      {/* Quick pane: click opens a default pane, Shift-click opens the New Pane
          dialog (moved here out of the top bar). */}
      <button
        className="hp-sidebar-icon"
        onClick={(e) => (e.shiftKey ? openNewPane() : addPane())}
        title="New pane — Shift-click for options"
        aria-label="New pane"
      >
        <IconPlus />
      </button>

      {/* Projects history: the folder icon toggles the flyout. */}
      <button
        className={`hp-sidebar-icon${projectsOpen ? ' active' : ''}`}
        onClick={() => setProjectsOpen((o) => !o)}
        title="Projects"
        aria-label="Projects"
        aria-expanded={projectsOpen}
      >
        <IconFolder />
        {projects.length > 0 && <span className="hp-sidebar-badge">{projects.length}</span>}
      </button>

      {projectsOpen && (
        <div className="hp-sidebar-flyout">
          <div className="hp-flyout-head">Projects</div>
          <div className="hp-flyout-list">
            {projects.length === 0 ? (
              <p className="hp-sidebar-empty">
                Projects you cd into show up here, each with its own color.
              </p>
            ) : (
              projects.map((p) => (
                <button
                  key={p.id}
                  className="hp-project"
                  title={p.path}
                  onClick={() => {
                    openInPane(p);
                    setProjectsOpen(false);
                  }}
                  onContextMenu={(e) => openMenu(e, p)}
                >
                  <span className="hp-project-dot" style={{ background: p.color }} />
                  <span className="hp-project-name">{p.name}</span>
                </button>
              ))
            )}
          </div>
        </div>
      )}
    </aside>
  );
}
