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

const isWin = process.platform === 'win32';

// Canonical absolute path for storage: strip trailing separators; on Windows
// normalize slashes to backslashes and uppercase the drive letter, so the SAME
// repo reported by different shells (cmd's c:\…, pwsh's C:\…, git-bash's /c/… via
// fileUriToPath) stores identically instead of as separate projects.
export function canonicalPath(p: string): string {
  let s = p.replace(/[\\/]+$/, '');
  if (isWin) {
    s = s.replace(/\//g, '\\').replace(/^([a-z]):/, (_m, d) => `${d.toUpperCase()}:`);
  }
  return s;
}

// Dedup key — case-insensitive on Windows (its paths ignore case), so the same
// directory never lands as two projects.
const pathKey = (p: string): string => {
  const c = canonicalPath(p);
  return isWin ? c.toLowerCase() : c;
};

function colorForPath(p: string): string {
  const key = pathKey(p);
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) >>> 0;
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

function storePath(): string {
  return join(app.getPath('userData'), 'projects.json');
}

// In-memory cache so reads don't hit disk on every cwd change; writes refresh it.
let cache: Project[] | null = null;

// Collapse entries pointing at the same directory (case-insensitively on Windows),
// keeping the most-recently-opened, and canonicalize each stored path. Self-heals
// duplicates saved before paths were canonicalized.
function dedupe(list: Project[]): Project[] {
  const byKey = new Map<string, Project>();
  for (const proj of list) {
    const canon = { ...proj, path: canonicalPath(proj.path) };
    const k = pathKey(canon.path);
    const prev = byKey.get(k);
    if (!prev || (canon.lastOpenedAt ?? 0) >= (prev.lastOpenedAt ?? 0)) byKey.set(k, canon);
  }
  return [...byKey.values()];
}

function load(): Project[] {
  if (cache) return cache;
  try {
    const data = JSON.parse(readFileSync(storePath(), 'utf8'));
    const raw = Array.isArray(data?.projects) ? (data.projects as Project[]) : [];
    const deduped = dedupe(raw);
    cache = deduped;
    if (deduped.length !== raw.length) save(deduped); // persist the cleanup
  } catch {
    cache = []; // missing/corrupt file → start empty
  }
  return cache ?? [];
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
  const path = canonicalPath(root);
  const key = pathKey(path);
  const list = load();
  const repo = gitRepoName(path);
  const existing = list.find((p) => pathKey(p.path) === key);
  if (existing) {
    existing.lastOpenedAt = Date.now();
    existing.path = path; // canonicalize a path stored before this normalization
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
