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

  /// Suggested first-run config: `{device_id, name}` derived from the phone's name/brand,
  /// with a stable 4-digit suffix on the id (persisted natively).
  static Future<Map<String, String>> deviceDefaults() async {
    final m = await _m.invokeMethod<Map>('deviceDefaults');
    return m == null ? {} : Map<String, String>.from(m);
  }

  /// Whether the control-plane service is enabled (i.e. it should be running).
  static Future<bool> isServiceEnabled() async =>
      (await _m.invokeMethod<bool>('isServiceEnabled')) ?? false;

  static Future<bool> isDefaultDialer() async =>
      (await _m.invokeMethod<bool>('isDefaultDialer')) ?? false;

  /// App version for the title bar, e.g. "0.1.18(123)".
  static Future<String> appVersion() async =>
      (await _m.invokeMethod<String>('appVersion')) ?? '';

  /// Whether calls are routed to the wired headset (the USB sound-card bridge).
  static Future<bool> getWiredHeadset() async =>
      (await _m.invokeMethod<bool>('getWiredHeadset')) ?? true;

  /// Route calls to the wired headset (bridge) when true, else the earpiece.
  static Future<void> setWiredHeadset(bool wired) =>
      _m.invokeMethod('setWiredHeadset', {'wired': wired});

  /// Whether DialF keeps itself running (auto-restart on boot/power/network/swipe).
  static Future<bool> getKeepRunning() async =>
      (await _m.invokeMethod<bool>('getKeepRunning')) ?? true;

  /// Keep DialF running as long as possible when true; when false, never auto-(re)launch.
  static Future<void> setKeepRunning(bool keep) =>
      _m.invokeMethod('setKeepRunning', {'keep': keep});

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
