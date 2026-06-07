import { describe, it, expect } from 'vitest';

// shell-integration.ts imports `app` from electron only for shellIntegrationDir;
// stub it so the pure helpers (fileUriToPath, parseOsc7, classify) run under Node.
import { vi } from 'vitest';
vi.mock('electron', () => ({ app: { isPackaged: false, getAppPath: () => '/app' } }));

import { classify, fileUriToPath, parseOsc7 } from './shell-integration';

describe('classify', () => {
  it('detects pwsh first (powershell also ends in "sh")', () => {
    expect(classify('powershell.exe')).toBe('pwsh');
    expect(classify('C:\\Program Files\\PowerShell\\7\\pwsh.exe')).toBe('pwsh');
    expect(classify('pwsh')).toBe('pwsh');
  });
  it('detects bash (incl. git-bash path)', () => {
    expect(classify('/bin/bash')).toBe('bash');
    expect(classify('C:\\Program Files\\Git\\bin\\bash.exe')).toBe('bash');
  });
  it('detects cmd', () => {
    expect(classify('cmd.exe')).toBe('cmd');
    expect(classify('C:\\Windows\\System32\\cmd.exe')).toBe('cmd');
  });
  it('everything else is other (no integration)', () => {
    expect(classify('zsh')).toBe('other');
    expect(classify('fish')).toBe('other');
    expect(classify('')).toBe('other');
  });
});

describe('fileUriToPath', () => {
  it('converts a pwsh Windows file URI', () => {
    expect(fileUriToPath('file:///C:/Users/me/repo')).toBe('C:\\Users\\me\\repo');
  });
  it('uppercases the drive letter', () => {
    expect(fileUriToPath('file:///c:/temp')).toBe('C:\\temp');
  });
  it('converts an MSYS (git-bash) drive path', () => {
    expect(fileUriToPath('file:///c/Users/me/repo')).toBe('C:\\Users\\me\\repo');
  });
  it('percent-decodes %20 and friends', () => {
    expect(fileUriToPath('file:///C:/Users/My%20Repo')).toBe('C:\\Users\\My Repo');
    expect(fileUriToPath('file:///c/Users/My%20Repo')).toBe('C:\\Users\\My Repo');
  });
  it('handles a percent-encoded colon', () => {
    expect(fileUriToPath('file:///c%3A/temp')).toBe('C:\\temp');
  });
  it('accepts an explicit localhost authority', () => {
    expect(fileUriToPath('file://localhost/C:/x')).toBe('C:\\x');
  });
  it('rejects a remote host (no relocating the local pane)', () => {
    expect(fileUriToPath('file://otherbox/home/me')).toBeNull();
    expect(fileUriToPath('file://192.168.1.5/srv')).toBeNull();
  });
  it('returns a POSIX absolute path unchanged', () => {
    expect(fileUriToPath('file:///home/me/proj')).toBe('/home/me/proj');
  });
  it('rejects non-file URIs and empties', () => {
    expect(fileUriToPath('http://example.com')).toBeNull();
    expect(fileUriToPath('')).toBeNull();
  });
  it('keeps the drive root', () => {
    expect(fileUriToPath('file:///C:/')).toBe('C:\\');
  });
});

describe('parseOsc7', () => {
  const BEL = '\x07';
  const ESC = '\x1b';
  const seq = (uri: string) => `${ESC}]7;${uri}${BEL}`;

  it('finds a complete sequence in one chunk', () => {
    const r = parseOsc7('', `hello${seq('file:///C:/a/b')}world`);
    expect(r.cwd).toBe('C:\\a\\b');
    expect(r.carry).toBe('');
  });

  it('fast-rejects a plain chunk with no ESC and no carry', () => {
    const r = parseOsc7('', 'just some normal output\n');
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });

  it('accepts an ST (ESC backslash) terminator', () => {
    const r = parseOsc7('', `${ESC}]7;file:///C:/x${ESC}\\`);
    expect(r.cwd).toBe('C:\\x');
  });

  it('returns the LAST complete sequence when several are present', () => {
    const r = parseOsc7('', `${seq('file:///C:/first')}${seq('file:///C:/second')}`);
    expect(r.cwd).toBe('C:\\second');
  });

  it('carries a URI split across two chunks', () => {
    const a = parseOsc7('', `${ESC}]7;file:///C:/Users/`);
    expect(a.cwd).toBeNull();
    expect(a.carry).toBe(`${ESC}]7;file:///C:/Users/`);
    const b = parseOsc7(a.carry, `me/repo${BEL}`);
    expect(b.cwd).toBe('C:\\Users\\me\\repo');
    expect(b.carry).toBe('');
  });

  it('carries a PREFIX split across two chunks (ESC]7 | ;file://...)', () => {
    const a = parseOsc7('', `output${ESC}]7`);
    expect(a.cwd).toBeNull();
    expect(a.carry).toBe(`${ESC}]7`);
    const b = parseOsc7(a.carry, `;file:///C:/proj${BEL}`);
    expect(b.cwd).toBe('C:\\proj');
  });

  it('carries a bare trailing ESC', () => {
    const a = parseOsc7('', `text${ESC}`);
    expect(a.carry).toBe(ESC);
    const b = parseOsc7(a.carry, `]7;file:///C:/q${BEL}`);
    expect(b.cwd).toBe('C:\\q');
  });

  it('abandons an oversized unterminated sequence (bounded carry)', () => {
    const huge = 'x'.repeat(20000);
    const r = parseOsc7('', `${ESC}]7;file:///C:/${huge}`);
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });

  it('does not retain a non-OSC7 escape tail', () => {
    const r = parseOsc7('', `\x1b[0m colored text`);
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });
});
