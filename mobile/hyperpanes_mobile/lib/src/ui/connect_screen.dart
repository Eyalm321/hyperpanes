/// Connect screen: saved hosts, QR scan (`hyperpanes pair` on the host prints the code),
/// and manual entry.
library;

import 'package:flutter/material.dart';
import 'package:mobile_scanner/mobile_scanner.dart';

import '../api/pairing.dart';
import '../state/saved_hosts.dart';
import 'dashboard_screen.dart';
import 'theme.dart';

class ConnectScreen extends StatefulWidget {
  const ConnectScreen({super.key});

  @override
  State<ConnectScreen> createState() => _ConnectScreenState();
}

class _ConnectScreenState extends State<ConnectScreen> {
  final _store = SavedHosts();
  List<HostPairing> _saved = [];
  bool _loading = true;

  @override
  void initState() {
    super.initState();
    _reload();
  }

  Future<void> _reload() async {
    final hosts = await _store.load();
    if (!mounted) return;
    setState(() {
      _saved = hosts;
      _loading = false;
    });
  }

  Future<void> _connect(HostPairing p) async {
    await _store.save(p);
    if (!mounted) return;
    await Navigator.of(context).push(
      MaterialPageRoute(builder: (_) => DashboardScreen(pairing: p)),
    );
    _reload();
  }

  Future<void> _scanQr() async {
    final url = await Navigator.of(context).push<String>(
      MaterialPageRoute(builder: (_) => const _QrScanPage()),
    );
    if (url == null || !mounted) return;
    final p = HostPairing.parse(url);
    if (p == null) {
      ScaffoldMessenger.of(context).showSnackBar(
        const SnackBar(content: Text('Not a hyperpanes pairing code')),
      );
      return;
    }
    _connect(p);
  }

  Future<void> _manualEntry() async {
    final p = await showDialog<HostPairing>(
      context: context,
      builder: (_) => const _ManualEntryDialog(),
    );
    if (p != null) _connect(p);
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('hyperpanes')),
      body: _loading
          ? const Center(child: CircularProgressIndicator())
          : ListView(
              padding: const EdgeInsets.all(16),
              children: [
                if (_saved.isEmpty)
                  Padding(
                    padding: const EdgeInsets.symmetric(vertical: 32),
                    child: Column(
                      children: [
                        const Icon(Icons.terminal, size: 56, color: hpTextDim),
                        const SizedBox(height: 12),
                        Text(
                          'Run `hyperpanes pair` on your host,\nthen scan the QR code.',
                          textAlign: TextAlign.center,
                          style: Theme.of(context)
                              .textTheme
                              .bodyMedium
                              ?.copyWith(color: hpTextDim),
                        ),
                      ],
                    ),
                  ),
                for (final h in _saved)
                  Card(
                    child: ListTile(
                      leading: const Icon(Icons.dns_outlined),
                      title: Text(h.displayName),
                      subtitle: Text('${h.host}:${h.port}',
                          style: const TextStyle(color: hpTextDim)),
                      trailing: IconButton(
                        icon: const Icon(Icons.delete_outline),
                        onPressed: () async {
                          await _store.remove(h);
                          _reload();
                        },
                      ),
                      onTap: () => _connect(h),
                    ),
                  ),
                const SizedBox(height: 16),
                FilledButton.icon(
                  onPressed: _scanQr,
                  icon: const Icon(Icons.qr_code_scanner),
                  label: const Text('Scan pairing QR'),
                ),
                const SizedBox(height: 8),
                OutlinedButton.icon(
                  onPressed: _manualEntry,
                  icon: const Icon(Icons.keyboard),
                  label: const Text('Enter manually'),
                ),
              ],
            ),
    );
  }
}

class _QrScanPage extends StatefulWidget {
  const _QrScanPage();

  @override
  State<_QrScanPage> createState() => _QrScanPageState();
}

class _QrScanPageState extends State<_QrScanPage> {
  bool _done = false;

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Scan pairing QR')),
      body: MobileScanner(
        onDetect: (capture) {
          if (_done) return;
          for (final b in capture.barcodes) {
            final v = b.rawValue;
            if (v != null && v.startsWith('hp://')) {
              _done = true;
              Navigator.of(context).pop(v);
              return;
            }
          }
        },
      ),
    );
  }
}

class _ManualEntryDialog extends StatefulWidget {
  const _ManualEntryDialog();

  @override
  State<_ManualEntryDialog> createState() => _ManualEntryDialogState();
}

class _ManualEntryDialogState extends State<_ManualEntryDialog> {
  final _hostCtl = TextEditingController();
  final _tokenCtl = TextEditingController();
  String? _error;

  @override
  void dispose() {
    _hostCtl.dispose();
    _tokenCtl.dispose();
    super.dispose();
  }

  void _submit() {
    final p = HostPairing.parse(
      _hostCtl.text,
      fallbackToken: _tokenCtl.text.trim().isEmpty ? null : _tokenCtl.text.trim(),
    );
    if (p == null) {
      setState(() => _error = 'Need host:port (+ token) or a full hp:// URL');
      return;
    }
    Navigator.of(context).pop(p);
  }

  @override
  Widget build(BuildContext context) {
    return AlertDialog(
      title: const Text('Add host'),
      content: Column(
        mainAxisSize: MainAxisSize.min,
        children: [
          TextField(
            controller: _hostCtl,
            decoration: const InputDecoration(
              labelText: 'Host (host:port or hp:// URL)',
              hintText: '100.71.2.9:51888',
            ),
            autocorrect: false,
          ),
          TextField(
            controller: _tokenCtl,
            decoration: const InputDecoration(
              labelText: 'Token (from control.json / pair URL)',
            ),
            autocorrect: false,
            obscureText: true,
          ),
          if (_error != null)
            Padding(
              padding: const EdgeInsets.only(top: 8),
              child: Text(_error!,
                  style: const TextStyle(color: Colors.redAccent)),
            ),
        ],
      ),
      actions: [
        TextButton(
          onPressed: () => Navigator.of(context).pop(),
          child: const Text('Cancel'),
        ),
        FilledButton(onPressed: _submit, child: const Text('Connect')),
      ],
    );
  }
}
