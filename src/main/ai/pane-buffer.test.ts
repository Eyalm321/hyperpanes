import { describe, expect, it } from 'vitest';
import { PaneTailBuffer } from './pane-buffer';

// Control bytes built from char codes so the test source stays pure ASCII,
// matching ../ansi-strip.test.ts.
const ESC = String.fromCharCode(0x1b);
const BEL = String.fromCharCode(0x07);
const CR = String.fromCharCode(0x0d);

describe('PaneTailBuffer', () => {
  it('joins multi-chunk append into a correct snapshot', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'line1\nline2\n');
    buf.append('a', 'line3\n');
    const snap = buf.snapshot('a');
    expect(snap.text).toBe('line1\nline2\nline3');
    expect(snap.lines).toBe(3);
    expect(snap.altScreen).toBe(false);
    expect(snap.dirty).toBe(true);
  });

  it('strips ANSI colour / cursor codes via the shared stripAnsi', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `${ESC}[31mred${ESC}[0m and ${ESC}[1mbold${ESC}[0m\n`);
    expect(buf.snapshot('a').text).toBe('red and bold');
  });

  it('stitches a partial line across chunk boundaries', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'foo');
    expect(buf.snapshot('a').text).toBe('foo'); // pending partial is visible
    buf.append('a', 'bar\nbaz');
    const snap = buf.snapshot('a');
    expect(snap.text).toBe('foobar\nbaz');
    expect(snap.lines).toBe(2);
    buf.append('a', 'qux\n');
    expect(buf.snapshot('a').text).toBe('foobar\nbazqux');
  });

  it('does not split a CRLF that straddles a chunk boundary', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `done${CR}`);
    buf.append('a', `\nnext\n`);
    expect(buf.snapshot('a').text).toBe('done\nnext');
  });

  it('treats a carriage return as an in-line redraw', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `10%${CR}50%${CR}100%\n`);
    expect(buf.snapshot('a').text).toBe('100%');
  });

  it('retains only the last maxLines lines', () => {
    const buf = new PaneTailBuffer({ maxLines: 3 });
    buf.append('a', 'l1\nl2\nl3\nl4\nl5\n');
    const snap = buf.snapshot('a');
    expect(snap.lines).toBe(3);
    expect(snap.text).toBe('l3\nl4\nl5');
  });

  it('enforces the maxChars hard cap by dropping oldest lines', () => {
    const buf = new PaneTailBuffer({ maxLines: 100, maxChars: 10 });
    buf.append('a', 'aaaaa\nbbbbb\nccccc\n'); // 3 lines of 5
    const snap = buf.snapshot('a');
    expect(snap.text).toBe('ccccc');
    expect(snap.text.length).toBeLessThanOrEqual(10);
  });

  it('truncates a single line that alone exceeds maxChars', () => {
    const buf = new PaneTailBuffer({ maxChars: 8 });
    buf.append('a', `${'x'.repeat(20)}\n`);
    const snap = buf.snapshot('a');
    expect(snap.text).toBe('x'.repeat(8));
  });

  it('detects alt-screen enter and leave (1049 and 47)', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `${ESC}[?1049h`);
    expect(buf.snapshot('a').altScreen).toBe(true);
    buf.append('a', `${ESC}[?1049l`);
    expect(buf.snapshot('a').altScreen).toBe(false);

    buf.append('b', `${ESC}[?47h`);
    expect(buf.snapshot('b').altScreen).toBe(true);
    buf.append('b', `${ESC}[?47l`);
    expect(buf.snapshot('b').altScreen).toBe(false);
  });

  it('uses the last alt-screen toggle in a chunk', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `${ESC}[?1049h${ESC}[?1049l`);
    expect(buf.snapshot('a').altScreen).toBe(false);
  });

  it('resets retained lines on a full clear (ESC[2J)', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'keep1\nkeep2\n');
    buf.append('a', `${ESC}[2Jfresh\n`);
    const snap = buf.snapshot('a');
    expect(snap.text).toBe('fresh');
    expect(snap.lines).toBe(1);
  });

  it('keeps only the text after the last clear in a chunk', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `old${ESC}[2Jnew\n`);
    expect(buf.snapshot('a').text).toBe('new');
  });

  it('returns an empty snapshot for an unknown uid without throwing', () => {
    const buf = new PaneTailBuffer();
    expect(buf.snapshot('nope')).toEqual({ text: '', altScreen: false, dirty: false, lines: 0 });
    // Unknown-uid markClean / clear are safe no-ops.
    expect(() => buf.markClean('nope')).not.toThrow();
    expect(() => buf.clear('nope')).not.toThrow();
  });

  it('tolerates empty chunks (no state, no dirty)', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', '');
    expect(buf.snapshot('a')).toEqual({ text: '', altScreen: false, dirty: false, lines: 0 });
  });

  it('markClean clears the dirty flag until the next append', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'hi\n');
    expect(buf.snapshot('a').dirty).toBe(true);
    buf.markClean('a');
    expect(buf.snapshot('a').dirty).toBe(false);
    buf.append('a', 'more\n');
    expect(buf.snapshot('a').dirty).toBe(true);
  });

  it('clear drops all state for a uid', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'something\n');
    buf.clear('a');
    expect(buf.snapshot('a')).toEqual({ text: '', altScreen: false, dirty: false, lines: 0 });
  });

  it('keeps uids independent', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', 'alpha\n');
    buf.append('b', 'beta\n');
    expect(buf.snapshot('a').text).toBe('alpha');
    expect(buf.snapshot('b').text).toBe('beta');
  });

  it('strips an OSC title sequence from the raw chunk', () => {
    const buf = new PaneTailBuffer();
    buf.append('a', `${ESC}]0;window title${BEL}real output\n`);
    expect(buf.snapshot('a').text).toBe('real output');
  });
});
