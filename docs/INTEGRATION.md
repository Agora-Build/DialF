# Integrating DialF into your own product

DialF is usable as a **phone runtime for another program**: your service decides what should
happen on a call, DialF holds the call and executes it. `dialf` the CLI is just one client of
that interface — anything that can open a Unix socket and write a line of JSON is another.

This guide is the contract for writing that second client. [PROTOCOL.md](PROTOCOL.md) is the
reference for every field; this one is about the shape of a working integration and the
handful of behaviours that will bite you if you learn them from production instead.

**What you get:** place and answer calls, run a scripted conversation against the far end,
send and read SMS, and receive per-step timings plus `rx`/`tx`/`mix` recordings to analyse
afterwards. **What you don't:** call audio over the network. Android forbids capturing call
audio, so audio crosses a physical bridge (USB sound card, or BlackHole for a virtual rig) on
the machine running the daemon. Your integration talks control; audio stays local.

---

## 1. The concept: a Programmable Application

Most automation assumes an API exists. A phone call has no API worth the name — the carrier
network, the handset, and the dialer app are all closed, and the parts you would most want to
observe (what the other side actually said, and when) were never exposed to begin with. The
usual answer is to give up on the real thing and test a simulation of it instead.

DialF takes the other route. It treats an ordinary application running on ordinary hardware as
a **Programmable Application**: something never built to be driven by software, wrapped in
enough scaffolding that a script can drive it anyway, and that a program can *read back* what
happened. Three planes do that work:

| Plane | For the phone | What it gives a script |
|---|---|---|
| **Control** | dialfd ↔ the DialF Phone app over WiFi | the app's actions — dial, answer, hang up, text |
| **Media** | USB sound card (or BlackHole) on the headset path | capture what it hears, inject what it should say |
| **Observation** | rx/tx/mix recordings + timed step outcomes | a readable record of what actually occurred |

What makes it *programmable* rather than merely scriptable is that all three are addressed by
one vocabulary. A job is a flat list of typed steps — data, not code, so your product can
**generate** it per run — and every step comes back as a structured outcome stamped against the
recording's own clock. You write "ask this, wait for the answer, cut in after two seconds", and
you get back exactly when each of those happened in a WAV you can analyse. That vocabulary is
[Libretto](PROTOCOL.md); DialF is one implementer of it, and `server.manifest` is how a build
tells you which parts of it this one speaks.

The pay-off is that the thing under control is **real**: a real SIM on a real carrier over a
real acoustic path. What you measure is what a person on the other end of that call would have
experienced — not what a mock of the network would have produced.

Today the programmable application is the phone's dialer and messaging. The three planes are
not specific to it: the audio bridge does not know which app is making the sound, and
device-level automation already exists in the codebase for [device
sharing](DEVICE_SHARING.md). `app.*` steps are reserved in the Libretto spec for driving a
third-party app's UI the same way. That is a stated direction, not a shipped feature — build
against what `server.manifest` lists.

### What that means for your code

```
  your service ──JSON lines──► /tmp/dialfd-501.sock ──► dialfd ──WiFi WS──► phone
                                                          │
                                                          └──► sound card ──► rx/tx/mix.wav
                                                                                    │
  your analysis ◄───────────── the result JSON names these files ◄──────────────────┘
```

Every awkward constraint in the rest of this document falls out of that diagram, so they are
worth accepting up front rather than discovering:

- **The daemon is a local peer, not a service you call over a network.** Its only entrance for
  you is a Unix socket on the same machine, because the media plane is a cable. Co-locate your
  process with `dialfd`.
- **There is one phone and one sound card**, so one call at a time. That is hardware, not
  policy, and no amount of connection pooling changes it.
- **File paths are resolved by the daemon**, not by you — it is the process holding the device.
- **Permissions are a runtime concern.** Driving a real device means the OS gets a vote. On
  macOS a missing microphone grant only *fails* when a job runs, but `server.info` reports it,
  so check it at startup (§6, §10).
- **The interesting output is files.** The socket tells you what happened and where the audio
  landed; the analysis you care about happens afterwards, against the WAVs.

---

## 2. Finding the socket

Resolve it the way the CLI does, in this order:

1. `control_socket` in `~/.config/dialf/config.yaml`, if set — an explicit path always wins.
2. This user's own daemon: `$XDG_RUNTIME_DIR/dialfd.sock`, else `/tmp/dialfd-<uid>.sock`.
3. The machine-wide daemon: `/var/run/dialfd.sock` (macOS) or `/run/dialf/dialfd.sock`.

Skipping to a hardcoded path works right up until the deployment that installed `dialfd` in the
other scope. Read the config first.

**The socket has no authentication beyond filesystem permissions.** A per-user install
(`dialf service install --user`) gives a socket only that user can open; a system install gives
one owned by the `dialf` group at `0660`, so every group member can dial, text, and run jobs on
the phone. Decide which of those your product wants before it ships — for an unattended
integration, a per-user daemon owned by your service account is almost always right.

---

## 3. The wire format

Line-delimited JSON over `SOCK_STREAM`. One request object per line; one response object per
line. Every request carries an `id` you choose, echoed back so you can correlate.

```jsonc
// →
{"id": "1", "op": "server.info"}
// ←
{"id": "1", "done": true, "ok": true,
 "data": {"version": "0.3.10", "ten_vad": "1.0", "microphone": "authorized",
          "config_path": "/Users/you/.config/dialf/config.yaml"}}
```

| Field | Meaning |
|---|---|
| `id` | echoes your request's `id`. Empty string if your JSON was unparseable |
| `done` | `true` on the terminal frame. `false` marks an interim event (only `autoanswer.serve` sends these) |
| `ok` | `true`/`false` |
| `error` | human-readable string, present when `ok` is `false` |
| `data` | op-specific payload |

Failures are **responses, not disconnections** — `ok: false` with an `error` string, and the
connection stays usable.

One wrinkle for anything that keys responses by `id`: a line the daemon cannot *parse* — which
includes a line naming an `op` this build doesn't have — fails before the `id` is read, so it
comes back as `id: ""`. An unrecognised op from a newer client therefore arrives as an
uncorrelatable error. Match the terminal frame on the connection rather than strictly on `id`,
or do the §6 handshake so it cannot happen.

---

## 4. The connection model — read this part

Four behaviours that are not guessable from the message format:

**Requests on one connection are handled strictly in sequence.** The daemon reads a line, runs
it to completion, writes the response, *then* reads the next line. A `job.run` that holds a
three-minute call blocks its own connection for three minutes.

**So `job.cancel` must come from a different connection.** Sending it down the socket that is
running the job means it sits unread in the buffer until the job you wanted to cancel has
already finished. Keep a second connection open for control. (This is exactly what `dialf run`
does on Ctrl+C.)

**Separate connections run concurrently, and are not isolated from each other.** `job.run`
takes the sound-card lock, so a second one is refused with `phone busy: a call or recording is
already in progress` — but `call.hangup`, `call.dial` and `audio.play` from another connection
are *not* refused, and will happily interfere with a live call. Nothing on the socket protects
your run from a colleague's CLI. Own the daemon: give your service its own per-user `dialfd`
and keep the CLI for provisioning and diagnostics.

**`job.run` returns nothing until the job ends.** There is no progress stream and `job.status`
is not implemented. For a scripted call that is minutes of silence followed by one large
result. Set your read timeout from the job's own worst case — the sum of its
`timeout_ms`/`end_timeout_ms` values plus slack — not from a default socket timeout, and if you
need liveness in the meantime, watch the daemon log (`~/Library/Logs/dialfd.<date>.log` on
macOS, journald on Linux).

---

## 5. A minimal client

Roughly thirty lines, and genuinely enough to drive a call:

```python
import json, os, re, socket, itertools

def socket_path():
    cfg = os.path.expanduser("~/.config/dialf/config.yaml")
    if os.path.exists(cfg):
        # `control_socket` is a top-level scalar, so a line scan avoids a YAML dependency.
        m = re.search(r'^control_socket:\s*"?(.+?)"?\s*$', open(cfg).read(), re.M)
        if m:
            return os.path.expanduser(m.group(1))
    user = os.path.join(os.environ["XDG_RUNTIME_DIR"], "dialfd.sock") \
        if "XDG_RUNTIME_DIR" in os.environ else f"/tmp/dialfd-{os.getuid()}.sock"
    if os.path.exists(user):
        return user
    system = "/var/run/dialfd.sock" if os.uname().sysname == "Darwin" else "/run/dialf/dialfd.sock"
    return system if os.path.exists(system) else user

class Dialf:
    """One connection. Open a second instance for cancels — see §4."""
    def __init__(self, path=None, timeout=600):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(timeout)   # must exceed the longest job you dispatch
        self.sock.connect(path or socket_path())
        self.rx = self.sock.makefile("rb")   # read side only; writes go via sendall
        self.ids = itertools.count(1)

    def send(self, op, **fields):
        rid = str(next(self.ids))
        self.sock.sendall(json.dumps({"id": rid, "op": op, **fields}).encode() + b"\n")
        return rid

    def call(self, op, **fields):
        """One-shot op. Raises on ok:false; returns `data`."""
        self.send(op, **fields)
        for resp in self.frames():
            if resp.get("done"):
                if not resp.get("ok"):
                    raise RuntimeError(resp.get("error", "unknown dialf error"))
                return resp.get("data")

    def frames(self):
        """Every response line, including `done:false` events."""
        for line in self.rx:
            if line.strip():
                yield json.loads(line)
```

Node, Go, or anything else is the same three ideas: connect, write a JSON line, read lines
until `done`.

---

## 6. Handshake: check before you dispatch

Do this once at startup. It is two round trips and it converts a mid-call failure into a
startup failure:

```python
d = Dialf()
info = d.call("server.info")
manifest = d.call("server.manifest")

if manifest["spec_version"] != "0.1":
    raise SystemExit(f"dialf speaks spec {manifest['spec_version']}, this client expects 0.1")

needed = {"call.dial", "call.wait_answered", "audio.play", "audio.wait_for_speech_start"}
missing = needed - set(manifest["steps"])
if missing:
    raise SystemExit(f"dialfd {info['version']} does not implement: {', '.join(sorted(missing))}")

# macOS: every job records, so a missing grant fails every job. Absent on daemons before 0.3.9.
mic = info.get("microphone")
if mic in ("denied", "not_determined"):
    raise SystemExit(f"dialfd's microphone permission is {mic} — see §10")
```

`server.manifest` lists exactly the steps the running build implements, generated from the same
in-code list the executor dispatches on, so it cannot drift. Check it rather than the version
number: a step you need is a capability, and capabilities are what the manifest states.
`inline_orchestrated` tells you which steps can run *during* a call rather than only around it.

`server.info` also returns `ten_vad`. If that reads `"stub"`, the build has no voice-activity
detection linked — every `audio.wait_for_speech*` step will misbehave. Fail at startup.

`microphone` is `authorized`, `denied`, `not_determined` or `unknown` on macOS, and
`not_applicable` on Linux, which has no such gate.

---

## 7. Outbound: your service places the call

Send the steps inline. You do not need a job file on disk, which matters when the script is
generated per run:

```python
result = d.call("job.run", name="eval-1182", steps=[
    {"type": "call.dial", "id": "dial", "number": "+15551234"},
    {"type": "call.wait_answered", "id": "answered", "timeout_ms": 30000},
    {"type": "audio.play", "id": "q1", "file": "/srv/corpus/question1.wav"},
    {"type": "audio.wait_for_speech", "id": "a1",
     "end_timeout_ms": 40000, "silence_duration_ms": 1500},
    {"type": "call.hangup", "id": "bye"},
])
```

Notes that save a debugging session:

- **`id` is yours.** DialF never interprets it and echoes it in the outcome, so you can match
  results to your own model without counting array indices.
- **`file` paths are resolved by the daemon, not by you.** An absolute path is unambiguous;
  a relative one resolves against the *job file's* directory, which for inline `steps` does not
  exist. Send absolute paths.
- **`name` labels the recordings** — `dialf-job-<name>-<timestamp>-rx.wav`. Characters outside
  `[A-Za-z0-9._-]` become `-`, daemon-side, because it becomes a filename.
- **Omit `device` when exactly one phone is connected.** With several, omitting it is an error
  rather than a guess. Audio-only jobs (no call or SMS steps) need no phone at all — that is how
  a BlackHole rig runs with nothing plugged in.

### Reading the result

```jsonc
{
  "steps": [
    { "index": 3, "id": "a1", "type": "audio.wait_for_speech",
      "description": "…", "t_start_ms": 12340, "t_end_ms": 19870,
      "end_reason": "completed", "summary": "speech 4.1s then 3.0s silence" }
  ],
  "recording": { "rx": "…-rx.wav", "tx": "…-tx.wav", "mix": "…-mix.wav",
                 "t0_epoch_ms": 1758412800123 },
  "call": { "answer_latency_ms": 4200, "duration_ms": 63500,
            "end_reason": "completed", "remote_number": "+1555…", "sim": "…" }
}
```

**`t=0` is the start of the recording, not the start of the job.** Every `t_start_ms` /
`t_end_ms` is therefore a byte offset into the WAVs in disguise — you can slice the rx leg by a
step's window with no clock correlation at all. `t0_epoch_ms` exists only to tie the session to
other systems' clocks.

The `rx` and `tx` legs are **timeline-aligned and the same length**: played audio is anchored at
the rx frame clock, and tx is silent wherever nothing played. So the gap between the end of your
speech on `tx` and the far end's onset on `rx` is response latency, computable from the two
files alone, with the step timestamps used to segment and cross-check.

`end_reason` is `completed` | `timeout` | `skipped` | `cancelled` | `call_ended`. Note that
**`timeout` is an outcome, not an error** — a wait that expired reports it and the job carries
on. If you treat any non-`completed` step as a failed run you will throw away good calls.

`recording` is absent for a job that ran without audio; `call` is absent when no call was placed.

---

## 8. Inbound: the far end calls you

`autoanswer.serve` registers an override — answer these numbers with this job — and **owns its
connection for as long as it lives**. Disconnect and the override reverts to config, which makes
it safe: a crashed integration cannot leave a phone auto-answering forever.

```python
serve = Dialf()                       # a dedicated connection; it never returns to one-shot use
serve.send("autoanswer.serve",
           numbers=["+15551234"],
           path="/srv/jobs/inbound-eval.yaml")   # absolute; a *file*, not inline steps
for frame in serve.frames():
    if frame.get("done"):             # terminal frame = refused (see below)
        raise RuntimeError(frame.get("error"))
    print(frame["data"]["event"])     # "14:02:11  answered +1555… — running inbound-eval.yaml"
```

Three constraints:

- **Only one serve session at a time**, machine-wide. A second is refused on its terminal frame
  with `a dialf serve session is already running`.
- **`path` only.** Unlike `job.run`, serve takes a file path — inline steps are not accepted, so
  a generated script must be written to disk first.
- **The event stream is for humans.** Those `event` strings are log lines, formatted for
  display, with no stability guarantee. Do not parse them. The machine-readable record of an
  inbound call is the same recording + result data the job produces; read it from the recording
  directory, keyed by the label.

---

## 9. Op reference

Every op takes `device` (omit when exactly one phone is connected) unless noted.

| `op` | Request fields | `data` on success |
|---|---|---|
| `server.info` | — | `{version, ten_vad, microphone, config_path}` |
| `server.manifest` | — | `{executor, version, spec_version, steps[], inline_orchestrated[], extensions[]}` |
| `devices.list` | — (no `device`) | `[{id, name, addr, last_seen_ms, current_call, adb?}]` — `adb`: wireless-debugging state, see [PROTOCOL.md](PROTOCOL.md#wireless-adb) |
| `call.dial` | `number`, `sim_sub_id?` | `{dialed, sim_sub_id}` |
| `call.answer` | — | — |
| `call.hangup` | — | — |
| `call.reject` | `drop?` | — |
| `call.list` | — | `{calls: [...]}` |
| `sms.send` | `to`, `body` | — |
| `sms.list` | — | `{messages: [...]}` |
| `sims.list` | — | `{sims: [...]}` |
| `mmi.send` | `code`, `sim_sub_id?` | `{code, success, response?}` |
| `voicemail.set` | `enabled`, `number?`, `sim_sub_id?` | `{enabled, success, response?}` |
| `audio.play` | `file` (no device needed) | — |
| `job.run` | `steps[]` \| `path`, `device?`, `name?` | §7 |
| `job.cancel` | `force?` | `{cancelled, force}` |
| `autoanswer.serve` | `numbers[]`, `path` | streamed events, §8 |
| `share.start` / `share.devices` / `share.stop` / `share.status` | see [DEVICE_SHARING.md](DEVICE_SHARING.md) | |

`sms.list`, `call.list` and `sims.list` ask the phone and wait ~800 ms for its reply, then
return what arrived. They are snapshots of what the daemon has recorded, not a guarantee the
phone answered — poll again if a just-sent message is missing.

**Cancelling.** `job.cancel` with `force: false` stops at the next step boundary and interrupts
an in-flight `wait_for_speech`, but lets a `play` finish. `force: true` also kills playback
mid-file. Either way the recording is finalized and the WAVs are saved, and the
job's own response still arrives normally on its own connection (`ok: true`) — steps that ran
keep their own `end_reason`, followed by a marker outcome with `end_reason: cancelled` and the
remaining steps as `skipped`. A cancelled run is a readable result, not a lost one.

---

## 10. Errors you will actually hit

| `error` contains | Meaning | What to do |
|---|---|---|
| `no device specified and not exactly one is connected` | zero or several phones | check `devices.list` at startup; pass `device` |
| `unknown device` | the phone dropped off WiFi | re-read `devices.list`; the id is stable, the connection is not |
| `phone busy: a call or recording is already in progress` | another `job.run` holds the card | serialize your own runs; don't retry blindly |
| `Microphone permission denied` | macOS TCC | grant it — see below |
| `unknown variant \`audio.…\`` | the daemon is older than your client | this is what §6's manifest check prevents |
| `parse job file …` | bad YAML, or a step this build lacks | validate against the manifest before dispatch |
| `a dialf serve session is already running` | second `autoanswer.serve` | one at a time, machine-wide |

**Microphone permission is the single most common deployment failure on macOS.** It fails at
the first job, not at daemon start, so it looks like a bug in your integration — read
`server.info`'s `microphone` at startup instead (§6). The grant is tied to the exact binary, so
every DialF upgrade needs a fresh one; a daemon started under `tmux`/`ssh` cannot show the
dialog at all, and a previously denied binary is never re-prompted. Run `dialfd` as a user
LaunchAgent (`dialf service install --user`), started from a plain Terminal, and have a human
approve it once per upgrade. Every job opens a recording session, so this gates *all* of them —
even a job with no audio steps.

---

## 11. Deploying alongside your product

- **Install `dialfd` as a service under your service account**, per-user scope
  (`dialf service install --user`). That gives you a private socket, a mic grant that survives
  reboots, and no CLI user able to interfere with a live run.
- **Pin the config path** your integration expects and read `config_path` back from
  `server.info` to confirm the daemon is running the one you think it is — a stale
  `config.yaml` in a working directory is a genuinely common surprise.
- **Version the pair.** Your client and `dialfd` are coupled through the manifest; check it on
  every daemon start, not just at install.
- **Recordings accumulate.** `audio.record_dir` grows by three WAVs per job, uncompressed, at
  the card's native rate. Nothing prunes it; that is your job.
- **Don't log the phone numbers** your integration passes through. They appear in `call.dial`,
  `sms.send`, `devices.list` and the `call` result block.

---

## See also

- [PROTOCOL.md](PROTOCOL.md) — field-level reference for the socket API, the phone WebSocket
  plane, the YAML step vocabulary, and config.
- [HARDWARE.md](HARDWARE.md) — the audio bridge: sound card, wiring, channel pinning.
- [DEVICE_SHARING.md](DEVICE_SHARING.md) — driving a phone attached to a *different* machine.
