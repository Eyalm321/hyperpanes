/// The live connection to one host: `/state` tree + per-pane liveness, kept fresh from
/// the events stream. Dashboard-cheap by design: output frames are NEVER handled here —
/// only PaneSession (the open terminal) consumes them.
library;

import 'dart:async';

import 'package:flutter/foundation.dart';

import '../api/control_client.dart';
import '../api/events.dart';
import '../api/models.dart';
import '../api/pairing.dart';

class HostSession extends ChangeNotifier {
  HostSession(this.pairing)
      : client = ControlClient(pairing),
        events = EventsChannel(pairing);

  final HostPairing pairing;
  final ControlClient client;
  final EventsChannel events;

  HostState state = HostState.empty;
  String? lastError;

  /// paneId → precise liveness (`working | awaiting-input | done | exited`), from
  /// `liveness` frames; falls back to the legacy `activity` field on `/state`.
  final Map<String, String> liveness = {};

  /// Fires when a pane flips INTO `awaiting-input` (notification hook).
  final ValueNotifier<String?> awaitingInputPane = ValueNotifier(null);

  StreamSubscription<ControlFrame>? _sub;
  Timer? _refetchDebounce;
  bool _disposed = false;

  Future<void> start() async {
    events.connect();
    _sub = events.frames.listen(_onFrame);
    await refresh();
  }

  Future<void> refresh() async {
    try {
      final s = await client.getState();
      if (_disposed) return;
      state = s;
      lastError = null;
      // Seed liveness for panes we haven't heard frames about.
      for (final p in s.allPanes) {
        liveness.putIfAbsent(
          p.id,
          () => switch (p.activity) {
            'busy' => 'working',
            'exited' => 'exited',
            _ => 'done',
          },
        );
      }
      liveness.removeWhere((id, _) => s.paneById(id) == null);
      notifyListeners();
    } catch (e) {
      if (_disposed) return;
      lastError = e.toString();
      notifyListeners();
    }
  }

  void _onFrame(ControlFrame f) {
    switch (f) {
      case StatePing():
        // Host coalesces (~100ms); debounce our refetch on top.
        _refetchDebounce?.cancel();
        _refetchDebounce =
            Timer(const Duration(milliseconds: 150), () => unawaited(refresh()));
      case LivenessFrame l:
        final prev = liveness[l.paneId];
        liveness[l.paneId] = l.state;
        if (l.state == 'awaiting-input' && prev != 'awaiting-input') {
          awaitingInputPane.value = l.paneId;
        }
        notifyListeners();
      case ActivityFrame a:
        // Legacy fallback — only overwrite when no precise state is known yet or the
        // precise map says something compatible.
        final mapped = switch (a.activity) {
          'busy' => 'working',
          'exited' => 'exited',
          _ => 'done',
        };
        final prev = liveness[a.paneId];
        // Never let coarse "idle" clobber the precise awaiting-input signal.
        if (prev != 'awaiting-input' || mapped == 'exited') {
          liveness[a.paneId] = mapped;
        }
        notifyListeners();
      case ExitFrame e:
        if (e.paneId != null) {
          liveness[e.paneId!] = 'exited';
        }
        notifyListeners();
      default:
        break;
    }
  }

  /// `working`-count across the host (app-bar badge).
  int get workingCount =>
      liveness.values.where((s) => s == 'working').length;

  @override
  void dispose() {
    _disposed = true;
    _refetchDebounce?.cancel();
    _sub?.cancel();
    events.dispose();
    client.dispose();
    awaitingInputPane.dispose();
    super.dispose();
  }
}
