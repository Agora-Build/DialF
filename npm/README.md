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

## Run the daemon as a service

`npm install` only installs the CLI. To run `dialfd` in the background:

```sh
sudo dialf service install        # boot service (launchd/systemd)
dialf service install --user      # or per-user (login), no sudo
```

## Usage

```sh
dialf daemon                       # run dialfd in the foreground
dialf devices                      # list connected phones
dialf call dial   <device> <number>   # place a call
dialf call pickup <device>            # answer the ringing call
dialf call hangup <device>            # end the active call
dialf call reject <device>            # decline the ringing call
dialf call list   <device>            # read the call log (JSON)
dialf sms send <device> <to> <body>
dialf sms list <device>
dialf run  <job.yaml> [--device <id>]
dialf play <file>
```

Full documentation, protocol, and the phone app live in the
[main repository](https://github.com/Agora-Build/DialF).

## License

MIT
