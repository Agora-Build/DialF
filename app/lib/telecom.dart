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

  /// The config the service is actually using: `{device_id, name, key, server}`. On a fresh
  /// install the id/name are derived from the phone (stable 4-digit suffix, persisted
  /// natively) and the key is the default. The UI MUST load this before saving — [saveConfig]
  /// writes back whatever is displayed, so an unloaded field would overwrite the real value.
  static Future<Map<String, String>> savedConfig() async {
    final m = await _m.invokeMethod<Map>('savedConfig');
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
