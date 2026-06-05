import { describe, it, expect } from 'vitest';
import { submitNewlines, keyToBytes, keysToBytes } from './control-input';

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

describe('keyToBytes', () => {
  it('maps the core named keys to their VT byte sequences', () => {
    expect(keyToBytes('enter')).toBe('\r');
    expect(keyToBytes('escape')).toBe('\x1b');
    expect(keyToBytes('tab')).toBe('\t');
    expect(keyToBytes('shift+tab')).toBe('\x1b[Z');
    expect(keyToBytes('up')).toBe('\x1b[A');
    expect(keyToBytes('down')).toBe('\x1b[B');
    expect(keyToBytes('left')).toBe('\x1b[D');
    expect(keyToBytes('right')).toBe('\x1b[C');
    expect(keyToBytes('backspace')).toBe('\x7f');
    expect(keyToBytes('pageup')).toBe('\x1b[5~');
    expect(keyToBytes('pagedown')).toBe('\x1b[6~');
  });

  it('is case- and whitespace-insensitive, and accepts synonyms', () => {
    expect(keyToBytes('  ENTER ')).toBe('\r');
    expect(keyToBytes('Esc')).toBe('\x1b');
    expect(keyToBytes('Return')).toBe('\r');
    expect(keyToBytes('pgdn')).toBe('\x1b[6~');
  });

  it('derives ctrl+<letter> as the C0 control code', () => {
    expect(keyToBytes('ctrl+c')).toBe('\x03');
    expect(keyToBytes('ctrl+d')).toBe('\x04');
    expect(keyToBytes('ctrl+a')).toBe('\x01');
  });

  it('returns null for an unknown key', () => {
    expect(keyToBytes('frobnicate')).toBeNull();
    expect(keyToBytes('ctrl+shift+x')).toBeNull();
  });
});

describe('keysToBytes', () => {
  it('concatenates a sequence of keys into one byte string', () => {
    expect(keysToBytes(['escape', 'enter'])).toEqual({ ok: true, bytes: '\x1b\r' });
  });

  it('treats an empty list as a valid no-op write', () => {
    expect(keysToBytes([])).toEqual({ ok: true, bytes: '' });
  });

  it('reports every unknown key, not just the first', () => {
    expect(keysToBytes(['enter', 'nope', 'also-bad'])).toEqual({
      ok: false,
      unknown: ['nope', 'also-bad']
    });
  });
});
