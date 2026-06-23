import 'dart:async';

import 'package:flutter/material.dart';
import 'package:permission_handler/permission_handler.dart';

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
  final _id = TextEditingController();
  final _name = TextEditingController();
  final _key = TextEditingController(text: 'change-me');
  final _addr = TextEditingController();

  bool _isDefaultDialer = false;
  bool _wiredHeadset = true;
  bool _keepRunning = true;
  bool _connected = false;
  String? _server;
  bool _running = false;
  final List<String> _log = [];
  StreamSubscription<Map<String, dynamic>>? _sub;

  @override
  void initState() {
    super.initState();
    _sub = Native.events().listen(_onEvent);
    _bootstrap();
  }

  Future<void> _bootstrap() async {
    await [Permission.phone, Permission.sms, Permission.notification].request();
    final defaults = await Native.deviceDefaults();
    if (_id.text.isEmpty) _id.text = defaults['device_id'] ?? 'phone1';
    if (_name.text.isEmpty) _name.text = defaults['name'] ?? 'DialF Phone';
    _isDefaultDialer = await Native.isDefaultDialer();
    _wiredHeadset = await Native.getWiredHeadset();
    _keepRunning = await Native.getKeepRunning();
    if (mounted) setState(() {});
  }

  void _onEvent(Map<String, dynamic> e) {
    switch (e['type']) {
      case 'status':
        _connected = e['connected'] == true;
        _server = e['server'] as String?;
        _logLine(_connected ? 'connected ${_server ?? ''}' : 'disconnected');
        break;
      case 'call_state':
        _logLine('call ${e['state']} ${e['number'] ?? ''}');
        break;
      case 'sms':
        _logLine('sms ${e['direction']} ${e['from'] ?? ''}');
        break;
      case 'dialer_role':
        _isDefaultDialer = e['granted'] == true;
        _logLine('dialer role: ${e['granted']}');
        break;
    }
    if (mounted) setState(() {});
  }

  void _logLine(String m) {
    final n = DateTime.now();
    String two(int x) => x.toString().padLeft(2, '0');
    final ts = '${two(n.hour)}:${two(n.minute)}:${two(n.second)}';
    _log.insert(0, '$ts  $m');
    if (_log.length > 50) _log.removeLast();
  }

  Future<void> _start() async {
    await Native.saveConfig(
      deviceId: _id.text.trim(),
      name: _name.text.trim(),
      key: _key.text,
      server: _addr.text.trim(),
    );
    await Native.startService();
    setState(() => _running = true);
  }

  Future<void> _stop() async {
    await Native.stopService();
    setState(() {
      _running = false;
      _connected = false;
    });
  }

  @override
  void dispose() {
    _sub?.cancel();
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
      body: ListView(
        padding: const EdgeInsets.all(16),
        children: [
          _statusCard(),
          const SizedBox(height: 12),
          _dialerCard(),
          const SizedBox(height: 12),
          _audioCard(),
          const SizedBox(height: 12),
          _keepRunningCard(),
          const SizedBox(height: 12),
          _configCard(),
          const SizedBox(height: 12),
          _buttons(),
          const SizedBox(height: 12),
          _logCard(),
        ],
      ),
    );
  }

  Widget _statusCard() {
    final (label, color) = _connected
        ? ('Connected${_server != null ? " · $_server" : ""}', Colors.green)
        : _running
            ? ('Service running — discovering…', Colors.orange)
            : ('Stopped', Colors.grey);
    return Card(
      child: ListTile(
        leading: Icon(Icons.circle, color: color, size: 16),
        title: Text(label),
        subtitle: const Text('control plane runs in a background service (works locked)'),
      ),
    );
  }

  Widget _audioCard() {
    return Card(
      child: SwitchListTile(
        secondary: Icon(
          _wiredHeadset ? Icons.headset_mic : Icons.phone_in_talk,
          color: _wiredHeadset ? Colors.green : Colors.grey,
        ),
        title: const Text('Route calls to wired headset'),
        subtitle: const Text('Use the USB sound-card bridge for call audio (default)'),
        value: _wiredHeadset,
        onChanged: (v) async {
          await Native.setWiredHeadset(v);
          setState(() => _wiredHeadset = v);
        },
      ),
    );
  }

  Widget _keepRunningCard() {
    return Card(
      child: SwitchListTile(
        secondary: Icon(
          _keepRunning ? Icons.lock_clock : Icons.timer_off,
          color: _keepRunning ? Colors.green : Colors.grey,
        ),
        title: const Text('Keep app running'),
        subtitle: const Text('Auto-restart on boot / power / network / swipe (default)'),
        value: _keepRunning,
        onChanged: (v) async {
          await Native.setKeepRunning(v);
          setState(() => _keepRunning = v);
        },
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
            await Native.requestDialerRole();
            await Future.delayed(const Duration(seconds: 1));
            _isDefaultDialer = await Native.isDefaultDialer();
            if (mounted) setState(() {});
          },
          child: const Text('Set'),
        ),
      ),
    );
  }

  Widget _configCard() {
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
              decoration: const InputDecoration(
                labelText: 'dialfd address (optional, host:port)',
                hintText: 'leave blank to auto-discover (mDNS)',
              ),
            ),
          ],
        ),
      ),
    );
  }

  Widget _buttons() {
    return Wrap(
      spacing: 8,
      runSpacing: 8,
      children: [
        FilledButton.icon(
          onPressed: _running ? null : _start,
          icon: const Icon(Icons.play_arrow),
          label: const Text('Start service'),
        ),
        OutlinedButton.icon(
          onPressed: _running ? _stop : null,
          icon: const Icon(Icons.stop),
          label: const Text('Stop'),
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
            if (_log.isEmpty)
              const Text('—', style: TextStyle(color: Colors.grey))
            else
              ..._log.take(30).map(
                    (l) => Text(l, style: const TextStyle(fontSize: 12, fontFamily: 'monospace')),
                  ),
          ],
        ),
      ),
    );
  }
}
