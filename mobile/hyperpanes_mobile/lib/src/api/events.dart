/// `/events` WebSocket frame types — mirrors the host's
/// `rs/crates/core/src/control/events.rs::ControlEvent` (tag field: `type`).
///
/// Unknown frame types parse to [UnknownFrame] (never throw): the host adds frames
/// additively and an older app must ignore what it doesn't know.
library;

import 'dart:convert';

sealed class ControlFrame {
  const ControlFrame();

  static ControlFrame parse(String raw) {
    Object? decoded;
    try {
      decoded = jsonDecode(raw);
    } catch (_) {
      return const UnknownFrame('');
    }
    if (decoded is! Map<String, dynamic>) return const UnknownFrame('');
    final j = decoded;
    final type = j['type'];
    if (type is! String) return const UnknownFrame('');
    switch (type) {
      case 'hello':
        return HelloFrame(
          pid: (j['pid'] as num?)?.toInt() ?? 0,
          version: j['version'] as String? ?? '',
        );
      case 'output':
        return OutputFrame(
          sessionUid: j['sessionUid'] as String? ?? '',
          paneId: j['paneId'] as String?,
          data: j['data'] as String? ?? '',
          // Pre-cursor hosts (< this feature) omit it; 0 means "unknown" and the
          // terminal session falls back to apply-everything-after-seed.
          cursor: (j['cursor'] as num?)?.toInt() ?? 0,
        );
      case 'exit':
        return ExitFrame(
          sessionUid: j['sessionUid'] as String? ?? '',
          paneId: j['paneId'] as String?,
          code: (j['code'] as num?)?.toInt() ?? 0,
        );
      case 'activity':
        return ActivityFrame(
          paneId: j['paneId'] as String? ?? '',
          activity: j['activity'] as String? ?? 'idle',
        );
      case 'liveness':
        return LivenessFrame(
          paneId: j['paneId'] as String? ?? '',
          state: j['state'] as String? ?? '',
          exitCode: (j['exitCode'] as num?)?.toInt(),
        );
      case 'message':
        return MessageFrame(
          to: j['to'] as String? ?? '',
          from: j['from'] as String? ?? '',
          seq: (j['seq'] as num?)?.toInt() ?? 0,
          body: j['body'] as String? ?? '',
        );
      case 'command':
        return CommandFrame(
          paneId: j['paneId'] as String? ?? '',
          phase: j['phase'] as String? ?? '',
          code: (j['code'] as num?)?.toInt(),
        );
      case 'supervisor':
        return SupervisorFrame(
          paneId: j['paneId'] as String? ?? '',
          state: j['state'] as String? ?? '',
        );
      case 'state':
        return const StatePing();
      default:
        return UnknownFrame(type);
    }
  }
}

class HelloFrame extends ControlFrame {
  const HelloFrame({required this.pid, required this.version});
  final int pid;
  final String version;
}

class OutputFrame extends ControlFrame {
  const OutputFrame({
    required this.sessionUid,
    required this.paneId,
    required this.data,
    required this.cursor,
  });
  final String sessionUid;
  final String? paneId;
  final String data;

  /// Monotonic byte cursor AFTER this chunk (UTF-16 code units); 0 = unknown host.
  final int cursor;
}

class ExitFrame extends ControlFrame {
  const ExitFrame({
    required this.sessionUid,
    required this.paneId,
    required this.code,
  });
  final String sessionUid;
  final String? paneId;
  final int code;
}

class ActivityFrame extends ControlFrame {
  const ActivityFrame({required this.paneId, required this.activity});
  final String paneId;

  /// `busy | idle | exited` (frozen legacy vocabulary).
  final String activity;
}

class LivenessFrame extends ControlFrame {
  const LivenessFrame({
    required this.paneId,
    required this.state,
    this.exitCode,
  });
  final String paneId;

  /// `working | awaiting-input | done | exited`.
  final String state;
  final int? exitCode;
}

class MessageFrame extends ControlFrame {
  const MessageFrame({
    required this.to,
    required this.from,
    required this.seq,
    required this.body,
  });
  final String to;
  final String from;
  final int seq;
  final String body;
}

class CommandFrame extends ControlFrame {
  const CommandFrame({required this.paneId, required this.phase, this.code});
  final String paneId;
  final String phase; // "start" | "end"
  final int? code;
}

class SupervisorFrame extends ControlFrame {
  const SupervisorFrame({required this.paneId, required this.state});
  final String paneId;
  final String state;
}

/// Coalesced "something structural changed — refetch /state" ping.
class StatePing extends ControlFrame {
  const StatePing();
}

class UnknownFrame extends ControlFrame {
  const UnknownFrame(this.type);
  final String type;
}
