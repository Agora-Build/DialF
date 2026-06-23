# DialF Phone (Android app)

The phone side of DialF. It carries **no call audio** — it places/answers/rejects calls,
sends/reads SMS, reads the call log/SIMs, and toggles carrier voicemail on command from
`dialfd`. Call audio is bridged physically through the host's USB sound card; the app's only
audio role is **routing the call to the wired headset** so the bridge can capture/inject it
(see the top-level [`README.md`](../README.md) and [`docs/HARDWARE.md`](../docs/HARDWARE.md)).

App id: `build.agora.dialf_phone`.

## What it does

- Registers as the Android **default dialer** (`ROLE_DIALER` + `InCallService`) to
  programmatically **dial / answer / hang up / reject** calls.
- **SMS** send + inbox read; **call log** and **SIM list** (dual-SIM, default tagged);
  per-call **SIM selection**; carrier **voicemail** on/off and raw **MMI/USSD** — all via
  `dialfd` commands. Inbound calls/SMS are forwarded to `dialfd` in real time.
- **Routes call audio to the wired headset** (`ROUTE_WIRED_HEADSET`, default on) so the USB
  sound-card bridge becomes the call mic/earpiece.
- Runs the WebSocket control connection in a **native foreground service**
  (`ConnForegroundService`), independent of the Flutter UI — works **while locked**, and
  stays up as long as possible (see below).

## Staying connected

The control plane is built to survive sleep, reboots, and network changes:

- **Foreground service** holds the WebSocket; `START_STICKY` + `onTaskRemoved` restart it if
  it's killed or the app is swiped away.
- **Auto-start** on boot (`BootReceiver`), on app open, and on power/wifi/battery/app-update
  broadcasts (`KeepAliveReceiver`).
- **Battery-optimization exemption** (requested on launch) puts the app on the Doze allowlist,
  so commands are delivered and execute even when the phone is asleep.
- **Reconnect backoff** is exponential and charge-aware (tight while charging, relaxed on
  battery), resetting on connect or when the network returns; a default-network callback
  reconnects on wifi changes; NSD discovery runs in bounded windows (not continuous multicast).
- All of this is gated by the **"Keep app running"** switch — turn it off to stop every
  auto-(re)launch.

## Architecture

```
Flutter UI (lib/) ──┐  config + toggles + status (Method/EventChannel)
                    ▼
ConnForegroundService.kt  ── WebSocket(OkHttp) ──▶ dialfd
   │  NSD discovery (bounded), 20s ping, 30s heartbeat, backoff reconnect, net callback
   ├─ Telecom.kt          dial / answer / hangup / reject / sms / callLog / sims / voicemail / mmi
   ├─ DialfInCallService  tracks calls, reports state, pins ROUTE_WIRED_HEADSET
   ├─ SmsReceiver.kt      inbound SMS → dialfd
   └─ Dialf.kt            process-wide bridge (UI sink + service listener)
BootReceiver / KeepAliveReceiver   (re)start the service to keep it alive
```

The UI is just config/status; the service is what matters at runtime.

## Configuration (in the app UI)

| Setting | Meaning |
|---|---|
| Device id | id this phone registers as; defaults to `<phone-name>-NNNN` (stable random suffix) |
| Device name | friendly name in `dialf devices`; defaults to the phone's name / brand+model |
| Shared key | must match `dialfd`'s `shared_key` |
| dialfd address | `host:port` to pin a daemon; **blank = auto-discover via mDNS** |
| Route calls to wired headset | use the USB sound-card bridge for call audio (default on) |
| Keep app running | auto-(re)start on boot/power/network/swipe (default on) |

## Install (prebuilt APK)

Sideload the prebuilt APK (Android 9+, debug-signed) — no toolchain needed:

- Newest build: https://dl.agora.build/dialf/dialf-phone-latest.apk
- All versions: https://github.com/Agora-Build/DialF/releases

```sh
adb install dialf-phone-latest.apk    # or open the .apk on the phone
```

## Build & run

```sh
cd app
flutter pub get
flutter run                       # debug, on a connected device
flutter build apk --release       # release APK
```

First launch: grant phone/SMS/notification permissions, **Allow** the battery-optimization
prompt, tap **Set** for the default-dialer role, enter the shared key (optionally a dialfd
address), then **Start service**. `am start -n build.agora.dialf_phone/.MainActivity --ez
start true` starts it headlessly.

## Notes

- **Cellular call audio** can't be captured/injected by any Android app — hence the USB
  sound-card bridge on the host, and the wired-headset routing here.
- Cleartext `ws://` is allowed (`usesCleartextTraffic`) for the LAN control connection.
- Pinning a **fixed dialfd address** skips mDNS on reconnect (handy if discovery is flaky).
