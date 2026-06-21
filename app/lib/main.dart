import 'package:flutter/material.dart';
import 'package:permission_handler/permission_handler.dart';

import 'client.dart';
import 'telecom.dart';

void main() {
  runApp(const DialfApp());
}

class DialfApp extends StatelessWidget {
  const DialfApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'DialF Phone',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  final client = DialfClient();
  final _id = TextEditingController(text: 'phone1');
  final _name = TextEditingController(text: 'DialF Phone');
  final _key = TextEditingController(text: 'change-me');
  final _addr = TextEditingController(); // optional host:port override
  bool _isDefaultDialer = false;

  @override
  void initState() {
    super.initState();
    _bootstrap();
  }

  Future<void> _bootstrap() async {
    await [Permission.phone, Permission.sms, Permission.notification].request();
    _isDefaultDialer = await Telecom.isDefaultDialer();
    if (mounted) setState(() {});
  }

  void _applyIdentity() {
    client.deviceId = _id.text.trim();
    client.deviceName = _name.text.trim();
    client.key = _key.text;
  }

  Future<void> _refreshDialer() async {
    _isDefaultDialer = await Telecom.isDefaultDialer();
    if (mounted) setState(() {});
  }

  @override
  void dispose() {
    client.dispose();
    _id.dispose();
    _name.dispose();
    _key.dispose();
    _addr.dispose();
    super.dispose();
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('DialF Phone')),
      body: ListenableBuilder(
        listenable: client,
        builder: (context, _) {
          return ListView(
            padding: const EdgeInsets.all(16),
            children: [
              _statusCard(),
              const SizedBox(height: 12),
              _dialerCard(),
              const SizedBox(height: 12),
              _identityCard(),
              const SizedBox(height: 12),
              _connectButtons(),
              const SizedBox(height: 12),
              _logCard(),
            ],
          );
        },
      ),
    );
  }

  Widget _statusCard() {
    final s = client.status;
    final (label, color) = switch (s) {
      ConnStatus.connected => (
          'Connected${client.serverInfo != null ? " · ${client.serverInfo}" : ""}',
          Colors.green
        ),
      ConnStatus.connecting => ('Connecting…', Colors.orange),
      ConnStatus.discovering => ('Discovering dialfd…', Colors.blue),
      ConnStatus.disconnected => ('Disconnected', Colors.grey),
    };
    return Card(
      child: ListTile(
        leading: Icon(Icons.circle, color: color, size: 16),
        title: Text(label),
        subtitle: Text('device: ${client.deviceId}'),
      ),
    );
  }

  Widget _dialerCard() {
    return Card(
      child: ListTile(
        leading: Icon(
          _isDefaultDialer ? Icons.verified : Icons.warning_amber,
          color: _isDefaultDialer ? Colors.green : Colors.orange,
        ),
        title: Text(_isDefaultDialer ? 'Default dialer ✓' : 'Not the default dialer'),
        subtitle: const Text('Required to answer/place calls programmatically'),
        trailing: TextButton(
          onPressed: () async {
            await Telecom.requestDialerRole();
            await Future.delayed(const Duration(seconds: 1));
            await _refreshDialer();
          },
          child: const Text('Set'),
        ),
      ),
    );
  }

  Widget _identityCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          children: [
            TextField(controller: _id, decoration: const InputDecoration(labelText: 'Device id')),
            TextField(controller: _name, decoration: const InputDecoration(labelText: 'Device name')),
            TextField(controller: _key, decoration: const InputDecoration(labelText: 'Shared key')),
            TextField(
              controller: _addr,
              onChanged: (_) => setState(() {}),
              decoration: const InputDecoration(
                labelText: 'dialfd address (optional, host:port)',
                hintText: 'leave blank to auto-discover',
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _connectButtons() {
    final connected = client.status == ConnStatus.connected;
    return Wrap(
      spacing: 8,
      runSpacing: 8,
      children: [
        FilledButton.icon(
          onPressed: connected
              ? null
              : () {
                  _applyIdentity();
                  // Tolerate full-width colon (CJK IMEs) and stray spaces.
                  final addr = _addr.text.trim().replaceAll('：', ':');
                  if (addr.contains(':')) {
                    final parts = addr.split(':');
                    client.connect(
                      parts[0].trim(),
                      int.tryParse(parts[1].trim()) ?? 8765,
                    );
                  } else {
                    client.autoConnect();
                  }
                },
          icon: const Icon(Icons.link),
          label: Text(_addr.text.trim().isEmpty ? 'Auto-connect' : 'Connect'),
        ),
        OutlinedButton.icon(
          onPressed: connected ? () => client.disconnect() : null,
          icon: const Icon(Icons.link_off),
          label: const Text('Disconnect'),
        ),
      ],
    );
  }

  Widget _logCard() {
    return Card(
      child: Padding(
        padding: const EdgeInsets.all(12),
        child: Column(
          crossAxisAlignment: CrossAxisAlignment.start,
          children: [
            const Text('Activity', style: TextStyle(fontWeight: FontWeight.bold)),
            const SizedBox(height: 8),
            if (client.log.isEmpty)
              const Text('—', style: TextStyle(color: Colors.grey))
            else
              ...client.log.take(30).map(
                    (l) => Text(l,
                        style: const TextStyle(fontSize: 12, fontFamily: 'monospace')),
                  ),
          ],
        ),
      ),
    );
  }
}
