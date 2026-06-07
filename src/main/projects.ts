import { app } from 'electron';
import { basename, join } from 'node:path';
import { readFileSync, statSync, writeFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';

// A git repo the app remembers from a pane cd-ing into it (sidebar projects
// history). Persisted to projects.json in userData. Each project gets its own
// frame/dot color and is titled by the repo folder name. Kept structurally
// identical to the renderer-side `Project` in src/renderer/types.ts.
export interface Project {
  id: string;
  path: string; // normalized git-root absolute path
  name: string; // basename of the git root (the title)
  color: string; // frame/dot color for panes in this project
  lastOpenedAt?: number; // epoch ms, for recency sorting
}

// A stable per-repo color: hash the path into the shared 8-slot palette so a repo
// keeps the same color across restarts regardless of detection order.
const PROJECT_COLORS = [
  '#e5484d',
  '#f5a623',
  '#30a46c',
  '#3b82f6',
  '#a855f7',
  '#ec4899',
  '#14b8a6',
  '#eab308'
];

function colorForPath(p: string): string {
  let h = 0;
  for (let i = 0; i < p.length; i++) h = (h * 31 + p.charCodeAt(i)) >>> 0;
  return PROJECT_COLORS[h % PROJECT_COLORS.length];
}

// Parse the repository name out of a remote URL (any host), so a clone checked out
// into a differently-named folder still shows its REAL repo name:
//   https://github.com/owner/my-repo.git  → my-repo
//   git@github.com:owner/my-repo.git       → my-repo
//   ssh://git@github.com/owner/My.Repo.git → My.Repo
export function repoNameFromUrl(url: string): string | null {
  const u = url
    .trim()
    .replace(/\.git$/i, '')
    .replace(/[\\/]+$/, '');
  if (!u) return null;
  const parts = u.split(/[\\/:]/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : null;
}

// The repo's name from its `origin` remote (e.g. the GitHub repo name), read
// straight from .git/config — no `git` spawn. Returns null when there's no plain
// .git directory (worktree/submodule pointer) or no origin url, and the caller
// falls back to the folder name.
function gitRepoName(gitRoot: string): string | null {
  try {
    const dotGit = join(gitRoot, '.git');
    if (!statSync(dotGit).isDirectory()) return null;
    const cfg = readFileSync(join(dotGit, 'config'), 'utf8');
    const m = cfg.match(/\[remote "origin"\][^[]*?\burl\s*=\s*(.+)/);
    return m ? repoNameFromUrl(m[1]) : null;
  } catch {
    return null;
  }
}

const norm = (p: string): string => p.replace(/[\\/]+$/, '');

function storePath(): string {
  return join(app.getPath('userData'), 'projects.json');
}

// In-memory cache so reads don't hit disk on every cwd change; writes refresh it.
let cache: Project[] | null = null;

function load(): Project[] {
  if (cache) return cache;
  try {
    const data = JSON.parse(readFileSync(storePath(), 'utf8'));
    cache = Array.isArray(data?.projects) ? (data.projects as Project[]) : [];
  } catch {
    cache = []; // missing/corrupt file → start empty
  }
  return cache;
}

function save(list: Project[]): void {
  cache = list;
  try {
    writeFileSync(storePath(), JSON.stringify({ projects: list }, null, 2), 'utf8');
  } catch (err) {
    console.error('failed to write projects.json', err);
  }
}

// Newest-first by last-opened; the sidebar renders in this order.
export function listProjects(): Project[] {
  return [...load()].sort((a, b) => (b.lastOpenedAt ?? 0) - (a.lastOpenedAt ?? 0));
}

// Remember a git root (or bump its recency if already known); returns the project.
export function upsertProjectByRoot(root: string): Project {
  const path = norm(root);
  const list = load();
  const repo = gitRepoName(path);
  const existing = list.find((p) => norm(p.path) === path);
  if (existing) {
    existing.lastOpenedAt = Date.now();
    // Heal an entry saved under the old folder-name logic to the real repo name —
    // but never clobber a name the user deliberately changed.
    if (repo && existing.name === basename(path) && existing.name !== repo) {
      existing.name = repo;
    }
    save(list);
    return existing;
  }
  const project: Project = {
    id: randomUUID(),
    path,
    name: repo || basename(path) || path,
    color: colorForPath(path),
    lastOpenedAt: Date.now()
  };
  save([...list, project]);
  return project;
}

export function setProjectColor(id: string, color: string): void {
  const list = load();
  const p = list.find((x) => x.id === id);
  if (p) {
    p.color = color;
    save(list);
  }
}

export function renameProject(id: string, name: string): void {
  const list = load();
  const p = list.find((x) => x.id === id);
  if (p) {
    p.name = name;
    save(list);
  }
}

export function removeProject(id: string): void {
  save(load().filter((x) => x.id !== id));
}
