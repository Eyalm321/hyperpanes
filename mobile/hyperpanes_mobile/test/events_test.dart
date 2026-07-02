import 'package:flutter_test/flutter_test.dart';
import 'package:hyperpanes_mobile/src/api/events.dart';

void main() {
  group('ControlFrame.parse', () {
    test('output frame with cursor', () {
      final f = ControlFrame.parse(
        '{"type":"output","sessionUid":"u1","paneId":"p1","data":"hi","cursor":42}',
      );
      expect(f, isA<OutputFrame>());
      final o = f as OutputFrame;
      expect(o.sessionUid, 'u1');
      expect(o.paneId, 'p1');
      expect(o.data, 'hi');
      expect(o.cursor, 42);
    });

    test('output frame WITHOUT cursor (pre-cursor host) → 0', () {
      final f = ControlFrame.parse(
        '{"type":"output","sessionUid":"u1","paneId":null,"data":"x"}',
      );
      expect((f as OutputFrame).cursor, 0);
      expect(f.paneId, isNull);
    });

    test('liveness / activity / exit / state', () {
      expect(
        (ControlFrame.parse(
                '{"type":"liveness","paneId":"p","state":"awaiting-input"}')
            as LivenessFrame)
            .state,
        'awaiting-input',
      );
      expect(
        (ControlFrame.parse('{"type":"activity","paneId":"p","activity":"busy"}')
                as ActivityFrame)
            .activity,
        'busy',
      );
      expect(
        (ControlFrame.parse(
                '{"type":"exit","sessionUid":"u","paneId":"p","code":3}')
            as ExitFrame)
            .code,
        3,
      );
      expect(ControlFrame.parse('{"type":"state"}'), isA<StatePing>());
    });

    test('unknown types and garbage never throw', () {
      expect(ControlFrame.parse('{"type":"future-frame"}'), isA<UnknownFrame>());
      expect(ControlFrame.parse('not json'), isA<UnknownFrame>());
      expect(ControlFrame.parse('[1,2]'), isA<UnknownFrame>());
      expect(ControlFrame.parse('{"no":"type"}'), isA<UnknownFrame>());
    });
  });
}
