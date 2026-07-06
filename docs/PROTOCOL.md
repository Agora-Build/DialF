# DialF protocol & control API

Three interfaces:

1. **Phone ↔ dialfd** — JSON over WebSocket (the control plane, over WiFi).
2. **dialf/tools → dialfd** — line-delimited JSON over a local Unix socket (the control API).
3. **`dialf` CLI** — a thin client over interface 2.

All message types are tagged by a `type` field. Source of truth: `protocol.rs`.

---

## 1. Phone ↔ dialfd (WebSocket)

The phone connects to `ws://<host>:8765` (discovered via mDNS `_dialfd._tcp`, or a fixed
address). The **first frame must be `hello`** with the correct shared key, or the socket is
closed.

### Phone → dialfd

| `type`       | Fields                                                                 |
|--------------|-----------------------------------------------------------------------|
| `hello`      | `device_id`, `name`, `key`, `caps[]`, `app_version?`                   |
| `heartbeat`  | `ts`, `battery?` — sent ~every 30s                                     |
| `call_state` | `call_id`, `state` (`dialing`/`ringing`/`active`/`ended`), `number?`, `direction` (`in`/`out`). `dialing` = outbound, far end ringing, not yet answered; `active` = answered/connected |
| `sms`        | `direction` (`in`/`out`), `from?`, `to?`, `body`, `ts`                 |
| `calls`      | `entries[]` of `{number?, kind, ts, duration}` — reply to `list_calls` |
| `sims`       | `entries[]` of `{slot, sub_id, name?, carrier?, number?, is_default}` — reply to `list_sims` |
| `mmi_result` | `code`, `success`, `response?` — reply to `mmi`                        |
| `voicemail_result` | `enabled`, `success`, `response?` — reply to `set_voicemail`     |
| `ack`        | `cmd_id`, `ok` — acknowledges a command                               |
| `error`      | `cmd_id?`, `msg`                                                       |

### dialfd → phone

A single frame, `cmd`, carrying `cmd_id` plus a flattened **action**:

| action          | Fields           | Effect                                          |
|-----------------|------------------|-------------------------------------------------|
| `answer`        | `call_id?`       | answer the ringing call (or the given leg)      |
| `dial`          | `number`, `sim_sub_id?` | place an outbound call (default SIM if omitted) |
| `hangup`        | `call_id?`       | end the active call (or the given leg)          |
| `reject`        | `call_id?`, `drop?` | decline the ringing call; `drop` answers then hangs up (no voicemail) |
| `send_sms`      | `to`, `body`     | send a text                                     |
| `list_sms`      | `since?`         | report the inbox (replies as `sms` frames)      |
| `list_calls`    | —                | report the call log (replies as one `calls` frame) |
| `list_sims`     | —                | report active SIMs (replies as one `sims` frame) |
| `mmi`           | `code`, `sim_sub_id?` | run a raw MMI/USSD code (low-level); replies `mmi_result` |
| `set_voicemail` | `enabled`, `number?`, `sim_sub_id?` | enable/disable voicemail; device maps to its mechanism (Android: GSM MMI). Replies `voicemail_result` |
| `set_autoanswer`| `numbers[]`      | replace the phone's local auto-answer list      |

**Auto-answer:** `dialfd` owns the routing (config `autoanswer`, a number→optional-job map,
plus any live `dialf run --autoanswer` override). On an inbound `call_state{state:ringing}`
whose `number` matches, `dialfd` either sends `answer` (answer-only) or answers and runs the
mapped job. One sound card → one call at a time, so a match while a job is already running is
skipped + logged.

---

## 2. Control API (local Unix socket)

`dialf` and any other local tool send one JSON request per line to `dialfd`'s control
socket and read one JSON response per line. Each request has an `id`, an `op`, and op-specific
fields; the response echoes `id` and carries `ok`, optional `data`, and `error`.

| `op`           | Fields                          | Returns                                |
|----------------|---------------------------------|----------------------------------------|
| `server.info`  | —                               | `{version, ten_vad}` (the daemon's own) |
| `devices.list` | —                               | array of devices                       |
| `call.dial`    | `device`, `number`, `sim_sub_id?` | `{dialed, sim_sub_id}`               |
| `call.answer`  | `device`                        | ok                                     |
| `call.hangup`  | `device`                        | ok                                     |
| `call.reject`  | `device`, `drop?`               | ok                                     |
| `sms.send`     | `device`, `to`, `body`          | ok                                     |
| `sms.list`     | `device`                        | `{messages:[...]}`                     |
| `call.list`    | `device`                        | `{calls:[...]}`                        |
| `sims.list`    | `device`                        | `{sims:[...]}`                         |
| `mmi.send`     | `device`, `code`, `sim_sub_id?` | `{code, success, response?}`           |
| `voicemail.set`| `device`, `enabled`, `number?`, `sim_sub_id?` | `{enabled, success, response?}`        |
| `audio.play`   | `file`, `device?`               | ok                                     |
| `job.run`      | `path?` \| `steps?`, `device?`  | `{steps:[...], recording:{rx,tx,mix}}` |
| `autoanswer.serve` | `numbers[]`, `path`, `device?` | streamed `{event}` lines (see below) |
| `job.status`   | `job_id`                        | (not tracked yet)                      |

`sms.list` asks the phone to report its inbox, waits briefly for the `sms` frames, then
returns what it has recorded.

`autoanswer.serve` is **connection-scoped**: the daemon registers an auto-answer override
(answer `numbers` with the job at `path`, overriding config) and streams `done:false`
`{event}` lines as calls are handled. The override is removed when the connection closes, so a
client (`dialf run --autoanswer`) reverts to config on exit. Only one serve session is allowed
at a time — a second `autoanswer.serve` is rejected with an error response.

---

## 3. `dialf` CLI

```
dialf daemon                                   run dialfd (control socket + WS + mDNS)
dialf devices                                  list connected phones
dialf sims <device>                            list the device's active SIMs (default tagged)
dialf call dial   <device> <number> [--sim N]  place a call (default SIM if --sim omitted)
dialf call answer <device>                      answer the ringing call
dialf call hangup <device>                      end the active call
dialf call reject <device> [--drop]             decline ringing call (--drop = answer+hangup, no voicemail)
dialf call list   <device> [--human]           read the call log (--human = readable times/numbers/durations)
dialf voicemail off <device> [--sim N]         disable carrier voicemail (MMI #004#)
dialf voicemail on  <device> [--number N] [--sim N]  re-enable (*004# or **004*N#)
dialf mmi <device> <code> [--sim N]            (advanced) send a raw MMI/USSD code
dialf sms send <device> <to> <body>            send a text
dialf sms list <device> [--human]              read recent texts (--human = readable)
dialf run  <job.yaml> [--device <id>]          run a YAML job once
dialf run  <job.yaml> --autoanswer <numbers>   serve: answer those numbers with this job (foreground; Ctrl-C reverts)
dialf play <file>                              inject audio out the sound card
dialf service install|uninstall|start|stop|status [--user] [--config <path>]
dialf --version                                CLI + running daemon (dialfd) versions
```

Flags:
- `--device` — target a specific phone; omit when exactly one is connected.
- `--user` (service) — install for the current login instead of system-wide (boot).

---

## YAML job steps

| `type`                  | Fields                                  |
|-------------------------|-----------------------------------------|
| `call.dial`             | `number`                                |
| `call.wait_answered`    | `timeout_ms` — block until the call is answered (active) |
| `call.answer`           | —                                       |
| `call.hangup`           | —                                       |
| `audio.play`            | `file`                                  |
| `audio.wait_for_speech` | `end_timeout_ms`, `silence_duration_ms`, `onset_duration_ms` |
| `sms.send`              | `to`, `body`                            |
| `wait`                  | `ms`                                    |
| `log`                   | `message`                               |

Example jobs in `server/jobs/`:

| File | What it does |
|------|--------------|
| `sample.yaml` | answer, then a two-turn VAD exchange |
| `outbound-call.yaml` | dial → greet → Q&A → SMS → hang up |
| `inbound-call.yaml` | auto-answer conversation (the daemon answers; no `call.answer` needed) |
| `live-call-pilot.yaml` | answer → play → wait → text → hang up |
| `record-only.yaml` | record the sound card only — no call; captures any app's audio |

In **auto-answer** mode the daemon answers the call itself, so `call.dial` /
`call.wait_answered` / `call.answer` in the job are skipped (with a warning). In normal
`dialf run … --device` mode they run as written.

---

## Config (dialfd)

| Key              | Meaning                                                  |
|------------------|----------------------------------------------------------|
| `shared_key`     | secret the phone must present in `hello`                  |
| `control_socket` | path to the local control socket                         |
| `ws_bind`        | `host:port` for the phone WebSocket server (`0.0.0.0:8765`) |
| `instance_name`  | name advertised via mDNS                                  |
| `autoanswer`     | map: number → optional job path (null = answer only; path = answer + run that job) |
| `audio`          | sound-card devices, commands, `record_dir`, `mix_recording`, `mix_channels` |
