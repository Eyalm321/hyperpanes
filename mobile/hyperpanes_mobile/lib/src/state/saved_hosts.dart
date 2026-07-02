/// Saved-host persistence: host/port/name in SharedPreferences, tokens preferentially
/// in the platform keystore (flutter_secure_storage) — a pairing token is a
/// full-control credential.
///
/// Keystore-backed storage is FLAKY on some devices (Samsung: reads silently return
/// null / throw after a process restart), which made saved hosts vanish. So the token
/// is dual-written: keystore first, SharedPreferences as fallback, and load() takes
/// whichever is readable. Losing hosts is worse than the marginal risk of a plaintext
/// token in app-private prefs (same app-sandbox protection as the rest of the app).
library;

import 'dart:convert';

import 'package:flutter_secure_storage/flutter_secure_storage.dart';
import 'package:shared_preferences/shared_preferences.dart';

import '../api/pairing.dart';

class SavedHosts {
  SavedHosts({FlutterSecureStorage? storage})
      : _secure = storage ?? const FlutterSecureStorage();

  static const _hostsKey = 'hp.hosts.v1';
  final FlutterSecureStorage _secure;

  Future<List<HostPairing>> load() async {
    final prefs = await SharedPreferences.getInstance();
    final raw = prefs.getString(_hostsKey);
    if (raw == null) return [];
    List<Map> entries;
    try {
      entries = (jsonDecode(raw) as List).whereType<Map>().toList();
    } catch (_) {
      return []; // corrupt index → start clean rather than crash
    }
    final out = <HostPairing>[];
    for (final e in entries) {
      final host = e['host'] as String?;
      final port = e['port'] as int?;
      if (host == null || port == null) continue;
      // Per-entry recovery: one unreadable token must not hide the other hosts.
      String? token;
      try {
        token = await _secure.read(key: _tokenKey(host, port));
      } catch (_) {
        token = null;
      }
      token ??= prefs.getString(_fallbackTokenKey(host, port));
      if (token == null) continue;
      out.add(HostPairing(
        host: host,
        port: port,
        token: token,
        name: e['name'] as String?,
      ));
    }
    return out;
  }

  Future<void> save(HostPairing p) async {
    final prefs = await SharedPreferences.getInstance();
    final existing = await load();
    final others =
        existing.where((h) => h.host != p.host || h.port != p.port).toList();
    final all = [p, ...others];
    // Keystore write is best-effort; the prefs fallback below is what guarantees
    // the host survives a restart on keystore-flaky devices.
    try {
      await _secure.write(key: _tokenKey(p.host, p.port), value: p.token);
    } catch (_) {}
    await prefs.setString(_fallbackTokenKey(p.host, p.port), p.token);
    await prefs.setString(
      _hostsKey,
      jsonEncode([
        for (final h in all)
          {'host': h.host, 'port': h.port, if (h.name != null) 'name': h.name}
      ]),
    );
  }

  Future<void> remove(HostPairing p) async {
    final prefs = await SharedPreferences.getInstance();
    final existing = await load();
    final rest =
        existing.where((h) => h.host != p.host || h.port != p.port).toList();
    try {
      await _secure.delete(key: _tokenKey(p.host, p.port));
    } catch (_) {}
    await prefs.remove(_fallbackTokenKey(p.host, p.port));
    await prefs.setString(
      _hostsKey,
      jsonEncode([
        for (final h in rest)
          {'host': h.host, 'port': h.port, if (h.name != null) 'name': h.name}
      ]),
    );
  }

  String _tokenKey(String host, int port) => 'hp.token.$host.$port';
  String _fallbackTokenKey(String host, int port) => 'hp.tokfb.$host.$port';
}
