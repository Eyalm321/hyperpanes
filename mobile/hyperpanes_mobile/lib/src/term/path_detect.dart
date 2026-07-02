/// Extract a file-path-like token around a tapped column in a terminal line —
/// the mobile half of the desktop's clickable-paths feature. Pure logic, unit-tested.
library;

/// A detected path candidate: the cleaned path plus an optional `:line` suffix.
class PathHit {
  const PathHit(this.path, {this.line});
  final String path;
  final int? line;

  @override
  bool operator ==(Object other) =>
      other is PathHit && other.path == path && other.line == line;

  @override
  int get hashCode => Object.hash(path, line);

  @override
  String toString() => 'PathHit($path${line == null ? '' : ':$line'})';
}

/// Characters that can appear inside a unix path token. Excludes quotes/brackets and
/// `:` (handled separately for `path:line:col` suffixes).
final _pathChar = RegExp(r'[A-Za-z0-9_\-./~+@%,=]');

/// Find the path-like token covering column [col] (0-based) of [line], or null.
/// Accepts absolute (`/…`), home (`~/…`), and explicit-relative (`./…`, `../…`)
/// paths — bare relative names are too ambiguous in prose to hot-link.
PathHit? pathAt(String line, int col) {
  if (line.isEmpty) return null;
  final c = col.clamp(0, line.length - 1);
  // Expand left/right over path characters + `:` (kept for line-suffix parsing).
  bool isTok(int i) =>
      i >= 0 &&
      i < line.length &&
      (_pathChar.hasMatch(line[i]) || line[i] == ':');
  if (!isTok(c)) return null;
  var start = c;
  while (isTok(start - 1)) {
    start--;
  }
  var end = c;
  while (isTok(end + 1)) {
    end++;
  }
  var token = line.substring(start, end + 1);
  // Strip trailing punctuation that prose glues onto paths.
  token = token.replaceFirst(RegExp(r'[.,:;]+$'), '');
  // Split off `:line[:col]`.
  int? lineNo;
  final m = RegExp(r'^(.*?):(\d+)(?::\d+)?$').firstMatch(token);
  if (m != null) {
    token = m.group(1)!;
    lineNo = int.tryParse(m.group(2)!);
  }
  if (!(token.startsWith('/') ||
      token.startsWith('~/') ||
      token.startsWith('./') ||
      token.startsWith('../'))) {
    return null;
  }
  // A lone `/` or `~/` isn't a file.
  final base = token.replaceFirst(RegExp(r'^(~|\.{1,2})?/'), '');
  if (base.isEmpty) return null;
  return PathHit(token, line: lineNo);
}
