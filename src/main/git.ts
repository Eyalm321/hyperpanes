import { existsSync } from 'node:fs';
import { dirname, join, parse } from 'node:path';

// Walk up from `dir` looking for a `.git` entry; return the repo root or null.
// Cheap (a filesystem check per level, no `git` spawn) and synchronous, so it's
// safe to call on every cwd change. Handles a `.git` file (worktrees/submodules)
// as well as a directory, since existsSync matches both.
export function findGitRoot(dir: string): string | null {
  if (!dir) return null;
  let cur = dir;
  const root = parse(cur).root;
  for (;;) {
    try {
      if (existsSync(join(cur, '.git'))) return cur;
    } catch {
      /* unreadable level — keep walking up */
    }
    if (cur === root) break;
    const parent = dirname(cur);
    if (parent === cur) break;
    cur = parent;
  }
  return null;
}
