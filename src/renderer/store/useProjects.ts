import { create } from 'zustand';
import type { Project } from '../types';
import { useWorkspace } from './useWorkspace';

// The preload exposes window.hp.projects (src/preload/index.ts) but the renderer's
// HpApi type lives in global.d.ts (a seam shared with other tracks). Add the
// projects surface here via interface-merging so this feature is self-contained
// and doesn't edit the shared declaration file. Mirrors the preload exactly.
declare global {
  interface HpApi {
    projects: {
      list(): Promise<Project[]>;
      setColor(id: string, color: string): Promise<Project[]>;
      rename(id: string, name: string): Promise<Project[]>;
      remove(id: string): Promise<Project[]>;
      onChanged(cb: (list: Project[]) => void): () => void;
      onPaneProject(cb: (uid: string, project: Project) => void): () => void;
    };
  }
}

// Renderer-side mirror of main's git-projects history (src/main/projects.ts). The
// list is owned by main (persisted to projects.json); this store keeps a live copy
// for the sidebar by loading projects.list() on init and re-syncing on every
// projects:changed event. Mutations are forwarded to main, which echoes back the
// new list through onChanged — so the store never edits its own copy directly.
interface ProjectsState {
  projects: Project[];
  setColor: (id: string, color: string) => void;
  rename: (id: string, name: string) => void;
  remove: (id: string) => void;
  // Open a project in a brand-new pane cd'd into its repo and tinted its color.
  openInPane: (project: Project) => void;
}

export const useProjects = create<ProjectsState>(() => ({
  projects: [],
  setColor: (id, color) => void window.hp?.projects.setColor(id, color),
  rename: (id, name) => void window.hp?.projects.rename(id, name),
  remove: (id) => void window.hp?.projects.remove(id),
  openInPane: (project) =>
    useWorkspace.getState().addPane({
      cwd: project.path,
      color: project.color,
      showFrame: true,
      showDot: true,
      label: project.name
    })
}));

// Wire the store to main once (guarded so non-Electron contexts like tests are a
// no-op). Initial load + live updates both land in the same setter.
if (typeof window !== 'undefined' && window.hp?.projects) {
  const apply = (list: Project[]) => useProjects.setState({ projects: list });
  void window.hp.projects.list().then(apply);
  window.hp.projects.onChanged(apply);
}
