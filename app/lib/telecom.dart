import 'package:flutter/services.dart';

/// Bridge to the native side. The control plane (WebSocket + telephony) runs in the
/// Android foreground service; this just configures it, controls start/stop, requests the
/// dialer role, and receives status/events for display.
class Native {
  static const MethodChannel _m = MethodChannel('dialf/telecom');
  static const EventChannel _e = EventChannel('dialf/events');
  static Stream<Map<String, dynamic>>? _events;

  /// Status + call/SMS/dialer-role events emitted by the native side.
  static Stream<Map<String, dynamic>> events() {
    _events ??= _e
        .receiveBroadcastStream()
        .map((e) => Map<String, dynamic>.from(e as Map));
    return _events!;
  }

  static Future<bool> isDefaultDialer() async =>
      (await _m.invokeMethod<bool>('isDefaultDialer')) ?? false;

  static Future<void> requestDialerRole() => _m.invokeMethod('requestDialerRole');

  /// Persist the service config (device id / name / shared key / optional host:port).
  static Future<void> saveConfig({
    required String deviceId,
    required String name,
    required String key,
    String server = '',
  }) =>
      _m.invokeMethod('saveConfig', {
        'device_id': deviceId,
        'name': name,
        'key': key,
        'server': server,
      });

  /// Start the headless control-plane service (auto-discovers dialfd, runs locked).
  static Future<void> startService() => _m.invokeMethod('startService');

  /// Stop the control-plane service.
  static Future<void> stopService() => _m.invokeMethod('stopService');
}
