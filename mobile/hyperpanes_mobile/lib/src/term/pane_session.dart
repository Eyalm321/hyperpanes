/// One attached pane: an xterm emulator seeded from `GET /output` and spliced onto the
/// live `/events` output stream (the tmux-attach pattern; docs/mobile-client-plan.md §1).
///
/// Attach sequence (why buffering matters):
///   1. subscribe to WS frames FIRST, buffering them,
///   2. `GET /output` → replay text + cursor C,
///   3. feed replay, then flush buffered + live frames applying only `cursor > C`.
/// Frames with `cursor == 0` come from a pre-cursor host — no dedupe is possible, so we
/// apply everything after the seed (worst case: one duplicated batch at the seam).
library;

import 'dart:async';

import 'package:flutter/foundation.dart';
import 'package:xterm/xterm.dart';

import '../api/control_client.dart';
import '../api/events.dart';

enum PaneSessionState { attaching, live, exited, error }

class PaneSession {
  /// Seam-level constructor (unit tests inject fakes); production code uses
  /// [PaneSession.connect].
  PaneSession({
    required this.paneId,
    required this.sessionUid,
    required this._frames,
    required this._fetchOutput,
    required this._sendData,
    required this._sendKeys,
    int? cols,
    int? rows,
  })  : hostCols = cols ?? 80,
        hostRows = rows ?? 24 {
    terminal = Terminal(
      maxLines: 10000,
      // Local echo is the host's job — every keystroke goes over the wire.
      onOutput: _onLocalInput,
    );
    terminal.resize(hostCols, hostRows);
  }

  /// Wire a session to a live [ControlClient] + [EventsChannel].
  factory PaneSession.connect({
    required String paneId,
    required String sessionUid,
    required ControlClient client,
    required EventsChannel events,
    int? cols,
    int? rows,
  }) {
    return PaneSession(
      paneId: paneId,
      sessionUid: sessionUid,
      frames: events.frames,
      fetchOutput: client.getOutput,
      sendData: (id, data, {submit = false}) =>
          client.sendInput(id, data, submit: submit),
      sendKeys: client.sendKeys,
      cols: cols,
      rows: rows,
    );
  }

  final String paneId;
  final String sessionUid;

  final Stream<ControlFrame> _frames;
  final Future<OutputSnapshot> Function(String paneId) _fetchOutput;
  final Future<void> Function(String paneId, String data, {bool submit})
      _sendData;
  final Future<void> Function(String paneId, List<String> keys) _sendKeys;

  /// The host pane's grid — the only width this byte stream renders correctly at.
  /// Mutable: a desktop-side pane resize flows in via `/state` → [updateDims].
  int hostCols;
  int hostRows;

  late final Terminal terminal;
  final ValueNotifier<PaneSessionState> state =
      ValueNotifier(PaneSessionState.attaching);
  final ValueNotifier<int?> exitCode = ValueNotifier(null);

  StreamSubscription<ControlFrame>? _sub;
  final List<OutputFrame> _preSeedBuffer = [];
  bool _seeded = false;
  int _seedCursor = 0;
  bool _disposed = false;

  /// Splice bookkeeping, exposed for tests.
  @visibleForTesting
  int get seedCursor => _seedCursor;

  Future<void> attach() async {
    // 1. Subscribe first — anything arriving during the HTTP fetch gets buffered.
    _sub = _frames.listen(_onFrame);
    try {
      // 2. Snapshot.
      final snap = await _fetchOutput(paneId);
      if (_disposed) return;
      _seedCursor = snap.cursor;
      terminal.write(snap.output);
      // 3. Flush the overlap buffer through the same dedupe gate.
      _seeded = true;
      for (final f in _preSeedBuffer) {
        _applyOutput(f);
      }
      _preSeedBuffer.clear();
      if (state.value == PaneSessionState.attaching) {
        state.value = PaneSessionState.live;
      }
    } catch (e) {
      if (!_disposed) state.value = PaneSessionState.error;
    }
  }

  void _onFrame(ControlFrame f) {
    switch (f) {
      case OutputFrame o when o.paneId == paneId || o.sessionUid == sessionUid:
        if (!_seeded) {
          _preSeedBuffer.add(o);
        } else {
          _applyOutput(o);
        }
      case ExitFrame e when e.paneId == paneId || e.sessionUid == sessionUid:
        exitCode.value = e.code;
        state.value = PaneSessionState.exited;
      default:
        break;
    }
  }

  void _applyOutput(OutputFrame o) {
    // cursor 0 = pre-cursor host → no dedupe possible, apply as-is.
    if (o.cursor != 0 && o.cursor <= _seedCursor) return;
    terminal.write(o.data);
  }

  /// Local keystrokes (xterm's encoded bytes) → the host pty verbatim.
  void _onLocalInput(String data) {
    if (_disposed) return;
    // Fire-and-forget; input errors surface via the connection banner, not per-key.
    unawaited(_sendData(paneId, data).catchError((_) {}));
  }

  /// Follow a host-side pane resize: re-grid the emulator so absolute-cursor
  /// output keeps lining up (the resizing TUI repaints itself right after).
  void updateDims(int? cols, int? rows) {
    if (cols == null || rows == null) return;
    if (cols == hostCols && rows == hostRows) return;
    hostCols = cols;
    hostRows = rows;
    terminal.resize(cols, rows);
  }

  /// Send named keys via the control API's `keys` vocabulary (quick-keys bar).
  Future<void> sendKeys(List<String> keys) => _sendKeys(paneId, keys);

  /// Composer path: paste `text` and (optionally) submit with the host-side CR beat.
  Future<void> sendText(String text, {bool submit = false}) =>
      _sendData(paneId, text, submit: submit);

  void dispose() {
    _disposed = true;
    _sub?.cancel();
    state.dispose();
    exitCode.dispose();
  }
}
