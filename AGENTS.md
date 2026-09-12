# DialF — agent working notes

Context for anyone (human or agent) picking this project up. The README explains what DialF
*is*; this file covers **how we work on it** and the **traps already paid for** — most of the
entries below cost hours of debugging, so read before re-deriving them.

## Ground rules

- **Privacy:** never print a full phone number in terminal output, logs you paste back, or
  commits. Redact with `sed -E 's/(\+?[0-9]{2,3})[0-9]{5,}/\1…/g'`, and grep every staged
  diff for real numbers before committing (the owner's own numbers must never land in the
  repo).
- **Comments:** lean. Don't narrate self-evident code (a log call, a simple assignment).
  Reserve comments for non-obvious *why*, gotchas, invariants. The existing Doze/wake-lock,
  socket-path, and TCC comments are the right calibration.
- **Releases happen only when the owner asks.** Never cut one proactively.
- **Claims need evidence.** "It works" means you ran it — on hardware where hardware is
  involved. Expect to be asked "are you sure?", "no regression?", "did you add tests?", and
  have the answer ready before it's asked.

## Layout

- `server/` — Rust workspace. `crates/dialf` is both the `dialf` CLI and the `dialfd` daemon;
  `crates/ten-vad-sys` is the VAD FFI.
- `app/` — Flutter + Kotlin Android app (the controlled phone). Real logic lives in
  `app/android/app/src/main/kotlin/build/agora/dialf_phone/`.
- `npm/` — the npm package wrapper (`bin/dialf.js` launcher + installer).
- `scripts/install.sh` — curl installer (symlinks the binary directly; no wrapper).

Two planes: **control** (phone ↔ dialfd over WiFi WebSocket, mDNS `_dialfd._tcp`, shared key;
CLI ↔ dialfd over a Unix control socket) and **audio** (physical: USB sound card or BlackHole).
Android forbids capturing call audio, so audio never crosses WiFi.

## Build / test / verify

```sh
cd server && cargo test --workspace          # ~101 tests; keep them green
cd app/android && ./gradlew :app:testDebugUnitTest   # JVM unit tests (no emulator)
cd app && flutter build apk --release --build-name=X.Y.Z --build-number=N
```

- Flutter **relocates the Gradle build dir**: test results are under
  `app/build/app/test-results/`, *not* `app/android/app/build/`. A bare "BUILD SUCCESSFUL"
  can mean the test task had no source — always read the result XML.
- `--build-number` must exceed the versionCode already installed, or `adb install -r` fails
  with `INSTALL_FAILED_VERSION_DOWNGRADE`.
- After `adb install -r` on a force-stopped app, Android sometimes refuses to start the
  process ("reported as REPLACED, but missing application info"). Launch it once from the
  launcher (`adb shell monkey -p build.agora.dialf_phone -c android.intent.category.LAUNCHER 1`).
- Flutter/Android SDK live under `~/Dev` on the owner's Mac (`~/Dev/flutter`).

## Releasing

Bump `version` in `server/Cargo.toml`, run `cargo check` (syncs `Cargo.lock`), commit both as
`release vX.Y.Z`, tag `vX.Y.Z`, push `main` **and** the tag. `.github/workflows/release.yml`
then builds 4 platform tarballs + the APK, publishes to npm, mirrors to R2, and creates the
GitHub release. Verify with `npm view @agora-build/dialf version` and
`gh release view vX.Y.Z --json assets`.

- **npm burns versions permanently.** A failed/partial publish makes that version number
  unusable forever — roll forward to the next patch, never retry the same number.
- npm read-side propagation can lag minutes behind a successful publish; check the job log
  (`+ @agora-build/dialf@X.Y.Z`) before concluding it failed.
- The APK ships in the release, so **app fixes need a release** to reach other phones.

## Traps already paid for

### macOS
- **Microphone (TCC) is the #1 time sink.** The grant is keyed to the *versioned binary path*
  (`vendor/dialf-<ver>-.../dialf`), so **every upgrade needs a re-grant**. Since 0.1.66 the
  daemon asks explicitly via `AVCaptureDevice.requestAccess` (see
  `audio/mic_permission.rs`; `build.rs` embeds an Info.plist with the usage string, which
  macOS requires before it will show the dialog for a bare CLI binary) — the dialog appears
  at daemon start and `dialf run` waits up to 2 min for the click. A previously *denied* app
  is never re-prompted by macOS; only the Settings toggle fixes it.
- **Under tmux/screen**, TCC attributes the mic to the multiplexer → silent capture. Run the
  user LaunchAgent, or a plain Terminal. A **system (root) daemon can never record** (no MDM).
- **SSH can't show TCC dialogs**; Screen Sharing can.
- `launchctl kill` on a `KeepAlive` unit is a **no-op** — launchd resurrects it instantly.
  `service stop` boots the job out instead; `start` bootstraps before kickstart.
- After an npm upgrade the LaunchAgent points at a **deleted** versioned binary → launchd
  crash-loops it (`EX_CONFIG`). `dialf import` self-heals this by re-running `service install`.

### Audio devices
- Some cards expose input and output as **two CoreAudio devices with the same name** (e.g. the
  TI "USB AUDIO  CODEC" — note the double space). sox selects by name and may open the wrong
  half → zero frames. Use `default` (direction-aware) or make the card the system default.
- Channel/port selection is `remix N` in a pinned `capture_cmd` (macOS) or `plughw:CARD,DEV`
  (Linux). On multi-channel interfaces, ch 3/4 are often a **loopback** of our own playback —
  pinning the wrong channel makes tx bleed into rx.
- `capture_device`/`playback_device` still matter with pinned commands: `dialf import`
  verifies them (and carries renames into the argv), and the daemon uses them to reap stray
  audio processes. Keep them in sync with the pinned command.
- The capture tool's stderr is relayed into the daemon log — read it before theorizing.

### Android app
- **Never use a time-limited foreground service type.** `dataSync` is capped at 6h/day on
  Android 15+; at the limit the OS *crashes* the app and every revival re-crashes for hours.
  The service is `specialUse` (with the required justification property) and overrides
  `onTimeout()` defensively.
- **Manifest-declared implicit broadcasts (power/battery/wifi) are not delivered** on Android
  8+. Revival runs on a self-re-arming `AlarmManager` chain (`KeepAliveReceiver`) — alarms
  live in the system, so they survive process death.
- **Don't add WorkManager**: it crashed the release build at startup (R8 strips Room's
  reflectively-loaded classes). The alarm chain replaced it.
- `NsdManager` resolves **one service at a time**; concurrent resolves fail with
  `FAILURE_ALREADY_ACTIVE`. Resolves are queued and retried — an empty `onResolveFailed`
  silently loses daemons (this cost ~90s to find the right daemon on a multi-daemon LAN).
- Pairing on a shared LAN is by **shared key**: dialfd closes a mismatch with code **4001**,
  and the app skips that endpoint for 10 min while continuing discovery
  (`DaemonCandidates`, unit-tested).

### Daemon / CLI
- **The npm launcher must stay transparent**: async `spawn`, survive SIGINT, forward
  SIGTERM/SIGHUP, mirror the child's exit code. `spawnSync` broke Ctrl+C cancel for every npm
  user (the wrapper died, orphaning the binary so the second Ctrl+C could never reach it).
- **Ctrl+C on `dialf run`**: 1st = graceful (current `audio.play`/`wait` finishes), 2nd =
  force-stop the current step (recordings still saved), 3rd = quit the client (the job keeps
  running in the daemon). The job runs **in the daemon**, not the client.
- **mDNS**: a loopback-bound daemon must not advertise, and every daemon start reaps
  *orphaned* advertisers (`ppid == 1`) of any instance name — killed scratch daemons used to
  leave ghost endpoints luring phones for weeks.
- Paths in config/job files may be absolute or relative; relative resolves against **that
  file's own directory** (`resolve_path_under`), never the daemon's CWD (services run `cwd=/`).
- Install scope decides socket sharing: `--user` = private per-user socket; system install =
  one shared socket owned by the `dialf` group (0660). The control socket has **no auth
  beyond fs permissions** — group members can dial/SMS/run jobs.

### Device sharing (`dialf devices share`)
- **The ADB server protocol has no authentication** — its loopback bind *is* its security
  model. Reaching the port means `shell`, APK install, `/sdcard` read/write, screen capture,
  and port-forwarding *into* the device.
- **The default bind is `0.0.0.0:5939` and a token is opt-in.** So the default share is open
  to the network, by deliberate choice: it is meant for a trusted LAN or an overlay
  (Netbird/Tailscale). The CLI prints an unmissable OPEN SHARE warning naming what an
  attacker could do; do not quietly remove it.
- **Tokens are minted per share and never persisted** — `dvs_` + 12 chars, printed once, held
  only in `ResolvedShare`. There is no config field and no file to read one back from, so a
  restarted share always has a new secret and `connect` has no on-disk fallback.
- **The token authenticates; it does not encrypt.** The proxied session after the handshake is
  plaintext. Off-LAN use belongs inside a VPN or SSH tunnel. TLS is deliberately not
  implemented.
- **Shares expire** (1h default, `<= 0` never). Expiry closes the listener *and* cancels
  in-flight connections — otherwise a held `adb shell` would outlive the deadline, which is
  the thing expiry exists to prevent.
- **The shim exists only because adb cannot authenticate.** `adb -H` opens a socket and starts
  speaking adb; it has no handshake hook, so `dialf devices connect` performs one on its
  behalf. Against a share with no token no shim is needed — point adb straight at it, and the
  shim refuses with that advice if pointed there.
- **Don't delete the shim as "redundant with SSH".** It has been questioned three times and
  kept each time. It serves token-protected shares, which is the original feature request
  ("expose an adb server URL"). The considered alternatives were dropping remote access
  entirely and swapping the token for an IP allowlist (spoofable, DHCP-fragile).
- **A byte splice is not enough** — `share/adb.rs` reads each connection's first request so
  `--target` is *enforced*, not advisory. It blocks `host:kill` (a remote `adb kill-server`,
  or just a peer on a mismatched platform-tools version, would otherwise stop the host's adb
  server), filters `host:devices` to shared serials only, and refuses `transport-any` when
  several devices are shared. This holds on an open share too — no token does not mean no
  policy.
- **Tunnel-using tools break over a remote adb server.** scrcpy, `flutter run` and Android
  Studio open their socket with `adb forward`/`adb reverse`, which runs on the *adb server's*
  host and binds `127.0.0.1` there — so the tool on the other machine waits on its own
  localhost forever (scrcpy: "Server connection failed", *after* a successful server push).
  `--forward-port` proxies that port alongside the adb port; it needs a network bind and is
  refused on loopback, where `adb forward` already owns the socket and `ssh -L` is the answer.
  A forward port is gated by peer address, not the token — the tool cannot handshake.
- **adb-over-WiFi serials contain colons** (`192.168.1.5:5555`), so never parse
  `host-serial:<serial>:<cmd>` by splitting on the first colon — match against known serials.
- **A host cannot reach its own overlay IP.** Netbird/Tailscale (100.64/10) route it off-box
  and don't hairpin, so only a *peer* can dial it; testing a share from the sharing machine
  must use its LAN address or loopback, or adb hangs until its connect timeout.
- `ShareHandle::stop` waits for the accept loop to exit rather than aborting it, but the last
  of the socket teardown is the kernel's — re-binding the same port can need ~30ms on macOS.

## Diagnosing a phone that isn't connected

```sh
adb shell dumpsys package build.agora.dialf_phone | grep -E "versionName|stopped"
adb shell dumpsys activity exit-info build.agora.dialf_phone | grep -E "timestamp=|reason="
adb logcat -d -b crash | grep -A20 dialf_phone
adb shell dumpsys activity services build.agora.dialf_phone | grep -E "isForeground|types="
adb logcat -d -s DialfConn | tail -30     # discovery / connect / key-rejection log
grep -E "phone|hello" ~/Library/Logs/dialfd.$(date +%F).log | tail   # daemon side
```

`types=0x40000000` is specialUse (good); `0x00000001` means an old dataSync build.
`reason=4 CRASH(EXCEPTION)` repeating on a widening backoff is the dataSync death cycle.
