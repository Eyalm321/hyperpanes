import { describe, it, expect, beforeEach, afterEach } from 'vitest';
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { AiMemoryStore, type TimelineEntry } from './ai-store';

// Each test gets its own tmp dir + file path so the store is exercised against a
// real (but disposable) on-disk file — never electron's userData.
let dir: string;
let file: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), 'ai-store-'));
  file = join(dir, 'ai-memory.json');
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe('AiMemoryStore.load', () => {
  it('starts empty when the file is missing', () => {
    const store = new AiMemoryStore(file);
    store.load();
    expect(store.getProject('/x')).toBeUndefined();
    expect(store.getPane('p1')).toBeUndefined();
  });

  it('tolerates a corrupt file and starts empty (never throws)', () => {
    writeFileSync(file, '{ this is not: valid json ]]', 'utf8');
    const store = new AiMemoryStore(file);
    expect(() => store.load()).not.toThrow();
    expect(store.getProject('/x')).toBeUndefined();
  });
});

describe('AiMemoryStore projects', () => {
  it('upserts then gets a project (roundtrip)', () => {
    const store = new AiMemoryStore(file);
    store.load();
    const p = store.upsertProject('/repo', { name: 'repo', summary: 'hi' });
    expect(p.path).toBe('/repo');
    expect(p.name).toBe('repo');
    expect(p.summary).toBe('hi');
    expect(store.getProject('/repo')).toEqual(p);
  });

  it('shallow-merges patches and stamps summaryUpdatedAt on summary change', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertProject('/repo', { name: 'repo', summary: 'first' });
    const before = store.getProject('/repo')!.summaryUpdatedAt;
    const merged = store.upsertProject('/repo', { summary: 'second' });
    expect(merged.name).toBe('repo'); // preserved by shallow merge
    expect(merged.summary).toBe('second');
    expect(merged.summaryUpdatedAt).toBeGreaterThanOrEqual(before);
  });

  it('stores by the caller-supplied key without canonicalizing', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertProject('c:\\Repo', { name: 'a' });
    store.upsertProject('C:\\repo', { name: 'b' });
    // Different keys -> two distinct records (no canonicalization).
    expect(store.getProject('c:\\Repo')!.name).toBe('a');
    expect(store.getProject('C:\\repo')!.name).toBe('b');
  });

  it('initializes a new project with sane defaults', () => {
    const store = new AiMemoryStore(file);
    store.load();
    const p = store.upsertProject('/repo', {});
    expect(p.name).toBe('');
    expect(p.summary).toBe('');
    expect(Array.isArray(p.timeline)).toBe(true);
    expect(p.timeline).toHaveLength(0);
  });
});

describe('AiMemoryStore.appendTimeline', () => {
  it('appends entries onto a project, creating it if absent', () => {
    const store = new AiMemoryStore(file);
    store.load();
    const e: TimelineEntry = { ts: 1, kind: 'note', text: 'hello' };
    store.appendTimeline('/repo', e);
    expect(store.getProject('/repo')!.timeline).toEqual([e]);
  });

  it('caps the timeline at 200 entries, dropping oldest (FIFO)', () => {
    const store = new AiMemoryStore(file);
    store.load();
    for (let i = 0; i < 250; i++) {
      store.appendTimeline('/repo', { ts: i, kind: 'note', text: `e${i}` });
    }
    const tl = store.getProject('/repo')!.timeline;
    expect(tl).toHaveLength(200);
    expect(tl[0].ts).toBe(50); // oldest 50 dropped
    expect(tl[199].ts).toBe(249);
  });
});

describe('AiMemoryStore panes', () => {
  it('upserts then gets a pane (roundtrip)', () => {
    const store = new AiMemoryStore(file);
    store.load();
    const pane = store.upsertPane('p1', { label: 'shell', lastCwd: '/tmp' });
    expect(pane.paneId).toBe('p1');
    expect(pane.label).toBe('shell');
    expect(pane.lastCwd).toBe('/tmp');
    expect(store.getPane('p1')).toEqual(pane);
  });

  it('shallow-merges pane patches and bumps updatedAt', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertPane('p1', { label: 'shell', lastCommand: 'ls' });
    const before = store.getPane('p1')!.updatedAt;
    const merged = store.upsertPane('p1', { lastCommand: 'pwd' });
    expect(merged.label).toBe('shell'); // preserved
    expect(merged.lastCommand).toBe('pwd');
    expect(merged.updatedAt).toBeGreaterThanOrEqual(before);
  });

  it('initializes a new pane with sane defaults', () => {
    const store = new AiMemoryStore(file);
    store.load();
    const pane = store.upsertPane('p1', {});
    expect(pane.projectPath).toBeNull();
    expect(pane.label).toBe('');
    expect(pane.subtitle).toBe('');
    expect(pane.summary).toBe('');
    expect(pane.lastCwd).toBe('');
    expect(pane.lastCommand).toBeNull();
  });

  it('prunePane removes a single pane', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertPane('p1', {});
    store.upsertPane('p2', {});
    store.prunePane('p1');
    expect(store.getPane('p1')).toBeUndefined();
    expect(store.getPane('p2')).toBeDefined();
  });

  it('prunePanesExcept keeps only the listed panes', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertPane('p1', {});
    store.upsertPane('p2', {});
    store.upsertPane('p3', {});
    store.prunePanesExcept(['p2']);
    expect(store.getPane('p1')).toBeUndefined();
    expect(store.getPane('p2')).toBeDefined();
    expect(store.getPane('p3')).toBeUndefined();
  });

  it('prunePanesExcept with an empty keep-list removes all panes', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertPane('p1', {});
    store.upsertPane('p2', {});
    store.prunePanesExcept([]);
    expect(store.getPane('p1')).toBeUndefined();
    expect(store.getPane('p2')).toBeUndefined();
  });
});

describe('AiMemoryStore persistence', () => {
  it('flush writes valid JSON and a fresh store loads it back', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertProject('/repo', { name: 'repo', summary: 's' });
    store.appendTimeline('/repo', { ts: 1, kind: 'milestone', text: 'm' });
    store.upsertPane('p1', { label: 'shell' });
    store.flush();

    expect(existsSync(file)).toBe(true);
    const parsed = JSON.parse(readFileSync(file, 'utf8'));
    expect(parsed.version).toBe(1);

    const reloaded = new AiMemoryStore(file);
    reloaded.load();
    expect(reloaded.getProject('/repo')!.name).toBe('repo');
    expect(reloaded.getProject('/repo')!.timeline).toHaveLength(1);
    expect(reloaded.getPane('p1')!.label).toBe('shell');
  });

  it('flush can be called with no pending changes without error', () => {
    const store = new AiMemoryStore(file);
    store.load();
    expect(() => store.flush()).not.toThrow();
  });

  it('does not leave a temp file behind after an atomic write', () => {
    const store = new AiMemoryStore(file);
    store.load();
    store.upsertProject('/repo', { name: 'repo' });
    store.flush();
    expect(existsSync(`${file}.tmp`)).toBe(false);
  });
});
