// The config screen must load what is stored before it can save. It writes back whatever it
// displays, so a field it never filled in would overwrite the real value with a default —
// which is how tapping "Start service" used to reset the shared key to `change-me` and
// silently unpair the phone.

import 'package:flutter/material.dart';
import 'package:flutter/services.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:dialf_phone/main.dart';

const _telecom = MethodChannel('dialf/telecom');
const _events = MethodChannel('dialf/events');
const _permissions = MethodChannel('flutter.baseflow.com/permissions/methods');

void main() {
  TestWidgetsFlutterBinding.ensureInitialized();

  late Map<String, String> stored;
  late List<Map<dynamic, dynamic>> saves;

  void mockNative() {
    final messenger = TestDefaultBinaryMessengerBinding.instance.defaultBinaryMessenger;

    messenger.setMockMethodCallHandler(_telecom, (call) async {
      switch (call.method) {
        case 'savedConfig':
          return stored;
        case 'saveConfig':
          saves.add(call.arguments as Map);
          return null;
        case 'isDefaultDialer':
        case 'isServiceEnabled':
          return false;
        case 'getWiredHeadset':
        case 'getKeepRunning':
          return true;
        case 'appVersion':
          return '0.2.2(74)';
        default:
          return null;
      }
    });
    messenger.setMockMethodCallHandler(_events, (call) async => null);
    // permission_handler returns a map of {permission index: status index}; 1 == granted.
    messenger.setMockMethodCallHandler(
      _permissions,
      (call) async => call.method == 'requestPermissions' ? {6: 1, 10: 1, 17: 1} : 1,
    );
  }

  setUp(() {
    saves = [];
    stored = {
      'device_id': 'pixel-9-pro-4827',
      'name': 'Pixel 9 Pro',
      'key': 'real-lan-key',
      'server': '192.168.1.50:8765',
    };
    mockNative();
  });

  Future<void> openApp(WidgetTester tester) async {
    // Tall enough that the lazy ListView builds every card, including the buttons.
    await tester.binding.setSurfaceSize(const Size(1000, 3000));
    addTearDown(() => tester.binding.setSurfaceSize(null));
    await tester.pumpWidget(const DialfApp());
    await tester.pumpAndSettle();
  }

  Future<void> tapStart(WidgetTester tester) async {
    await tester.tap(find.text('Start service'));
    await tester.pumpAndSettle();
  }

  testWidgets('the saved shared key and pinned address are shown, not the defaults',
      (tester) async {
    await openApp(tester);

    expect(find.widgetWithText(TextField, 'real-lan-key'), findsOneWidget);
    expect(find.widgetWithText(TextField, '192.168.1.50:8765'), findsOneWidget);
    expect(find.widgetWithText(TextField, 'change-me'), findsNothing);
  });

  testWidgets('Start service saves the stored key back, not a default', (tester) async {
    await openApp(tester);
    await tapStart(tester);

    expect(saves, hasLength(1));
    expect(saves.single['key'], 'real-lan-key');
    expect(saves.single['server'], '192.168.1.50:8765');
  });

  testWidgets('an edited key is what gets saved', (tester) async {
    await openApp(tester);
    await tester.enterText(find.widgetWithText(TextField, 'real-lan-key'), 'verify-key-123');
    await tapStart(tester);

    expect(saves.single['key'], 'verify-key-123');
  });

  testWidgets('clearing the address really does clear the pin', (tester) async {
    // Blank server is a meaningful setting — it hands discovery back to mDNS — unlike a
    // blank key, which the native side treats as "the UI never loaded it".
    await openApp(tester);
    await tester.enterText(find.widgetWithText(TextField, '192.168.1.50:8765'), '');
    await tapStart(tester);

    expect(saves.single['server'], '');
    expect(saves.single['key'], 'real-lan-key');
  });

  testWidgets('a fresh install still shows the native defaults', (tester) async {
    stored = {'device_id': 'phone-1234', 'name': 'Phone', 'key': 'change-me', 'server': ''};
    await openApp(tester);

    expect(find.widgetWithText(TextField, 'change-me'), findsOneWidget);
  });
}
