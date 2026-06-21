# DialF

Autonomous phone pick/call system. The `dialf` CLI commands a **mobile app** (the phone,
on its SIM) to **make or answer real calls** over WiFi; call audio is bridged through a
**USB sound card** on the host, where a daemon runs scripted audio conversations with
voice-activity detection (ten-vad).

```
dialf (CLI) ──▶ dialfd (host daemon) ──WiFi──▶ mobile app  ── places/answers call on SIM
                     │
                     └─ USB sound card  ◀──physical──▶  phone headset jack   (all call audio)
```

Two planes:
- **Control plane (WiFi):** app ↔ `dialfd` over WebSocket (mDNS discovery + shared key).
  Pickup / dial / SMS / heartbeat. No audio.
- **Audio plane (physical):** phone headset jack ↔ USB sound card on the host. `dialfd`
  owns the audio engine, VAD, recording, and the YAML job runner.

See `.../plans/mellow-spinning-candy.md` for the full design.

## Layout

- `server/` — Rust workspace
  - `crates/dialf/` — the `dialf` binary + library (CLI, protocol, audio engine, jobs)
  - `crates/ten-vad-sys/` — FFI bindings to the prebuilt ten-vad C library
  - `jobs/sample.yaml` — example scripted call
- `app/` — Flutter + Kotlin app (M3, not yet started)
- `corpus/` — audio assets referenced by jobs

## Build

```sh
cd server
cargo build            # workspace
cargo test --workspace # unit tests (pure logic: VAD, resample, tooling, job runner)
```

### ten-vad (VAD engine)
ten-vad is a git **submodule** at `third_party/ten-vad`. After cloning:
```sh
git submodule update --init --recursive
```

**Default = build from source** (the open-source ONNX variant) — works on **any
architecture**, incl. Linux aarch64 / Raspberry Pi. `build.rs` auto-downloads the matching
**onnxruntime** release for your host into `$CARGO_HOME/ten-vad-ort/` (one-time, needs
network). To use your own onnxruntime (offline / CI), set `ORT_ROOT` to a dir with
`include/` + `lib/`.

**Opt-in `prebuilt`** links ten-vad's prebuilt lib instead (faster, no onnxruntime, but
only for shipped platforms: macOS, Linux x64, Android, iOS, Windows):
```sh
# vendor the lib once (see server/crates/ten-vad-sys/vendor/README.md), then:
cargo build --features prebuilt
```

The ONNX model loads via `$TEN_VAD_MODEL` at runtime (defaults to the submodule's
`src/onnx_model/ten-vad.onnx`, baked in at build time).

## Audio tools (external, configurable)
`dialfd` shells out to whatever audio tool is available — no bound audio library:
- **Linux:** `arecord`/`aplay`, or `ffmpeg`, or `sox` (`rec`/`play`)
- **macOS:** `ffmpeg` or `sox` for capture; `afplay`/`ffplay`/`play` for playback

Auto-detected via `PATH`; override the exact command in the config (`audio.capture_cmd` /
`audio.playback_cmd`). Capture must emit raw little-endian s16 mono PCM on stdout.

## Install (daemon as a service)

Prebuilt binaries (macOS arm64/x86_64, Linux x86_64) ship on GitHub Releases. Both
installers fetch the binary (with the ten-vad lib bundled) and can register `dialfd` as a
background service.

```sh
# curl — installs the binary and starts dialfd as a boot service (prompts for sudo)
curl -fsSL https://dl.agora.build/dialf/install.sh | bash

# npm — installs the CLI; then enable the service explicitly
npm install -g @agora-build/dialf
sudo dialf service install            # boot service (launchd/systemd)
# or, no sudo, runs at login:
dialf service install --user
```

Service management (writes a launchd LaunchDaemon on macOS / systemd unit on Linux;
`RunAtLoad`/`enable` + keep-alive restart):

```sh
dialf service install [--user] [--config <path>]   # system scope needs sudo
dialf service status  [--user]
dialf service stop|start|uninstall [--user]
```

### Build from source (any platform/arch)

Prebuilt covers macOS arm64/x86_64 + Linux x86_64. For anything else (e.g. Linux
**aarch64** / Raspberry Pi), `install.sh` **auto-falls back** to an on-device source build;
you can also run it directly:

```sh
curl -fsSL  | bash
```

It installs Rust + build deps (apt/dnf/pacman/brew), clones with submodules, and runs
`cargo build --release` (the default ONNX path — `build.rs` auto-downloads the matching
onnxruntime for the device's OS/arch: darwin arm64/x86_64, linux x64/aarch64), then
installs the service. Works on **mac/linux × arm64/x86_64**.

Packaging lives in `scripts/install.sh`, `scripts/`, `npm/`, and
`.github/workflows/release.yml` (tag `vX.Y.Z` via `scripts/release.sh` → builds binaries +
publishes the Release and npm).

## Status — M1–M4 (M3 verified on a real Pixel; M4 service verified)
Done & tested: workspace; protocol + control-API types; config; YAML job schema + runner;
VAD turn-detector; 16 kHz resampler; external-tool detection/templating; subprocess +
WAV audio backends; audio engine; **ten-vad FFI** (build-from-source default + `prebuilt`
opt-in, both verified); **dialfd daemon** with control socket, device registry, loopback
phone; full `dialf` CLI (`devices`/`call`/`pickup`/`hangup`/`sms`/`run`/`play`). The full
audio pipeline (WAV → resample → ten-vad) and the loopback call flow run end-to-end.

**M2** adds the real phone control plane: WebSocket server + shared-key auth, mDNS
advertisement (`_dialfd._tcp`), device registry, command/ack with timeout, **auto-pickup**
of allow-listed numbers, and phone-driven jobs — all verified end-to-end with a built-in
mock phone.

Run the loopback demo (no hardware):
```sh
cargo run -- daemon --dry-audio &      # dry = simulate audio (no card needed)
cargo run -- devices
cargo run -- call loopback 5551234
cargo run -- run jobs/sample.yaml
```

Try the phone plane with the mock client:
```sh
cargo run -- daemon --dry-audio &
cargo run -- mock-phone --id phone1 --ring 5551234 &   # connects over WS
cargo run -- devices                                   # shows loopback + phone1
cargo run -- call phone1 5559999
cargo run -- run jobs/sample.yaml --device phone1
```

**M3** (Android app) is built and **verified on a real Pixel 9 Pro**: auto-discovers
`dialfd` over WiFi (mDNS), connects with the shared key, registers in `dialf devices`,
default-dialer granted, command round-trip confirmed. See `app/`.

**M4** packaging: `dialf service` installs dialfd as a launchd/systemd service (verified
in user scope); curl + npm installers and a tag-driven release workflow are in place (see
Install above).

Remaining for production: real outbound call / SMS / auto-pickup on a live call, and the
USB sound-card audio bridge (a scripted call through ten-vad) — these need the audio
hardware cabled to the phone.
```
