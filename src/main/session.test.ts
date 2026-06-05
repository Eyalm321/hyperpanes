import { describe, it, expect, vi } from 'vitest';

// session.ts pulls in electron + the native node-pty at import time; stub both so
// the pure spawn-target logic (buildArgs / resolveSpawn) runs under plain Node.
vi.mock('electron', () => ({ app: { getPath: () => '/tmp' } }));
vi.mock('node-pty', () => ({ spawn: vi.fn() }));

import { buildArgs, resolveSpawn } from './session';

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
