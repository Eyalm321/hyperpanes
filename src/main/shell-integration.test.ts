import { describe, it, expect } from 'vitest';

// shell-integration.ts imports `app` from electron only for shellIntegrationDir;
// stub it so the pure helpers (fileUriToPath, parseOscCwd, classify) run under Node.
import { vi } from 'vitest';
vi.mock('electron', () => ({ app: { isPackaged: false, getAppPath: () => '/app' } }));

import { classify, fileUriToPath, integrationFor, parseOscCwd } from './shell-integration';

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

describe('parseOscCwd', () => {
  const BEL = '\x07';
  const ESC = '\x1b';
  const seq = (uri: string) => `${ESC}]7;${uri}${BEL}`;

  it('finds a complete sequence in one chunk', () => {
    const r = parseOscCwd('', `hello${seq('file:///C:/a/b')}world`);
    expect(r.cwd).toBe('C:\\a\\b');
    expect(r.carry).toBe('');
  });

  it('fast-rejects a plain chunk with no ESC and no carry', () => {
    const r = parseOscCwd('', 'just some normal output\n');
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });

  it('accepts an ST (ESC backslash) terminator', () => {
    const r = parseOscCwd('', `${ESC}]7;file:///C:/x${ESC}\\`);
    expect(r.cwd).toBe('C:\\x');
  });

  it('returns the LAST complete sequence when several are present', () => {
    const r = parseOscCwd('', `${seq('file:///C:/first')}${seq('file:///C:/second')}`);
    expect(r.cwd).toBe('C:\\second');
  });

  it('carries a URI split across two chunks', () => {
    const a = parseOscCwd('', `${ESC}]7;file:///C:/Users/`);
    expect(a.cwd).toBeNull();
    expect(a.carry).toBe(`${ESC}]7;file:///C:/Users/`);
    const b = parseOscCwd(a.carry, `me/repo${BEL}`);
    expect(b.cwd).toBe('C:\\Users\\me\\repo');
    expect(b.carry).toBe('');
  });

  it('carries a PREFIX split across two chunks (ESC]7 | ;file://...)', () => {
    const a = parseOscCwd('', `output${ESC}]7`);
    expect(a.cwd).toBeNull();
    expect(a.carry).toBe(`${ESC}]7`);
    const b = parseOscCwd(a.carry, `;file:///C:/proj${BEL}`);
    expect(b.cwd).toBe('C:\\proj');
  });

  it('carries a bare trailing ESC', () => {
    const a = parseOscCwd('', `text${ESC}`);
    expect(a.carry).toBe(ESC);
    const b = parseOscCwd(a.carry, `]7;file:///C:/q${BEL}`);
    expect(b.cwd).toBe('C:\\q');
  });

  it('abandons an oversized unterminated sequence (bounded carry)', () => {
    const huge = 'x'.repeat(20000);
    const r = parseOscCwd('', `${ESC}]7;file:///C:/${huge}`);
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });

  it('does not retain a non-OSC7 escape tail', () => {
    const r = parseOscCwd('', `\x1b[0m colored text`);
    expect(r.cwd).toBeNull();
    expect(r.carry).toBe('');
  });
});

describe('parseOscCwd — OSC 9;9 (cmd)', () => {
  const BEL = '\x07';
  const ESC = '\x1b';

  it('reads a raw Windows path from OSC 9;9 (ST-terminated)', () => {
    const r = parseOscCwd('', `${ESC}]9;9;C:\\Users\\me\\repo${ESC}\\`);
    expect(r.cwd).toBe('C:\\Users\\me\\repo');
  });

  it('strips surrounding quotes (Windows Terminal style)', () => {
    const r = parseOscCwd('', `${ESC}]9;9;"C:\\Program Files\\x"${BEL}`);
    expect(r.cwd).toBe('C:\\Program Files\\x');
  });

  it('ignores a non-cwd OSC (title 0;…)', () => {
    const r = parseOscCwd('', `${ESC}]0;my tab title${BEL}`);
    expect(r.cwd).toBeNull();
  });

  it('picks the cwd OSC even when a title OSC precedes it', () => {
    const r = parseOscCwd('', `${ESC}]0;title${BEL}${ESC}]9;9;C:\\proj${BEL}`);
    expect(r.cwd).toBe('C:\\proj');
  });

  it('carries a 9;9 path split across two chunks', () => {
    const a = parseOscCwd('', `${ESC}]9;9;C:\\Users\\`);
    expect(a.cwd).toBeNull();
    const b = parseOscCwd(a.carry, `me\\repo${BEL}`);
    expect(b.cwd).toBe('C:\\Users\\me\\repo');
  });
});

describe('integrationFor — cmd', () => {
  it('gives cmd a PROMPT that emits the OSC 9;9 cwd, with no extra args', () => {
    const r = integrationFor('C:\\Windows\\System32\\cmd.exe', '/whatever');
    expect(r).not.toBeNull();
    expect(r!.args).toEqual([]);
    expect(r!.env.PROMPT).toContain(']9;9;');
    expect(r!.env.PROMPT).toContain('$P');
  });

  it('still returns null for an unknown (other) shell', () => {
    expect(integrationFor('zsh', '/x')).toBeNull();
  });
});
