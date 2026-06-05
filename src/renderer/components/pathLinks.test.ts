import { describe, it, expect } from 'vitest';
import { extractPathCandidates, hasPathShape, cellFromIndex } from './pathLinks';

describe('hasPathShape', () => {
  it('accepts paths with a separator', () => {
    expect(hasPathShape('src/index.ts')).toBe(true);
    expect(hasPathShape('src\\index.ts')).toBe(true);
    expect(hasPathShape('./build')).toBe(true);
    expect(hasPathShape('../a')).toBe(true);
    expect(hasPathShape('C:\\foo')).toBe(true);
  });
  it('accepts bare files with an extension', () => {
    expect(hasPathShape('package.json')).toBe(true);
    expect(hasPathShape('.gitignore')).toBe(true);
  });
  it('rejects bare words even if a file could exist', () => {
    expect(hasPathShape('build')).toBe(false);
    expect(hasPathShape('src')).toBe(false);
    expect(hasPathShape('README')).toBe(false);
  });
});

describe('extractPathCandidates', () => {
  const only = (line: string) => extractPathCandidates(line);

  it('finds a relative path and underlines exactly it', () => {
    const line = 'see src/renderer/Terminal.tsx for details';
    const c = only(line);
    expect(c).toHaveLength(1);
    expect(c[0].path).toBe('src/renderer/Terminal.tsx');
    expect(line.slice(c[0].start, c[0].end)).toBe('src/renderer/Terminal.tsx');
    expect(c[0].line).toBeUndefined();
  });

  it('parses :line and :line:col suffixes', () => {
    expect(only('a/b.ts:42')[0]).toMatchObject({ path: 'a/b.ts', line: 42, col: undefined });
    expect(only('a/b.ts:42:7')[0]).toMatchObject({ path: 'a/b.ts', line: 42, col: 7 });
  });

  it('keeps the drive-letter colon as part of an absolute Windows path', () => {
    const c = only('at C:\\hyperpanes\\src\\Terminal.tsx:224');
    expect(c).toHaveLength(1);
    expect(c[0]).toMatchObject({ path: 'C:\\hyperpanes\\src\\Terminal.tsx', line: 224 });
  });

  it('handles a quoted path with spaces', () => {
    const line = 'open "C:\\Program Files\\app\\read me.txt" now';
    const c = only(line);
    expect(c).toHaveLength(1);
    expect(c[0].path).toBe('C:\\Program Files\\app\\read me.txt');
    // range excludes the surrounding quotes
    expect(line.slice(c[0].start, c[0].end)).toBe('C:\\Program Files\\app\\read me.txt');
  });

  it('handles a quoted path with a suffix after the closing quote', () => {
    const c = only('"a b.ts":10:3');
    expect(c[0]).toMatchObject({ path: 'a b.ts', line: 10, col: 3 });
  });

  it('strips wrapping punctuation: parens, backticks, trailing period', () => {
    expect(only('(src/a.ts)')[0].path).toBe('src/a.ts');
    expect(only('`src/a.ts`')[0].path).toBe('src/a.ts');
    expect(only('edited src/a.ts.')[0].path).toBe('src/a.ts');
    expect(only('files: a.ts, b.ts')[0].path).toBe('a.ts');
  });

  it('does not mistake host:port or bare words for paths', () => {
    expect(only('listening on localhost:3000')).toHaveLength(0);
    expect(only('run the build step in src now')).toHaveLength(0);
    expect(only('version v18.3.1 here').map((c) => c.path)).toEqual(['v18.3.1']);
    // ^ shape-passes (looks like .1 ext) but the disk check downstream rejects it
  });

  it('finds multiple paths on one line', () => {
    const c = only('moved src/a.ts -> dist/a.js');
    expect(c.map((x) => x.path)).toEqual(['src/a.ts', 'dist/a.js']);
  });

  it('matches ./ ../ and ~/ prefixes', () => {
    expect(only('./scripts/run.sh')[0].path).toBe('./scripts/run.sh');
    expect(only('../shared/x.ts')[0].path).toBe('../shared/x.ts');
    expect(only('~/notes/todo.md')[0].path).toBe('~/notes/todo.md');
  });
});

describe('cellFromIndex', () => {
  it('maps within a single row (1-based)', () => {
    expect(cellFromIndex(0, 5, 80)).toEqual({ x: 1, y: 6 });
    expect(cellFromIndex(79, 5, 80)).toEqual({ x: 80, y: 6 });
  });
  it('wraps to the next row past the column count', () => {
    expect(cellFromIndex(80, 5, 80)).toEqual({ x: 1, y: 7 });
    expect(cellFromIndex(165, 0, 80)).toEqual({ x: 6, y: 3 });
  });
});
