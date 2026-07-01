import 'dart:async';

import 'package:flutter_test/flutter_test.dart';
import 'package:hyperpanes_mobile/src/api/control_client.dart';
import 'package:hyperpanes_mobile/src/api/events.dart';
import 'package:hyperpanes_mobile/src/term/pane_session.dart';

/// Harness: a manually-driven frame stream + a gated snapshot fetch, so tests control
/// the exact interleaving of "frames arriving" vs "snapshot resolving".
class _Harness {
  final frames = StreamController<ControlFrame>.broadcast(sync: true);
  final snapshotGate = Completer<void>();
  OutputSnapshot snapshot = const OutputSnapshot(output: '', cursor: 0);
  final sent = <(String, String, bool)>[];
  final keys = <List<String>>[];

  late final PaneSession session = PaneSession(
    paneId: 'p1',
    sessionUid: 'u1',
    frames: frames.stream,
    fetchOutput: (_) async {
      await snapshotGate.future;
      return snapshot;
    },
    sendData: (id, data, {submit = false}) async {
      sent.add((id, data, submit));
    },
    sendKeys: (id, k) async => keys.add(k),
    cols: 20,
    rows: 5,
  );

  /// Visible text of row [r] in the emulator.
  String row(int r) =>
      session.terminal.buffer.lines[r].toString().trimRight();

  OutputFrame out(String data, int cursor) => OutputFrame(
        sessionUid: 'u1',
        paneId: 'p1',
        data: data,
        cursor: cursor,
      );
}

void main() {
  test('seed + live frames splice without duplicates', () async {
    final h = _Harness();
    h.snapshot = const OutputSnapshot(output: 'hello ', cursor: 6);
    final attaching = h.session.attach();
    // Overlap: this frame's bytes are ALREADY in the snapshot (cursor 6 <= seed 6).
    h.frames.add(h.out('hello ', 6));
    // This one is new (cursor 11 > 6).
    h.frames.add(h.out('world', 11));
    h.snapshotGate.complete();
    await attaching;
    await Future<void>.delayed(Duration.zero);
    expect(h.row(0), 'hello world');
    expect(h.session.state.value, PaneSessionState.live);
    expect(h.session.seedCursor, 6);
  });

  test('frames buffered during fetch are deduped against the seed', () async {
    final h = _Harness();
    h.snapshot = const OutputSnapshot(output: 'AB', cursor: 2);
    final attaching = h.session.attach();
    // All three arrive BEFORE the snapshot resolves.
    h.frames.add(h.out('A', 1));
    h.frames.add(h.out('B', 2));
    h.frames.add(h.out('C', 3));
    h.snapshotGate.complete();
    await attaching;
    expect(h.row(0), 'ABC');
  });

  test('cursor 0 (pre-cursor host) applies everything after seed', () async {
    final h = _Harness();
    h.snapshot = const OutputSnapshot(output: 'X', cursor: 5);
    final attaching = h.session.attach();
    h.snapshotGate.complete();
    await attaching;
    h.frames.add(h.out('Y', 0));
    expect(h.row(0), 'XY');
  });

  test('frames for other panes are ignored', () async {
    final h = _Harness();
    h.snapshot = const OutputSnapshot(output: '', cursor: 0);
    final attaching = h.session.attach();
    h.snapshotGate.complete();
    await attaching;
    h.frames.add(const OutputFrame(
      sessionUid: 'other',
      paneId: 'p2',
      data: 'NOPE',
      cursor: 99,
    ));
    expect(h.row(0), '');
  });

  test('exit frame flips state and records the code', () async {
    final h = _Harness();
    h.snapshot = const OutputSnapshot(output: '', cursor: 0);
    final attaching = h.session.attach();
    h.snapshotGate.complete();
    await attaching;
    h.frames.add(const ExitFrame(sessionUid: 'u1', paneId: 'p1', code: 42));
    expect(h.session.state.value, PaneSessionState.exited);
    expect(h.session.exitCode.value, 42);
  });

  test('failed snapshot → error state', () async {
    final h = _Harness();
    h.snapshotGate.completeError(Exception('boom'));
    await h.session.attach();
    expect(h.session.state.value, PaneSessionState.error);
  });

  test('sendText and sendKeys route with pane id', () async {
    final h = _Harness();
    await h.session.sendText('ls -la', submit: true);
    await h.session.sendKeys(['ctrl+c']);
    expect(h.sent, [('p1', 'ls -la', true)]);
    expect(h.keys, [
      ['ctrl+c']
    ]);
  });

  test('emulator is locked to host dims', () {
    final h = _Harness();
    expect(h.session.terminal.viewWidth, 20);
    expect(h.session.terminal.viewHeight, 5);
  });
}
