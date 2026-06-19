import 'package:flutter/services.dart';

/// Thin wrapper over the native Telecom MethodChannel + event EventChannel
/// (implemented in MainActivity.kt / Dialf.kt).
class Telecom {
  static const MethodChannel _m = MethodChannel('dialf/telecom');
  static const EventChannel _e = EventChannel('dialf/events');
  static Stream<Map<String, dynamic>>? _events;

  /// Broadcast stream of native events (call_state, dialer_role).
  static Stream<Map<String, dynamic>> events() {
    _events ??= _e
        .receiveBroadcastStream()
        .map((e) => Map<String, dynamic>.from(e as Map));
    return _events!;
  }

  static Future<bool> isDefaultDialer() async =>
      (await _m.invokeMethod<bool>('isDefaultDialer')) ?? false;

  static Future<void> requestDialerRole() => _m.invokeMethod('requestDialerRole');

  static Future<void> placeCall(String number) =>
      _m.invokeMethod('placeCall', {'number': number});

  static Future<void> answer(String? callId) =>
      _m.invokeMethod('answer', {'call_id': callId});

  static Future<void> hangup(String? callId) =>
      _m.invokeMethod('hangup', {'call_id': callId});

  static Future<void> sendSms(String to, String body) =>
      _m.invokeMethod('sendSms', {'to': to, 'body': body});

  static Future<List<dynamic>> listSms([int limit = 20]) async =>
      (await _m.invokeMethod('listSms', {'limit': limit})) as List<dynamic>? ?? [];

  static Future<void> startService() => _m.invokeMethod('startService');

  static Future<void> stopService() => _m.invokeMethod('stopService');
}
