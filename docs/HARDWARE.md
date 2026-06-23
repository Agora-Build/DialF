# Audio bridge & host setup

DialF's audio plane is physical: a USB sound card bridges the phone and the host, because
Android won't let an app capture/inject cellular-call audio. `dialfd` plays prompts out the
card and records the far end from the card. This guide covers the wiring, the phone routing,
and — the fiddly part — running `dialfd` so it can actually capture the mic on macOS.

## Quick start: record a call

1. **Install the CLI** (host = macOS or Linux):
   ```sh
   npm install -g @agora-build/dialf   # or: curl -fsSL https://dl.agora.build/dialf/install.sh | bash
   ```
2. **Wire the bridge** (diagram below) and set the card's input gain.
3. **Configure** `~/.config/dialf/config.yaml` — `audio.capture_device`/`playback_device` =
   your card, `record_dir`, `mix_recording: true` (see `config.example.yaml`).
4. **Run dialfd where it can use the mic:**
   - **Linux:** `dialf service install` (systemd, records headless — no permission gate), or `dialf daemon`.
   - **macOS:** `dialf service install --user` (login LaunchAgent) and **Allow** the mic prompt
     (a *system* daemon can't record — see below).
5. **Phone app:** keep **Route calls to wired headset** on, then **Start service**.
6. **Record:**
   ```sh
   dialf run server/jobs/live-call.yaml   --device <id>   # call + record
   dialf run server/jobs/record-only.yaml --device <id>   # record only, no call
   ```
   → writes `<job>-rx.wav`, `-tx.wav`, `-mix.wav` in `record_dir`.

## Wiring (sound card ↔ phone)

```
  Physical chain:

    host (mac/linux) ──USB── USB sound card ──line out/in── USB-C⇄TRRS adapter ──USB-C── phone
        dialfd                 (MiniFuse 2)                   (4-pole headset)          (Pixel)

  Signal flow (two independent directions):

    tx   dialfd ─▶ card OUT ─▶ adapter MIC pin ─▶ phone call mic ─▶ … ─▶ far end
    rx   far end ─▶ … ─▶ phone earpiece ─▶ adapter EAR pin ─▶ card IN ─▶ dialfd (records rx)
```

The bridge needs **two directions**:

- **card OUTPUT → phone call MIC** — so injected prompts become what the far end hears (tx).
- **phone EARPIECE → card INPUT** — so the far-end voice is captured (rx).

On a phone with no 3.5 mm jack (e.g. Pixel 9 Pro), use a **USB‑C → 4‑pole TRRS headset
adapter** and a TRRS breakout so:

- sound-card **out → headset MIC pin**
- headset **earpiece pin → sound-card IN**

The phone must recognize the adapter as a **headset** (headset icon shows). A plain USB‑C DAC
or line cable won't do mic injection — it needs the TRRS mic pin.

**Levels:** set the card's input gain so rx peaks around −12…−20 dBFS. If the far end is too
quiet, raise the card's input gain first; you can also add software gain in `dialfd` (see
`capture_cmd` below). Tested interface: Arturia **MiniFuse 2** (CoreAudio name `MiniFuse 2`).

## Phone app

Open the DialF app and leave **"Route calls to wired headset" ON** (default). This makes
`InCallService` pin call audio to `ROUTE_WIRED_HEADSET`, so the call's mic + earpiece use the
wired bridge instead of the phone's built-in mic/earpiece. Without it, the phone uses its own
mic and the bridge does nothing.

## Running dialfd so it can record (macOS)

macOS gates microphone access via TCC, and this is the part that bites:

| How dialfd runs | Mic capture? |
|---|---|
| From the Claude/ssh/headless shell | ❌ silently denied → records silence |
| **System LaunchDaemon** (`sudo dialf service install`, root, boot) | ❌ no GUI/TCC → cannot record (needs MDM PPPC) |
| **User LaunchAgent** (`dialf service install --user`, login session) | ✅ can hold the mic |
| Linux (systemd, system or `--user`) | ✅ no TCC gate |

So on macOS, **run the audio host as a user LaunchAgent** (it auto-starts at login,
keep-alive restarts, and runs in the GUI session that can be granted the mic):

```sh
dialf service install --user
```

The first time it captures, macOS prompts to allow the microphone — **Allow** it (the binary
then appears under System Settings → Privacy & Security → Microphone). If you never see the
prompt, trigger it once from a GUI Terminal: `sox -d -n stat trim 0 2` → Allow.

A root **system** daemon can't record on macOS (no way to grant it the mic without MDM). For a
true headless/boot service that records, run the bridge host on **Linux** (no TCC).

## Config

`~/.config/dialf/config.yaml` for the sound-card host (see `config.example.yaml`):

```yaml
control_socket: /tmp/dialfd.sock
audio:
  sample_rate: 48000
  channels: 1
  capture_device: "MiniFuse 2"     # CoreAudio name (macOS) / plughw:1,0 (ALSA)
  playback_device: "MiniFuse 2"
  record_dir: /Users/you/dialf/recordings
  mix_recording: true
  # Pin sox by full path + explicit device so a stripped PATH can't fall back to a
  # default-output tool. `gain 20` boosts a quiet far-end capture ~20 dB (optional).
  capture_cmd:  ["/opt/homebrew/bin/sox", "-q", "-t", "coreaudio", "MiniFuse 2", "-t", "raw", "-b", "16", "-e", "signed-integer", "-r", "{rate}", "-c", "{channels}", "-", "gain", "20"]
  playback_cmd: ["/opt/homebrew/bin/sox", "-q", "-V1", "{file}", "-t", "coreaudio", "MiniFuse 2"]
```

## Jobs

- **`server/jobs/live-call.yaml`** — places a call, plays prompts, records the conversation.
- **`server/jobs/record-only.yaml`** — plays + records through the card with **no call**
  (only `audio.play` / `audio.wait_for_speech`).

Each recorded job writes (paths returned by `dialf run`):

- `<job>-rx.wav` — captured from the card (far end)
- `<job>-tx.wav` — audio injected into the card (our prompts)
- `<job>-mix.wav` — both, time-aligned (when `mix_recording: true`)

```sh
dialf run server/jobs/live-call.yaml   --device <id>   # call + record
dialf run server/jobs/record-only.yaml --device <id>   # record only, no dial
```

Tip: serve the WAVs to listen from another machine — `atem serv files <dir> --background`.
