# DialF

Autonomous phone pick/call system. The `dialf` CLI commands a mobile app (the phone, on its
SIM) to make / answer / reject real calls and send/receive SMS over WiFi, while call audio is
bridged through a USB sound card on the host, where `dialfd` runs scripted audio
conversations with voice-activity detection (ten-vad).

```
dialf (CLI) ──▶ dialfd (host daemon) ──WiFi──▶ mobile app  ── places/answers call on SIM
                     │
                     └─ USB sound card  ◀──physical──▶  phone headset jack   (all call audio)
```

- Make / answer / hang up / **reject** calls — `reject --drop` answers then instantly hangs
  up so callers can't reach voicemail (when the carrier won't disable it).
- **Auto-answer** an allow-list (`autopickup`); **dual-SIM aware** (`sims`, `call dial --sim`).
- Read the **call log**, send & receive **SMS**.
- **Carrier controls**: `voicemail` on/off and raw `mmi`/USSD (reply captured headlessly).
- **Scripted audio conversations** (YAML + ten-vad), call recording, runtime audio injection.
- Works **while the phone is locked**; runs on macOS & Linux, arm64 & x86_64.

See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the wire protocol + control API,
[`docs/HARDWARE.md`](docs/HARDWARE.md) for the sound-card bridge wiring + macOS
microphone/LaunchAgent setup, and [`app/README.md`](app/README.md) for the phone app.

## Install

Prebuilt binaries (macOS arm64/x86_64, Linux x86_64/aarch64) ship on GitHub Releases. The
installers fetch the binary (onnxruntime + ten-vad model bundled) and can register `dialfd`
as a background service.

```sh
# curl — installs the binary and starts dialfd as a boot service (prompts for sudo)
curl -fsSL https://dl.agora.build/dialf/install.sh | bash

# npm — installs the CLI; then enable the service
npm install -g @agora-build/dialf
sudo dialf service install            # boot service (launchd/systemd)
dialf service install --user          # or, no sudo, runs at login
```

Manage the service (launchd LaunchDaemon on macOS / systemd unit on Linux):

```sh
dialf service install [--user] [--config <path>]   # system scope needs sudo
dialf service status|stop|start|uninstall [--user]
```

### Build from source

```sh
git submodule update --init --recursive   # ten-vad lives at third_party/ten-vad
cd server
cargo build --release
cargo install --path crates/dialf          # puts `dialf` in ~/.cargo/bin
```

ten-vad is always compiled from source (the ONNX variant). `build.rs` auto-downloads the
matching onnxruntime into `$CARGO_HOME/ten-vad-ort/` (one-time, needs network); set `ORT_ROOT`
(a dir with `include/` + `lib/`) to use your own / build offline. The model loads via
`$TEN_VAD_MODEL`, defaulting to the submodule's `src/onnx_model/ten-vad.onnx`. Works on any
platform/arch, including Linux aarch64 / Raspberry Pi.

## Commands

Start the daemon (or install it as a service, above), then drive it with `dialf`:

```sh
dialf daemon [--dry-audio] [--with-loopback]   # run dialfd; --dry-audio = no sound card,
                                               #   --with-loopback = in-process test phone
dialf devices                                  # list connected phones
dialf sims <device>                            # list SIMs (slot/number/carrier, default tagged)

dialf call dial   <device> <number> [--sim N]  # place a call (default SIM if --sim omitted)
dialf call pickup <device>                     # answer the ringing call
dialf call hangup <device>                     # end the active call
dialf call reject <device> [--drop]            # decline ringing call (--drop = answer+hangup)
dialf call list   <device> [--human]           # read the call log (JSON, or --human)

dialf sms send <device> <to> <body>            # send a text
dialf sms list <device> [--human]              # read recent texts (JSON, or --human)

dialf voicemail off <device> [--sim N]                  # disable carrier voicemail
dialf voicemail on  <device> [--number <vm#>] [--sim N] # re-enable
dialf mmi <device> <code> [--sim N]            # (advanced) raw MMI/USSD code, returns the reply

dialf run  <job.yaml> [--device <id>]          # run a scripted job
dialf play <file>                              # inject audio out the sound card
```

`<device>` is the id the phone registered as (see `dialf devices`). `dialf` talks to `dialfd`
over a local control socket, so it must run on the same host. `--human` formats
times/numbers/durations; omit it for JSON (scriptable).

### Try it without hardware

```sh
dialf daemon --dry-audio --with-loopback &
dialf call dial loopback 5551234
dialf run server/jobs/sample.yaml
```

## How It Works

Two independent planes:

- **Control plane (WiFi):** app ↔ `dialfd` over WebSocket (mDNS discovery + shared key) —
  dial / pickup / reject / SMS / call-log / heartbeat. No audio. The app runs a native
  foreground service, so it works while the phone is locked and resumes on reboot.
- **Audio plane (physical):** phone headset jack ↔ USB sound card on the host. `dialfd` owns
  the audio engine, VAD, recording, and the YAML job runner. (Android blocks apps from
  capturing cellular-call audio, so audio is bridged physically — never over WiFi.)

### Scripted jobs (YAML)

A job is a list of steps run in order. See `server/jobs/sample.yaml` (two-turn exchange) and
`server/jobs/end-to-end-call.yaml` (dial → greet → Q&A → SMS → hangup).

```yaml
- type: call.dial            # also: call.pickup, call.hangup
  number: "5551234"
- type: audio.play
  file: corpus/turn_taking/en/audio/en_question_short1.wav
- type: audio.wait_for_speech
  end_timeout_ms: 45000      # hard cap waiting for the turn to end
  silence_duration_ms: 3000  # trailing silence that marks end-of-turn
- type: sms.send  { to: "5551234", body: "thanks!" }
- type: wait      { ms: 1000 }
- type: log       { message: "done" }
```

`audio.wait_for_speech` captures from the card → resamples to 16 kHz → runs ten-vad per
256-sample hop; speech onset followed by `silence_duration_ms` of continuous non-speech ends
the turn (`end_timeout_ms` is the overall cap).

### Sound-card bridge + recording

A USB sound card bridges the phone and host: card **output → phone mic** (inject prompts),
card **input ← phone earpiece** (capture the far end). A recorded job writes (paths returned
by `dialf run`):

- `<job>-rx.wav` — captured from the card (the phone / far end)
- `<job>-tx.wav` — audio injected into the card (our prompts)
- `<job>-mix.wav` — the two summed (when `mix_recording: true`)

List ALSA cards with `arecord -l` (Linux). On macOS, capturing needs Microphone permission
for the host app; Linux/ALSA has no such gate.

### Audio tools (external, configurable)

`dialfd` shells out to whatever is available — no bound audio library:

- **Linux:** `arecord`/`aplay`, or `ffmpeg`, or `sox` (`rec`/`play`)
- **macOS:** `ffmpeg` or `sox` for capture; `afplay`/`ffplay`/`play` for playback

Auto-detected via `PATH`; override with `audio.capture_cmd` / `audio.playback_cmd`. Capture
must emit raw little-endian s16 mono PCM on stdout.

## Configuration

`dialfd` reads `~/.config/dialf/config.yaml` (override with `--config`):

```yaml
shared_key: change-me              # must match the app's shared key
ws_bind: "0.0.0.0:8765"            # phone WebSocket server bind
instance_name: dialfd              # mDNS instance name
autopickup: ["+15551234"]          # numbers auto-answered when they ring
audio:
  capture_device: "plughw:1,0"     # macOS: the CoreAudio device name, e.g. "USB Audio Device"
  playback_device: "plughw:1,0"
  record_dir: /var/lib/dialf/recordings
  mix_recording: true
```

The app's shared key / device id / optional fixed `dialfd` address are set in its UI — see
[`app/README.md`](app/README.md).

## Development

```sh
cd server
cargo build
cargo test --workspace            # protocol, VAD, resample, tooling, job runner, formatters
```

Layout:

- `server/` — Rust workspace
  - `crates/dialf/` — the `dialf` binary + library (CLI, protocol, audio engine, jobs)
  - `crates/ten-vad-sys/` — FFI bindings to ten-vad (built from source)
  - `jobs/` — sample jobs
- `app/` — Flutter + Kotlin phone app ([`app/README.md`](app/README.md))
- `corpus/` — audio assets referenced by jobs
- `docs/` — [`PROTOCOL.md`](docs/PROTOCOL.md), [`HARDWARE.md`](docs/HARDWARE.md)
- `config.example.yaml` — sample daemon config (sound card + recording)

### Release

Tag `vX.Y.Z` (via `scripts/release.sh`) triggers `.github/workflows/release.yml`: builds
prebuilt binaries for macOS arm64/x86_64 + Linux x86_64/aarch64 (ten-vad compiled from
source; linux-aarch64 cross-compiled), publishes a GitHub Release, the npm package
(`@agora-build/dialf`), and mirrors the tarballs + `install.sh` to Cloudflare R2
(`dl.agora.build`). Packaging lives in `scripts/` and `npm/`.

## License

MIT — see [LICENSE](LICENSE).
