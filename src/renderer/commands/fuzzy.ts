// Lightweight subsequence fuzzy matcher. Returns a score (higher is better) or
// null when `query` isn't a subsequence of `text`. Rewards consecutive matches
// and matches at word boundaries so "lg" ranks "Layout: Grid" sensibly.
export function fuzzyScore(query: string, text: string): number | null {
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  if (q.length === 0) return 0;

  let ti = 0;
  let score = 0;
  let streak = 0;

  for (const ch of q) {
    let found = -1;
    for (let j = ti; j < t.length; j++) {
      if (t[j] === ch) {
        found = j;
        break;
      }
    }
    if (found === -1) return null;

    if (found === ti) {
      streak += 1;
      score += 2 + streak; // consecutive run bonus
    } else {
      streak = 0;
      score += 1;
    }
    const prev = found > 0 ? t[found - 1] : ' ';
    if (prev === ' ' || prev === ':' || prev === '-') score += 4; // word-boundary bonus

    ti = found + 1;
  }

  return score;
}
