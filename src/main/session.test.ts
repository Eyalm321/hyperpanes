import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import fs from 'node:fs';

// session.ts pulls in electron + the native node-pty at import time; stub both so
// the pure spawn-target logic (buildArgs / resolveSpawn) runs under plain Node.
vi.mock('electron', () => ({ app: { getPath: () => '/tmp' } }));
vi.mock('node-pty', () => ({ spawn: vi.fn() }));

import { buildArgs, resolveSpawn, resolveWindowsCommand } from './session';

describe('buildArgs', () => {
  it('wraps a command for PowerShell / pwsh', () => {
    expect(buildArgs('powershell.exe', 'npm run dev')).toEqual(['-NoLogo', '-Command', 'npm run dev']);
    expect(buildArgs('pwsh', 'echo hi')).toEqual(['-NoLogo', '-Command', 'echo hi']);
  });

  it('wraps a command for POSIX-family shells with -c (covers git-bash on Windows)', () => {
    expect(buildArgs('/bin/bash', 'ls -la')).toEqual(['-c', 'ls -la']);
    expect(buildArgs('zsh', 'ls')).toEqual(['-c', 'ls']);
    expect(buildArgs('C:\\Program Files\\Git\\bin\\bash.exe', 'ls')).toEqual(['-c', 'ls']);
  });

  it('returns the bare base args (or none) for an interactive shell', () => {
    expect(buildArgs('pwsh')).toEqual([]);
    expect(buildArgs('bash', undefined, ['-l'])).toEqual(['-l']);
  });
});

describe('resolveSpawn (P4a)', () => {
  let statSyncSpy: any;

  beforeEach(() => {
    statSyncSpy = vi.spyOn(fs, 'statSync').mockImplementation(() => {
      throw new Error('ENOENT');
    });
  });

  afterEach(() => {
    statSyncSpy.mockRestore();
  });

  it('with command + non-empty args, spawns the command DIRECTLY — no shell, verbatim argv', () => {
    expect(
      resolveSpawn('powershell.exe', 'claude', ['--append-system-prompt', 'be a pirate, matey'])
    ).toEqual({ file: 'claude', args: ['--append-system-prompt', 'be a pirate, matey'] });
  });

  it('preserves an arg containing spaces and quotes as ONE element (the P4a fix)', () => {
    // The exact failure mode the args form exists to defeat: a value cmd.exe would
    // otherwise re-split. It must reach the pty intact, as a single argv element.
    const argv = ['--msg', 'hello "world" of panes'];
    expect(resolveSpawn('cmd.exe', 'mytool', argv)).toEqual({ file: 'mytool', args: argv });
  });

  it('with command but no args, runs it through the shell (back-compat)', () => {
    expect(resolveSpawn('pwsh', 'npm run dev')).toEqual({
      file: 'pwsh',
      args: ['-NoLogo', '-Command', 'npm run dev']
    });
    expect(resolveSpawn('/bin/bash', 'ls -la')).toEqual({ file: '/bin/bash', args: ['-c', 'ls -la'] });
  });

  it('treats an empty args array as "no args" — shell-wraps the command', () => {
    expect(resolveSpawn('pwsh', 'top', [])).toEqual({
      file: 'pwsh',
      args: ['-NoLogo', '-Command', 'top']
    });
  });

  it('with no command, spawns the interactive shell (args, if any, go to the shell)', () => {
    expect(resolveSpawn('pwsh')).toEqual({ file: 'pwsh', args: [] });
    expect(resolveSpawn('/bin/bash', undefined, ['-l'])).toEqual({ file: '/bin/bash', args: ['-l'] });
  });
});

describe('resolveWindowsCommand', () => {
  let statSyncSpy: any;

  beforeEach(() => {
    statSyncSpy = vi.spyOn(fs, 'statSync');
  });

  afterEach(() => {
    statSyncSpy.mockRestore();
  });

  it('resolves absolute path exactly if it exists', () => {
    const target = 'C:\\Program Files\\MyTool\\tool.exe';
    statSyncSpy.mockImplementation((p: any) => {
      if (p === target) {
        return { isFile: () => true } as any;
      }
      throw new Error('ENOENT');
    });

    expect(resolveWindowsCommand(target, 'C:\\', {})).toBe(target);
  });

  it('resolves relative path with extension in cwd', () => {
    const cwd = 'C:\\myproj';
    const target = '.\\bin\\tool';
    const expected = 'C:\\myproj\\bin\\tool.exe';
    statSyncSpy.mockImplementation((p: any) => {
      if (p === expected) {
        return { isFile: () => true } as any;
      }
      throw new Error('ENOENT');
    });

    expect(resolveWindowsCommand(target, cwd, { PATHEXT: '.EXE;.CMD' })).toBe(expected);
  });

  it('searches cwd first then PATH', () => {
    const cwd = 'C:\\myproj';
    const env = {
      PATH: 'C:\\bin;C:\\Windows\\system32',
      PATHEXT: '.EXE;.CMD'
    };

    // Case 1: exists in cwd
    statSyncSpy.mockImplementation((p: any) => {
      if (p === 'C:\\myproj\\tool.cmd') {
        return { isFile: () => true } as any;
      }
      throw new Error('ENOENT');
    });
    expect(resolveWindowsCommand('tool', cwd, env)).toBe('C:\\myproj\\tool.cmd');

    // Case 2: exists in PATH (C:\Windows\system32)
    statSyncSpy.mockImplementation((p: any) => {
      if (p === 'C:\\Windows\\system32\\tool.exe') {
        return { isFile: () => true } as any;
      }
      throw new Error('ENOENT');
    });
    expect(resolveWindowsCommand('tool', cwd, env)).toBe('C:\\Windows\\system32\\tool.exe');
  });

  it('falls back to verbatim command if not found', () => {
    statSyncSpy.mockImplementation(() => {
      throw new Error('ENOENT');
    });
    expect(resolveWindowsCommand('unknowncmd', 'C:\\', { PATH: 'C:\\bin' })).toBe('unknowncmd');
  });
});
