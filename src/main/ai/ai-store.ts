import { dirname } from 'node:path';
import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';

// On-disk memory for the ambient-AI feature: per-project rolling summaries +
// a timeline, and per-pane records. Mirrors the persistence shape of
// src/main/projects.ts — an in-memory cache fronting a debounced, ATOMIC write
// (write a temp file, then rename over the target) so a crash mid-write can
// never corrupt an existing good file. Unlike projects.ts this is path-injected
// (no electron `app` import) so it can be unit-tested against a tmp file.

// A single dated note on a project's timeline.
export interface TimelineEntry {
  ts: number;
  kind: 'milestone' | 'note' | 'error';
  text: string;
}

// A project's rolling memory: a summary that gets rewritten over time plus a
// capped FIFO timeline of notable events.
export interface ProjectMemory {
  path: string;
  name: string;
  summary: string;
  summaryUpdatedAt: number;
  timeline: TimelineEntry[]; // FIFO, capped at 200 (drop oldest)
}

// A pane's last-known state and rolling summary.
export interface PaneMemory {
  paneId: string;
  projectPath: string | null;
  label: string;
  subtitle: string;
  summary: string;
  lastCwd: string;
  lastCommand: string | null;
  updatedAt: number;
}

// The whole persisted document. `version` lets future migrations branch.
export interface AiMemoryFile {
  version: 1;
  projects: Record<string, ProjectMemory>; // keyed by caller-supplied path
  panes: Record<string, PaneMemory>; // keyed by paneId
}

const TIMELINE_CAP = 200;
const DEBOUNCE_MS = 2000;

function emptyFile(): AiMemoryFile {
  return { version: 1, projects: {}, panes: {} };
}

export class AiMemoryStore {
  private readonly filePath: string;
  private data: AiMemoryFile = emptyFile();
  private timer: ReturnType<typeof setTimeout> | null = null;

  constructor(filePath: string) {
    this.filePath = filePath;
  }

  // Read the file into the in-memory cache. A missing OR corrupt/unparseable
  // file is tolerated — we log and start empty, never throwing out of load().
  load(): void {
    try {
      const parsed = JSON.parse(readFileSync(this.filePath, 'utf8')) as Partial<AiMemoryFile>;
      this.data = {
        version: 1,
        projects: parsed?.projects ?? {},
        panes: parsed?.panes ?? {}
      };
    } catch {
      this.data = emptyFile(); // missing/corrupt → start empty
    }
  }

  getProject(path: string): ProjectMemory | undefined {
    return this.data.projects[path];
  }

  // Create-or-update a project record, shallow-merging the patch over any
  // existing record. Stamps summaryUpdatedAt whenever the summary is touched.
  // Stores by the caller-supplied key verbatim (no path canonicalization).
  upsertProject(
    path: string,
    patch: Partial<Omit<ProjectMemory, 'path'>> & { name?: string }
  ): ProjectMemory {
    const existing = this.data.projects[path];
    const base: ProjectMemory = existing ?? {
      path,
      name: '',
      summary: '',
      summaryUpdatedAt: 0,
      timeline: []
    };
    const merged: ProjectMemory = { ...base, ...patch, path };
    if ('summary' in patch) merged.summaryUpdatedAt = Date.now();
    this.data.projects[path] = merged;
    this.scheduleWrite();
    return merged;
  }

  // Push an entry onto a project's timeline (creating the project if absent),
  // trimming to the most-recent TIMELINE_CAP entries (drop oldest).
  appendTimeline(path: string, entry: TimelineEntry): void {
    const project = this.data.projects[path] ?? this.upsertProject(path, {});
    project.timeline.push(entry);
    if (project.timeline.length > TIMELINE_CAP) {
      project.timeline = project.timeline.slice(-TIMELINE_CAP);
    }
    this.scheduleWrite();
  }

  getPane(paneId: string): PaneMemory | undefined {
    return this.data.panes[paneId];
  }

  // Create-or-update a pane record, shallow-merging the patch and bumping
  // updatedAt. Stores by the caller-supplied paneId verbatim.
  upsertPane(paneId: string, patch: Partial<Omit<PaneMemory, 'paneId'>>): PaneMemory {
    const existing = this.data.panes[paneId];
    const base: PaneMemory = existing ?? {
      paneId,
      projectPath: null,
      label: '',
      subtitle: '',
      summary: '',
      lastCwd: '',
      lastCommand: null,
      updatedAt: 0
    };
    const merged: PaneMemory = { ...base, ...patch, paneId, updatedAt: Date.now() };
    this.data.panes[paneId] = merged;
    this.scheduleWrite();
    return merged;
  }

  prunePane(paneId: string): void {
    if (this.data.panes[paneId]) {
      delete this.data.panes[paneId];
      this.scheduleWrite();
    }
  }

  // Drop every pane whose id is not in the keep-list (e.g. panes that no longer
  // exist after a session restore).
  prunePanesExcept(keepPaneIds: string[]): void {
    const keep = new Set(keepPaneIds);
    let changed = false;
    for (const id of Object.keys(this.data.panes)) {
      if (!keep.has(id)) {
        delete this.data.panes[id];
        changed = true;
      }
    }
    if (changed) this.scheduleWrite();
  }

  // Force an immediate synchronous write now, cancelling any pending debounce
  // (used on before-quit).
  flush(): void {
    if (this.timer) {
      clearTimeout(this.timer);
      this.timer = null;
    }
    this.writeNow();
  }

  // Coalesce rapid mutations into a single deferred write.
  private scheduleWrite(): void {
    if (this.timer) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.writeNow();
    }, DEBOUNCE_MS);
    // Don't keep the process alive just for a pending flush.
    if (typeof this.timer === 'object' && typeof this.timer.unref === 'function') {
      this.timer.unref();
    }
  }

  // Atomic write: serialize to a temp sibling, then rename over the target so a
  // reader never sees a half-written file.
  private writeNow(): void {
    const tmp = `${this.filePath}.tmp`;
    try {
      mkdirSync(dirname(this.filePath), { recursive: true });
      writeFileSync(tmp, JSON.stringify(this.data, null, 2), 'utf8');
      renameSync(tmp, this.filePath);
    } catch (err) {
      console.error('failed to write ai-memory.json', err);
    }
  }
}
