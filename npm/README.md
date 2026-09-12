# @agora-build/dialf

CLI for **DialF** — an autonomous phone pick/call system. The `dialf` CLI commands a phone
(via the `dialfd` daemon) to make/answer real calls and send/receive SMS over WiFi, while
call audio is bridged through a USB sound card on the host.

This package downloads the prebuilt native `dialf` binary (with onnxruntime + the ten-vad
model bundled) for your platform on install.

## Install

```sh
npm install -g @agora-build/dialf

# or, without npm:
curl -fsSL https://dl.agora.build/dialf/install.sh | bash
```

Supported platforms: macOS (arm64/x86_64) and Linux (x86_64/aarch64). The postinstall step
fetches the matching binary from the [GitHub Releases](https://github.com/Agora-Build/DialF/releases);
it never hard-fails `npm install` — if no prebuilt exists for your platform it prints
guidance to build from source.

## The phone app (DialF Phone)

This package installs the **controller** (`dialf` CLI + `dialfd`). The phone DialF controls
runs the **DialF Phone** Android app — it places/answers/rejects calls, sends/reads SMS, and
reads the call log/SIMs on command, while call audio is bridged through the host's USB sound
card. Install it on the phone and pair it to `dialfd` with a shared key:

- Newest APK: https://dl.agora.build/dialf/dialf-phone-latest.apk
- All versions: https://github.com/Agora-Build/DialF/releases

See the [main repository](https://github.com/Agora-Build/DialF) for the end-to-end setup
walkthrough and the [phone-app reference](https://github.com/Agora-Build/DialF/blob/main/app/README.md).

## Run the daemon as a service

`npm install` only installs the CLI. To run `dialfd` in the background, pick a scope — the
install scope decides whether the daemon is private to you or shared by everyone on the machine:

```sh
# Isolated (your own private daemon):
dialf service install --user
# per-user socket: /run/user/$UID/dialfd.sock (Linux) or /tmp/dialfd-$UID.sock (macOS).
# CLI and daemon auto-agree on the socket.

# Shared (one daemon all login users can drive):
sudo dialf service install
# creates the `dialf` group and binds a machine-wide socket
# (/run/dialf/dialfd.sock on Linux, /var/run/dialfd.sock on macOS), group `dialf`, mode 0660.
sudo usermod -aG dialf <user>                      # grant a user access — Linux
sudo dseditgroup -o edit -a <user> -t user dialf   #                       macOS
# the user logs out/in, then `dialf devices` reaches the shared daemon.
```

The control socket lets its holder dial, hang up, send SMS, and run jobs, so keep the `dialf`
group to trusted users. Manage the service with `dialf service status|stop|start|uninstall [--user]`.

## Usage

```sh
dialf daemon                       # run dialfd in the foreground
dialf devices                      # list connected phones
dialf sims <device>                # list SIMs (default tagged)
dialf call dial   <device> <number> [--sim <sub_id>]   # place a call (default SIM if omitted)
dialf call answer <device>            # answer the ringing call
dialf call hangup <device>            # end the active call
dialf call reject <device> [--drop]   # decline ringing call (--drop = answer+hangup, no voicemail)
dialf call list   <device>            # read the call log (JSON)
dialf voicemail off|on <device> [--sim N]   # toggle carrier voicemail (MMI)
dialf mmi <device> <code> [--sim N]   # (advanced) raw MMI/USSD code
dialf sms send <device> <to> <body>
dialf sms list <device>
dialf run  <job.yaml> [--device <id>]                  # run a job once
dialf run  <job.yaml> --autoanswer <numbers>           # serve a job for inbound calls (Ctrl-C reverts)
dialf play <file>

dialf devices share --list                             # adb devices attached to this host
dialf devices share --target <serial> [--token]        # share one with other machines
dialf devices share --status | --stop
dialf devices connect <host> --token <dvs_...>         # reach a token-protected share
```

## Sharing a phone with another machine

The phone is plugged into machine **A**; you want `adb` on machine **B**. `dialf devices share`
exposes A's adb server, so B can run `adb shell`, `install`, `logcat`, `push`/`pull` as if the
phone were plugged in locally. Handy over a LAN or a Netbird/Tailscale/VPN overlay.

```sh
# on A
dialf devices share --list            # find the serial
dialf devices share --target 4B2B1C   # share it

# on B
export ADB_SERVER_SOCKET=tcp:<A-address>:5939
adb devices
```

The default share binds `0.0.0.0:5939` and is **open** — anyone who can reach that port can
install apps, read `/sdcard` and open a shell on the phone. That is fine on a network you
trust and not on one you don't; add `--token` for a one-time secret (B then uses
`dialf devices connect`), or bind loopback and tunnel in with `ssh -L`. Shares expire after
an hour by default.

See [`docs/DEVICE_SHARING.md`](https://github.com/Agora-Build/DialF/blob/main/docs/DEVICE_SHARING.md)
for the full guide.

Full documentation, protocol, and the phone app live in the
[main repository](https://github.com/Agora-Build/DialF).

## License

MIT
