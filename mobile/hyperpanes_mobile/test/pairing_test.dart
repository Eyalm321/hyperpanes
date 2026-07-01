import 'package:flutter_test/flutter_test.dart';
import 'package:hyperpanes_mobile/src/api/pairing.dart';

void main() {
  group('HostPairing.parse', () {
    test('canonical hp:// pairing URL', () {
      final p = HostPairing.parse('hp://100.71.2.9:51888/?token=abc123&v=1');
      expect(p, isNotNull);
      expect(p!.host, '100.71.2.9');
      expect(p.port, 51888);
      expect(p.token, 'abc123');
    });

    test('bare host:port with fallback token', () {
      final p = HostPairing.parse('192.168.0.5:4000', fallbackToken: 'tok');
      expect(p, isNotNull);
      expect(p!.host, '192.168.0.5');
      expect(p.port, 4000);
      expect(p.token, 'tok');
    });

    test('rejects missing token / port / garbage', () {
      expect(HostPairing.parse('hp://1.2.3.4:5/?v=1'), isNull);
      expect(HostPairing.parse('1.2.3.4', fallbackToken: 't'), isNull);
      expect(HostPairing.parse(''), isNull);
      expect(HostPairing.parse('   '), isNull);
    });

    test('IPv6 host round-trips', () {
      final p = HostPairing.parse('hp://[fd7a::1]:51888/?token=t&v=1');
      expect(p, isNotNull);
      expect(p!.host, 'fd7a::1');
      expect(p.toPairingUrl(), 'hp://[fd7a::1]:51888/?token=t&v=1');
    });

    test('URIs built from pairing', () {
      final p = HostPairing(host: 'h.local', port: 1234, token: 'x');
      expect(p.httpBase.toString(), 'http://h.local:1234');
      expect(p.eventsUri.toString(), 'ws://h.local:1234/events?token=x');
    });
  });
}
