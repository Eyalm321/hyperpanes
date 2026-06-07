import type {
  AiConfigPatch,
  AiPanePublish,
  AiStatus,
  ControlCommand,
  ControlStatus,
  ControlWindowPayload,
  GroupPayload,
  GroupSpec,
  MetricsSnapshot,
  WindowSpec,
  WorkspaceFile
} from './types';

export {};

declare global {
  interface HpSpawnOptions {
    uid: string;
    paneId?: string; // injected into the pty env as HYPERPANES_PANE_ID (pane self-awareness)
    shell?: string;
    command?: string;
    args?: string[]; // direct-spawn argv (with command): verbatim, no shell re-parse (P4a)
    cwd?: string;
    env?: Record<string, string>; // extra pty env (e.g. a scoped control token, agent-orchestration F)
    cols?: number;
    rows?: number;
  }

  interface HpPathResolveResult {
    token: string;
    absPath: string;
    exists: boolean;
    isDir: boolean;
    isExe: boolean;
  }

  interface HpApi {
    platform: string;
    spawn(opts: HpSpawnOptions): Promise<{ uid: string; attached: boolean; replay?: string }>;
    write(uid: string, data: string): void;
    resize(uid: string, cols: number, rows: number): void;
    kill(uid: string): void;
    metrics(): Promise<MetricsSnapshot>;
    paths: {
      resolve(cwd: string | undefined, tokens: string[]): Promise<HpPathResolveResult[]>;
      open(
        absPath: string,
        line: number | undefined,
        col: number | undefined,
        editorCommand: string
      ): Promise<{ ok: boolean; blocked?: boolean; error?: string }>;
    };
    onData(cb: (uid: string, data: string) => void): () => void;
    onExit(cb: (uid: string, code: number) => void): () => void;
    workspace: {
      getInitial(): Promise<WorkspaceFile | null>;
      open(): Promise<WorkspaceFile | null>;
      save(data: WorkspaceFile): Promise<boolean>;
      publishSession(payload: { active: number; groups: GroupSpec[] }): void;
    };
    control: {
      getStatus(): Promise<ControlStatus>;
      setEnabled(enabled: boolean): Promise<ControlStatus>;
      setAllowInput(allow: boolean): Promise<ControlStatus>;
      publishState(payload: ControlWindowPayload): void;
      onActive(cb: (active: boolean) => void): () => void;
      onCommand(cb: (command: ControlCommand) => void): () => void;
      commandResult(
        correlationId: string,
        reply: { ok: boolean; result?: unknown; error?: string }
      ): void;
    };
    ai: {
      getStatus(): Promise<AiStatus>;
      setEnabled(enabled: boolean): Promise<AiStatus>;
      configure(patch: AiConfigPatch): Promise<AiStatus>;
      publishPanes(panes: AiPanePublish[]): void;
      onStatus(cb: (status: AiStatus) => void): () => void;
    };
    win: {
      minimize(): void;
      toggleMaximize(): void;
      close(): void;
      isMaximized(): Promise<boolean>;
      onMaximizeChange(cb: (maximized: boolean) => void): () => void;
      setFullScreen(on: boolean): void;
      onFullScreenChange(cb: (fullscreen: boolean) => void): () => void;
      getSeed(): Promise<{
        seed: GroupPayload | null;
        windowSpec?: WindowSpec | null;
        primary: boolean;
      }>;
      spawnGroupWindow(group: GroupPayload): Promise<{ ok: boolean }>;
      dragDetach(group: GroupPayload, moveWindow?: boolean): Promise<{ id: number }>;
      dragDrop(): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }>;
      dragCancel(): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }>;
      reportStitchHit(valid: boolean): void;
      onReceiveTab(cb: (group: GroupPayload, x?: number) => void): () => void;
      onTabPreview(cb: (preview: { x: number; title: string } | null) => void): () => void;
      onPaneStitchPreview(cb: (at: { x: number; y: number } | null) => void): () => void;
      onPaneStitch(cb: (p: { group: GroupPayload; x: number; y: number }) => void): () => void;
      onPrimary(cb: () => void): () => void;
    };
  }

  interface Window {
    hp: HpApi;
  }
}
