# DialF Phone (Android app)

The phone side of DialF. It is the **control plane only** — it places/answers calls and
sends/reads SMS on command from `dialfd`. It never records, streams, or plays call audio;
all call audio is bridged physically through the host's USB sound card (see the top-level
[`README.md`](../README.md)).

App id: `build.agora.dialf_phone`.

## What it does

- Registers as the Android **default dialer** (`ROLE_DIALER` + `InCallService`) so it can
  programmatically **answer / place / hang up** calls.
- Sends and lists **SMS** (`SmsManager`), and forwards **inbound** texts/calls to `dialfd`.
- Runs the WebSocket control connection in a **native foreground service**
  (`ConnForegroundService`), independent of the Flutter UI — so it keeps working **while the
  phone is locked** and restarts on boot (`BootReceiver`).
- **Auto-discovers** `dialfd` on the LAN via mDNS/NSD (`_dialfd._tcp`), or connects to a
  **fixed address** if one is configured.

## Architecture

```
Flutter UI (lib/) ──┐
                    │ config + status (MethodChannel/EventChannel)
                    ▼
ConnForegroundService.kt  ── WebSocket(OkHttp) ──▶ dialfd
   │  NSD discovery, 20s ping, 30s heartbeat, reconnect, command dispatch
   ├─ Telecom.kt          place / answer / hangup / sendSms / listSms
   ├─ DialfInCallService  tracks live calls, reports state
   ├─ SmsReceiver.kt      inbound SMS → dialfd
   └─ Dialf.kt            process-wide bridge (UI sink + service listener)
BootReceiver.kt           start service on BOOT_COMPLETED (if enabled)
```

The UI is just for configuration/status; the service is the part that matters at runtime.

## Configuration (set in the app UI)

| Field            | Meaning                                                        |
|------------------|---------------------------------------------------------------|
| Device id        | id this phone registers as; defaults to `<phone-name>-NNNN` (4 random digits, stable) |
| Device name      | friendly name shown in `dialf devices`; defaults to the phone's name / brand+model |
| Shared key       | must match `dialfd`'s `shared_key`                            |
| dialfd address   | `host:port` to pin a daemon; **leave blank to auto-discover** |

**Discovery vs. fixed address** (`ConnForegroundService.connectOrDiscover`): if the address
field contains `host:port` the service connects there directly on every (re)connect; if it's
blank it discovers via mDNS. A fixed address also avoids the **Doze** caveat below.

## Build & run

```sh
cd app
flutter pub get
flutter run                       # debug, on a connected device
flutter build apk --release       # release APK
```

First launch: grant phone/SMS/notification permissions, tap **Set** to become the default
dialer, fill in the shared key (and optionally the dialfd address), then **Start service**.

`am start -n build.agora.dialf_phone/.MainActivity --ez start true` starts the service
headlessly (also used to auto-resume).

## Caveats

- **Doze:** an established connection survives Doze, but *re-establishing* one needs
  multicast, which Doze blocks — so after the daemon restarts while the screen is off,
  reconnect can stall until the screen wakes. Avoid this by setting a **fixed dialfd
  address** (direct TCP, no multicast) and/or exempting the app from battery optimization.
- **Cellular call audio** cannot be captured/injected by any Android app — that's the whole
  reason for the USB sound-card bridge on the host.
- Cleartext `ws://` is allowed (`usesCleartextTraffic`) for the LAN control connection.
