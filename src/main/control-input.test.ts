import { describe, it, expect } from 'vitest';
import { submitNewlines } from './control-input';

describe('submitNewlines', () => {
  it('passes input through untouched off Windows (POSIX LF already submits)', () => {
    expect(submitNewlines('dir\n', 'linux')).toBe('dir\n');
    expect(submitNewlines('a\r\nb\n', 'darwin')).toBe('a\r\nb\n');
  });

  it('collapses a bare LF to CR on Windows so the line submits', () => {
    expect(submitNewlines('dir\n', 'win32')).toBe('dir\r');
  });

  it('collapses CRLF to a single CR on Windows (no trailing blank line)', () => {
    expect(submitNewlines('dir\r\n', 'win32')).toBe('dir\r');
  });

  it('submits every line of multi-line input on Windows', () => {
    expect(submitNewlines('a\nb\n', 'win32')).toBe('a\rb\r');
  });

  it('leaves a bare CR and newline-free text untouched on Windows', () => {
    expect(submitNewlines('echo hi\r', 'win32')).toBe('echo hi\r');
    expect(submitNewlines('no newline', 'win32')).toBe('no newline');
  });
});
