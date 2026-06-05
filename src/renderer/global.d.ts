import type { GroupPayload, WorkspaceFile } from './types';

export {};

declare global {
  interface HpSpawnOptions {
    uid: string;
    shell?: string;
    command?: string;
    cwd?: string;
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
      saveLast(data: WorkspaceFile): void;
    };
    win: {
      minimize(): void;
      toggleMaximize(): void;
      close(): void;
      isMaximized(): Promise<boolean>;
      onMaximizeChange(cb: (maximized: boolean) => void): () => void;
      setFullScreen(on: boolean): void;
      onFullScreenChange(cb: (fullscreen: boolean) => void): () => void;
      getSeed(): Promise<{ seed: GroupPayload | null; primary: boolean }>;
      spawnGroupWindow(group: GroupPayload): Promise<{ ok: boolean }>;
      dragDetach(group: GroupPayload, moveWindow?: boolean): Promise<{ id: number }>;
      dragDrop(): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }>;
      dragCancel(): Promise<{ action: 'docked' | 'stitched' | 'detached' | 'none' }>;
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
