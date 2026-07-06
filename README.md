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
- **Auto-answer** an allow-list (`autoanswer`); **dual-SIM aware** (`sims`, `call dial --sim`).
- Read the **call log**, send & receive **SMS**.
- **Carrier controls**: `voicemail` on/off and raw `mmi`/USSD (reply captured headlessly).
- **Scripted audio conversations** (YAML + ten-vad), call recording, runtime audio injection.
- Works **while the phone is locked**; runs on macOS & Linux, arm64 & x86_64.

See [`docs/PROTOCOL.md`](docs/PROTOCOL.md) for the wire protocol + control API,
[`docs/HARDWARE.md`](docs/HARDWARE.md) for the sound-card bridge wiring + macOS
microphone/LaunchAgent setup, and [`app/README.md`](app/README.md) for the phone app.

## Install

Prebuilt binaries (macOS arm64/x86_64, Linux x86_64/aarch64) ship on GitHub Releases. Both
installers just install the `dialf` CLI (onnxruntime + ten-vad model bundled) — they don't
start a service; you launch `dialfd` separately.

```sh
npm install -g @agora-build/dialf   # or: curl -fsSL https://dl.agora.build/dialf/install.sh | bash
```

Then launch the daemon — pick one:

```sh
dialf daemon                  # run in the foreground
sudo dialf service install    # system service at boot (launchd/systemd)
dialf service install --user  # per-user service at login (no sudo)
```

**macOS + audio recording:** a system (root) service can't access the microphone, so for
recording on macOS run the **user** service: `dialf service install --user` (details in
[`docs/HARDWARE.md`](docs/HARDWARE.md)). On Linux a system service records fine (no TCC gate).

Manage the service (launchd on macOS / systemd on Linux):

```sh
dialf service status|stop|start|uninstall [--user]
```

(The curl installer is install-only by default; `DIALF_SERVICE=system|user` makes it also
install that service.)

### Upgrading

```sh
npm install -g @agora-build/dialf   # or re-run the curl installer
dialf service install [--user]      # repoint the service at the new binary, reload
```

The new binary installs at a versioned path, so **re-run `dialf service install` after an
upgrade** to point the service at it and reload (idempotent — no uninstall needed). On
**macOS**, the daemon is unsigned, so the OS re-prompts for the **Microphone** the first
time the upgraded daemon records — **Allow** it. (A silent empty `rx.wav` is the tell that
the mic grant is missing; see [`docs/HARDWARE.md`](docs/HARDWARE.md).) The phone reconnects
on its own a few seconds after the reload.

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

## The phone app (DialF Phone)

**DialF Phone** is the Android side — it *is* the phone DialF controls (on its own SIM). It
carries **no call audio**; it executes commands from `dialfd` and routes call audio to the
wired headset so the host's USB sound card becomes the call mic/earpiece.

- **Default dialer** (`ROLE_DIALER` + `InCallService`) → programmatic **dial / answer / hang up / reject**.
- **Dual-SIM aware** — per-call SIM selection, SIM list with the default tagged.
- **SMS** send + live inbox forwarding, **call log**, carrier **voicemail** on/off, raw **MMI/USSD**.
- **Native foreground service** — works while the phone is **locked**, auto-(re)starts on
  boot / power / network changes, and is Doze-exempt so commands run while it's asleep.

Install the APK (Android 9+, sideload — debug-signed):

- Newest build: https://dl.agora.build/dialf/dialf-phone-latest.apk
- All versions: https://github.com/Agora-Build/DialF/releases

```sh
adb install dialf-phone-latest.apk     # or just open the .apk on the phone
```

Full UI/permissions reference: [`app/README.md`](app/README.md).

## Getting started (end-to-end)

Host (the computer running `dialfd`) and phone on the **same WiFi**. Minimal manual run:

1. **Install the CLI** on the host (macOS/Linux): `npm install -g @agora-build/dialf`.
2. **Configure `dialfd`** — create `~/.config/dialf/config.yaml` with a `shared_key` and your
   sound-card devices (copy [`config.example.yaml`](config.example.yaml); see *Configuration*).
3. **Start the daemon**: `dialf daemon` (foreground), or install it as a service (above). On
   macOS use `dialf service install --user` if you need call **recording**.
4. **Wire the sound card** to the phone's headset jack ([`docs/HARDWARE.md`](docs/HARDWARE.md)).
   Skip this if you only need call control / SMS and no audio.
5. **Install & open DialF Phone** (APK above). Grant phone/SMS/notification permissions,
   **Allow** the battery-optimization prompt, and **Set** it as the default dialer.
6. **Pair them**: in the app enter the same **shared key**; leave the dialfd address blank to
   auto-discover over mDNS (or pin `host:port`). Tap **Start service**.
7. **Verify** on the host: `dialf devices` — the phone should appear.
8. **Drive it**:
   ```sh
   dialf sims <device>                         # which SIMs are in the phone
   dialf call dial <device> +15551234          # place a call
   dialf sms send <device> +15551234 "hi"      # send a text
   dialf call list <device> --human            # read the call log
   ```
9. **Run a scripted call** (needs the audio bridge):
   `dialf run server/jobs/outbound-call.yaml --device <device>`.

(`<device>` is the id from `dialf devices`; omit `--device`/`<device>` when exactly one phone
is connected.)

## Commands

Start the daemon (or install it as a service, above), then drive it with `dialf`:

```sh
dialf daemon                                   # run dialfd (control socket + WS + mDNS)
dialf devices                                  # list connected phones (add --human for a readable list)
dialf sims <device>                            # list SIMs (slot/number/carrier, default tagged)

dialf call dial   <device> <number> [--sim N]  # place a call (default SIM if --sim omitted)
dialf call answer <device>                     # answer the ringing call
dialf call hangup <device>                     # end the active call
dialf call reject <device> [--drop]            # decline ringing call (--drop = answer+hangup)
dialf call list   <device> [--human]           # read the call log (JSON, or --human)

dialf sms send <device> <to> <body>            # send a text
dialf sms list <device> [--human]              # read recent texts (JSON, or --human)

dialf voicemail off <device> [--sim N]                  # disable carrier voicemail
dialf voicemail on  <device> [--number <vm#>] [--sim N] # re-enable
dialf mmi <device> <code> [--sim N]            # (advanced) raw MMI/USSD code, returns the reply

dialf run  <job.yaml> [--device <id>]          # run a scripted job once
dialf run  <job.yaml> --autoanswer <numbers>   # serve: answer those numbers with this job (Ctrl-C reverts)
dialf play <file>                              # inject audio out the sound card
dialf --version                                # CLI + running daemon (dialfd) versions
```

`<device>` is the id the phone registered as (see `dialf devices`). `dialf` talks to `dialfd`
over a local control socket, so it must run on the same host. `--human` formats
times/numbers/durations; omit it for JSON (scriptable).

## How It Works

Two independent planes:

- **Control plane (WiFi):** app ↔ `dialfd` over WebSocket (mDNS discovery + shared key) —
  dial / answer / reject / SMS / call-log / heartbeat. No audio. The app runs a native
  foreground service, so it works while the phone is locked and resumes on reboot.
- **Audio plane (physical):** phone headset jack ↔ USB sound card on the host. `dialfd` owns
  the audio engine, VAD, recording, and the YAML job runner. (Android blocks apps from
  capturing cellular-call audio, so audio is bridged physically — never over WiFi.)

### Scripted jobs (YAML)

A job is a list of steps run in order. Examples in `server/jobs/`:
`sample.yaml` (answer + two-turn exchange), `outbound-call.yaml` (dial → greet → Q&A → SMS →
hangup), `inbound-call.yaml` (auto-answer conversation), `live-call-pilot.yaml`
(answer → play → wait → text → hangup), `record-only.yaml` (record the sound card only — no
call; any app's audio).

```yaml
- type: call.dial            # also: call.answer, call.hangup
  number: "5551234"
- type: call.wait_answered   # block until the callee actually answers
  timeout_ms: 30000
- type: audio.play
  file: samples/prompt-en-1.wav
- type: audio.wait_for_speech
  end_timeout_ms: 45000      # hard cap waiting for the turn to end
  silence_duration_ms: 3000  # trailing silence that marks end-of-turn
  onset_duration_ms: 100     # sustained voice needed to count as speech (debounces noise)
- type: sms.send  { to: "5551234", body: "thanks!" }
- type: wait      { ms: 1000 }
- type: log       { message: "done" }
```

`call.wait_answered` waits for the outbound call to reach the answered (`active`) state, so
prompts play only after a real answer (not on a fixed timer). `audio.wait_for_speech` captures
from the card → resamples to 16 kHz → runs ten-vad per 256-sample hop; speech onset (a
continuous `onset_duration_ms` voiced run, so noise/echo doesn't false-trigger) followed by
`silence_duration_ms` of non-speech ends the turn (`end_timeout_ms` is the overall cap).

### Auto-answer inbound calls

`dialfd` can answer incoming calls and run a job in response. In auto-answer mode the **daemon
answers the call itself**, so the job is just the conversation — `call.dial` /
`call.wait_answered` / `call.answer` are skipped (with a warning) if present, so a job can be
shared with normal outbound use (see `server/jobs/inbound-call.yaml`). Two ways to wire which
numbers it answers:

```yaml
# Persistent — in config.yaml. Number → optional job path; null = answer only.
autoanswer:
  "+15551234": jobs/inbound-call.yaml   # answer, then run this job
  "+15559876":                          # answer only (no job)
```

```sh
# Ad-hoc — no config edit, no file change. Foreground "serve": answers these numbers with
# this job (overriding config.yaml) and reverts when you press Ctrl-C.
dialf run server/jobs/inbound-call.yaml --autoanswer +15551234,+15559876
```

Notes: one phone + one sound card, so DialF runs **one call at a time** — a second call that
arrives while one is in progress is **skipped and logged** (it rings/goes to voicemail; only a
brief call-waiting beep, if any, bleeds into the active recording). For the same reason only
**one `dialf run --autoanswer` session** may run at a time; a second is refused. If the
configured job file fails to load, the call is still answered (plain answer, logged).

### Sound-card bridge + recording

A USB sound card bridges the phone and host: card **output → phone mic** (inject prompts),
card **input ← phone earpiece** (capture the far end). A recorded job writes (paths returned
by `dialf run`):

- `<job>-rx.wav` — captured from the card (the phone / far end), mono
- `<job>-tx.wav` — audio injected into the card (our prompts), mono
- `<job>-mix.wav` — **stereo** (when `mix_recording: true`): left = tx, right = rx, so the two
  voices stay separated. Swap with `mix_channels: rx_tx`.

Recording is **full-duplex on a single clock** — rx records continuously for the whole job
(including during playback and `wait` gaps), tx carries each prompt at its true offset, and
all three files are the same length and sample-aligned. That makes them usable for **latency
measurement** (cross-correlate tx vs rx); see [`docs/HARDWARE.md`](docs/HARDWARE.md).

List ALSA cards with `arecord -l` (Linux). On macOS, capturing needs Microphone permission
for the host app; Linux/ALSA has no such gate.

### Audio tools (external, configurable)

`dialfd` shells out to whatever is available — no bound audio library:

- **Linux:** `arecord`/`aplay`, or `ffmpeg`, or `sox` (`rec`/`play`)
- **macOS:** `ffmpeg` or `sox` for capture; `afplay`/`ffplay`/`play` for playback

Auto-detected via `PATH`; override with `audio.capture_cmd` / `audio.playback_cmd`. Capture
must emit raw little-endian s16 mono PCM on stdout.

## Configuration

`dialfd` reads `~/.config/dialf/config.yaml` (override with `dialf daemon --config <path>`, or
bake it into the service with `dialf service install --config <path>` — `--config` is a daemon
option; client commands like `dialf devices`/`run` don't take it):

```yaml
shared_key: change-me              # must match the app's shared key
ws_bind: "0.0.0.0:8765"            # phone WebSocket server bind
instance_name: dialfd              # mDNS instance name
autoanswer:                        # inbound routing: number → optional job path
  "+15551234": jobs/inbound-call.yaml   # answer, then run this job
  "+15559876":                          # answer only
audio:
  capture_device: "plughw:1,0"     # macOS: the CoreAudio device name, e.g. "USB Audio Device"
  playback_device: "plughw:1,0"
  record_dir: /var/lib/dialf/recordings
  mix_recording: true
  mix_channels: tx_rx              # mix.wav: left=tx / right=rx (default); rx_tx swaps
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
- `samples/` — ready-to-use voice prompts for the sample jobs
- `docs/` — [`PROTOCOL.md`](docs/PROTOCOL.md), [`HARDWARE.md`](docs/HARDWARE.md)
- `config.example.yaml` — sample daemon config (sound card + recording)

### Release

Tag `vX.Y.Z` (via `scripts/release.sh`) triggers `.github/workflows/release.yml`.

## License

MIT — see [LICENSE](LICENSE).
