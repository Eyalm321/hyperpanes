// Facade that ties the ambient-AI feature together in the main process. It owns
// the four building blocks (redactor, pane-buffer, ollama-client, ai-store) plus
// the SummaryScheduler, holds the uid→pane context map fed by the renderer
// (ai:publishPanes), and implements the actual summarize job the scheduler runs.
//
// Independent of the control server: the output tap (ipc onData/onCwd/onExit)
// feeds it directly, and it writes the generated line back through an injected
// pushMeta (which ipc routes to the owning window via the shared dispatch).

import { basename } from 'node:path';
import { readFileSync, writeFileSync } from 'node:fs';
import { PaneTailBuffer } from './pane-buffer';
import { OllamaClient } from './ollama-client';
import { AiMemoryStore } from './ai-store';
import { redact } from './redactor';
import { SummaryScheduler, type JobResult } from './scheduler';
import type { AiStatus, AiPanePublish } from '../../renderer/types';

export type { AiPanePublish };

const SYSTEM_PROMPT =
  'You label what a developer is doing in one terse present-tense phrase ' +
  '(max 8 words). No trailing punctuation. Never include secrets, paths, or code. ' +
  "If unclear, answer 'working'.";

// Persisted config (ai-settings.json). Master enable is OFF by default.
export interface AiSettings {
  enabled: boolean;
  endpoint: string;
  model: string;
  settleMs: number;
  maxStalenessSec: number;
  concurrency: number;
}

const DEFAULTS: AiSettings = {
  enabled: false,
  endpoint: 'http://localhost:11434',
  model: 'gemma3:4b',
  settleMs: 1500,
  maxStalenessSec: 180,
  concurrency: 1
};

// A minimal project descriptor passed from the cwd tap (already canonicalized
// upstream by projects.ts). AiStatus + AiPanePublish are shared types (see
// renderer/types.ts) so preload/renderer agree on the shape.
export interface AiProjectRef {
  path: string;
  name: string;
}

interface PaneCtx {
  paneId: string;
  windowId: number;
  label: string;
  cwd: string;
  projectPath: string | null;
  projectName: string;
  muted: boolean;
}

export interface AiServiceOpts {
  settingsPath: string;
  memoryPath: string;
  pushMeta: (windowId: number, paneId: string, meta: Record<string, string>) => void;
  onStatus: (status: AiStatus) => void;
}

// Cheap, stable content fingerprint so we don't re-summarize unchanged output.
function fingerprint(text: string): string {
  let h = 5381;
  for (let i = 0; i < text.length; i++) h = ((h << 5) + h + text.charCodeAt(i)) | 0;
  return `${text.length}:${h}`;
}

export class AiService {
  private readonly settingsPath: string;
  private readonly pushMeta: (windowId: number, paneId: string, meta: Record<string, string>) => void;
  private readonly emit: (status: AiStatus) => void;

  private readonly buffer = new PaneTailBuffer();
  private readonly client = new OllamaClient({ endpoint: DEFAULTS.endpoint, model: DEFAULTS.model });
  private readonly store: AiMemoryStore;
  private readonly scheduler: SummaryScheduler;

  private readonly ctxByUid = new Map<string, PaneCtx>();
  private readonly lastHash = new Map<string, string>();
  // paneIds each window currently publishes — union drives store pruning so one
  // window's publish never prunes another window's pane records.
  private readonly publishedByWindow = new Map<number, Set<string>>();

  private settings: AiSettings = { ...DEFAULTS };
  private online = false;
  private lastError: string | undefined;

  constructor(opts: AiServiceOpts) {
    this.settingsPath = opts.settingsPath;
    this.pushMeta = opts.pushMeta;
    this.emit = opts.onStatus;
    this.store = new AiMemoryStore(opts.memoryPath);
    this.scheduler = new SummaryScheduler(
      {
        settleMs: DEFAULTS.settleMs,
        maxStalenessSec: DEFAULTS.maxStalenessSec,
        concurrency: DEFAULTS.concurrency
      },
      {
        runJob: (uid) => this.runJob(uid),
        onStatus: (online, err) => {
          this.online = online;
          if (err) this.lastError = err;
          this.emitStatus();
        }
      }
    );
  }

  // Load persisted settings + memory and start if it was left enabled.
  init(): void {
    this.settings = this.loadSettings();
    this.store.load();
    this.client.configure({ endpoint: this.settings.endpoint, model: this.settings.model });
    this.scheduler.setConfig(this.schedConfig());
    if (this.settings.enabled) {
      this.scheduler.start();
      void this.refreshStatus();
    }
    this.emitStatus();
  }

  get enabled(): boolean {
    return this.settings.enabled;
  }

  status(): AiStatus {
    return {
      enabled: this.settings.enabled,
      online: this.online,
      endpoint: this.settings.endpoint,
      model: this.settings.model,
      lastError: this.lastError
    };
  }

  setEnabled(enabled: boolean): void {
    if (this.settings.enabled === enabled) return;
    this.settings.enabled = enabled;
    this.saveSettings();
    if (enabled) {
      this.scheduler.start();
      void this.refreshStatus();
    } else {
      this.scheduler.stop();
      this.online = false;
    }
    this.emitStatus();
  }

  // Live-update endpoint/model/cadence from Preferences.
  configure(patch: Partial<Omit<AiSettings, 'enabled'>>): void {
    this.settings = { ...this.settings, ...patch };
    this.saveSettings();
    this.client.configure({ endpoint: this.settings.endpoint, model: this.settings.model });
    this.scheduler.setConfig(this.schedConfig());
    if (this.settings.enabled) void this.refreshStatus();
    this.emitStatus();
  }

  // ---- taps from ipc (session output) ----
  onData(uid: string, data: string): void {
    if (!this.settings.enabled) return;
    const ctx = this.ctxByUid.get(uid);
    if (!ctx || ctx.muted) return;
    this.buffer.append(uid, data);
    this.scheduler.noteOutput(uid);
  }

  onCwd(uid: string, cwd: string, project: AiProjectRef | null): void {
    const ctx = this.ctxByUid.get(uid);
    if (!ctx) return;
    ctx.cwd = cwd;
    if (project) {
      ctx.projectPath = project.path;
      ctx.projectName = project.name;
    }
  }

  onSessionExit(uid: string): void {
    this.buffer.clear(uid);
    this.scheduler.forget(uid);
    this.ctxByUid.delete(uid);
    this.lastHash.delete(uid);
  }

  // A window publishes its live set of watched panes (paneId↔sessionUid + label
  // + mute). windowId comes from ipc (the sending webContents). We reconcile our
  // context map for THAT window only, then prune the store against the union of
  // all windows' published panes.
  onPaneContext(windowId: number, panes: AiPanePublish[]): void {
    const seen = new Set<string>();
    for (const p of panes) {
      seen.add(p.sessionUid);
      const prev = this.ctxByUid.get(p.sessionUid);
      const wasMuted = prev?.muted ?? false;
      this.ctxByUid.set(p.sessionUid, {
        paneId: p.paneId,
        windowId,
        label: p.label,
        cwd: prev?.cwd ?? '',
        projectPath: prev?.projectPath ?? null,
        projectName: prev?.projectName ?? '',
        muted: p.muted
      });
      if (p.muted && !wasMuted) {
        this.pushMeta(windowId, p.paneId, { 'ai.subtitle': '' });
        this.scheduler.forget(p.sessionUid);
        this.lastHash.delete(p.sessionUid);
      }
    }
    // Drop context for this window's panes that are no longer published.
    for (const [uid, ctx] of this.ctxByUid) {
      if (ctx.windowId === windowId && !seen.has(uid)) {
        this.ctxByUid.delete(uid);
        this.buffer.clear(uid);
        this.scheduler.forget(uid);
        this.lastHash.delete(uid);
      }
    }
    this.publishedByWindow.set(windowId, new Set(panes.map((p) => p.paneId)));
    this.prunePanes();
  }

  // A window closed — forget its panes and re-prune.
  dropWindow(windowId: number): void {
    this.publishedByWindow.delete(windowId);
    for (const [uid, ctx] of this.ctxByUid) {
      if (ctx.windowId === windowId) {
        this.ctxByUid.delete(uid);
        this.buffer.clear(uid);
        this.scheduler.forget(uid);
        this.lastHash.delete(uid);
      }
    }
    this.prunePanes();
  }

  // Prune store pane records to the union of all windows' published panes. Only
  // prunes once at least one window has published, so an early/empty state can't
  // wipe persisted memory.
  private prunePanes(): void {
    const all = new Set<string>();
    for (const set of this.publishedByWindow.values()) for (const id of set) all.add(id);
    if (all.size > 0) this.store.prunePanesExcept([...all]);
  }

  shutdown(): void {
    this.scheduler.stop();
    this.store.flush();
  }

  // ---- the job the scheduler runs ----
  private async runJob(uid: string): Promise<JobResult> {
    if (!this.settings.enabled) return 'skip';
    const ctx = this.ctxByUid.get(uid);
    if (!ctx || ctx.muted) return 'skip';
    const snap = this.buffer.snapshot(uid);
    if (snap.altScreen) return 'skip'; // full-screen TUI: raw tail is redraw noise
    const text = snap.text.trim();
    if (text.length < 3) return 'skip';
    const hash = fingerprint(text);
    if (this.lastHash.get(uid) === hash) return 'skip'; // nothing new since last summary

    const prior = this.store.getPane(ctx.paneId)?.summary ?? '';
    const prompt = this.buildPrompt(ctx, prior, redact(snap.text));

    let line: string;
    try {
      line = await this.client.summarize({ system: SYSTEM_PROMPT, prompt });
    } catch (err) {
      this.lastError = err instanceof Error ? err.message : String(err);
      return 'fail';
    }

    line = redact(line);
    this.lastHash.set(uid, hash);
    this.pushMeta(ctx.windowId, ctx.paneId, { 'ai.subtitle': line });
    this.store.upsertPane(ctx.paneId, {
      projectPath: ctx.projectPath,
      label: ctx.label,
      summary: line,
      lastCwd: ctx.cwd
    });
    if (ctx.projectPath) {
      this.store.upsertProject(ctx.projectPath, { name: ctx.projectName, summary: line });
    }
    return 'ok';
  }

  private buildPrompt(ctx: PaneCtx, prior: string, redactedTail: string): string {
    const lines: string[] = [];
    if (ctx.label) lines.push(`Pane label: ${ctx.label}`);
    if (ctx.cwd) lines.push(`Directory: ${basename(ctx.cwd)}`);
    if (prior) lines.push(`Previous summary: ${prior}`);
    lines.push('', 'Recent terminal output:', redactedTail);
    return lines.join('\n');
  }

  private schedConfig() {
    return {
      settleMs: this.settings.settleMs,
      maxStalenessSec: this.settings.maxStalenessSec,
      concurrency: this.settings.concurrency
    };
  }

  private async refreshStatus(): Promise<void> {
    const ok = await this.client.ping();
    this.online = ok;
    if (ok) this.lastError = undefined;
    this.emitStatus();
  }

  private emitStatus(): void {
    this.emit(this.status());
  }

  private loadSettings(): AiSettings {
    try {
      const parsed = JSON.parse(readFileSync(this.settingsPath, 'utf8')) as Partial<AiSettings>;
      return { ...DEFAULTS, ...parsed };
    } catch {
      return { ...DEFAULTS };
    }
  }

  private saveSettings(): void {
    try {
      writeFileSync(this.settingsPath, JSON.stringify(this.settings, null, 2), 'utf8');
    } catch (err) {
      console.error('failed to write ai-settings.json', err);
    }
  }
}
