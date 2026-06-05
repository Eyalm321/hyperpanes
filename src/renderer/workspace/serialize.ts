import { activeGroup, specFromGroup, useWorkspace } from '../store/useWorkspace';
import type { WorkspaceFile } from '../types';

// Snapshot the active tab into the persistable file shape (pane identity +
// layout; runtime-only fields like sessionUid/status are dropped). Used by the
// single-file Save/Open feature.
export function serializeWorkspace(): WorkspaceFile {
  const g = activeGroup(useWorkspace.getState());
  const spec = specFromGroup(g);
  return { name: g.title, layout: g.layout, panes: spec.panes };
}

// Snapshot the whole session — every tab plus the active index — for autosave/
// restore. The top-level fields mirror the active tab for back-compat.
export function serializeSession(): WorkspaceFile {
  const s = useWorkspace.getState();
  const active = activeGroup(s);
  const spec = specFromGroup(active);
  return {
    name: active.title,
    layout: active.layout,
    panes: spec.panes,
    groups: s.groups.map(specFromGroup),
    active: s.groups.findIndex((g) => g.id === s.activeId)
  };
}
