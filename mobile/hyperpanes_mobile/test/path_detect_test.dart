import 'package:flutter_test/flutter_test.dart';
import 'package:hyperpanes_mobile/src/term/path_detect.dart';

void main() {
  test('absolute path anywhere in the token is hit', () {
    const line = 'error in /home/user/dev/app/src/main.rs, aborting';
    for (final col in [9, 20, 39]) {
      expect(
        pathAt(line, col),
        const PathHit('/home/user/dev/app/src/main.rs'),
        reason: 'col $col',
      );
    }
  });

  test('path:line:col extracts the line number', () {
    const line = '  --> crates/core/src/app.rs — see /tmp/x/y.rs:143:9 for it';
    expect(pathAt(line, 40), const PathHit('/tmp/x/y.rs', line: 143));
  });

  test('home and explicit-relative prefixes accepted, bare words not', () {
    expect(pathAt('cat ~/notes/todo.md', 8), const PathHit('~/notes/todo.md'));
    expect(pathAt('see ./README.md.', 7), const PathHit('./README.md'));
    expect(pathAt('see ../lib/a.dart', 8), const PathHit('../lib/a.dart'));
    expect(pathAt('plain words here', 3), isNull);
    expect(pathAt('src/main.rs is relative', 4), isNull);
  });

  test('trailing punctuation stripped; tap outside token misses', () {
    expect(pathAt('open /etc/hosts, then edit', 8),
        const PathHit('/etc/hosts'));
    expect(pathAt('open /etc/hosts, then edit', 18), isNull);
    expect(pathAt('', 0), isNull);
    expect(pathAt('/', 0), isNull);
    expect(pathAt('~/', 1), isNull);
  });
}
