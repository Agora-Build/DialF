import 'dart:async';
import 'dart:convert';

import 'package:flutter/foundation.dart';
import 'package:nsd/nsd.dart' as nsd;
import 'package:web_socket_channel/web_socket_channel.dart';

import 'telecom.dart';

enum ConnStatus { disconnected, discovering, connecting, connected }

/// Implements the DialF control-plane protocol: discover dialfd via mDNS, connect over
/// WebSocket, authenticate with the shared key, relay commands to [Telecom], and report
/// call state back. Mirrors the Rust `protocol.rs`.
class DialfClient extends ChangeNotifier {
  String deviceId;
  String deviceName;
  String key;

  ConnStatus status = ConnStatus.disconnected;
  String? serverInfo;
  final List<String> log = [];

  WebSocketChannel? _ch;
  Timer? _hb;
  StreamSubscription<Map<String, dynamic>>? _evSub;
  nsd.Discovery? _disc;

  DialfClient({
    this.deviceId = 'phone1',
    this.deviceName = 'DialF Phone',
    this.key = 'change-me',
  });

  void _log(String m) {
    log.insert(0, m);
    if (log.length > 100) log.removeLast();
    notifyListeners();
  }

  /// Discover dialfd on the LAN and connect to the first instance found.
  Future<void> autoConnect() async {
    if (status == ConnStatus.connected) return;
    status = ConnStatus.discovering;
    notifyListeners();
    _log('discovering _dialfd._tcp …');
    try {
      _disc = await nsd.startDiscovery('_dialfd._tcp');
      _disc!.addListener(() {
        if (status == ConnStatus.connected) return;
        for (final s in _disc!.services) {
          final host = s.host;
          final port = s.port;
          if (host != null && port != null) {
            connect(host, port);
            break;
          }
        }
      });
    } catch (e) {
      _log('discovery error: $e');
      status = ConnStatus.disconnected;
      notifyListeners();
    }
  }

  /// Connect directly to a known dialfd address.
  Future<void> connect(String host, int port) async {
    if (status == ConnStatus.connected) return;
    status = ConnStatus.connecting;
    serverInfo = '$host:$port';
    notifyListeners();
    try {
      final ch = WebSocketChannel.connect(Uri.parse('ws://$host:$port'));
      _ch = ch;
      _send({
        'type': 'hello',
        'device_id': deviceId,
        'name': deviceName,
        'key': key,
        'caps': ['call', 'sms'],
        'app_version': '0.1',
      });
      ch.stream.listen(
        _onFrame,
        onDone: _onClosed,
        onError: (Object e) {
          _log('ws error: $e');
          _onClosed();
        },
      );
      status = ConnStatus.connected;
      notifyListeners();
      _log('connected to $host:$port');

      _hb?.cancel();
      _hb = Timer.periodic(const Duration(seconds: 30), (_) {
        _send({'type': 'heartbeat', 'ts': DateTime.now().millisecondsSinceEpoch});
      });
      _evSub ??= Telecom.events().listen(_onTelecomEvent);
      unawaited(Telecom.startService());
    } catch (e) {
      _log('connect failed: $e');
      status = ConnStatus.disconnected;
      notifyListeners();
    }
  }

  void _send(Map<String, dynamic> m) => _ch?.sink.add(jsonEncode(m));

  void _onClosed() {
    if (status == ConnStatus.disconnected) return;
    status = ConnStatus.disconnected;
    _hb?.cancel();
    _ch = null;
    notifyListeners();
    _log('disconnected');
  }

  Future<void> _onFrame(dynamic data) async {
    Map<String, dynamic> m;
    try {
      m = jsonDecode(data as String) as Map<String, dynamic>;
    } catch (_) {
      return;
    }
    if (m['type'] != 'cmd') return;
    final cmdId = m['cmd_id'] as String?;
    final action = m['action'] as String?;
    try {
      switch (action) {
        case 'dial':
          await Telecom.placeCall(m['number'] as String);
          break;
        case 'pickup':
          await Telecom.answer(m['call_id'] as String?);
          break;
        case 'hangup':
          await Telecom.hangup(m['call_id'] as String?);
          break;
        case 'send_sms':
          await Telecom.sendSms(m['to'] as String, m['body'] as String);
          break;
        case 'list_sms':
          final list = await Telecom.listSms();
          for (final s in list) {
            _send({'type': 'sms', ...Map<String, dynamic>.from(s as Map)});
          }
          break;
        case 'set_autopickup':
          // dialfd owns the picklist; nothing to do client-side for now.
          break;
        default:
          _send({'type': 'error', 'cmd_id': cmdId, 'msg': 'unknown action $action'});
          return;
      }
      _log('cmd $action ok');
      _send({'type': 'ack', 'cmd_id': cmdId, 'ok': true});
    } catch (e) {
      _log('cmd $action failed: $e');
      _send({'type': 'error', 'cmd_id': cmdId, 'msg': '$e'});
    }
  }

  void _onTelecomEvent(Map<String, dynamic> e) {
    switch (e['type']) {
      case 'call_state':
        _send({
          'type': 'call_state',
          'call_id': e['call_id'],
          'state': e['state'],
          'number': e['number'],
          'direction': e['direction'],
        });
        _log('call ${e['state']} ${e['number'] ?? ''}');
        break;
      case 'sms':
        _send({
          'type': 'sms',
          'direction': e['direction'] ?? 'in',
          'from': e['from'],
          'to': e['to'],
          'body': e['body'],
          'ts': e['ts'] ?? DateTime.now().millisecondsSinceEpoch,
        });
        _log('sms in from ${e['from']}');
        break;
      case 'dialer_role':
        _log('dialer role granted: ${e['granted']}');
        break;
    }
  }

  Future<void> disconnect() async {
    _ch?.sink.close();
    _onClosed();
    final d = _disc;
    if (d != null) {
      try {
        await nsd.stopDiscovery(d);
      } catch (_) {}
      _disc = null;
    }
    unawaited(Telecom.stopService());
  }

  @override
  void dispose() {
    _hb?.cancel();
    _evSub?.cancel();
    super.dispose();
  }
}
