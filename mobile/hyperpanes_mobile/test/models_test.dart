import 'package:flutter_test/flutter_test.dart';
import 'package:hyperpanes_mobile/src/api/models.dart';

void main() {
  test('HostState parses the /state tree with additive fields', () {
    final s = HostState.fromJson({
      'windows': [
        {
          'windowId': 1,
          'activeTabId': 't1',
          'tabs': [
            {
              'id': 't1',
              'title': 'work',
              'layout': 'grid',
              'panes': [
                {
                  'id': 'p1',
                  'sessionUid': 'u1',
                  'label': 'claude',
                  'color': '#3b82f6',
                  'command': 'claude',
                  'status': 'running',
                  'activity': 'busy',
                  'meta': {'ai.subtitle': 'refactoring auth'},
                  'cols': 120,
                  'rows': 30,
                  'someFutureField': {'ignored': true},
                },
                {
                  'id': 'p2',
                  'sessionUid': 'u2',
                  'label': 'shell',
                  'color': '#30a46c',
                  'status': 'running',
                  'activity': 'idle',
                },
              ],
            }
          ],
        }
      ],
    });
    expect(s.allPanes.length, 2);
    final p1 = s.paneById('p1')!;
    expect(p1.cols, 120);
    expect(p1.rows, 30);
    expect(p1.looksLikeAgent, isTrue);
    expect(p1.aiSubtitle, 'refactoring auth');
    final p2 = s.paneById('p2')!;
    expect(p2.cols, isNull);
    expect(p2.looksLikeAgent, isFalse);
    expect(s.paneById('nope'), isNull);
  });

  test('malformed panes are skipped, not fatal', () {
    final s = HostState.fromJson({
      'windows': [
        {
          'windowId': 1,
          'tabs': [
            {
              'id': 't1',
              'panes': [
                {'id': 'ok', 'sessionUid': 'u'},
                {'sessionUid': 'missing-id'},
                'not-a-map',
              ],
            }
          ],
        }
      ],
    });
    expect(s.allPanes.length, 1);
    expect(s.allPanes.first.id, 'ok');
  });
}
