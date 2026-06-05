import { describe, it, expect } from 'vitest';
import { trimScreenText, serializeTerminal, type TerminalLike } from './screen';

describe('trimScreenText', () => {
  it('drops trailing blank lines but keeps interior ones', () => {
    expect(trimScreenText(['a', '', 'b', '', '  '])).toBe('a\n\nb');
  });

  it('returns empty string for an all-blank buffer', () => {
    expect(trimScreenText(['', '   ', ''])).toBe('');
    expect(trimScreenText([])).toBe('');
  });

  it('preserves a single content line', () => {
    expect(trimScreenText(['hello'])).toBe('hello');
  });
});

// A fake xterm buffer: each row is a pre-trimmed string; getLine returns a line
// whose translateToString hands it back (true ⇒ already right-trimmed here).
function fakeTerm(rows: string[]): TerminalLike {
  return {
    buffer: {
      active: {
        length: rows.length,
        getLine: (i: number) =>
          i >= 0 && i < rows.length ? { translateToString: () => rows[i] } : undefined
      }
    }
  };
}

describe('serializeTerminal', () => {
  it('joins the active buffer rows and trims trailing blanks', () => {
    const term = fakeTerm(['$ claude', 'Hi there', '', '', '']);
    expect(serializeTerminal(term)).toBe('$ claude\nHi there');
  });

  it('tolerates a missing line (getLine undefined) as a blank row', () => {
    const term: TerminalLike = {
      buffer: {
        active: {
          length: 3,
          getLine: (i: number) => (i === 1 ? undefined : { translateToString: () => `row${i}` })
        }
      }
    };
    expect(serializeTerminal(term)).toBe('row0\n\nrow2');
  });
});
