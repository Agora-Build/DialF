// Smoke test: the app builds and shows its title.

import 'package:flutter_test/flutter_test.dart';

import 'package:dialf_phone/main.dart';

void main() {
  testWidgets('DialfApp renders', (WidgetTester tester) async {
    await tester.pumpWidget(const DialfApp());
    expect(find.text('DialF Phone'), findsWidgets);
  });
}
