/// HTTP + WebSocket client for the hyperpanes control API.
///
/// HTTP surface used (see `rs/crates/core/src/control/routes.rs`):
///   GET  /health, GET /state, GET /projects
///   GET  /panes/{id}/output?since=&strip=&tail=&mode=
///   POST /panes/{id}/input        { data, submit } | { keys }
///   POST /command                 { type: newPane|closePane|restartPane|… }
/// WS   /events?token=…            (see events.dart)
///
/// The events channel auto-reconnects with jittered exponential backoff and exposes a
/// broadcast stream plus a connection-state notifier for the UI.
library;

import 'dart:async';
import 'dart:convert';
import 'dart:math';

import 'package:flutter/foundation.dart';
import 'package:http/http.dart' as http;
import 'package:web_socket_channel/web_socket_channel.dart';

import 'events.dart';
import 'models.dart';
import 'pairing.dart';

class ControlApiException implements Exception {
  ControlApiException(this.status, this.message);
  final int status;
  final String message;
  @override
  String toString() => 'ControlApiException($status): $message';
}

/// Snapshot returned by [ControlClient.getOutput].
class OutputSnapshot {
  const OutputSnapshot({
    required this.output,
    required this.cursor,
    this.truncated = false,
  });
  final String output;

  /// Byte cursor matching the end of [output] — splice WS frames with
  /// `frame.cursor > cursor`.
  final int cursor;
  final bool truncated;
}

class ControlClient {
  ControlClient(this.pairing, {http.Client? httpClient})
      : _http = httpClient ?? http.Client();

  final HostPairing pairing;
  final http.Client _http;

  Map<String, String> get _headers => {
        'Authorization': 'Bearer ${pairing.token}',
        'Content-Type': 'application/json',
      };

  Uri _u(String path, [Map<String, String>? q]) =>
      pairing.httpBase.replace(path: path, queryParameters: q);

  Future<Map<String, dynamic>> _get(String path,
      [Map<String, String>? q]) async {
    final res = await _http
        .get(_u(path, q), headers: _headers)
        .timeout(const Duration(seconds: 15));
    return _body(res);
  }

  Future<Map<String, dynamic>> _post(String path, Object payload) async {
    final res = await _http
        .post(_u(path), headers: _headers, body: jsonEncode(payload))
        .timeout(const Duration(seconds: 15));
    return _body(res);
  }

  Map<String, dynamic> _body(http.Response res) {
    final Object? decoded;
    try {
      decoded = jsonDecode(utf8.decode(res.bodyBytes));
    } catch (_) {
      throw ControlApiException(res.statusCode, 'non-JSON response');
    }
    final map = decoded is Map<String, dynamic> ? decoded : <String, dynamic>{};
    if (res.statusCode >= 400) {
      throw ControlApiException(
        res.statusCode,
        map['error']?.toString() ?? 'HTTP ${res.statusCode}',
      );
    }
    return map;
  }

  Future<Map<String, dynamic>> health() => _get('/health');

  Future<HostState> getState() async => HostState.fromJson(await _get('/state'));

  Future<List<ProjectInfo>> getProjects() async {
    final j = await _get('/projects');
    return (j['projects'] as List? ?? [])
        .whereType<Map<String, dynamic>>()
        .map(ProjectInfo.fromJson)
        .whereType<ProjectInfo>()
        .toList();
  }

  /// Raw ANSI replay + cursor for seeding a terminal (or a `since`-sliced tail).
  Future<OutputSnapshot> getOutput(String paneId, {int? since}) async {
    final j = await _get('/panes/$paneId/output', {
      if (since != null) 'since': '$since',
    });
    return OutputSnapshot(
      output: j['output'] as String? ?? '',
      cursor: (j['cursor'] as num?)?.toInt() ?? 0,
      truncated: j['truncated'] == true,
    );
  }

  /// Type text into the pane. `submit` sends a trailing CR as a separate pty write
  /// (host-side beat) so TUIs in bracketed-paste mode read it as Enter.
  Future<void> sendInput(String paneId, String data,
      {bool submit = false}) async {
    await _post('/panes/$paneId/input', {
      'data': data,
      if (submit) 'submit': true,
    });
  }

  /// Send named keys (`enter`, `esc`, `ctrl+c`, `up`, `shift+tab`, …).
  Future<void> sendKeys(String paneId, List<String> keys) async {
    await _post('/panes/$paneId/input', {'keys': keys});
  }

  /// Fire a workspace command (`newPane`, `closePane`, `restartPane`, `renamePane`,
  /// `recolorPane`, `focusPane`, `setLayout`, …).
  Future<Map<String, dynamic>> command(Map<String, dynamic> cmd) =>
      _post('/command', cmd);

  void dispose() => _http.close();
}

enum EventsState { connecting, connected, disconnected }

/// Auto-reconnecting `/events` subscription. One per host; every screen listens to
/// [frames] (broadcast) and filters for what it cares about.
class EventsChannel {
  EventsChannel(this.pairing);

  final HostPairing pairing;

  final _frames = StreamController<ControlFrame>.broadcast();
  final ValueNotifier<EventsState> state =
      ValueNotifier(EventsState.disconnected);

  Stream<ControlFrame> get frames => _frames.stream;

  WebSocketChannel? _channel;
  StreamSubscription? _sub;
  Timer? _retry;
  int _attempt = 0;
  bool _closed = false;
  final _rng = Random();

  void connect() {
    if (_closed) return;
    _retry?.cancel();
    state.value = EventsState.connecting;
    try {
      final ch = WebSocketChannel.connect(pairing.eventsUri);
      _channel = ch;
      _sub = ch.stream.listen(
        (msg) {
          if (state.value != EventsState.connected) {
            state.value = EventsState.connected;
            _attempt = 0;
          }
          if (msg is String) _frames.add(ControlFrame.parse(msg));
        },
        onError: (_) => _scheduleReconnect(),
        onDone: _scheduleReconnect,
        cancelOnError: true,
      );
    } catch (_) {
      _scheduleReconnect();
    }
  }

  void _scheduleReconnect() {
    if (_closed) return;
    state.value = EventsState.disconnected;
    _sub?.cancel();
    _sub = null;
    _channel = null;
    // 0.5s → 1s → 2s … capped at 15s, ±25% jitter.
    final base = min(15000, 500 * (1 << min(_attempt, 5)));
    final delay = (base * (0.75 + _rng.nextDouble() * 0.5)).round();
    _attempt++;
    _retry = Timer(Duration(milliseconds: delay), connect);
  }

  void dispose() {
    _closed = true;
    _retry?.cancel();
    _sub?.cancel();
    _channel?.sink.close();
    _frames.close();
    state.dispose();
  }
}
