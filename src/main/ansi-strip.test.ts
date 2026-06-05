import { describe, expect, it } from 'vitest';
import { stripAnsi } from './ansi-strip';

const ESC = String.fromCharCode(0x1b);
const BEL = String.fromCharCode(0x07);

describe('stripAnsi', () => {
  it('removes SGR color codes, keeps the text', () => {
    expect(stripAnsi(`${ESC}[31mred${ESC}[0m text`)).toBe('red text');
    expect(stripAnsi(`${ESC}[1;32mok${ESC}[39m`)).toBe('ok');
  });

  it('removes cursor / erase CSI sequences', () => {
    expect(stripAnsi(`a${ESC}[2Kb${ESC}[Hc`)).toBe('abc');
    expect(stripAnsi(`${ESC}[2J${ESC}[3Jclear`)).toBe('clear');
  });

  it('removes OSC title sequences (BEL or ST terminated)', () => {
    expect(stripAnsi(`${ESC}]0;my title${BEL}body`)).toBe('body');
    expect(stripAnsi(`${ESC}]2;t${ESC}\\after`)).toBe('after');
  });

  it('preserves newlines, tabs and ordinary punctuation', () => {
    const s = `line1\n\tline2 — done!`;
    expect(stripAnsi(s)).toBe(s);
  });

  it('is a no-op on plain text', () => {
    expect(stripAnsi('no escapes here')).toBe('no escapes here');
  });
});
