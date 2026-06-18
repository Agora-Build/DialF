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

## Status — M1 + M2 complete
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

Next: **M3** (Android app — Flutter UI + Kotlin Telecom/InCallService implementing this
same protocol), then **M4** (packaging + background service).
```
