import { describe, expect, it, vi } from 'vitest';
import { useProjects } from './useProjects';
import { useWorkspace } from './useWorkspace';
import type { Project } from '../types';

const project: Project = {
  id: 'p1',
  path: 'C:/code/myrepo',
  name: 'myrepo',
  color: '#30a46c',
  lastOpenedAt: 123
};

describe('useProjects.openInPane', () => {
  it('opens a new pane cd\'d into the repo, tinted with the project color and titled by name', () => {
    const spy = vi.spyOn(useWorkspace.getState(), 'addPane');
    useProjects.getState().openInPane(project);
    expect(spy).toHaveBeenCalledWith({
      cwd: 'C:/code/myrepo',
      color: '#30a46c',
      showFrame: true,
      showDot: true,
      label: 'myrepo'
    });
    spy.mockRestore();
  });
});
