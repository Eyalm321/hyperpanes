/// Pairing-URL parsing — the mobile half of the host's `hyperpanes pair` output.
///
/// Canonical form (keep in sync with `rs/crates/app/src/pair.rs`):
///   `hp://<host>:<port>/?token=<token>&v=1`
/// Also accepted, for manual entry: `http://host:port?token=…`, `ws://…`, or a bare
/// `host:port` (token supplied separately in the connect form).
library;

/// One saved/parsed host connection: everything needed to reach a hyperpanes
/// control API.
class HostPairing {
  const HostPairing({
    required this.host,
    required this.port,
    required this.token,
    this.name,
  });

  final String host;
  final int port;
  final String token;

  /// User-visible label; defaults to `host:port` when unset.
  final String? name;

  String get displayName => name ?? '$host:$port';

  /// Base for HTTP calls, e.g. `http://100.71.2.9:51888`.
  Uri get httpBase => Uri(scheme: 'http', host: host, port: port);

  /// The `/events` WebSocket URL (token as query param, mirroring control.json).
  Uri get eventsUri => Uri(
        scheme: 'ws',
        host: host,
        port: port,
        path: '/events',
        queryParameters: {'token': token},
      );

  /// Parse a pairing URL (`hp://`, `http://`, `ws://`) or bare `host:port`.
  /// Returns null when the string can't be a connection target.
  static HostPairing? parse(String input, {String? fallbackToken}) {
    final s = input.trim();
    if (s.isEmpty) return null;
    Uri? uri = Uri.tryParse(s);
    // Bare `host:port` parses as scheme=host — re-parse with a scheme prefix.
    if (uri == null || !s.contains('://')) {
      uri = Uri.tryParse('hp://$s');
    }
    if (uri == null || uri.host.isEmpty) return null;
    final port = uri.hasPort ? uri.port : 0;
    if (port < 1 || port > 65535) return null;
    final token = uri.queryParameters['token'] ?? fallbackToken;
    if (token == null || token.isEmpty) return null;
    return HostPairing(host: uri.host, port: port, token: token);
  }

  /// The canonical pairing URL (what the host QR encodes).
  String toPairingUrl() {
    final h = host.contains(':') ? '[$host]' : host;
    return 'hp://$h:$port/?token=$token&v=1';
  }

  Map<String, dynamic> toJson() => {
        'host': host,
        'port': port,
        'token': token,
        if (name != null) 'name': name,
      };

  static HostPairing? fromJson(Map<String, dynamic> j) {
    final host = j['host'];
    final port = j['port'];
    final token = j['token'];
    if (host is! String || port is! int || token is! String) return null;
    return HostPairing(
      host: host,
      port: port,
      token: token,
      name: j['name'] as String?,
    );
  }
}
