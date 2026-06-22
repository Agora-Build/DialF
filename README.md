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

## Quick install

```sh
# curl (macOS & Linux, arm64/x86_64) — installs dialf + runs dialfd as a boot service
curl -fsSL https://dl.agora.build/dialf/install.sh | bash

# or npm — installs the CLI, then enable the service
npm install -g @agora-build/dialf && sudo dialf service install
```

More options (per-user service, service management, build from source) in
[Install](#install) below.

## Features

- **Make & answer calls** programmatically on the phone's SIM, from the CLI or a job.
- **Auto-answer** an allow-list of numbers (`autopickup`) — `dialfd` answers matching
  inbound calls automatically.
- **Send & receive SMS**; `dialf sms list` reads the phone's real inbox.
- **Scripted audio conversations** via YAML: play a prompt, wait for the caller to stop
  talking (ten-vad end-of-speech), play the next — through the USB sound card.
- **Call recording**: both directions separately (`-rx`/`-tx`) and/or mixed (`-mix`), as
  time-aligned 16 kHz WAVs.
- **Runtime audio injection**: `dialf play <file>` pushes audio out the card mid-call.
- **Zero-config discovery**: the app finds `dialfd` over the LAN via mDNS; works **while the
  phone is locked** (native foreground service) and survives reboots.
- **No bound audio library**: shells out to `aplay`/`arecord`/`ffmpeg`/`sox`/`afplay`,
  auto-detected per platform and overridable in config.
- **Runs anywhere**: macOS & Linux, arm64 & x86_64; ten-vad is always compiled from source
  (ONNX), so even Linux aarch64 / Raspberry Pi works.

See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the wire protocol + local control API, and
[`app/README.md`](app/README.md) for the phone app.

## Install

Prebuilt binaries (macOS arm64/x86_64, Linux x86_64/aarch64) ship on GitHub Releases. Both
installers fetch the binary (with onnxruntime + the ten-vad model bundled) and can register
`dialfd` as a background service.

```sh
# curl — installs the binary and starts dialfd as a boot service (prompts for sudo)
curl -fsSL https://dl.agora.build/dialf/install.sh | bash

# npm — installs the CLI; then enable the service explicitly
npm install -g @agora-build/dialf
sudo dialf service install            # boot service (launchd/systemd)
dialf service install --user          # or, no sudo, runs at login
```

For targets without a prebuilt binary, build from source — see
[Build from source](#build-from-source) below.

Manage the service (launchd LaunchDaemon on macOS / systemd unit on Linux):

```sh
dialf service install [--user] [--config <path>]   # system scope needs sudo
dialf service status  [--user]
dialf service stop|start|uninstall [--user]
```

## Usage

Start the daemon (or install it as a service, above), then drive it with `dialf`:

```sh
dialf daemon                       # run dialfd in the foreground
dialf daemon --dry-audio           # simulate audio steps (no sound card needed)
dialf daemon --with-loopback       # also register an in-process simulated phone for testing

dialf devices                      # list connected phones
dialf call dial   <device> <number>   # place a call
dialf call pickup <device>            # answer the ringing call
dialf call hangup <device>            # end the active call
dialf call reject <device>            # decline the ringing call
dialf call list   <device>            # read the call log (JSON)

dialf sms send <device> <to> <body>   # send a text
dialf sms list <device>               # read recent texts (JSON)

dialf run  <job.yaml> [--device <id>] # run a scripted job
dialf play <file>                     # inject audio out the sound card now
```

`<device>` is the id the phone registered as (see `dialf devices`); omit `--device` on
`run` when exactly one phone is connected. `dialf` talks to `dialfd` over a local control
socket, so it must run on the same host. Tip: pretty-print SMS with `dialf sms list phone1
| python3 -m json.tool`.

### Try it without hardware

```sh
dialf daemon --dry-audio --with-loopback &
dialf devices
dialf call dial loopback 5551234
dialf run server/jobs/sample.yaml
```

### YAML jobs

A job is a list of steps run in order. See `server/jobs/sample.yaml` (two-turn exchange)
and `server/jobs/end-to-end-call.yaml` (dial → greet → Q&A → SMS → hangup).

```yaml
- type: call.dial            # also: call.pickup, call.hangup
  number: "5551234"
- type: audio.play
  file: corpus/turn_taking/en/audio/en_question_short1.wav
- type: audio.wait_for_speech
  end_timeout_ms: 45000      # hard cap waiting for the turn to end
  silence_duration_ms: 3000  # trailing silence that marks end-of-turn
- type: sms.send
  to: "5551234"
  body: "thanks!"
- type: wait  { ms: 1000 }
- type: log   { message: "done" }
```

`audio.wait_for_speech` captures from the card → resamples to 16 kHz → runs ten-vad per
256-sample hop; speech onset followed by `silence_duration_ms` of continuous non-speech ends
the turn (`end_timeout_ms` is the overall cap).

### Sound-card bridge + recording

A USB sound card bridges the phone and the host: card **output → phone mic** (inject
prompts), card **input ← phone earpiece** (capture the far end). Select the card and enable
recording in the config:

```yaml
audio:
  capture_device: "plughw:1,0"   # macOS: the CoreAudio device name, e.g. "USB Audio Device"
  playback_device: "plughw:1,0"
  record_dir: /var/lib/dialf/recordings
  mix_recording: true
```

A recorded job writes (returned by `dialf run`):
- `<job>-rx.wav` — captured from the card (the phone / far end)
- `<job>-tx.wav` — audio injected into the card (our prompts)
- `<job>-mix.wav` — the two summed (when `mix_recording: true`)

List ALSA cards with `arecord -l` (Linux). On macOS, capturing needs Microphone permission
for the host app; Linux/ALSA has no such gate.

### Audio tools (external, configurable)

`dialfd` shells out to whatever is available — no bound audio library:
- **Linux:** `arecord`/`aplay`, or `ffmpeg`, or `sox` (`rec`/`play`)
- **macOS:** `ffmpeg` or `sox` for capture; `afplay`/`ffplay`/`play` for playback

Auto-detected via `PATH`; override the exact command with `audio.capture_cmd` /
`audio.playback_cmd`. Capture must emit raw little-endian s16 mono PCM on stdout.

## Layout

- `server/` — Rust workspace
  - `crates/dialf/` — the `dialf` binary + library (CLI, protocol, audio engine, jobs)
  - `crates/ten-vad-sys/` — FFI bindings to ten-vad (built from source)
  - `jobs/` — sample jobs
- `app/` — Flutter + Kotlin phone app — see [`app/README.md`](app/README.md)
- `corpus/` — audio assets referenced by jobs
- `docs/` — [`PROTOCOL.md`](docs/PROTOCOL.md)

## Build from source

```sh
git submodule update --init --recursive   # ten-vad lives at third_party/ten-vad
cd server
cargo build --release
cargo test --workspace                     # VAD, resample, tooling, job runner
```

ten-vad is **always compiled from source** (the ONNX variant). `build.rs` auto-downloads the
matching **onnxruntime** for the target into `$CARGO_HOME/ten-vad-ort/` (one-time, needs
network); set `ORT_ROOT` (a dir with `include/` + `lib/`) to use your own / build offline.
The model loads via `$TEN_VAD_MODEL`, defaulting to the submodule's
`src/onnx_model/ten-vad.onnx`.

To install the CLI locally: `cargo install --path server/crates/dialf` (puts `dialf` in
`~/.cargo/bin`).

Packaging lives in `scripts/install.sh`, `npm/`, and `.github/workflows/release.yml`
(tag `vX.Y.Z` via `scripts/release.sh`).
