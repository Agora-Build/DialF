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
| `call_state` | `call_id`, `state` (`ringing`/`active`/`ended`), `number?`, `direction` (`in`/`out`) |
| `sms`        | `direction` (`in`/`out`), `from?`, `to?`, `body`, `ts`                 |
| `calls`      | `entries[]` of `{number?, kind, ts, duration}` — reply to `list_calls` |
| `sims`       | `entries[]` of `{slot, sub_id, name?, carrier?, number?, is_default}` — reply to `list_sims` |
| `ack`        | `cmd_id`, `ok` — acknowledges a command                               |
| `error`      | `cmd_id?`, `msg`                                                       |

### dialfd → phone

A single frame, `cmd`, carrying `cmd_id` plus a flattened **action**:

| action          | Fields           | Effect                                          |
|-----------------|------------------|-------------------------------------------------|
| `pickup`        | `call_id?`       | answer the ringing call (or the given leg)      |
| `dial`          | `number`, `sim_sub_id?` | place an outbound call (default SIM if omitted) |
| `hangup`        | `call_id?`       | end the active call (or the given leg)          |
| `reject`        | `call_id?`       | decline the ringing call (or the given leg)     |
| `send_sms`      | `to`, `body`     | send a text                                     |
| `list_sms`      | `since?`         | report the inbox (replies as `sms` frames)      |
| `list_calls`    | —                | report the call log (replies as one `calls` frame) |
| `list_sims`     | —                | report active SIMs (replies as one `sims` frame) |
| `set_autopickup`| `numbers[]`      | replace the phone's local auto-pickup list      |

**Auto-pickup:** `dialfd` owns the allow-list (config `autopickup`). On an inbound
`call_state{state:ringing}` whose `number` is on the list, `dialfd` sends `pickup`.

---

## 2. Control API (local Unix socket)

`dialf` and any other local tool send one JSON request per line to `dialfd`'s control
socket and read one JSON response per line. Each request has an `id`, an `op`, and op-specific
fields; the response echoes `id` and carries `ok`, optional `data`, and `error`.

| `op`           | Fields                          | Returns                                |
|----------------|---------------------------------|----------------------------------------|
| `devices.list` | —                               | array of devices                       |
| `call.dial`    | `device`, `number`, `sim_sub_id?` | `{dialed, sim_sub_id}`               |
| `call.pickup`  | `device`                        | ok                                     |
| `call.hangup`  | `device`                        | ok                                     |
| `call.reject`  | `device`                        | ok                                     |
| `sms.send`     | `device`, `to`, `body`          | ok                                     |
| `sms.list`     | `device`                        | `{messages:[...]}`                     |
| `call.list`    | `device`                        | `{calls:[...]}`                        |
| `sims.list`    | `device`                        | `{sims:[...]}`                         |
| `audio.play`   | `file`, `device?`               | ok                                     |
| `job.run`      | `path?` \| `steps?`, `device?`  | `{steps:[...], recording:{rx,tx,mix}}` |
| `job.status`   | `job_id`                        | (not tracked yet)                      |

`sms.list` asks the phone to report its inbox, waits briefly for the `sms` frames, then
returns what it has recorded.

---

## 3. `dialf` CLI

```
dialf daemon [--dry-audio] [--with-loopback]   run dialfd (control socket + WS + mDNS)
dialf devices                                  list connected phones
dialf sims <device>                            list the device's active SIMs (default tagged)
dialf call dial   <device> <number> [--sim N]  place a call (default SIM if --sim omitted)
dialf call pickup <device>                      answer the ringing call
dialf call hangup <device>                      end the active call
dialf call reject <device>                      decline the ringing call
dialf call list   <device>                      read the call log (JSON)
dialf sms send <device> <to> <body>            send a text
dialf sms list <device>                         read recent texts (JSON)
dialf run  <job.yaml> [--device <id>]          run a YAML job
dialf play <file>                              inject audio out the sound card
dialf service install|uninstall|start|stop|status [--user] [--config <path>]
```

Flags:
- `--dry-audio` — simulate audio steps (no sound card / ten-vad needed).
- `--with-loopback` — also register an in-process simulated phone (off by default; real
  phones only).
- `--device` — target a specific phone; omit when exactly one is connected.
- `--user` (service) — install for the current login instead of system-wide (boot).

---

## YAML job steps

| `type`                  | Fields                                  |
|-------------------------|-----------------------------------------|
| `call.dial`             | `number`                                |
| `call.pickup`           | —                                       |
| `call.hangup`           | —                                       |
| `audio.play`            | `file`                                  |
| `audio.wait_for_speech` | `end_timeout_ms`, `silence_duration_ms` |
| `sms.send`              | `to`, `body`                            |
| `wait`                  | `ms`                                    |
| `log`                   | `message`                               |

See `server/jobs/sample.yaml` and `server/jobs/end-to-end-call.yaml`.

---

## Config (dialfd)

| Key              | Meaning                                                  |
|------------------|----------------------------------------------------------|
| `shared_key`     | secret the phone must present in `hello`                  |
| `control_socket` | path to the local control socket                         |
| `ws_bind`        | `host:port` for the phone WebSocket server (`0.0.0.0:8765`) |
| `instance_name`  | name advertised via mDNS                                  |
| `autopickup`     | numbers answered automatically when they ring            |
| `audio`          | sound-card devices, commands, `record_dir`, `mix_recording` |
