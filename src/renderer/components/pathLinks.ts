// Detects file-path tokens in a terminal line so they can be turned into
// clickable links (see Terminal.tsx). Pure + unit-tested; the on-disk
// verification, cwd resolution and open/copy actions live in main/paths.ts and
// the link provider that consumes these candidates.
//
// Shape rule (decided): a candidate must contain a path separator OR end in a
// file extension. Bare words like `build`/`src`/`test` never linkify even when a
// matching file exists — that keeps prose from lighting up. The drive-letter
// colon in `C:\foo.ts:42` stays part of the path; only a trailing `:line[:col]`
// is parsed off as a location suffix.

export interface PathCandidate {
  /** The path portion, with any :line:col suffix and wrapping punctuation removed. */
  path: string;
  line?: number;
  col?: number;
  /** Inclusive start index into the source line (for the link's underline range). */
  start: number;
  /** Exclusive end index into the source line. */
  end: number;
}

// Wrapping punctuation stripped from the ends of an unquoted token: brackets,
// backticks/quotes, and trailing sentence punctuation (`see src/a.ts.`).
const LEAD = new Set(['(', '[', '{', '<', '`', '"', "'"]);
const TRAIL = new Set([')', ']', '}', '>', '`', '"', "'", ',', ';', '.', '!', '?']);

// One token: a double- or single-quoted string, or a run of non-space,
// non-quote characters (which may itself hold a drive colon and a :line suffix).
const TOKEN_RE = /"[^"]*"|'[^']*'|[^\s"']+/g;

/** True when a string looks path-shaped: has a separator, or ends in an extension. */
export function hasPathShape(p: string): boolean {
  if (/[\\/]/.test(p)) return true; // separator → covers ./ ../ ~/ and C:\ too
  return /\.[A-Za-z0-9]{1,12}$/.test(p); // trailing .ext (also catches .gitignore)
}

// Split a trailing :line[:col] off a token, but only when the part before it is
// itself path-shaped (so `localhost:3000` isn't mistaken for a located path).
function splitSuffix(core: string): { path: string; line?: number; col?: number } {
  const m = /^(.+?):(\d+)(?::(\d+))?$/.exec(core);
  if (m && hasPathShape(m[1])) {
    return { path: m[1], line: +m[2], col: m[3] ? +m[3] : undefined };
  }
  return { path: core };
}

export function extractPathCandidates(line: string): PathCandidate[] {
  const out: PathCandidate[] = [];
  TOKEN_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = TOKEN_RE.exec(line)) !== null) {
    const tok = m[0];
    const tokStart = m.index;

    // Quoted path ("C:\Program Files\app\readme.txt"), optionally followed by a
    // :line:col suffix right after the closing quote.
    if (tok[0] === '"' || tok[0] === "'") {
      const inner = tok.slice(1, -1);
      const after = tokStart + tok.length;
      const suffix = /^:(\d+)(?::(\d+))?/.exec(line.slice(after));
      let end = after - 1; // exclude the closing quote from the underline range
      let lineNo: number | undefined;
      let colNo: number | undefined;
      if (suffix) {
        lineNo = +suffix[1];
        colNo = suffix[2] ? +suffix[2] : undefined;
        end = after + suffix[0].length;
        TOKEN_RE.lastIndex = end; // don't re-tokenize the suffix
      }
      if (inner && hasPathShape(inner)) {
        out.push({ path: inner, line: lineNo, col: colNo, start: tokStart + 1, end });
      }
      continue;
    }

    // Unquoted run: trim wrapping punctuation, then peel the location suffix.
    let s = 0;
    let e = tok.length;
    while (s < e && LEAD.has(tok[s])) s++;
    while (e > s && TRAIL.has(tok[e - 1])) e--;
    if (e <= s) continue;
    const core = tok.slice(s, e);
    const { path, line: lineNo, col: colNo } = splitSuffix(core);
    if (!path || !hasPathShape(path)) continue;
    out.push({ path, line: lineNo, col: colNo, start: tokStart + s, end: tokStart + e });
  }
  return out;
}

/**
 * Map a 0-based index into a (wrap-joined) logical line to a 1-based xterm cell.
 * Assumes one column per character — exact for ASCII paths; wide (CJK) glyphs
 * earlier on the line can shift this, an accepted v1 limitation.
 */
export function cellFromIndex(index: number, startRow: number, cols: number): { x: number; y: number } {
  return { x: (index % cols) + 1, y: startRow + Math.floor(index / cols) + 1 };
}
