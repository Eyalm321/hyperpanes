/// Saved-host persistence: host/port/name in SharedPreferences, tokens in the platform
/// keystore (flutter_secure_storage) — a pairing token is a full-control credential.
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
    final out = <HostPairing>[];
    try {
      for (final e in (jsonDecode(raw) as List).whereType<Map>()) {
        final host = e['host'] as String?;
        final port = e['port'] as int?;
        if (host == null || port == null) continue;
        final token = await _secure.read(key: _tokenKey(host, port));
        if (token == null) continue;
        out.add(HostPairing(
          host: host,
          port: port,
          token: token,
          name: e['name'] as String?,
        ));
      }
    } catch (_) {
      // Corrupt store → start clean rather than crash the connect screen.
    }
    return out;
  }

  Future<void> save(HostPairing p) async {
    final prefs = await SharedPreferences.getInstance();
    final existing = await load();
    final others =
        existing.where((h) => h.host != p.host || h.port != p.port).toList();
    final all = [p, ...others];
    await _secure.write(key: _tokenKey(p.host, p.port), value: p.token);
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
    await _secure.delete(key: _tokenKey(p.host, p.port));
    await prefs.setString(
      _hostsKey,
      jsonEncode([
        for (final h in rest)
          {'host': h.host, 'port': h.port, if (h.name != null) 'name': h.name}
      ]),
    );
  }

  String _tokenKey(String host, int port) => 'hp.token.$host.$port';
}
